//! Normalized records and the shared primitives that every other crate names: identifiers, time, log
//! sequence numbers, table and partition ids, batch tokens, the frozen enums, `ProjectScope` and
//! deterministic collections (contracts sections 1 and 3).
//!
//! The records are [`SpanRecord`], [`McpOpRecord`], [`McpToolInventoryRecord`], [`McpDeployRecord`], [`Fact`],
//! [`TraceHead`], [`TraceStatsRow`] and the rollup rows. The kernels are the latency histogram ([`Bucket`],
//! [`Histogram`]), the MCP error classifier ([`classify_mcp_error`]), the payload truncation rule
//! ([`SpanRecord::enforce_payload_caps`]) and rank-prefixed picks ([`Pick`]). The caps the contract names are in
//! [`caps`]. The `span` module documentation explains how the records are represented for throughput.
//!
//! This crate is pure: no `Env`, no IO.

pub mod caps;
mod classify;
mod collections;
mod cost;
mod enums;
mod fact;
mod hex;
mod histogram;
mod id;
mod ingestion;
mod lsn;
mod mcp;
mod merge;
mod rollup;
mod scope;
mod span;
mod table;
mod time;
mod token;
mod trace;
mod truncate;

pub use classify::{
    McpClassification, McpErrorAnnotations, McpErrorInput, class_fits_status, classify_mcp_error,
};
pub use collections::{
    DetMap, DetSet, det_map, det_map_with_capacity, det_set, det_set_with_capacity,
};
pub use cost::NanoUsd;
pub use enums::{
    AuthorKind, CancelSource, CaptureSource, CostSource, DeletionKind, DeletionState, Direction,
    Era, ErrorClass, ErrorSubclass, FactTargetKind, GroupKeyKind, HeadKind, IdentitySource,
    IoShape, Level, McpSide, McpStatus, McpTargetKind, ObservationType, ResultType, ScoreDataType,
    ScoreSource, SourceFormat, SpanKind, StatusCode, TagOp, TracePublic, Transport, Truncation,
};
pub use fact::{
    CorrectionBody, Fact, FactBody, FactEnvelope, FactTarget, FactVersion, FlagBody, ScoreBody,
    TagBody, effective_tags, fact_order, fold_facts,
};
pub use histogram::{
    Bucket, BucketCounts, Histogram, MCP_SEMCONV_DURATION_BOUNDS, SemconvHistogram, semconv_bucket,
};
pub use id::{
    AlertRuleId, ApiKeyId, AuthSessionId, ChannelId, CommentId, DashboardId, DeletionRequestId,
    ExportJobId, FactId, IdParseError, MediaObjectId, OrgId, ProjectId, RequestId, SavedQueryId,
    SpanId, SqlRunId, TraceId, UserId, Uuid, WalEpoch, WidgetId,
};
pub use ingestion::Ingestion;
pub use lsn::Lsn;
pub use mcp::{McpDeployRecord, McpOpRecord, McpToolInventoryRecord};
pub use rollup::{
    McpToolInventoryRow, McpToolRollupRow, SessionRollupRow, SpanMetricsRow, UserRollupRow,
};
pub use scope::{NilProjectId, ProjectScope};
pub use span::{
    PayloadCaps, PayloadField, PayloadFields, SpanCore, SpanPayload, SpanRecord, UsageEntry,
};
pub use table::{PartitionId, PartitionParseError, TableId};
pub use time::{CivilDate, InvalidWindow, TimeWindow, Timestamp, TimestampParseError};
pub use token::{BatchToken, MAX_TOKEN_LEN, TokenError, TokenText};
pub use trace::{NO_HEAD_START, Pick, TraceHead, TraceStatsRow};
pub use truncate::{truncate_str, utf8_floor, utf8_prefix};
