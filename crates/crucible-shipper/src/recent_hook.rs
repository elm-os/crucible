//! The shipper's [`RecentIndex`] hook over the writer of `crucible-recent` (contract 8.6): stage B is the one
//! writer of the recent-span index.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crucible_env::Clock;
use crucible_normalize::{Admission, NormalizedFrame};
use crucible_recent::{
    Frame, QuarantineReach, QuarantinedPair, RecentWriter, SpanEntry, SpanSummary, WalLocator,
};
use crucible_records::{HeadKind, Lsn, PartitionId, TableId};
use crucible_wal::WalFrame;
use crucible_wal_codec::{Record, RecordType};

use crate::hooks::RecentIndex;

/// Feeds the recent-span index from stage B and pins the WAL below the index's oldest entry.
///
/// The host builds the index of an incarnation with `crucible_recent::RecentIndex::rebuild` at the recovered
/// watermark, wraps its writer in this hook and gives the hook to the shipper. Stage B then replays the retained
/// prefix and tails the log through it:
///
/// - an `IngestRequest` frame becomes the index entries of its admitted spans. A frame at or below the recovered
///   watermark that is older than the visibility grace is skipped (contract 8.6, rebuild): the store serves it;
/// - a `Checkpoint` frame moves the index's watermark, which makes entries evictable after the grace and closes
///   gaps;
/// - a `Quarantine` frame marks its pairs. The frame does not name projects, so the mark reaches every project;
/// - every frame moves the applied LSN that the read barrier waits for.
///
/// Milestone 1 writes no `Tombstone` frames; they pass like any other control frame until deletion lands.
#[derive(Debug)]
pub struct RecentHook {
    writer: Mutex<RecentWriter>,
    clock: Arc<dyn Clock>,
    recovered_watermark: Lsn,
}

impl RecentHook {
    /// A hook over the writer of an index that was rebuilt at `recovered_watermark`.
    pub fn new(writer: RecentWriter, clock: Arc<dyn Clock>, recovered_watermark: Lsn) -> Self {
        Self {
            writer: Mutex::new(writer),
            clock,
            recovered_watermark,
        }
    }

    fn writer(&self) -> MutexGuard<'_, RecentWriter> {
        self.writer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Marks pairs that were quarantined before this incarnation and whose `Quarantine` frames the retained log
    /// no longer holds. The host reads them from the manifests under the quarantine root at start, so that a
    /// read which could touch such a pair still answers `partial` (invariant I3).
    pub fn restore_quarantined(
        &self,
        pairs: impl IntoIterator<Item = (Lsn, TableId, PartitionId)>,
    ) {
        let pairs = pairs
            .into_iter()
            .map(|(batch_seq, table, partition)| QuarantinedPair {
                batch_seq,
                table,
                partition,
                reach: QuarantineReach::All,
            })
            .collect();
        self.writer().apply(Frame::Quarantine {
            lsn: Lsn::ZERO,
            pairs,
        });
    }

    /// Runs eviction and gap closure without a frame, for a node that receives no traffic.
    pub fn maintain(&self) {
        self.writer().maintain();
    }
}

fn control(source: &WalFrame) -> Frame {
    let lsn = source.lsn();
    let record_type = source.record_type();
    let wanted = matches!(record_type, RecordType::Checkpoint | RecordType::Quarantine);
    let payload = match source.payload() {
        Ok(payload) if wanted => payload,
        _ => return Frame::Other { lsn },
    };
    match Record::decode(record_type, &payload) {
        Ok(Record::Checkpoint(checkpoint)) => Frame::Checkpoint {
            lsn,
            last_lsn: checkpoint.last_lsn,
        },
        Ok(Record::Quarantine(quarantine)) => Frame::Quarantine {
            lsn,
            pairs: quarantine
                .pairs
                .iter()
                .map(|pair| QuarantinedPair {
                    batch_seq: quarantine.first_lsn,
                    table: pair.table_id,
                    partition: pair.partition_id,
                    reach: QuarantineReach::All,
                })
                .collect(),
        },
        // The shipper wrote these frames and recovery validated them; a frame that does not decode still
        // moves the applied LSN.
        _ => Frame::Other { lsn },
    }
}

impl RecentIndex for RecentHook {
    fn applied(&self, source: &WalFrame, frame: &NormalizedFrame, admission: &Admission) {
        let lsn = source.lsn();
        let mut writer = self.writer();
        let grace = writer.index().config().visibility_grace;
        let grace = i64::try_from(grace.as_micros()).unwrap_or(i64::MAX);
        let served_by_store = lsn <= self.recovered_watermark
            && source.received_at().saturating_add_micros(grace) < self.clock.wall_now();
        if served_by_store {
            writer.apply(Frame::Other { lsn });
            return;
        }
        let spans = frame
            .spans
            .iter()
            .enumerate()
            .map(|(position, span)| SpanEntry {
                project_id: span.core.project_id,
                summary: SpanSummary::of(&span.core),
                head: if span.core.is_root {
                    Some(HeadKind::Root)
                } else if admission.provisional_heads.contains(&position) {
                    Some(HeadKind::Provisional)
                } else {
                    None
                },
                locator: WalLocator {
                    segment: source.segment_no(),
                    offset: source.offset(),
                    // The span's position among the frame's admitted spans.
                    ordinal: u32::try_from(position).unwrap_or(u32::MAX),
                },
            })
            .collect();
        writer.apply(Frame::Spans {
            lsn,
            received_at: source.received_at(),
            spans,
        });
    }

    fn passed(&self, source: &WalFrame) {
        self.writer().apply(control(source));
    }

    fn floor(&self) -> Option<Lsn> {
        let mut writer = self.writer();
        // Garbage collection asks on every pass, which is what evicts on a node without traffic.
        writer.maintain();
        writer.index().floor()
    }
}
