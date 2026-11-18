//! Names inside the WAL directory (contract section 4.1) and the small directory entries.

use bytes::Bytes;
use crucible_env::{Disk, DiskError, DiskPath, OpenMode, Root};
use crucible_records::Lsn;
use crucible_wal_codec::CheckpointHint;

pub(crate) const EPOCH_NAME: &str = "EPOCH";
pub(crate) const CHECKPOINT_NAME: &str = "CHECKPOINT";
pub(crate) const CHECKPOINT_TMP_NAME: &str = "CHECKPOINT.tmp";

/// What the log knows about one segment without reading its frames.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentMeta {
    /// The segment's number.
    pub segment_no: u64,
    /// LSN of the first frame written to the segment.
    pub first_lsn: Lsn,
    /// When the segment was created, in wall-clock microseconds.
    pub created_at_micros: i64,
}

pub(crate) fn wal_dir() -> DiskPath {
    DiskPath::root_dir(Root::Wal)
}

pub(crate) fn entry(name: &str) -> Result<DiskPath, DiskError> {
    DiskPath::new(Root::Wal, name)
}

pub(crate) fn segment_name(segment_no: u64) -> String {
    format!("seg-{segment_no:020}.wal")
}

pub(crate) fn segment_path(segment_no: u64) -> Result<DiskPath, DiskError> {
    entry(&segment_name(segment_no))
}

/// The segment number of a segment file name; `None` for every other name.
pub(crate) fn parse_segment_name(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("seg-")?.strip_suffix(".wal")?;
    if digits.len() != 20 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Replaces the `CHECKPOINT` hint: write a temporary entry, sync it, rename it over the hint, sync the
/// directory.
pub(crate) async fn write_hint(disk: &dyn Disk, hint: CheckpointHint) -> Result<(), DiskError> {
    let tmp = entry(CHECKPOINT_TMP_NAME)?;
    let encoded = hint.encode();
    {
        let file = disk.open(&tmp, OpenMode::Write).await?;
        file.append(0, vec![Bytes::copy_from_slice(&encoded)])
            .await?;
        file.truncate(encoded.len() as u64).await?;
        file.sync_data().await?;
    }
    disk.rename(&tmp, &entry(CHECKPOINT_NAME)?).await?;
    disk.sync_dir(&wal_dir()).await
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn segment_names_have_twenty_digits() {
        assert_eq!(segment_name(1), "seg-00000000000000000001.wal");
        assert_eq!(segment_name(u64::MAX), "seg-18446744073709551615.wal");
        assert_eq!(parse_segment_name("seg-00000000000000000001.wal"), Some(1));
        for bad in [
            "seg-1.wal",
            "seg-0000000000000000000a.wal",
            "seg-00000000000000000001.tmp",
            "seg-99999999999999999999.wal",
            "EPOCH",
            "seg-+0000000000000000001.wal",
        ] {
            assert_eq!(parse_segment_name(bad), None, "{bad}");
        }
    }

    proptest! {
        #[test]
        fn segment_names_round_trip_and_sort_numerically(a in any::<u64>(), b in any::<u64>()) {
            prop_assert_eq!(parse_segment_name(&segment_name(a)), Some(a));
            prop_assert_eq!(a.cmp(&b), segment_name(a).cmp(&segment_name(b)));
        }
    }
}
