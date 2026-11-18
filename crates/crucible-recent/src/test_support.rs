//! A manual clock and an in-memory WAL view, for tests of the index and of the query layer over it. Enabled by
//! the `test-support` feature, which product builds never enable.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crucible_env::{Clock, Instant};
use crucible_records::{
    ApiKeyId, CostSource, Ingestion, IoShape, Lsn, McpSide, ProjectId, SourceFormat, SpanCore,
    SpanKind, SpanPayload, SpanRecord, Timestamp, TracePublic, Truncation, WalEpoch,
};

use crate::entry::{SpanEntry, WalLocator};
use crate::wal::{SpanRead, WalReadError, WalView};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug)]
struct ClockState {
    now: u64,
    wall: Timestamp,
    wakers: Vec<Waker>,
}

/// A clock that moves only when the test says so.
#[derive(Clone, Debug)]
pub struct ManualClock {
    state: Arc<Mutex<ClockState>>,
}

impl ManualClock {
    /// A clock at monotonic zero whose wall time is `wall`.
    pub fn new(wall: Timestamp) -> Self {
        let state = ClockState {
            now: 0,
            wall,
            wakers: Vec::new(),
        };
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Advances the monotonic and the wall clock and wakes every sleeper.
    pub fn advance(&self, d: Duration) {
        let wakers = {
            let mut state = lock(&self.state);
            let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            state.now = state.now.saturating_add(nanos);
            let micros = i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
            state.wall = state.wall.saturating_add_micros(micros);
            std::mem::take(&mut state.wakers)
        };
        for waker in wakers {
            waker.wake();
        }
    }

    /// Sets the wall clock.
    pub fn set_wall(&self, wall: Timestamp) {
        lock(&self.state).wall = wall;
    }
}

struct Sleep {
    state: Arc<Mutex<ClockState>>,
    deadline: u64,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = lock(&self.state);
        if state.now >= self.deadline {
            Poll::Ready(())
        } else {
            state.wakers.push(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Instant {
        Instant::from_nanos(lock(&self.state).now)
    }

    fn wall_now(&self) -> Timestamp {
        lock(&self.state).wall
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(Sleep {
            state: Arc::clone(&self.state),
            deadline: deadline.nanos(),
        })
    }
}

#[derive(Debug, Default)]
struct WalState {
    durable: Lsn,
    spans: BTreeMap<(u64, u64, u32), SpanRecord>,
}

/// A WAL view over a map: the test sets the durable LSN and stores the records that locators resolve to.
#[derive(Clone, Debug, Default)]
pub struct FakeWal {
    state: Arc<Mutex<WalState>>,
}

impl FakeWal {
    /// An empty WAL whose durable LSN is zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the durable LSN: every frame at or below it is acknowledged.
    pub fn set_durable(&self, lsn: Lsn) {
        lock(&self.state).durable = lsn;
    }

    /// Stores the record a locator resolves to.
    pub fn put(&self, locator: WalLocator, record: SpanRecord) {
        let key = (locator.segment, locator.offset, locator.ordinal);
        lock(&self.state).spans.insert(key, record);
    }
}

impl WalView for FakeWal {
    fn durable_lsn(&self) -> Lsn {
        lock(&self.state).durable
    }

    fn read_span(&self, project_id: ProjectId, locator: WalLocator) -> SpanRead {
        let key = (locator.segment, locator.offset, locator.ordinal);
        let found = lock(&self.state).spans.get(&key).cloned();
        Box::pin(async move {
            found
                .filter(|record| record.core.project_id == project_id)
                .ok_or(WalReadError::Gone)
        })
    }
}

/// The normalized record of an entry: the span the entry summarizes, with every other field empty. The frame is
/// `lsn`, in a batch of its own.
pub fn span_record(entry: &SpanEntry, lsn: Lsn, received_at: Timestamp) -> SpanRecord {
    let s = &entry.summary;
    let core = SpanCore {
        project_id: entry.project_id,
        trace_id: s.trace_id,
        span_id: s.span_id,
        parent_span_id: s.parent_span_id,
        is_root: s.is_root,
        is_app_root: false,
        kind: SpanKind::Internal,
        name: (*s.name).into(),
        start_time: s.start_time,
        end_time: s.end_time,
        completion_start_time: None,
        status_code: s.status_code,
        status_message: "".into(),
        service_name: "".into(),
        service_version: "".into(),
        scope_name: "".into(),
        scope_version: "".into(),
        telemetry_sdk_name: "".into(),
        telemetry_sdk_language: "".into(),
        telemetry_sdk_version: "".into(),
        sdk_name: "".into(),
        sdk_version: "".into(),
        observation_type: s.observation_type,
        level: s.level,
        environment: (&*s.environment).into(),
        release: "".into(),
        version: "".into(),
        trace_name: (*s.trace_name).into(),
        user_id: (*s.user_id).into(),
        session_id: (*s.session_id).into(),
        trace_tags: Vec::new(),
        trace_public: TracePublic::Unset,
        tags: Vec::new(),
        model_requested: "".into(),
        model_response: (*s.model).into(),
        provider: "".into(),
        finish_reasons: Vec::new(),
        usage: Vec::new(),
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        total_tokens: s.total_tokens,
        total_cost_nusd: s.total_cost_nusd,
        cost_source: CostSource::None,
        price_model_id: None,
        price_tier_id: None,
        prompt_name: "".into(),
        prompt_version: "".into(),
        tool_names: Vec::new(),
        tool_call_count: 0,
        mcp_side: McpSide::None,
        io_shape: IoShape::Raw,
        input_preview: "".into(),
        output_preview: "".into(),
        input_bytes: 0,
        output_bytes: 0,
        input_truncation: Truncation::None,
        output_truncation: Truncation::None,
        input_sha256: None,
        output_sha256: None,
        source_format: SourceFormat::Otel,
        compat_dialect: 0,
        normalizer_version: 1,
        clock_skew: false,
        ingestion: Ingestion {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            batch_seq: lsn,
            lsn,
            received_at,
        },
        api_key_id: ApiKeyId::from_bytes([1; 16]),
        row_digest: 0,
        event_bytes: 0,
    };
    SpanRecord {
        core,
        payload: SpanPayload::default(),
    }
}
