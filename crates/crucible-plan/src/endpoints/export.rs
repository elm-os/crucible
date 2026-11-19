//! Exports (contracts 8.7 and 12.8): a keyset scan of `span_core` in the table's key order under the export's
//! fixed `as_of`, then lookups of the payload rows, the MCP rows and the facts of each page. Every plan runs in
//! class `export`.

use crucible_records::{ProjectScope, SpanId, TableId, TimeWindow, Timestamp, TraceId};
use crucible_schema::{Column, McpContentCol, McpOpsCol, SpanCoreCol, SpanPayloadCol};

use crate::build::RetentionClamp;
use crate::class::QueryClass;
use crate::endpoints::facts::{FactTable, FactsOf, scan_every_column};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{ArrivalBounds, Direction, Node, Order, PointLookup, Scan};
use crate::plan::QueryPlan;
use crate::pred::{MAX_LIST, Pred};

/// The largest page of [`export_spans`].
pub const EXPORT_PAGE_MAX: u32 = 10_000;

/// What an export reads besides its window: every condition given must hold.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ExportSelection<'a> {
    /// Only these traces; every trace when empty. At most [`MAX_LIST`].
    pub trace_ids: &'a [TraceId],
    /// Only spans of this user.
    pub user_id: Option<&'a str>,
    /// Only spans of this session.
    pub session_id: Option<&'a str>,
}

/// A span's position in the key order of `span_core`: `(start_minute, trace_hash, start_time, span_id)`, with
/// `start_minute` as the first instant of the minute. Rows of one span sent twice share it, and so, rarely, do
/// two spans whose trace hashes and span ids collide.
pub type ExportKey = (Timestamp, u32, Timestamp, SpanId);

/// The key order columns, which are the columns of [`ExportKey`].
const KEY: [SpanCoreCol; 4] = [
    SpanCoreCol::StartMinute,
    SpanCoreCol::TraceHash,
    SpanCoreCol::StartTime,
    SpanCoreCol::SpanId,
];

fn key_literals(key: ExportKey) -> Vec<Literal> {
    vec![
        key.0.into(),
        Literal::from(key.1),
        key.2.into(),
        Literal::from(key.3),
    ]
}

fn selection(selection: ExportSelection<'_>) -> Result<Pred, PlanError> {
    if selection.trace_ids.len() > MAX_LIST {
        return Err(PlanError::Limit);
    }
    let mut all = Vec::new();
    if !selection.trace_ids.is_empty() {
        let ids = selection.trace_ids.iter().copied().map(Literal::from);
        all.push(Pred::is_in(SpanCoreCol::TraceId, ids.collect()));
    }
    if let Some(user) = selection.user_id {
        all.push(Pred::eq(SpanCoreCol::UserId, user));
    }
    if let Some(session) = selection.session_id {
        all.push(Pred::eq(SpanCoreCol::SessionId, session));
    }
    Ok(Pred::And(all))
}

fn spans_scan(
    selected: ExportSelection<'_>,
    as_of: Timestamp,
    extra: Vec<Pred>,
) -> Result<Scan, PlanError> {
    let mut pred = selection(selected)?;
    if let Pred::And(all) = &mut pred {
        all.extend(extra);
    }
    Scan::new(TableId::SpanCore, SpanCoreCol::ALL.iter().copied(), pred)?
        .ordered(Order::by(KEY, Direction::Asc))?
        .arrived(ArrivalBounds {
            as_of: Some(as_of),
            since: None,
        })
}

/// One page of an export: every column of `span_core` for the selected spans that start inside the window and
/// arrived at or before `as_of`, in key order ([`ExportKey`]), after `after` when given, at most `limit` rows
/// (1 to [`EXPORT_PAGE_MAX`]). Rows that tie on the key can straddle the page's end; [`export_ties`] reads them
/// whole.
pub fn export_spans(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    selected: ExportSelection<'_>,
    as_of: Timestamp,
    after: Option<ExportKey>,
    limit: u32,
) -> Result<Option<QueryPlan>, PlanError> {
    if limit == 0 || limit > EXPORT_PAGE_MAX {
        return Err(PlanError::Limit);
    }
    let mut scan = spans_scan(selected, as_of, Vec::new())?.limited(limit)?;
    if let Some(key) = after {
        scan = scan.after(key_literals(key))?;
    }
    clamp.plan(scope, window, QueryClass::Export, Node::Scan(scan))
}

/// Every selected row of `span_core` whose key is `key`, under the same conditions as [`export_spans`]: what a
/// page that ends inside a run of tied rows reads instead of cutting the run.
pub fn export_ties(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    selected: ExportSelection<'_>,
    as_of: Timestamp,
    key: ExportKey,
) -> Result<Option<QueryPlan>, PlanError> {
    let equal = KEY
        .iter()
        .zip(key_literals(key))
        .map(|(column, value)| Pred::eq(*column, value))
        .collect();
    let scan = spans_scan(selected, as_of, equal)?;
    clamp.plan(scope, window, QueryClass::Export, Node::Scan(scan))
}

/// A span as the lookups of one page name it: `(start_time, trace_id, span_id)`.
pub type ExportSpanKey = (Timestamp, TraceId, SpanId);

fn lookup_keys(spans: &[ExportSpanKey]) -> Result<Vec<Vec<Literal>>, PlanError> {
    if spans.is_empty() || spans.len() > MAX_LIST {
        return Err(PlanError::Limit);
    }
    Ok(spans
        .iter()
        .map(|(start, trace, span)| {
            vec![
                start.floor_minute().into(),
                Literal::from(trace.trace_hash()),
                Literal::from(*span),
                Literal::from(*trace),
            ]
        })
        .collect())
}

/// The window of a set of spans: from the earliest start to just after the latest.
fn spans_window(spans: &[ExportSpanKey]) -> Result<TimeWindow, PlanError> {
    let from = spans.iter().map(|s| s.0).min().ok_or(PlanError::Empty)?;
    let last = spans.iter().map(|s| s.0).max().ok_or(PlanError::Empty)?;
    let to = last.checked_add_micros(1).ok_or(PlanError::Bounds)?;
    TimeWindow::new(from, to).map_err(|_| PlanError::Bounds)
}

/// The payload rows of some spans of a page (1 to [`MAX_LIST`]), every column of `span_payload`, in no order. A
/// span sent twice has a row per copy; a span whose payload expired under the payload retention has none.
pub fn export_payloads(
    scope: ProjectScope,
    clamp: &RetentionClamp,
    spans: &[ExportSpanKey],
) -> Result<Option<QueryPlan>, PlanError> {
    let lookup = PointLookup::new(
        TableId::SpanPayload,
        [
            SpanPayloadCol::StartMinute,
            SpanPayloadCol::TraceHash,
            SpanPayloadCol::SpanId,
            SpanPayloadCol::TraceId,
        ],
        lookup_keys(spans)?,
        SpanPayloadCol::ALL.iter().copied(),
    )?;
    clamp.plan(
        scope,
        spans_window(spans)?,
        QueryClass::Export,
        Node::PointLookup(lookup),
    )
}

/// The MCP operation rows of some spans of a page (1 to [`MAX_LIST`]), every column of `mcp_ops`, in no order.
pub fn export_mcp_ops(
    scope: ProjectScope,
    clamp: &RetentionClamp,
    spans: &[ExportSpanKey],
) -> Result<Option<QueryPlan>, PlanError> {
    let lookup = PointLookup::new(
        TableId::McpOps,
        [
            McpOpsCol::StartMinute,
            McpOpsCol::TraceHash,
            McpOpsCol::SpanId,
            McpOpsCol::TraceId,
        ],
        lookup_keys(spans)?,
        McpOpsCol::ALL.iter().copied(),
    )?;
    clamp.plan(
        scope,
        spans_window(spans)?,
        QueryClass::Export,
        Node::PointLookup(lookup),
    )
}

/// The captured MCP content rows of some spans of a page (1 to [`MAX_LIST`]), every column of `mcp_content`,
/// in no order.
pub fn export_mcp_content(
    scope: ProjectScope,
    clamp: &RetentionClamp,
    spans: &[ExportSpanKey],
) -> Result<Option<QueryPlan>, PlanError> {
    let lookup = PointLookup::new(
        TableId::McpContent,
        [
            McpContentCol::StartMinute,
            McpContentCol::TraceHash,
            McpContentCol::SpanId,
            McpContentCol::TraceId,
        ],
        lookup_keys(spans)?,
        McpContentCol::ALL.iter().copied(),
    )?;
    clamp.plan(
        scope,
        spans_window(spans)?,
        QueryClass::Export,
        Node::PointLookup(lookup),
    )
}

/// The candidate facts of the traces of a page in one fact table, about the traces themselves and about their
/// spans, in no order: every column of the table, in the table's order. The window is over the facts'
/// `target_time`.
pub fn export_facts(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    table: FactTable,
    trace_ids: &[TraceId],
) -> Result<Option<QueryPlan>, PlanError> {
    let scan = scan_every_column(table, FactsOf::TraceTree(trace_ids))?;
    clamp.plan(scope, window, QueryClass::Export, Node::Scan(scan))
}
