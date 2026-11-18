//! The life of one (batch, table, partition) pair: probe, insert, classify the outcome, back off, quarantine
//! (contract sections 5.5 and 5.6, and the rules the ingest model found, `docs/testing/formal-model.md`).

use std::sync::Arc;

use crucible_chfmt::EncodedBlock;
use crucible_env::{ClockExt, InsertError, InsertSettings, RetryReason};
use crucible_records::{BatchToken, TableId, WalEpoch};

use crate::batch::BatchId;
use crate::error::ShipError;
use crate::hooks::Barrier;
use crate::limits::backoff;
use crate::probe::batch_probe;
use crate::quarantine::quarantine_pair;
use crate::shipper::Ctx;

/// One pair to ship.
#[derive(Debug)]
pub(crate) struct PairJob {
    pub(crate) token: BatchToken,
    /// The compressed block, kept until the pair is resolved so that every retry sends identical bytes.
    pub(crate) block: EncodedBlock,
    /// An attempt of this pair may have landed: every pair of a batch recovered after a restart.
    pub(crate) in_doubt: bool,
}

/// How a pair ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PairEnd {
    /// The block is present: the insert was acknowledged, or a probe read present.
    Confirmed,
    /// The pair is named by a durable `Quarantine` frame.
    Quarantined,
    /// The store refuses the table's inserts for its configuration. The pair stays unresolved, so its batch is
    /// never checkpointed in this process and the WAL keeps it.
    Stopped,
}

/// The one data error that quarantines at once (contract 5.6): code 252 in the poison class is a block that spans
/// partitions. The store reports the too-many-parts form of 252 as a retry.
const MULTI_PARTITION_CODE: u32 = 252;

/// The data errors of one pair so far: the code and how many identical failures in a row.
#[derive(Clone, Copy, Debug)]
struct Poison {
    code: u32,
    count: u16,
    reason_len: usize,
}

async fn pause(ctx: &Ctx, attempt: u32) {
    let delay = backoff(
        ctx.config.backoff_min,
        ctx.config.backoff_max,
        attempt,
        ctx.rng.as_ref(),
    );
    ctx.env.clock.sleep(delay).await;
}

/// Probes until the store answers. A failed probe is never read as absent.
async fn probe(ctx: &Ctx, token: &BatchToken) -> Result<bool, ShipError> {
    let probe = batch_probe(token)?;
    let mut attempt = 0;
    loop {
        let answer = {
            let _connection = ctx.gate.acquire().await;
            ctx.stats.probes.add(1);
            ctx.stores.telemetry.probe(probe).await
        };
        match answer {
            Ok(present) => return Ok(present),
            Err(e) => {
                tracing::warn!(token = %token.text().as_str(), error = %e, "existence probe failed");
                pause(ctx, attempt).await;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// Ships one pair until it is resolved.
///
/// - **Once in doubt, always probe.** After an unknown outcome, or after a restart, every send is preceded by a
///   probe that reads absent. A later definite rejection does not clear the doubt: the earlier attempt may
///   still land.
/// - **Probe results expire.** A probe that read absent authorizes one send, and only within
///   `probe_valid_for` of the moment the probe was issued.
/// - **Quarantine after an absent probe.** A pair in doubt reaches quarantine only through the probe at the top
///   of the loop.
pub(crate) async fn ship_pair(
    ctx: &Arc<Ctx>,
    batch: &BatchId,
    job: PairJob,
) -> Result<PairEnd, ShipError> {
    let pair = (job.token.table(), job.token.partition());
    let mut in_doubt = job.in_doubt;
    let mut attempt: u32 = 0;
    let mut poison: Option<Poison> = None;
    let mut reason = String::new();
    let fact_table = matches!(
        pair.0,
        TableId::FactScores | TableId::FactTags | TableId::FactFlags | TableId::FactCorrections
    );
    loop {
        if attempt > 0 && fact_table && ctx.config.drops_fact_on_retry() {
            return Ok(PairEnd::Confirmed);
        }
        let poisoned = poison.filter(|p| p.count >= ctx.config.poison_limit);
        if let Some(p) = poisoned
            && ctx.config.quarantines_without_probe()
        {
            quarantine_pair(ctx, batch, pair, p.code, p.count, &reason[..p.reason_len]).await?;
            return Ok(PairEnd::Quarantined);
        }
        let mut probed_at = None;
        if in_doubt {
            let issued = ctx.env.clock.now();
            if probe(ctx, &job.token).await? {
                ctx.stats.probes_present.add(1);
                return Ok(PairEnd::Confirmed);
            }
            probed_at = Some(issued);
        }
        if let Some(p) = poisoned {
            quarantine_pair(ctx, batch, pair, p.code, p.count, &reason[..p.reason_len]).await?;
            return Ok(PairEnd::Quarantined);
        }

        let connection = ctx.gate.acquire().await;
        ctx.barriers
            .reached(Barrier::BeforeInsertSend(pair.0, pair.1))
            .await;
        let now = ctx.env.clock.now();
        if probed_at
            .is_some_and(|at| now.saturating_duration_since(at) >= ctx.config.probe_valid_for)
        {
            // The wait for a connection outlived the probe's answer.
            ctx.stats.probes_expired.add(1);
            continue;
        }
        let token = if ctx.config.drops_token() {
            let epoch = WalEpoch::from_uuid(ctx.env.new_id());
            BatchToken::new(epoch, batch.first_lsn, batch.last_lsn, pair.0, pair.1)?
        } else {
            job.token
        };
        let settings = InsertSettings::new(token, ctx.config.flavour);
        ctx.stats.inserts.add(1);
        let outcome = ctx
            .env
            .clock
            .timeout(
                ctx.config.insert_timeout,
                ctx.stores
                    .telemetry
                    .insert_block(job.block.clone(), settings),
            )
            .await;
        let latency = ctx.env.clock.now().saturating_duration_since(now);
        drop(connection);
        ctx.barriers
            .reached(Barrier::InsertSent(pair.0, pair.1))
            .await;

        let mut known_rejection = false;
        match outcome {
            Ok(Ok(())) => {
                ctx.gate.grow();
                ctx.with_limits(|limits| {
                    if latency >= ctx.config.pressure_latency {
                        limits.widen();
                    } else {
                        limits.relax();
                    }
                });
                return Ok(PairEnd::Confirmed);
            }
            // A timeout, a dropped call or an unclassified failure: the block may have landed.
            Err(_) | Ok(Err(InsertError::UnknownOutcome { .. })) => {
                ctx.stats.unknown_outcomes.add(1);
                if !ctx.config.skips_probe_after_unknown() {
                    in_doubt = true;
                }
            }
            Ok(Err(InsertError::Retry { reason, message })) => {
                known_rejection = true;
                tracing::debug!(token = %job.token.text().as_str(), ?reason, %message, "insert not applied");
                match reason {
                    RetryReason::TooManyParts => ctx.with_limits(|limits| limits.widen()),
                    RetryReason::Memory => ctx.gate.halve(),
                    _ => {}
                }
            }
            Ok(Err(InsertError::FatalConfig { code, message })) => {
                // The operator alert of contract 5.6. The WAL keeps the batch; a restart ships it.
                tracing::error!(
                    table = pair.0.name(),
                    code,
                    %message,
                    "the store refuses inserts for its configuration; the batch stays unresolved until a restart"
                );
                return Ok(PairEnd::Stopped);
            }
            Ok(Err(InsertError::Poison { code, message })) => {
                known_rejection = true;
                let count = if code == MULTI_PARTITION_CODE {
                    // A routing bug: no retry can cure it.
                    ctx.config.poison_limit
                } else {
                    poison
                        .filter(|p| p.code == code)
                        .map_or(1, |p| p.count.saturating_add(1))
                };
                reason = message;
                poison = Some(Poison {
                    code,
                    count,
                    reason_len: crucible_records::truncate_str(
                        &reason,
                        crucible_wal_codec::MAX_QUARANTINE_REASON_BYTES,
                    )
                    .len(),
                });
            }
        }
        if known_rejection && ctx.config.rejection_clears_doubt() {
            in_doubt = false;
        }
        if poison.is_none_or(|p| p.count < ctx.config.poison_limit) {
            pause(ctx, attempt).await;
        }
        attempt = attempt.saturating_add(1);
    }
}
