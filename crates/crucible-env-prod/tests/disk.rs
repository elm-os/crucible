//! The portable disk backend against a real temporary directory.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use bytes::Bytes;
use crucible_env::{Disk, DiskError, DiskPath, OpenMode, Root};
use crucible_env_prod::PortableDisk;
use tempfile::TempDir;

fn disk() -> (TempDir, PortableDisk) {
    let dir = tempfile::tempdir().unwrap();
    let roots = BTreeMap::from([
        (Root::Wal, dir.path().join("wal")),
        (Root::Exports, dir.path().join("exports")),
    ]);
    let disk = PortableDisk::new(tokio::runtime::Handle::current(), &roots).unwrap();
    (dir, disk)
}

fn wal(rel: &str) -> DiskPath {
    DiskPath::new(Root::Wal, rel).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_append_read_sync_round_trip() {
    let (_dir, disk) = disk();
    disk.create_dir_all(&wal("segments")).await.unwrap();
    let path = wal("segments/0001.wal");
    let file = disk.open(&path, OpenMode::CreateNew).await.unwrap();

    file.append(
        0,
        vec![
            Bytes::from_static(b"hello "),
            Bytes::new(),
            Bytes::from_static(b"world"),
        ],
    )
    .await
    .unwrap();
    file.append(11, vec![Bytes::from_static(b"!")])
        .await
        .unwrap();
    file.sync_data().await.unwrap();
    disk.sync_dir(&wal("segments")).await.unwrap();
    disk.sync_dir(&DiskPath::root_dir(Root::Wal)).await.unwrap();

    assert_eq!(
        file.read_at(0, 12).await.unwrap(),
        Bytes::from_static(b"hello world!")
    );
    assert_eq!(
        file.read_at(6, 5).await.unwrap(),
        Bytes::from_static(b"world")
    );
    // Short only at end of file.
    assert_eq!(
        file.read_at(6, 100).await.unwrap(),
        Bytes::from_static(b"world!")
    );
    assert_eq!(file.read_at(12, 4).await.unwrap(), Bytes::new());
    assert_eq!(file.read_at(500, 4).await.unwrap(), Bytes::new());

    // A second handle, opened for reading, sees the same bytes and cannot write.
    let reader = disk.open(&path, OpenMode::Read).await.unwrap();
    assert_eq!(
        reader.read_at(0, 5).await.unwrap(),
        Bytes::from_static(b"hello")
    );
    assert!(
        reader
            .append(0, vec![Bytes::from_static(b"x")])
            .await
            .is_err()
    );

    // Overwriting in place at an explicit offset.
    file.append(0, vec![Bytes::from_static(b"J")])
        .await
        .unwrap();
    assert_eq!(
        reader.read_at(0, 5).await.unwrap(),
        Bytes::from_static(b"Jello")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn create_new_conflicts_and_write_mode_creates_without_truncating() {
    let (_dir, disk) = disk();
    let path = wal("EPOCH");
    let file = disk.open(&path, OpenMode::CreateNew).await.unwrap();
    file.append(0, vec![Bytes::from_static(b"abc")])
        .await
        .unwrap();

    assert_eq!(
        disk.open(&path, OpenMode::CreateNew).await.unwrap_err(),
        DiskError::AlreadyExists
    );
    let again = disk.open(&path, OpenMode::Write).await.unwrap();
    assert_eq!(
        again.read_at(0, 10).await.unwrap(),
        Bytes::from_static(b"abc")
    );

    let fresh = disk.open(&wal("fresh"), OpenMode::Write).await.unwrap();
    assert_eq!(fresh.read_at(0, 10).await.unwrap(), Bytes::new());
    assert_eq!(
        disk.open(&wal("missing"), OpenMode::Read)
            .await
            .unwrap_err(),
        DiskError::NotFound
    );
    assert_eq!(
        disk.open(&wal("no-dir/file"), OpenMode::CreateNew)
            .await
            .unwrap_err(),
        DiskError::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn list_is_sorted_by_name_and_reports_lengths() {
    let (_dir, disk) = disk();
    for (name, len) in [("b", 2usize), ("a", 1), ("B", 4), ("10", 3), ("9", 5)] {
        let file = disk.open(&wal(name), OpenMode::CreateNew).await.unwrap();
        file.append(0, vec![Bytes::from(vec![7; len])])
            .await
            .unwrap();
    }
    disk.create_dir_all(&wal("dir/nested")).await.unwrap();

    let entries = disk.list(&DiskPath::root_dir(Root::Wal)).await.unwrap();
    let listed: Vec<(&str, u64)> = entries.iter().map(|e| (e.name.as_str(), e.len)).collect();
    assert_eq!(
        listed,
        [
            ("10", 3),
            ("9", 5),
            ("B", 4),
            ("a", 1),
            ("b", 2),
            ("dir", 0)
        ]
    );
    assert_eq!(disk.list(&wal("dir/nested")).await.unwrap(), []);
    assert_eq!(
        disk.list(&wal("absent")).await.unwrap_err(),
        DiskError::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_replaces_and_remove_deletes() {
    let (_dir, disk) = disk();
    let tmp = disk
        .open(&wal("CHECKPOINT.tmp"), OpenMode::CreateNew)
        .await
        .unwrap();
    tmp.append(0, vec![Bytes::from_static(b"new")])
        .await
        .unwrap();
    tmp.sync_data().await.unwrap();
    let old = disk
        .open(&wal("CHECKPOINT"), OpenMode::CreateNew)
        .await
        .unwrap();
    old.append(0, vec![Bytes::from_static(b"old-old")])
        .await
        .unwrap();
    drop((tmp, old));

    disk.rename(&wal("CHECKPOINT.tmp"), &wal("CHECKPOINT"))
        .await
        .unwrap();
    disk.sync_dir(&DiskPath::root_dir(Root::Wal)).await.unwrap();
    let names: Vec<String> = disk
        .list(&DiskPath::root_dir(Root::Wal))
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["CHECKPOINT"]);
    let file = disk.open(&wal("CHECKPOINT"), OpenMode::Read).await.unwrap();
    assert_eq!(
        file.read_at(0, 10).await.unwrap(),
        Bytes::from_static(b"new")
    );
    drop(file);

    assert_eq!(
        disk.rename(&wal("absent"), &wal("x")).await.unwrap_err(),
        DiskError::NotFound
    );
    let across = disk
        .rename(
            &wal("CHECKPOINT"),
            &DiskPath::new(Root::Exports, "x").unwrap(),
        )
        .await;
    assert!(matches!(across, Err(DiskError::InvalidPath { .. })));

    disk.remove(&wal("CHECKPOINT")).await.unwrap();
    assert_eq!(
        disk.remove(&wal("CHECKPOINT")).await.unwrap_err(),
        DiskError::NotFound
    );
    disk.create_dir_all(&wal("empty")).await.unwrap();
    disk.remove(&wal("empty")).await.unwrap();
    assert_eq!(disk.list(&DiskPath::root_dir(Root::Wal)).await.unwrap(), []);
}

#[tokio::test(flavor = "multi_thread")]
async fn preallocate_reads_zeros_and_truncate_sets_the_length() {
    let (_dir, disk) = disk();
    let path = wal("segment");
    let file = disk.open(&path, OpenMode::CreateNew).await.unwrap();
    file.append(0, vec![Bytes::from_static(b"head")])
        .await
        .unwrap();
    file.preallocate(1 << 16).await.unwrap();
    // Never shrinks.
    file.preallocate(10).await.unwrap();

    let all = file.read_at(0, 1 << 17).await.unwrap();
    assert_eq!(all.len(), 1 << 16);
    assert_eq!(&all[..4], b"head");
    assert!(all[4..].iter().all(|b| *b == 0));
    assert_eq!(
        disk.list(&DiskPath::root_dir(Root::Wal)).await.unwrap()[0].len,
        1 << 16
    );

    file.append(100, vec![Bytes::from_static(b"frame")])
        .await
        .unwrap();
    file.truncate(102).await.unwrap();
    file.sync_data().await.unwrap();
    let all = file.read_at(0, 1 << 16).await.unwrap();
    assert_eq!(all.len(), 102);
    assert_eq!(&all[100..], b"fr");
}

#[tokio::test(flavor = "multi_thread")]
async fn free_space_is_reported_and_unconfigured_roots_are_not_found() {
    let (_dir, disk) = disk();
    assert!(disk.free_space(Root::Wal).await.unwrap() > 0);
    assert_eq!(
        disk.free_space(Root::Ui).await.unwrap_err(),
        DiskError::NotFound
    );
    let ui = DiskPath::new(Root::Ui, "index.html").unwrap();
    assert_eq!(
        disk.open(&ui, OpenMode::Read).await.unwrap_err(),
        DiskError::NotFound
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn operations_on_one_root_apply_in_submission_order() {
    let (_dir, disk) = disk();
    let file = disk
        .open(&wal("ordered"), OpenMode::CreateNew)
        .await
        .unwrap();
    // Submit without awaiting in between; the writer thread must apply them in order.
    let writes: Vec<_> = (0..200u8)
        .map(|i| file.append(0, vec![Bytes::from(vec![i; 64])]))
        .collect();
    for result in futures::future::join_all(writes).await {
        result.unwrap();
    }
    assert_eq!(
        file.read_at(0, 64).await.unwrap(),
        Bytes::from(vec![199; 64])
    );
}
