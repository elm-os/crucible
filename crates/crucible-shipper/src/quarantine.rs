//! Quarantine (contract 5.6): the frames of a batch are copied to the quarantine root with a manifest and synced,
//! then a `Quarantine` frame is appended and synced. Only then does the pair count as resolved.
//!
//! Layout of `quarantine/<quarantine_ref>/`:
//!
//! | Entry | Contents |
//! |---|---|
//! | `frames` | Every frame of the batch's LSN range, in LSN order, each as its 36-byte WAL frame header followed by its stored payload: the bytes of a segment without the segment header |
//! | `manifest` | Text, one `key=value` per line: `version`, `wal_epoch`, `first_lsn`, `last_lsn`, `table`, `table_id`, `partition`, `token`, `error_code`, `attempts`, `frames`, `bytes`, `reason` |
//!
//! `quarantine_ref` is the pair's token with `:` replaced by `_`, so a pair has one directory, and a copy
//! repeated after a crash overwrites the earlier one (contract 5.9).
//!
//! **Frame quarantine.** A durable frame that this binary cannot decode or normalize is copied by itself to
//! `quarantine/frame-<wal_epoch>-<lsn>/`, and then counts as a frame without rows. No WAL record announces it:
//! a replay with the same binary decides the same, finds the copy and moves on.
//!
//! | Entry | Contents |
//! |---|---|
//! | `frames` | The one frame, as above |
//! | `manifest` | Text, one `key=value` per line: `version`, `kind=frame`, `wal_epoch`, `lsn`, `record_type`, `project_id` (when the envelope decodes), `normalizer_version`, `stage`, `reason`, `frames=1`, `bytes` |
//!
//! `bytes` is the last line of a whole manifest. The manifest is created with `CreateNew` after the frames file
//! is synced; a manifest that is already whole ends the copy at once.

use std::sync::Arc;

use bytes::Bytes;
use crucible_env::{DiskError, DiskPath, File, OpenMode, Root};
use crucible_normalize::NORMALIZER_VERSION;
use crucible_records::{Lsn, ProjectId, WalEpoch};
use crucible_wal::{ReplayError, WalFrame};
use crucible_wal_codec::{Quarantine, QuarantinePair, Record};

use crate::batch::{BatchId, Pair, pair_token};
use crate::error::ShipError;
use crate::frames::Unshippable;
use crate::hooks::Barrier;
use crate::limits::backoff;
use crate::shipper::Ctx;

/// The file of a quarantine directory that holds the frames.
pub const FRAMES_FILE: &str = "frames";
/// The file of a quarantine directory that describes it.
pub const MANIFEST_FILE: &str = "manifest";

/// Bytes buffered before one write of the copy.
const COPY_CHUNK: usize = 1 << 20;

/// The directory name of a pair's quarantine.
pub fn quarantine_ref(batch: &BatchId, pair: Pair) -> Result<String, ShipError> {
    Ok(pair_token(batch, pair)?.text().as_str().replace(':', "_"))
}

#[derive(Debug, thiserror::Error)]
enum CopyError {
    #[error("disk: {0}")]
    Disk(#[from] DiskError),
    #[error("replay: {0}")]
    Replay(#[from] ReplayError),
    #[error("the log ends at {0}, inside the batch")]
    Short(Lsn),
    #[error("a frame header does not encode")]
    Header,
}

/// Replaces the contents of `path` with `chunks` and syncs the file.
async fn write_file(ctx: &Ctx, path: &DiskPath, chunks: Vec<Bytes>) -> Result<(), DiskError> {
    let file = ctx.env.disk.open(path, OpenMode::Write).await?;
    file.truncate(0).await?;
    file.append(0, chunks).await?;
    file.sync_data().await
}

async fn flush(file: &File, offset: &mut u64, pending: &mut Vec<Bytes>) -> Result<(), DiskError> {
    let len: usize = pending.iter().map(Bytes::len).sum();
    if len > 0 {
        file.append(*offset, std::mem::take(pending)).await?;
        *offset += len as u64;
    }
    Ok(())
}

/// One attempt at the copy: the frames file, the manifest, and the directory entries, all synced.
async fn copy(
    ctx: &Ctx,
    batch: &BatchId,
    dir: &DiskPath,
    manifest_head: &str,
) -> Result<(), CopyError> {
    let disk = &ctx.env.disk;
    disk.create_dir_all(dir).await?;
    // An earlier copy's manifest must not describe the frames of this one while they are written.
    let manifest_path = dir.join(MANIFEST_FILE)?;
    write_file(ctx, &manifest_path, Vec::new()).await?;
    let file = disk.open(&dir.join(FRAMES_FILE)?, OpenMode::Write).await?;
    file.truncate(0).await?;
    let mut replay = ctx.wal.replay(batch.first_lsn);
    let mut offset = 0u64;
    let mut pending = Vec::new();
    let mut buffered = 0usize;
    let mut frames = 0u64;
    while replay.position() <= batch.last_lsn {
        let Some(frame) = replay.next().await? else {
            return Err(CopyError::Short(replay.position()));
        };
        let header = frame.header().encode().map_err(|_| CopyError::Header)?;
        buffered += header.len() + frame.stored_payload().len();
        pending.push(Bytes::copy_from_slice(&header));
        pending.push(Bytes::copy_from_slice(frame.stored_payload()));
        frames += 1;
        if buffered >= COPY_CHUNK {
            flush(&file, &mut offset, &mut pending).await?;
            buffered = 0;
        }
    }
    flush(&file, &mut offset, &mut pending).await?;
    file.sync_data().await?;
    let manifest = format!("{manifest_head}frames={frames}\nbytes={offset}\n");
    write_file(ctx, &manifest_path, vec![Bytes::from(manifest)]).await?;
    disk.sync_dir(dir).await?;
    disk.sync_dir(&DiskPath::root_dir(Root::Quarantine)).await?;
    Ok(())
}

/// Quarantines one pair. Returns once the `Quarantine` frame is durable. Disk and replay errors are retried
/// with backoff: the pair stays unresolved, and so does the watermark, until the copy is durable.
pub(crate) async fn quarantine_pair(
    ctx: &Arc<Ctx>,
    batch: &BatchId,
    pair: Pair,
    error_code: u32,
    attempts: u16,
    reason: &str,
) -> Result<(), ShipError> {
    let name = quarantine_ref(batch, pair)?;
    let dir = DiskPath::new(Root::Quarantine, &name).map_err(|e| ShipError::Frame {
        lsn: batch.first_lsn,
        reason: e.to_string(),
    })?;
    let one_line: String = reason
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let manifest_head = format!(
        "version=1\nwal_epoch={}\nfirst_lsn={}\nlast_lsn={}\ntable={}\ntable_id={}\npartition={}\ntoken={}\n\
         error_code={error_code}\nattempts={attempts}\nreason={one_line}\n",
        batch.epoch,
        batch.first_lsn,
        batch.last_lsn,
        pair.0.name(),
        pair.0.id(),
        pair.1,
        pair_token(batch, pair)?.text().as_str(),
    );
    let mut attempt = 0;
    while let Err(e) = copy(ctx, batch, &dir, &manifest_head).await {
        if let CopyError::Replay(ReplayError::Reclaimed { lsn, .. }) = e {
            return Err(ShipError::Reclaimed(lsn));
        }
        tracing::warn!(quarantine = %name, error = %e, "quarantine copy failed; retrying");
        let delay = backoff(
            ctx.config.backoff_min,
            ctx.config.backoff_max,
            attempt,
            ctx.rng.as_ref(),
        );
        ctx.env.clock.sleep(delay).await;
        attempt = attempt.saturating_add(1);
    }
    ctx.barriers.reached(Barrier::QuarantineCopied).await;

    let record = Record::Quarantine(Quarantine {
        first_lsn: batch.first_lsn,
        last_lsn: batch.last_lsn,
        pairs: vec![QuarantinePair {
            table_id: pair.0,
            partition_id: pair.1,
        }],
        error_code: i32::try_from(error_code).unwrap_or(i32::MAX),
        attempts,
        quarantine_ref: &name,
        reason,
    });
    ctx.append_durable(&record).await?;
    ctx.stats.quarantined.add(1);
    // The operator alert of contract 5.6.
    tracing::error!(
        quarantine = %name,
        table = pair.0.name(),
        error_code,
        attempts,
        "pair quarantined after repeated data errors"
    );
    Ok(())
}

/// The directory name of a frame's quarantine (contract 5.6).
pub fn frame_quarantine_ref(epoch: WalEpoch, lsn: Lsn) -> String {
    format!("frame-{epoch}-{lsn}")
}

/// What the manifest of a frame quarantine says, as the host reads it at start to mark the frames whose log
/// segments are gone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameManifest {
    /// The frame's LSN.
    pub lsn: Lsn,
    /// The frame's project, when its envelope decoded.
    pub project_id: Option<ProjectId>,
    /// Whether the frame is a `FactBatch`.
    pub facts: bool,
    /// The declared length of the frames file.
    pub bytes: u64,
}

impl FrameManifest {
    /// Parses a whole manifest of a frame quarantine. `None` for the manifest of a pair, and for one that a
    /// crash cut short.
    pub fn parse(text: &str) -> Option<Self> {
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        };
        if field("kind") != Some("frame") {
            return None;
        }
        Some(Self {
            lsn: Lsn::new(field("lsn")?.parse().ok()?),
            project_id: field("project_id").and_then(|id| ProjectId::parse(id).ok()),
            facts: field("record_type")? == "fact_batch",
            bytes: field("bytes")?.parse().ok()?,
        })
    }
}

/// One attempt at the copy of a frame. `Ok` once a whole manifest and the frame it describes are durable.
async fn copy_frame(
    ctx: &Ctx,
    dir: &DiskPath,
    frame: &WalFrame,
    manifest: &str,
) -> Result<(), CopyError> {
    let disk = &ctx.env.disk;
    disk.create_dir_all(dir).await?;
    let manifest_path = dir.join(MANIFEST_FILE)?;
    if let Ok(file) = disk.open(&manifest_path, OpenMode::Read).await {
        let held = file.read_at(0, manifest.len() + 1).await?;
        if held == manifest.as_bytes() {
            // A repeat after a crash: the copy is there.
            return Ok(());
        }
    }
    let header = frame.header().encode().map_err(|_| CopyError::Header)?;
    let chunks = vec![
        Bytes::copy_from_slice(&header),
        Bytes::copy_from_slice(frame.stored_payload()),
    ];
    write_file(ctx, &dir.join(FRAMES_FILE)?, chunks).await?;
    let chunks = vec![Bytes::copy_from_slice(manifest.as_bytes())];
    match disk.open(&manifest_path, OpenMode::CreateNew).await {
        Ok(file) => {
            file.append(0, chunks).await?;
            file.sync_data().await?;
        }
        // What a crash inside an earlier attempt left: a manifest that is not whole.
        Err(DiskError::AlreadyExists) => write_file(ctx, &manifest_path, chunks).await?,
        Err(e) => return Err(e.into()),
    }
    disk.sync_dir(dir).await?;
    disk.sync_dir(&DiskPath::root_dir(Root::Quarantine)).await?;
    Ok(())
}

/// Quarantines one frame that this binary cannot decode or normalize (contract 5.6). Returns once the copy is
/// durable; disk errors are retried with backoff, so the frame joins no batch, and the watermark stays below
/// it, until then.
pub(crate) async fn quarantine_frame(
    ctx: &Arc<Ctx>,
    frame: &WalFrame,
    why: &Unshippable,
) -> Result<(), ShipError> {
    let lsn = frame.lsn();
    let name = frame_quarantine_ref(ctx.wal.epoch(), lsn);
    let dir = DiskPath::new(Root::Quarantine, &name).map_err(|e| ShipError::Frame {
        lsn,
        reason: e.to_string(),
    })?;
    let one_line: String = crucible_records::truncate_str(
        &why.reason,
        crucible_wal_codec::MAX_QUARANTINE_REASON_BYTES,
    )
    .chars()
    .map(|c| if c.is_control() { ' ' } else { c })
    .collect();
    let project = why
        .project
        .map(|p| format!("project_id={p}\n"))
        .unwrap_or_default();
    let bytes = crucible_wal_codec::FRAME_HEADER_LEN + frame.stored_payload().len();
    let manifest = format!(
        "version=1\nkind=frame\nwal_epoch={}\nlsn={lsn}\nrecord_type={}\n{project}\
         normalizer_version={NORMALIZER_VERSION}\nstage={}\nreason={one_line}\nframes=1\nbytes={bytes}\n",
        ctx.wal.epoch(),
        if why.facts {
            "fact_batch"
        } else {
            "ingest_request"
        },
        why.stage,
    );
    let mut attempt = 0;
    while !ctx.config.quarantines_frames_without_copy() {
        let Err(e) = copy_frame(ctx, &dir, frame, &manifest).await else {
            break;
        };
        tracing::warn!(quarantine = %name, error = %e, "frame quarantine copy failed; retrying");
        let delay = backoff(
            ctx.config.backoff_min,
            ctx.config.backoff_max,
            attempt,
            ctx.rng.as_ref(),
        );
        ctx.env.clock.sleep(delay).await;
        attempt = attempt.saturating_add(1);
    }
    ctx.stats.quarantined_frames.add(1);
    // The operator alert of contract 5.6.
    tracing::error!(
        quarantine = %name,
        lsn = %lsn,
        stage = why.stage,
        reason = %one_line,
        "a durable frame does not normalize with this binary; it is quarantined and yields no rows"
    );
    Ok(())
}
