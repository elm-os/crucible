//! Alert rules and notification channels: `projects/{project_id}/alerts/...` and
//! `projects/{project_id}/channels/...` (contract 12.6).

use serde::{Deserialize, Serialize};
use ts_rs::TS;
use utoipa::{IntoParams, ToSchema};

use crate::metrics::SeriesKey;
use crate::wire::{SafeI64, SafeU64};

/// How a rule compares its value with its threshold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum AlertComparison {
    /// Above the threshold.
    Gt,
    /// At or above the threshold.
    Gte,
    /// Below the threshold.
    Lt,
    /// At or below the threshold.
    Lte,
}

/// What a series becomes when it has had no rows for `no_data_for_secs`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum AlertNoDataPolicy {
    /// The state `no_data`, with one notification.
    NoData,
    /// As if the condition did not hold.
    Ok,
    /// The series keeps its state.
    Keep,
}

/// The state of a series, or of a rule (the most severe state of its series).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum AlertStatus {
    /// The condition does not hold.
    Ok,
    /// The condition holds, for less than the rule's `for_secs`.
    Pending,
    /// The condition has held for the rule's `for_secs`.
    Firing,
    /// The condition stopped holding at the last evaluation.
    Resolved,
    /// The series had no rows for the rule's no-data time.
    NoData,
}

/// What a history row records.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum AlertEventKind {
    /// A series moved from one state to another.
    Transition,
    /// A series that is still firing was notified again.
    Renotify,
    /// The rule's evaluations failed five times in a row.
    EvaluationFailing,
}

/// The body of `POST alerts`: a rule. Members left out take the defaults named on them.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AlertRuleInput {
    /// The name, 1 to 120 characters.
    pub name: String,
    /// Whether the rule is evaluated. Default `true`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub enabled: Option<bool>,
    /// The source of the measure. Rules read the span rollup: `spans` (the default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub source: Option<String>,
    /// The measure, from `GET metrics/schema`.
    pub measure: String,
    /// How the measure is aggregated over the window, one of the measure's aggregations.
    pub aggregation: String,
    /// Up to two dimensions; each combination of their values is one series. Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub dimensions: Option<Vec<String>>,
    /// The comparison.
    pub comparison: AlertComparison,
    /// The threshold in the measure's stored unit: a count, microseconds, or nano-USD.
    pub threshold: SafeI64,
    /// The window each evaluation reads, in seconds: whole minutes from 60 to 86400.
    pub window_secs: u32,
    /// The time between evaluations, in seconds: 60 (the default) to 86400.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub interval_secs: Option<u32>,
    /// How long the condition must hold before a series fires, in seconds. Default 0: it fires at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub for_secs: Option<u32>,
    /// The no-data policy. Default `keep`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub no_data: Option<AlertNoDataPolicy>,
    /// How long a series may have no rows before the policy applies, in seconds. Default 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub no_data_for_secs: Option<u32>,
    /// How long after a notification a series that still fires is notified again, in seconds: 300 to 604800.
    /// Default 14400 (four hours).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub renotify_secs: Option<u32>,
    /// The ids of the channels the rule notifies, at most 10. Default none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub channels: Option<Vec<String>>,
}

/// The body of `PATCH alerts/{alert_id}`: the members to change. A change to what the rule measures or when
/// starts its series afresh; a change to the name, the switch, the channels or `renotify_secs` keeps them.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct AlertRulePatch {
    /// A new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub name: Option<String>,
    /// Turns the rule on or off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub enabled: Option<bool>,
    /// A new measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub measure: Option<String>,
    /// A new aggregation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub aggregation: Option<String>,
    /// New dimensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub dimensions: Option<Vec<String>>,
    /// A new comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub comparison: Option<AlertComparison>,
    /// A new threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub threshold: Option<SafeI64>,
    /// A new window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub window_secs: Option<u32>,
    /// A new interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub interval_secs: Option<u32>,
    /// A new `for`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub for_secs: Option<u32>,
    /// A new no-data policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub no_data: Option<AlertNoDataPolicy>,
    /// A new no-data time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub no_data_for_secs: Option<u32>,
    /// A new time between repeated notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub renotify_secs: Option<u32>,
    /// The new list of channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub channels: Option<Vec<String>>,
}

/// One series a rule tracks.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertSeries {
    /// The series: one value per dimension of the rule, in the rule's order. Empty for a rule without
    /// dimensions.
    pub key: Vec<SeriesKey>,
    /// The state.
    pub status: AlertStatus,
    /// When the series entered the state (RFC 3339).
    #[schema(format = DateTime)]
    pub since: String,
    /// The value at the last evaluation with rows, in the measure's stored unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub value: Option<SafeI64>,
    /// When the series was last notified as firing (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub last_notified_at: Option<String>,
}

/// An alert rule with the state of its series.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertRule {
    /// The id.
    pub id: String,
    /// The name.
    pub name: String,
    /// Whether the rule is evaluated.
    pub enabled: bool,
    /// The source of the measure.
    pub source: String,
    /// The measure.
    pub measure: String,
    /// The aggregation.
    pub aggregation: String,
    /// The unit of the threshold and of every value: `count`, `us` (microseconds) or `nusd` (nano-USD).
    pub unit: String,
    /// The dimensions.
    pub dimensions: Vec<String>,
    /// The comparison.
    pub comparison: AlertComparison,
    /// The threshold, in `unit`.
    pub threshold: SafeI64,
    /// The window, in seconds.
    pub window_secs: u32,
    /// The time between evaluations, in seconds.
    pub interval_secs: u32,
    /// How long the condition must hold before a series fires, in seconds.
    pub for_secs: u32,
    /// The no-data policy.
    pub no_data: AlertNoDataPolicy,
    /// How long a series may have no rows before the policy applies, in seconds.
    pub no_data_for_secs: u32,
    /// The time between repeated notifications of a firing series, in seconds.
    pub renotify_secs: u32,
    /// The ids of the channels the rule notifies.
    pub channels: Vec<String>,
    /// The most severe state among the rule's series: firing, then no_data, pending, resolved, ok.
    pub status: AlertStatus,
    /// The series the rule tracks, at most 50, ordered by key.
    pub series: Vec<AlertSeries>,
    /// When the rule was last evaluated (RFC 3339); absent before its first evaluation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub last_evaluated_at: Option<String>,
    /// Evaluations that failed in a row. From five on, the rule's channels were told once.
    pub eval_failures: u32,
    /// Evaluations that never ran, for example while the server was down.
    pub skipped_ticks: SafeU64,
    /// When the rule was created (RFC 3339).
    #[schema(format = DateTime)]
    pub created_at: String,
    /// When the rule was last changed (RFC 3339).
    #[schema(format = DateTime)]
    pub updated_at: String,
    /// Whether a value of this object was clamped to the wire's integer range (contract 1.3).
    pub saturated: bool,
    /// The version, also sent as `ETag`; `PATCH` and `DELETE` take it in `If-Match`.
    pub version: u32,
}

/// The rules of a project.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertRuleList {
    /// The rules, ordered by id (oldest first).
    pub data: Vec<AlertRule>,
}

/// One row of a rule's history.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertEvent {
    /// The id.
    pub id: String,
    /// What the row records.
    pub kind: AlertEventKind,
    /// The series; empty for a rule without dimensions and for a row about the rule itself.
    pub key: Vec<SeriesKey>,
    /// The state before.
    pub from_status: AlertStatus,
    /// The state after.
    pub to_status: AlertStatus,
    /// The value that was compared, when the series had rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub value: Option<SafeI64>,
    /// Whether notifications were written for the rule's channels with this row.
    pub notified: bool,
    /// The time of the evaluation (RFC 3339).
    #[schema(format = DateTime)]
    pub at: String,
}

/// The query of `GET alerts/{alert_id}/history`.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, IntoParams, TS)]
#[into_params(parameter_in = Query)]
#[serde(deny_unknown_fields)]
pub struct AlertHistoryParams {
    /// Rows per page: 1 to 200, default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(nullable = false)]
    #[ts(optional)]
    pub limit: Option<u32>,
    /// The `next_cursor` of the page before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(nullable = false)]
    #[ts(optional)]
    pub cursor: Option<String>,
}

/// A page of a rule's history, newest first.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertHistory {
    /// The rows.
    pub data: Vec<AlertEvent>,
    /// The cursor of the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub next_cursor: Option<String>,
    /// Whether there are older rows.
    pub has_more: bool,
    /// Whether a value of this object was clamped to the wire's integer range (contract 1.3).
    pub saturated: bool,
}

/// One series of a preview.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertPreviewSeries {
    /// The series.
    pub key: Vec<SeriesKey>,
    /// The value over the window, in the measure's stored unit.
    pub value: SafeI64,
    /// Whether the value breaches the threshold.
    pub breach: bool,
}

/// What a rule would see if it were evaluated now. Nothing is stored and nobody is notified.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct AlertPreview {
    /// Start of the window that was read (RFC 3339).
    #[schema(format = DateTime)]
    pub from: String,
    /// End of the window: the last whole minute at least 30 s ago (RFC 3339).
    #[schema(format = DateTime)]
    pub to: String,
    /// The unit of the values.
    pub unit: String,
    /// The series that had rows, the ones nearest to a breach first.
    pub series: Vec<AlertPreviewSeries>,
    /// Whether the rule has more than 50 series; only the 50 nearest to a breach are tracked.
    pub truncated: bool,
    /// Whether a value of this object was clamped to the wire's integer range (contract 1.3).
    pub saturated: bool,
}

/// The kind of a notification channel.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    /// A Slack incoming webhook.
    Slack,
    /// Email through the deployment's mail server.
    Email,
    /// A signed webhook.
    Webhook,
}

/// The body of `POST channels`.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ChannelCreate {
    /// The name, 1 to 80 characters.
    pub name: String,
    /// The kind.
    pub kind: ChannelKind,
    /// The URL of a Slack incoming webhook (`https://hooks.slack.com/...`) or of a webhook receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub url: Option<String>,
    /// The recipients of an email channel, 1 to 20.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub to: Option<Vec<String>>,
}

/// The body of `PATCH channels/{channel_id}`: the members to change.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize, ToSchema, TS)]
#[serde(deny_unknown_fields)]
pub struct ChannelPatch {
    /// A new name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub name: Option<String>,
    /// Turns the channel on or off. Turning it on clears the failure count and the reason it was turned off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub enabled: Option<bool>,
    /// A new URL, for a Slack or webhook channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub url: Option<String>,
    /// New recipients, for an email channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub to: Option<Vec<String>>,
    /// `true` replaces the signing secret of a webhook channel; the answer carries the new one, once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub rotate_secret: Option<bool>,
}

/// The outcome of a test-send. It never carries anything the receiver answered.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ChannelTestResult {
    /// Whether the receiver accepted the test.
    pub ok: bool,
    /// The class of the receiver's HTTP status: `2xx` to `5xx`. Absent for email and without an answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub status_class: Option<String>,
    /// How long the attempt took, in milliseconds.
    pub latency_ms: SafeU64,
    /// The stable reason of a failure, for example `connect_failed`, `timeout`, `receiver_rejected`,
    /// `redirect_not_followed`, `metadata_address` or `private_address`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub error_code: Option<String>,
    /// When the test was sent (RFC 3339); present on a channel's `last_test`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false, format = DateTime)]
    #[ts(optional)]
    pub at: Option<String>,
}

/// A notification channel. Its secret is never part of it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct Channel {
    /// The id.
    pub id: String,
    /// The name.
    pub name: String,
    /// The kind.
    pub kind: ChannelKind,
    /// Whether notifications are sent to it.
    pub enabled: bool,
    /// Why the server turned the channel off (50 deliveries in a row failed). Absent when it is on or a
    /// person turned it off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub disabled_reason: Option<String>,
    /// Deliveries that failed in a row.
    pub failures: u32,
    /// The URL of a webhook channel. Shown only to callers who may manage channels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub url: Option<String>,
    /// A Slack URL shortened to its host and last characters; the URL itself is a credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub url_hint: Option<String>,
    /// The recipients of an email channel. Shown only to callers who may manage channels.
    pub to: Vec<String>,
    /// The outcome of the last test-send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub last_test: Option<ChannelTestResult>,
    /// The signing secret of a webhook channel. Present only in the answer that created or rotated it: it is
    /// shown once and cannot be read again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(nullable = false)]
    #[ts(optional)]
    pub signing_secret: Option<String>,
    /// The version, also sent as `ETag`; `PATCH` and `DELETE` take it in `If-Match`.
    pub version: u32,
}

/// The channels of a project.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize, ToSchema, TS)]
pub struct ChannelList {
    /// The channels, ordered by id (oldest first).
    pub data: Vec<Channel>,
}
