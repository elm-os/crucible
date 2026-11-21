//! The shipper task's time by phase: `crucible_shipper_task_microseconds_total{phase}`. The task times itself
//! with the `Env` clock; the recorder never reads a clock (contract 2.12).

use crucible_env::metrics::registry;
use crucible_env::{Counter, Instant, Metrics};

/// A phase of the shipper task, the `phase` label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    /// Reading the next durable frame from the WAL.
    Read,
    /// Decoding and normalizing a frame (stage A, contract 3.2).
    StageA,
    /// The duplicate window, the first-seen set, the deletions and the recent-index insert (stage B).
    StageB,
    /// Deriving a frame's table rows and adding them to the builders.
    Rows,
    /// Sorting, combining, encoding and compressing the blocks of a sealed batch.
    Encode,
    /// Making the shipper's own frames (`BatchSeal`, `Checkpoint`) durable, and garbage collection.
    WalAppend,
    /// Waiting while a full batch, or a frame held for the `shipper.encoded_bytes` budget, waits for the
    /// batches in flight.
    WaitInserts,
    /// The rest: caught up with the durable log, or waiting for the control store, a retry or a frame
    /// quarantine copy.
    WaitFrames,
}

/// The label of each phase, in the order of [`Phase`].
const LABELS: [&str; 8] = [
    "read",
    "stage_a",
    "stage_b",
    "rows",
    "encode",
    "wal_append",
    "wait_inserts",
    "wait_frames",
];

/// Charges the task's time to phases. Each lap charges the time since the previous lap to one phase, so the
/// phases add up to the time since [`Laps::start`]. The counters take whole microseconds; the nanoseconds below
/// one carry to the phase's next lap, so no time is lost to rounding.
#[derive(Debug)]
pub(crate) struct Laps {
    mark: Instant,
    counters: [Counter; 8],
    carry_nanos: [u64; 8],
}

impl Laps {
    pub(crate) fn new(metrics: &dyn Metrics, now: Instant) -> Self {
        Self {
            mark: now,
            counters: LABELS.map(|label| metrics.counter(&registry::SHIPPER_TASK_TIME, &[label])),
            carry_nanos: [0; 8],
        }
    }

    /// Starts the count at `now`: the time before it is charged to no phase.
    pub(crate) fn start(&mut self, now: Instant) {
        self.mark = now;
    }

    /// Charges the time since the previous lap to `phase`.
    pub(crate) fn lap(&mut self, phase: Phase, now: Instant) {
        let nanos = now.nanos().saturating_sub(self.mark.nanos());
        self.mark = now;
        let index = phase as usize;
        if let (Some(counter), Some(carry)) =
            (self.counters.get(index), self.carry_nanos.get_mut(index))
        {
            let total = carry.saturating_add(nanos);
            counter.add(total / 1_000);
            *carry = total % 1_000;
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::MemoryRecorder;
    use proptest::prelude::*;

    use super::*;

    const PHASES: [Phase; 8] = [
        Phase::Read,
        Phase::StageA,
        Phase::StageB,
        Phase::Rows,
        Phase::Encode,
        Phase::WalAppend,
        Phase::WaitInserts,
        Phase::WaitFrames,
    ];

    fn charged(recorder: &MemoryRecorder) -> [u64; 8] {
        LABELS.map(|label| recorder.counter_value(&registry::SHIPPER_TASK_TIME, &[label]))
    }

    proptest! {
        /// Each phase holds exactly the whole microseconds of the time charged to it, whatever the laps' lengths,
        /// so the phases add up to the time since the start, less under one microsecond per phase.
        #[test]
        fn each_phase_holds_the_whole_microseconds_charged_to_it(
            start in 0u64..1 << 40,
            laps in proptest::collection::vec((0usize..8, 0u64..5_000_000), 0..200),
        ) {
            let recorder = MemoryRecorder::new();
            let mut timer = Laps::new(&recorder, Instant::from_nanos(0));
            timer.start(Instant::from_nanos(start));
            let mut now = start;
            let mut expected = [0u64; 8];
            for (phase, nanos) in laps {
                now += nanos;
                timer.lap(PHASES[phase], Instant::from_nanos(now));
                expected[phase] += nanos;
            }
            let held = charged(&recorder);
            prop_assert_eq!(held, expected.map(|nanos| nanos / 1_000));
            let total: u64 = held.iter().sum();
            prop_assert!(total * 1_000 <= now - start);
            prop_assert!(now - start - total * 1_000 < 8 * 1_000);
        }
    }

    #[test]
    fn the_time_before_the_start_is_charged_to_no_phase() {
        let recorder = MemoryRecorder::new();
        let mut timer = Laps::new(&recorder, Instant::from_nanos(0));
        timer.start(Instant::from_nanos(5_000_000));
        timer.lap(Phase::Encode, Instant::from_nanos(7_500_400));
        timer.lap(Phase::Encode, Instant::from_nanos(7_500_900));
        let mut expected = [0; 8];
        expected[Phase::Encode as usize] = 2_500;
        assert_eq!(charged(&recorder), expected);
    }
}
