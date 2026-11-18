//! The claimed-work runner (contract 11.7): claims the due rows of one queue, runs the handler for each, and
//! records the outcome: done, due again after a backoff, or parked.

use std::sync::Arc;

use crucible_control::{Claim, ControlStore, LEASE_SAFETY_MARGIN, Queue, duration_micros};
use crucible_env::{ClockExt, Env, Instant, Rng};
use crucible_records::Timestamp;
use futures::{FutureExt, select_biased};

use crate::cadence::{Cadence, backoff};
use crate::job::{ClaimCtx, ClaimPolicy, Run};

/// The runner of one queue consumer.
pub(crate) struct ClaimRunner {
    pub(crate) env: Env,
    pub(crate) store: Arc<dyn ControlStore>,
    pub(crate) name: &'static str,
    pub(crate) queue: Queue,
    pub(crate) cadence: Cadence,
    pub(crate) policy: ClaimPolicy,
    pub(crate) run: Run<ClaimCtx>,
}

impl ClaimRunner {
    /// Runs until shutdown. A handler in flight at shutdown is dropped; its row's claim expires and another
    /// node claims it.
    pub(crate) async fn run(self) {
        let rng = self
            .env
            .rng
            .fork(&format!("crucible-jobs/claimed/{}", self.name));
        let mut shutdown = self.env.shutdown().fuse();
        loop {
            select_biased! {
                () = shutdown => return,
                () = self.drain(rng.as_ref()).fuse() => {}
            }
            let pause = self.cadence.next_delay(rng.as_ref());
            select_biased! {
                () = shutdown => return,
                () = self.env.clock.sleep(pause).fuse() => {}
            }
        }
    }

    /// Claims and handles one row at a time until none is due.
    async fn drain(&self, rng: &dyn Rng) {
        loop {
            // Read before the call, so the handler's deadline is ahead of the claim's end in the database.
            let asked = self.env.clock.now();
            let claimed = self
                .store
                .claims()
                .claim(self.queue, 1, self.policy.lease_for)
                .await;
            match claimed {
                Ok(claims) if claims.is_empty() => return,
                Ok(claims) => {
                    for claim in claims {
                        self.handle(claim, asked, rng).await;
                    }
                }
                Err(error) => {
                    tracing::debug!(job = self.name, %error, "claiming failed; trying again at the next cadence");
                    return;
                }
            }
        }
    }

    async fn handle(&self, claim: Claim, asked: Instant, rng: &dyn Rng) {
        // The counter passed the cap without an answer from the earlier attempts: they died mid-run.
        if claim.attempt > self.policy.max_attempts {
            return self
                .park(&claim, "its earlier attempts never answered")
                .await;
        }
        let deadline =
            asked.saturating_add(self.policy.lease_for.saturating_sub(LEASE_SAFETY_MARGIN));
        let ctx = ClaimCtx {
            env: self.env.clone(),
            store: Arc::clone(&self.store),
            claim: claim.clone(),
        };
        let reason = match self.env.clock.timeout_at(deadline, (self.run)(ctx)).await {
            Ok(Ok(())) => {
                match self.store.claims().complete(&claim).await {
                    Ok(true) => {}
                    Ok(false) => tracing::debug!(
                        job = self.name, id = %claim.id, attempt = claim.attempt,
                        "the row was claimed again before this attempt completed it"
                    ),
                    // Applied or not, there is nothing to undo: an unrecorded completion lets the claim
                    // expire, and the handler, which is idempotent, runs again.
                    Err(error) => tracing::debug!(
                        job = self.name, id = %claim.id, %error, "recording the completion failed"
                    ),
                }
                return;
            }
            Ok(Err(error)) => error.to_string(),
            Err(_) => "the handler ran until its claim was about to end".to_owned(),
        };
        if claim.attempt >= self.policy.max_attempts {
            return self.park(&claim, &reason).await;
        }
        let delay = backoff(claim.attempt, rng);
        let retry_at = self
            .db_now(&claim, asked)
            .saturating_add_micros(duration_micros(delay));
        tracing::debug!(
            job = self.name, id = %claim.id, attempt = claim.attempt, reason, retry_in = ?delay,
            "the attempt failed"
        );
        if let Err(error) = self.store.claims().fail(&claim, retry_at).await {
            tracing::debug!(job = self.name, id = %claim.id, %error, "recording the failure failed");
        }
    }

    /// The database clock now, from the claim: its `lease_until` is the database clock at the claim plus the
    /// lease, and the node's monotonic clock gives the time since. A node's wall clock is never compared
    /// with the database's.
    fn db_now(&self, claim: &Claim, asked: Instant) -> Timestamp {
        let since = self.env.clock.now().saturating_duration_since(asked);
        claim
            .lease_until
            .saturating_add_micros(-duration_micros(self.policy.lease_for))
            .saturating_add_micros(duration_micros(since))
    }

    async fn park(&self, claim: &Claim, reason: &str) {
        match self.store.claims().park(claim).await {
            Ok(true) => tracing::warn!(
                job = self.name, queue = %self.queue, id = %claim.id, attempts = claim.attempt, reason,
                "the row reached the attempt cap and is parked; it stays in the table and is not retried"
            ),
            Ok(false) => {}
            Err(error) => tracing::debug!(
                job = self.name, id = %claim.id, %error, "parking the row failed; its claim expires"
            ),
        }
    }
}
