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

use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use crucible_env::{DiskError, DiskPath, File, OpenMode, Root};
use crucible_records::Lsn;
use crucible_wal::ReplayError;
use crucible_wal_codec::{Quarantine, QuarantinePair, Record};

use crate::batch::{BatchId, Pair, pair_token};
use crate::error::ShipError;
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
    write_file(ctx, &dir.join(MANIFEST_FILE)?, vec![Bytes::from(manifest)]).await?;
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
    ctx.stats.quarantined.fetch_add(1, Ordering::Relaxed);
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
