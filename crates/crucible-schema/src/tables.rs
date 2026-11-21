//! The 19 tables of contract section 6.3, and the hourly tier of the minute MCP rollup.

use crucible_records::TableId;

use crate::columns::{
    DEDUP_SELFTEST, FACT_CORRECTIONS, FACT_FLAGS, FACT_SCORES, FACT_TAGS, MCP_CONTENT, MCP_DEPLOYS,
    MCP_OPS, MCP_TOOL_INVENTORY, MCP_TOOL_ROLLUP, MCP_TOOL_ROLLUP_HOURLY, SESSION_ROLLUP,
    SPAN_CORE, SPAN_METRICS, SPAN_PAYLOAD, SQL_PROBE, TRACE_HEADS, TRACE_STATS, USER_ROLLUP,
};
use crate::table::{
    ColumnDef, PartitionExpr, Retention, RetentionBinding, SkipIndex, TableDef, TableKind,
    Tokenizer,
};

const GRANULARITY_8192: &[(&str, u64)] = &[("index_granularity", 8192)];
/// Both values are to be benchmarked (contract section 6.3).
const PAYLOAD_SETTINGS: &[(&str, u64)] = &[
    ("index_granularity", 1024),
    ("index_granularity_bytes", 16_777_216),
];

const fn retention(binding: RetentionBinding, time_column: &'static str) -> Retention {
    Retention {
        binding,
        time_column,
    }
}

const FACT_ORDER: &[&str] = &[
    "project_id",
    "key",
    "target_day",
    "trace_id",
    "span_id",
    "session_id",
    "version",
    "fact_id",
];
const FACT_INDEXES: &[SkipIndex] = &[
    SkipIndex::bloom("trace_id", 1),
    SkipIndex::bloom("session_id", 1),
    SkipIndex::minmax("batch_seq"),
];

const fn fact_table(
    id: TableId,
    columns: &'static [ColumnDef],
    binding: RetentionBinding,
) -> TableDef {
    TableDef {
        id,
        kind: TableKind::Plain,
        columns,
        partition: PartitionExpr::Month("target_time"),
        order_by: FACT_ORDER,
        primary_key_len: 4,
        indexes: FACT_INDEXES,
        settings: GRANULARITY_8192,
        retention: retention(binding, "target_time"),
        sql_readable: true,
    }
}

/// Every table, in table-id order.
pub static TABLES: [TableDef; 19] = [
    TableDef {
        id: TableId::SpanCore,
        kind: TableKind::Plain,
        columns: SPAN_CORE,
        partition: PartitionExpr::Day("start_time"),
        order_by: &[
            "project_id",
            "start_minute",
            "trace_hash",
            "start_time",
            "span_id",
        ],
        primary_key_len: 3,
        indexes: &[
            SkipIndex::bloom("trace_id", 1),
            SkipIndex::bloom("user_id", 2),
            SkipIndex::bloom("session_id", 2),
            SkipIndex::set100("observation_type", 4),
            SkipIndex::set100("level", 4),
            SkipIndex::text("name", Tokenizer::Ngrams(3), true),
            SkipIndex::minmax("batch_seq"),
            SkipIndex::minmax("received_at"),
        ],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "start_time"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::SpanPayload,
        kind: TableKind::Plain,
        columns: SPAN_PAYLOAD,
        partition: PartitionExpr::Day("start_time"),
        order_by: &["project_id", "start_minute", "trace_hash", "span_id"],
        primary_key_len: 3,
        indexes: &[
            SkipIndex::text("input", Tokenizer::SplitByNonAlpha, true),
            SkipIndex::text("output", Tokenizer::SplitByNonAlpha, true),
            SkipIndex::text("metadata_kv", Tokenizer::Array, false),
            SkipIndex::bloom("trace_id", 1),
            SkipIndex::minmax("batch_seq"),
        ],
        settings: PAYLOAD_SETTINGS,
        retention: retention(RetentionBinding::PayloadRetentionDays, "start_time"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::McpOps,
        kind: TableKind::Plain,
        columns: MCP_OPS,
        partition: PartitionExpr::Day("start_time"),
        order_by: &[
            "project_id",
            "side",
            "start_minute",
            "trace_hash",
            "span_id",
        ],
        primary_key_len: 4,
        indexes: &[
            SkipIndex::bloom("trace_id", 1),
            SkipIndex::bloom("group_key", 1),
            SkipIndex::bloom("user_subject_hash", 1),
            SkipIndex::bloom("user_id", 1),
            SkipIndex::set100("error_class", 1),
            SkipIndex::minmax("batch_seq"),
        ],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "start_time"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::McpContent,
        kind: TableKind::Plain,
        columns: MCP_CONTENT,
        partition: PartitionExpr::Day("start_time"),
        order_by: &[
            "project_id",
            "side",
            "start_minute",
            "trace_hash",
            "span_id",
        ],
        primary_key_len: 4,
        indexes: &[
            SkipIndex::text("args", Tokenizer::SplitByNonAlpha, true),
            SkipIndex::text("result", Tokenizer::SplitByNonAlpha, true),
            SkipIndex::minmax("batch_seq"),
        ],
        settings: PAYLOAD_SETTINGS,
        retention: retention(RetentionBinding::McpContentRetentionDays, "start_time"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::McpToolInventory,
        kind: TableKind::Aggregating,
        columns: MCP_TOOL_INVENTORY,
        partition: PartitionExpr::None,
        order_by: &[
            "project_id",
            "server_name",
            "deploy_id",
            "tool_name",
            "schema_hash",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 5,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "last_seen"),
        sql_readable: true,
    },
    fact_table(
        TableId::FactScores,
        FACT_SCORES,
        RetentionBinding::RetentionDays,
    ),
    fact_table(
        TableId::FactTags,
        FACT_TAGS,
        RetentionBinding::RetentionDays,
    ),
    fact_table(
        TableId::FactFlags,
        FACT_FLAGS,
        RetentionBinding::RetentionDays,
    ),
    fact_table(
        TableId::FactCorrections,
        FACT_CORRECTIONS,
        RetentionBinding::PayloadRetentionDays,
    ),
    TableDef {
        id: TableId::TraceHeads,
        kind: TableKind::Plain,
        columns: TRACE_HEADS,
        partition: PartitionExpr::Day("start_time"),
        order_by: &["project_id", "start_time", "trace_hash", "trace_id"],
        primary_key_len: 3,
        indexes: &[
            SkipIndex::bloom("trace_id", 1),
            SkipIndex::bloom("user_id", 1),
            SkipIndex::bloom("session_id", 1),
            SkipIndex::text("name", Tokenizer::Ngrams(3), true),
            SkipIndex::text("tags", Tokenizer::Array, false),
            SkipIndex::set100("environment", 1),
            SkipIndex::minmax("batch_seq"),
            SkipIndex::minmax("received_at"),
        ],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "start_time"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::TraceStats,
        kind: TableKind::Aggregating,
        columns: TRACE_STATS,
        partition: PartitionExpr::DateColumn("row_day"),
        order_by: &[
            "project_id",
            "trace_id",
            "row_day",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 2,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "row_day"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::SessionRollup,
        kind: TableKind::Aggregating,
        columns: SESSION_ROLLUP,
        partition: PartitionExpr::Month("day"),
        order_by: &[
            "project_id",
            "day",
            "session_id",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 3,
        indexes: &[SkipIndex::bloom("session_id", 1)],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "day"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::UserRollup,
        kind: TableKind::Aggregating,
        columns: USER_ROLLUP,
        partition: PartitionExpr::Month("day"),
        order_by: &[
            "project_id",
            "day",
            "user_id",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 3,
        indexes: &[SkipIndex::bloom("user_id", 1)],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "day"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::SpanMetrics,
        kind: TableKind::Aggregating,
        columns: SPAN_METRICS,
        partition: PartitionExpr::Month("minute"),
        order_by: &[
            "project_id",
            "minute",
            "environment",
            "observation_type",
            "model_response",
            "level",
            "release",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 2,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "minute"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::McpToolRollup,
        kind: TableKind::Aggregating,
        columns: MCP_TOOL_ROLLUP,
        partition: PartitionExpr::Month("minute"),
        order_by: &[
            "project_id",
            "side",
            "minute",
            "server_name",
            "deploy_id",
            "capture_source",
            "era",
            "method",
            "target_name",
            "client_name",
            "error_class",
            "caller_fault",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 3,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "minute"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::McpDeploys,
        kind: TableKind::Aggregating,
        columns: MCP_DEPLOYS,
        partition: PartitionExpr::None,
        order_by: &[
            "project_id",
            "server_name",
            "deploy_id",
            "server_version",
            "capture_source",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 3,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "last_seen"),
        sql_readable: true,
    },
    TableDef {
        id: TableId::DedupSelftest,
        kind: TableKind::Plain,
        columns: DEDUP_SELFTEST,
        partition: PartitionExpr::None,
        order_by: &["id"],
        primary_key_len: 1,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::OneDay, "created_at"),
        sql_readable: false,
    },
    TableDef {
        id: TableId::SqlProbe,
        kind: TableKind::Plain,
        columns: SQL_PROBE,
        partition: PartitionExpr::None,
        order_by: &["project_id", "id"],
        primary_key_len: 1,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::None, "start_time"),
        sql_readable: true,
    },
    // The hourly tier of `mcp_tool_rollup` ([`hourly_tier`]): its columns in its order, with `hour` in place of
    // `minute`. Long-window MCP reads take their whole hours from it. The SQL workspace reads the minute rollup
    // only.
    TableDef {
        id: TableId::McpToolRollupHourly,
        kind: TableKind::Aggregating,
        columns: MCP_TOOL_ROLLUP_HOURLY,
        partition: PartitionExpr::Month("hour"),
        order_by: &[
            "project_id",
            "side",
            "hour",
            "server_name",
            "deploy_id",
            "capture_source",
            "era",
            "method",
            "target_name",
            "client_name",
            "error_class",
            "caller_fault",
            "marker_epoch",
            "marker_batch_seq",
        ],
        primary_key_len: 3,
        indexes: &[],
        settings: GRANULARITY_8192,
        retention: retention(RetentionBinding::RetentionDays, "hour"),
        sql_readable: false,
    },
];

/// The definition of a table.
pub fn table(id: TableId) -> &'static TableDef {
    &TABLES[usize::from(id.id()) - 1]
}

/// The hourly tier of a minute rollup: a table with the same columns in the same order, whose time column holds
/// the start of the hour instead of the minute. The shipper writes both from the same operations, so the rows of
/// a whole hour of the tier merge to what the minute rows of that hour merge to.
pub const fn hourly_tier(id: TableId) -> Option<TableId> {
    match id {
        TableId::McpToolRollup => Some(TableId::McpToolRollupHourly),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::columns::*;
    use crate::table::{Column, ColumnRef, PROJECT_ID};
    use crate::types::{Codec, EnumKind, Ty};

    fn refs<C: Column>() -> Vec<ColumnRef> {
        C::ALL.iter().map(|c| c.column_ref()).collect()
    }

    fn all_enum_refs() -> Vec<Vec<ColumnRef>> {
        vec![
            refs::<SpanCoreCol>(),
            refs::<SpanPayloadCol>(),
            refs::<McpOpsCol>(),
            refs::<McpContentCol>(),
            refs::<McpToolInventoryCol>(),
            refs::<FactScoresCol>(),
            refs::<FactTagsCol>(),
            refs::<FactFlagsCol>(),
            refs::<FactCorrectionsCol>(),
            refs::<TraceHeadsCol>(),
            refs::<TraceStatsCol>(),
            refs::<SessionRollupCol>(),
            refs::<UserRollupCol>(),
            refs::<SpanMetricsCol>(),
            refs::<McpToolRollupCol>(),
            refs::<McpDeploysCol>(),
            refs::<DedupSelftestCol>(),
            refs::<SqlProbeCol>(),
            refs::<McpToolRollupHourlyCol>(),
        ]
    }

    #[test]
    fn tables_follow_table_ids() {
        for (def, id) in TABLES.iter().zip(TableId::ALL) {
            assert_eq!(def.id, id);
            assert_eq!(table(id), def);
        }
    }

    #[test]
    fn enums_cover_every_column_except_project_id() {
        for (def, refs) in TABLES.iter().zip(all_enum_refs()) {
            let named: Vec<&str> = refs.iter().map(ColumnRef::name).collect();
            let expected: Vec<&str> = def
                .columns
                .iter()
                .map(|c| c.name)
                .filter(|n| *n != PROJECT_ID)
                .collect();
            assert_eq!(named, expected, "{}", def.name());
            assert!(refs.iter().all(|r| r.table() == def.id));
            assert!(!named.contains(&PROJECT_ID));
            assert!(def.key_columns().iter().all(|r| r.name() != PROJECT_ID));
        }
    }

    #[test]
    fn project_id_leads_every_table_but_the_selftest() {
        for def in &TABLES {
            let scoped = def.id != TableId::DedupSelftest;
            assert_eq!(def.is_scoped(), scoped, "{}", def.name());
            assert_eq!(def.order_by[0] == PROJECT_ID, scoped);
            let count = def.columns.iter().filter(|c| c.name == PROJECT_ID).count();
            assert_eq!(count, usize::from(scoped));
        }
    }

    #[test]
    fn keys_indexes_and_time_columns_exist() {
        for def in &TABLES {
            let mut names: Vec<&str> = def.columns.iter().map(|c| c.name).collect();
            names.sort_unstable();
            names.dedup();
            assert_eq!(names.len(), def.columns.len(), "{}", def.name());
            assert!(def.primary_key_len >= 1 && def.primary_key_len <= def.order_by.len());
            for key in def.order_by {
                let col = def
                    .column(key)
                    .unwrap_or_else(|| panic!("{key} in {}", def.name()));
                assert!(col.ty.agg_fn().is_none(), "key {key} is aggregated");
            }
            for index in def.indexes {
                assert!(def.column(index.column).is_some(), "{}", index.column);
            }
            assert!(def.column(def.retention.time_column).is_some());
            if let Some(column) = def.partition.column() {
                assert!(def.column(column).is_some());
            }
        }
    }

    #[test]
    fn plain_tables_carry_ingestion_columns_and_a_batch_seq_index() {
        for def in TABLES.iter().filter(|d| d.kind == TableKind::Plain) {
            if def.id == TableId::SqlProbe {
                continue; // An access object, never written by the shipper.
            }
            assert_eq!(def.column("wal_epoch").map(|c| c.ty), Some(Ty::Uuid));
            assert_eq!(def.column("batch_seq").map(|c| c.ty), Some(Ty::UInt64));
            if def.id == TableId::DedupSelftest {
                continue;
            }
            let last4: Vec<&str> = def.columns[def.columns.len() - 4..]
                .iter()
                .map(|c| c.name)
                .collect();
            assert_eq!(last4, ["wal_epoch", "batch_seq", "lsn", "received_at"]);
            assert_eq!(
                def.column("received_at").map(|c| c.ty),
                Some(Ty::DateTime64)
            );
            assert!(def.indexes.contains(&SkipIndex::minmax("batch_seq")));
        }
    }

    #[test]
    fn aggregating_tables_end_their_key_with_markers_and_aggregate_the_rest() {
        let aggregating: Vec<&TableDef> = TABLES
            .iter()
            .filter(|d| d.kind == TableKind::Aggregating)
            .collect();
        assert_eq!(aggregating.len(), 8);
        for def in aggregating {
            let tail = &def.order_by[def.order_by.len() - 2..];
            assert_eq!(tail, ["marker_epoch", "marker_batch_seq"], "{}", def.name());
            assert_eq!(def.column("marker_at").map(|c| c.ty), Some(MARKER_AT));
            for col in def.columns {
                let is_key = def.order_by.contains(&col.name);
                assert_eq!(
                    col.ty.agg_fn().is_none(),
                    is_key,
                    "{}.{}",
                    def.name(),
                    col.name
                );
            }
        }
        for def in TABLES.iter().filter(|d| d.kind == TableKind::Plain) {
            assert!(def.columns.iter().all(|c| c.ty.agg_fn().is_none()));
        }
    }

    #[test]
    fn sql_workspace_columns_follow_the_curated_rule() {
        for def in &TABLES {
            let readable: Vec<&str> = def.sql_columns().map(|c| c.name).collect();
            for hidden in [
                "wal_epoch",
                "batch_seq",
                "lsn",
                "received_at",
                "marker_epoch",
                "marker_batch_seq",
                "marker_at",
                "api_key_id",
                "row_digest",
            ] {
                assert!(!readable.contains(&hidden), "{}.{hidden}", def.name());
            }
            let hidden = [TableId::DedupSelftest, TableId::McpToolRollupHourly];
            assert_eq!(readable.is_empty(), hidden.contains(&def.id));
        }
    }

    /// An hourly tier has its table's columns in its order, with the same types, codecs and workspace
    /// flags, and differs only in the name of the time column; its key, partition and retention follow the
    /// renamed column. A read of the tier is therefore a read of the table with coarser time.
    #[test]
    fn the_hourly_tier_mirrors_its_minute_table() {
        let tiers: Vec<(TableId, TableId)> = TableId::ALL
            .into_iter()
            .filter_map(|id| hourly_tier(id).map(|tier| (id, tier)))
            .collect();
        assert_eq!(
            tiers,
            [(TableId::McpToolRollup, TableId::McpToolRollupHourly)]
        );
        for (minute, hour) in tiers {
            let (m, h) = (table(minute), table(hour));
            let (m_time, h_time) = (m.retention.time_column, h.retention.time_column);
            assert_eq!((m_time, h_time), ("minute", "hour"));
            let rename = |name: &'static str| if name == m_time { h_time } else { name };
            assert_eq!(m.columns.len(), h.columns.len());
            for (a, b) in m.columns.iter().zip(h.columns) {
                assert_eq!(rename(a.name), b.name);
                assert_eq!((a.ty, a.codec), (b.ty, b.codec), "{}", a.name);
            }
            assert_eq!(m.kind, h.kind);
            assert_eq!(PartitionExpr::Month(h_time), h.partition);
            assert_eq!(PartitionExpr::Month(m_time), m.partition);
            let renamed: Vec<&str> = m.order_by.iter().map(|c| rename(c)).collect();
            assert_eq!(renamed, h.order_by);
            assert_eq!(m.primary_key_len, h.primary_key_len);
            assert_eq!((m.indexes, m.settings), (h.indexes, h.settings));
            assert_eq!(m.retention.binding, h.retention.binding);
            assert!(m.sql_readable && !h.sql_readable);
        }
    }

    #[test]
    fn codecs_follow_the_convention() {
        for def in &TABLES {
            for col in def.columns {
                match col.ty.scalar() {
                    Ty::DateTime64 => assert_eq!(col.codec, Codec::DeltaZstd1, "{}", col.name),
                    Ty::String if col.codec == Codec::Zstd3 => {}
                    Ty::UInt32 | Ty::UInt64 | Ty::Int64
                        if col.name.contains("hash") || col.name == "row_digest" =>
                    {
                        assert_eq!(col.codec, Codec::Zstd1, "{}", col.name);
                    }
                    Ty::UInt32 | Ty::UInt64 | Ty::Int64 => {
                        assert_eq!(col.codec, Codec::T64Zstd1, "{}", col.name);
                    }
                    _ => assert_eq!(col.codec, Codec::Zstd1, "{}", col.name),
                }
            }
        }
    }

    #[test]
    fn type_strings_and_wire_types() {
        let arr_lc = Ty::Array(&Ty::LowCardinalityString);
        assert_eq!(arr_lc.to_string(), "Array(LowCardinality(String))");
        assert_eq!(arr_lc.wire().to_string(), "Array(String)");
        assert_eq!(
            HIST.to_string(),
            "SimpleAggregateFunction(sumMap, Tuple(Array(UInt16), Array(UInt64)))"
        );
        assert_eq!(HIST.wire().to_string(), HIST.to_string());
        assert_eq!(
            MARKER_AT.wire().to_string(),
            "SimpleAggregateFunction(max, DateTime('UTC'))"
        );
        let uniq_lc = Ty::Saf(
            crate::AggFn::GroupUniqArrayArray(8),
            &Ty::Array(&Ty::LowCardinalityString),
        );
        assert_eq!(
            uniq_lc.wire().to_string(),
            "SimpleAggregateFunction(groupUniqArrayArray(8), Array(String))"
        );
        assert_eq!(
            Ty::Enum8(EnumKind::StatusCode).to_string(),
            "Enum8('unset' = 0, 'ok' = 1, 'error' = 2)"
        );
    }
}
