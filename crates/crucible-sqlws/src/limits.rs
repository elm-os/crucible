//! The application-side limits of the workspace (contract 7.4): 1 concurrent run per actor, 4 per project and
//! 30 runs per minute per actor. They are per node; ClickHouse's own quota and concurrency cap are the
//! deployment-wide bound.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crucible_control::SqlActor;
use crucible_env::Instant;
use crucible_records::{ProjectId, Uuid};

/// Concurrent runs per actor.
pub const MAX_CONCURRENT_PER_ACTOR: u32 = 1;
/// Concurrent runs per project.
pub const MAX_CONCURRENT_PER_PROJECT: u32 = 4;
/// Runs per actor in any [`RATE_WINDOW`].
pub const MAX_RUNS_PER_WINDOW: usize = 30;
/// The window of the run rate.
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Which limit refused a run.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LimitHit {
    /// The actor already has a run in flight.
    ActorConcurrency,
    /// The project already has four runs in flight.
    ProjectConcurrency,
    /// The actor started 30 runs in the last minute; a slot frees after this long.
    Rate {
        /// How long until the oldest run leaves the window.
        retry_after: Duration,
    },
}

type ActorKey = (u8, Uuid);

fn actor_key(actor: SqlActor) -> ActorKey {
    match actor {
        SqlActor::User(id) => (0, id.uuid()),
        SqlActor::ApiKey(id) => (1, id.uuid()),
    }
}

#[derive(Debug, Default)]
struct State {
    actors: BTreeMap<ActorKey, u32>,
    projects: BTreeMap<ProjectId, u32>,
    starts: BTreeMap<ActorKey, VecDeque<Instant>>,
}

/// The limiter of one node.
#[derive(Clone, Debug, Default)]
pub struct Limiter {
    state: Arc<Mutex<State>>,
}

/// One admitted run. Dropping it frees its concurrency slots.
#[derive(Debug)]
pub struct Permit {
    state: Arc<Mutex<State>>,
    actor: ActorKey,
    project: ProjectId,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        release(&mut state.actors, self.actor);
        release(&mut state.projects, self.project);
    }
}

fn release<K: Ord + Copy>(counts: &mut BTreeMap<K, u32>, key: K) {
    if let Some(count) = counts.get_mut(&key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(&key);
        }
    }
}

impl Limiter {
    /// Admits a run of `actor` on `project` at `now`, or says which limit refuses it. A refused run does not
    /// count toward the rate.
    pub fn admit(
        &self,
        actor: SqlActor,
        project: ProjectId,
        now: Instant,
    ) -> Result<Permit, LimitHit> {
        let key = actor_key(actor);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.actors.get(&key).copied().unwrap_or_default() >= MAX_CONCURRENT_PER_ACTOR {
            return Err(LimitHit::ActorConcurrency);
        }
        if state.projects.get(&project).copied().unwrap_or_default() >= MAX_CONCURRENT_PER_PROJECT {
            return Err(LimitHit::ProjectConcurrency);
        }
        let starts = state.starts.entry(key).or_default();
        while starts
            .front()
            .is_some_and(|start| now.saturating_duration_since(*start) >= RATE_WINDOW)
        {
            starts.pop_front();
        }
        if starts.len() >= MAX_RUNS_PER_WINDOW {
            let oldest = starts.front().copied().unwrap_or(now);
            return Err(LimitHit::Rate {
                retry_after: RATE_WINDOW.saturating_sub(now.saturating_duration_since(oldest)),
            });
        }
        starts.push_back(now);
        *state.actors.entry(key).or_default() += 1;
        *state.projects.entry(project).or_default() += 1;
        Ok(Permit {
            state: Arc::clone(&self.state),
            actor: key,
            project,
        })
    }
}

#[cfg(test)]
mod tests {
    use crucible_records::UserId;
    use proptest::prelude::*;

    use super::*;

    fn user(n: u8) -> SqlActor {
        SqlActor::User(UserId::from_uuid(Uuid::from_bytes([n; 16])))
    }

    fn project(n: u8) -> ProjectId {
        ProjectId::from_uuid(Uuid::from_bytes([n; 16]))
    }

    fn at(secs: u64) -> Instant {
        Instant::from_nanos(secs * 1_000_000_000)
    }

    #[test]
    fn one_run_per_actor_and_four_per_project() {
        let limiter = Limiter::default();
        let first = limiter.admit(user(1), project(1), at(0)).unwrap();
        assert_eq!(
            limiter.admit(user(1), project(1), at(0)).unwrap_err(),
            LimitHit::ActorConcurrency
        );
        // The same actor on another project is still one actor.
        assert_eq!(
            limiter.admit(user(1), project(2), at(0)).unwrap_err(),
            LimitHit::ActorConcurrency
        );
        let _others: Vec<Permit> = (2..=4)
            .map(|n| limiter.admit(user(n), project(1), at(0)).unwrap())
            .collect();
        assert_eq!(
            limiter.admit(user(5), project(1), at(0)).unwrap_err(),
            LimitHit::ProjectConcurrency
        );
        assert!(limiter.admit(user(5), project(2), at(0)).is_ok());
        drop(first);
        assert!(limiter.admit(user(5), project(1), at(0)).is_ok());
    }

    #[test]
    fn thirty_runs_a_minute_per_actor() {
        let limiter = Limiter::default();
        for n in 0..30 {
            drop(limiter.admit(user(1), project(1), at(n)).unwrap());
        }
        assert_eq!(
            limiter.admit(user(1), project(1), at(40)).unwrap_err(),
            LimitHit::Rate {
                retry_after: Duration::from_secs(20)
            }
        );
        assert!(limiter.admit(user(2), project(1), at(40)).is_ok());
        // The first run leaves the window at second 60.
        assert!(limiter.admit(user(1), project(1), at(60)).is_ok());
        assert!(limiter.admit(user(1), project(1), at(60)).is_err());
    }

    proptest! {
        /// Whatever the order of starts and ends, the limits hold and a dropped permit frees its slots.
        #[test]
        fn the_limits_hold_under_any_schedule(ops in prop::collection::vec((0u8..4, 0u8..2, any::<bool>()), 0..200)) {
            let limiter = Limiter::default();
            let mut held: Vec<(u8, u8, Permit)> = Vec::new();
            let mut started: BTreeMap<u8, Vec<u64>> = BTreeMap::new();
            for (step, (actor, proj, end)) in ops.into_iter().enumerate() {
                let now = step as u64;
                if end && !held.is_empty() {
                    held.remove(0);
                    continue;
                }
                let per_actor = held.iter().filter(|(a, _, _)| *a == actor).count();
                let per_project = held.iter().filter(|(_, p, _)| *p == proj).count();
                let recent = started.get(&actor).map_or(0, |s| s.iter().filter(|t| now - **t < 60).count());
                let expect_ok = per_actor < 1 && per_project < 4 && recent < 30;
                match limiter.admit(user(actor), project(proj), at(now)) {
                    Ok(permit) => {
                        prop_assert!(expect_ok);
                        held.push((actor, proj, permit));
                        started.entry(actor).or_default().push(now);
                    }
                    Err(_) => prop_assert!(!expect_ok),
                }
            }
        }
    }
}
