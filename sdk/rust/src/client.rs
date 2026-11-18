//! The process-wide client: options, `init`, `flush`, `shutdown` and `stats`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockWriteGuard};
use std::time::Duration;

use http::{HeaderValue, Uri};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_http::hyper::HyperClient;
use opentelemetry_otlp::{Protocol, WithExportConfig as _, WithHttpConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::resource::TelemetryResourceDetector;
use opentelemetry_sdk::trace::{IdGenerator as _, RandomIdGenerator, Sampler, SdkTracer};

use crate::export::{self, Pipeline};
use crate::keys;

/// The SDK version, sent in the `User-Agent` of every export and as the instrumentation scope version.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The instrumentation scope of every span the SDK starts.
pub(crate) const SCOPE: &str = "crucible-sdk";

const DEFAULT_QUEUE_SIZE: usize = 2048;
const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_EXPORT_TIMEOUT: Duration = Duration::from_secs(10);

/// Options of [`init`]. Only `endpoint` and `secret_key` are required.
#[derive(Clone, Default)]
pub struct Options {
    /// Base URL of the Crucible server, for example `http://localhost:4318`. Spans are sent to
    /// `endpoint + "/v1/traces"` as OTLP protobuf. Plain HTTP works.
    pub endpoint: String,
    /// A project secret key (`csk_...`) with the ingest scope, sent as `Authorization: Bearer <key>`.
    pub secret_key: String,
    /// Resource `service.name`. An MCP server's name defaults to the name its handler declares.
    pub service_name: String,
    /// Resource `service.version`.
    pub service_version: String,
    /// Resource `deployment.environment.name`.
    pub environment: String,
    /// `crucible.release` on every span; when empty, Crucible uses the service version.
    pub release: String,
    /// Resource `crucible.deploy.id`; when empty, Crucible uses the MCP server version (contract 9.7).
    pub deploy_id: String,
    /// Finished spans waiting for export. When the queue is full a span is dropped and counted in [`stats`].
    /// 0 means 2,048.
    pub queue_size: usize,
    /// How often queued spans are exported. Zero means one second.
    pub flush_interval: Duration,
    /// Bound on one export, and on each [`flush`] and [`shutdown`]. Zero means ten seconds.
    pub export_timeout: Duration,
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("endpoint", &self.endpoint)
            .field("secret_key", &"<redacted>")
            .field("service_name", &self.service_name)
            .field("service_version", &self.service_version)
            .field("environment", &self.environment)
            .field("release", &self.release)
            .field("deploy_id", &self.deploy_id)
            .field("queue_size", &self.queue_size)
            .field("flush_interval", &self.flush_interval)
            .field("export_timeout", &self.export_timeout)
            .finish()
    }
}

/// Why [`init`] refused. Each message says which option to change.
#[derive(Debug)]
#[non_exhaustive]
pub enum InitError {
    /// `endpoint` is empty.
    MissingEndpoint,
    /// `endpoint` is not an `http` or `https` URL.
    InvalidEndpoint(String),
    /// `secret_key` is empty.
    MissingSecretKey,
    /// `secret_key` is not a project secret key.
    NotASecretKey,
    /// [`init`] was already called.
    AlreadyInitialized,
    /// The exporter could not start.
    Start(String),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const EXAMPLE: &str =
            "set it to the base URL of your Crucible server, for example http://localhost:4318";
        match self {
            Self::MissingEndpoint => write!(f, "crucible: Options::endpoint is empty; {EXAMPLE}"),
            Self::InvalidEndpoint(endpoint) => write!(
                f,
                "crucible: Options::endpoint {endpoint:?} is not an http or https URL; {EXAMPLE}"
            ),
            Self::MissingSecretKey => f.write_str(
                "crucible: Options::secret_key is empty; set it to a project secret key (csk_...) with the ingest scope",
            ),
            Self::NotASecretKey => f.write_str(
                "crucible: Options::secret_key is not a project secret key; sending telemetry needs a secret key (csk_...) with the ingest scope, not a public key (cpk_...)",
            ),
            Self::AlreadyInitialized => f.write_str(
                "crucible: init was already called; call crucible_sdk::shutdown before calling init again",
            ),
            Self::Start(reason) => write!(f, "crucible: the exporter could not start: {reason}"),
        }
    }
}

impl std::error::Error for InitError {}

/// Why [`flush`] or [`shutdown`] could not export every queued span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushError(pub(crate) String);

impl fmt::Display for FlushError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "crucible: {}", self.0)
    }
}

impl std::error::Error for FlushError {}

/// What happened to finished spans since [`init`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Stats {
    /// Spans the server accepted.
    pub exported: u64,
    /// Spans dropped because the export queue was full.
    pub dropped: u64,
    /// Spans dropped because their export failed.
    pub failed: u64,
}

/// A validated configuration.
#[derive(Debug)]
pub(crate) struct Config {
    pub(crate) traces_url: String,
    pub(crate) secret_key: String,
    pub(crate) release: String,
    pub(crate) queue_size: usize,
    pub(crate) flush_interval: Duration,
    pub(crate) export_timeout: Duration,
    pub(crate) resource: Resource,
}

/// One started exporter and the tracer whose spans it exports.
#[derive(Debug)]
pub(crate) struct Client {
    pub(crate) tracer: SdkTracer,
    /// The key of the multi-round state hashes (contract 9.6).
    #[cfg(any(feature = "mcp", feature = "mcp-client"))]
    pub(crate) secret_key: String,
    pipeline: Pipeline,
}

static ACTIVE: RwLock<Option<Arc<Client>>> = RwLock::new(None);

/// The process-wide client, or `None` before [`init`] and after [`shutdown`].
pub(crate) fn active() -> Option<Arc<Client>> {
    ACTIVE
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn active_mut() -> RwLockWriteGuard<'static, Option<Arc<Client>>> {
    ACTIVE.write().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the process-wide exporter. Spans recorded before `init` are not exported.
///
/// The export runs on a background thread of its own, so `init` may be called inside or outside an async
/// runtime. It returns an error that says which option to change when `options` is incomplete, and when `init`
/// was already called.
pub fn init(options: Options) -> Result<(), InitError> {
    let config = validate(options)?;
    let mut active = active_mut();
    if active.is_some() {
        return Err(InitError::AlreadyInitialized);
    }
    *active = Some(Arc::new(Client::start(config)?));
    Ok(())
}

/// Exports every span that has ended, and returns the export error if there is one. Before [`init`] it does
/// nothing. It waits at most `Options::export_timeout`.
pub async fn flush() -> Result<(), FlushError> {
    match active() {
        Some(client) => client.pipeline.flush().await,
        None => Ok(()),
    }
}

/// Exports what has ended and stops the exporter; [`init`] may be called again afterwards. Spans that end
/// after `shutdown` are not exported. It waits at most `Options::export_timeout`.
pub async fn shutdown() -> Result<(), FlushError> {
    let client = active_mut().take();
    match client {
        Some(client) => client.pipeline.shutdown().await,
        None => Ok(()),
    }
}

/// The counters of the process-wide exporter; zero before [`init`] and after [`shutdown`].
pub fn stats() -> Stats {
    active().map_or_else(Stats::default, |client| client.pipeline.stats())
}

fn validate(options: Options) -> Result<Config, InitError> {
    if options.endpoint.is_empty() {
        return Err(InitError::MissingEndpoint);
    }
    let base = options.endpoint.trim_end_matches('/');
    let uri: Option<Uri> = base.parse().ok();
    if !uri.is_some_and(|u| {
        matches!(u.scheme_str(), Some("http" | "https")) && u.authority().is_some()
    }) {
        return Err(InitError::InvalidEndpoint(options.endpoint));
    }
    if options.secret_key.is_empty() {
        return Err(InitError::MissingSecretKey);
    }
    if !options.secret_key.starts_with("csk_") || HeaderValue::from_str(&options.secret_key).is_err() {
        return Err(InitError::NotASecretKey);
    }
    let mut attributes = vec![KeyValue::new(
        keys::SERVICE_INSTANCE_ID,
        instance_id(),
    )];
    for (key, value) in [
        (keys::SERVICE_NAME, &options.service_name),
        (keys::SERVICE_VERSION, &options.service_version),
        (keys::DEPLOYMENT_ENVIRONMENT, &options.environment),
        (keys::DEPLOY_ID, &options.deploy_id),
    ] {
        if !value.is_empty() {
            attributes.push(KeyValue::new(key, value.clone()));
        }
    }
    // An empty builder: OTEL_* environment variables do not relabel the export.
    let resource = Resource::builder_empty()
        .with_detector(Box::new(TelemetryResourceDetector))
        .with_attributes(attributes)
        .build();
    Ok(Config {
        traces_url: format!("{base}/v1/traces"),
        secret_key: options.secret_key,
        release: options.release,
        queue_size: nonzero(options.queue_size, DEFAULT_QUEUE_SIZE),
        flush_interval: nonzero(options.flush_interval, DEFAULT_FLUSH_INTERVAL),
        export_timeout: nonzero(options.export_timeout, DEFAULT_EXPORT_TIMEOUT),
        resource,
    })
}

fn nonzero<T: Default + PartialEq>(value: T, default: T) -> T {
    if value == T::default() { default } else { value }
}

/// A random version 4 UUID: the process's `service.instance.id`, the group key of last resort (contract 9.4).
fn instance_id() -> String {
    let mut b = RandomIdGenerator::default().new_trace_id().to_bytes();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl Client {
    fn start(config: Config) -> Result<Self, InitError> {
        let exporter = exporter(&config)?;
        let (processor, pipeline) = export::start(exporter, &config).map_err(InitError::Start)?;
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            // Every span is kept: an unsampled remote parent must not suppress an MCP operation, and
            // OTEL_TRACES_SAMPLER cannot turn sampling off.
            .with_sampler(Sampler::AlwaysOn)
            .with_resource(config.resource.clone())
            .with_span_processor(processor)
            .build();
        let tracer = provider
            .tracer_with_scope(InstrumentationScope::builder(SCOPE).with_version(VERSION).build());
        Ok(Self {
            tracer,
            #[cfg(any(feature = "mcp", feature = "mcp-client"))]
            secret_key: config.secret_key,
            pipeline,
        })
    }
}

/// The OTLP/HTTP protobuf exporter, on a hyper client with a rustls connector that uses the ring provider.
fn exporter(config: &Config) -> Result<opentelemetry_otlp::SpanExporter, InitError> {
    let start = |e: &dyn fmt::Display| InitError::Start(e.to_string());
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = match hyper_rustls::HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(provider.clone())
    {
        Ok(tls) => tls,
        // No usable system certificates (a minimal container): trust the bundled Mozilla roots.
        Err(_) => hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(provider)
            .map_err(|e| start(&e))?,
    };
    let connector = tls.https_or_http().enable_http1().build();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {}", config.secret_key))
        .map_err(|_| InitError::NotASecretKey)?;
    authorization.set_sensitive(true);
    // The client sets Authorization on every request, so no OTEL_* header variable can replace the key.
    let client = HyperClient::new(connector, config.export_timeout, Some(authorization));
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_http_client(client)
        .with_endpoint(config.traces_url.clone())
        .with_protocol(Protocol::HttpBinary)
        .with_timeout(config.export_timeout)
        .with_headers(HashMap::from([(
            "user-agent".to_owned(),
            format!("crucible-sdk-rust/{VERSION}"),
        )]))
        .build()
        .map_err(|e| start(&e))
}
