//! The versioned pure normalizer (contracts 3.2, 5.3 and 10.2): from an accepted OTLP request to [`SpanRecord`]s.
//!
//! **Stage A** ([`normalize`]) is a pure function of one WAL frame and a [`NormCtx`]: no clock, randomness or IO,
//! so frames may be normalized in parallel and a replay gives byte-identical records. It re-scans the canonical
//! OTLP body, skips the spans the envelope marks as rejected, maps each span through its dialect's
//! [`SpanMapper`] (dialect 0 only in milestone 1), forward-fills trace-level attributes within the request, applies
//! the time rules, defaults, cost, payload caps and the row digest.
//!
//! **Stage B** ([`StageB`]) is sequential in LSN order: the duplicate window drops exact repeats and the first-seen
//! set marks provisional trace heads.
//!
//! The shipper stamps `batch_seq` when it seals a batch ([`NormalizedFrame::stamp_batch_seq`]); until then it is
//! [`Lsn::ZERO`]. The ingestion group is outside the row digest, so stamping never changes a digest.
//!
//! Provided usage and cost are used as sent; without a provided cost, usage is priced from the price tables that
//! the [`NormCtx`] pins ([`PriceTable`]). The MCP projector is a stub until milestone 3, so `mcp_side` is always
//! `none`.
//!
//! This crate is pure: no `Env`, no IO.

mod cost;
mod digest;
mod engine;
mod fill;
mod json;
mod mapper;
mod price;
mod stage_a;
mod stage_b;

use crucible_otlp::ScanError;
use crucible_records::{ApiKeyId, Lsn, PayloadCaps, ProjectId, SpanRecord, Timestamp, WalEpoch};

pub use digest::{DIGEST_ENCODING, row_digest};
pub use mapper::{
    Hints, InventoryDraft, MapCtx, McpOpDraft, McpProjector, NATIVE, SpanDraft, SpanMapper,
    TableMapper,
};
pub use price::{
    BUNDLED_PRICE_TABLE_VERSION, Compare, Condition, ModelPrice, PriceSnapshot, PriceTable,
    PriceTableError, PriceTier,
};
pub use stage_a::{normalize, normalize_with};
pub use stage_b::{Admission, DUPLICATE_WINDOW_KEYS, FIRST_SEEN_TRACES, OutOfOrder, StageB};

/// The version of this normalizer. Every record carries it, and a `BatchSeal` pins it (contract 4.4). Any change
/// to the records stage A produces for some input ships with a new version.
///
/// - 1: the first version; cost was provided or `none`.
/// - 2: usage without a provided cost is priced from the price tables (`cost_source = calculated`, per-key costs,
///   `total_cost_nusd` and the price row and tier ids).
pub const NORMALIZER_VERSION: u32 = 2;

/// One `IngestRequest` WAL frame as the normalizer reads it: the frame header values and the envelope (contracts
/// 4.3 and 4.4), borrowing the payload. The shipper fills it from the decoded frame; this crate does not depend on
/// the WAL codec (contract 1.5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct IngestFrame<'a> {
    /// Epoch of the WAL that holds the frame.
    pub wal_epoch: WalEpoch,
    /// LSN of the frame.
    pub lsn: Lsn,
    /// The frame header's `received_at`.
    pub received_at: Timestamp,
    /// Project of the authenticated credential.
    pub project_id: ProjectId,
    /// The API key that authenticated the request.
    pub api_key_id: ApiKeyId,
    /// The request-level dialect hint; 0 when there is none.
    pub compat_dialect: u8,
    /// SDK name from the request headers.
    pub sdk_name: &'a str,
    /// SDK version from the request headers.
    pub sdk_version: &'a str,
    /// Bitmap over span ordinals in wire order; a set bit marks a span rejected before the ack.
    pub rejected: &'a [u8],
    /// The canonical OTLP protobuf `ExportTraceServiceRequest`.
    pub otlp: &'a [u8],
}

impl IngestFrame<'_> {
    /// Whether the span at `ordinal` (wire order) was rejected before the ack: bit `ordinal % 8`, least significant
    /// first, of byte `ordinal / 8` (contract 4.10). Ordinals beyond the bitmap are not rejected.
    pub fn is_rejected(&self, ordinal: u32) -> bool {
        self.rejected
            .get((ordinal / 8) as usize)
            .is_some_and(|byte| byte & (1 << (ordinal % 8)) != 0)
    }
}

/// What stage A's output is pinned to (contract 3.2). Equal frames normalized with equal contexts give
/// byte-identical records.
///
/// The project snapshot contributes the caps and, from milestone 4, the project's price overrides; the salt,
/// capture settings and tombstones join with the stages that use them.
#[derive(Clone, Copy, Debug)]
pub struct NormCtx<'a> {
    /// The normalizer version recorded in every record.
    pub normalizer_version: u32,
    /// The price table version, recorded in the seal (contract 5.3). Equal versions must mean equal tables in
    /// `prices`.
    pub price_table_version: u32,
    /// The price tables usage is priced with.
    pub prices: PriceSnapshot<'a>,
    /// The project's payload caps.
    pub caps: PayloadCaps,
}

impl<'a> NormCtx<'a> {
    /// A context for this normalizer ([`NORMALIZER_VERSION`]) that prices with `bundled` alone and records its
    /// version.
    pub const fn new(bundled: &'a PriceTable, caps: PayloadCaps) -> Self {
        Self {
            normalizer_version: NORMALIZER_VERSION,
            price_table_version: bundled.version(),
            prices: PriceSnapshot {
                project: None,
                bundled,
            },
            caps,
        }
    }
}

/// How many spans of a frame stage A skipped, and why.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct FrameSummary {
    /// Spans in the OTLP body, skipped ones included.
    pub spans: u32,
    /// Spans the envelope's `rejected` bitmap marks.
    pub rejected_before_ack: u32,
    /// Spans with a missing or all-zero trace or span id that the bitmap does not mark. Ingest marks every such
    /// span, so this is 0 for frames that ingest wrote.
    pub missing_ids: u32,
}

/// The records of one frame (contract 3.2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NormalizedFrame {
    /// LSN of the frame.
    pub lsn: Lsn,
    /// One record per accepted span, in wire order.
    pub spans: Vec<SpanRecord>,
    /// The skipped spans.
    pub summary: FrameSummary,
}

impl NormalizedFrame {
    /// Stamps the batch the frame was sealed into (contract 5.1). The shipper calls it at seal time.
    pub fn stamp_batch_seq(&mut self, batch_seq: Lsn) {
        for span in &mut self.spans {
            span.core.ingestion.batch_seq = batch_seq;
        }
    }
}

/// Why a frame cannot be normalized.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum NormalizeError {
    /// The frame's OTLP body does not scan, although ingest scanned it before the append.
    #[error("the frame's OTLP body does not scan: {0}")]
    Scan(#[from] ScanError),
}

#[cfg(test)]
mod test_support {
    use crucible_records::{
        ApiKeyId, CostSource, Ingestion, IoShape, Level, Lsn, McpSide, NanoUsd, ObservationType,
        ProjectId, SourceFormat, SpanCore, SpanId, SpanKind, SpanPayload, SpanRecord, StatusCode,
        Timestamp, TraceId, TracePublic, Truncation, WalEpoch,
    };

    /// A record with every field set to a plain value.
    pub(crate) fn record() -> SpanRecord {
        SpanRecord {
            core: SpanCore {
                project_id: ProjectId::from_bytes([1; 16]),
                trace_id: TraceId::from_bytes([2; 16]),
                span_id: SpanId::from_bytes([3; 8]),
                parent_span_id: SpanId::ZERO,
                is_root: true,
                is_app_root: false,
                kind: SpanKind::Internal,
                name: "span".into(),
                start_time: Timestamp::from_micros(1_794_907_800_000_000),
                end_time: Timestamp::from_micros(1_794_907_800_250_000),
                completion_start_time: None,
                status_code: StatusCode::Unset,
                status_message: "".into(),
                service_name: "svc".into(),
                service_version: "".into(),
                scope_name: "".into(),
                scope_version: "".into(),
                telemetry_sdk_name: "".into(),
                telemetry_sdk_language: "".into(),
                telemetry_sdk_version: "".into(),
                sdk_name: "".into(),
                sdk_version: "".into(),
                observation_type: ObservationType::Span,
                level: Level::Default,
                environment: "default".into(),
                release: "".into(),
                version: "".into(),
                trace_name: "".into(),
                user_id: "".into(),
                session_id: "".into(),
                trace_tags: Vec::new(),
                trace_public: TracePublic::Unset,
                tags: Vec::new(),
                model_requested: "".into(),
                model_response: "".into(),
                provider: "".into(),
                finish_reasons: Vec::new(),
                usage: Vec::new(),
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                total_cost_nusd: NanoUsd::ZERO,
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
                    wal_epoch: WalEpoch::from_bytes([4; 16]),
                    batch_seq: Lsn::new(10),
                    lsn: Lsn::new(12),
                    received_at: Timestamp::from_micros(1_794_907_801_000_000),
                },
                api_key_id: ApiKeyId::from_bytes([5; 16]),
                row_digest: 0,
                event_bytes: 0,
            },
            payload: SpanPayload::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rejected_bitmap_is_read_least_significant_bit_first() {
        let rejected = [0b0000_0101, 0b1000_0000];
        let frame = IngestFrame {
            wal_epoch: WalEpoch::from_bytes([1; 16]),
            lsn: Lsn::new(1),
            received_at: Timestamp::from_micros(0),
            project_id: ProjectId::from_bytes([2; 16]),
            api_key_id: ApiKeyId::from_bytes([3; 16]),
            compat_dialect: 0,
            sdk_name: "",
            sdk_version: "",
            rejected: &rejected,
            otlp: &[],
        };
        let set: Vec<u32> = (0..64).filter(|&o| frame.is_rejected(o)).collect();
        assert_eq!(set, [0, 2, 15]);
        assert!(!frame.is_rejected(u32::MAX));
    }
}
