//! What an outbox row says: one notification for one channel. The alert evaluation writes it in the
//! transaction of the state change; the consumer of the outbox renders it for the channel's kind.

use crucible_control::{NewJob, TxError};
use crucible_records::{Timestamp, Uuid};
use serde::{Deserialize, Serialize};

/// A UUID as its text.
mod uuid_text {
    use crucible_records::Uuid;
    use serde::{Deserialize, Deserializer, Serializer, de::Error as _};

    pub(super) fn serialize<S: Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(id)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Uuid, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(|_| D::Error::custom("not a UUID"))
    }
}

/// What a notification announces.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    /// A series started firing.
    Firing,
    /// A series is still firing.
    Renotify,
    /// A series stopped firing.
    Resolved,
    /// A series has no data.
    NoData,
    /// The rule's evaluations keep failing.
    EvaluationFailing,
    /// A test-send.
    Test,
}

impl NoticeKind {
    /// The wire name, also the suffix of the webhook event `alert.<name>`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Firing => "firing",
            Self::Renotify => "renotify",
            Self::Resolved => "resolved",
            Self::NoData => "no_data",
            Self::EvaluationFailing => "evaluation_failing",
            Self::Test => "test",
        }
    }

    /// The word a person reads first.
    pub const fn headline(self) -> &'static str {
        match self {
            Self::Firing => "Firing",
            Self::Renotify => "Still firing",
            Self::Resolved => "Resolved",
            Self::NoData => "No data",
            Self::EvaluationFailing => "Evaluation failing",
            Self::Test => "Test",
        }
    }
}

/// One dimension value of the series a notification is about.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SeriesLabel {
    /// The dimension.
    pub dimension: String,
    /// Its value.
    pub value: String,
}

/// One notification for one channel.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Notification {
    /// The channel.
    #[serde(with = "uuid_text")]
    pub channel_id: Uuid,
    /// The project.
    #[serde(with = "uuid_text")]
    pub project_id: Uuid,
    /// The rule.
    #[serde(with = "uuid_text")]
    pub rule_id: Uuid,
    /// The rule's name when the notification was written.
    pub rule_name: String,
    /// What it announces.
    pub kind: NoticeKind,
    /// The series; empty for a rule without dimensions or a notice about the rule itself.
    pub series: Vec<SeriesLabel>,
    /// The measure.
    pub measure: String,
    /// How the measure is aggregated.
    pub aggregation: String,
    /// The comparison's wire name.
    pub comparison: String,
    /// The threshold as people read it.
    pub threshold: String,
    /// The value as people read it, when the series had rows.
    pub value: Option<String>,
    /// The rule's window in seconds.
    pub window_secs: u32,
    /// The evaluation tick, in microseconds since the epoch.
    pub at_micros: i64,
    /// The evaluation tick's number.
    pub eval_seq: u64,
}

impl Notification {
    /// The evaluation tick.
    pub const fn at(&self) -> Timestamp {
        Timestamp::from_micros(self.at_micros)
    }

    /// The series as one line: `environment=prod, model=gpt`; empty without dimensions.
    pub fn series_text(&self) -> String {
        self.series
            .iter()
            .map(|label| format!("{}={}", label.dimension, label.value))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The row to enqueue: due at once, with a deduplication key that is one per rule, tick, series, kind
    /// and channel, so a transition can never be in the outbox twice.
    pub fn outbox_job(&self, id: Uuid, series_key: &str) -> Result<NewJob, TxError> {
        let payload = serde_json::to_string(self)
            .map_err(|error| TxError::Invalid(format!("notification: {error}")))?;
        Ok(NewJob {
            id,
            dedup_key: format!(
                "{}:{}:{}:{}:{}",
                self.rule_id,
                self.eval_seq,
                self.kind.as_str(),
                self.channel_id,
                series_key
            ),
            payload,
            run_at: self.at(),
        })
    }

    /// The notification an outbox row carries.
    pub fn from_payload(payload: &str) -> Option<Self> {
        serde_json::from_str(payload).ok()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn notification(kind: NoticeKind) -> Notification {
        Notification {
            channel_id: Uuid::from_v7_parts(1_800_000_000_000, 1),
            project_id: Uuid::from_v7_parts(1_800_000_000_000, 2),
            rule_id: Uuid::from_v7_parts(1_800_000_000_000, 3),
            rule_name: "High error rate".to_owned(),
            kind,
            series: vec![SeriesLabel {
                dimension: "environment".to_owned(),
                value: "prod".to_owned(),
            }],
            measure: "error_count".to_owned(),
            aggregation: "sum".to_owned(),
            comparison: "gt".to_owned(),
            threshold: "5".to_owned(),
            value: Some("9".to_owned()),
            window_secs: 300,
            at_micros: 1_800_000_000_000_000,
            eval_seq: 30_000_000,
        }
    }

    #[test]
    fn a_notification_survives_its_outbox_row() {
        let n = notification(NoticeKind::Firing);
        let job = n.outbox_job(Uuid::from_v7_parts(1, 1), "[\"prod\"]").unwrap();
        assert_eq!(Notification::from_payload(&job.payload), Some(n.clone()));
        assert_eq!(job.run_at, n.at());
        // The key separates kinds, channels, ticks and series.
        let mut other = n.clone();
        other.kind = NoticeKind::Resolved;
        assert_ne!(
            other.outbox_job(Uuid::from_v7_parts(1, 2), "[\"prod\"]").unwrap().dedup_key,
            job.dedup_key
        );
        assert_ne!(
            n.outbox_job(Uuid::from_v7_parts(1, 3), "[\"dev\"]").unwrap().dedup_key,
            job.dedup_key
        );
        assert_eq!(Notification::from_payload("{"), None);
    }
}
