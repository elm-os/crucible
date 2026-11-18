//! `ProxyCore`: the proxy as a state machine with no IO.
//!
//! Inputs are byte chunks with a direction, connection events, clock readings and export results. Outputs are
//! the bytes to forward and finished [`Op`] records in export batches. Nothing here reads a clock, a socket or a
//! process, so the same code runs under the real pumps and under a deterministic simulation.
//!
//! The core has two faces over one bounded queue:
//!
//! - [`Tap`] is the forwarding face. [`Tap::forward`] returns the bytes to write, which are the bytes it was
//!   given, and offers a copy to the queue. A full queue drops the copy and counts it; it never holds the bytes
//!   back. This is invariant I11.
//! - [`ProxyCore`] is the analysis face. [`ProxyCore::step`] takes one queued item, scans it, correlates
//!   requests with responses by (direction, JSON-RPC id) and puts finished operations in a buffer of at most
//!   8 MiB or 10,000 operations that drops its oldest entry when full. [`ProxyCore::next_export`] and
//!   [`ProxyCore::export_done`] move batches out, with backoff while the collector fails.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Clock, Instant, Rng};
use crucible_records::{
    CancelSource, CaptureSource, Direction, Era, ErrorClass, IdentitySource, McpClassification,
    McpErrorAnnotations, McpErrorInput, McpSide, McpStatus, ResultType, SpanId, Timestamp, TraceId,
    Transport, classify_mcp_error,
};

use crate::scan::{JsonId, Message, Scanner};
use crate::sse::SseReader;

/// The first protocol version of the modern era (contract 9.1).
pub const MODERN_VERSION: &str = "2026-07-28";

/// The size charged to the analysis queue for an item that carries no bytes.
const CONTROL_ITEM_BYTES: usize = 64;
/// Sessions and exchanges remembered at once; the oldest is forgotten beyond this.
const MAX_TRACKED: usize = 8_192;

/// One reading of both clocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Now {
    /// Monotonic time, for durations and backoff.
    pub mono: Instant,
    /// Wall-clock time, for span timestamps.
    pub wall: Timestamp,
}

impl Now {
    /// Reads `clock`.
    pub fn read(clock: &dyn Clock) -> Self {
        Self {
            mono: clock.now(),
            wall: clock.wall_now(),
        }
    }
}

/// One byte stream pair: the stdio session, or one HTTP exchange.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StreamId(pub u64);

impl StreamId {
    /// The single stream of a stdio proxy.
    pub const STDIO: Self = Self(0);
}

/// Who made a request, as far as the forwarding layer could tell without handing over the token.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Identity {
    /// How the identity was established; `None` means no identity keys are sent.
    pub source: Option<IdentitySource>,
    /// The token issuer.
    pub issuer: String,
    /// The token subject, raw; ingest hashes it (contract 9.3).
    pub subject: String,
    /// The OAuth client id.
    pub client_id: String,
    /// The tenant.
    pub tenant: String,
}

/// What the analyser may know about an HTTP request: an allowlist of header values and fields derived from
/// the rest. It never holds the `Authorization` value, a cookie or an `Mcp-Param-*` value.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct HeaderView {
    /// `User-Agent`.
    pub user_agent: String,
    /// `Mcp-Method`.
    pub mcp_method: Option<String>,
    /// `Mcp-Name`.
    pub mcp_name: Option<String>,
    /// `MCP-Protocol-Version`.
    pub protocol_version: Option<String>,
    /// The legacy session id, already hashed (contract 9.4).
    pub session_key: Option<String>,
    /// The client address cut to a /24 or /48 prefix.
    pub ip_prefix: String,
    /// The request body can be scanned: it is not compressed.
    pub body_readable: bool,
    /// The identity derived from the token, when identity is switched on.
    pub identity: Identity,
}

/// How an HTTP response body is read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyKind {
    /// One JSON document.
    Json,
    /// A `text/event-stream`.
    Sse,
    /// Not scanned: another media type, or a compressed body.
    Opaque,
}

/// Why an HTTP exchange ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndReason {
    /// The response body ended normally.
    Complete,
    /// The client went away first.
    ClientClosed,
    /// The upstream connection failed or broke.
    UpstreamFailed,
}

/// A seeded defect for the tests of the test suite (feature `test-mutants`). Inert unless a test sets it.
#[cfg(feature = "test-mutants")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mutant {
    /// Changes one byte of some forwarded chunks.
    FlipByte,
    /// Holds a chunk back while the analysis queue is full, as a blocking send would.
    BlockOnFullQueue,
}

/// The settings of a core.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// `proxy_stdio` or `proxy_http`.
    pub capture_source: CaptureSource,
    /// The most bytes of copied chunks waiting for analysis.
    pub queue_bytes: usize,
    /// The most items waiting for analysis.
    pub queue_items: usize,
    /// The most estimated bytes of finished operations waiting for export (8 MiB).
    pub buffer_bytes: usize,
    /// The most finished operations waiting for export (10,000).
    pub buffer_ops: usize,
    /// The most requests waiting for their response.
    pub max_pending: usize,
    /// The most operations in one export.
    pub batch_ops: usize,
    /// How long a finished operation may wait before an export is due.
    pub flush_interval: Duration,
    /// The longest wait between export attempts while the collector fails.
    pub max_backoff: Duration,
    /// The seeded defect.
    #[cfg(feature = "test-mutants")]
    pub mutant: Option<Mutant>,
}

impl CoreConfig {
    /// The contract's defaults for a capture source.
    pub fn new(capture_source: CaptureSource) -> Self {
        Self {
            capture_source,
            queue_bytes: 4 << 20,
            queue_items: 8_192,
            buffer_bytes: 8 << 20,
            buffer_ops: 10_000,
            max_pending: 10_000,
            batch_ops: 512,
            flush_interval: Duration::from_secs(1),
            max_backoff: Duration::from_secs(30),
            #[cfg(feature = "test-mutants")]
            mutant: None,
        }
    }
}

/// One finished MCP operation, ready to become a span (contract 9.1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Op {
    /// The trace: the one of the `traceparent` in `params._meta`, else a new one.
    pub trace_id: TraceId,
    /// A new span id.
    pub span_id: SpanId,
    /// The span of the `traceparent`, when there was one.
    pub parent_span_id: Option<SpanId>,
    /// `mcp.method.name`.
    pub method: String,
    /// The tool or prompt name.
    pub target: String,
    /// `jsonrpc.request.id`; `None` for a notification.
    pub jsonrpc_id: Option<String>,
    /// Who sent the request or notification.
    pub direction: Direction,
    /// The message had no id.
    pub is_notification: bool,
    /// The message was an element of a batch array.
    pub is_batch: bool,
    /// `stdio` or `streamable_http`.
    pub transport: Transport,
    /// When the request was seen.
    pub start: Timestamp,
    /// From the request to its response, cancellation or failure.
    pub duration: Duration,
    /// The protocol version, when one was observed.
    pub protocol_version: String,
    /// The era, when a version was observed.
    pub era: Option<Era>,
    /// The classification of contract 9.2.
    pub classification: McpClassification,
    /// The JSON-RPC error code of the response.
    pub jsonrpc_code: Option<i32>,
    /// The result carried `isError: true`.
    pub is_error: bool,
    /// The HTTP status of the response that carried or replaced the answer.
    pub http_status: Option<u16>,
    /// How the operation was cancelled.
    pub cancel_source: CancelSource,
    /// The kind of result.
    pub result_type: ResultType,
    /// Progress notifications seen for the request.
    pub progress_count: u32,
    /// The size of the request message.
    pub request_bytes: u64,
    /// The size of the response message.
    pub response_bytes: u64,
    /// Content blocks of the result, per type.
    pub content_blocks: BTreeMap<String, u32>,
    /// Self-reported client name.
    pub client_name: String,
    /// Self-reported client version.
    pub client_version: String,
    /// The server's name, from `serverInfo`.
    pub server_name: String,
    /// The server's version.
    pub server_version: String,
    /// The hashed legacy session id.
    pub session_key: Option<String>,
    /// `User-Agent` of the HTTP request.
    pub user_agent: String,
    /// The client address prefix.
    pub ip_prefix: String,
    /// The identity derived from the token.
    pub identity: Identity,
}

impl Op {
    /// A rough size for the export buffer's byte bound.
    fn estimated_bytes(&self) -> usize {
        let strings = self.method.len()
            + self.target.len()
            + self.jsonrpc_id.as_ref().map_or(0, String::len)
            + self.protocol_version.len()
            + self.client_name.len()
            + self.client_version.len()
            + self.server_name.len()
            + self.server_version.len()
            + self.user_agent.len()
            + self.ip_prefix.len()
            + self.identity.issuer.len()
            + self.identity.subject.len()
            + self.identity.client_id.len()
            + self.identity.tenant.len();
        let blocks: usize = self.content_blocks.keys().map(|key| key.len() + 48).sum();
        320 + strings + blocks
    }
}

/// Operations handed out for one export.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Batch {
    /// The operations, oldest first.
    pub ops: Vec<Op>,
}

/// What became of an export.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportResult {
    /// The collector accepted the batch.
    Delivered,
    /// The collector could not be reached, timed out or asked to retry. The batch returns to the buffer.
    Retry,
    /// The collector refused the batch for good (for example a wrong key). The batch is dropped.
    Rejected,
}

/// Counters of a core. All of them only grow.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Chunks forwarded.
    pub forwarded_chunks: u64,
    /// Bytes forwarded.
    pub forwarded_bytes: u64,
    /// Chunk copies dropped because the analysis queue was full.
    pub dropped_chunks: u64,
    /// Bytes of those copies.
    pub dropped_chunk_bytes: u64,
    /// Lines or documents that were not JSON-RPC.
    pub malformed: u64,
    /// Operations finished.
    pub ops: u64,
    /// Finished operations dropped from the export buffer (oldest first) or refused by the collector.
    pub dropped_ops: u64,
    /// Requests forgotten because too many were waiting for a response.
    pub evicted_pending: u64,
    /// Operations the collector accepted.
    pub exported_ops: u64,
    /// Exports that failed and will be retried.
    pub export_failures: u64,
}

#[derive(Debug)]
enum Item {
    Chunk {
        stream: StreamId,
        direction: Direction,
        bytes: Bytes,
    },
    Open {
        stream: StreamId,
        view: Box<HeaderView>,
    },
    Head {
        stream: StreamId,
        status: u16,
        body: BodyKind,
        session_key: Option<String>,
    },
    Close {
        stream: StreamId,
        reason: EndReason,
    },
    SessionEnd,
}

#[derive(Debug)]
struct Queued {
    item: Item,
    now: Now,
    /// Chunk copies were dropped since the item before this one.
    gap: bool,
}

#[derive(Debug, Default)]
struct Shared {
    queue: VecDeque<Queued>,
    bytes: usize,
    gap: bool,
    forwarded_chunks: u64,
    forwarded_bytes: u64,
    dropped_chunks: u64,
    dropped_chunk_bytes: u64,
    /// Chunks held back by the `BlockOnFullQueue` mutant.
    #[cfg(feature = "test-mutants")]
    held: Vec<(StreamId, Direction, Bytes)>,
}

/// The forwarding face of a core. Cheap to clone; every clone feeds the same queue.
#[derive(Clone, Debug)]
pub struct Tap {
    shared: Arc<Mutex<Shared>>,
    queue_bytes: usize,
    queue_items: usize,
    #[cfg(feature = "test-mutants")]
    mutant: Option<Mutant>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Tap {
    /// Takes a chunk read from one side and returns the bytes to write to the other side: the same bytes.
    /// A copy goes to the analysis queue when it has room; otherwise the copy is dropped and counted. This
    /// never waits.
    pub fn forward(&self, stream: StreamId, direction: Direction, bytes: Bytes, now: Now) -> Bytes {
        let mut shared = lock(&self.shared);
        shared.forwarded_chunks += 1;
        shared.forwarded_bytes += bytes.len() as u64;
        let full = shared.queue.len() >= self.queue_items
            || shared.bytes.saturating_add(bytes.len()) > self.queue_bytes;
        #[cfg(feature = "test-mutants")]
        let bytes = match self.mutant {
            Some(Mutant::FlipByte)
                if shared.forwarded_chunks.is_multiple_of(5) && !bytes.is_empty() =>
            {
                let mut changed = bytes.to_vec();
                changed[0] ^= 0x20;
                Bytes::from(changed)
            }
            Some(Mutant::BlockOnFullQueue) => {
                if full {
                    shared.held.push((stream, direction, bytes));
                    return Bytes::new();
                }
                let mut out = Vec::new();
                shared.held.retain(|(held_stream, held_direction, held)| {
                    let same = *held_stream == stream && *held_direction == direction;
                    if same {
                        out.extend_from_slice(held);
                    }
                    !same
                });
                out.extend_from_slice(&bytes);
                Bytes::from(out)
            }
            _ => bytes,
        };
        if full {
            shared.gap = true;
            shared.dropped_chunks += 1;
            shared.dropped_chunk_bytes += bytes.len() as u64;
        } else {
            shared.bytes += bytes.len();
            let gap = std::mem::take(&mut shared.gap);
            shared.queue.push_back(Queued {
                item: Item::Chunk {
                    stream,
                    direction,
                    bytes: bytes.clone(),
                },
                now,
                gap,
            });
        }
        bytes
    }

    fn control(&self, item: Item, now: Now) {
        let mut shared = lock(&self.shared);
        // Control items are small and carry what closes an operation, so they get twice the room of chunks.
        if shared.queue.len() >= self.queue_items.saturating_mul(2) {
            shared.gap = true;
            return;
        }
        shared.bytes += CONTROL_ITEM_BYTES;
        let gap = std::mem::take(&mut shared.gap);
        shared.queue.push_back(Queued { item, now, gap });
    }

    /// An HTTP exchange started.
    pub fn open(&self, stream: StreamId, view: HeaderView, now: Now) {
        self.control(
            Item::Open {
                stream,
                view: Box::new(view),
            },
            now,
        );
    }

    /// The response head of an HTTP exchange arrived. `session_key` is the hashed legacy session id that the
    /// response assigned, if any.
    pub fn head(
        &self,
        stream: StreamId,
        status: u16,
        body: BodyKind,
        session_key: Option<String>,
        now: Now,
    ) {
        self.control(
            Item::Head {
                stream,
                status,
                body,
                session_key,
            },
            now,
        );
    }

    /// An HTTP exchange ended.
    pub fn close(&self, stream: StreamId, reason: EndReason, now: Now) {
        self.control(Item::Close { stream, reason }, now);
    }

    /// The stdio session ended: the server exited or its output closed.
    pub fn session_end(&self, now: Now) {
        self.control(Item::SessionEnd, now);
    }

    /// Whether items wait for analysis.
    pub fn has_work(&self) -> bool {
        !lock(&self.shared).queue.is_empty()
    }
}

/// What groups requests with their responses.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Scope {
    Stdio,
    Exchange(u64),
    Session(String),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
struct PendingKey {
    scope: Scope,
    /// The direction of the request.
    direction: Direction,
    id: JsonId,
}

#[derive(Clone, Debug, Default)]
struct Session {
    protocol_version: Option<String>,
    client_name: String,
    client_version: String,
}

#[derive(Debug)]
struct Pending {
    seq: u64,
    stream: StreamId,
    started: Instant,
    progress_token: Option<JsonId>,
    op: Op,
}

#[derive(Debug)]
enum ResponseReader {
    Lines(Scanner),
    Json(Scanner),
    Sse(SseReader),
    Opaque,
}

#[derive(Debug)]
struct StreamState {
    scope: Scope,
    request: Scanner,
    response: ResponseReader,
    view: HeaderView,
    status: Option<u16>,
    /// The session id the response assigned (the initialize exchange of a legacy session).
    assigned_session: Option<String>,
    /// Requests that arrived in this exchange and may still wait for a response.
    started: Vec<PendingKey>,
    /// A message was read from the request body.
    saw_request: bool,
    opened: Now,
}

impl StreamState {
    fn malformed(&self) -> u64 {
        self.request.malformed()
            + match &self.response {
                ResponseReader::Lines(scanner) | ResponseReader::Json(scanner) => {
                    scanner.malformed()
                }
                ResponseReader::Sse(reader) => reader.malformed(),
                ResponseReader::Opaque => 0,
            }
    }
}

/// What ends an operation.
#[derive(Debug, Default)]
struct Outcome {
    jsonrpc_code: Option<i32>,
    is_error: bool,
    http_status: Option<u16>,
    cancel_source: Option<CancelSource>,
    transport_failure: bool,
    result_type: Option<String>,
    response_bytes: u64,
    content_blocks: BTreeMap<String, u32>,
    protocol_version: Option<String>,
    has_result: bool,
}

/// See the module documentation.
#[derive(Debug)]
pub struct ProxyCore {
    config: CoreConfig,
    shared: Arc<Mutex<Shared>>,
    rng: Arc<dyn Rng>,
    streams: BTreeMap<StreamId, StreamState>,
    sessions: BTreeMap<Scope, Session>,
    /// The server's name and version, once a result has carried them.
    server: Option<(String, String)>,
    pending: BTreeMap<PendingKey, Pending>,
    /// Pending requests by age.
    order: BTreeMap<u64, PendingKey>,
    /// Progress tokens of pending requests: (scope, direction of the request, token).
    progress: BTreeMap<(Scope, Direction, JsonId), PendingKey>,
    next_seq: u64,
    buffer: VecDeque<Op>,
    buffer_bytes: usize,
    /// When the oldest buffered operation finished.
    waiting_since: Option<Instant>,
    in_flight: bool,
    failures: u32,
    next_attempt: Instant,
    /// Operations dropped since the last delivered export.
    unreported_drops: u64,
    stats: Stats,
}

fn opposite(direction: Direction) -> Direction {
    match direction {
        Direction::ClientToServer => Direction::ServerToClient,
        Direction::ServerToClient => Direction::ClientToServer,
    }
}

/// The era of a protocol version: versions are dates, so text order is date order.
fn era_of(version: &str) -> Era {
    if version >= MODERN_VERSION {
        Era::Modern
    } else {
        Era::Legacy
    }
}

/// The trace and parent span of a W3C `traceparent`, when it is well formed.
fn parse_traceparent(text: &str) -> Option<(TraceId, SpanId)> {
    let mut parts = text.split('-');
    let (version, trace, span, _flags) =
        (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if version.len() != 2 || version == "ff" {
        return None;
    }
    let trace = TraceId::parse(trace).ok().filter(|id| !id.is_zero())?;
    let span = SpanId::parse(span).ok()?.non_zero()?;
    Some((trace, span))
}

impl ProxyCore {
    /// A core with an empty queue. `rng` supplies trace and span ids; fork it for the proxy.
    pub fn new(config: CoreConfig, rng: Arc<dyn Rng>) -> Self {
        let mut streams = BTreeMap::new();
        if config.capture_source == CaptureSource::ProxyStdio {
            streams.insert(
                StreamId::STDIO,
                StreamState {
                    scope: Scope::Stdio,
                    request: Scanner::lines(),
                    response: ResponseReader::Lines(Scanner::lines()),
                    view: HeaderView::default(),
                    status: None,
                    assigned_session: None,
                    started: Vec::new(),
                    saw_request: false,
                    opened: Now::default(),
                },
            );
        }
        Self {
            config,
            shared: Arc::default(),
            rng,
            streams,
            sessions: BTreeMap::new(),
            server: None,
            pending: BTreeMap::new(),
            order: BTreeMap::new(),
            progress: BTreeMap::new(),
            next_seq: 0,
            buffer: VecDeque::new(),
            buffer_bytes: 0,
            waiting_since: None,
            in_flight: false,
            failures: 0,
            next_attempt: Instant::default(),
            unreported_drops: 0,
            stats: Stats::default(),
        }
    }

    /// The forwarding face.
    pub fn tap(&self) -> Tap {
        Tap {
            shared: Arc::clone(&self.shared),
            queue_bytes: self.config.queue_bytes,
            queue_items: self.config.queue_items,
            #[cfg(feature = "test-mutants")]
            mutant: self.config.mutant,
        }
    }

    /// The counters.
    pub fn stats(&self) -> Stats {
        let shared = lock(&self.shared);
        Stats {
            forwarded_chunks: shared.forwarded_chunks,
            forwarded_bytes: shared.forwarded_bytes,
            dropped_chunks: shared.dropped_chunks,
            dropped_chunk_bytes: shared.dropped_chunk_bytes,
            ..self.stats
        }
    }

    /// Finished operations waiting for export.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Requests waiting for a response.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    /// Analyses one queued item. Returns false when the queue was empty.
    pub fn step(&mut self) -> bool {
        let queued = {
            let mut shared = lock(&self.shared);
            let Some(queued) = shared.queue.pop_front() else {
                return false;
            };
            let size = match &queued.item {
                Item::Chunk { bytes, .. } => bytes.len(),
                _ => CONTROL_ITEM_BYTES,
            };
            shared.bytes = shared.bytes.saturating_sub(size);
            queued
        };
        if queued.gap {
            for stream in self.streams.values_mut() {
                stream.request.gap();
                match &mut stream.response {
                    ResponseReader::Lines(scanner) | ResponseReader::Json(scanner) => scanner.gap(),
                    ResponseReader::Sse(reader) => reader.gap(),
                    ResponseReader::Opaque => {}
                }
            }
        }
        let now = queued.now;
        match queued.item {
            Item::Chunk {
                stream,
                direction,
                bytes,
            } => self.chunk(stream, direction, &bytes, now),
            Item::Open { stream, view } => self.open(stream, *view, now),
            Item::Head {
                stream,
                status,
                body,
                session_key,
            } => {
                if let Some(state) = self.streams.get_mut(&stream) {
                    state.status = Some(status);
                    state.assigned_session = session_key;
                    state.response = match body {
                        BodyKind::Json => ResponseReader::Json(Scanner::document()),
                        BodyKind::Sse => ResponseReader::Sse(SseReader::new()),
                        BodyKind::Opaque => ResponseReader::Opaque,
                    };
                }
            }
            Item::Close { stream, reason } => self.close(stream, reason, now),
            Item::SessionEnd => self.session_end(now),
        }
        true
    }

    /// Analyses everything queued.
    pub fn drain(&mut self) {
        while self.step() {}
    }

    fn chunk(&mut self, id: StreamId, direction: Direction, bytes: &[u8], now: Now) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let before = stream.malformed();
        let messages = match direction {
            Direction::ClientToServer => {
                if !stream.view.body_readable && stream.scope != Scope::Stdio {
                    return;
                }
                stream.request.push(bytes);
                stream.request.take()
            }
            Direction::ServerToClient => match &mut stream.response {
                ResponseReader::Lines(scanner) | ResponseReader::Json(scanner) => {
                    scanner.push(bytes);
                    scanner.take()
                }
                ResponseReader::Sse(reader) => {
                    reader.push(bytes);
                    reader.take()
                }
                ResponseReader::Opaque => return,
            },
        };
        self.stats.malformed += stream.malformed() - before;
        let view = stream.view.clone();
        for message in messages {
            self.message(id, &view, direction, message, now);
        }
    }

    fn open(&mut self, id: StreamId, view: HeaderView, now: Now) {
        while self.streams.len() >= MAX_TRACKED {
            let Some(oldest) = self.streams.keys().next().copied() else {
                break;
            };
            self.streams.remove(&oldest);
        }
        let scope = match &view.session_key {
            Some(key) => Scope::Session(key.clone()),
            None => Scope::Exchange(id.0),
        };
        self.streams.insert(
            id,
            StreamState {
                scope,
                request: Scanner::document(),
                response: ResponseReader::Opaque,
                view,
                status: None,
                assigned_session: None,
                started: Vec::new(),
                saw_request: false,
                opened: now,
            },
        );
    }

    fn message(
        &mut self,
        id: StreamId,
        view: &HeaderView,
        direction: Direction,
        message: Message,
        now: Now,
    ) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let scope = stream.scope.clone();
        if direction == Direction::ClientToServer {
            stream.saw_request = true;
        }
        if let (Some(name), Some(version)) = (&message.server_name, &message.server_version) {
            self.server = Some((name.clone(), version.clone()));
        } else if let Some(name) = &message.server_name {
            self.server = Some((name.clone(), String::new()));
        }
        match (message.method.clone(), message.id.clone()) {
            (Some(method), Some(request_id)) => {
                self.request(id, view, scope, direction, method, request_id, message, now);
            }
            (Some(method), None) => {
                self.notification(id, view, scope, direction, method, message, now);
            }
            (None, Some(response_id)) if message.has_result || message.has_error => {
                let key = PendingKey {
                    scope,
                    direction: opposite(direction),
                    id: response_id,
                };
                let http_status = self.streams.get(&id).and_then(|stream| stream.status);
                let outcome = Outcome {
                    jsonrpc_code: message.has_error.then(|| message.error_code.unwrap_or(0)),
                    is_error: message.is_error,
                    http_status,
                    result_type: message.result_type,
                    response_bytes: message.bytes,
                    content_blocks: message.content_blocks,
                    protocol_version: message.protocol_version,
                    has_result: message.has_result,
                    ..Outcome::default()
                };
                self.finish(&key, outcome, now);
            }
            _ => {}
        }
    }

    /// The operation of a request or notification, before its outcome is known.
    fn begin(
        &mut self,
        view: &HeaderView,
        scope: &Scope,
        direction: Direction,
        method: String,
        message: &Message,
        now: Now,
    ) -> Op {
        let (trace_id, parent_span_id) =
            match message.traceparent.as_deref().and_then(parse_traceparent) {
                Some((trace, parent)) => (trace, Some(parent)),
                None => (self.new_trace_id(), None),
            };
        let span_id = self.new_span_id();
        // The initialize request opens a legacy session: remember what the client said about itself.
        if method == "initialize" && direction == Direction::ClientToServer {
            if self.sessions.len() >= MAX_TRACKED {
                self.sessions.pop_first();
            }
            self.sessions.insert(
                scope.clone(),
                Session {
                    protocol_version: message.protocol_version.clone(),
                    client_name: message.client_name.clone().unwrap_or_default(),
                    client_version: message.client_version.clone().unwrap_or_default(),
                },
            );
        }
        let session = self.sessions.get(scope);
        let protocol_version = message
            .protocol_version
            .clone()
            .or_else(|| view.protocol_version.clone())
            .or_else(|| session.and_then(|session| session.protocol_version.clone()))
            .unwrap_or_default();
        let from_session = |pick: fn(&Session) -> &String| {
            session
                .map(|session| pick(session).clone())
                .unwrap_or_default()
        };
        let (server_name, server_version) = self.server.clone().unwrap_or_default();
        let transport = if *scope == Scope::Stdio {
            Transport::Stdio
        } else {
            Transport::StreamableHttp
        };
        Op {
            trace_id,
            span_id,
            parent_span_id,
            target: match method.as_str() {
                "tools/call" | "prompts/get" => message.name.clone().unwrap_or_default(),
                _ => String::new(),
            },
            method,
            jsonrpc_id: message.id.as_ref().map(|id| id.text().to_owned()),
            direction,
            is_notification: message.id.is_none(),
            is_batch: message.is_batch,
            transport,
            start: now.wall,
            duration: Duration::ZERO,
            era: (!protocol_version.is_empty()).then(|| era_of(&protocol_version)),
            protocol_version,
            classification: classify_mcp_error(&error_input(&Outcome::default(), None)),
            jsonrpc_code: None,
            is_error: false,
            http_status: None,
            cancel_source: CancelSource::None,
            result_type: ResultType::None,
            progress_count: 0,
            request_bytes: message.bytes,
            response_bytes: 0,
            content_blocks: BTreeMap::new(),
            client_name: message
                .client_name
                .clone()
                .unwrap_or_else(|| from_session(|session| &session.client_name)),
            client_version: message
                .client_version
                .clone()
                .unwrap_or_else(|| from_session(|session| &session.client_version)),
            server_name,
            server_version,
            session_key: view.session_key.clone(),
            user_agent: view.user_agent.clone(),
            ip_prefix: view.ip_prefix.clone(),
            identity: view.identity.clone(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn request(
        &mut self,
        id: StreamId,
        view: &HeaderView,
        scope: Scope,
        direction: Direction,
        method: String,
        request_id: JsonId,
        message: Message,
        now: Now,
    ) {
        let op = self.begin(view, &scope, direction, method, &message, now);
        let key = PendingKey {
            scope: scope.clone(),
            direction,
            id: request_id,
        };
        // A reused id replaces the request that still waited under it.
        self.forget_pending(&key);
        let seq = self.next_seq;
        self.next_seq += 1;
        if let Some(token) = &message.meta_progress_token {
            self.progress
                .insert((scope.clone(), direction, token.clone()), key.clone());
        }
        self.order.insert(seq, key.clone());
        self.pending.insert(
            key.clone(),
            Pending {
                seq,
                stream: id,
                started: now.mono,
                progress_token: message.meta_progress_token,
                op,
            },
        );
        if scope != Scope::Stdio
            && let Some(stream) = self.streams.get_mut(&id)
        {
            stream.started.push(key);
        }
        while self.pending.len() > self.config.max_pending {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.forget_pending(&oldest);
            self.stats.evicted_pending += 1;
        }
    }

    fn forget_pending(&mut self, key: &PendingKey) -> Option<Pending> {
        let pending = self.pending.remove(key)?;
        self.order.remove(&pending.seq);
        if let Some(token) = &pending.progress_token {
            self.progress
                .remove(&(key.scope.clone(), key.direction, token.clone()));
        }
        Some(pending)
    }

    #[allow(clippy::too_many_arguments)]
    fn notification(
        &mut self,
        id: StreamId,
        view: &HeaderView,
        scope: Scope,
        direction: Direction,
        method: String,
        message: Message,
        now: Now,
    ) {
        match method.as_str() {
            // A cancellation names a request the sender made itself.
            "notifications/cancelled" => {
                if let Some(request_id) = message.request_id.clone() {
                    let key = PendingKey {
                        scope: scope.clone(),
                        direction,
                        id: request_id,
                    };
                    let outcome = Outcome {
                        cancel_source: Some(CancelSource::Notification),
                        ..Outcome::default()
                    };
                    self.finish(&key, outcome, now);
                }
            }
            // Progress flows towards whoever made the request.
            "notifications/progress" => {
                if let Some(token) = message.progress_token.clone() {
                    let key = self
                        .progress
                        .get(&(scope.clone(), opposite(direction), token))
                        .cloned();
                    if let Some(pending) = key.and_then(|key| self.pending.get_mut(&key)) {
                        pending.op.progress_count = pending.op.progress_count.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
        let mut op = self.begin(view, &scope, direction, method, &message, now);
        op.http_status = self.streams.get(&id).and_then(|stream| stream.status);
        self.emit(op, now);
    }

    fn finish(&mut self, key: &PendingKey, outcome: Outcome, now: Now) {
        let Some(pending) = self.forget_pending(key) else {
            return;
        };
        let mut op = pending.op;
        op.duration = now.mono.saturating_duration_since(pending.started);
        // The initialize result settles the version of a legacy session.
        if let Some(version) = &outcome.protocol_version
            && op.method == "initialize"
        {
            if let Some(session) = self.sessions.get_mut(&key.scope) {
                session.protocol_version = Some(version.clone());
            }
            op.protocol_version.clone_from(version);
            op.era = Some(era_of(version));
        }
        if let Some((name, version)) = &self.server {
            op.server_name.clone_from(name);
            op.server_version.clone_from(version);
        }
        if let Some(stream) = self.streams.get_mut(&pending.stream) {
            stream.started.retain(|started| started != key);
            if op.session_key.is_none() {
                op.session_key.clone_from(&stream.assigned_session);
            }
        }
        op.classification = classify_mcp_error(&error_input(&outcome, op.era));
        op.jsonrpc_code = outcome.jsonrpc_code;
        op.is_error = outcome.is_error;
        op.http_status = outcome.http_status;
        op.cancel_source = outcome.cancel_source.unwrap_or(CancelSource::None);
        op.response_bytes = outcome.response_bytes;
        op.content_blocks = outcome.content_blocks;
        op.result_type = if outcome.has_result {
            match outcome.result_type.as_deref() {
                Some("input_required") => ResultType::InputRequired,
                Some("task") => ResultType::Task,
                _ => ResultType::Complete,
            }
        } else {
            ResultType::None
        };
        self.emit(op, now);
    }

    fn emit(&mut self, op: Op, now: Now) {
        self.stats.ops += 1;
        self.buffer_bytes += op.estimated_bytes();
        self.buffer.push_back(op);
        self.waiting_since.get_or_insert(now.mono);
        self.trim_buffer();
    }

    /// Drop-oldest: the newest operations are the ones worth keeping through an outage.
    fn trim_buffer(&mut self) {
        while self.buffer.len() > self.config.buffer_ops
            || self.buffer_bytes > self.config.buffer_bytes
        {
            let Some(oldest) = self.buffer.pop_front() else {
                break;
            };
            self.buffer_bytes = self.buffer_bytes.saturating_sub(oldest.estimated_bytes());
            self.stats.dropped_ops += 1;
            self.unreported_drops += 1;
        }
    }

    fn close(&mut self, id: StreamId, reason: EndReason, now: Now) {
        let Some(mut stream) = self.streams.remove(&id) else {
            return;
        };
        let before = stream.malformed();
        stream.request.end_document();
        if let ResponseReader::Json(scanner) = &mut stream.response {
            scanner.end_document();
        }
        self.stats.malformed += stream.malformed() - before;
        // The session id a response assigned names the session that the initialize request opened.
        if let (Some(key), Scope::Exchange(_)) = (&stream.assigned_session, &stream.scope) {
            if let Some(session) = self.sessions.remove(&stream.scope) {
                self.sessions.insert(Scope::Session(key.clone()), session);
            }
        } else if matches!(stream.scope, Scope::Exchange(_)) {
            self.sessions.remove(&stream.scope);
        }
        let status = stream.status;
        let failed_status = status.is_some_and(|status| status >= 400);
        // In a legacy session an answer may still come on another stream of that session.
        let may_answer_elsewhere = matches!(stream.scope, Scope::Session(_))
            && reason == EndReason::Complete
            && !failed_status;
        if !may_answer_elsewhere {
            for key in std::mem::take(&mut stream.started) {
                let outcome = Outcome {
                    http_status: status,
                    cancel_source: (reason == EndReason::ClientClosed)
                        .then_some(CancelSource::StreamClose),
                    transport_failure: reason == EndReason::UpstreamFailed
                        || (reason == EndReason::Complete && !failed_status),
                    ..Outcome::default()
                };
                self.finish(&key, outcome, now);
            }
        }
        // A body that could not be read still names its operation in the request headers.
        if !stream.saw_request
            && let Some(method) = stream.view.mcp_method.clone()
        {
            let message = Message {
                name: stream.view.mcp_name.clone(),
                ..Message::default()
            };
            let scope = Scope::Exchange(id.0);
            let direction = Direction::ClientToServer;
            let mut op = self.begin(&stream.view, &scope, direction, method, &message, now);
            let opened = stream.opened;
            // Without the body there is no id: the row is neither a request with an id nor a notification.
            op.is_notification = false;
            op.start = opened.wall;
            op.duration = now.mono.saturating_duration_since(opened.mono);
            let outcome = Outcome {
                http_status: status,
                cancel_source: (reason == EndReason::ClientClosed)
                    .then_some(CancelSource::StreamClose),
                transport_failure: reason == EndReason::UpstreamFailed,
                has_result: status.is_some_and(|status| status < 400),
                ..Outcome::default()
            };
            op.classification = classify_mcp_error(&error_input(&outcome, op.era));
            op.http_status = status;
            op.cancel_source = outcome.cancel_source.unwrap_or(CancelSource::None);
            self.emit(op, now);
        }
    }

    fn session_end(&mut self, now: Now) {
        let keys: Vec<PendingKey> = self.order.values().cloned().collect();
        for key in keys {
            let outcome = Outcome {
                transport_failure: true,
                ..Outcome::default()
            };
            self.finish(&key, outcome, now);
        }
    }

    fn new_trace_id(&self) -> TraceId {
        loop {
            let mut bytes = [0u8; 16];
            self.rng.fill_bytes(&mut bytes);
            let id = TraceId::from_bytes(bytes);
            if !id.is_zero() {
                return id;
            }
        }
    }

    fn new_span_id(&self) -> SpanId {
        loop {
            let mut bytes = [0u8; 8];
            self.rng.fill_bytes(&mut bytes);
            if let Some(id) = SpanId::from_bytes(bytes).non_zero() {
                return id;
            }
        }
    }

    /// The next batch to export, when one is due: no export is in flight, the backoff after a failure has
    /// passed, and either `flush` is set, a full batch waits, or the oldest operation has waited for the flush
    /// interval. The caller exports it and reports back through [`ProxyCore::export_done`].
    pub fn next_export(&mut self, now: Instant, flush: bool) -> Option<Batch> {
        if self.in_flight || self.buffer.is_empty() || now < self.next_attempt {
            return None;
        }
        let waited = self.waiting_since.is_some_and(|since| {
            now.saturating_duration_since(since) >= self.config.flush_interval
        });
        if !(flush || waited || self.buffer.len() >= self.config.batch_ops) {
            return None;
        }
        let count = self.buffer.len().min(self.config.batch_ops);
        let ops: Vec<Op> = self.buffer.drain(..count).collect();
        let bytes: usize = ops.iter().map(Op::estimated_bytes).sum();
        self.buffer_bytes = self.buffer_bytes.saturating_sub(bytes);
        if self.buffer.is_empty() {
            self.waiting_since = None;
        }
        self.in_flight = true;
        Some(Batch { ops })
    }

    /// When the next export may be due, for the caller's timer: `None` while nothing waits.
    pub fn next_export_at(&self) -> Option<Instant> {
        if self.in_flight {
            return None;
        }
        let since = self.waiting_since?;
        Some(
            since
                .saturating_add(self.config.flush_interval)
                .max(self.next_attempt),
        )
    }

    /// Reports the outcome of the batch that [`ProxyCore::next_export`] handed out. On `Delivered` it returns
    /// how many operations were dropped since the previous delivered export, for the caller to report.
    pub fn export_done(&mut self, batch: Batch, result: ExportResult, now: Instant) -> u64 {
        self.in_flight = false;
        match result {
            ExportResult::Delivered => {
                self.failures = 0;
                self.next_attempt = now;
                self.stats.exported_ops += batch.ops.len() as u64;
                std::mem::take(&mut self.unreported_drops)
            }
            ExportResult::Rejected => {
                self.stats.dropped_ops += batch.ops.len() as u64;
                self.unreported_drops += batch.ops.len() as u64;
                0
            }
            ExportResult::Retry => {
                self.stats.export_failures += 1;
                self.failures = self.failures.saturating_add(1);
                let backoff = Duration::from_secs(1)
                    .saturating_mul(1u32 << (self.failures - 1).min(16))
                    .min(self.config.max_backoff);
                self.next_attempt = now.saturating_add(backoff);
                // Back to the front: they are the oldest, so they are the first to go if the buffer is full.
                for op in batch.ops.into_iter().rev() {
                    self.buffer_bytes += op.estimated_bytes();
                    self.buffer.push_front(op);
                }
                self.waiting_since.get_or_insert(now);
                self.trim_buffer();
                0
            }
        }
    }
}

fn error_input(outcome: &Outcome, era: Option<Era>) -> McpErrorInput {
    McpErrorInput {
        era: era.unwrap_or(Era::Legacy),
        side: McpSide::Server,
        jsonrpc_code: outcome.jsonrpc_code,
        is_error: outcome.is_error,
        http_status: outcome.http_status,
        cancel_source: outcome.cancel_source.unwrap_or(CancelSource::None),
        timed_out: false,
        transport_failure: outcome.transport_failure,
        unhandled_exception: false,
        annotations: McpErrorAnnotations::NONE,
    }
}

/// The `error.type` of an operation (contract 9.2): the JSON-RPC code as text, `tool_error`, or what failed.
pub fn error_type(op: &Op) -> Option<String> {
    if let Some(code) = op.jsonrpc_code {
        return Some(code.to_string());
    }
    if op.is_error && op.classification.status == McpStatus::ToolError {
        return Some("tool_error".to_owned());
    }
    match op.classification.error_class {
        ErrorClass::None => None,
        ErrorClass::Auth | ErrorClass::RateLimit | ErrorClass::Internal => {
            Some(op.http_status.map_or_else(
                || op.classification.error_class.as_str().to_owned(),
                |status| status.to_string(),
            ))
        }
        class => Some(class.as_str().to_owned()),
    }
}
