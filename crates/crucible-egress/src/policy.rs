//! The egress policy (contracts 1.7 and 12.6): which targets a notification may call.
//!
//! - Cloud metadata addresses are refused under every setting, in every spelling.
//! - By default every other `http` and `https` target is allowed, private networks included, because the
//!   receivers of a self-hosted deployment usually live there.
//! - `egress.mode = strict` is the operator's switch: `https` only, and no loopback, private, link-local,
//!   carrier-grade NAT or reserved address (nor an IPv6 form that carries one) outside `egress.allow_cidrs`, and
//!   none of Crucible's own databases or its object store.
//!
//! Every refusal names the setting that changes it.

use std::net::IpAddr;

use http::Uri;

use crate::addr::{AddrClass, Cidr, classify};

/// How strict the policy is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Mode {
    /// The default: any network except the metadata addresses.
    #[default]
    Open,
    /// The operator's switch: public `https` targets only, plus `allow_cidrs`.
    Strict,
}

impl Mode {
    /// The value of `egress.mode`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Strict => "strict",
        }
    }

    /// The mode named by a value of `egress.mode`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "open" => Some(Self::Open),
            "strict" => Some(Self::Strict),
            _ => None,
        }
    }
}

/// The policy of a deployment.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct EgressPolicy {
    /// `egress.mode`.
    pub mode: Mode,
    /// `egress.allow_cidrs`: ranges that strict mode lets through. They never open a metadata address.
    pub allow_cidrs: Vec<Cidr>,
    /// The hosts of Crucible's own databases (ClickHouse, Postgres) and object store, which strict mode refuses.
    pub infrastructure: Vec<String>,
}

/// Why a target is refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// The URL does not parse, has no host, or carries a user name or password.
    InvalidUrl,
    /// The scheme is neither `http` nor `https`.
    Scheme,
    /// Strict mode allows `https` only.
    HttpsRequired,
    /// The host has no address.
    NoAddress,
    /// The address, or an address its name resolves to, is a cloud metadata address.
    Metadata(IpAddr),
    /// Strict mode refuses the address's range.
    Range(IpAddr, AddrClass),
    /// Strict mode refuses Crucible's own databases and object store.
    Infrastructure(String),
}

impl Refusal {
    /// The stable reason, for `error_code` fields and logs.
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::Scheme => "unsupported_scheme",
            Self::HttpsRequired => "https_required",
            Self::NoAddress => "no_address",
            Self::Metadata(_) => "metadata_address",
            Self::Range(_, AddrClass::Loopback) => "loopback_address",
            Self::Range(_, AddrClass::LinkLocal) => "link_local_address",
            Self::Range(_, AddrClass::Private) => "private_address",
            Self::Range(_, AddrClass::Cgnat) => "cgnat_address",
            Self::Range(..) => "reserved_address",
            Self::Infrastructure(_) => "own_infrastructure",
        }
    }

    /// What is refused and which setting changes it (contract 1.7).
    pub fn message(&self) -> String {
        match self {
            Self::InvalidUrl => {
                "the target is not a URL Crucible can call; use the form https://host/path without a user \
                 name or password"
                    .to_owned()
            }
            Self::Scheme => {
                "only http and https targets can be called; use an http:// or https:// URL".to_owned()
            }
            Self::HttpsRequired => {
                "egress.mode = strict allows https targets only; use an https:// URL, or set egress.mode = open \
                 (CRUCIBLE_EGRESS_MODE=open)"
                    .to_owned()
            }
            Self::NoAddress => {
                "the host name of the target has no address; check the name, or the DNS the server uses"
                    .to_owned()
            }
            Self::Metadata(ip) => format!(
                "{ip} is a cloud metadata address, which Crucible never calls; no setting allows it. Point the \
                 channel at the receiver itself"
            ),
            Self::Range(ip, class) => format!(
                "{ip} is {} and egress.mode = strict refuses it; add its range to egress.allow_cidrs \
                 (CRUCIBLE_EGRESS_ALLOW_CIDRS), or set egress.mode = open",
                class.describe()
            ),
            Self::Infrastructure(host) => format!(
                "{host} is one of the databases or the object store of this Crucible deployment, which \
                 egress.mode = strict refuses; set egress.mode = open (CRUCIBLE_EGRESS_MODE=open) to allow it"
            ),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for Refusal {}

/// A target that passed the checks that need no network: where to connect and what to ask for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Target {
    /// Whether the scheme is `https`.
    pub https: bool,
    /// The host without brackets: a name or an address literal.
    pub host: String,
    /// The port: the one in the URL, or the scheme's.
    pub port: u16,
    /// The `Host` header: the URL's authority.
    pub authority: String,
    /// The path and query.
    pub path: String,
}

impl EgressPolicy {
    /// Checks one address the target would be reached at.
    pub fn check_addr(&self, ip: IpAddr) -> Result<(), Refusal> {
        match (classify(ip), self.mode) {
            (AddrClass::Metadata, _) => Err(Refusal::Metadata(ip)),
            (AddrClass::Public, _) | (_, Mode::Open) => Ok(()),
            (_, Mode::Strict) if self.allow_cidrs.iter().any(|cidr| cidr.contains(ip)) => Ok(()),
            (class, Mode::Strict) => Err(Refusal::Range(ip, class)),
        }
    }

    /// Whether `host` names one of the deployment's own databases or its object store.
    pub(crate) fn is_infrastructure(&self, host: &str) -> bool {
        self.infrastructure
            .iter()
            .any(|own| own.eq_ignore_ascii_case(host))
    }

    /// The checks that need no network, for when a channel is saved: the URL's shape and scheme, and its
    /// host when that is an address literal. A host name is resolved and checked again at every send.
    pub fn check_url(&self, url: &str) -> Result<Target, Refusal> {
        let uri: Uri = url.trim().parse().map_err(|_| Refusal::InvalidUrl)?;
        let https = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => false,
            Some(_) => return Err(Refusal::Scheme),
            None => return Err(Refusal::InvalidUrl),
        };
        let authority = uri.authority().ok_or(Refusal::InvalidUrl)?;
        if authority.as_str().contains('@') {
            return Err(Refusal::InvalidUrl);
        }
        let host = authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if host.is_empty() {
            return Err(Refusal::InvalidUrl);
        }
        if let Ok(ip) = host.parse::<IpAddr>() {
            self.check_addr(ip)?;
        }
        if self.mode == Mode::Strict {
            if !https {
                return Err(Refusal::HttpsRequired);
            }
            if self.is_infrastructure(&host) {
                return Err(Refusal::Infrastructure(host));
            }
        }
        Ok(Target {
            https,
            port: authority.port_u16().unwrap_or(if https { 443 } else { 80 }),
            authority: authority.as_str().to_owned(),
            path: uri
                .path_and_query()
                .map_or("/", |path| path.as_str())
                .to_owned(),
            host,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strict() -> EgressPolicy {
        EgressPolicy {
            mode: Mode::Strict,
            allow_cidrs: vec!["10.20.0.0/16".parse().unwrap()],
            infrastructure: vec!["clickhouse.internal".to_owned()],
        }
    }

    #[test]
    fn the_default_allows_private_receivers_over_plain_http() {
        let open = EgressPolicy::default();
        let target = open
            .check_url("http://10.0.0.5:9000/hooks/crucible?x=1")
            .unwrap();
        assert_eq!(
            (
                target.https,
                target.host.as_str(),
                target.port,
                target.path.as_str()
            ),
            (false, "10.0.0.5", 9000, "/hooks/crucible?x=1")
        );
        assert!(open.check_url("http://localhost/hook").is_ok());
        assert!(open.check_url("https://[fd12::7]/hook").is_ok());
        assert_eq!(open.check_url("https://example.com").unwrap().path, "/");
    }

    #[test]
    fn metadata_addresses_are_refused_in_every_mode() {
        for policy in [EgressPolicy::default(), strict()] {
            for url in [
                "http://169.254.169.254/latest/meta-data/",
                "https://[fd00:ec2::254]/",
                "http://[::ffff:169.254.169.254]/",
                "http://[::ffff:a9fe:a9fe]:80/",
            ] {
                let refusal = policy.check_url(url).unwrap_err();
                assert_eq!(refusal.reason(), "metadata_address", "{url}");
                assert!(refusal.message().contains("no setting allows it"));
            }
        }
        // An allowed range never opens a metadata address.
        let wide = EgressPolicy {
            mode: Mode::Strict,
            allow_cidrs: vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()],
            infrastructure: Vec::new(),
        };
        assert!(wide.check_addr("169.254.169.254".parse().unwrap()).is_err());
        assert!(wide.check_addr("10.0.0.1".parse().unwrap()).is_ok());
    }

    #[test]
    fn strict_mode_refuses_with_the_setting_that_allows() {
        let strict = strict();
        let refusal = strict.check_url("https://192.168.1.10/hook").unwrap_err();
        assert_eq!(refusal.reason(), "private_address");
        assert!(refusal.message().contains("egress.allow_cidrs"));
        assert!(refusal.message().contains("egress.mode = open"));
        assert!(strict.check_url("https://10.20.3.4/hook").is_ok());
        assert_eq!(
            strict.check_url("http://example.com/hook").unwrap_err(),
            Refusal::HttpsRequired
        );
        assert_eq!(
            strict
                .check_url("https://ClickHouse.internal:8443/")
                .unwrap_err()
                .reason(),
            "own_infrastructure"
        );
        assert_eq!(
            strict.check_url("https://127.0.0.1/").unwrap_err().reason(),
            "loopback_address"
        );
    }

    #[test]
    fn malformed_targets_are_refused() {
        let open = EgressPolicy::default();
        for url in [
            "",
            "example.com/hook",
            "/hook",
            "https://user:pw@example.com/",
        ] {
            assert_eq!(
                open.check_url(url).unwrap_err(),
                Refusal::InvalidUrl,
                "{url}"
            );
        }
        for url in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "gopher://example.com/",
        ] {
            let refusal = open.check_url(url).unwrap_err();
            assert!(
                matches!(refusal, Refusal::Scheme | Refusal::InvalidUrl),
                "{url}"
            );
        }
    }
}
