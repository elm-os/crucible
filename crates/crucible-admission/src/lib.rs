//! Admission control before the ack (contract 4.8): byte budgets, per-project token buckets, the
//! `Retry-After` rules and the decision table that answers admit, 429 or 503; and the CPU budget, a
//! first-come pool of permits for the blocking work of each stage.
//!
//! This crate is pure: no `Env`, no IO. Callers pass in the monotonic time and the random jitter.

mod bucket;
mod budget;
mod decide;
mod permits;

pub use bucket::{BucketConfig, ProjectBuckets, Throttled, TokenBucket};
pub use budget::{BudgetExceeded, ByteBudget, OverRelease};
pub use decide::{
    Decision, Signals, THROTTLED_MAX_S, THROTTLED_MIN_S, UNAVAILABLE_MAX_S, UNAVAILABLE_MIN_S,
    decide, retry_after_throttled, retry_after_unavailable,
};
pub use permits::{CpuShares, Job, Offer, Permit, PermitPool, Waiting};
