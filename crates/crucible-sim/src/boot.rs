//! Recovering the WAL of a simulated host through crashes and faults, shared by the simulation suites.
//!
//! One uninterrupted recovery on a fault-free copy of the crashed disk gives the reference state. The real
//! recovery is then attempted on the faulty disk, cut by seeded crashes and process stops, until it succeeds;
//! after three failures the faults are healed, so a failure from then on is a defect.

use futures::FutureExt;

use crucible_env::Rng;
use crucible_wal::{RecoveryError, RecoveryReport, Wal};

use crate::disk::{DiskFaults, SimDisk};
use crate::runtime::{Host, Sim};
use crate::wal_sim::{LogState, Plan, coin, pick, read_log};

/// What a recovery works on.
pub(crate) struct Recovery<'a> {
    pub(crate) sim: &'a Sim,
    pub(crate) disk: &'a SimDisk,
    pub(crate) plan: &'a Plan,
    /// The driver's stream: the crash points of the recovery are drawn from it.
    pub(crate) rng: &'a dyn Rng,
    /// The number of this boot, which labels its hosts.
    pub(crate) boots: u64,
}

/// Counters of one boot.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BootCounts {
    pub(crate) attempts: u64,
    pub(crate) retries: u64,
    pub(crate) stops: u64,
    pub(crate) crashes: u64,
}

/// A recovered host. The disk is left with whatever faults the last attempt ran under.
pub(crate) struct Booted {
    pub(crate) host: Host,
    pub(crate) wal: Wal,
    pub(crate) report: RecoveryReport,
    /// The state one uninterrupted recovery reaches.
    pub(crate) reference: LogState,
    /// Whether that uninterrupted recovery had a tail to repair.
    pub(crate) repaired: bool,
}

fn event(sim: &Sim, label: &str, fields: &[u64]) {
    let micros = sim.elapsed().as_micros() as u64;
    sim.trace().event("at", &[micros]);
    sim.trace().event(label, fields);
}

/// What one uninterrupted recovery of the disk as it is now yields, computed on a fault-free copy: the
/// state, whether a tail was repaired, and how many disk operations the recovery took.
async fn reference(r: &Recovery<'_>) -> Result<(LogState, bool, u64), RecoveryError> {
    let label = format!("wal-sim/reference-{}-{}", r.boots, r.disk.ops());
    let copy = r.disk.fork(r.sim.rng(&label).as_ref());
    let host = r.sim.host(&label, &copy, r.plan.wall_offset_micros);
    let ops_before = copy.ops();
    let opened = Box::pin(Wal::open(&host.env, r.plan.config.clone())).await;
    let ops = copy.ops() - ops_before;
    let state = match opened {
        Ok((wal, report)) => {
            let log = read_log(&wal, report.first_retained_lsn())
                .await
                .unwrap_or_default();
            let repaired = report.tail_repair.is_some();
            Ok((LogState::new(&report, &log), repaired, ops))
        }
        Err(e) => Err(e),
    };
    host.stop().await;
    state
}

/// Recovers the WAL on a new host, through crashes and faults. `stopped` says whether the last incarnation
/// ended with a process stop, so that the disk holds bytes that are readable and not durable. An error is a
/// recovery that failed on a fault-free copy or on the healed disk.
pub(crate) async fn recover(
    r: &Recovery<'_>,
    stopped: &mut bool,
    counts: &mut BootCounts,
) -> Result<Booted, RecoveryError> {
    let mut expected = None;
    let mut attempt = 0u64;
    let (host, wal, report) = loop {
        // After a stop, a crash during recovery loses what was readable and not durable, so the
        // reference is taken again after that crash. From then on, and after a crash from the start,
        // the disk holds durable bytes only and every later attempt must reach the one reference.
        if expected.is_none() {
            expected = Some(reference(r).await?);
        }
        let recovery_ops = expected.as_ref().map_or(1, |e| e.2);
        attempt += 1;
        // After three failures the faults are healed: a recovery must then succeed.
        let healed = attempt > 3;
        if healed {
            r.disk.set_faults(DiskFaults::default());
        }
        let name = format!("wal-sim/boot-{}/attempt-{attempt}", r.boots);
        let host = r.sim.host(&name, r.disk, r.plan.wall_offset_micros);
        if r.plan.crash_in_recovery && !healed && coin(r.rng) {
            // Anywhere in the recovery, its last operation included.
            r.disk.arm_crash_after_ops(1 + pick(r.rng, recovery_ops));
        }
        counts.attempts += 1;
        let opened = {
            let mut open = Box::pin(Wal::open(&host.env, r.plan.config.clone())).fuse();
            let mut crash_point = Box::pin(r.disk.crash_point_reached()).fuse();
            futures::select_biased! {
                opened = open => Some(opened),
                () = crash_point => None,
            }
        };
        r.disk.disarm_crash();
        match opened {
            Some(Ok((wal, report))) => break (host, wal, report),
            Some(Err(e)) if healed => {
                host.stop().await;
                return Err(e);
            }
            failed => {
                event(
                    r.sim,
                    "recovery.retry",
                    &[attempt, u64::from(failed.is_some())],
                );
                counts.retries += 1;
                let errored = failed.is_some();
                drop(failed);
                if errored && r.plan.clean_stops && coin(r.rng) {
                    // The process gives up and is started again; the machine stays up, so what
                    // this attempt wrote is readable and not durable.
                    counts.stops += 1;
                    host.stop().await;
                    *stopped = true;
                    expected = None;
                } else {
                    counts.crashes += 1;
                    host.crash().await;
                    if std::mem::take(stopped) {
                        expected = None;
                    }
                }
            }
        }
    };
    *stopped = false;
    match expected {
        Some((reference, repaired, _)) => Ok(Booted {
            host,
            wal,
            report,
            reference,
            repaired,
        }),
        // Unreachable: the loop takes a reference before every attempt.
        None => {
            drop(wal);
            host.stop().await;
            Err(RecoveryError::InvalidConfig("no reference recovery"))
        }
    }
}
