//! The mapping traits (contract 10.2) and their one production implementation, [`TableMapper`].
//!
//! Stage A depends only on [`SpanMapper`] and [`McpProjector`]. [`TableMapper`] is a thin wrapper over the pure
//! table-driven engine: `detect` evaluates its table's detection rules and `map` runs the engine for one span. Test
//! doubles may implement the traits; no second production implementation exists.
//!
//! The traits live here rather than in `crucible-compat` because they name [`SpanView`], and `crucible-compat` may
//! depend only on `crucible-records` (contract 1.5).

use crucible_compat::{Confidence, DIALECT_0, DetectOn, DialectTable};
use crucible_otlp::{ResourceView, ScopeView, SpanView};
use crucible_records::{Level, McpOpRecord, McpToolInventoryRecord, SpanRecord};

use crate::IngestFrame;
use crate::engine;

/// A span as mappers fill it: the record before the request-local forward-fill, defaults, cost, caps and the row
/// digest. Stage A fills the fields that do not depend on the dialect (ids, times, status, scope, links, the
/// envelope and ingestion values) before a mapper runs.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpanDraft {
    /// The record under construction.
    pub record: SpanRecord,
    /// An explicit level. Without one the level is `error` when the status is `error`, else `default`.
    pub level: Option<Level>,
}

/// What a mapper may read besides the span.
#[derive(Clone, Copy, Debug)]
pub struct MapCtx<'a> {
    /// The frame being normalized: the envelope and the frame header values.
    pub frame: &'a IngestFrame<'a>,
    /// The normalizer version that stage A records.
    pub normalizer_version: u32,
}

/// Request-level hints for detection.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hints {
    /// The envelope's `compat_dialect`; 0 when the request carries no hint.
    pub dialect: u8,
}

/// Maps spans of one dialect (contract 10.2).
pub trait SpanMapper: Sync {
    /// The dialect id ([`crucible_compat::DIALECTS`]).
    fn dialect(&self) -> u8;

    /// How sure the mapper is that the span belongs to its dialect, or `None` when it does not.
    fn detect(
        &self,
        resource: ResourceView<'_>,
        scope: ScopeView<'_>,
        span: &SpanView<'_>,
        hints: &Hints,
    ) -> Option<Confidence>;

    /// Maps the span onto the draft.
    fn map(&self, ctx: &MapCtx<'_>, span: &SpanView<'_>, out: &mut SpanDraft);
}

/// The draft of an MCP operation. Milestone 3 gives it its own fields; until then it names the record.
pub type McpOpDraft = McpOpRecord;

/// The draft of a tool inventory entry. Milestone 3 gives it its own fields; until then it names the record.
pub type InventoryDraft = McpToolInventoryRecord;

/// Projects MCP operations and tool inventory entries from mapped spans (contract 9.1). Stage A calls it from
/// milestone 3; until then the production implementation projects nothing.
pub trait McpProjector: Sync {
    /// The MCP operation of the span, which exists if and only if the span carries `mcp.method.name`.
    fn project(&self, draft: &SpanDraft, span: &SpanView<'_>) -> Option<McpOpDraft>;

    /// The tool inventory entries the span reports.
    fn inventory(&self, draft: &SpanDraft, span: &SpanView<'_>) -> Vec<InventoryDraft>;
}

/// The production [`SpanMapper`] and [`McpProjector`]: one dialect table run by the engine.
#[derive(Clone, Copy, Debug)]
pub struct TableMapper {
    table: &'static DialectTable,
}

impl TableMapper {
    /// A mapper for one dialect table.
    pub const fn new(table: &'static DialectTable) -> Self {
        Self { table }
    }

    /// The table the mapper runs.
    pub const fn table(&self) -> &'static DialectTable {
        self.table
    }
}

impl SpanMapper for TableMapper {
    fn dialect(&self) -> u8 {
        self.table.dialect
    }

    /// The highest confidence among the table's rules that match. The request hint is not a rule: stage A applies
    /// it before detection.
    fn detect(
        &self,
        _resource: ResourceView<'_>,
        scope: ScopeView<'_>,
        span: &SpanView<'_>,
        _hints: &Hints,
    ) -> Option<Confidence> {
        self.table
            .detect
            .iter()
            .filter(|rule| match rule.on {
                DetectOn::ScopeName(name) => name.matches(scope.name()),
                DetectOn::SpanKey(key) => span.attributes().any(|(k, _)| key.matches(k)),
            })
            .map(|rule| rule.confidence)
            .max()
    }

    fn map(&self, ctx: &MapCtx<'_>, span: &SpanView<'_>, out: &mut SpanDraft) {
        engine::map_span(self.table, ctx, span, out);
    }
}

impl McpProjector for TableMapper {
    fn project(&self, _draft: &SpanDraft, _span: &SpanView<'_>) -> Option<McpOpDraft> {
        None
    }

    fn inventory(&self, _draft: &SpanDraft, _span: &SpanView<'_>) -> Vec<InventoryDraft> {
        Vec::new()
    }
}

/// The native mapper (dialect 0), the only production mapper in milestone 1.
pub static NATIVE: TableMapper = TableMapper::new(&DIALECT_0);
