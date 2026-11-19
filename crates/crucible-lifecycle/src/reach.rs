//! The reach list as data (contract 13.4): which ClickHouse tables retention acts on, and which a deletion
//! request reaches, by which of its selectors.
//!
//! A deletion resolves the traces it selects in pages. Each page becomes deletes by trace id on the tables that
//! only a trace id reaches. Once every page is planned, the tables that carry the selector's own column
//! (`user_id` or `session_id`) get one delete per partition by that column. The span tables are among these,
//! so the spans that the resolution reads are still there while it pages.

use crucible_control::DeletionSelector;
use crucible_plan::lifecycle_time_column;
use crucible_records::TableId;
use crucible_schema::{RetentionBinding, table};

/// The span tiers and trace heads.
const SPAN_TABLES: [TableId; 3] = [TableId::SpanCore, TableId::SpanPayload, TableId::TraceHeads];
/// The MCP rows.
const MCP_TABLES: [TableId; 2] = [TableId::McpOps, TableId::McpContent];
/// The four fact tables.
const FACT_TABLES: [TableId; 4] = [
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
];

/// The reach-list places outside ClickHouse that a deletion request reports on (contract 13.4).
pub const OTHER_PLACES: [&str; 3] = ["comments", "media", EXPORT_FILES];
/// The place of the export files, which a completed request has expired and removed.
pub const EXPORT_FILES: &str = "export_files";

/// The partitioned tables with a retention setting whose time column a predicate can name: every table of
/// contract 6.3 except the two unpartitioned ones (their weekly purge by `last_seen` is contract 13.2 step 4)
/// and the service tables.
pub fn retention_tables() -> impl Iterator<Item = TableId> {
    TableId::ALL.into_iter().filter(|id| {
        let def = table(*id);
        def.is_scoped()
            && lifecycle_time_column(*id).is_some()
            && matches!(
                def.retention.binding,
                RetentionBinding::RetentionDays
                    | RetentionBinding::PayloadRetentionDays
                    | RetentionBinding::McpContentRetentionDays
            )
    })
}

/// Which selectors of a request a delete on one table carries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Reach {
    /// The table.
    pub table: TableId,
    /// The page's trace ids.
    pub traces: bool,
    /// The request's `user_id`.
    pub user: bool,
    /// The request's `session_id`.
    pub session: bool,
}

const fn by_trace(table: TableId) -> Reach {
    Reach {
        table,
        traces: true,
        user: false,
        session: false,
    }
}

/// The tables one page of resolved traces reaches, for a request with this selector.
pub fn page_reach(selector: &DeletionSelector) -> Vec<Reach> {
    let mut reach = Vec::new();
    if matches!(selector, DeletionSelector::Traces(_)) {
        reach.extend(SPAN_TABLES.map(by_trace));
    }
    let user = matches!(selector, DeletionSelector::User { .. });
    reach.extend(MCP_TABLES.map(|table| Reach {
        user,
        ..by_trace(table)
    }));
    reach.extend(FACT_TABLES.map(by_trace));
    reach.push(by_trace(TableId::TraceStats));
    reach
}

/// The tables a request reaches by its own column, once every page is planned.
pub fn final_reach(selector: &DeletionSelector) -> Vec<Reach> {
    let column = |table, user, session| Reach {
        table,
        traces: false,
        user,
        session,
    };
    match selector {
        DeletionSelector::Traces(_) => Vec::new(),
        DeletionSelector::User { .. } => SPAN_TABLES
            .into_iter()
            .chain([TableId::UserRollup])
            .map(|table| column(table, true, false))
            .collect(),
        DeletionSelector::Session(_) => SPAN_TABLES
            .into_iter()
            .chain(FACT_TABLES)
            .chain([TableId::SessionRollup])
            .map(|table| column(table, false, true))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crucible_plan::lifecycle_column;
    use crucible_records::TraceId;

    use super::*;

    fn selectors() -> [DeletionSelector; 3] {
        [
            DeletionSelector::Traces(vec![TraceId::from_bytes([1; 16])]),
            DeletionSelector::User {
                user_id: "ada".to_owned(),
                issuer: None,
            },
            DeletionSelector::Session("s-1".to_owned()),
        ]
    }

    #[test]
    fn retention_acts_on_every_partitioned_table() {
        let tables: Vec<&str> = retention_tables().map(|id| id.name()).collect();
        assert_eq!(
            tables,
            [
                "span_core",
                "span_payload",
                "mcp_ops",
                "mcp_content",
                "fact_scores",
                "fact_tags",
                "fact_flags",
                "fact_corrections",
                "trace_heads",
                "trace_stats",
                "session_rollup",
                "user_rollup",
                "span_metrics",
                "mcp_tool_rollup",
            ]
        );
    }

    /// Contract 13.4: every kind reaches the span tiers, the heads, the MCP rows, the four fact tables and
    /// `trace_stats`; a user also `user_rollup`, a session also `session_rollup`; never the minute rollups,
    /// the deploys or the inventory, which hold no user dimension.
    #[test]
    fn every_kind_reaches_its_places_by_columns_the_tables_have() {
        let common: BTreeSet<TableId> = SPAN_TABLES
            .into_iter()
            .chain(MCP_TABLES)
            .chain(FACT_TABLES)
            .chain([TableId::TraceStats])
            .collect();
        for selector in selectors() {
            let (page, last) = (page_reach(&selector), final_reach(&selector));
            let mut reached: BTreeSet<TableId> =
                page.iter().chain(&last).map(|r| r.table).collect();
            match selector {
                DeletionSelector::Traces(_) => {}
                DeletionSelector::User { .. } => assert!(reached.remove(&TableId::UserRollup)),
                DeletionSelector::Session(_) => assert!(reached.remove(&TableId::SessionRollup)),
            }
            assert_eq!(reached, common, "{selector:?}");
            for reach in page.iter().chain(&last) {
                assert!(reach.traces || reach.user || reach.session);
                for (wanted, name) in [
                    (reach.traces, "trace_id"),
                    (reach.user, "user_id"),
                    (reach.session, "session_id"),
                ] {
                    assert!(
                        !wanted || lifecycle_column(reach.table, name).is_some(),
                        "{} has no {name}",
                        reach.table.name()
                    );
                }
            }
            // The span tables are read while the traces are resolved, so they go last for a user or a session.
            if !matches!(selector, DeletionSelector::Traces(_)) {
                assert!(page.iter().all(|r| !SPAN_TABLES.contains(&r.table)));
            }
        }
    }
}
