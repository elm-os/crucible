//! Alert rules (contracts 11.7 and 12.6; design: alerts engine).
//!
//! - [`RuleSpec`] is a rule's definition: one measure of the span rollup, at most two dimensions, a
//!   comparison and threshold, a window, an interval of at least 60 s, a `for` duration, a no-data policy,
//!   `renotify` and channels.
//! - [`schedule`] gives each rule its ticks, shifted inside the interval by an offset derived from the rule's
//!   id, and the window each tick reads, which lags the tick by at least 30 s.
//! - [`machine`] is the pure state machine of one series: ok, pending, firing, resolved and no_data.
//! - [`Evaluator`] handles one tick as claimed work: it reads through the metrics registry with a typed plan
//!   of class `alert`, and applies the states, the history and the outbox rows of the notices in one
//!   transaction.
//! - [`jobs`] are the two jobs a node registers: the scheduler of ticks and the consumer of evaluations.
//! - [`Alerts`] is rule management for the API.

mod evaluate;
pub mod machine;
pub mod schedule;
mod scheduler;
mod service;
mod source;
mod spec;

use crucible_control::Queue;
use crucible_jobs::{Cadence, Job};

pub use evaluate::{EVALUATION_POLICY, Evaluator, FAILING_AFTER, evaluation_job};
pub use scheduler::{KEEP_EVALUATIONS, KEEP_NOTIFICATIONS, SCAN_EVERY};
pub use service::{AlertError, Alerts, Preview, PreviewSeries, RuleInput, RuleView};
pub use source::{Observed, QuerySource, ReadFailure, SeriesSource, series_key, series_values};
pub use spec::{
    Comparison, FOR_MAX_SECS, INTERVAL_SECS, MAX_CHANNELS, MAX_SERIES, McpRuleRead, NAME_MAX_CHARS,
    NO_DATA_FOR_DEFAULT_SECS, NO_DATA_FOR_MAX_SECS, NoDataPolicy, RENOTIFY_DEFAULT_SECS,
    RENOTIFY_SECS, RuleSpec, SpecIssue, WINDOW_SECS, display_value,
};

/// The jobs of alerting, each registered with one line (docs/architecture/adding-a-job.md): the scheduler,
/// a timer on every node, and the consumer of the queue of evaluations.
pub fn jobs(evaluator: Evaluator) -> [Job; 2] {
    let consumer = Job::claimed(
        "alert-evaluations",
        Queue::AlertEvaluations,
        Cadence::every(std::time::Duration::from_secs(5)),
        EVALUATION_POLICY,
        move |ctx| {
            let evaluator = evaluator.clone();
            async move { evaluator.handle(ctx).await }
        },
    );
    [scheduler::job(), consumer]
}
