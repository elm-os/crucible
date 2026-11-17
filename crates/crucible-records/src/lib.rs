//! Normalized records and the shared primitives that every other crate names: identifiers, time, log
//! sequence numbers, table and partition ids, batch tokens, the frozen enums, `ProjectScope` and
//! deterministic collections (contracts sections 1 and 3).
//!
//! This crate is pure: no `Env`, no IO.

mod collections;
mod enums;
mod hex;
mod id;
mod lsn;
mod scope;
mod table;
mod time;
mod token;

pub use collections::{
    DetMap, DetSet, det_map, det_map_with_capacity, det_set, det_set_with_capacity,
};
pub use enums::{
    AuthorKind, CancelSource, CaptureSource, CostSource, DeletionKind, DeletionState, Direction,
    Era, ErrorClass, ErrorSubclass, FactTargetKind, GroupKeyKind, HeadKind, IdentitySource,
    IoShape, Level, McpSide, McpStatus, McpTargetKind, ObservationType, ResultType, ScoreDataType,
    ScoreSource, SourceFormat, SpanKind, StatusCode, TagOp, TracePublic, Transport, Truncation,
};
pub use id::{
    AlertRuleId, ApiKeyId, AuthSessionId, ChannelId, CommentId, DashboardId, DeletionRequestId,
    ExportJobId, FactId, IdParseError, MediaObjectId, OrgId, ProjectId, RequestId, SavedQueryId,
    SpanId, SqlRunId, TraceId, UserId, Uuid, WalEpoch, WidgetId,
};
pub use lsn::Lsn;
pub use scope::{NilProjectId, ProjectScope};
pub use table::{PartitionId, PartitionParseError, TableId};
pub use time::{CivilDate, InvalidWindow, TimeWindow, Timestamp};
pub use token::{BatchToken, TokenError};
