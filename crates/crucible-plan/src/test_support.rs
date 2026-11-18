//! Probes and unclamped plans for tests of stores in other crates. Enabled by the `test-support` feature, which
//! product builds never enable: in product code only `crucible-shipper` builds a [`BatchProbe`], through
//! `BatchProbe::for_shipper`, and a [`QueryPlan`] comes only from this crate's builders, which clamp its window to
//! the project's retention.

use crucible_records::{Lsn, PartitionId, ProjectScope, TableId, TimeWindow, WalEpoch};

use crate::class::{ClassLimits, QueryClass};
use crate::error::PlanError;
use crate::node::Node;
use crate::plan::{BatchProbe, QueryPlan};

/// A plan of any node over a window that no retention clamp has seen, for tests only.
pub fn query_plan(
    scope: ProjectScope,
    window: TimeWindow,
    class: QueryClass,
    root: Node,
) -> QueryPlan {
    QueryPlan::new(scope, window, class, root)
}

/// A probe for the block of one (batch, table, partition) pair, for tests only.
pub fn batch_probe(
    table_id: TableId,
    partition: PartitionId,
    epoch: WalEpoch,
    batch_seq: Lsn,
) -> Result<BatchProbe, PlanError> {
    BatchProbe::checked(table_id, partition, epoch, batch_seq)
}

/// The same plan under other limits than its class's defaults, for tests of the guardrails only: every store
/// applies the limits the plan carries.
pub fn with_limits(plan: QueryPlan, limits: ClassLimits) -> QueryPlan {
    plan.with_limits(limits)
}
