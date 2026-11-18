//! The three typed forms a store accepts: `QueryPlan`, `BatchProbe` and `DeletePlan` (contract section 8.1).

use crucible_records::{Lsn, PartitionId, ProjectScope, TableId, TimeWindow, Timestamp, WalEpoch};
use crucible_schema::{PartitionExpr, ServiceIdentity, TableKind, table};

use crate::class::QueryClass;
use crate::error::PlanError;
use crate::node::Node;
use crate::pred::Pred;
use crate::value::{self, OutputSchema};

/// A telemetry read. It cannot be built without a scope and a window: the renderer turns the scope into
/// `project_id = {scope}` on every table reference and the window into the sort-key and partition predicates.
#[derive(Clone, PartialEq, Debug)]
pub struct QueryPlan {
    scope: ProjectScope,
    window: TimeWindow,
    class: QueryClass,
    root: Node,
}

impl QueryPlan {
    /// A plan for one project and one time window.
    pub const fn new(
        scope: ProjectScope,
        window: TimeWindow,
        class: QueryClass,
        root: Node,
    ) -> Self {
        Self {
            scope,
            window,
            class,
            root,
        }
    }

    /// The project the plan reads.
    pub const fn scope(&self) -> ProjectScope {
        self.scope
    }

    /// The time window the plan reads.
    pub const fn window(&self) -> TimeWindow {
        self.window
    }

    /// The query class.
    pub const fn class(&self) -> QueryClass {
        self.class
    }

    /// The root node.
    pub const fn root(&self) -> &Node {
        &self.root
    }

    /// The ClickHouse identity that runs the plan.
    pub const fn identity(&self) -> ServiceIdentity {
        self.class.limits().identity
    }

    /// The ordered output columns that every row of this plan follows, whichever store runs it. This is the
    /// single definition of the result shape: the ClickHouse adapter and the simulation model must both return
    /// rows that pass [`OutputSchema::check_row`].
    ///
    /// | Root | Output columns |
    /// |---|---|
    /// | `Scan`, `PointLookup` | the node's columns, in the order given |
    /// | `Aggregate` | the group dimensions, then one column per measure, in the order given |
    /// | `SemiJoin` | the columns of the left scan |
    /// | `TextSearch` | the table's key columns (`TableDef::key_columns`) |
    /// | `TimeSeries` | `bucket` (the bucket's first instant, `DateTime('UTC')`), then the aggregate's output |
    /// | `FieldRange` | the field; the bytes of the range follow the `String` rule of [`crate::Value`] |
    ///
    /// Measures: `count` and `uniq` are `UInt64`; `sum` is `UInt64`, `Int64` or `Float64` by the column's
    /// signedness; `min`, `max`, the pick merge and latest-by-version have the column's result type; a histogram
    /// merge is the `(keys, counts)` tuple with keys ascending and unique.
    ///
    /// Rows: a node with an order returns rows in that order, and rows that tie on it in no defined order; a
    /// node without one returns rows in no defined order. A grouped read returns one row per group that has at
    /// least one matching row. An `Aggregate` with no group dimensions is one group: it returns one row when at
    /// least one row matches and **no row** otherwise, never a row of defaults. The one exception is a
    /// `TimeSeries` with `Fill::Zero`, which also returns rows for its empty buckets.
    pub fn output(&self) -> OutputSchema {
        value::output(&self.root)
    }
}

/// How a block's presence is probed (contract section 5.5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProbeKind {
    /// Plain table: `count()` of the batch's rows by `wal_epoch` and `batch_seq`.
    BatchSeq,
    /// Aggregating table: primary-key lookup of the block's marker row.
    Marker,
}

/// The existence probe of the insert protocol: the one read rendered without a project predicate. It returns
/// only a boolean, runs as `crucible_ingest` through `TelemetryStore::probe`, and is not a [`Node`], so no
/// scoped plan can contain it.
///
/// Only `crucible-shipper` may build one. [`BatchProbe::for_shipper`] is a disallowed method in `clippy.toml`,
/// like `ProjectScope::mint`, and the CI allow-attribute guard permits the allow in exactly one file,
/// `crates/crucible-shipper/src/probe.rs`. Tests of stores obtain probes from `crucible_plan::test_support`
/// (feature `test-support`, which product builds never enable).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct BatchProbe {
    table: TableId,
    partition: PartitionId,
    epoch: WalEpoch,
    batch_seq: Lsn,
}

impl BatchProbe {
    /// A probe for the block of one (batch, table, partition) pair. `sql_probe` is never shipped and
    /// `dedup_selftest` is probed by its own self-test.
    pub fn for_shipper(
        table_id: TableId,
        partition: PartitionId,
        epoch: WalEpoch,
        batch_seq: Lsn,
    ) -> Result<Self, PlanError> {
        Self::checked(table_id, partition, epoch, batch_seq)
    }

    /// The checked constructor behind [`BatchProbe::for_shipper`] and `test_support::batch_probe`.
    pub(crate) fn checked(
        table_id: TableId,
        partition: PartitionId,
        epoch: WalEpoch,
        batch_seq: Lsn,
    ) -> Result<Self, PlanError> {
        if matches!(table_id, TableId::SqlProbe | TableId::DedupSelftest) {
            return Err(PlanError::Table(table_id));
        }
        check_partition(table_id, partition)?;
        Ok(Self {
            table: table_id,
            partition,
            epoch,
            batch_seq,
        })
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The partition.
    pub const fn partition(&self) -> PartitionId {
        self.partition
    }

    /// The WAL epoch of the batch.
    pub const fn epoch(&self) -> WalEpoch {
        self.epoch
    }

    /// The batch sequence number (the batch's first LSN).
    pub const fn batch_seq(&self) -> Lsn {
        self.batch_seq
    }

    /// Which probe the table kind calls for.
    pub fn kind(&self) -> ProbeKind {
        match table(self.table).kind {
            TableKind::Plain => ProbeKind::BatchSeq,
            TableKind::Aggregating => ProbeKind::Marker,
        }
    }
}

/// The partition id must be of the kind the table's partition expression produces.
fn check_partition(table_id: TableId, partition: PartitionId) -> Result<(), PlanError> {
    let ok = match table(table_id).partition {
        PartitionExpr::None => partition.is_unpartitioned(),
        PartitionExpr::Day(_) | PartitionExpr::DateColumn(_) => partition.as_date().is_some(),
        PartitionExpr::Month(_) => partition.as_month().is_some(),
    };
    if ok {
        Ok(())
    } else {
        Err(PlanError::Partition(table_id))
    }
}

/// Whose rows a delete removes. Every delete carries one of the two predicates; there is no unscoped form.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum DeleteTarget {
    /// Rows of one project: `project_id = {scope}`.
    Project(ProjectScope),
    /// Marker rows (`project_id` = nil) whose `marker_at` is before the instant.
    MarkerPurge {
        /// Markers older than this are purged.
        older_than: Timestamp,
    },
}

/// A lightweight `DELETE ... IN PARTITION` with a typed predicate, used by the lifecycle executors.
#[derive(Clone, PartialEq, Debug)]
pub struct DeletePlan {
    target: DeleteTarget,
    table: TableId,
    partition: PartitionId,
    pred: Pred,
}

impl DeletePlan {
    /// Deletes the rows of the scope's project in one partition that match `pred`.
    pub fn project(
        scope: ProjectScope,
        table_id: TableId,
        partition: PartitionId,
        pred: Pred,
    ) -> Result<Self, PlanError> {
        if !table(table_id).is_scoped() {
            return Err(PlanError::Table(table_id));
        }
        Self::build(DeleteTarget::Project(scope), table_id, partition, pred)
    }

    /// Purges the marker rows of an aggregating table's partition that are older than `older_than` and match
    /// `pred` (which excludes the markers of batches named by an open quarantine).
    pub fn marker_purge(
        table_id: TableId,
        partition: PartitionId,
        older_than: Timestamp,
        pred: Pred,
    ) -> Result<Self, PlanError> {
        if table(table_id).kind != TableKind::Aggregating {
            return Err(PlanError::Table(table_id));
        }
        Self::build(
            DeleteTarget::MarkerPurge { older_than },
            table_id,
            partition,
            pred,
        )
    }

    fn build(
        target: DeleteTarget,
        table_id: TableId,
        partition: PartitionId,
        pred: Pred,
    ) -> Result<Self, PlanError> {
        check_partition(table_id, partition)?;
        pred.check(table_id)?;
        Ok(Self {
            target,
            table: table_id,
            partition,
            pred,
        })
    }

    /// Whose rows are deleted.
    pub const fn target(&self) -> DeleteTarget {
        self.target
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The partition.
    pub const fn partition(&self) -> PartitionId {
        self.partition
    }

    /// The row filter, in addition to the target's predicate.
    pub const fn pred(&self) -> &Pred {
        &self.pred
    }
}
