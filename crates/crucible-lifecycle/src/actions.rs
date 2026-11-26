//! The ledger of leader actions on ClickHouse (contract 11.7 and 13.2): every delete, partition drop and mask
//! application is a row of `lifecycle_actions`, recorded as issued under the fencing token before it is sent.
//!
//! [`advance`] moves every open action one step: it issues what is planned, polls what is issued, marks what
//! a mask application covered and verifies what was masked. A step is one fenced transaction, and the store
//! call between two steps is idempotent, so a leader that stops anywhere leaves a row the next leader
//! continues from. Mutations of one table and partition never overlap: the store names a mutation by its
//! table and partition only (contract 8.9), so the next action of a pair waits for the one before it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crucible_auth::stored_project_scope;
use crucible_control::{ActionKind, ActionState, LifecycleAction, TxError};
use crucible_env::{MutationId, MutationStatus, StoreError, TelemetryStore};
use crucible_jobs::{JobError, JobResult, LeaderCtx};
use crucible_records::{
    DeletionRequestId, PartitionId, ProjectId, ProjectScope, TableId, Timestamp, Uuid,
};

use crate::config::LifecycleConfig;
use crate::deletion::Purges;
use crate::detail::{self, Call, Detail};

/// The most actions one run moves. The rest wait for the next run, 30 s later.
const MAX_STEPS_PER_RUN: usize = 500;

/// What the lifecycle jobs share.
#[derive(Debug)]
pub(crate) struct Shared {
    pub(crate) telemetry: Arc<dyn TelemetryStore>,
    pub(crate) subjects: Arc<dyn crate::Subjects>,
    pub(crate) exports: Option<crucible_export::ExportObjects>,
    pub(crate) config: LifecycleConfig,
}

/// Where an action acts and for whom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Place {
    pub(crate) table: TableId,
    pub(crate) partition: PartitionId,
    /// The project whose rows a delete removes.
    pub(crate) project_id: Option<ProjectId>,
    /// The deletion request the action belongs to.
    pub(crate) request_id: Option<DeletionRequestId>,
}

/// A new action row, planned and not yet issued.
pub(crate) fn planned(
    id: Uuid,
    now: Timestamp,
    key: String,
    kind: ActionKind,
    place: Place,
    detail: &Detail,
) -> LifecycleAction {
    LifecycleAction {
        id,
        key,
        kind,
        table: place.table,
        partition: place.partition,
        project_id: place.project_id,
        request_id: place.request_id,
        detail: detail.to_text(),
        state: ActionState::Planned,
        fencing_token: 0,
        mutation: None,
        attempts: 0,
        created_at: now,
        updated_at: now,
    }
}

/// Records planned actions, in fenced transactions of at most 50 rows. Returns how many were new.
pub(crate) async fn plan(
    ctx: &LeaderCtx,
    actions: Vec<LifecycleAction>,
) -> Result<usize, JobError> {
    let mut new = 0;
    let mut actions = actions.into_iter().peekable();
    while actions.peek().is_some() {
        let batch: Vec<LifecycleAction> = actions.by_ref().take(50).collect();
        new += ctx
            .fenced(move |tx| {
                Box::pin(async move {
                    let mut new = 0;
                    for action in &batch {
                        new += usize::from(tx.lifecycle().plan_action(action).await?);
                    }
                    Ok(new)
                })
            })
            .await?;
    }
    Ok(new)
}

/// Whether the next action of the same table and partition may go on in this run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Pair {
    Free,
    Busy,
}

/// The scopes of the projects that the actions of one run touch.
#[derive(Default)]
struct Scopes(BTreeMap<ProjectId, Option<ProjectScope>>);

impl Scopes {
    async fn of(
        &mut self,
        ctx: &LeaderCtx,
        project_id: Option<ProjectId>,
    ) -> Result<Option<ProjectScope>, JobError> {
        let Some(project_id) = project_id else {
            return Ok(None);
        };
        if let Some(scope) = self.0.get(&project_id) {
            return Ok(*scope);
        }
        let project = ctx.store().projects().get(project_id).await?;
        let scope = project.and_then(|project| stored_project_scope(&project).ok());
        self.0.insert(project_id, scope);
        Ok(scope)
    }
}

struct Run<'a> {
    ctx: &'a LeaderCtx,
    shared: &'a Shared,
    scopes: Scopes,
    purges: Purges,
    /// The open actions by id, as this run last wrote them.
    open: BTreeMap<Uuid, LifecycleAction>,
}

impl Run<'_> {
    fn now(&self) -> Timestamp {
        self.ctx.env.clock.wall_now()
    }

    /// One fenced transaction that writes `rows`.
    async fn write(&mut self, rows: Vec<LifecycleAction>) -> Result<(), JobError> {
        let batch = rows.clone();
        self.ctx
            .fenced(move |tx| {
                Box::pin(async move {
                    for row in &batch {
                        tx.lifecycle().update_action(row).await?;
                    }
                    Ok(())
                })
            })
            .await?;
        for row in rows {
            self.open.insert(row.id, row);
        }
        Ok(())
    }

    fn next(&self, action: &LifecycleAction, state: ActionState) -> LifecycleAction {
        LifecycleAction {
            state,
            updated_at: self.now(),
            ..action.clone()
        }
    }

    /// A definite failure: the action is planned again, or given up once its attempts have run out.
    async fn failed(&mut self, action: &LifecycleAction, why: &str) -> Result<Pair, JobError> {
        let attempts = action.attempts.saturating_add(1);
        let given_up = attempts >= self.shared.config.max_attempts;
        let state = if given_up {
            ActionState::Failed
        } else {
            ActionState::Planned
        };
        if given_up {
            tracing::warn!(
                action = %action.key, table = action.table.name(), partition = %action.partition,
                attempts, why,
                "a lifecycle action was given up after its attempts ran out; it stays in \
                 lifecycle_actions as failed for an operator to inspect"
            );
        } else {
            tracing::debug!(action = %action.key, attempts, why, "a lifecycle action failed and is planned again");
        }
        let row = LifecycleAction {
            attempts,
            mutation: None,
            ..self.next(action, state)
        };
        self.write(vec![row]).await?;
        Ok(Pair::Free)
    }

    /// A purge whose partition holds rows of its project again after its delete ran and the mask removed what it
    /// deleted: those rows arrived after the delete, and a purge takes every row of its project whatever its arrival
    /// (contract 13.4), so it is planned again, with its attempts as they were. Rows that keep landing therefore keep
    /// the purge going rather than give it up, and it is verified once a check finds the partition without them.
    async fn again(&mut self, action: &LifecycleAction) -> Result<Pair, JobError> {
        tracing::debug!(
            action = %action.key, table = action.table.name(), partition = %action.partition,
            "rows of a deleted project landed after its purge; the purge runs again"
        );
        let row = LifecycleAction {
            mutation: None,
            ..self.next(action, ActionState::Planned)
        };
        self.write(vec![row]).await?;
        Ok(Pair::Free)
    }

    /// Sends the action's call to the store, after recording it as issued under the fencing token. A purge of a
    /// project deletion is recorded and sent as a drop or a delete, whichever its partition allows now.
    async fn issue(&mut self, action: &LifecycleAction) -> Result<Pair, JobError> {
        let kind = self.purges.kind(self.ctx, self.shared, action).await?;
        let action = &LifecycleAction {
            kind,
            ..action.clone()
        };
        let scope = self.scopes.of(self.ctx, action.project_id).await?;
        let call = match detail::call(action, scope) {
            Ok(call) => call,
            Err(error) => {
                // A row that cannot be built will not build later either.
                let row = LifecycleAction {
                    attempts: self.shared.config.max_attempts,
                    ..self.next(action, ActionState::Failed)
                };
                tracing::warn!(action = %action.key, %error, "a lifecycle action cannot be built and is given up");
                self.write(vec![row]).await?;
                return Ok(Pair::Free);
            }
        };
        let lease = self.ctx.lease()?;
        let issued = LifecycleAction {
            fencing_token: lease.fencing_token,
            mutation: None,
            ..self.next(action, ActionState::Issued)
        };
        let telemetry = Arc::clone(&self.shared.telemetry);
        let send = move || async move {
            match call {
                Call::Delete(plan) => telemetry.delete(plan).await.map(Some),
                Call::Drop(table, partition) => telemetry
                    .drop_partition(table, partition)
                    .await
                    .map(|()| None),
                Call::Mask(table, partition) => telemetry
                    .apply_deleted_mask(table, partition)
                    .await
                    .map(Some),
            }
        };
        let answer = if self.shared.config.issues_unfenced() {
            send().await
        } else {
            let record = issued.clone();
            let answer = self
                .ctx
                .issue(
                    move |tx| {
                        Box::pin(async move {
                            tx.lifecycle().update_action(&record).await?;
                            Ok(())
                        })
                    },
                    |_lease| send(),
                )
                .await?;
            self.open.insert(issued.id, issued.clone());
            answer
        };
        match answer {
            Ok(None) => {
                // A drop is whole when it returns: nothing is left to mask or to count.
                self.write(vec![self.next(&issued, ActionState::Verified)])
                    .await?;
                Ok(Pair::Free)
            }
            Ok(Some(mutation)) => self.poll(&issued, &mutation).await,
            // The outcome is unknown and the call is idempotent: the row stays issued without a mutation
            // and is sent again by the next run.
            Err(StoreError::Unavailable(_)) => Ok(Pair::Busy),
            Err(error) => self.failed(&issued, &error.to_string()).await,
        }
    }

    /// Asks the store where the action's mutation stands and records the answer.
    async fn poll(
        &mut self,
        action: &LifecycleAction,
        mutation: &MutationId,
    ) -> Result<Pair, JobError> {
        let named = LifecycleAction {
            mutation: Some(mutation.name().to_owned()),
            ..action.clone()
        };
        match self.shared.telemetry.mutation_status(mutation).await {
            Ok(MutationStatus::Done) => {
                self.write(vec![self.next(&named, ActionState::Done)])
                    .await?;
                Ok(Pair::Free)
            }
            Ok(MutationStatus::Failed { code, reason }) => {
                self.failed(&named, &format!("mutation failed ({code}): {reason}"))
                    .await
            }
            Ok(MutationStatus::Pending) | Err(_) => {
                if action.mutation != named.mutation {
                    self.write(vec![self.next(&named, ActionState::Issued)])
                        .await?;
                }
                Ok(Pair::Busy)
            }
        }
    }

    /// A finished mask application: the deletes it covered are masked, and the mask itself is done with.
    async fn masked(&mut self, mask: &LifecycleAction) -> Result<Pair, JobError> {
        let covers = Detail::parse(&mask.detail)
            .map(|detail| detail.covers())
            .unwrap_or_default();
        let mut rows = vec![self.next(mask, ActionState::Verified)];
        for id in covers {
            if let Some(delete) = self.open.get(&id)
                && delete.state == ActionState::Done
            {
                rows.push(self.next(delete, ActionState::Masked));
            }
        }
        self.write(rows).await?;
        Ok(Pair::Free)
    }

    /// Verifies a masked delete (contract 13.3, step 7): no part of the partition carries a delete mask and
    /// no row matches the delete's predicate.
    async fn verify(&mut self, action: &LifecycleAction) -> Result<Pair, JobError> {
        let telemetry = &self.shared.telemetry;
        let Ok(masked) = telemetry.masked_parts(action.table, action.partition).await else {
            return Ok(Pair::Free);
        };
        if masked > 0 {
            // A later delete masked rows of the partition again: the next cycle's mask covers this one too.
            self.write(vec![self.next(action, ActionState::Done)])
                .await?;
            return Ok(Pair::Free);
        }
        let Some(scope) = self.scopes.of(self.ctx, action.project_id).await? else {
            return self.failed(action, "the action's project is gone").await;
        };
        // A purge is verified by the project's presence: no visible row of it is left in the partition.
        if Detail::parse(&action.detail).is_ok_and(|detail| detail.by_presence()) {
            return match telemetry
                .partition_projects(action.table, action.partition)
                .await
            {
                Ok(listed) if !listed.contains(&scope.project_id()) => {
                    self.write(vec![self.next(action, ActionState::Verified)])
                        .await?;
                    Ok(Pair::Free)
                }
                Ok(_) => self.again(action).await,
                // The store did not answer: the next run asks again.
                Err(_) => Ok(Pair::Free),
            };
        }
        let plan = match detail::verification(action, scope, self.shared.config.granularity) {
            Ok(plan) => plan,
            Err(error) => return self.failed(action, &error.to_string()).await,
        };
        let rows = match telemetry.execute(plan).await {
            Ok(stream) => stream.rows().await,
            Err(error) => Err(error),
        };
        match rows {
            Ok(rows) if rows.is_empty() => {
                self.write(vec![self.next(action, ActionState::Verified)])
                    .await?;
                Ok(Pair::Free)
            }
            Ok(_) => self.failed(action, "rows match the delete again").await,
            // The store did not answer: the next run asks again.
            Err(_) => Ok(Pair::Free),
        }
    }

    async fn step(&mut self, action: &LifecycleAction) -> Result<Pair, JobError> {
        match (action.kind, action.state, &action.mutation) {
            (_, ActionState::Planned, _) | (_, ActionState::Issued, None) => {
                self.issue(action).await
            }
            (_, ActionState::Issued, Some(name)) => {
                let mutation = MutationId::new(action.table, name.clone());
                self.poll(action, &mutation).await
            }
            (ActionKind::Mask, ActionState::Done, _) => self.masked(action).await,
            (ActionKind::Drop, ActionState::Done, _) => {
                self.write(vec![self.next(action, ActionState::Verified)])
                    .await?;
                Ok(Pair::Free)
            }
            (ActionKind::Delete, ActionState::Masked, _) => self.verify(action).await,
            // A finished delete waits for the mask cycle; a masked drop or mask does not exist.
            (_, ActionState::Done | ActionState::Masked, _)
            | (_, ActionState::Verified | ActionState::Failed, _) => Ok(Pair::Free),
        }
    }
}

/// Moves every open action one step, oldest first. Errors of the control store end the run; the job runs
/// again at its next cadence.
pub(crate) async fn advance(ctx: &LeaderCtx, shared: &Shared) -> JobResult {
    let actions = ctx.store().lifecycle().open_actions().await?;
    if actions.is_empty() {
        return Ok(());
    }
    let mut run = Run {
        ctx,
        shared,
        scopes: Scopes::default(),
        purges: Purges::default(),
        open: actions.iter().map(|a| (a.id, a.clone())).collect(),
    };
    let mut busy: BTreeSet<(TableId, PartitionId)> = BTreeSet::new();
    let mut steps = 0;
    for listed in &actions {
        let pair = (listed.table, listed.partition);
        if busy.contains(&pair) {
            continue;
        }
        // An earlier step of this run may have changed the row (a mask marks the deletes it covered).
        let Some(action) = run.open.get(&listed.id).cloned() else {
            continue;
        };
        // A delete that waits for the mask cycle costs nothing and does not count.
        if !(action.kind == ActionKind::Delete && action.state == ActionState::Done) {
            steps += 1;
        }
        match run.step(&action).await {
            Ok(Pair::Free) => {}
            Ok(Pair::Busy) => {
                busy.insert(pair);
            }
            Err(JobError::LeaseLost) => return Err(JobError::LeaseLost),
            Err(JobError::Store(error)) if !matches!(error, TxError::Invalid(_)) => {
                // The control store is failing: every further step would fail the same way.
                return Err(JobError::Store(error));
            }
            Err(error) => {
                tracing::warn!(action = %action.key, %error, "a lifecycle action could not be moved");
                busy.insert(pair);
            }
        }
        if steps >= MAX_STEPS_PER_RUN {
            break;
        }
    }
    Ok(())
}
