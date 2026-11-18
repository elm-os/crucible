//! The bookkeeping that decides what may leave the index and what a read must admit: the checkpoint watermark,
//! the visibility grace, open gaps and quarantined pairs (contract 8.6 and invariant I3).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Duration;

use crucible_env::Instant;
use crucible_records::{Lsn, PartitionId, ProjectId, TableId};

/// Whose rows a quarantined pair holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum QuarantineReach {
    /// Rows of these projects only.
    Projects(Vec<ProjectId>),
    /// Unknown: every project is treated as affected.
    All,
}

/// One (batch, table, partition) pair that the shipper quarantined (contract 5.6). Its rows count as resolved
/// for the checkpoint, so its spans leave the index while absent from the store, until the pair is requeued.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct QuarantinedPair {
    /// The batch (its first LSN).
    pub batch_seq: Lsn,
    /// The table.
    pub table: TableId,
    /// The partition.
    pub partition: PartitionId,
    /// The projects with rows in the pair.
    pub reach: QuarantineReach,
}

/// What a read must know besides the entries it found. It is always taken after the entries are read: the
/// writer records a gap before it drops an entry, so a reader that misses an entry sees its gap.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ReadState {
    /// The checkpoint watermark: every entry at or below it is confirmed present in the store or quarantined.
    pub watermark: Lsn,
    /// Whether the project has an open gap: entries dropped under memory pressure that the store may not serve
    /// yet. A read of the project must answer `partial: true` with `store_unavailable`.
    pub gap: bool,
    quarantined: u32,
}

impl ReadState {
    /// Whether the project has rows in a quarantined pair of one of these tables. A read that uses the table
    /// must answer `partial: true` with `quarantined`.
    pub fn quarantined(&self, tables: &[TableId]) -> bool {
        tables
            .iter()
            .any(|&table| self.quarantined & bit(table) != 0)
    }
}

const fn bit(table: TableId) -> u32 {
    1 << table.id()
}

#[derive(Debug, Default)]
pub(crate) struct Control {
    watermark: Lsn,
    /// Checkpoints still inside the visibility grace, oldest first, with the instant each was applied.
    pending: VecDeque<(Lsn, Instant)>,
    /// The highest checkpoint that is older than the grace: entries at or below it may be evicted.
    evictable: Lsn,
    /// Per project, the highest LSN dropped under memory pressure. The gap is open while that LSN is above
    /// `evictable`.
    gaps: BTreeMap<ProjectId, Lsn>,
    quarantined: Vec<QuarantinedPair>,
}

impl Control {
    /// Records a checkpoint applied at `now`.
    pub(crate) fn checkpoint(&mut self, last_lsn: Lsn, now: Instant) {
        if last_lsn > self.watermark {
            self.watermark = last_lsn;
            self.pending.push_back((last_lsn, now));
        }
    }

    /// Moves the checkpoints that are now older than the grace out of the waiting list, closes the gaps they
    /// cover, and returns the LSN through which entries may be evicted.
    pub(crate) fn advance(&mut self, now: Instant, grace: Duration) -> Lsn {
        while let Some(&(lsn, at)) = self.pending.front() {
            if now.saturating_duration_since(at) < grace {
                break;
            }
            self.evictable = lsn;
            self.pending.pop_front();
        }
        let evictable = self.evictable;
        self.gaps.retain(|_, upper| *upper > evictable);
        evictable
    }

    /// Opens or widens the gap of every project that is about to lose entries at or below `lsn`.
    pub(crate) fn open_gap(&mut self, projects: &BTreeSet<ProjectId>, lsn: Lsn) {
        if lsn <= self.evictable {
            return;
        }
        for project in projects {
            let upper = self.gaps.entry(*project).or_insert(lsn);
            *upper = (*upper).max(lsn);
        }
    }

    pub(crate) fn quarantine(&mut self, pair: QuarantinedPair) {
        self.requeue(pair.batch_seq, pair.table, pair.partition);
        self.quarantined.push(pair);
    }

    pub(crate) fn requeue(&mut self, batch_seq: Lsn, table: TableId, partition: PartitionId) {
        self.quarantined
            .retain(|p| (p.batch_seq, p.table, p.partition) != (batch_seq, table, partition));
    }

    pub(crate) fn state(&self, project: ProjectId) -> ReadState {
        let reaches = |pair: &&QuarantinedPair| match &pair.reach {
            QuarantineReach::All => true,
            QuarantineReach::Projects(projects) => projects.contains(&project),
        };
        ReadState {
            watermark: self.watermark,
            gap: self.gaps.contains_key(&project),
            quarantined: self
                .quarantined
                .iter()
                .filter(reaches)
                .fold(0, |tables, pair| tables | bit(pair.table)),
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::CivilDate;

    use super::*;

    const GRACE: Duration = Duration::from_secs(2);

    fn project(n: u8) -> ProjectId {
        ProjectId::from_bytes([n; 16])
    }

    fn at(ms: u64) -> Instant {
        Instant::from_nanos(ms * 1_000_000)
    }

    #[test]
    fn a_checkpoint_frees_entries_only_after_the_grace() {
        let mut control = Control::default();
        control.checkpoint(Lsn::new(10), at(0));
        control.checkpoint(Lsn::new(20), at(1_500));
        // A checkpoint that does not move the watermark is not a new one.
        control.checkpoint(Lsn::new(15), at(1_600));
        assert_eq!(control.advance(at(1_999), GRACE), Lsn::ZERO);
        assert_eq!(control.advance(at(2_000), GRACE), Lsn::new(10));
        assert_eq!(control.advance(at(3_499), GRACE), Lsn::new(10));
        assert_eq!(control.advance(at(3_500), GRACE), Lsn::new(20));
        assert_eq!(control.state(project(1)).watermark, Lsn::new(20));
    }

    #[test]
    fn a_gap_is_per_project_and_closes_when_the_grace_has_covered_it() {
        let mut control = Control::default();
        let dropped = BTreeSet::from([project(1)]);
        control.open_gap(&dropped, Lsn::new(7));
        control.open_gap(&dropped, Lsn::new(5));
        assert!(control.state(project(1)).gap);
        assert!(!control.state(project(2)).gap);
        // The watermark passing the gap is not enough: the grace must elapse too.
        control.checkpoint(Lsn::new(6), at(0));
        control.checkpoint(Lsn::new(7), at(100));
        control.advance(at(2_000), GRACE);
        assert!(control.state(project(1)).gap);
        control.advance(at(2_100), GRACE);
        assert!(!control.state(project(1)).gap);
        // Entries the store already serves leave no gap.
        control.open_gap(&dropped, Lsn::new(7));
        assert!(!control.state(project(1)).gap);
    }

    #[test]
    fn a_quarantined_pair_marks_its_projects_and_table_until_requeued() {
        let mut control = Control::default();
        let day = PartitionId::date(CivilDate::new(2026, 11, 17).unwrap());
        let pair = |table, reach| QuarantinedPair {
            batch_seq: Lsn::new(40),
            table,
            partition: day,
            reach,
        };
        let only = QuarantineReach::Projects(vec![project(1)]);
        control.quarantine(pair(TableId::SpanCore, only.clone()));
        control.quarantine(pair(TableId::SpanCore, only));
        control.quarantine(pair(TableId::TraceStats, QuarantineReach::All));
        let (one, two) = (control.state(project(1)), control.state(project(2)));
        assert!(one.quarantined(&[TableId::SpanCore]) && one.quarantined(&[TableId::TraceStats]));
        assert!(!two.quarantined(&[TableId::SpanCore, TableId::TraceHeads]));
        assert!(two.quarantined(&[TableId::TraceHeads, TableId::TraceStats]));
        // Checkpoints never clear a quarantine; only a requeue does.
        control.checkpoint(Lsn::new(99), at(0));
        control.advance(at(60_000), GRACE);
        assert!(control.state(project(1)).quarantined(&[TableId::SpanCore]));
        control.requeue(Lsn::new(40), TableId::SpanCore, day);
        assert!(!control.state(project(1)).quarantined(&[TableId::SpanCore]));
        assert!(
            control
                .state(project(1))
                .quarantined(&[TableId::TraceStats])
        );
    }
}
