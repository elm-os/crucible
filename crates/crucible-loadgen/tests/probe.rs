//! The fsync-honesty probe through the production `Env` on the build machine's disk. The verdict depends
//! on the machine, so only the report's shape and the cleanup are asserted.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use crucible_env::{DiskPath, Root};
use crucible_env_prod::{ProdConfig, WalBackend};
use crucible_loadgen::{PROBE_SCHEMA, Verdict, WRITE_SIZES, probe};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_measures_every_phase_and_cleans_up() {
    let config = ProdConfig {
        roots: BTreeMap::from([(
            Root::Wal,
            PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("fsync-probe-test"),
        )]),
        wal_backend: WalBackend::Portable,
    };
    let env = crucible_env_prod::build(tokio::runtime::Handle::current(), &config)
        .unwrap()
        .env;
    let root = DiskPath::root_dir(Root::Wal);
    let before = env.disk.list(&root).await.unwrap();

    let report = probe(&env, 5).await.unwrap();

    assert_eq!(report.schema, PROBE_SCHEMA);
    assert_eq!(
        report.writes.iter().map(|w| w.bytes).collect::<Vec<_>>(),
        WRITE_SIZES
    );
    for write in &report.writes {
        assert_eq!(write.sync_ns.count, 5);
        assert_eq!(write.write_ns.samples.len(), 5);
        assert!(write.total_ns.min >= write.sync_ns.min);
        assert!(write.sync_ns.max > 0);
    }
    assert_eq!(report.dir_sync.sync_ns.count, 5);
    assert_eq!(report.hop_ns.count, 5 * WRITE_SIZES.len() + 5);
    assert_eq!(report.checks.len(), 3);
    assert_eq!(
        report.verdict == Verdict::Honest,
        report.checks.iter().all(|c| c.pass)
    );
    assert_eq!(env.disk.list(&root).await.unwrap(), before);
}
