//! The job runner (contract 11.7; design: background jobs): every background feature of the control plane
//! registers a [`Job`] here and the runners do the rest.
//!
//! A job is one of three kinds:
//!
//! - a **per-node timer** ([`Job::per_node`]): runs on every node, with or without Postgres;
//! - a **leader job** ([`Job::leader`]): runs on the one node that holds a lease row. The runner takes the
//!   lease, renews it every 5 s, trusts it only until the local acquire or renew time plus the TTL minus 2 s,
//!   drops the job's future as soon as that window ends, and releases the lease on shutdown. The job writes
//!   to Postgres only through [`LeaderCtx::fenced`] and [`LeaderCtx::set_cursor`], and acts on ClickHouse
//!   only through [`LeaderCtx::issue`];
//! - a **queue consumer** ([`Job::claimed`]): claims rows with a `lease_until` and an attempt counter, runs an
//!   idempotent handler, and completes the row, gives it back with a backoff of 5 s doubling to 1 h, or
//!   parks it at the attempt cap.
//!
//! All timing is on the `Env`'s `Clock`, every task on its `Spawner` and every jitter from its `Rng`, so the
//! runners run unchanged in simulation (`crucible_sim::jobs_sim`). `docs/architecture/adding-a-job.md` says
//! how a track adds a job.

mod cadence;
mod claimed;
pub mod housekeeping;
mod job;
mod leader;
mod registry;

pub use cadence::{BACKOFF_FIRST, BACKOFF_MAX, Cadence, backoff, backoff_base};
pub use job::{ClaimCtx, ClaimPolicy, Job, JobError, JobResult, NodeCtx};
pub use leader::{ACQUIRE_EVERY, DRAIN_LIMIT, LeaderCtx, RELEASE_TIMEOUT, RENEW_RETRY};
pub use registry::{Registry, Running};
