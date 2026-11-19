//! The ingest service: `POST /v1/traces` and the compat ingest routes.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, USER_AGENT};
use axum::response::Response;
use axum::routing::post;
use crucible_admission::{BucketConfig, PermitPool, UNAVAILABLE_MIN_S};
use crucible_auth::{KeyPrincipal, KeyVerifier, Refusal};
use crucible_env::metrics::registry;
use crucible_env::{Counter, Env, Histogram};
use crucible_otlp::{JsonError, PartialSuccess, ScanError, TraceRequest, transcode_json};
use crucible_records::{ProjectId, Timestamp, truncate_str};
use crucible_wal::{AppendError, Wal, WalConfig};
use crucible_wal_codec::{
    Compression, ContentOrigin, FRAME_HEADER_LEN, IngestRequest, IngestTransport,
    MAX_ENVELOPE_BYTES, MAX_SDK_NAME_BYTES, MAX_SDK_VERSION_BYTES, PreparedFrame, Record,
    RecordType,
};

use crate::admission::{Admission, Held};
use crate::body::{self, Format, MAX_GZIP_BYTES, MAX_PLAIN_BYTES};
use crate::reply::{self, Reject};

/// Default `ingest.inflight_bytes` (default; revisit): room for several requests at the body cap, each of
/// which holds about two canonical bodies at its peak.
pub const DEFAULT_INFLIGHT_BYTES: u64 = 512 << 20;

/// Default `ingest.max_event_age` (contract 1.2).
pub const DEFAULT_MAX_EVENT_AGE: Duration = Duration::from_secs(400 * 24 * 60 * 60);

/// Default ingest-prepare share of the CPU budget (default; revisit). The binary sets the share from `cpu.permits`,
/// whose default it computes from the CPUs the process may use: product code never reads the core count.
pub const DEFAULT_PREPARE_PERMITS: u32 = 4;

/// The largest uncompressed body that is prepared on the async worker instead of in a blocking job. Up to this
/// size preparing costs about as much as the hop to a blocking thread and its way back, so the hop would only add
/// latency and a thread wake-up, and the worker stays responsive (`benches/prepare.rs` measures both). A gzip body
/// always takes the hop: its inflated size is unknown until it is inflated.
const INLINE_PREPARE_MAX_BYTES: usize = 16 << 10;

/// Settings of the ingest path.
/// A seeded defect for the simulation suite, which must catch it.
///
/// Without the cargo feature `test-mutants` this type has no value, so [`IngestConfig::mutant`] is always `None`
/// and no defect can be switched on. Only `crucible-sim` enables the feature.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// Answer 200 once the frame is handed to the WAL, without waiting for the append to resolve.
    #[cfg(feature = "test-mutants")]
    AckBeforeAppend,
}

/// A compat ingest route (contract 12.1): a path served like `/v1/traces` whose requests carry a dialect hint.
/// The routes themselves are data of the compat registry, which the binary hands in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CompatRoute {
    /// The request path.
    pub path: &'static str,
    /// The envelope's `compat_dialect` for every request on the path.
    pub dialect: u8,
    /// The lowercase names of the headers that carry the SDK's name and version, in that order. When either is
    /// present, `sdk_name` and `sdk_version` come from them instead of `User-Agent`.
    pub sdk_headers: Option<(&'static str, &'static str)>,
}

/// A request header whose presence on `/v1/traces` sets a dialect hint (contract 12.1). It never selects a
/// project.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoutingHeader {
    /// The lowercase header name.
    pub name: &'static str,
    /// The envelope's `compat_dialect` when the header is present.
    pub dialect: u8,
}

#[derive(Clone, Debug)]
pub struct IngestConfig {
    /// `ingest.inflight_bytes`: the budget for the request bytes that handlers hold, from the first body byte
    /// until the request's frame is durable.
    pub inflight_bytes: u64,
    /// `ingest.max_event_age`: a span that starts more than this long before the request arrives is rejected
    /// through partial success. Milestone 1 has no per-project retention, which may only shorten it.
    pub max_event_age: Duration,
    /// `wal.compression` for the frames that ingest prepares.
    pub compression: Compression,
    /// The ingest-prepare share of the CPU budget (`cpu.permits`): the most blocking jobs that inflate, transcode,
    /// scan and compress request bodies at once. A request beyond it waits for a permit, first come, first
    /// served, on its async task, so it holds no thread while it waits.
    pub prepare_permits: u32,
    /// `ingest.project_bytes_per_s` and its burst: the per-project token bucket of contract 4.8, in request
    /// bytes as received (and fact payload bytes). The default is `None`: ingest is not rate-limited and
    /// backpressure governs (contract 11.9). An operator who sets a rate should keep the burst at or above the
    /// largest body a client sends, because a request larger than the burst can never pass.
    pub project_rate: Option<BucketConfig>,
    /// The compat ingest routes that [`Ingest::router`] serves besides `/v1/traces`; none by default.
    pub compat_routes: Vec<CompatRoute>,
    /// The routing headers that set a hint on `/v1/traces`, checked in order; none by default.
    pub routing_headers: Vec<RoutingHeader>,
    /// A seeded defect; `None` outside the simulation suite.
    pub mutant: Option<Mutant>,
}

impl IngestConfig {
    fn acks_before_append(&self) -> bool {
        #[cfg(feature = "test-mutants")]
        return self.mutant == Some(Mutant::AckBeforeAppend);
        #[cfg(not(feature = "test-mutants"))]
        false
    }
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            inflight_bytes: DEFAULT_INFLIGHT_BYTES,
            max_event_age: DEFAULT_MAX_EVENT_AGE,
            compression: Compression::Lz4,
            prepare_permits: DEFAULT_PREPARE_PERMITS,
            project_rate: None,
            compat_routes: Vec::new(),
            routing_headers: Vec::new(),
            mutant: None,
        }
    }
}

/// OTLP trace ingest in front of one WAL. Cloning gives another handle to the same service.
#[derive(Clone, Debug)]
pub struct Ingest {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) env: Env,
    keys: Arc<dyn KeyVerifier>,
    pub(crate) admission: Arc<Admission>,
    /// The CPU budget's ingest-prepare share.
    permits: Arc<PermitPool>,
    max_event_age_micros: i64,
    pub(crate) compression: Compression,
    acks_before_append: bool,
    compat_routes: Vec<CompatRoute>,
    routing_headers: Vec<RoutingHeader>,
    pub(crate) metrics: Metrics,
    pub(crate) facts: crate::facts::FactMetrics,
}

/// The labels of `crucible_ingest_spans_total`, in the order of
/// [`PartialSuccess::by_reason`] after `accepted`.
const SPAN_OUTCOMES: [&str; 4] = ["accepted", "missing_trace_id", "missing_span_id", "too_old"];

/// The stage label of ingest's blocking preparations in the CPU budget's metrics.
const PREPARE_STAGE: &str = "ingest_prepare";

#[derive(Debug)]
pub(crate) struct Metrics {
    accepted: Counter,
    partial: Counter,
    refused: [Counter; Reject::OUTCOMES.len()],
    spans: [Counter; SPAN_OUTCOMES.len()],
    /// How long a blocking preparation waited for its permit, and how long it held it.
    permit_wait: Histogram,
    permit_hold: Histogram,
}

impl Metrics {
    fn new(env: &Env) -> Self {
        let m = env.metrics.as_ref();
        let requests = |outcome| m.counter(&registry::INGEST_REQUESTS, &[outcome]);
        Self {
            accepted: requests("accepted"),
            partial: requests("partial"),
            refused: Reject::OUTCOMES.map(requests),
            spans: SPAN_OUTCOMES.map(|outcome| m.counter(&registry::INGEST_SPANS, &[outcome])),
            permit_wait: m.histogram(&registry::CPU_PERMIT_WAIT, &[PREPARE_STAGE]),
            permit_hold: m.histogram(&registry::CPU_PERMIT_HOLD, &[PREPARE_STAGE]),
        }
    }

    pub(crate) fn acknowledged(&self, acked: &Acked) {
        let rejected = acked.partial.map_or(0, |p| p.rejected_spans());
        if let Some(partial) = acked.partial {
            self.partial.add(1);
            for ((_, n), counter) in partial.by_reason().iter().zip(&self.spans[1..]) {
                counter.add(u64::from(*n));
            }
        } else {
            self.accepted.add(1);
        }
        self.spans[0].add(u64::from(acked.spans.saturating_sub(rejected)));
    }

    pub(crate) fn refused(&self, reject: &Reject) {
        if let Some(counter) = self.refused.get(reject.outcome()) {
            counter.add(1);
        }
    }
}

/// What the client is told after its frame is durable.
#[derive(Debug)]
pub(crate) struct Acked {
    spans: u32,
    pub(crate) partial: Option<PartialSuccess>,
}

/// A frame ready for the WAL, and the reservation that covers it until it is durable.
#[derive(Debug)]
struct Prepared {
    frame: PreparedFrame,
    acked: Acked,
    held: Held,
}

/// A received body and what its frame needs from the request.
#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) wire: Vec<u8>,
    pub(crate) gzip: bool,
    pub(crate) format: Format,
    pub(crate) transport: IngestTransport,
    pub(crate) principal: KeyPrincipal,
    pub(crate) origin: Origin,
    /// Spans that start before this are rejected as too old.
    pub(crate) min_start: Timestamp,
}

/// What the envelope takes from the route and the request headers (contracts 4.4, 10.1 and 12.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    /// `compat_dialect`: the request-level dialect hint, 0 for none.
    dialect: u8,
    sdk_name: String,
    sdk_version: String,
}

/// A header's value as text, when it is present and visible ASCII.
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

impl Origin {
    /// The hint and the SDK of a request. On a compat route the hint is the route's, and the SDK comes from the
    /// route's SDK headers when either is present. Elsewhere the first routing header present sets the hint.
    /// Otherwise the SDK is the first product of `User-Agent`, `name/version` as OTLP exporters send it. Both
    /// are cut to the envelope's caps.
    pub(crate) fn of(
        headers: &HeaderMap,
        route: Option<&CompatRoute>,
        routing: &[RoutingHeader],
    ) -> Self {
        let dialect = match route {
            Some(route) => route.dialect,
            None => routing
                .iter()
                .find(|r| headers.contains_key(r.name))
                .map_or(0, |r| r.dialect),
        };
        let from_sdk_headers = route
            .and_then(|r| r.sdk_headers)
            .and_then(|(name, version)| {
                let (name, version) = (header(headers, name), header(headers, version));
                (name.is_some() || version.is_some())
                    .then(|| (name.unwrap_or_default(), version.unwrap_or_default()))
            });
        let (name, version) = from_sdk_headers.unwrap_or_else(|| {
            let agent = header(headers, USER_AGENT.as_str()).unwrap_or_default();
            let product = agent.split_ascii_whitespace().next().unwrap_or_default();
            product.split_once('/').unwrap_or((product, ""))
        });
        Self {
            dialect,
            sdk_name: truncate_str(name.trim(), MAX_SDK_NAME_BYTES).to_owned(),
            sdk_version: truncate_str(version.trim(), MAX_SDK_VERSION_BYTES).to_owned(),
        }
    }
}

/// The envelope's `rejected` bitmap (contracts 4.4 and 4.10): bit `ordinal % 8`, least significant first, of
/// byte `ordinal / 8` is set for each span rejected through partial success. The bitmap ends at the byte of
/// the last rejected span, so a request without rejections has an empty one.
fn rejected_bitmap(request: &TraceRequest<'_>) -> Vec<u8> {
    let mut bitmap = Vec::new();
    if request.partial_success().is_none() {
        return bitmap;
    }
    for span in request.spans().filter(|span| span.rejection().is_some()) {
        let ordinal = span.ordinal() as usize;
        if bitmap.len() <= ordinal / 8 {
            bitmap.resize(ordinal / 8 + 1, 0);
        }
        if let Some(byte) = bitmap.get_mut(ordinal / 8) {
            *byte |= 1 << (ordinal % 8);
        }
    }
    bitmap
}

/// Whole microseconds, saturating.
fn micros(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

fn json_reject(e: JsonError) -> Reject {
    match e {
        JsonError::BodyTooLarge { .. } => Reject::TooLarge(e.to_string()),
        JsonError::Invalid(_) => Reject::Malformed(e.to_string()),
    }
}

fn scan_reject(e: ScanError) -> Reject {
    match e {
        ScanError::BodyTooLarge { .. } | ScanError::TooManySpans => Reject::TooLarge(e.to_string()),
        ScanError::Malformed(_) => Reject::Malformed(e.to_string()),
    }
}

impl Ingest {
    /// The service over `wal`, whose settings are `wal_config`. Requests authenticate with `keys`.
    pub fn new(
        env: &Env,
        wal: Wal,
        wal_config: &WalConfig,
        keys: Arc<dyn KeyVerifier>,
        config: IngestConfig,
    ) -> Self {
        let gauge = env.metrics.gauge(&registry::INGEST_INFLIGHT_BYTES, &[]);
        let admission = Admission::new(
            wal,
            wal_config,
            env.rng.fork("crucible-ingest/retry-after"),
            config.inflight_bytes,
            gauge,
            config.project_rate,
            Arc::clone(&env.clock),
        );
        let max_age = i64::try_from(config.max_event_age.as_micros()).unwrap_or(i64::MAX);
        let acks_before_append = config.acks_before_append();
        Self {
            inner: Arc::new(Inner {
                env: env.clone(),
                keys,
                admission: Arc::new(admission),
                permits: Arc::new(PermitPool::new(config.prepare_permits.max(1))),
                max_event_age_micros: max_age,
                compression: config.compression,
                acks_before_append,
                compat_routes: config.compat_routes,
                routing_headers: config.routing_headers,
                metrics: Metrics::new(env),
                facts: crate::facts::FactMetrics::new(env),
            }),
        }
    }

    /// The HTTP routes for the `:8080` and `:4318` listeners alike: `POST /v1/traces` (contract 12.6) and
    /// each compat ingest route of the configuration.
    pub fn router(&self) -> Router {
        let mut router = Router::new().route(
            "/v1/traces",
            post(|State(ingest): State<Ingest>, request: Request| export(ingest, None, request)),
        );
        for route in &self.inner.compat_routes {
            let route = *route;
            router = router.route(
                route.path,
                post(move |State(ingest): State<Ingest>, request: Request| {
                    export(ingest, Some(route), request)
                }),
            );
        }
        router.with_state(self.clone())
    }

    /// The gRPC routes for the `:4317` listener: `TraceService/Export` (contract 12.1); any other method answers
    /// `UNIMPLEMENTED`.
    pub fn grpc_router(&self) -> Router {
        Router::new()
            .route(crate::grpc::EXPORT_PATH, post(crate::grpc::export))
            .fallback(crate::grpc::unimplemented)
            .with_state(self.clone())
    }
}

/// An OTLP/HTTP export on `/v1/traces` or a compat route. The response is in the request's format; a request in
/// an unsupported format is answered in protobuf, the OTLP default.
async fn export(ingest: Ingest, route: Option<CompatRoute>, request: Request) -> Response {
    let inner = &ingest.inner;
    let (parts, body) = request.into_parts();
    let format = Format::of(&parts.headers);
    let reply_format = format.unwrap_or(Format::Protobuf);
    let origin = Origin::of(&parts.headers, route.as_ref(), &inner.routing_headers);
    match inner.export(&parts.headers, format, origin, body).await {
        Ok(acked) => {
            inner.metrics.acknowledged(&acked);
            reply::accepted(reply_format, acked.partial.as_ref())
        }
        Err(reject) => {
            inner.metrics.refused(&reject);
            reply::rejected(reply_format, &reject)
        }
    }
}

impl Inner {
    /// The principal of the request's `Authorization` header (contract 12.2): a secret key as Bearer, or a
    /// public and secret key pair as Basic.
    pub(crate) fn authenticate(&self, headers: &HeaderMap) -> Result<KeyPrincipal, Reject> {
        let value = headers
            .get(AUTHORIZATION)
            .ok_or(Reject::Unauthenticated(Refusal::Missing))?;
        let value = value
            .to_str()
            .map_err(|_| Reject::Unauthenticated(Refusal::Malformed))?;
        crucible_auth::authenticate(self.keys.as_ref(), value).map_err(Reject::Unauthenticated)
    }

    /// The earliest start time a span of a request arriving now may have.
    pub(crate) fn min_start(&self) -> Timestamp {
        self.env
            .clock
            .wall_now()
            .saturating_add_micros(self.max_event_age_micros.saturating_neg())
    }

    /// Charges `bytes` that a request sent beyond what it declared to the project's rate.
    pub(crate) fn charge(&self, project: ProjectId, bytes: u64) -> Result<(), Reject> {
        if bytes > 0
            && !self.admission.take(project, bytes)
            && let Some(reject) = self.admission.refusal(false, true)
        {
            return Err(reject);
        }
        Ok(())
    }

    /// Authenticate, check the headers, admit, read; then decode, scan, prepare and append the frame
    /// ([`Inner::commit`]).
    async fn export(
        self: &Arc<Self>,
        headers: &HeaderMap,
        format: Option<Format>,
        origin: Origin,
        body: Body,
    ) -> Result<Acked, Reject> {
        let principal = self.authenticate(headers)?;
        let format = format.ok_or_else(|| {
            Reject::UnsupportedMediaType(
                "Content-Type must be application/x-protobuf or application/json".to_owned(),
            )
        })?;
        let gzip = body::is_gzip(headers)?;
        let cap = if gzip {
            MAX_GZIP_BYTES
        } else {
            MAX_PLAIN_BYTES
        };
        let declared = body::declared_len(headers)?;
        if declared.is_some_and(|len| len > cap as u64) {
            return Err(body::too_large("the request body", cap));
        }
        let project = principal.scope.project_id();
        let mut held = self.admission.admit(project, declared.unwrap_or(0))?;
        let wire = body::read(body, cap, declared, &mut held).await?;
        // A body without a declared length is charged to the project's rate once its size is known.
        self.charge(
            project,
            (wire.len() as u64).saturating_sub(declared.unwrap_or(0)),
        )?;
        let job = Job {
            wire,
            gzip,
            format,
            transport: IngestTransport::Http,
            principal,
            origin,
            min_start: self.min_start(),
        };
        self.commit(job, held).await
    }

    /// Decodes, scans and prepares the frame, on the async worker for a small uncompressed body and otherwise in a
    /// blocking job; then appends it, and returns only once the frame is durable.
    pub(crate) async fn commit(self: &Arc<Self>, job: Job, held: Held) -> Result<Acked, Reject> {
        let prepared = if !job.gzip && job.wire.len() <= INLINE_PREPARE_MAX_BYTES {
            self.prepare(job, held)?
        } else {
            self.prepare_blocking(job, held).await?
        };

        let Prepared { frame, acked, held } = prepared;
        let appended = self.admission.wal.append(frame);
        if self.acks_before_append {
            drop(appended);
        } else {
            appended.await.map_err(|e| self.append_failed(e))?;
        }
        drop(held);
        Ok(acked)
    }

    /// Prepares the frame in a blocking job, which takes a permit of the CPU budget's ingest-prepare share before
    /// it is spawned and gives it back when it ends. The wait for the permit and the time it is held are recorded.
    async fn prepare_blocking(self: &Arc<Self>, job: Job, held: Held) -> Result<Prepared, Reject> {
        let asked = self.env.clock.now();
        let permit = self.permits.acquire().await;
        let granted = self.env.clock.now();
        self.metrics
            .permit_wait
            .record(micros(granted.saturating_duration_since(asked)));
        let inner = Arc::clone(self);
        let mut task = self.env.spawn_blocking("ingest-prepare", move || {
            let prepared = inner.prepare(job, held);
            let hold = inner.env.clock.now().saturating_duration_since(granted);
            inner.metrics.permit_hold.record(micros(hold));
            drop(permit);
            prepared
        });
        task.join().await.map_err(|_| Reject::Unavailable {
            retry_after_s: UNAVAILABLE_MIN_S,
            detail: "the server is shutting down",
        })?
    }

    /// Inflates, transcodes and scans the body, then builds the `IngestRequest` payload and its frame. The
    /// reservation follows the buffers held at each step.
    fn prepare(&self, job: Job, mut held: Held) -> Result<Prepared, Reject> {
        let Job {
            wire,
            gzip,
            format,
            transport,
            principal,
            origin,
            min_start,
        } = job;
        let decoded = if gzip {
            let plain = body::inflate(&wire, wire.len() as u64, &mut held)?;
            drop(wire);
            held.set(plain.len() as u64)?;
            plain
        } else {
            wire
        };
        let canonical = match format {
            Format::Protobuf => decoded,
            Format::Json => {
                let protobuf = transcode_json(&decoded).map_err(json_reject)?;
                held.grow_to((decoded.len() + protobuf.len()) as u64)?;
                drop(decoded);
                held.set(protobuf.len() as u64)?;
                protobuf
            }
        };

        let request = TraceRequest::scan(&canonical, Some(min_start)).map_err(scan_reject)?;
        let acked = Acked {
            spans: request.span_count(),
            partial: request.partial_success(),
        };
        let rejected = rejected_bitmap(&request);
        let content_origin = match format {
            Format::Protobuf => ContentOrigin::Protobuf,
            Format::Json => ContentOrigin::Json,
        };
        let envelope = IngestRequest {
            project_id: principal.scope.project_id(),
            api_key_id: principal.api_key_id,
            compat_dialect: origin.dialect,
            transport,
            content_origin,
            // Import keys arrive with the control plane.
            admin_import: false,
            sdk_name: &origin.sdk_name,
            sdk_version: &origin.sdk_version,
            // Idempotency keys are not honoured on OTLP ingest (contract 12.5).
            idempotency_key: &[],
            span_count: acked.spans,
            rejected: &rejected,
            otlp: &canonical,
        };

        held.grow_to((2 * canonical.len() + MAX_ENVELOPE_BYTES) as u64)?;
        let payload = Record::IngestRequest(envelope)
            .encode()
            .map_err(|_| Reject::Internal("the request does not fit the WAL payload layout"))?;
        drop(canonical);
        // The frame keeps a buffer as large as the payload whether or not it compresses; LZ4's worst case is
        // the payload plus 1/255 of it plus a few bytes.
        let payload_len = payload.len() as u64;
        held.set(payload_len)?;
        held.grow_to(2 * payload_len + payload_len / 255 + 32)?;
        let frame = PreparedFrame::new(RecordType::IngestRequest, payload, self.compression)
            .map_err(|_| Reject::Internal("the request does not fit a WAL frame"))?;
        held.set(payload_len + FRAME_HEADER_LEN as u64)?;
        Ok(Prepared { frame, acked, held })
    }

    pub(crate) fn append_failed(&self, error: AppendError) -> Reject {
        match error {
            AppendError::TooLarge { len, max } => Reject::TooLarge(format!(
                "the request's WAL frame of {len} bytes is above the segment capacity of {max} bytes"
            )),
            AppendError::InvalidControl => Reject::Internal("the WAL refused the frame"),
            AppendError::Unavailable | AppendError::Failed | AppendError::Closed => {
                self.admission.append_failed()
            }
        }
    }
}
