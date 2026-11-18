//! The column lists of contract section 6.4, and one column enum per table.
//!
//! Each `columns!` block is the single source for a table's `ColumnDef` list and for its column enum, so the two
//! cannot drift. The enums leave out `project_id`: plans cannot name it (contract section 8.1).

use crucible_records::TableId;

use crate::table::{Column, ColumnDef, ColumnRef};
use crate::types::{AggFn, Codec, EnumKind, Ty};

const DT64: Ty = Ty::DateTime64;
const LC: Ty = Ty::LowCardinalityString;
const STR: Ty = Ty::String;
const U8: Ty = Ty::UInt8;
const U32: Ty = Ty::UInt32;
const U64: Ty = Ty::UInt64;
const FS8: Ty = Ty::FixedString(8);
const FS16: Ty = Ty::FixedString(16);
const ARR_STR: Ty = Ty::Array(&Ty::String);
const ARR_LC: Ty = Ty::Array(&Ty::LowCardinalityString);
const ARR_U64: Ty = Ty::Array(&Ty::UInt64);
const ARR_I64: Ty = Ty::Array(&Ty::Int64);
const SUM_U64: Ty = Ty::Saf(AggFn::Sum, &Ty::UInt64);
const SUM_I64: Ty = Ty::Saf(AggFn::Sum, &Ty::Int64);
const MIN_DT64: Ty = Ty::Saf(AggFn::Min, &Ty::DateTime64);
const MAX_DT64: Ty = Ty::Saf(AggFn::Max, &Ty::DateTime64);
const MAX_STR: Ty = Ty::Saf(AggFn::Max, &Ty::String);
pub(crate) const MARKER_AT: Ty = Ty::Saf(AggFn::Max, &Ty::DateTime);
/// The latency histogram of contract section 6.5: bucket indexes and counts.
pub(crate) const HIST: Ty = Ty::Saf(
    AggFn::SumMap,
    &Ty::Tuple2(&Ty::Array(&Ty::UInt16), &Ty::Array(&Ty::UInt64)),
);
/// Exact counts per fixed bucket of the MCP operation-duration metric.
const SEMCONV_HIST: Ty = Ty::Saf(
    AggFn::SumMap,
    &Ty::Tuple2(&Ty::Array(&Ty::UInt8), &Ty::Array(&Ty::UInt64)),
);

const fn e(kind: EnumKind) -> Ty {
    Ty::Enum8(kind)
}

const PROJECT_ID_COL: ColumnDef = ColumnDef::new(crate::table::PROJECT_ID, Ty::Uuid, Codec::Zstd1);

macro_rules! col {
    ($name:literal, $ty:expr) => {
        ColumnDef::new($name, $ty, Codec::conventional(&$ty))
    };
    ($name:literal, $ty:expr, $codec:expr) => {
        ColumnDef::new($name, $ty, $codec)
    };
}

macro_rules! columns {
    ($table:ident, $enum:ident, $cols:ident, scoped; $($rest:tt)+) => {
        columns! { @emit $table, $enum, $cols, 1, [PROJECT_ID_COL,]; $($rest)+ }
    };
    ($table:ident, $enum:ident, $cols:ident, unscoped; $($rest:tt)+) => {
        columns! { @emit $table, $enum, $cols, 0, []; $($rest)+ }
    };
    (@emit $table:ident, $enum:ident, $cols:ident, $offset:literal, [$($prefix:tt)*];
     $($variant:ident $name:literal: $ty:expr $(=> $codec:expr)?;)+) => {
        #[doc = concat!("The columns of `", stringify!($cols), "` that a plan may name.")]
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
        #[repr(u16)]
        pub enum $enum {
            $(
                #[doc = concat!("`", $name, "`.")]
                $variant,
            )+
        }

        pub(crate) const $cols: &[ColumnDef] = &[$($prefix)* $(col!($name, $ty $(, $codec)?),)+];

        impl Column for $enum {
            const TABLE: TableId = TableId::$table;
            const ALL: &'static [Self] = &[$(Self::$variant,)+];

            fn column_ref(self) -> ColumnRef {
                ColumnRef::new(TableId::$table, self as u16 + $offset)
            }
        }
    };
}

columns! { SpanCore, SpanCoreCol, SPAN_CORE, scoped;
    StartMinute "start_minute": Ty::DateTime;
    TraceHash "trace_hash": U32 => Codec::Zstd1;
    StartTime "start_time": DT64;
    EndTime "end_time": DT64;
    SpanId "span_id": FS8;
    ParentSpanId "parent_span_id": FS8;
    TraceId "trace_id": FS16;
    IsRoot "is_root": U8;
    IsAppRoot "is_app_root": U8;
    ClockSkew "clock_skew": U8;
    Kind "kind": e(EnumKind::SpanKind);
    StatusCode "status_code": e(EnumKind::StatusCode);
    ObservationType "observation_type": e(EnumKind::ObservationType);
    Level "level": e(EnumKind::Level);
    TracePublic "trace_public": e(EnumKind::TracePublic);
    CostSource "cost_source": e(EnumKind::CostSource);
    McpSide "mcp_side": e(EnumKind::McpSide);
    IoShape "io_shape": e(EnumKind::IoShape);
    InputTruncation "input_truncation": e(EnumKind::Truncation);
    OutputTruncation "output_truncation": e(EnumKind::Truncation);
    SourceFormat "source_format": e(EnumKind::SourceFormat);
    Name "name": STR;
    StatusMessage "status_message": STR;
    ServiceVersion "service_version": STR;
    ScopeVersion "scope_version": STR;
    TelemetrySdkVersion "telemetry_sdk_version": STR;
    SdkVersion "sdk_version": STR;
    Release "release": STR;
    Version "version": STR;
    TraceName "trace_name": STR;
    UserId "user_id": STR;
    SessionId "session_id": STR;
    PromptName "prompt_name": STR;
    PromptVersion "prompt_version": STR;
    ServiceName "service_name": LC;
    ScopeName "scope_name": LC;
    TelemetrySdkName "telemetry_sdk_name": LC;
    TelemetrySdkLanguage "telemetry_sdk_language": LC;
    SdkName "sdk_name": LC;
    Environment "environment": LC;
    ModelRequested "model_requested": LC;
    ModelResponse "model_response": LC;
    Provider "provider": LC;
    DurationUs "duration_us": U64;
    InputTokens "input_tokens": U64;
    OutputTokens "output_tokens": U64;
    TotalTokens "total_tokens": U64;
    InputBytes "input_bytes": U64;
    OutputBytes "output_bytes": U64;
    CompletionStartTime "completion_start_time": Ty::Nullable(&Ty::DateTime64);
    TraceTags "trace_tags": ARR_STR;
    Tags "tags": ARR_STR;
    ToolNames "tool_names": ARR_STR;
    FinishReasons "finish_reasons": ARR_LC;
    UsageKeys "usage_keys": ARR_LC;
    UsageValues "usage_values": ARR_U64;
    CostValues "cost_values": ARR_I64;
    TotalCostNusd "total_cost_nusd": Ty::Int64;
    PriceModelId "price_model_id": Ty::Uuid;
    PriceTierId "price_tier_id": Ty::Uuid;
    ApiKeyId "api_key_id": Ty::Uuid;
    ToolCallCount "tool_call_count": U32;
    NormalizerVersion "normalizer_version": U32;
    EventBytes "event_bytes": U32;
    CompatDialect "compat_dialect": U8;
    InputPreview "input_preview": STR;
    OutputPreview "output_preview": STR;
    InputSha256 "input_sha256": Ty::FixedString(32);
    OutputSha256 "output_sha256": Ty::FixedString(32);
    RowDigest "row_digest": U64 => Codec::Zstd1;
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    Lsn "lsn": U64;
    ReceivedAt "received_at": DT64;
    UsageAggregate "usage_aggregate": U8;
}

columns! { SpanPayload, SpanPayloadCol, SPAN_PAYLOAD, scoped;
    StartMinute "start_minute": Ty::DateTime;
    TraceHash "trace_hash": U32 => Codec::Zstd1;
    SpanId "span_id": FS8;
    TraceId "trace_id": FS16;
    StartTime "start_time": DT64;
    UserId "user_id": STR;
    SessionId "session_id": STR;
    Input "input": STR => Codec::Zstd3;
    Output "output": STR => Codec::Zstd3;
    SystemInstructions "system_instructions": STR => Codec::Zstd3;
    Attributes "attributes": STR => Codec::Zstd3;
    ResourceAttributes "resource_attributes": STR => Codec::Zstd3;
    Events "events": STR => Codec::Zstd3;
    Links "links": STR => Codec::Zstd3;
    ToolDefinitions "tool_definitions": STR => Codec::Zstd3;
    ToolCalls "tool_calls": STR => Codec::Zstd3;
    ModelParameters "model_parameters": STR => Codec::Zstd3;
    TraceMetadata "trace_metadata": STR => Codec::Zstd3;
    MetadataKeys "metadata_keys": ARR_STR;
    MetadataValues "metadata_values": ARR_STR;
    MetadataKv "metadata_kv": ARR_STR;
    UsageProvidedKeys "usage_provided_keys": ARR_LC;
    CostProvidedKeys "cost_provided_keys": ARR_LC;
    UsageProvidedValues "usage_provided_values": ARR_U64;
    CostProvidedValues "cost_provided_values": ARR_I64;
    TraceState "trace_state": STR;
    TruncatedFields "truncated_fields": ARR_LC;
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    Lsn "lsn": U64;
    ReceivedAt "received_at": DT64;
}

columns! { McpOps, McpOpsCol, MCP_OPS, scoped;
    Side "side": e(EnumKind::McpSide);
    CaptureSource "capture_source": e(EnumKind::CaptureSource);
    Era "era": e(EnumKind::Era);
    Transport "transport": e(EnumKind::Transport);
    TargetKind "target_kind": e(EnumKind::McpTargetKind);
    Direction "direction": e(EnumKind::Direction);
    ResultType "result_type": e(EnumKind::ResultType);
    CancelSource "cancel_source": e(EnumKind::CancelSource);
    Status "status": e(EnumKind::McpStatus);
    ErrorClass "error_class": e(EnumKind::ErrorClass);
    ErrorSubclass "error_subclass": e(EnumKind::ErrorSubclass);
    IdentitySource "identity_source": e(EnumKind::IdentitySource);
    GroupKeyKind "group_key_kind": e(EnumKind::GroupKeyKind);
    ContentTruncation "content_truncation": e(EnumKind::Truncation);
    StartMinute "start_minute": Ty::DateTime;
    TraceHash "trace_hash": U32 => Codec::Zstd1;
    StartTime "start_time": DT64;
    SpanId "span_id": FS8;
    ParentSpanId "parent_span_id": FS8;
    TransportLinkSpanId "transport_link_span_id": FS8;
    TraceId "trace_id": FS16;
    TransportLinkTraceId "transport_link_trace_id": FS16;
    UserSubjectHash "user_subject_hash": FS16;
    MrtrStateIn "mrtr_state_in": FS16;
    MrtrStateOut "mrtr_state_out": FS16;
    ServerName "server_name": LC;
    Method "method": LC;
    TargetName "target_name": LC;
    ClientName "client_name": LC;
    ServerVersion "server_version": STR;
    DeployId "deploy_id": STR;
    InstanceId "instance_id": STR;
    ProtocolVersion "protocol_version": STR;
    JsonrpcId "jsonrpc_id": STR;
    TaskId "task_id": STR;
    SubscriptionId "subscription_id": STR;
    ErrorType "error_type": STR;
    ErrorMessage "error_message": STR;
    ClientVersion "client_version": STR;
    OauthClientId "oauth_client_id": STR;
    UserId "user_id": STR;
    UserIssuer "user_issuer": STR;
    Tenant "tenant": STR;
    SessionKey "session_key": STR;
    HandleId "handle_id": STR;
    GroupKey "group_key": STR;
    IpPrefix "ip_prefix": STR;
    UserAgent "user_agent": STR;
    IsNotification "is_notification": U8;
    IsBatch "is_batch": U8;
    TimedOut "timed_out": U8;
    CallerFault "caller_fault": U8;
    ContentCaptured "content_captured": U8;
    MrtrRound "mrtr_round": Ty::UInt16;
    DurationUs "duration_us": U64;
    RequestBytes "request_bytes": U64;
    ResponseBytes "response_bytes": U64;
    ClientCapabilitiesHash "client_capabilities_hash": U64 => Codec::Zstd1;
    ProgressCount "progress_count": Ty::Nullable(&Ty::UInt32);
    JsonrpcCode "jsonrpc_code": Ty::Nullable(&Ty::Int32);
    HttpStatus "http_status": Ty::Nullable(&Ty::UInt16);
    ContentBlockTypes "content_block_types": ARR_LC;
    ContentBlockCounts "content_block_counts": Ty::Array(&Ty::UInt32);
    ContentSampleRate "content_sample_rate": Ty::Float32;
    NormalizerVersion "normalizer_version": U32;
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    Lsn "lsn": U64;
    ReceivedAt "received_at": DT64;
}

columns! { McpContent, McpContentCol, MCP_CONTENT, scoped;
    Side "side": e(EnumKind::McpSide);
    StartMinute "start_minute": Ty::DateTime;
    TraceHash "trace_hash": U32 => Codec::Zstd1;
    SpanId "span_id": FS8;
    TraceId "trace_id": FS16;
    StartTime "start_time": DT64;
    UserId "user_id": STR;
    UserSubjectHash "user_subject_hash": FS16;
    TargetName "target_name": LC;
    Args "args": STR => Codec::Zstd3;
    Result "result": STR => Codec::Zstd3;
    ArgsBytes "args_bytes": U64;
    ResultBytes "result_bytes": U64;
    ContentTruncation "content_truncation": e(EnumKind::Truncation);
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    Lsn "lsn": U64;
    ReceivedAt "received_at": DT64;
}

columns! { McpToolInventory, McpToolInventoryCol, MCP_TOOL_INVENTORY, scoped;
    ServerName "server_name": LC;
    DeployId "deploy_id": STR;
    ToolName "tool_name": LC;
    SchemaHash "schema_hash": U64 => Codec::Zstd1;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    FirstSeen "first_seen": MIN_DT64;
    LastSeen "last_seen": MAX_DT64;
    CaptureSources "capture_sources": Ty::Saf(AggFn::GroupUniqArrayArray(4), &Ty::Array(&Ty::UInt8));
}

/// The columns shared by the four fact tables, the table's body columns, then the ingestion columns.
macro_rules! fact_columns {
    ($table:ident, $enum:ident, $cols:ident; $($body:tt)+) => {
        columns! { $table, $enum, $cols, scoped;
            Key "key": STR;
            TargetDay "target_day": Ty::Date;
            TraceId "trace_id": FS16;
            SpanId "span_id": FS8;
            SessionId "session_id": STR;
            Version "version": U64;
            FactId "fact_id": Ty::Uuid;
            TargetKind "target_kind": e(EnumKind::FactTargetKind);
            TargetTime "target_time": DT64;
            Retracted "retracted": U8;
            AuthorKind "author_kind": e(EnumKind::AuthorKind);
            AuthorId "author_id": Ty::Uuid;
            CreatedAt "created_at": DT64;
            $($body)+
            WalEpoch "wal_epoch": Ty::Uuid;
            BatchSeq "batch_seq": U64;
            Lsn "lsn": U64;
            ReceivedAt "received_at": DT64;
        }
    };
}

fact_columns! { FactScores, FactScoresCol, FACT_SCORES;
    DataType "data_type": e(EnumKind::ScoreDataType);
    ValueNum "value_num": Ty::Float64;
    ValueStr "value_str": STR;
    Comment "comment": STR;
    Source "source": e(EnumKind::ScoreSource);
    Metadata "metadata": STR;
    ConfigId "config_id": Ty::Uuid;
}

fact_columns! { FactTags, FactTagsCol, FACT_TAGS;
    Op "op": e(EnumKind::TagOp);
}

fact_columns! { FactFlags, FactFlagsCol, FACT_FLAGS;
    Value "value": U8;
}

fact_columns! { FactCorrections, FactCorrectionsCol, FACT_CORRECTIONS;
    Corrected "corrected": STR => Codec::Zstd3;
    Note "note": STR;
}

columns! { TraceHeads, TraceHeadsCol, TRACE_HEADS, scoped;
    StartTime "start_time": DT64;
    TraceHash "trace_hash": U32 => Codec::Zstd1;
    TraceId "trace_id": FS16;
    SpanId "span_id": FS8;
    HeadKind "head_kind": e(EnumKind::HeadKind);
    EndTime "end_time": DT64;
    Name "name": STR;
    Level "level": e(EnumKind::Level);
    StatusCode "status_code": e(EnumKind::StatusCode);
    UserId "user_id": STR;
    SessionId "session_id": STR;
    Tags "tags": ARR_STR;
    Environment "environment": LC;
    Release "release": STR;
    Version "version": STR;
    InputPreview "input_preview": STR;
    OutputPreview "output_preview": STR;
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    Lsn "lsn": U64;
    ReceivedAt "received_at": DT64;
}

columns! { TraceStats, TraceStatsCol, TRACE_STATS, scoped;
    TraceId "trace_id": FS16;
    RowDay "row_day": Ty::Date;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    MinStart "min_start": MIN_DT64;
    MaxEnd "max_end": MAX_DT64;
    HasRoot "has_root": Ty::Saf(AggFn::Max, &Ty::UInt8);
    MinHeadStart "min_head_start": MIN_DT64;
    MaxLevel "max_level": Ty::Saf(AggFn::Max, &Ty::UInt8);
    SpanCount "span_count": SUM_U64;
    ErrorCount "error_count": SUM_U64;
    GenerationCount "generation_count": SUM_U64;
    ToolCount "tool_count": SUM_U64;
    McpCount "mcp_count": SUM_U64;
    InputTokens "input_tokens": SUM_U64;
    OutputTokens "output_tokens": SUM_U64;
    TotalTokens "total_tokens": SUM_U64;
    TotalCostNusd "total_cost_nusd": SUM_I64;
    Environments "environments": Ty::Saf(AggFn::GroupUniqArrayArray(16), &Ty::Array(&Ty::String));
    TraceNamePick "trace_name_pick": MAX_STR;
    UserIdPick "user_id_pick": MAX_STR;
    SessionIdPick "session_id_pick": MAX_STR;
}

columns! { SessionRollup, SessionRollupCol, SESSION_ROLLUP, scoped;
    Day "day": Ty::Date;
    SessionId "session_id": STR;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    FirstSeen "first_seen": MIN_DT64;
    LastSeen "last_seen": MAX_DT64;
    TraceCount "trace_count": SUM_U64;
    SpanCount "span_count": SUM_U64;
    ErrorCount "error_count": SUM_U64;
    InputTokens "input_tokens": SUM_U64;
    OutputTokens "output_tokens": SUM_U64;
    TotalTokens "total_tokens": SUM_U64;
    TotalCostNusd "total_cost_nusd": SUM_I64;
    UserIds "user_ids": Ty::Saf(AggFn::GroupUniqArrayArray(8), &Ty::Array(&Ty::String));
}

columns! { UserRollup, UserRollupCol, USER_ROLLUP, scoped;
    Day "day": Ty::Date;
    UserId "user_id": STR;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    FirstSeen "first_seen": MIN_DT64;
    LastSeen "last_seen": MAX_DT64;
    TraceCount "trace_count": SUM_U64;
    SpanCount "span_count": SUM_U64;
    ErrorCount "error_count": SUM_U64;
    InputTokens "input_tokens": SUM_U64;
    OutputTokens "output_tokens": SUM_U64;
    TotalTokens "total_tokens": SUM_U64;
    TotalCostNusd "total_cost_nusd": SUM_I64;
}

columns! { SpanMetrics, SpanMetricsCol, SPAN_METRICS, scoped;
    Minute "minute": Ty::DateTime;
    Environment "environment": LC;
    ObservationType "observation_type": e(EnumKind::ObservationType);
    ModelResponse "model_response": LC;
    Level "level": e(EnumKind::Level);
    Release "release": STR;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    Count "count": SUM_U64;
    ErrorCount "error_count": SUM_U64;
    InputTokens "input_tokens": SUM_U64;
    OutputTokens "output_tokens": SUM_U64;
    TotalTokens "total_tokens": SUM_U64;
    TotalCostNusd "total_cost_nusd": SUM_I64;
    DurHist "dur_hist": HIST;
    TtftHist "ttft_hist": HIST;
}

columns! { McpToolRollup, McpToolRollupCol, MCP_TOOL_ROLLUP, scoped;
    Side "side": e(EnumKind::McpSide);
    Minute "minute": Ty::DateTime;
    ServerName "server_name": LC;
    DeployId "deploy_id": STR;
    CaptureSource "capture_source": e(EnumKind::CaptureSource);
    Era "era": e(EnumKind::Era);
    Method "method": LC;
    TargetName "target_name": LC;
    ClientName "client_name": LC;
    ErrorClass "error_class": e(EnumKind::ErrorClass);
    CallerFault "caller_fault": U8;
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    Count "count": SUM_U64;
    RequestBytes "request_bytes": SUM_U64;
    ResponseBytes "response_bytes": SUM_U64;
    DurHist "dur_hist": HIST;
    SemconvHist "semconv_hist": SEMCONV_HIST;
    LastSeen "last_seen": MAX_DT64;
}

columns! { McpDeploys, McpDeploysCol, MCP_DEPLOYS, scoped;
    ServerName "server_name": LC;
    DeployId "deploy_id": STR;
    ServerVersion "server_version": STR;
    CaptureSource "capture_source": e(EnumKind::CaptureSource);
    MarkerEpoch "marker_epoch": Ty::Uuid;
    MarkerBatchSeq "marker_batch_seq": U64;
    MarkerAt "marker_at": MARKER_AT;
    FirstSeen "first_seen": MIN_DT64;
    LastSeen "last_seen": MAX_DT64;
}

columns! { DedupSelftest, DedupSelftestCol, DEDUP_SELFTEST, unscoped;
    Id "id": Ty::Uuid;
    WalEpoch "wal_epoch": Ty::Uuid;
    BatchSeq "batch_seq": U64;
    CreatedAt "created_at": DT64;
}

columns! { SqlProbe, SqlProbeCol, SQL_PROBE, scoped;
    Id "id": U32;
    Canary "canary": STR;
    StartTime "start_time": DT64;
}
