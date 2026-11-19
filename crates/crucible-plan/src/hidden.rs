//! Rows that a pending deletion request hides from typed reads (contract 13.4): from the moment a request is
//! accepted until its deletes have run, every plan a read builds carries the request as a predicate.
//!
//! A request hides a row of a table by the columns the table has: `trace_id` for a trace deletion, `user_id`
//! for a user deletion, `session_id` for a session deletion, and only rows received before the request where
//! the table records an arrival time. A table without the selector's column is reached through the traces
//! that the span tables no longer show.

use crucible_records::{TableId, Timestamp, TraceId};

use crate::endpoints::{lifecycle_column, trace_id_in};
use crate::literal::Literal;
use crate::plan::QueryPlan;
use crate::pred::{CmpOp, Pred};

/// What a pending deletion selects.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HiddenSelector {
    /// These traces, at most 10,000.
    Traces(Vec<TraceId>),
    /// The rows of this `user_id`.
    User(String),
    /// The rows of this `session_id`.
    Session(String),
}

/// One pending deletion as reads see it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Hidden {
    /// What is deleted.
    pub selector: HiddenSelector,
    /// Data received at or after this instant is kept.
    pub requested_at: Timestamp,
}

impl Hidden {
    /// Whether the deletion hides a row with these fields.
    pub fn hides(
        &self,
        trace_id: TraceId,
        user_id: &str,
        session_id: &str,
        received_at: Timestamp,
    ) -> bool {
        received_at < self.requested_at
            && match &self.selector {
                HiddenSelector::Traces(ids) => ids.contains(&trace_id),
                HiddenSelector::User(user) => !user.is_empty() && user == user_id,
                HiddenSelector::Session(session) => !session.is_empty() && session == session_id,
            }
    }

    /// The rows of `table_id` the deletion hides, or `None` when the table lacks the selector's column.
    fn selected(&self, table_id: TableId) -> Option<Pred> {
        let selected = match &self.selector {
            HiddenSelector::Traces(ids) if !ids.is_empty() => {
                trace_id_in(lifecycle_column(table_id, "trace_id")?, ids)
            }
            HiddenSelector::User(user) if !user.is_empty() => Pred::Cmp(
                lifecycle_column(table_id, "user_id")?,
                CmpOp::Eq,
                Literal::from(user.as_str()),
            ),
            HiddenSelector::Session(session) if !session.is_empty() => Pred::Cmp(
                lifecycle_column(table_id, "session_id")?,
                CmpOp::Eq,
                Literal::from(session.as_str()),
            ),
            _ => return None,
        };
        // The tables merged by key carry no arrival time: their rows are hidden by key, as they are deleted.
        Some(match lifecycle_column(table_id, "received_at") {
            Some(received_at) => Pred::And(vec![
                selected,
                Pred::Cmp(received_at, CmpOp::Lt, Literal::Time(self.requested_at)),
            ]),
            None => selected,
        })
    }
}

/// Narrows `pred` to the rows of `table_id` that no deletion of `hidden` selects.
pub(crate) fn keep(table_id: TableId, pred: &mut Pred, hidden: &[Hidden]) {
    let hides: Vec<Pred> = hidden
        .iter()
        .filter_map(|h| h.selected(table_id))
        .filter(|hide| hide.check(table_id).is_ok())
        .map(|hide| Pred::Not(Box::new(hide)))
        .collect();
    if hides.is_empty() {
        return;
    }
    let own = core::mem::replace(pred, Pred::And(Vec::new()));
    let mut all = match own {
        Pred::And(operands) => operands,
        other => vec![other],
    };
    all.extend(hides);
    *pred = Pred::And(all);
}

/// The most trace ids the pending deletions of a project add to one scan. Every id is a parameter of the
/// statement, a plan can hold two scans beside its own lists, and a ClickHouse server refuses a request with
/// more than 1,000 parameters.
pub const HIDDEN_TRACE_IDS: usize = 128;
/// The most pending deletions by user or session that one scan carries.
pub const HIDDEN_REQUESTS: usize = 32;

/// The deletions a plan can carry, oldest first: every deletion until the budgets run out, and of a list of
/// traces the ids that still fit. What does not fit stays readable until its deletes have run.
fn within_budget(hidden: &[Hidden]) -> Vec<Hidden> {
    let mut oldest: Vec<&Hidden> = hidden.iter().collect();
    oldest.sort_by_key(|pending| pending.requested_at);
    let (mut ids_left, mut others_left) = (HIDDEN_TRACE_IDS, HIDDEN_REQUESTS);
    let mut carried = Vec::new();
    for pending in oldest {
        match &pending.selector {
            HiddenSelector::Traces(ids) => {
                let fits = ids.len().min(ids_left);
                if fits > 0 {
                    ids_left -= fits;
                    carried.push(Hidden {
                        selector: HiddenSelector::Traces(ids[..fits].to_vec()),
                        requested_at: pending.requested_at,
                    });
                }
            }
            HiddenSelector::User(_) | HiddenSelector::Session(_) => {
                if others_left > 0 {
                    others_left -= 1;
                    carried.push(pending.clone());
                }
            }
        }
    }
    carried
}

impl QueryPlan {
    /// The same plan without the rows that pending deletions hide, within [`HIDDEN_TRACE_IDS`] and
    /// [`HIDDEN_REQUESTS`]. An empty list changes nothing.
    #[must_use]
    pub fn hiding(mut self, hidden: &[Hidden]) -> Self {
        if !hidden.is_empty() {
            self.root_mut().hide(&within_budget(hidden));
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use crucible_auth::test_support::scope;
    use crucible_records::{ProjectId, TimeWindow};
    use crucible_schema::{Column, SpanCoreCol, SpanMetricsCol, TraceStatsCol};

    use super::*;
    use crate::class::QueryClass;
    use crate::node::{Aggregate, Measure, Node, Scan};

    fn plan(root: Node) -> QueryPlan {
        let window = TimeWindow::new(Timestamp::EPOCH, Timestamp::from_micros(1 << 40)).unwrap();
        QueryPlan::new(
            scope(ProjectId::from_bytes([9; 16])).unwrap(),
            window,
            QueryClass::Point,
            root,
        )
    }

    fn hidden() -> Vec<Hidden> {
        vec![
            Hidden {
                selector: HiddenSelector::Traces(vec![TraceId::from_bytes([7; 16])]),
                requested_at: Timestamp::from_micros(50),
            },
            Hidden {
                selector: HiddenSelector::User("ada".to_owned()),
                requested_at: Timestamp::from_micros(60),
            },
        ]
    }

    #[test]
    fn a_scan_keeps_its_predicate_and_gains_one_term_per_deletion_its_table_can_express() {
        let own = Pred::eq(SpanCoreCol::Name, "root");
        let scan = Scan::new(TableId::SpanCore, [SpanCoreCol::SpanId], own.clone()).unwrap();
        let plan = plan(Node::Scan(scan)).hiding(&hidden());
        let Node::Scan(scan) = plan.root() else {
            panic!("a scan stays a scan");
        };
        let Pred::And(terms) = scan.pred() else {
            panic!("the predicate is a conjunction");
        };
        assert_eq!(terms.len(), 3);
        assert_eq!(terms[0], own);
        assert!(terms[1..].iter().all(|term| matches!(term, Pred::Not(_))));
    }

    #[test]
    fn a_table_merged_by_key_is_hidden_by_key_and_a_table_without_the_column_is_left_alone() {
        let stats = Aggregate::new(
            TableId::TraceStats,
            Pred::And(Vec::new()),
            [TraceStatsCol::TraceId],
            vec![Measure::Min(TraceStatsCol::RowDay.column_ref())],
        )
        .unwrap();
        let hiding = plan(Node::Aggregate(stats)).hiding(&hidden());
        let Node::Aggregate(stats) = hiding.root() else {
            panic!("an aggregate stays an aggregate");
        };
        // Only the trace deletion reaches `trace_stats`, and without an arrival term.
        assert_eq!(
            stats.pred(),
            &Pred::And(vec![Pred::Not(Box::new(Pred::In(
                TraceStatsCol::TraceId.column_ref(),
                vec![Literal::from(TraceId::from_bytes([7; 16]))]
            )))])
        );
        let metrics = Aggregate::new(
            TableId::SpanMetrics,
            Pred::And(Vec::new()),
            [SpanMetricsCol::Environment],
            vec![Measure::Min(SpanMetricsCol::Minute.column_ref())],
        )
        .unwrap();
        let before = plan(Node::Aggregate(metrics));
        assert_eq!(before.clone().hiding(&hidden()), before);
    }

    #[test]
    fn a_long_list_of_traces_becomes_one_in_list_per_thousand() {
        let column = SpanCoreCol::TraceId.column_ref();
        let ids = |n: u128| -> Vec<TraceId> {
            (0..n)
                .map(|i| TraceId::from_bytes(i.to_be_bytes()))
                .collect()
        };
        assert!(matches!(trace_id_in(column, &ids(500)), Pred::In(_, list) if list.len() == 500));
        let Pred::Or(lists) = trace_id_in(column, &ids(10_000)) else {
            panic!("a list above the chunk is a disjunction");
        };
        assert_eq!(lists.len(), 20);
        let Pred::Or(lists) = trace_id_in(column, &ids(501)) else {
            panic!("a list above the chunk is a disjunction");
        };
        assert!(matches!(&lists[1], Pred::In(_, list) if list.len() == 1));
        // The longest request a project can have still makes a plan the table accepts.
        let longest = Hidden {
            selector: HiddenSelector::Traces(ids(10_000)),
            requested_at: Timestamp::from_micros(50),
        };
        assert!(
            longest
                .selected(TableId::SpanCore)
                .unwrap()
                .check(TableId::SpanCore)
                .is_ok()
        );
    }

    #[test]
    fn a_plan_carries_the_oldest_deletions_that_fit_its_budget() {
        let id = |n: u128| TraceId::from_bytes(n.to_be_bytes());
        let traces = |from: u128, count: u128, at: i64| Hidden {
            selector: HiddenSelector::Traces((from..from + count).map(id).collect()),
            requested_at: Timestamp::from_micros(at),
        };
        let carried = within_budget(&[traces(5_000, 1_000, 30), traces(0, 100, 10)]);
        // The older request whole, then the first 28 ids of the newer one.
        assert_eq!(carried.len(), 2);
        assert_eq!(carried[0], traces(0, 100, 10));
        assert_eq!(carried[1], traces(5_000, 28, 30));
        let users: Vec<Hidden> = (0..100)
            .map(|n| Hidden {
                selector: HiddenSelector::User(format!("u{n}")),
                requested_at: Timestamp::from_micros(100 - n),
            })
            .collect();
        let carried = within_budget(&users);
        assert_eq!(carried.len(), HIDDEN_REQUESTS);
        assert_eq!(carried[0].selector, HiddenSelector::User("u99".to_owned()));
        // A request of the longest list still gives a statement of bounded size.
        let scan = Scan::new(TableId::SpanCore, [SpanCoreCol::SpanId], Pred::all()).unwrap();
        let hiding = plan(Node::Scan(scan)).hiding(&[traces(0, 10_000, 5)]);
        let Node::Scan(scan) = hiding.root() else {
            panic!("a scan stays a scan");
        };
        let listed = format!("{:?}", scan.pred()).matches("Bytes").count();
        assert!(listed <= HIDDEN_TRACE_IDS, "{listed}");
    }

    #[test]
    fn a_deletion_hides_only_what_arrived_before_it() {
        let [traces, user] = <[Hidden; 2]>::try_from(hidden()).unwrap();
        let id = TraceId::from_bytes([7; 16]);
        assert!(traces.hides(id, "", "", Timestamp::from_micros(49)));
        assert!(!traces.hides(id, "", "", Timestamp::from_micros(50)));
        assert!(!traces.hides(TraceId::from_bytes([8; 16]), "", "", Timestamp::EPOCH));
        assert!(user.hides(id, "ada", "", Timestamp::from_micros(59)));
        assert!(!user.hides(id, "bob", "", Timestamp::from_micros(59)));
        let nobody = Hidden {
            selector: HiddenSelector::Session(String::new()),
            requested_at: Timestamp::from_micros(60),
        };
        assert!(!nobody.hides(id, "", "", Timestamp::EPOCH));
    }
}
