//! The metrics registry (contract section 8.8). `crucible-plan` owns it; this is the skeleton that the first
//! plans use: the sources and the rollup that carries each source's data bounds. Measures, dimensions and filter
//! fields join it with the metrics compiler.

use crucible_records::TableId;
use crucible_schema::{Column, ColumnRef, McpToolRollupCol, SpanMetricsCol};

/// A metrics source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Source {
    /// Spans.
    Spans,
    /// MCP operations.
    McpOps,
    /// Traces.
    Traces,
}

/// The minute rollup of a source.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Rollup {
    /// The rollup table.
    pub table: TableId,
    /// Its minute column.
    pub minute: ColumnRef,
}

impl Source {
    /// Every source.
    pub const ALL: [Self; 3] = [Self::Spans, Self::McpOps, Self::Traces];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spans => "spans",
            Self::McpOps => "mcp_ops",
            Self::Traces => "traces",
        }
    }

    /// The source with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == name)
    }

    /// The minute rollup that covers the source, if one exists. Traces have none.
    pub fn rollup(self) -> Option<Rollup> {
        match self {
            Self::Spans => Some(Rollup {
                table: SpanMetricsCol::TABLE,
                minute: SpanMetricsCol::Minute.column_ref(),
            }),
            Self::McpOps => Some(Rollup {
                table: McpToolRollupCol::TABLE,
                minute: McpToolRollupCol::Minute.column_ref(),
            }),
            Self::Traces => None,
        }
    }
}
