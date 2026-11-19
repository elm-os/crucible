//! Channels, alert rules, their per-series state and their history (contract 11.1), and the repositories over
//! them. The rule's definition and a channel's settings are JSON documents that `crucible-alerts` and
//! `crucible-notify` own; the columns here are what the store itself orders, filters or changes atomically.

use std::fmt;

use async_trait::async_trait;
use crucible_records::{AlertRuleId, ChannelId, ProjectId, Timestamp, Uuid};
use serde::{Deserialize, Serialize};

use crate::error::TxError;
use crate::types::{JsonDoc, text_enum};

text_enum! {
    /// The kind of a notification channel (contract 11.1).
    ChannelKind {
        /// A Slack incoming webhook.
        Slack => "slack",
        /// Email through the deployment's SMTP server.
        Email => "email",
        /// A signed webhook.
        Webhook => "webhook",
    }
}

text_enum! {
    /// The state of one series of an alert rule.
    AlertStatus {
        /// The condition does not hold.
        Ok => "ok",
        /// The condition holds, for less than the rule's `for`.
        Pending => "pending",
        /// The condition has held for the rule's `for`.
        Firing => "firing",
        /// The condition stopped holding at the last evaluation.
        Resolved => "resolved",
        /// The series had no rows for the rule's no-data time, under the `no_data` policy.
        NoData => "no_data",
    }
}

text_enum! {
    /// What a history row records.
    AlertEventKind {
        /// A series moved from one state to another.
        Transition => "transition",
        /// A series that is still firing was notified again.
        Renotify => "renotify",
        /// The rule's evaluations failed several times in a row.
        EvaluationFailing => "evaluation_failing",
    }
}

/// A notification channel. `config` holds what the kind needs (a URL, recipients); a secret it needs is sealed
/// in `secrets` under `secret_id`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Channel {
    /// The id.
    pub id: ChannelId,
    /// The project.
    pub project_id: ProjectId,
    /// The display name.
    pub name: String,
    /// The kind.
    pub kind: ChannelKind,
    /// The kind's settings.
    pub config: JsonDoc,
    /// The sealed secret, for kinds that have one.
    pub secret_id: Option<Uuid>,
    /// Whether notifications are sent to it.
    pub enabled: bool,
    /// Why the server turned the channel off; `None` when a person did it or it is on.
    pub disabled_reason: Option<String>,
    /// Deliveries that failed in a row.
    pub failures: u32,
    /// The outcome of the last test-send.
    pub last_test: Option<JsonDoc>,
    /// The optimistic-concurrency version; starts at 1.
    pub version: u64,
}

/// An alert rule. `spec` is the rule's definition; `interval_secs` repeats its interval for the scheduler.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlertRule {
    /// The id.
    pub id: AlertRuleId,
    /// The project.
    pub project_id: ProjectId,
    /// The display name.
    pub name: String,
    /// Whether the rule is evaluated.
    pub enabled: bool,
    /// The definition.
    pub spec: JsonDoc,
    /// Seconds between evaluations.
    pub interval_secs: u32,
    /// The optimistic-concurrency version; starts at 1.
    pub version: u64,
    /// When the rule was created.
    pub created_at: Timestamp,
    /// When the definition last changed.
    pub updated_at: Timestamp,
    /// The last evaluation tick that was applied.
    pub last_eval_seq: Option<u64>,
    /// Evaluations that failed in a row.
    pub eval_failures: u32,
    /// Ticks that were never evaluated, for example during downtime.
    pub skipped_ticks: u64,
}

/// The state of one series of a rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlertSeriesState {
    /// The rule.
    pub rule_id: AlertRuleId,
    /// The series: the JSON array of its dimension values, `[]` for a rule without dimensions.
    pub series_key: String,
    /// The state.
    pub status: AlertStatus,
    /// When the series entered the state.
    pub since: Timestamp,
    /// The last evaluation at which the series had rows.
    pub last_rows_at: Timestamp,
    /// When a firing notification was last written for the series.
    pub last_notified_at: Option<Timestamp>,
    /// The value at the last evaluation with rows, in the measure's stored unit.
    pub value: Option<i64>,
}

/// One row of a rule's history.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlertEvent {
    /// The id.
    pub id: Uuid,
    /// The rule.
    pub rule_id: AlertRuleId,
    /// The series; empty for a row about the rule itself.
    pub series_key: String,
    /// What the row records.
    pub kind: AlertEventKind,
    /// The state before.
    pub from_status: AlertStatus,
    /// The state after.
    pub to_status: AlertStatus,
    /// The value that was compared, when the series had rows.
    pub value: Option<i64>,
    /// Whether notifications were written to the outbox with this row.
    pub notified: bool,
    /// The evaluation tick.
    pub eval_seq: u64,
    /// The time of the evaluation tick.
    pub at: Timestamp,
}

/// Notification channels.
#[async_trait]
pub trait ChannelRepo: Send + Sync {
    /// The channel with this id.
    async fn get(&self, id: ChannelId) -> Result<Option<Channel>, TxError>;
    /// The channels of a project, ordered by id.
    async fn list(&self, project_id: ProjectId) -> Result<Vec<Channel>, TxError>;
    /// Creates a channel.
    async fn insert(&self, channel: &Channel) -> Result<(), TxError>;
    /// Writes `channel` with version `expected + 1` if the stored version is `expected`. `false` when it is
    /// not. `last_test` is not written here.
    async fn update_if_version(&self, channel: &Channel, expected: u64) -> Result<bool, TxError>;
    /// Deletes the channel if its stored version is `expected`. `false` when it is not or there is none.
    async fn delete_if_version(&self, id: ChannelId, expected: u64) -> Result<bool, TxError>;
    /// Stores the outcome of a test-send. Changes no version. `false` when there is no such channel.
    async fn set_last_test(&self, id: ChannelId, outcome: &JsonDoc) -> Result<bool, TxError>;
    /// Counts a failed delivery to an enabled channel, in one statement. When the count reaches `disable_at`
    /// the channel is turned off with `reason` and its version increases; `true` exactly then.
    async fn record_failure(
        &self,
        id: ChannelId,
        reason: &str,
        disable_at: u32,
    ) -> Result<bool, TxError>;
    /// Resets the failure count after a delivery. `false` when it was already zero or there is no such channel.
    async fn record_success(&self, id: ChannelId) -> Result<bool, TxError>;
}

/// Alert rules.
#[async_trait]
pub trait AlertRuleRepo: Send + Sync {
    /// The rule with this id.
    async fn get(&self, id: AlertRuleId) -> Result<Option<AlertRule>, TxError>;
    /// The rules of a project, ordered by id.
    async fn list(&self, project_id: ProjectId) -> Result<Vec<AlertRule>, TxError>;
    /// Every enabled rule, ordered by id, for the scheduler.
    async fn list_enabled(&self) -> Result<Vec<AlertRule>, TxError>;
    /// Creates a rule.
    async fn insert(&self, rule: &AlertRule) -> Result<(), TxError>;
    /// Writes the rule's name, switch, definition, interval and `updated_at` with version `expected + 1` if
    /// the stored version is `expected`. `false` when it is not. The evaluation columns are not written here.
    async fn update_if_version(&self, rule: &AlertRule, expected: u64) -> Result<bool, TxError>;
    /// Deletes the rule with its state and history if its stored version is `expected`. `false` when it is
    /// not or there is none.
    async fn delete_if_version(&self, id: AlertRuleId, expected: u64) -> Result<bool, TxError>;
    /// Records an evaluation: the last tick applied, the failures in a row and the ticks skipped so far.
    /// Changes no version. `false` when there is no such rule.
    async fn record_evaluation(
        &self,
        id: AlertRuleId,
        last_eval_seq: Option<u64>,
        eval_failures: u32,
        skipped_ticks: u64,
    ) -> Result<bool, TxError>;
}

/// The state of each series of a rule.
#[async_trait]
pub trait AlertStateRepo: Send + Sync {
    /// The series of a rule, ordered by series key.
    async fn list(&self, rule_id: AlertRuleId) -> Result<Vec<AlertSeriesState>, TxError>;
    /// Creates or replaces the state of one series.
    async fn put(&self, state: &AlertSeriesState) -> Result<(), TxError>;
    /// Removes the state of one series. `false` when there was none.
    async fn delete(&self, rule_id: AlertRuleId, series_key: &str) -> Result<bool, TxError>;
}

/// The history of alert rules.
#[async_trait]
pub trait AlertEventRepo: Send + Sync {
    /// Appends a row.
    async fn insert(&self, event: &AlertEvent) -> Result<(), TxError>;
    /// The rows of a rule, newest first (by time then id), at most `limit`, strictly before `before` when
    /// given.
    async fn list(
        &self,
        rule_id: AlertRuleId,
        before: Option<(Timestamp, Uuid)>,
        limit: u32,
    ) -> Result<Vec<AlertEvent>, TxError>;
}
