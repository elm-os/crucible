//! Properties of the egress policy (contract 12.6):
//!
//! - no spelling of a metadata address passes, under any mode or allowed range;
//! - strict mode refuses every loopback, link-local, private, carrier-grade NAT and reserved address, in
//!   every spelling, outside the allowed ranges;
//! - over a scripted network whose resolver changes its answer between calls and whose receiver answers
//!   with redirects, no connection is ever made to a metadata address, and every connection goes to an
//!   address of an answer that passed the policy whole.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use crucible_egress::{
    AddrClass, Cidr, Egress, EgressError, EgressPolicy, Mode, Refusal, classify,
};
use crucible_env::{Env, Instant, Listener, Net, NetError, Stream, TlsRoots};
use http_body_util::Full;
use hyper_util::rt::TokioIo;
use proptest::prelude::*;

/// The spellings of an IPv4 address: itself and the IPv6 forms that carry it.
fn spell(ip: Ipv4Addr, form: u8) -> IpAddr {
    let v4 = u128::from(u32::from(ip));
    let v6 = |bits: u128| IpAddr::V6(Ipv6Addr::from(bits));
    match form % 7 {
        0 => IpAddr::V4(ip),
        1 => v6((0xffff << 32) | v4),
        2 => v6(v4),
        3 => v6((0x0064_ff9b << 96) | v4),
        4 => v6((0x0064_ff9b_0001 << 80) | v4),
        5 => v6((0x2002 << 112) | (v4 << 80) | 1),
        _ => v6((0x2001_0000 << 96) | (0x0808_0808 << 64) | u128::from(!u32::from(ip))),
    }
}

fn metadata_v4() -> impl Strategy<Value = Ipv4Addr> {
    prop_oneof![
        Just(Ipv4Addr::new(169, 254, 169, 254)),
        Just(Ipv4Addr::new(169, 254, 170, 2)),
        Just(Ipv4Addr::new(100, 100, 100, 200)),
        Just(Ipv4Addr::new(168, 63, 129, 16)),
        Just(Ipv4Addr::new(192, 0, 0, 192)),
        any::<u8>().prop_map(|d| Ipv4Addr::new(169, 254, 169, d)),
        any::<u8>().prop_map(|d| Ipv4Addr::new(169, 254, 170, d)),
    ]
}

fn metadata() -> impl Strategy<Value = IpAddr> {
    prop_oneof![
        4 => (metadata_v4(), any::<u8>()).prop_map(|(ip, form)| spell(ip, form)),
        1 => any::<u64>().prop_map(|low| {
            IpAddr::V6(Ipv6Addr::from((0xfd00_0ec2_u128 << 96) | u128::from(low)))
        }),
        1 => Just("fe80::a9fe:a9fe".parse().unwrap()),
        1 => Just("fd20:ce::254".parse().unwrap()),
    ]
}

/// An IPv4 address of a range that strict mode refuses, with the class it must have.
fn internal_v4() -> impl Strategy<Value = (Ipv4Addr, AddrClass)> {
    let host = any::<[u8; 3]>();
    prop_oneof![
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(127, b, c, d), AddrClass::Loopback)),
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(0, b, c, d), AddrClass::Loopback)),
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(10, b, c, d), AddrClass::Private)),
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(172, 16 + b % 16, c, d), AddrClass::Private)),
        host.prop_map(|[_, c, d]| (Ipv4Addr::new(192, 168, c, d), AddrClass::Private)),
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(100, 64 + b % 64, c, d), AddrClass::Cgnat)),
        host.prop_filter("the metadata blocks are their own class", |[_, c, _]| {
            *c != 169 && *c != 170
        })
        .prop_map(|[_, c, d]| (Ipv4Addr::new(169, 254, c, d), AddrClass::LinkLocal)),
        host.prop_map(|[b, c, d]| (Ipv4Addr::new(224 + b % 32, b, c, d), AddrClass::Reserved)),
    ]
    .prop_filter("a metadata address inside the range", |(ip, _)| {
        classify(IpAddr::V4(*ip)) != AddrClass::Metadata
    })
}

fn internal() -> impl Strategy<Value = IpAddr> {
    prop_oneof![
        4 => (internal_v4(), any::<u8>()).prop_map(|((ip, _), form)| spell(ip, form)),
        1 => any::<u64>().prop_map(|low| IpAddr::V6(Ipv6Addr::from((0xfd12_u128 << 112) | u128::from(low)))),
        1 => any::<u64>().prop_map(|low| IpAddr::V6(Ipv6Addr::from((0xfe80_u128 << 112) | u128::from(low) | 1 << 70))),
        1 => Just(IpAddr::V6(Ipv6Addr::LOCALHOST)),
    ]
}

fn public() -> impl Strategy<Value = IpAddr> {
    (1u8..=4, any::<[u8; 2]>(), 1u8..=254, any::<u8>()).prop_map(|(pick, [b, c], d, form)| {
        let first = [8, 93, 151, 203][usize::from(pick - 1)];
        // 203.0.113.0/24 is a documentation range; keep clear of it.
        let ip = Ipv4Addr::new(first, b | 1, c, d);
        // Plain, mapped or NAT64: the forms that stay public when what they carry is public.
        spell(ip, [0, 1, 3][usize::from(form % 3)])
    })
}

fn cidr() -> impl Strategy<Value = Cidr> {
    prop_oneof![
        Just("0.0.0.0/0".parse().unwrap()),
        Just("::/0".parse().unwrap()),
        Just("169.254.0.0/16".parse().unwrap()),
        Just("fd00::/8".parse().unwrap()),
        (any::<[u8; 4]>(), 0u8..=32).prop_map(|(octets, prefix)| {
            format!("{}/{prefix}", Ipv4Addr::from(octets)).parse().unwrap()
        }),
    ]
}

fn policy() -> impl Strategy<Value = EgressPolicy> {
    (any::<bool>(), proptest::collection::vec(cidr(), 0..4)).prop_map(|(strict, allow_cidrs)| {
        EgressPolicy {
            mode: if strict { Mode::Strict } else { Mode::Open },
            allow_cidrs,
            infrastructure: Vec::new(),
        }
    })
}

proptest! {
    #[test]
    fn no_spelling_of_a_metadata_address_passes(ip in metadata(), policy in policy()) {
        prop_assert_eq!(classify(ip), AddrClass::Metadata);
        prop_assert_eq!(policy.check_addr(ip), Err(Refusal::Metadata(ip)));
        let url = match ip {
            IpAddr::V4(ip) => format!("https://{ip}/hook"),
            IpAddr::V6(ip) => format!("https://[{ip}]:8443/hook"),
        };
        prop_assert_eq!(policy.check_url(&url), Err(Refusal::Metadata(ip)));
    }

    #[test]
    fn plain_internal_addresses_have_their_class((ip, class) in internal_v4()) {
        prop_assert_eq!(classify(IpAddr::V4(ip)), class);
    }

    #[test]
    fn strict_mode_refuses_every_internal_address(ip in internal()) {
        let strict = EgressPolicy { mode: Mode::Strict, ..EgressPolicy::default() };
        let refused = matches!(strict.check_addr(ip), Err(Refusal::Range(..)));
        prop_assert!(refused, "{} passed strict mode", ip);
        // The default allows the same address: self-hosted receivers live there.
        prop_assert_eq!(EgressPolicy::default().check_addr(ip), Ok(()));
        // And an allowed range is the way through strict mode.
        let everything = EgressPolicy {
            mode: Mode::Strict,
            allow_cidrs: vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()],
            infrastructure: Vec::new(),
        };
        prop_assert_eq!(everything.check_addr(ip), Ok(()));
    }

    #[test]
    fn public_addresses_pass_in_both_modes(ip in public()) {
        let strict = EgressPolicy { mode: Mode::Strict, ..EgressPolicy::default() };
        prop_assert_eq!(strict.check_addr(ip), Ok(()));
        prop_assert_eq!(EgressPolicy::default().check_addr(ip), Ok(()));
    }
}

/// What the scripted receiver answers.
#[derive(Clone, Debug)]
struct Answer {
    status: u16,
    location: Option<String>,
}

/// A network whose resolver gives the next scripted answer at each call (the last one repeats) and whose
/// every address accepts connections, so a wrong connection would succeed and be recorded.
#[derive(Debug)]
struct ScriptedNet {
    env: Env,
    dns: Mutex<VecDeque<Vec<IpAddr>>>,
    asked: Mutex<Vec<(String, Vec<IpAddr>)>>,
    connects: Mutex<Vec<SocketAddr>>,
    answer: Answer,
}

#[async_trait]
impl Net for ScriptedNet {
    async fn listen(&self, _addr: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
        Err(NetError::Io(std::io::Error::other("no listeners")))
    }

    async fn connect(&self, addr: SocketAddr, _deadline: Instant) -> Result<Stream, NetError> {
        self.connects.lock().unwrap().push(addr);
        let (client, server) = tokio::io::duplex(1 << 16);
        let answer = self.answer.clone();
        self.env.spawn("scripted-receiver", async move {
            let service = hyper::service::service_fn(move |_request| {
                let answer = answer.clone();
                async move {
                    let mut response = http::Response::builder().status(answer.status);
                    if let Some(location) = &answer.location {
                        response = response.header(http::header::LOCATION, location);
                    }
                    response.body(Full::new(Bytes::from_static(b"the receiver's own words")))
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(server), service)
                .await;
        });
        Ok(Stream::new(client))
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        let mut dns = self.dns.lock().unwrap();
        let answer = if dns.len() > 1 {
            dns.pop_front().unwrap()
        } else {
            dns.front().cloned().unwrap_or_default()
        };
        self.asked.lock().unwrap().push((host.to_owned(), answer.clone()));
        Ok(answer)
    }

    async fn tls_connect(
        &self,
        stream: Stream,
        _server_name: &str,
        _roots: &TlsRoots,
    ) -> Result<Stream, NetError> {
        Ok(stream)
    }
}

fn address() -> impl Strategy<Value = IpAddr> {
    prop_oneof![3 => public(), 2 => internal(), 2 => metadata()]
}

fn answer() -> impl Strategy<Value = Answer> {
    let location = prop_oneof![
        Just("http://169.254.169.254/latest/meta-data/iam/".to_owned()),
        Just("http://[fd00:ec2::254]/".to_owned()),
        Just("https://metadata.test/computeMetadata/v1/".to_owned()),
        Just("/elsewhere".to_owned()),
    ];
    prop_oneof![
        1 => Just(Answer { status: 200, location: None }),
        1 => Just(Answer { status: 500, location: None }),
        3 => (prop_oneof![Just(301u16), Just(302), Just(303), Just(307), Just(308)], location)
            .prop_map(|(status, location)| Answer { status, location: Some(location) }),
    ]
}

struct Observed {
    results: Vec<Result<u16, EgressError>>,
    asked: Vec<(String, Vec<IpAddr>)>,
    connects: Vec<SocketAddr>,
}

fn run_calls(
    seed: u64,
    dns: Vec<Vec<IpAddr>>,
    answer: Answer,
    policy: EgressPolicy,
    https: bool,
    calls: usize,
) -> Observed {
    crucible_sim::run(seed, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let net = Arc::new(ScriptedNet {
            env: host.env.clone(),
            dns: Mutex::new(dns.into()),
            asked: Mutex::default(),
            connects: Mutex::default(),
            answer,
        });
        let mut env = host.env.clone();
        env.net = net.clone();
        let egress = Egress::new(&env, policy, TlsRoots::WebPki);
        let url = if https {
            "https://receiver.test/hook"
        } else {
            "http://receiver.test:8080/hook"
        };
        let mut results = Vec::new();
        for _ in 0..calls {
            let reply = egress.post(url, &[], Bytes::from_static(b"{}")).await;
            results.push(reply.map(|reply| reply.status));
        }
        Observed {
            results,
            asked: net.asked.lock().unwrap().clone(),
            connects: net.connects.lock().unwrap().clone(),
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    #[test]
    fn no_resolution_or_redirect_path_reaches_a_metadata_address(
        seed in any::<u64>(),
        dns in proptest::collection::vec(proptest::collection::vec(address(), 0..4), 1..5),
        answer in answer(),
        strict in any::<bool>(),
        https in any::<bool>(),
    ) {
        let policy = EgressPolicy {
            mode: if strict { Mode::Strict } else { Mode::Open },
            ..EgressPolicy::default()
        };
        let calls = 3;
        let seen = run_calls(seed, dns, answer.clone(), policy.clone(), https, calls);

        // The property: nothing was ever connected to a metadata address, and in strict mode to nothing
        // but public addresses.
        for addr in &seen.connects {
            prop_assert_ne!(classify(addr.ip()), AddrClass::Metadata, "connected to {}", addr);
            if strict {
                prop_assert_eq!(classify(addr.ip()), AddrClass::Public, "connected to {}", addr);
            }
        }
        // Only the receiver's own name was ever resolved: a redirect's target never was.
        for (host, _) in &seen.asked {
            prop_assert_eq!(host.as_str(), "receiver.test");
        }
        // One resolution per call, and at most one connection per call, to the first address of that
        // call's answer, which passed the policy whole.
        let plain_http_in_strict = strict && !https;
        if plain_http_in_strict {
            prop_assert!(seen.asked.is_empty() && seen.connects.is_empty());
        } else {
            prop_assert_eq!(seen.asked.len(), calls);
        }
        let mut connects = seen.connects.iter();
        for (call, result) in seen.results.iter().enumerate() {
            if plain_http_in_strict {
                prop_assert_eq!(result, &Err(EgressError::Refused(Refusal::HttpsRequired)));
                continue;
            }
            let answered = &seen.asked[call].1;
            let passes = !answered.is_empty()
                && answered.iter().all(|ip| policy.check_addr(*ip).is_ok());
            match result {
                Ok(status) => {
                    prop_assert!(passes, "call {} got an answer through {:?}", call, answered);
                    // A redirect is returned, not followed.
                    prop_assert_eq!(*status, answer.status);
                    let to = connects.next().expect("an answered call connected once");
                    prop_assert_eq!(to.ip(), answered[0]);
                    prop_assert_eq!(to.port(), if https { 443 } else { 8080 });
                }
                Err(EgressError::Refused(refusal)) => {
                    prop_assert!(!passes, "call {} was refused with {:?}", call, refusal);
                }
                Err(other) => prop_assert!(false, "call {} failed with {:?}", call, other),
            }
        }
        prop_assert!(connects.next().is_none(), "a connection no call accounts for");
    }
}

#[test]
fn a_resolver_that_changes_its_answer_cannot_move_the_call() {
    let good: IpAddr = "93.184.216.34".parse().unwrap();
    let metadata: IpAddr = "169.254.169.254".parse().unwrap();
    // The first call sees the public address; from the second on, the name points at the metadata service.
    let seen = run_calls(
        7,
        vec![vec![good], vec![metadata]],
        Answer {
            status: 204,
            location: None,
        },
        EgressPolicy::default(),
        false,
        3,
    );
    assert_eq!(seen.connects, vec![SocketAddr::new(good, 8080)]);
    assert_eq!(seen.results[0], Ok(204));
    for later in &seen.results[1..] {
        assert_eq!(
            later,
            &Err(EgressError::Refused(Refusal::Metadata(metadata)))
        );
    }
}

#[test]
fn a_name_with_one_refused_address_among_good_ones_is_refused() {
    let good: IpAddr = "93.184.216.34".parse().unwrap();
    let hidden: IpAddr = "::ffff:169.254.169.254".parse().unwrap();
    let seen = run_calls(
        9,
        vec![vec![good, hidden]],
        Answer {
            status: 200,
            location: None,
        },
        EgressPolicy::default(),
        true,
        1,
    );
    assert!(seen.connects.is_empty());
    assert_eq!(
        seen.results,
        vec![Err(EgressError::Refused(Refusal::Metadata(hidden)))]
    );
}

#[test]
fn a_redirect_is_returned_and_not_followed() {
    let good: IpAddr = "10.1.2.3".parse().unwrap();
    let seen = run_calls(
        11,
        vec![vec![good]],
        Answer {
            status: 302,
            location: Some("http://169.254.169.254/latest/".to_owned()),
        },
        EgressPolicy::default(),
        false,
        1,
    );
    assert_eq!(seen.results, vec![Ok(302)]);
    assert_eq!(seen.connects, vec![SocketAddr::new(good, 8080)]);
}
