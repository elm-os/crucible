//! The fsync-honesty probe: does a volume's flush reach durable media?
//!
//! Durable-ack latency means something only on a volume whose flushes are real, so `docs/testing/perf.md`
//! lets durable-ack p99 gate a merge only after this probe classified the WAL volume `honest`.
//!
//! **Method.** Everything goes through `Env`'s disk, under the `wal` root, in a fresh directory that is
//! removed afterwards; in production that is the portable backend the WAL itself uses. For each write size
//! the probe preallocates a file and, per iteration, appends the size at the next offset (fresh bytes, as the
//! WAL writes into a preallocated segment) and calls `sync_data`. The bytes are incompressible. Every disk
//! call crosses the backend's writer-thread hop, so each iteration also times an empty `append`, which is the
//! hop alone. The directory phase creates an empty file and calls `sync_dir`. The first
//! [`WARMUP_ITERATIONS`] of every phase are discarded, and at most [`BYTES_PER_SIZE`] bytes are measured per
//! size, because tmpfs bytes are memory.
//!
//! **Classification.** The checks use medians net of the hop median: a CPU quota throttles the tails, not
//! the median.
//!
//! 1. `small_write_flush`: syncing a fresh 4 KiB write costs at least [`FLUSH_FLOOR_NS`] more than the hop.
//! 2. `sync_scales_with_bytes`: syncing 8 MiB costs more than syncing 4 KiB by at least the time
//!    [`MAX_DEVICE_BYTES_PER_S`] needs for 8 MiB (about 0.84 ms), so the sync moved the bytes.
//! 3. `dir_sync`: syncing the directory after a create costs at least [`DIR_SYNC_FLOOR_NS`] more than the hop.
//!
//! The volume is `honest` when all three hold and `suspicious` otherwise.

use bytes::Bytes;
use crucible_env::{ChaChaRng, DiskError, DiskPath, Env, Instant, OpenMode, Rng, Root};
use serde::Serialize;

const KIB: usize = 1_024;
const MIB: usize = 1_024 * KIB;

/// The write sizes measured, smallest first. The largest is the WAL's group cap (contract 4.5).
pub const WRITE_SIZES: [usize; 4] = [4 * KIB, 64 * KIB, MIB, 8 * MIB];
/// The most bytes measured per write size; warm-up iterations come on top.
pub const BYTES_PER_SIZE: usize = 64 * MIB;
/// Iterations discarded at the start of every phase.
pub const WARMUP_ITERATIONS: u32 = 3;
/// The least a 4 KiB sync must cost beyond the hop on an honest volume.
pub const FLUSH_FLOOR_NS: u64 = 100_000;
/// The least a directory sync after a create must cost beyond the hop on an honest volume.
pub const DIR_SYNC_FLOOR_NS: u64 = 100_000;
/// No device the probe expects writes faster than this, so a sync that moved its bytes is no faster.
pub const MAX_DEVICE_BYTES_PER_S: u64 = 10_000_000_000;
/// The `schema` value of [`ProbeReport`].
pub const PROBE_SCHEMA: &str = "crucible-loadgen.fsync-probe.v1";

/// The JSON report of a probe (schema [`PROBE_SCHEMA`], documented in `docs/testing/perf.md`).
#[derive(Clone, Debug, Serialize)]
pub struct ProbeReport {
    /// Always [`PROBE_SCHEMA`].
    pub schema: &'static str,
    /// The probed directory, as the caller names it.
    pub dir: String,
    /// The classification.
    pub verdict: Verdict,
    /// The checks behind the verdict.
    pub checks: Vec<Check>,
    /// The writer-thread hop alone (an empty append), once per measured iteration of every phase.
    pub hop_ns: Distribution,
    /// One entry per write size, smallest first.
    pub writes: Vec<WriteSize>,
    /// The directory phase.
    pub dir_sync: DirSync,
}

/// Whether the volume's flushes look durable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every check passed: the latencies look like real device flushes.
    Honest,
    /// Some check failed: flushes are too fast to be durable.
    Suspicious,
}

/// One classification check.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub struct Check {
    /// `small_write_flush`, `sync_scales_with_bytes` or `dir_sync`.
    pub name: &'static str,
    /// The measured net cost; negative when the operation was faster than its baseline.
    pub value_ns: i64,
    /// The least an honest volume costs.
    pub floor_ns: u64,
    /// `value_ns >= floor_ns`.
    pub pass: bool,
}

/// A latency distribution with its raw samples.
#[derive(Clone, PartialEq, Eq, Debug, Serialize)]
pub struct Distribution {
    /// Samples.
    pub count: usize,
    /// Smallest sample.
    pub min: u64,
    /// 10th percentile (nearest rank).
    pub p10: u64,
    /// Median (nearest rank).
    pub p50: u64,
    /// 90th percentile (nearest rank).
    pub p90: u64,
    /// 99th percentile (nearest rank).
    pub p99: u64,
    /// Largest sample.
    pub max: u64,
    /// Every sample, in measurement order.
    pub samples: Vec<u64>,
}

impl Distribution {
    /// Summarizes `samples`; percentiles use the nearest-rank definition, and are 0 without samples.
    pub fn of(samples: Vec<u64>) -> Self {
        let mut sorted = samples.clone();
        sorted.sort_unstable();
        let at = |percent: usize| match sorted.len() {
            0 => 0,
            n => sorted[(percent * n).div_ceil(100).max(1) - 1],
        };
        Self {
            count: samples.len(),
            min: at(0),
            p10: at(10),
            p50: at(50),
            p90: at(90),
            p99: at(99),
            max: at(100),
            samples,
        }
    }
}

/// The append-then-sync phase of one write size.
#[derive(Clone, Debug, Serialize)]
pub struct WriteSize {
    /// Bytes per write.
    pub bytes: usize,
    /// The append.
    pub write_ns: Distribution,
    /// The `sync_data` after it.
    pub sync_ns: Distribution,
    /// Append plus sync, per iteration.
    pub total_ns: Distribution,
}

/// The create-then-sync phase.
#[derive(Clone, Debug, Serialize)]
pub struct DirSync {
    /// Creating an empty file.
    pub create_ns: Distribution,
    /// The `sync_dir` after it.
    pub sync_ns: Distribution,
}

/// The medians the verdict is computed from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Medians {
    /// The hop alone.
    pub hop_ns: u64,
    /// `sync_data` after a write of the smallest size.
    pub small_sync_ns: u64,
    /// `sync_data` after a write of the largest size.
    pub large_sync_ns: u64,
    /// The largest size.
    pub large_bytes: usize,
    /// `sync_dir` after a create.
    pub dir_sync_ns: u64,
}

/// The verdict and the checks behind it; see the module documentation.
pub fn classify(medians: &Medians) -> (Verdict, Vec<Check>) {
    let net = |value: u64, baseline: u64| {
        let wide = i128::from(value) - i128::from(baseline);
        i64::try_from(wide).unwrap_or(if wide < 0 { i64::MIN } else { i64::MAX })
    };
    let bytes_floor = u128::try_from(medians.large_bytes).unwrap_or(u128::MAX) * 1_000_000_000
        / u128::from(MAX_DEVICE_BYTES_PER_S);
    let checks = vec![
        check(
            "small_write_flush",
            net(medians.small_sync_ns, medians.hop_ns),
            FLUSH_FLOOR_NS,
        ),
        check(
            "sync_scales_with_bytes",
            net(medians.large_sync_ns, medians.small_sync_ns),
            u64::try_from(bytes_floor).unwrap_or(u64::MAX),
        ),
        check(
            "dir_sync",
            net(medians.dir_sync_ns, medians.hop_ns),
            DIR_SYNC_FLOOR_NS,
        ),
    ];
    let verdict = if checks.iter().all(|c| c.pass) {
        Verdict::Honest
    } else {
        Verdict::Suspicious
    };
    (verdict, checks)
}

fn check(name: &'static str, value_ns: i64, floor_ns: u64) -> Check {
    Check {
        name,
        value_ns,
        floor_ns,
        pass: value_ns >= i64::try_from(floor_ns).unwrap_or(i64::MAX),
    }
}

/// Runs the probe in a fresh directory under the `wal` root, with up to `iterations` measured iterations
/// per phase, and removes the directory afterwards. The returned report's `dir` is empty for the caller to
/// fill in.
pub async fn probe(env: &Env, iterations: u32) -> Result<ProbeReport, DiskError> {
    let dir = DiskPath::new(Root::Wal, &format!("fsync-probe-{}", env.new_id()))?;
    env.disk.create_dir_all(&dir).await?;
    let measured = measure(env, &dir, iterations.max(1)).await;
    let removed = remove_dir(env, &dir).await;
    let report = measured?;
    removed?;
    Ok(report)
}

async fn measure(env: &Env, dir: &DiskPath, iterations: u32) -> Result<ProbeReport, DiskError> {
    let hop = env
        .disk
        .open(&dir.join("hop")?, OpenMode::CreateNew)
        .await?;
    let mut data = vec![0; WRITE_SIZES[WRITE_SIZES.len() - 1]];
    ChaChaRng::from_u64(0).fill_bytes(&mut data);
    let data = Bytes::from(data);
    let mut hop_ns = Vec::new();

    let mut writes = Vec::with_capacity(WRITE_SIZES.len());
    for size in WRITE_SIZES {
        let measured = iterations.min(u32::try_from(BYTES_PER_SIZE / size).unwrap_or(u32::MAX));
        let rounds = WARMUP_ITERATIONS + measured;
        let path = dir.join(&format!("write-{size}"))?;
        let file = env.disk.open(&path, OpenMode::CreateNew).await?;
        file.preallocate(u64::from(rounds) * size as u64).await?;
        let (mut write_ns, mut sync_ns) = (Vec::new(), Vec::new());
        for round in 0..rounds {
            let t0 = env.clock.now();
            hop.append(0, Vec::new()).await?;
            let t1 = env.clock.now();
            file.append(u64::from(round) * size as u64, vec![data.slice(..size)])
                .await?;
            let t2 = env.clock.now();
            file.sync_data().await?;
            let t3 = env.clock.now();
            if round >= WARMUP_ITERATIONS {
                hop_ns.push(nanos(t0, t1));
                write_ns.push(nanos(t1, t2));
                sync_ns.push(nanos(t2, t3));
            }
        }
        drop(file);
        env.disk.remove(&path).await?;
        let total_ns = write_ns.iter().zip(&sync_ns).map(|(w, s)| w + s).collect();
        writes.push(WriteSize {
            bytes: size,
            write_ns: Distribution::of(write_ns),
            sync_ns: Distribution::of(sync_ns),
            total_ns: Distribution::of(total_ns),
        });
    }

    let (mut create_ns, mut dir_sync_ns) = (Vec::new(), Vec::new());
    for round in 0..WARMUP_ITERATIONS + iterations {
        let t0 = env.clock.now();
        hop.append(0, Vec::new()).await?;
        let t1 = env.clock.now();
        let entry = dir.join(&format!("entry-{round}"))?;
        drop(env.disk.open(&entry, OpenMode::CreateNew).await?);
        let t2 = env.clock.now();
        env.disk.sync_dir(dir).await?;
        let t3 = env.clock.now();
        if round >= WARMUP_ITERATIONS {
            hop_ns.push(nanos(t0, t1));
            create_ns.push(nanos(t1, t2));
            dir_sync_ns.push(nanos(t2, t3));
        }
    }
    drop(hop);

    let hop_ns = Distribution::of(hop_ns);
    let dir_sync = DirSync {
        create_ns: Distribution::of(create_ns),
        sync_ns: Distribution::of(dir_sync_ns),
    };
    let (small, large) = (&writes[0], &writes[writes.len() - 1]);
    let (verdict, checks) = classify(&Medians {
        hop_ns: hop_ns.p50,
        small_sync_ns: small.sync_ns.p50,
        large_sync_ns: large.sync_ns.p50,
        large_bytes: large.bytes,
        dir_sync_ns: dir_sync.sync_ns.p50,
    });
    Ok(ProbeReport {
        schema: PROBE_SCHEMA,
        dir: String::new(),
        verdict,
        checks,
        hop_ns,
        writes,
        dir_sync,
    })
}

async fn remove_dir(env: &Env, dir: &DiskPath) -> Result<(), DiskError> {
    for entry in env.disk.list(dir).await? {
        env.disk.remove(&dir.join(&entry.name)?).await?;
    }
    env.disk.remove(dir).await?;
    env.disk.sync_dir(&DiskPath::root_dir(Root::Wal)).await
}

fn nanos(from: Instant, to: Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_nanos()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const US: u64 = 1_000;

    fn medians(hop: u64, small: u64, large: u64, dir: u64) -> Medians {
        Medians {
            hop_ns: hop,
            small_sync_ns: small,
            large_sync_ns: large,
            large_bytes: 8 * MIB,
            dir_sync_ns: dir,
        }
    }

    #[test]
    fn no_op_flushes_are_suspicious_on_every_check() {
        // tmpfs-like: every sync costs about the hop.
        let (verdict, checks) = classify(&medians(20 * US, 21 * US, 24 * US, 19 * US));
        assert_eq!(verdict, Verdict::Suspicious);
        assert!(checks.iter().all(|c| !c.pass), "{checks:?}");
        assert_eq!(checks[2].value_ns, -1_000);
    }

    #[test]
    fn device_flushes_are_honest() {
        let (verdict, checks) = classify(&medians(20 * US, 900 * US, 5_000 * US, 1_200 * US));
        assert_eq!(verdict, Verdict::Honest);
        assert_eq!(checks[1].floor_ns, 838_860);
    }

    #[test]
    fn slow_but_flat_syncs_are_suspicious() {
        // A throttled no-op sync can be slow; it still does not grow with the bytes written.
        let (verdict, checks) = classify(&medians(20 * US, 2_000 * US, 2_100 * US, 2_000 * US));
        assert_eq!(verdict, Verdict::Suspicious);
        assert_eq!(
            checks.iter().map(|c| c.pass).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[test]
    fn an_empty_distribution_is_zero() {
        let d = Distribution::of(Vec::new());
        assert_eq!((d.count, d.min, d.p50, d.max), (0, 0, 0, 0));
    }

    proptest! {
        /// Each percentile is the nearest-rank value: at least `ceil(p * n / 100)` samples are at or below
        /// it and fewer are strictly below it. Samples keep their order.
        #[test]
        fn percentiles_are_nearest_rank(samples in proptest::collection::vec(0u64..1_000, 1..200)) {
            let d = Distribution::of(samples.clone());
            prop_assert_eq!(&d.samples, &samples);
            let n = samples.len();
            for (percent, value) in [(10, d.p10), (50, d.p50), (90, d.p90), (99, d.p99), (100, d.max)] {
                let rank = (percent * n).div_ceil(100).max(1);
                prop_assert!(samples.iter().filter(|&&s| s <= value).count() >= rank);
                prop_assert!(samples.iter().filter(|&&s| s < value).count() < rank);
            }
            prop_assert_eq!(d.min, *samples.iter().min().unwrap());
        }

        /// Slower syncs never make an honest volume suspicious: raising the large sync or the directory sync,
        /// or both file syncs together, keeps every check passing.
        #[test]
        fn slower_syncs_never_lower_the_verdict(
            hop in 0u64..100_000,
            small in 0u64..10_000_000,
            large in 0u64..100_000_000,
            dir in 0u64..10_000_000,
            raise_large in 0u64..10_000_000,
            raise_dir in 0u64..10_000_000,
            raise_both in 0u64..10_000_000,
        ) {
            let before = classify(&medians(hop, small, large, dir)).0;
            let after = classify(&medians(
                hop,
                small + raise_both,
                large + raise_large + raise_both,
                dir + raise_dir,
            )).0;
            prop_assert!(before == Verdict::Suspicious || after == Verdict::Honest);
        }

        /// Syncs that cost no more than the hop are never honest, however slow the hop is.
        #[test]
        fn syncs_no_slower_than_the_hop_are_suspicious(hop in 0u64..100_000_000, below in 0u64..100_000_000) {
            let sync = hop.saturating_sub(below);
            let (verdict, _) = classify(&medians(hop, sync, sync, sync));
            prop_assert_eq!(verdict, Verdict::Suspicious);
        }
    }
}
