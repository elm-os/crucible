//! Recovery (contract section 4.6): validate the directory, find the end of the valid log, repair the
//! tail of the last segment and collect what the shipper needs to resume.
//!
//! Every step is idempotent, so a crash at any point followed by another recovery reaches the state one
//! uninterrupted recovery reaches. Work is bounded by the retained segments: each is read at most twice.
//!
//! Two rules go beyond the letter of the contract and are what its fault model requires:
//!
//! - The bytes after the valid prefix of the last segment are zeroed whenever any of them is not zero,
//!   not only when the prefix ends in a torn frame. A crash can lose one block and keep a later one, so a
//!   clean end may be followed by frames of an earlier incarnation; a later append would line them up
//!   behind new frames.
//! - The valid frames of the last segment are written again from their verified bytes and synced before
//!   the log accepts appends. Their durability is unknown: the previous process may have died after a
//!   failed sync, and a sync on a fresh handle says nothing about bytes written before it.

use bytes::Bytes;
use crucible_env::{Clock, Disk, DiskError, Env, File, OpenMode};
use crucible_records::{Lsn, WalEpoch};
use crucible_wal_codec::{
    BatchSeal, Checkpoint, CheckpointHint, EPOCH_ENTRY_LEN, Record, RecordType,
    SEGMENT_HEADER_LEN, SegmentEnd, SegmentHeader, decode_epoch, encode_epoch,
};

use crate::config::{GROUP_BYTES, WalConfig};
use crate::error::{
    RecoveryError, SegmentHeaderIssue, WalGap, bad_compression, bad_payload,
};
use crate::layout::{
    CHECKPOINT_NAME, EPOCH_NAME, SegmentMeta, entry, parse_segment_name, segment_path, wal_dir,
};
use crate::scan::{SegmentReader, Step, WalFrame};

const HEADER_LEN: u64 = SEGMENT_HEADER_LEN as u64;

/// The tail of the last segment that recovery zeroed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TailRepair {
    /// The segment.
    pub segment_no: u64,
    /// End of the valid prefix: everything from here on was zeroed.
    pub offset: u64,
}

/// The highest checkpoint in the log and where its frame is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Watermark {
    /// The checkpoint record. Every frame with an LSN at or below its `last_lsn` is resolved.
    pub checkpoint: Checkpoint,
    /// The segment that holds the checkpoint frame.
    pub segment_no: u64,
}

impl Watermark {
    /// The low watermark.
    pub fn last_lsn(&self) -> Lsn {
        self.checkpoint.last_lsn
    }

    pub(crate) fn hint(&self) -> CheckpointHint {
        CheckpointHint {
            batch_seq: self.checkpoint.batch_seq,
            last_lsn: self.checkpoint.last_lsn,
            segment_no: self.segment_no,
        }
    }
}

/// What recovery found.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RecoveryReport {
    /// The directory's epoch.
    pub epoch: WalEpoch,
    /// The directory was empty and has just been initialised.
    pub created: bool,
    /// The last durable LSN; zero for an empty log. The next append gets this plus 1.
    pub last_lsn: Lsn,
    /// The highest checkpoint.
    pub watermark: Option<Watermark>,
    /// Every `BatchSeal` whose `last_lsn` is above the watermark, in LSN order: sealed batches that no
    /// checkpoint covers. The shipper re-ships them with these boundaries.
    pub unchecked_seals: Vec<BatchSeal>,
    /// The retained segments, oldest first.
    pub segments: Vec<SegmentMeta>,
    /// The tail of the last segment that was zeroed.
    pub tail_repair: Option<TailRepair>,
    /// A newest segment whose creation never finished and that was removed.
    pub removed_unfinished_segment: Option<u64>,
    /// Whether the `CHECKPOINT` hint shortened the scan.
    pub used_hint: bool,
    /// Segments whose frames were scanned.
    pub scanned_segments: u64,
    /// Frame bytes scanned.
    pub scanned_bytes: u64,
}

impl RecoveryReport {
    /// The watermark's LSN, or zero.
    pub fn watermark_lsn(&self) -> Lsn {
        self.watermark.map_or(Lsn::ZERO, |w| w.last_lsn())
    }

    /// First LSN of the oldest retained segment: where a replay of the retained prefix starts.
    pub fn first_retained_lsn(&self) -> Lsn {
        self.segments.first().map_or(Lsn::FIRST, |s| s.first_lsn)
    }

    /// The LSN range `[from, to]` that is above the watermark and must be re-shipped or shipped; empty
    /// when `from > to`.
    pub fn unresolved_range(&self) -> (Lsn, Lsn) {
        (
            Lsn::new(self.watermark_lsn().get().saturating_add(1)),
            self.last_lsn,
        )
    }

    /// The last LSN covered by a checkpoint or an unchecked seal. Frames above it are the unsealed tail.
    pub fn sealed_through(&self) -> Lsn {
        self.unchecked_seals
            .iter()
            .map(|s| s.last_lsn)
            .fold(self.watermark_lsn(), Lsn::max)
    }
}

/// The open log that recovery hands to the committer.
#[derive(Debug)]
pub(crate) struct Recovered {
    pub(crate) report: RecoveryReport,
    /// The last segment, open for writing.
    pub(crate) file: File,
    /// Where the next frame goes in it.
    pub(crate) write_offset: u64,
    /// The watermark of the valid `CHECKPOINT` hint on disk.
    pub(crate) hint_lsn: Option<Lsn>,
}

async fn read_entry(
    disk: &dyn Disk,
    name: &str,
    max: usize,
) -> Result<Option<Bytes>, DiskError> {
    match disk.open(&entry(name)?, OpenMode::Read).await {
        Ok(file) => Ok(Some(file.read_at(0, max).await?)),
        Err(DiskError::NotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Creates segment `segment_no`: preallocate, write the header, `sync_data`, then `sync_dir`.
pub(crate) async fn create_segment(
    disk: &dyn Disk,
    clock: &dyn Clock,
    config: &WalConfig,
    epoch: WalEpoch,
    segment_no: u64,
    first_lsn: Lsn,
) -> Result<(File, SegmentMeta), DiskError> {
    let header = SegmentHeader {
        wal_epoch: epoch,
        segment_no,
        first_lsn,
        created_at: clock.wall_now(),
    };
    let file = disk
        .open(&segment_path(segment_no)?, OpenMode::CreateNew)
        .await?;
    file.preallocate(config.segment_bytes).await?;
    file.append(0, vec![Bytes::copy_from_slice(&header.encode())])
        .await?;
    file.sync_data().await?;
    disk.sync_dir(&wal_dir()).await?;
    let meta = SegmentMeta {
        segment_no,
        first_lsn,
        created_at_micros: header.created_at.micros(),
    };
    Ok((file, meta))
}

async fn create_epoch(env: &Env) -> Result<WalEpoch, DiskError> {
    let epoch = WalEpoch::from_uuid(env.new_id());
    let file = env
        .disk
        .open(&entry(EPOCH_NAME)?, OpenMode::CreateNew)
        .await?;
    file.append(0, vec![Bytes::copy_from_slice(&encode_epoch(epoch))])
        .await?;
    file.sync_data().await?;
    env.disk.sync_dir(&wal_dir()).await?;
    Ok(epoch)
}

/// Reads `EPOCH`, or creates it when the directory holds no segment. Returns the epoch and whether it was
/// created.
async fn load_epoch(env: &Env, has_segments: bool) -> Result<(WalEpoch, bool), RecoveryError> {
    // One byte more than the entry, so that a longer file fails the length check.
    let stored = read_entry(env.disk.as_ref(), EPOCH_NAME, EPOCH_ENTRY_LEN + 1).await?;
    match stored.as_deref().map(decode_epoch) {
        Some(Ok(epoch)) => Ok((epoch, false)),
        Some(Err(e)) if has_segments => Err(RecoveryError::InvalidEpoch(e)),
        None if has_segments => Err(RecoveryError::MissingEpoch),
        Some(Err(_)) => {
            // An initialisation that crashed before its sync: no segment can exist, so nothing is lost.
            env.disk.remove(&entry(EPOCH_NAME)?).await?;
            Ok((create_epoch(env).await?, true))
        }
        None => Ok((create_epoch(env).await?, true)),
    }
}

fn decode_header(bytes: &[u8]) -> Result<SegmentHeader, SegmentHeaderIssue> {
    if bytes.iter().all(|&b| b == 0) {
        return Err(SegmentHeaderIssue::Unwritten);
    }
    let array = <&[u8; SEGMENT_HEADER_LEN]>::try_from(bytes).map_err(|_| SegmentHeaderIssue::Short)?;
    Ok(SegmentHeader::decode(array)?)
}

/// Validates every segment header and the contiguity of segment numbers.
///
/// The newest segment may be an unfinished creation: its header is zero, short or partly written and
/// nothing follows it, because no frame is written to a segment before its header and its directory entry
/// are durable. Such a segment is removed. Removal destroys data, so the header is read twice first.
async fn load_segments(
    disk: &dyn Disk,
    epoch: WalEpoch,
    numbers: &[u64],
) -> Result<(Vec<SegmentMeta>, Option<u64>), RecoveryError> {
    let mut metas: Vec<SegmentMeta> = Vec::with_capacity(numbers.len());
    let mut removed = None;
    for (index, &segment_no) in numbers.iter().enumerate() {
        let bad = |reason| RecoveryError::BadSegmentHeader { segment_no, reason };
        if let Some(previous) = metas.last()
            && previous.segment_no.checked_add(1) != Some(segment_no)
        {
            return Err(WalGap::MissingSegment {
                expected: previous.segment_no.saturating_add(1),
                found: segment_no,
            }
            .into());
        }
        let path = segment_path(segment_no)?;
        let file = disk.open(&path, OpenMode::Read).await?;
        let header = match decode_header(&file.read_at(0, SEGMENT_HEADER_LEN).await?) {
            Ok(header) => header,
            Err(issue) if index + 1 == numbers.len() => {
                if decode_header(&file.read_at(0, SEGMENT_HEADER_LEN).await?).is_ok() {
                    return Err(RecoveryError::UnstableRead {
                        segment_no,
                        offset: 0,
                    });
                }
                if SegmentReader::new(file, segment_no, HEADER_LEN)
                    .tail_has_data()
                    .await?
                {
                    return Err(bad(issue));
                }
                disk.remove(&path).await?;
                disk.sync_dir(&wal_dir()).await?;
                removed = Some(segment_no);
                break;
            }
            Err(issue) => return Err(bad(issue)),
        };
        if header.wal_epoch != epoch {
            return Err(bad(SegmentHeaderIssue::EpochMismatch));
        }
        if header.segment_no != segment_no {
            return Err(bad(SegmentHeaderIssue::NumberMismatch));
        }
        let floor = metas.last().map_or(Lsn::FIRST, |m| m.first_lsn);
        if header.first_lsn < floor {
            return Err(WalGap::FirstLsnRegressed {
                segment_no,
                first_lsn: header.first_lsn,
            }
            .into());
        }
        metas.push(SegmentMeta {
            segment_no,
            first_lsn: header.first_lsn,
            created_at_micros: header.created_at.micros(),
        });
    }
    Ok((metas, removed))
}

/// The index of the segment that holds the hint's watermark plus 1: frames before it are all resolved.
fn hint_start(segments: &[SegmentMeta], hint: CheckpointHint) -> Option<usize> {
    let first = segments.first()?;
    let last = segments.last()?;
    let unresolved = hint.last_lsn.get().checked_add(1)?;
    if hint.segment_no < first.segment_no
        || hint.segment_no > last.segment_no
        || first.first_lsn.get() > unresolved
    {
        return None;
    }
    let index = segments
        .iter()
        .skip(1)
        .position(|next| next.first_lsn.get() > unresolved)
        .unwrap_or(segments.len() - 1);
    Some(index)
}

#[derive(Debug)]
struct Scanned {
    last_lsn: Lsn,
    file: File,
    write_offset: u64,
    watermark: Option<Watermark>,
    seals: Vec<BatchSeal>,
    tail_repair: Option<TailRepair>,
    segments: u64,
    bytes: u64,
}

#[derive(Debug, Default)]
struct Control {
    watermark: Option<Watermark>,
    seals: Vec<BatchSeal>,
}

impl Control {
    fn collect(&mut self, frame: &WalFrame) -> Result<(), RecoveryError> {
        let record_type = frame.record_type();
        if !matches!(record_type, RecordType::BatchSeal | RecordType::Checkpoint) {
            return Ok(());
        }
        let lsn = frame.lsn();
        let payload = frame.payload().map_err(|e| bad_compression(lsn, e))?;
        let beyond = |last: Lsn| RecoveryError::BadRecord {
            lsn,
            reason: format!("covers lsn {last}, at or above its own"),
        };
        match Record::decode(record_type, &payload).map_err(|e| bad_payload(lsn, &e))? {
            Record::BatchSeal(seal) => {
                if seal.last_lsn >= lsn {
                    return Err(beyond(seal.last_lsn));
                }
                self.seals.push(seal);
            }
            Record::Checkpoint(checkpoint) => {
                if checkpoint.last_lsn >= lsn {
                    return Err(beyond(checkpoint.last_lsn));
                }
                if self
                    .watermark
                    .is_none_or(|w| checkpoint.last_lsn >= w.last_lsn())
                {
                    self.watermark = Some(Watermark {
                        checkpoint,
                        segment_no: frame.segment_no(),
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Verified frame bytes of the last segment, waiting to be written again.
#[derive(Debug)]
struct Rewrite {
    start: u64,
    len: usize,
    bufs: Vec<Bytes>,
}

impl Rewrite {
    async fn flush(&mut self, file: &File) -> Result<(), DiskError> {
        if self.len > 0 {
            file.append(self.start, std::mem::take(&mut self.bufs))
                .await?;
            self.start += self.len as u64;
            self.len = 0;
        }
        Ok(())
    }
}

/// Reads the frames of one segment that belong to the valid log. Returns the LSN expected next and how the
/// data ended (`None` when the segment reached `limit`, the next segment's first LSN).
async fn read_frames(
    reader: &mut SegmentReader,
    meta: &SegmentMeta,
    limit: Option<u64>,
    control: &mut Control,
    stats: &mut Stats,
    mut rewrite: Option<&mut Rewrite>,
) -> Result<(u64, Option<SegmentEnd>), RecoveryError> {
    let segment_no = meta.segment_no;
    let mut expect = meta.first_lsn.get();
    stats.segments += 1;
    loop {
        if limit == Some(expect) {
            return Ok((expect, None));
        }
        let frame = match reader.step().await? {
            Step::Frame(frame) => frame,
            Step::End(SegmentEnd::Corrupt { error, .. }) => {
                return Err(RecoveryError::CorruptFrame {
                    segment_no,
                    offset: reader.offset(),
                    error,
                });
            }
            Step::End(end) => return Ok((expect, Some(end))),
        };
        let lsn = frame.lsn().get();
        // The next segment's first LSN is authoritative: frames at or above it were superseded.
        if limit.is_some_and(|limit| lsn >= limit) {
            return Ok((expect, None));
        }
        if lsn != expect {
            return Err(WalGap::LsnMismatch {
                segment_no,
                offset: frame.offset(),
                expected: Lsn::new(expect),
                found: frame.lsn(),
            }
            .into());
        }
        control.collect(&frame)?;
        stats.bytes += frame.raw().len() as u64;
        expect = expect.saturating_add(1);
        if let Some(rewrite) = rewrite.as_deref_mut() {
            rewrite.len += frame.raw().len();
            rewrite.bufs.push(frame.raw().clone());
            if rewrite.len >= GROUP_BYTES {
                rewrite.flush(reader.file()).await?;
            }
        }
    }
}

#[derive(Debug, Default)]
struct Stats {
    segments: u64,
    bytes: u64,
}

/// Scans the frames of `earlier` and `last` (consecutive segments ending with the newest) and repairs the
/// tail of `last`.
async fn scan(
    disk: &dyn Disk,
    config: &WalConfig,
    earlier: &[SegmentMeta],
    last: &SegmentMeta,
) -> Result<Scanned, RecoveryError> {
    let mut control = Control::default();
    let mut stats = Stats::default();
    let nexts = earlier.iter().skip(1).chain(std::iter::once(last));
    for (meta, next) in earlier.iter().zip(nexts) {
        let file = disk
            .open(&segment_path(meta.segment_no)?, OpenMode::Read)
            .await?;
        let mut reader = SegmentReader::new(file, meta.segment_no, HEADER_LEN);
        let limit = next.first_lsn.get();
        let (expect, _) = read_frames(
            &mut reader,
            meta,
            Some(limit),
            &mut control,
            &mut stats,
            None,
        )
        .await?;
        if expect < limit {
            return Err(WalGap::ShortSegment {
                segment_no: meta.segment_no,
                valid_through: Lsn::new(expect.saturating_sub(1)),
                next_first_lsn: next.first_lsn,
            }
            .into());
        }
    }

    // The last segment: make its verified frames durable and leave only zeros after them.
    let segment_no = last.segment_no;
    let file = disk
        .open(&segment_path(segment_no)?, OpenMode::Write)
        .await?;
    let mut reader = SegmentReader::new(file, segment_no, HEADER_LEN);
    let mut rewrite = Rewrite {
        start: HEADER_LEN,
        len: 0,
        bufs: Vec::new(),
    };
    let (expect, end) = read_frames(
        &mut reader,
        last,
        None,
        &mut control,
        &mut stats,
        Some(&mut rewrite),
    )
    .await?;
    rewrite.flush(reader.file()).await?;
    let offset = reader.offset();
    let torn = matches!(end, Some(SegmentEnd::Torn { .. }));
    let dirty = torn || reader.tail_has_data().await?;
    if torn {
        // Truncating destroys data, and one read can be wrong: read the tail a second time.
        reader.invalidate();
        match reader.step().await? {
            Step::Frame(_) => return Err(RecoveryError::UnstableRead { segment_no, offset }),
            Step::End(SegmentEnd::Corrupt { error, .. }) => {
                return Err(RecoveryError::CorruptFrame {
                    segment_no,
                    offset,
                    error,
                });
            }
            Step::End(_) => {}
        }
    }
    let file = reader.into_file();
    let mut tail_repair = None;
    if dirty && !config.keep_torn_tail() {
        file.truncate(offset).await?;
        tail_repair = Some(TailRepair { segment_no, offset });
    }
    file.preallocate(config.segment_bytes).await?;
    file.sync_data().await?;
    Ok(Scanned {
        last_lsn: Lsn::new(expect.saturating_sub(1)),
        file,
        write_offset: offset,
        watermark: control.watermark,
        seals: control.seals,
        tail_repair,
        segments: stats.segments,
        bytes: stats.bytes,
    })
}

/// Opens the WAL directory: creates it when empty, otherwise recovers it.
pub(crate) async fn recover(env: &Env, config: &WalConfig) -> Result<Recovered, RecoveryError> {
    let disk = env.disk.as_ref();
    let entries = disk.list(&wal_dir()).await?;
    let numbers: Vec<u64> = entries
        .iter()
        .filter_map(|e| parse_segment_name(&e.name))
        .collect();
    let (epoch, created) = load_epoch(env, !numbers.is_empty()).await?;
    let (segments, removed_unfinished_segment) = load_segments(disk, epoch, &numbers).await?;

    let Some((last, earlier)) = segments.split_last() else {
        let (file, meta) =
            create_segment(disk, env.clock.as_ref(), config, epoch, 1, Lsn::FIRST).await?;
        return Ok(Recovered {
            report: RecoveryReport {
                epoch,
                created,
                last_lsn: Lsn::ZERO,
                watermark: None,
                unchecked_seals: Vec::new(),
                segments: vec![meta],
                tail_repair: None,
                removed_unfinished_segment,
                used_hint: false,
                scanned_segments: 0,
                scanned_bytes: 0,
            },
            file,
            write_offset: HEADER_LEN,
            hint_lsn: None,
        });
    };

    // The hint is advisory: it is used only when the scan it selects finds a checkpoint that confirms it.
    let hint = read_entry(disk, CHECKPOINT_NAME, 64)
        .await?
        .and_then(|bytes| CheckpointHint::decode(&bytes).ok());
    let mut used_hint = false;
    let mut scanned = None;
    if let Some(hint) = hint
        && let Some(start) = hint_start(&segments, hint)
        && start > 0
    {
        let candidate = scan(disk, config, earlier.get(start..).unwrap_or_default(), last).await?;
        if candidate
            .watermark
            .is_some_and(|w| w.last_lsn() >= hint.last_lsn)
        {
            used_hint = true;
            scanned = Some(candidate);
        }
    }
    let scanned = match scanned {
        Some(scanned) => scanned,
        None => scan(disk, config, earlier, last).await?,
    };

    let watermark_lsn = scanned.watermark.map_or(Lsn::ZERO, |w| w.last_lsn());
    let oldest_first_lsn = segments.first().map_or(Lsn::FIRST, |s| s.first_lsn);
    if oldest_first_lsn.get() > watermark_lsn.get().saturating_add(1) {
        return Err(WalGap::MissingPrefix {
            oldest_first_lsn,
            watermark: watermark_lsn,
        }
        .into());
    }
    let mut unchecked_seals = scanned.seals;
    unchecked_seals.retain(|seal| seal.last_lsn > watermark_lsn);

    Ok(Recovered {
        report: RecoveryReport {
            epoch,
            created,
            last_lsn: scanned.last_lsn,
            watermark: scanned.watermark,
            unchecked_seals,
            segments,
            tail_repair: scanned.tail_repair,
            removed_unfinished_segment,
            used_hint,
            scanned_segments: scanned.segments,
            scanned_bytes: scanned.bytes,
        },
        file: scanned.file,
        write_offset: scanned.write_offset,
        hint_lsn: hint
            .filter(|h| h.last_lsn <= watermark_lsn)
            .map(|h| h.last_lsn),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(segment_no: u64, first_lsn: u64) -> SegmentMeta {
        SegmentMeta {
            segment_no,
            first_lsn: Lsn::new(first_lsn),
            created_at_micros: 0,
        }
    }

    fn hint(last_lsn: u64, segment_no: u64) -> CheckpointHint {
        CheckpointHint {
            batch_seq: 1,
            last_lsn: Lsn::new(last_lsn),
            segment_no,
        }
    }

    #[test]
    fn the_hint_selects_the_segment_that_holds_the_first_unresolved_lsn() {
        let segments = [meta(3, 10), meta(4, 20), meta(5, 20), meta(6, 30)];
        // Watermark 9: lsn 10 is the first frame of segment 3.
        assert_eq!(hint_start(&segments, hint(9, 6)), Some(0));
        // Watermark 18: lsn 19 is still in segment 3, although the checkpoint frame is in segment 6.
        assert_eq!(hint_start(&segments, hint(18, 6)), Some(0));
        // Watermark 19: lsn 20 belongs to segment 5; segment 4 was abandoned empty.
        assert_eq!(hint_start(&segments, hint(19, 6)), Some(2));
        assert_eq!(hint_start(&segments, hint(29, 6)), Some(3));
        assert_eq!(hint_start(&segments, hint(500, 6)), Some(3));
    }

    #[test]
    fn a_hint_that_does_not_fit_the_directory_is_ignored() {
        let segments = [meta(3, 10), meta(4, 20)];
        assert_eq!(hint_start(&segments, hint(15, 2)), None);
        assert_eq!(hint_start(&segments, hint(15, 5)), None);
        // The oldest segment starts above the hint's watermark plus 1.
        assert_eq!(hint_start(&segments, hint(5, 3)), None);
        assert_eq!(hint_start(&segments, hint(u64::MAX, 3)), None);
        assert_eq!(hint_start(&[], hint(1, 1)), None);
    }
}
