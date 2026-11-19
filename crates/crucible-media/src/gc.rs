//! Garbage collection under the `media_gc` lease (contracts 13.4 and 14.4): one leader job, `media-gc`, that
//! runs hourly.
//!
//! Each run, in order:
//!
//! 1. **Retention.** Uses whose target time is before their project's payload cut-off are deleted
//!    (`payload_retention_days`, capped by `retention_days`, aligned to UTC days as the retention executor
//!    aligns them).
//! 2. **Stale uploads.** Pending objects asked for more than 24 h ago lose their row and their uses, then the
//!    stored object, which an upload may have left without a confirm.
//! 3. **Unused objects.** Uploaded objects that no use refers to lose their row, then the stored object.
//! 4. **Orphans.** Once a week, every key under the deployment's prefix is listed, a page at a time, and the
//!    objects older than 48 h whose media id has no row are deleted. The scan's position is the job cursor,
//!    so a scan that a run could not finish goes on at the next.
//!
//! A row is deleted in a fenced transaction and its object only once that has committed: an object is never
//! deleted while a row could still lead a reader to it. A crash in between leaves an object without a row,
//! which the next orphan scan removes. A use recorded while its object's row is being deleted makes one of the
//! two transactions fail (the foreign key), and the object is kept.

use std::sync::Arc;
use std::time::Duration;

use crucible_control::{LeaseName, TxError};
use crucible_env::ObjectStore;
use crucible_jobs::{Cadence, Job, JobError, LeaderCtx};
use crucible_records::MediaObjectId;
use crucible_schema::{RetentionBinding, RetentionSettings, retention_cutoff};
use serde::{Deserialize, Serialize};

use crate::rules::{ORPHAN_AGE, PENDING_TTL, key_root, media_id_of, object_key};

/// How often the job runs.
pub const GC_EVERY: Duration = Duration::from_secs(3_600);
/// How often the orphan scan starts.
pub const ORPHAN_SCAN_EVERY: Duration = Duration::from_secs(7 * 24 * 3_600);
/// Rows one batch of steps 2 and 3 looks at.
const BATCH: u32 = 500;
/// Batches one run takes per step, and listing pages per run: a large backlog is worked off over several runs.
const MAX_BATCHES: usize = 20;
/// The most keys one batch delete names.
const DELETE_BATCH: usize = 1_000;

/// Which rows steps 2 and 3 collect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sweep {
    /// Pending objects asked for more than 24 h ago.
    Stale,
    /// Uploaded objects that no use refers to.
    Unused,
}

/// The job cursor: where the orphan scan stands.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
struct Cursor {
    /// When the last finished scan started, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scanned_at: Option<i64>,
    /// When the scan in progress started, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan_started_at: Option<i64>,
    /// The last key the scan in progress has looked at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scan_after: Option<String>,
}

/// What one run did, for its log line and for the simulation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Collected {
    /// Uses deleted under retention.
    pub expired_uses: u64,
    /// Stale pending objects removed.
    pub stale: u64,
    /// Unused uploaded objects removed.
    pub unused: u64,
    /// Objects without a row deleted by the orphan scan.
    pub orphans: u64,
}

/// The collector.
#[derive(Clone, Debug)]
pub struct Gc {
    objects: Arc<dyn ObjectStore>,
    prefix: String,
}

/// The `media-gc` leader job over `objects`, whose keys start with `prefix`.
pub fn job(objects: Arc<dyn ObjectStore>, prefix: String) -> Job {
    let gc = Gc { objects, prefix };
    Job::leader(
        "media-gc",
        LeaseName::MediaGc,
        Cadence::every(GC_EVERY),
        move |ctx: LeaderCtx| {
            let gc = gc.clone();
            async move {
                let collected = gc.run(&ctx).await?;
                if collected != Collected::default() {
                    tracing::info!(
                        job = "media-gc",
                        expired_uses = collected.expired_uses,
                        stale = collected.stale,
                        unused = collected.unused,
                        orphans = collected.orphans,
                        "media collected"
                    );
                }
                Ok(())
            }
        },
    )
}

fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

/// Whether a fenced write lost to another writer of the same row: the row is left for a later run.
fn conflict(error: &JobError) -> bool {
    matches!(
        error,
        JobError::Store(TxError::SerializationFailure | TxError::ForeignKeyViolation(_))
    )
}

impl Gc {
    /// A collector over `objects`, whose keys start with `prefix`.
    pub fn new(objects: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { objects, prefix }
    }

    /// One run: the four steps of the module documentation.
    pub async fn run(&self, ctx: &LeaderCtx) -> Result<Collected, JobError> {
        let mut collected = Collected {
            expired_uses: self.expire(ctx).await?,
            ..Collected::default()
        };
        collected.stale = self.collect(ctx, Sweep::Stale).await?;
        collected.unused = self.collect(ctx, Sweep::Unused).await?;
        collected.orphans = self.orphans(ctx).await?;
        Ok(collected)
    }

    /// Step 1: the uses past their project's payload cut-off.
    async fn expire(&self, ctx: &LeaderCtx) -> Result<u64, JobError> {
        let now = ctx.env.clock.wall_now();
        let mut deleted = 0;
        for project in ctx.store().projects().list().await? {
            let settings = RetentionSettings {
                retention_days: project.retention_days,
                payload_retention_days: project.payload_retention_days,
                mcp_content_retention_days: project.mcp_content_retention_days,
            };
            let Some(days) = settings.days(RetentionBinding::PayloadRetentionDays) else {
                continue;
            };
            let Ok(cutoff) = retention_cutoff(now, days) else {
                continue;
            };
            let (project_id, cutoff) = (project.id, cutoff.start());
            deleted += ctx
                .fenced(move |tx| {
                    Box::pin(async move { tx.media().delete_refs_before(project_id, cutoff).await })
                })
                .await?;
        }
        Ok(deleted)
    }

    /// Steps 2 and 3: each object the sweep finds loses its row in a fenced transaction of its own, then the
    /// stored objects go in batches.
    async fn collect(&self, ctx: &LeaderCtx, sweep: Sweep) -> Result<u64, JobError> {
        let before = ctx
            .env
            .clock
            .wall_now()
            .saturating_add_micros(-micros(PENDING_TTL));
        let mut removed = 0;
        for _ in 0..MAX_BATCHES {
            let media = ctx.store().media();
            let found = match sweep {
                Sweep::Stale => media.stale_pending(before, BATCH).await?,
                Sweep::Unused => media.unreferenced(BATCH).await?,
            };
            let mut keys = Vec::new();
            for object in &found {
                let id = object.id;
                let deleted = ctx
                    .fenced(move |tx| {
                        Box::pin(async move {
                            match sweep {
                                Sweep::Stale => tx.media().delete_pending(id, before).await,
                                Sweep::Unused => tx.media().delete_unreferenced(id).await,
                            }
                        })
                    })
                    .await;
                match deleted {
                    Ok(true) => keys.push(object_key(&self.prefix, object.project_id, id)),
                    Ok(false) => {}
                    Err(error) if conflict(&error) => {
                        tracing::debug!(media = %id, %error, "a media row changed while it was collected; it is kept");
                    }
                    Err(error) => return Err(error),
                }
            }
            removed += keys.len() as u64;
            self.delete(&keys).await;
            if found.len() < BATCH as usize || keys.is_empty() {
                break;
            }
        }
        Ok(removed)
    }

    /// Deletes stored objects whose rows are gone. A failure leaves them to the orphan scan.
    async fn delete(&self, keys: &[String]) {
        for chunk in keys.chunks(DELETE_BATCH) {
            if let Err(error) = self.objects.delete_batch(chunk).await {
                tracing::warn!(
                    job = "media-gc",
                    %error,
                    objects = chunk.len(),
                    "deleting media objects failed; the weekly orphan scan deletes them later"
                );
            }
        }
    }

    /// Step 4: the orphan scan, when one is due or in progress.
    async fn orphans(&self, ctx: &LeaderCtx) -> Result<u64, JobError> {
        let now = ctx.env.clock.wall_now();
        let mut cursor: Cursor = ctx
            .cursor()
            .await?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let started = match cursor.scan_started_at {
            Some(started) => started,
            None => {
                let due = cursor
                    .scanned_at
                    .is_none_or(|at| now.micros().saturating_sub(at) >= micros(ORPHAN_SCAN_EVERY));
                if !due {
                    return Ok(0);
                }
                now.micros()
            }
        };
        let old = now.saturating_add_micros(-micros(ORPHAN_AGE));
        let root = key_root(&self.prefix);
        let mut deleted = 0;
        for _ in 0..MAX_BATCHES {
            let page = self
                .objects
                .list(&root, cursor.scan_after.as_deref())
                .await
                .map_err(|error| {
                    JobError::Failed(format!("listing media objects failed: {error}"))
                })?;
            let candidates: Vec<(String, MediaObjectId)> = page
                .objects
                .iter()
                .filter(|object| object.created_at < old)
                .filter_map(|object| {
                    media_id_of(&self.prefix, &object.key).map(|(_, id)| (object.key.clone(), id))
                })
                .collect();
            let ids: Vec<MediaObjectId> = candidates.iter().map(|(_, id)| *id).collect();
            let existing = ctx.store().media().existing(&ids).await?;
            let orphans: Vec<String> = candidates
                .into_iter()
                .filter(|(_, id)| existing.binary_search(id).is_err())
                .map(|(key, _)| key)
                .collect();
            deleted += orphans.len() as u64;
            self.delete(&orphans).await;
            match page.next {
                Some(next) => {
                    cursor.scan_started_at = Some(started);
                    cursor.scan_after = Some(next);
                }
                None => {
                    cursor = Cursor {
                        scanned_at: Some(started),
                        scan_started_at: None,
                        scan_after: None,
                    };
                }
            }
            let text = serde_json::to_string(&cursor)
                .map_err(|error| JobError::Failed(error.to_string()))?;
            ctx.set_cursor(&text).await?;
            if cursor.scan_started_at.is_none() {
                break;
            }
        }
        Ok(deleted)
    }
}
