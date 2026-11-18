//! The open batch: per-(table, partition) row builders over an adjacent LSN range, the seal thresholds, and the
//! blocks of a sealed batch (contract 5.1 and 5.2).

use std::collections::BTreeMap;

use crucible_chfmt::{Compression, EncodedBlock, MAX_BLOCK_ROWS};
use crucible_env::Instant;
use crucible_records::{BatchToken, Lsn, PartitionId, TableId, Timestamp, WalEpoch};
use crucible_schema::{TableDef, TableKind, table};

use crate::limits::SealLimits;
use crate::tables::RoutedRow;
use crate::value::{BuildError, Row, encode_block, marker_row, sort_and_combine};

/// A (table, partition) pair.
pub type Pair = (TableId, PartitionId);

/// Rows one block of a table may take from its builder: the block limit, less the marker row of an aggregating
/// table.
fn builder_capacity(def: &TableDef) -> usize {
    MAX_BLOCK_ROWS - usize::from(def.kind == TableKind::Aggregating)
}

/// The batch being filled: the frames from `first_lsn` on that are not sealed yet.
#[derive(Debug)]
pub struct OpenBatch {
    first_lsn: Lsn,
    /// The last frame taken; `first_lsn - 1` while the batch has none.
    last_lsn: Lsn,
    /// `received_at` of the last frame taken.
    last_received_at: Timestamp,
    /// When the first frame that warrants a seal was taken.
    sealable_since: Option<Instant>,
    span_rows: usize,
    raw_bytes: u64,
    pairs: BTreeMap<Pair, Vec<Row>>,
}

impl OpenBatch {
    /// An empty batch whose range starts at `first_lsn`.
    pub fn new(first_lsn: Lsn) -> Self {
        Self {
            first_lsn,
            last_lsn: Lsn::new(first_lsn.get().saturating_sub(1)),
            last_received_at: Timestamp::EPOCH,
            sealable_since: None,
            span_rows: 0,
            raw_bytes: 0,
            pairs: BTreeMap::new(),
        }
    }

    /// The first LSN of the range, which is the batch sequence number.
    pub const fn first_lsn(&self) -> Lsn {
        self.first_lsn
    }

    /// The last LSN taken.
    pub const fn last_lsn(&self) -> Lsn {
        self.last_lsn
    }

    /// Whether the batch holds a frame that is not one of the shipper's own control frames. Only such a batch
    /// is sealed: a seal for control frames alone would be answered by another seal for ever.
    pub const fn is_sealable(&self) -> bool {
        self.sealable_since.is_some()
    }

    /// `received_at` of the last frame, which is the marker time of the batch's blocks (contract 5.3).
    pub const fn marker_at(&self) -> Timestamp {
        self.last_received_at
    }

    /// Whether every row fits its builder: no (table, partition) block would pass the block limit.
    pub fn fits(&self, rows: &[RoutedRow]) -> bool {
        let mut added: BTreeMap<Pair, usize> = BTreeMap::new();
        for row in rows {
            *added.entry((row.table, row.partition)).or_default() += 1;
        }
        added.iter().all(|(pair, more)| {
            let held = self.pairs.get(pair).map_or(0, Vec::len);
            held + more <= builder_capacity(table(pair.0))
        })
    }

    /// Takes the next frame of the LSN stream into the range. `sealable` is false for the shipper's own control
    /// frames.
    pub fn take_frame(
        &mut self,
        lsn: Lsn,
        received_at: Timestamp,
        raw_bytes: u64,
        sealable: bool,
        now: Instant,
    ) {
        self.last_lsn = lsn;
        self.last_received_at = received_at;
        self.raw_bytes = self.raw_bytes.saturating_add(raw_bytes);
        if sealable && self.sealable_since.is_none() {
            self.sealable_since = Some(now);
        }
    }

    /// Adds the rows of the frame taken last. `spans` is the frame's span count after stage B.
    pub fn add_rows(&mut self, rows: Vec<RoutedRow>, spans: usize) {
        self.span_rows += spans;
        for routed in rows {
            self.pairs
                .entry((routed.table, routed.partition))
                .or_default()
                .push(routed.row);
        }
    }

    /// Whether a size threshold is reached: the batch takes no further frame.
    pub fn is_full(&self, limits: &SealLimits, seal_bytes: u64) -> bool {
        self.span_rows >= limits.rows() || self.raw_bytes >= seal_bytes
    }

    /// Whether the batch must seal now (contract 5.1): it holds something to seal and has reached the linger,
    /// the row limit or the byte limit.
    pub fn is_due(&self, limits: &SealLimits, seal_bytes: u64, now: Instant) -> bool {
        self.sealable_since.is_some_and(|since| {
            self.is_full(limits, seal_bytes)
                || now.saturating_duration_since(since) >= limits.linger()
        })
    }

    /// The row builders, for the blocks of the sealed batch.
    pub fn into_pairs(self) -> BTreeMap<Pair, Vec<Row>> {
        self.pairs
    }
}

/// The identity of a sealed batch (contract 5.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BatchId {
    /// Epoch of the WAL.
    pub epoch: WalEpoch,
    /// First LSN of the range; the batch sequence number.
    pub first_lsn: Lsn,
    /// Last LSN of the range.
    pub last_lsn: Lsn,
}

/// The one block of a (batch, table, partition) pair (contract 5.2): the rows sorted by the table's ORDER BY key,
/// rollup rows combined, and on an aggregating table the marker row.
pub fn pair_block(
    batch: &BatchId,
    pair: Pair,
    mut rows: Vec<Row>,
    marker_at: Timestamp,
    compression: Compression,
) -> Result<EncodedBlock, BuildError> {
    let def = table(pair.0);
    if def.kind == TableKind::Aggregating {
        rows.push(marker_row(
            def,
            pair.1,
            batch.epoch,
            batch.first_lsn,
            marker_at,
        ));
    }
    sort_and_combine(def, &mut rows)?;
    encode_block(def, &rows, compression)
}

/// The token of a pair (contract 5.2).
pub fn pair_token(batch: &BatchId, pair: Pair) -> Result<BatchToken, crucible_records::TokenError> {
    BatchToken::new(batch.epoch, batch.first_lsn, batch.last_lsn, pair.0, pair.1)
}
