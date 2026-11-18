//! The shipper's [`RecentIndex`] hook over the writer of `crucible-recent` (contract 8.6): stage B is the one
//! writer of the recent-span index.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crucible_env::Clock;
use crucible_normalize::{Admission, NormalizedFrame};
use crucible_recent::{
    Frame, QuarantineReach, QuarantinedPair, RecentWriter, SpanEntry, SpanSummary, WalLocator,
};
use crucible_records::{HeadKind, Lsn, PartitionId, ProjectId, TableId};
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
/// - a frame that does not normalize and is quarantined as a frame marks every table its rows would have
///   reached, for its project when the envelope names one;
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

    /// Marks frames that were quarantined as frames before this incarnation and that the retained log no
    /// longer holds: `(lsn, project, facts)` as [`crate::FrameManifest`] reads them from the quarantine root.
    /// Frames the log still holds are marked again when stage B replays them.
    pub fn restore_unshippable(
        &self,
        frames: impl IntoIterator<Item = (Lsn, Option<ProjectId>, bool)>,
    ) {
        let pairs = frames
            .into_iter()
            .flat_map(|(lsn, project, facts)| frame_marks(lsn, project, facts))
            .collect();
        self.writer().apply(Frame::Quarantine {
            lsn: Lsn::ZERO,
            pairs,
        });
    }
}

/// The tables a frame's rows reach: what a quarantined frame withholds from the store.
const SPAN_TABLES: [TableId; 7] = [
    TableId::SpanCore,
    TableId::SpanPayload,
    TableId::TraceHeads,
    TableId::TraceStats,
    TableId::SessionRollup,
    TableId::UserRollup,
    TableId::SpanMetrics,
];
const FACT_TABLES: [TableId; 4] = [
    TableId::FactScores,
    TableId::FactTags,
    TableId::FactFlags,
    TableId::FactCorrections,
];

/// The marks of a frame quarantined as a frame: one per table its rows would have reached. The frame's own LSN
/// stands for the batch and the partition is left open, because the frame never joined a pair.
fn frame_marks(lsn: Lsn, project: Option<ProjectId>, facts: bool) -> Vec<QuarantinedPair> {
    let tables: &[TableId] = if facts { &FACT_TABLES } else { &SPAN_TABLES };
    tables
        .iter()
        .map(|&table| QuarantinedPair {
            batch_seq: lsn,
            table,
            partition: PartitionId::UNPARTITIONED,
            reach: project.map_or(QuarantineReach::All, |p| QuarantineReach::Projects(vec![p])),
        })
        .collect()
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
                    // The span's position in stage A's output, which is where a re-read of the frame through
                    // the normalizer finds it, whatever stage B dropped before it.
                    ordinal: admission
                        .stage_a_ordinals
                        .get(position)
                        .copied()
                        .unwrap_or(u32::MAX),
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

    fn unshippable(&self, source: &WalFrame, project: Option<ProjectId>, facts: bool) {
        let lsn = source.lsn();
        self.writer().apply(Frame::Quarantine {
            lsn,
            pairs: frame_marks(lsn, project, facts),
        });
    }

    fn floor(&self) -> Option<Lsn> {
        let mut writer = self.writer();
        // Garbage collection asks on every pass, which is what evicts on a node without traffic.
        writer.maintain();
        writer.index().floor()
    }
}
