//! Facts (contract 3.8 and 8.7): the candidate rows of the facts of a target, one plan per fact table. No plan
//! orders facts or picks a winner: ClickHouse compares UUIDs by their second half first, so the query service
//! resolves the rows with `crucible_records::fold_facts`, the comparator the store model and the differential
//! check share.

use crucible_records::{FactTargetKind, ProjectScope, SpanId, TableId, TimeWindow, TraceId};
use crucible_schema::{
    Column, ColumnRef, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol,
};

use crate::build::RetentionClamp;
use crate::class::{Caller, QueryClass};
use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Node, Scan};
use crate::plan::QueryPlan;
use crate::pred::Pred;

/// A fact table.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FactTable {
    /// `fact_scores`.
    Scores,
    /// `fact_tags`.
    Tags,
    /// `fact_flags`.
    Flags,
    /// `fact_corrections`.
    Corrections,
}

impl FactTable {
    /// Every fact table.
    pub const ALL: [Self; 4] = [Self::Scores, Self::Tags, Self::Flags, Self::Corrections];

    /// The table.
    pub const fn table(self) -> TableId {
        match self {
            Self::Scores => TableId::FactScores,
            Self::Tags => TableId::FactTags,
            Self::Flags => TableId::FactFlags,
            Self::Corrections => TableId::FactCorrections,
        }
    }
}

/// Whose facts a read fetches.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FactsOf<'a> {
    /// The facts whose target is one of these traces.
    Traces(&'a [TraceId]),
    /// The facts whose target is this span of this trace.
    Span(TraceId, SpanId),
    /// The facts whose target is this session.
    Session(&'a str),
}

/// The columns of one fact table that a read selects or filters on.
struct Columns {
    envelope: [ColumnRef; 11],
    body: Vec<ColumnRef>,
    target_kind: ColumnRef,
    trace_id: ColumnRef,
    span_id: ColumnRef,
    session_id: ColumnRef,
}

macro_rules! columns {
    ($c:ident, [$($body:ident),*]) => {
        Columns {
            envelope: [
                $c::TargetKind,
                $c::TraceId,
                $c::SpanId,
                $c::SessionId,
                $c::Key,
                $c::Version,
                $c::FactId,
                $c::Retracted,
                $c::AuthorKind,
                $c::AuthorId,
                $c::CreatedAt,
            ]
            .map(|c| c.column_ref()),
            body: vec![$($c::$body.column_ref()),*],
            target_kind: $c::TargetKind.column_ref(),
            trace_id: $c::TraceId.column_ref(),
            span_id: $c::SpanId.column_ref(),
            session_id: $c::SessionId.column_ref(),
        }
    };
}

fn columns(table: FactTable) -> Columns {
    match table {
        FactTable::Scores => columns!(
            FactScoresCol,
            [
                DataType, ValueNum, ValueStr, Comment, Source, Metadata, ConfigId
            ]
        ),
        FactTable::Tags => columns!(FactTagsCol, [Op]),
        FactTable::Flags => columns!(FactFlagsCol, [Value]),
        FactTable::Corrections => columns!(FactCorrectionsCol, [Corrected, Note]),
    }
}

fn kind(columns: &Columns, kind: FactTargetKind) -> Pred {
    Pred::eq(columns.target_kind, Literal::Enum(kind.value()))
}

fn scan(table: FactTable, of: FactsOf<'_>, brief: bool) -> Result<Scan, PlanError> {
    let c = columns(table);
    let pred = match of {
        FactsOf::Traces(ids) => Pred::And(vec![
            kind(&c, FactTargetKind::Trace),
            Pred::is_in(c.trace_id, ids.iter().copied().map(Literal::from).collect()),
        ]),
        FactsOf::Span(trace_id, span_id) => Pred::And(vec![
            kind(&c, FactTargetKind::Span),
            Pred::eq(c.trace_id, trace_id),
            Pred::eq(c.span_id, span_id),
        ]),
        FactsOf::Session(session_id) => Pred::And(vec![
            kind(&c, FactTargetKind::Session),
            Pred::eq(c.session_id, session_id),
        ]),
    };
    let mut selected = c.envelope.to_vec();
    match (table, brief) {
        // A list shows a score's name and value, not its comment or metadata.
        (FactTable::Scores, true) => selected.extend(c.body.iter().take(3).chain(&c.body[4..5])),
        (FactTable::Corrections, true) => {}
        _ => selected.extend(c.body),
    }
    Scan::new(table.table(), selected, pred)
}

/// The candidate facts of a target in one fact table, in no order, for a detail read (trace detail, the facts
/// of one target). The outputs are, in order: `target_kind`, `trace_id`, `span_id`, `session_id`, `key`,
/// `version`, `fact_id`, `retracted`, `author_kind`, `author_id`, `created_at`, then the table's body columns
/// in declaration order: for scores `data_type`, `value_num`, `value_str`, `comment`, `source`, `metadata` and
/// `config_id`; for tags `op`; for flags `value`; for corrections `corrected` and `note`.
///
/// The window is over the facts' `target_time`, which the write path takes from the target's earliest known
/// start, so the window of the target's own time bounds holds every fact on it.
pub fn fact_rows(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    table: FactTable,
    of: FactsOf<'_>,
) -> Result<Option<QueryPlan>, PlanError> {
    let scan = scan(table, of, false)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::Point),
        Node::Scan(scan),
    )
}

/// The candidate facts of a page of traces in one fact table, for the decoration of a list (contract 3.8): the
/// facts whose target is one of the traces, without the large body columns. The outputs are those of
/// [`fact_rows`] except that scores leave out `comment`, `metadata` and `config_id`, and corrections carry no
/// body column.
pub fn fact_page(
    scope: ProjectScope,
    window: TimeWindow,
    clamp: &RetentionClamp,
    caller: Caller,
    table: FactTable,
    trace_ids: &[TraceId],
) -> Result<Option<QueryPlan>, PlanError> {
    let scan = scan(table, FactsOf::Traces(trace_ids), true)?;
    clamp.plan(
        scope,
        window,
        caller.class(QueryClass::List),
        Node::Scan(scan),
    )
}
