//! The identity reconciler (contract 7.4): a leader job under the `sql_identities` lease that brings the
//! ClickHouse identities to what `ch_sql_identities` and the projects say.
//!
//! For every identity row, in project order:
//!
//! - the project is deleted, or the identity has been idle for 30 days: mark it `dropping`, deprovision
//!   (revoke, drop user, drop policies), delete the row;
//! - `pending`: provision it (policies, user, grant) and mark it `active`;
//! - `active` and its retention differs from the project's: rewrite its policies, so retention hides rows from
//!   SQL at once;
//! - `active` and `target_generation` is ahead: provision with the new generation's password (rotation);
//! - `active` and not confirmed for an hour: provision again, which repairs drift.
//!
//! While this node's self-probe has not passed, the pass also provisions the two probe identities again (contract
//! 7.5), with the passwords of this node's master key: a start with another key can have left them with that
//! key's passwords, and then the probe cannot sign in.
//!
//! Every ClickHouse statement is idempotent and every row change is a compare-and-set on the state and the
//! generation, so a leader that lost its lease mid-run, or a first use on another node, cannot leave an
//! identity that reads more than its project. Passwords are never stored: the store derives them from the
//! master key, the user name and the generation.
//!
//! The job also deletes `sql_runs` rows older than 30 days.

use std::time::Duration;

use crucible_control::{
    HeldLease, LEASE_RENEW_EVERY, LEASE_TTL, LeaseName, SqlIdentity, SqlIdentityState, TxError,
};
use crucible_env::SqlError;
use crucible_records::Timestamp;
use crucible_schema::{PROBE_PROJECTS, RetentionSettings};
use futures::{FutureExt as _, select_biased};

use crate::service::{SqlWorkspace, retention_settings};

/// How often the reconciler runs.
pub const RECONCILE_EVERY: Duration = Duration::from_secs(300);
/// An identity unused for this long is dropped; the next use provisions it again.
pub const IDLE_DROP: Duration = Duration::from_secs(30 * 24 * 3600);
/// An active identity is provisioned again when it was last confirmed this long ago.
pub const CONFIRM_EVERY: Duration = Duration::from_secs(3600);
/// `sql_runs` rows are kept this long.
pub const RUNS_KEPT: Duration = Duration::from_secs(30 * 24 * 3600);

/// What one reconciler pass did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ReconcileReport {
    /// Whether this node held the lease. `false` means another node is the leader and nothing was done.
    pub leader: bool,
    /// Identities provisioned for the first time.
    pub provisioned: u32,
    /// Identities whose policies were rewritten for a retention change.
    pub rewritten: u32,
    /// Identities whose password generation was raised.
    pub rotated: u32,
    /// Identities provisioned again as a periodic confirmation.
    pub confirmed: u32,
    /// Identities removed.
    pub dropped: u32,
    /// Probe identities provisioned again because this node's self-probe had not passed.
    pub probes: u32,
    /// Identities whose step failed and will be tried again next pass.
    pub failed: u32,
    /// Old `sql_runs` rows deleted.
    pub runs_purged: u64,
}

/// Why a pass stopped early.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum ReconcileError {
    /// The control plane failed.
    #[error(transparent)]
    Control(#[from] TxError),
    /// The lease was lost during the pass; another node continues.
    #[error("the sql_identities lease was lost during the pass")]
    LeaseLost,
}

fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

fn older_than(at: Timestamp, now: Timestamp, d: Duration) -> bool {
    now.micros().saturating_sub(at.micros()) >= micros(d)
}

enum Step {
    Provisioned,
    Rewritten,
    Rotated,
    Confirmed,
    Dropped,
    Nothing,
}

impl SqlWorkspace {
    /// Requests a password rotation of every identity: the reconciler provisions each with the next
    /// generation. Returns how many identities will rotate.
    pub async fn rotate_identities(&self) -> Result<u64, TxError> {
        self.control().sql_identities().rotate_all().await
    }

    async fn reconcile_identity(&self, identity: &SqlIdentity) -> Result<Step, StepError> {
        let repo = self.control().sql_identities();
        let now = self.env().clock.wall_now();
        let project_id = identity.project_id;
        let project = self
            .control()
            .projects()
            .get(project_id)
            .await?
            .filter(|project| project.deleted_at.is_none());
        let idle = older_than(identity.last_used_at, now, IDLE_DROP);
        let drop_it = identity.state == SqlIdentityState::Dropping || project.is_none() || idle;
        if drop_it {
            if identity.state != SqlIdentityState::Dropping {
                let dropping = SqlIdentity {
                    state: SqlIdentityState::Dropping,
                    ..identity.clone()
                };
                // A use that raced the idle check keeps the identity: `touch` moved `last_used_at`, and the
                // next pass sees it. A lost compare-and-set means the row changed; leave it for the next pass.
                let current = repo.get(project_id).await?;
                let still_idle = current
                    .as_ref()
                    .is_some_and(|row| older_than(row.last_used_at, now, IDLE_DROP));
                if project.is_some() && !still_idle {
                    return Ok(Step::Nothing);
                }
                if !repo
                    .update_if(&dropping, identity.state, identity.generation)
                    .await?
                {
                    return Ok(Step::Nothing);
                }
            }
            self.store().deprovision(project_id).await?;
            repo.delete_dropping(project_id).await?;
            return Ok(Step::Dropped);
        }
        let Some(project) = project else {
            return Ok(Step::Nothing);
        };
        let wanted = crucible_control::AppliedRetention {
            retention_days: project.retention_days,
            payload_retention_days: project.payload_retention_days,
            mcp_content_retention_days: project.mcp_content_retention_days,
        };
        let generation = identity.target_generation.max(identity.generation);
        let stale = identity
            .last_probe
            .is_none_or(|at| older_than(at, now, CONFIRM_EVERY));
        let step = if identity.state == SqlIdentityState::Pending {
            Step::Provisioned
        } else if generation != identity.generation {
            Step::Rotated
        } else if identity.retention != wanted {
            Step::Rewritten
        } else if stale {
            Step::Confirmed
        } else {
            return Ok(Step::Nothing);
        };
        self.store()
            .provision(project_id, generation, &retention_settings(wanted))
            .await?;
        let active = SqlIdentity {
            generation,
            target_generation: generation,
            state: SqlIdentityState::Active,
            retention: wanted,
            last_probe: Some(now),
            ..identity.clone()
        };
        repo.update_if(&active, identity.state, identity.generation)
            .await?;
        Ok(step)
    }

    /// One pass under the `sql_identities` lease. Another node holding the lease is not an error: the report
    /// says this node is not the leader.
    pub async fn reconcile_once(&self, holder: &str) -> Result<ReconcileReport, ReconcileError> {
        let control = self.control();
        let clock = &self.env().clock;
        let mut report = ReconcileReport::default();
        let asked = clock.now();
        let Some(lease) = control
            .leases()
            .acquire(LeaseName::SqlIdentities, holder, LEASE_TTL)
            .await?
        else {
            return Ok(report);
        };
        report.leader = true;
        let mut held = HeldLease::new(lease, asked.nanos(), LEASE_TTL);
        let mut renewed = asked;
        if !self.status().enabled {
            for project in PROBE_PROJECTS {
                match self
                    .store()
                    .provision(project, 0, &RetentionSettings::default())
                    .await
                {
                    Ok(()) => report.probes += 1,
                    Err(error) => {
                        report.failed += 1;
                        tracing::warn!(project_id = %project, %error, "provisioning a probe identity failed");
                    }
                }
            }
        }
        let identities = control.sql_identities().list().await?;
        for identity in &identities {
            // The holder acts only inside its local window, and renews well before it ends.
            let now = clock.now();
            if now.saturating_duration_since(renewed) >= LEASE_RENEW_EVERY {
                let lease = control
                    .leases()
                    .renew(&held.lease, LEASE_TTL)
                    .await?
                    .ok_or(ReconcileError::LeaseLost)?;
                held = HeldLease::new(lease, now.nanos(), LEASE_TTL);
                renewed = now;
            }
            if !held.is_valid_at(clock.now().nanos()) {
                return Err(ReconcileError::LeaseLost);
            }
            match self.reconcile_identity(identity).await {
                Ok(Step::Provisioned) => report.provisioned += 1,
                Ok(Step::Rewritten) => report.rewritten += 1,
                Ok(Step::Rotated) => report.rotated += 1,
                Ok(Step::Confirmed) => report.confirmed += 1,
                Ok(Step::Dropped) => report.dropped += 1,
                Ok(Step::Nothing) => {}
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(project_id = %identity.project_id, %error, "reconciling a SQL identity failed");
                }
            }
        }
        let cutoff = clock.wall_now().saturating_add_micros(-micros(RUNS_KEPT));
        report.runs_purged = control.sql_runs().delete_before(cutoff).await?;
        // Best effort: an unreleased lease expires by itself.
        let _ = control.leases().release(&held.lease).await;
        Ok(report)
    }

    /// Runs a pass at startup and then every five minutes, until the host shuts down. A first use never waits
    /// for it: the workspace provisions on demand, and this job repairs, rotates and removes. The shutdown signal
    /// ends it at once; a pass it cuts leaves its lease to expire (contract 11.7), and every step of a pass is
    /// idempotent.
    pub async fn run_reconciler(self, holder: String) {
        let env = self.env().clone();
        let mut shutdown = env.shutdown().fuse();
        loop {
            let pass = select_biased! {
                () = shutdown => return,
                pass = self.reconcile_once(&holder).fuse() => pass,
            };
            match pass {
                Ok(report) if report.leader => {
                    tracing::debug!(?report, "SQL identity reconciler pass")
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "SQL identity reconciler pass failed"),
            }
            select_biased! {
                () = shutdown => return,
                () = env.clock.sleep(RECONCILE_EVERY).fuse() => {}
            }
        }
    }
}

/// A failed step on one identity.
#[derive(Debug, thiserror::Error)]
enum StepError {
    #[error(transparent)]
    Control(#[from] TxError),
    #[error(transparent)]
    Store(#[from] SqlError),
}
