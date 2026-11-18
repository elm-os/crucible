//! Probes for tests of stores in other crates. Enabled by the `test-support` feature, which product builds never
//! enable: in product code only `crucible-shipper` builds a [`BatchProbe`], through `BatchProbe::for_shipper`.

use crucible_records::{Lsn, PartitionId, TableId, WalEpoch};

use crate::error::PlanError;
use crate::plan::BatchProbe;

/// A probe for the block of one (batch, table, partition) pair, for tests only.
pub fn batch_probe(
    table_id: TableId,
    partition: PartitionId,
    epoch: WalEpoch,
    batch_seq: Lsn,
) -> Result<BatchProbe, PlanError> {
    BatchProbe::checked(table_id, partition, epoch, batch_seq)
}
