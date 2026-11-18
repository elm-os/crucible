//! The ClickHouse schema as data (contract section 6), the DDL renderer for the three deployment variants, the
//! migration steps, the service-identity access step (section 7), and the partition routing and retention
//! boundary kernel (sections 5.2, 13.1 and 13.2).
//!
//! This crate is pure: no `Env`, no IO.

mod access;
mod columns;
mod ddl;
mod migrate;
mod partition;
mod table;
mod tables;
mod types;

pub use access::{
    AGENT_MAX_CONCURRENT_QUERIES, AGENT_QUOTA, AGENT_QUOTA_EXECUTION_SECONDS_PER_HOUR,
    AGENT_QUOTA_QUERIES_PER_HOUR, AccessParams, INGEST_MAX_EXECUTION_SECONDS, InvalidPasswordHash,
    MAINT_SYSTEM_TABLES, PasswordHash, ProfileCaps, ServiceIdentity, service_policy,
    service_policy_check, service_policy_name,
};
pub use columns::{
    DedupSelftestCol, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol,
    McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol, SessionRollupCol, SpanCoreCol,
    SpanMetricsCol, SpanPayloadCol, SqlProbeCol, TraceHeadsCol, TraceStatsCol, UserRollupCol,
};
pub use ddl::{
    DATABASE, DEDUP_WINDOW, DEDUP_WINDOW_SECONDS, Granularity, Ident, InvalidIdent, SQL_DATABASE,
    Variant, create_database, create_table, partition_by,
};
pub use migrate::{
    ACCESS_SERVICE, ACCESS_WORKSPACE, LEDGER_ORDER, MigrationStep, PolicyForm, SCHEMA_BASE,
    StepKind, WorkspaceAction, access_service, access_workspace_shape, schema_base,
};
pub use partition::{
    CutoffDay, DAY_COUNT, OutOfRange, PartitionFate, PartitionRule, RetentionSettings, clamp_from,
    day_of, partition_fate, retention_cutoff, route,
};
pub use table::{
    Column, ColumnDef, ColumnRef, IndexKind, PROJECT_ID, PartitionExpr, Retention,
    RetentionBinding, SkipIndex, TEXT_INDEX_GRANULARITY, TableDef, TableKind, Tokenizer,
};
pub use tables::{TABLES, table};
pub use types::{AggFn, Codec, EnumKind, Ty, WireTy};
