//! Shared by the process tests: the production `Env`, a collector in each of its states, and the checks of
//! the wire contract on what the collector received.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crucible_env::{Env, JoinHandle};
use crucible_env_prod::ProdConfig;
use crucible_fake_collector::registry::Registry;
use crucible_fake_collector::value::{Value, get};
use crucible_fake_collector::{Collector, Event, RecordedSpan};
use crucible_proxy::encode::ResourceInfo;
use crucible_proxy::live::{ExportConfig, Telemetry};
use crucible_proxy::net::Target;
use crucible_proxy::{CoreConfig, ProxyCore};
use crucible_records::CaptureSource;

pub const SECRET: &str = "csk_test0000000000000000000000000000000000000000";

pub fn prod_env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

pub fn loopback() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0)
}

/// The state of the collector for a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sink {
    Healthy,
    /// Nothing listens on the port.
    Refused,
    /// The port accepts connections and never answers.
    Stalled,
    /// No collector is configured.
    Absent,
}

pub const SINKS: [Sink; 4] = [Sink::Healthy, Sink::Refused, Sink::Stalled, Sink::Absent];

/// The telemetry task of a proxy under test and the collector it exports to.
pub struct Sinked {
    pub telemetry: Telemetry,
    collector: Option<Collector>,
    /// Keeps the stalled listener's connections open.
    _stalled: Option<JoinHandle<()>>,
}

pub async fn start_sink(env: &Env, sink: Sink, capture_source: CaptureSource) -> Sinked {
    let mut collector = None;
    let mut stalled = None;
    let endpoint = match sink {
        Sink::Healthy => {
            let started = Collector::start(env).await.unwrap();
            let endpoint = started.endpoint();
            collector = Some(started);
            Some(endpoint)
        }
        Sink::Refused | Sink::Stalled => {
            let mut listener = env.net.listen(loopback()).await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            if sink == Sink::Stalled {
                stalled = Some(env.spawn("stalled-collector", async move {
                    let mut open = Vec::new();
                    while let Ok((stream, _)) = listener.accept().await {
                        open.push(stream);
                    }
                }));
            }
            // For `Refused` the listener is dropped here, so the port is closed.
            Some(endpoint)
        }
        Sink::Absent => None,
    };
    let telemetry = match endpoint {
        Some(endpoint) => Telemetry::start(
            env,
            ProxyCore::new(CoreConfig::new(capture_source), env.rng.fork("proxy")),
            ExportConfig {
                endpoint: Target::parse(&endpoint).unwrap(),
                secret_key: SECRET.to_owned(),
                timeout: Duration::from_secs(5),
                resource: ResourceInfo {
                    service_name: "scripted".to_owned(),
                    instance_id: "instance-1".to_owned(),
                    deploy_id: None,
                    capture_source,
                },
            },
        ),
        None => Telemetry::disabled(env),
    };
    Sinked {
        telemetry,
        collector,
        _stalled: stalled,
    }
}

/// What the collector received: the spans, and every export body, decoded, for searches of planted text.
pub struct Received {
    pub spans: Vec<RecordedSpan>,
    pub bodies: Vec<Vec<u8>>,
}

impl Sinked {
    /// Flushes the telemetry (at most the flush deadline) and returns what the collector received.
    pub async fn finish(self) -> Received {
        self.telemetry.finish().await;
        let mut received = Received {
            spans: Vec::new(),
            bodies: Vec::new(),
        };
        let Some(collector) = self.collector else {
            return received;
        };
        for event in collector.events() {
            if let Event::Export(export) = event {
                // The export contract (9.10): path, media type and the project key as a Bearer token.
                assert_eq!(export.status, 200, "{:?}", export.refusal);
                assert_eq!(export.path, "/v1/traces");
                assert_eq!(
                    export.content_type.as_deref(),
                    Some("application/x-protobuf")
                );
                assert_eq!(
                    export.authorization.as_deref(),
                    Some(format!("Bearer {SECRET}").as_str())
                );
                received.spans.extend(export.spans);
                received.bodies.push(export.body);
            }
        }
        received
    }
}

impl Received {
    /// Whether `needle` occurs anywhere in what was exported.
    pub fn contains(&self, needle: &str) -> bool {
        self.bodies
            .iter()
            .any(|body| memchr::memmem::find(body, needle.as_bytes()).is_some())
    }
}

pub fn text<'a>(span: &'a RecordedSpan, key: &str) -> Option<&'a str> {
    get(&span.attributes, key).and_then(Value::as_str)
}

pub fn int(span: &RecordedSpan, key: &str) -> Option<i64> {
    match get(&span.attributes, key) {
        Some(Value::Int(number)) => Some(*number),
        _ => None,
    }
}

pub fn by_id<'a>(spans: &'a [RecordedSpan], id: &str) -> &'a RecordedSpan {
    spans
        .iter()
        .find(|span| text(span, "jsonrpc.request.id") == Some(id))
        .unwrap_or_else(|| panic!("no span for request {id}"))
}

/// Every attribute is a registry key of the right type, nothing forbidden is sent, and the derived fields say
/// which proxy captured the row.
pub fn check_wire_contract(spans: &[RecordedSpan], capture_source: &str) {
    let registry = Registry::parse(
        "registry.json",
        include_bytes!("../../../../conformance/registry.json"),
    )
    .unwrap();
    for span in spans {
        for (key, value) in span.attributes.iter().chain(&span.resource) {
            assert!(registry.key(key).is_some(), "{key} is not a registry key");
            assert!(!registry.is_forbidden(key), "{key} is forbidden");
            assert_eq!(registry.type_error(key, value), None);
        }
        let field = |name: &str| registry.derive(name, span).unwrap_or_default();
        assert_eq!(field("capture_source"), capture_source);
        assert_eq!(field("side"), "server");
        assert!(text(span, "mcp.method.name").is_some());
        assert!(span.end >= span.start);
    }
}
