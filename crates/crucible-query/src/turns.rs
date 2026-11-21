//! The metrics queries one project runs at once (the setting `metrics.queries_per_project`). A dashboard sends
//! one query per widget, up to 50, so a project's queries take turns: at most the limit read at once on this
//! node, and the rest wait, oldest first, within their deadline. A query that gets no turn by its deadline ends
//! as `query_timeout`, never as unavailable.
//!
//! A metrics query sends its store reads one after another (the ranking, the series, the `other` series), so
//! each turn is at most one statement in flight. The default of 4 is a quarter of the 16 statements that the
//! agent identity, `crucible_read_agent`, may run at once on a node across every project
//! (`max_concurrent_queries_for_user`, contract 7.1): one project's dashboard leaves the other twelve to the
//! other projects and to its own lists and traces. Session reads run as `crucible_read_ui`, which has no such
//! cap, and the same four turns keep one dashboard from sending ClickHouse 50 analytics statements together.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crucible_env::{Clock, ClockExt as _, QueryError};
use crucible_plan::QueryClass;
use crucible_records::ProjectId;
use tokio::sync::Semaphore;

use crate::context::{QueryServiceError, ReadContext};
use crate::service::QueryService;

/// The default of `metrics.queries_per_project`.
pub const METRICS_QUERIES_PER_PROJECT: u32 = 4;

/// The turns of every project that has a metrics query running or waiting.
#[derive(Debug)]
pub(crate) struct Turns {
    per_project: u32,
    projects: Mutex<BTreeMap<ProjectId, Arc<Semaphore>>>,
}

/// A query's hold on its project's turns, while it waits and while it reads: once no query holds one, the
/// project is forgotten, also when a wait is cancelled.
#[derive(Debug)]
struct Claim {
    turns: Arc<Turns>,
    project: ProjectId,
    slots: Option<Arc<Semaphore>>,
}

impl Drop for Claim {
    fn drop(&mut self) {
        drop(self.slots.take());
        self.turns.forget_if_idle(self.project);
    }
}

/// One query's turn, held until the query ends.
#[derive(Debug)]
pub(crate) struct Turn(Claim);

impl Drop for Turn {
    fn drop(&mut self) {
        // The turn's permit was taken out of the semaphore's count when it was granted.
        if let Some(slots) = &self.0.slots {
            slots.add_permits(1);
        }
    }
}

impl Turns {
    /// Turns of `per_project` queries at once per project, at least one.
    pub(crate) fn new(per_project: u32) -> Self {
        Self {
            per_project: per_project.max(1),
            projects: Mutex::new(BTreeMap::new()),
        }
    }

    fn projects(&self) -> MutexGuard<'_, BTreeMap<ProjectId, Arc<Semaphore>>> {
        self.projects.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets a project that no query holds or waits for a turn of: only the map holds its semaphore then, and
    /// every other handle is taken under the lock.
    fn forget_if_idle(&self, project: ProjectId) {
        let mut projects = self.projects();
        if projects
            .get(&project)
            .is_some_and(|slots| Arc::strong_count(slots) == 1)
        {
            projects.remove(&project);
        }
    }

    /// Waits for a turn of `project` until `deadline` on `clock`, oldest waiter first; `None` when none came by
    /// then.
    pub(crate) async fn take(
        self: &Arc<Self>,
        project: ProjectId,
        clock: &dyn Clock,
        deadline: crucible_env::Instant,
    ) -> Option<Turn> {
        // Taken under the lock, as every handle on a project's semaphore is.
        let slots = Arc::clone(
            self.projects()
                .entry(project)
                .or_insert_with(|| Arc::new(Semaphore::new(self.per_project as usize))),
        );
        let claim = Claim {
            turns: Arc::clone(self),
            project,
            slots: Some(slots),
        };
        let granted = match &claim.slots {
            Some(slots) => match clock.timeout_at(deadline, slots.acquire()).await {
                // The turn gives the permit back when it ends.
                Ok(Ok(permit)) => {
                    permit.forget();
                    true
                }
                // The semaphore is never closed, so only the deadline ends the wait without a permit.
                Ok(Err(_)) | Err(_) => false,
            },
            None => false,
        };
        granted.then(|| Turn(claim))
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.projects().len()
    }
}

impl QueryService {
    /// One of the project's metrics turns, waited for within the deadline of a read in `class` (contract 8.4):
    /// a wait that reaches it ends the request as `query_timeout`.
    pub(crate) async fn metrics_turn(
        &self,
        ctx: &ReadContext,
        class: QueryClass,
    ) -> Result<Turn, QueryServiceError> {
        let wait = Duration::from_secs(u64::from(ctx.caller.class(class).deadline_secs()));
        let deadline = self.clock.now().saturating_add(wait);
        self.turns
            .take(ctx.scope.project_id(), self.clock.as_ref(), deadline)
            .await
            .ok_or(QueryServiceError::Store(QueryError::Timeout))
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt as _;
    use std::pin::pin;

    use crucible_recent::test_support::ManualClock;
    use crucible_records::Timestamp;

    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn project(n: u8) -> ProjectId {
        ProjectId::from_bytes([n; 16])
    }

    fn setup(per_project: u32) -> (Arc<Turns>, ManualClock) {
        (
            Arc::new(Turns::new(per_project)),
            ManualClock::new(Timestamp::from_micros(1_794_907_800_000_000)),
        )
    }

    #[test]
    fn a_project_gets_its_limit_of_turns_and_the_next_query_waits() {
        let (turns, clock) = setup(2);
        let deadline = clock.now().saturating_add(10 * SECOND);
        let first = turns.take(project(1), &clock, deadline).now_or_never();
        let second = turns.take(project(1), &clock, deadline).now_or_never();
        assert!(matches!((&first, &second), (Some(Some(_)), Some(Some(_)))));
        let mut third = pin!(turns.take(project(1), &clock, deadline));
        assert!(
            third.as_mut().now_or_never().is_none(),
            "a third turn of two"
        );
        // Another project has turns of its own.
        let other = turns.take(project(2), &clock, deadline).now_or_never();
        assert!(matches!(other, Some(Some(_))));
        // The waiting query gets the turn the first gives back, before its deadline.
        drop(first);
        clock.advance(SECOND);
        assert!(matches!(third.as_mut().now_or_never(), Some(Some(_))));
    }

    #[test]
    fn a_query_that_gets_no_turn_by_its_deadline_gives_up() {
        let (turns, clock) = setup(1);
        let deadline = clock.now().saturating_add(12 * SECOND);
        let held = turns.take(project(1), &clock, deadline).now_or_never();
        assert!(matches!(held, Some(Some(_))));
        let mut waiting = pin!(turns.take(project(1), &clock, deadline));
        assert!(waiting.as_mut().now_or_never().is_none());
        clock.advance(12 * SECOND - Duration::from_nanos(1));
        assert!(
            waiting.as_mut().now_or_never().is_none(),
            "before the deadline"
        );
        clock.advance(Duration::from_nanos(1));
        assert!(matches!(waiting.as_mut().now_or_never(), Some(None)));
    }

    #[test]
    fn turns_are_handed_out_oldest_first_and_projects_are_forgotten_when_idle() {
        let (turns, clock) = setup(1);
        let deadline = clock.now().saturating_add(10 * SECOND);
        let held = turns
            .take(project(1), &clock, deadline)
            .now_or_never()
            .flatten();
        let mut older = pin!(turns.take(project(1), &clock, deadline));
        let mut newer = pin!(turns.take(project(1), &clock, deadline));
        assert!(older.as_mut().now_or_never().is_none());
        assert!(newer.as_mut().now_or_never().is_none());
        drop(held);
        assert!(
            newer.as_mut().now_or_never().is_none(),
            "the newer query waits"
        );
        let older = older.as_mut().now_or_never().flatten();
        assert!(older.is_some());
        drop(older);
        let newer = newer.as_mut().now_or_never().flatten();
        assert!(newer.is_some());
        assert_eq!(turns.tracked(), 1);
        drop(newer);
        assert_eq!(turns.tracked(), 0, "an idle project is forgotten");
        // A wait that is cancelled, as when its request is, forgets the project too.
        let held = turns
            .take(project(2), &clock, deadline)
            .now_or_never()
            .flatten();
        let mut cancelled = Box::pin(turns.take(project(2), &clock, deadline));
        assert!(cancelled.as_mut().now_or_never().is_none());
        drop(held);
        assert_eq!(
            turns.tracked(),
            1,
            "the cancelled wait still holds its claim"
        );
        drop(cancelled);
        assert_eq!(turns.tracked(), 0);
    }

    #[test]
    fn a_limit_of_zero_still_lets_one_query_read() {
        let (turns, clock) = setup(0);
        let deadline = clock.now().saturating_add(SECOND);
        assert!(matches!(
            turns.take(project(1), &clock, deadline).now_or_never(),
            Some(Some(_))
        ));
    }
}
