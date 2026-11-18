//! The ingest service: `POST /v1/traces`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, USER_AGENT};
use axum::response::Response;
use axum::routing::post;
use crucible_admission::{BucketConfig, UNAVAILABLE_MIN_S};
use crucible_auth::{KeyPrincipal, KeyVerifier};
use crucible_env::metrics::registry;
use crucible_env::{Counter, Env};
use crucible_otlp::{JsonError, PartialSuccess, ScanError, TraceRequest, transcode_json};
use crucible_records::{Timestamp, truncate_str};
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
    /// `ingest.project_bytes_per_s` and its burst: the per-project token bucket of contract 4.8, in request
    /// bytes as received (and fact payload bytes). The default is `None`: ingest is not rate-limited and
    /// backpressure governs (contract 11.9). An operator who sets a rate should keep the burst at or above the
    /// largest body a client sends, because a request larger than the burst can never pass.
    pub project_rate: Option<BucketConfig>,
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
            project_rate: None,
            mutant: None,
        }
    }
}

/// OTLP/HTTP trace ingest in front of one WAL. Cloning gives another handle to the same service.
#[derive(Clone, Debug)]
pub struct Ingest {
    pub(crate) inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) env: Env,
    keys: Arc<dyn KeyVerifier>,
    pub(crate) admission: Arc<Admission>,
    max_event_age_micros: i64,
    pub(crate) compression: Compression,
    acks_before_append: bool,
    metrics: Metrics,
    pub(crate) facts: crate::facts::FactMetrics,
}

/// The labels of `crucible_ingest_spans_total`, in the order of
/// [`PartialSuccess::by_reason`] after `accepted`.
const SPAN_OUTCOMES: [&str; 4] = ["accepted", "missing_trace_id", "missing_span_id", "too_old"];

#[derive(Debug)]
struct Metrics {
    accepted: Counter,
    partial: Counter,
    refused: [Counter; Reject::OUTCOMES.len()],
    spans: [Counter; SPAN_OUTCOMES.len()],
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
        }
    }

    fn acknowledged(&self, acked: &Acked) {
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
}

/// What the client is told after its frame is durable.
#[derive(Debug)]
struct Acked {
    spans: u32,
    partial: Option<PartialSuccess>,
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
struct Job {
    wire: Vec<u8>,
    gzip: bool,
    format: Format,
    principal: KeyPrincipal,
    sdk: Sdk,
    /// Spans that start before this are rejected as too old.
    min_start: Timestamp,
}

/// What the envelope takes from the request headers.
#[derive(Debug)]
struct Sdk {
    name: String,
    version: String,
}

impl Sdk {
    /// `sdk_name` and `sdk_version` (contract 4.4): the first product of `User-Agent`, `name/version` as OTLP
    /// exporters send it, cut to the envelope's caps.
    fn from_headers(headers: &HeaderMap) -> Self {
        let agent = headers
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let product = agent.split_ascii_whitespace().next().unwrap_or_default();
        let (name, version) = product.split_once('/').unwrap_or((product, ""));
        Self {
            name: truncate_str(name, MAX_SDK_NAME_BYTES).to_owned(),
            version: truncate_str(version, MAX_SDK_VERSION_BYTES).to_owned(),
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
        Self {
            inner: Arc::new(Inner {
                env: env.clone(),
                keys,
                admission: Arc::new(admission),
                max_event_age_micros: max_age,
                compression: config.compression,
                acks_before_append: config.acks_before_append(),
                metrics: Metrics::new(env),
                facts: crate::facts::FactMetrics::new(env),
            }),
        }
    }

    /// The routes: `POST /v1/traces` (contract 12.6), for the `:8080` and `:4318` listeners alike.
    pub fn router(&self) -> Router {
        Router::new()
            .route("/v1/traces", post(export))
            .with_state(self.clone())
    }
}

/// `POST /v1/traces`. The response is in the request's format; a request in an unsupported format is answered
/// in protobuf, the OTLP default.
async fn export(State(ingest): State<Ingest>, request: Request) -> Response {
    let inner = &ingest.inner;
    let (parts, body) = request.into_parts();
    let format = Format::of(&parts.headers);
    let reply_format = format.unwrap_or(Format::Protobuf);
    match inner.export(&parts.headers, format, body).await {
        Ok(acked) => {
            inner.metrics.acknowledged(&acked);
            reply::accepted(reply_format, acked.partial.as_ref())
        }
        Err(reject) => {
            if let Some(counter) = inner.metrics.refused.get(reject.outcome()) {
                counter.add(1);
            }
            reply::rejected(reply_format, &reject)
        }
    }
}

impl Inner {
    fn authenticate(&self, headers: &HeaderMap) -> Result<KeyPrincipal, Reject> {
        let value = headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(Reject::Unauthenticated)?;
        let (scheme, secret) = value.split_once(' ').ok_or(Reject::Unauthenticated)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(Reject::Unauthenticated);
        }
        self.keys
            .verify(secret.trim())
            .ok_or(Reject::Unauthenticated)
    }

    /// Authenticate, check the headers, admit, read; then decode, scan and prepare the frame off the async
    /// threads; then append, and return only once the frame is durable.
    async fn export(
        self: &Arc<Self>,
        headers: &HeaderMap,
        format: Option<Format>,
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
        let undeclared = (wire.len() as u64).saturating_sub(declared.unwrap_or(0));
        if undeclared > 0
            && !self.admission.take(project, undeclared)
            && let Some(reject) = self.admission.refusal(false, true)
        {
            return Err(reject);
        }
        let job = Job {
            wire,
            gzip,
            format,
            principal,
            sdk: Sdk::from_headers(headers),
            min_start: self
                .env
                .clock
                .wall_now()
                .saturating_add_micros(self.max_event_age_micros.saturating_neg()),
        };
        let inner = Arc::clone(self);
        let mut task = self
            .env
            .spawn_blocking("ingest-prepare", move || inner.prepare(job, held));
        let prepared = task.join().await.map_err(|_| Reject::Unavailable {
            retry_after_s: UNAVAILABLE_MIN_S,
            detail: "the server is shutting down",
        })??;

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

    /// Inflates, transcodes and scans the body, then builds the `IngestRequest` payload and its frame. The
    /// reservation follows the buffers held at each step.
    fn prepare(&self, job: Job, mut held: Held) -> Result<Prepared, Reject> {
        let Job {
            wire,
            gzip,
            format,
            principal,
            sdk,
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
            // Native OTLP carries no request-level dialect hint (contract 10.1).
            compat_dialect: 0,
            transport: IngestTransport::Http,
            content_origin,
            // Import keys arrive with the control plane.
            admin_import: false,
            sdk_name: &sdk.name,
            sdk_version: &sdk.version,
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
