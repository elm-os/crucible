//! What every node does for retention and deletion (contract 13.3, steps 1 to 4): a per-node timer that
//! heartbeats in the node registry, refreshes the pending-deletion set and the retention snapshot, appends a
//! `Tombstone` frame to the node's own WAL for every request it has not recorded yet, and reports when its
//! checkpoint has passed that tombstone.
//!
//! The executor acts on a request only after every live node has reported (step 5). A node is live while its
//! heartbeat is at most 60 s old on the database clock, so a node that stops does not hold a request up for
//! longer than that.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use crucible_control::{ControlStore, DeletionRequest, Node, NodeAck, TxError};
use crucible_jobs::{Cadence, Job, JobError, JobResult, NodeCtx};
use crucible_records::{DeletionRequestId, DeletionState, Lsn, WalEpoch};

use crate::config::LifecycleConfig;
use crate::pending::{PendingDeletions, RetentionSnapshot};

/// Why a tombstone could not be made durable. The request stays pending and the node tries again.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("the WAL cannot take the tombstone: {0}")]
pub struct LogError(pub String);

/// The node's log as the lifecycle sees it: the WAL of contract 4, or a model of it in simulation.
#[async_trait]
pub trait NodeLog: fmt::Debug + Send + Sync {
    /// The epoch of the node's WAL directory.
    fn epoch(&self) -> WalEpoch;
    /// Appends the `Tombstone` frame of a request and resolves with its LSN once the frame is durable
    /// (contract 4.5: the ack rule).
    async fn append_tombstone(&self, request: &DeletionRequest) -> Result<Lsn, LogError>;
    /// The checkpoint watermark: every frame at or below it is resolved.
    fn watermark(&self) -> Lsn;
}

/// What one node holds for retention and deletion.
pub struct NodeState {
    log: Arc<dyn NodeLog>,
    pending: Arc<PendingDeletions>,
    retention: Arc<RetentionSnapshot>,
    version: String,
    address: String,
    /// The tombstones this process made durable, by request: a tombstone whose acknowledgement Postgres did
    /// not take is recorded again from here instead of being appended twice.
    appended: Mutex<BTreeMap<DeletionRequestId, Lsn>>,
}

impl fmt::Debug for NodeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeState")
            .field("epoch", &self.log.epoch())
            .field("pending", &self.pending.len())
            .finish_non_exhaustive()
    }
}

impl NodeState {
    /// The state of a node over its log. `version` and `address` go into the node registry.
    pub fn new(log: Arc<dyn NodeLog>, version: &str, address: &str) -> Self {
        Self {
            log,
            pending: Arc::default(),
            retention: Arc::default(),
            version: version.to_owned(),
            address: address.to_owned(),
            appended: Mutex::default(),
        }
    }

    /// The node's pending-deletion set.
    pub fn pending(&self) -> &Arc<PendingDeletions> {
        &self.pending
    }

    /// The node's retention snapshot.
    pub fn retention(&self) -> &Arc<RetentionSnapshot> {
        &self.retention
    }

    /// The epoch of the node's WAL directory.
    pub fn epoch(&self) -> WalEpoch {
        self.log.epoch()
    }

    /// Makes the request's tombstone durable in this node's WAL, then records its LSN in
    /// `deletion_node_acks`. Idempotent: a tombstone this process already appended is not appended again, and
    /// an acknowledgement that Postgres already holds is kept.
    pub async fn acknowledge(
        &self,
        control: &dyn ControlStore,
        request: &DeletionRequest,
    ) -> Result<(), AckError> {
        let known = self.appended().get(&request.id).copied();
        let lsn = match known {
            Some(lsn) => lsn,
            None => {
                let lsn = self.log.append_tombstone(request).await?;
                self.appended().insert(request.id, lsn);
                lsn
            }
        };
        let ack = NodeAck {
            request_id: request.id,
            wal_epoch: self.log.epoch(),
            tombstone_lsn: lsn,
            passed_at: None,
        };
        control.lifecycle().record_ack(&ack).await?;
        Ok(())
    }

    fn appended(&self) -> std::sync::MutexGuard<'_, BTreeMap<DeletionRequestId, Lsn>> {
        self.appended.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One pass of the node timer.
    async fn tick(
        &self,
        control: &dyn ControlStore,
        now: crucible_records::Timestamp,
    ) -> JobResult {
        let repo = control.lifecycle();
        let epoch = self.log.epoch();
        repo.heartbeat(&Node {
            wal_epoch: epoch,
            heartbeat_at: now,
            version: self.version.clone(),
            checkpoint_lsn: self.log.watermark(),
            address: self.address.clone(),
        })
        .await?;
        let open = repo.open_requests().await?;
        self.pending.replace(&open, now);
        let mut acks: BTreeMap<DeletionRequestId, NodeAck> = repo
            .node_acks(epoch)
            .await?
            .into_iter()
            .map(|ack| (ack.request_id, ack))
            .collect();
        let mut first_error = None;
        for request in &open {
            let logs = matches!(
                request.state,
                DeletionState::Pending | DeletionState::Logged
            );
            if !logs || acks.contains_key(&request.id) {
                continue;
            }
            match self.acknowledge(control, request).await {
                Ok(()) => {
                    if let Some(lsn) = self.appended().get(&request.id).copied() {
                        acks.insert(
                            request.id,
                            NodeAck {
                                request_id: request.id,
                                wal_epoch: epoch,
                                tombstone_lsn: lsn,
                                passed_at: None,
                            },
                        );
                    }
                }
                Err(error) => first_error = first_error.or(Some(error)),
            }
        }
        // Step 4: the checkpoint watermark is at or above the tombstone.
        let watermark = self.log.watermark();
        for ack in acks.values() {
            if ack.passed_at.is_none() && watermark >= ack.tombstone_lsn {
                repo.mark_passed(ack.request_id, epoch, now).await?;
            }
        }
        // What this process appended is only needed until Postgres holds the acknowledgement.
        self.appended()
            .retain(|id, _| open.iter().any(|r| r.id == *id));
        self.retention.refresh(control).await?;
        match first_error {
            Some(AckError::Log(error)) => Err(JobError::Failed(error.to_string())),
            Some(AckError::Store(error)) => Err(JobError::Store(error)),
            None => Ok(()),
        }
    }
}

/// Why a node could not acknowledge a request.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum AckError {
    /// The tombstone is not durable.
    #[error(transparent)]
    Log(#[from] LogError),
    /// The tombstone is durable, and Postgres did not take its acknowledgement yet.
    #[error(transparent)]
    Store(#[from] TxError),
}

/// The per-node timer of contract 13.3: every `node_every` (5 s). It needs Postgres; without it the timer does
/// nothing.
pub fn node_job(state: Arc<NodeState>, config: &LifecycleConfig) -> Job {
    Job::per_node(
        "lifecycle-node",
        Cadence::every(config.node_every),
        move |ctx: NodeCtx| {
            let state = Arc::clone(&state);
            async move {
                let Some(control) = ctx.control.as_deref() else {
                    return Ok(());
                };
                state.tick(control, ctx.env.clock.wall_now()).await
            }
        },
    )
}
