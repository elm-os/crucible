//! The ingestion group (contract 6.1): where and when the WAL accepted the data behind a row.

use crate::id::WalEpoch;
use crate::lsn::Lsn;
use crate::time::Timestamp;

/// The ingestion columns of every plain-table row, in column order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ingestion {
    /// Epoch of the WAL that accepted the frame.
    pub wal_epoch: WalEpoch,
    /// The batch, which is its first LSN (contract 5.1).
    pub batch_seq: Lsn,
    /// LSN of the frame that carried the data.
    pub lsn: Lsn,
    /// When the committer assigned the LSN (the frame header's `received_at`).
    pub received_at: Timestamp,
}
