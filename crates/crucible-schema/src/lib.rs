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
mod workspace;

pub use access::{
    AGENT_MAX_CONCURRENT_QUERIES, AGENT_QUOTA, AGENT_QUOTA_EXECUTION_SECONDS_PER_HOUR,
    AGENT_QUOTA_QUERIES_PER_HOUR, AccessParams, INGEST_MAX_EXECUTION_SECONDS, InvalidPasswordHash,
    MAINT_SYSTEM_TABLES, PasswordHash, ProfileCaps, ServiceIdentity, service_policy,
    service_policy_check, service_policy_name, without_secrets,
};
pub use columns::{
    DedupSelftestCol, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol,
    McpDeploysCol, McpOpsCol, McpToolInventoryCol, McpToolRollupCol, McpToolRollupHourlyCol,
    SessionRollupCol, SpanCoreCol, SpanMetricsCol, SpanPayloadCol, SqlProbeCol, TraceHeadsCol,
    TraceStatsCol, UserRollupCol,
};
pub use ddl::{
    DATABASE, DEDUP_WINDOW, DEDUP_WINDOW_SECONDS, Granularity, Ident, InvalidIdent, SQL_DATABASE,
    Variant, create_database, create_table, partition_by,
};
pub use migrate::{
    ACCESS_SERVICE, ACCESS_WORKSPACE, HOURLY_COPY_DONE, LEDGER_ORDER, MigrationStep, PolicyForm,
    SCHEMA_BASE, SCHEMA_MCP_TOOL_ROLLUP_HOURLY, SCHEMA_SPAN_USAGE_AGGREGATE, StepKind,
    WorkspaceAction, access_service, access_workspace_shape, schema_base,
    schema_mcp_tool_rollup_hourly, schema_span_usage_aggregate,
};
pub use partition::{
    CutoffDay, DAY_COUNT, OutOfRange, PartitionFate, PartitionRule, RetentionSettings, clamp_from,
    day_of, partition_fate, retention_cutoff, route,
};
pub use table::{
    Column, ColumnDef, ColumnRef, IndexKind, PROJECT_ID, PartitionExpr, Retention,
    RetentionBinding, SkipIndex, TEXT_INDEX_GRANULARITY, TableDef, TableKind, Tokenizer,
};
pub use tables::{TABLES, hourly_tier, table};
pub use types::{AggFn, Codec, EnumKind, Ty, WireTy};
pub use workspace::{
    CURATED_VIEWS, CuratedView, JOIN_SPILL_SETTINGS, PROBE_PROJECTS, SQL_PROFILE, SQL_QUOTA,
    SQL_QUOTA_ERRORS_PER_HOUR, SQL_QUOTA_EXECUTION_SECONDS_PER_HOUR, SQL_QUOTA_QUERIES_PER_HOUR,
    SQL_QUOTA_READ_ROWS_PER_HOUR, SQL_READER_ROLE, SQL_USER_PREFIX, STATIC_PREDICATE, SqlProfile,
    ViewColumn, WorkspaceParams, access_workspace, alter_quota, deprovision_identity,
    expected_policies, policy_inventory, probe_canary, project_hex, project_policies,
    project_policy_name, project_predicate, provision_identity, sql_tables, static_policy,
    static_policy_name, workspace_user,
};
