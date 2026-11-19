//! The evaluation of one tick of one rule (contracts 11.7 and 12.6): a claimed row of the queue
//! `alert_evaluations`.
//!
//! The handler reads the rule's series, then applies the tick in one transaction: the state of every series,
//! the history rows, the outbox rows of every notice, and the rule's last applied tick. The transaction first
//! reads that last tick, so a tick is applied once however often its row is handled: after a claim that ran
//! out, a commit whose outcome was unknown, or a second node. A notice is in the outbox exactly when its
//! transition is committed.
//!
//! A read that fails leaves the state as it is. The first two attempts of a row give it back to be retried
//! after the runner's backoff; the third records the failure and consumes the tick. Five failed ticks in a row
//! write one "evaluation failing" notice.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crucible_control::{
    AlertEvent, AlertEventKind, AlertRule, AlertSeriesState, AlertStatus, ControlTx, NewJob, Queue,
    TxError,
};
use crucible_jobs::{ClaimCtx, ClaimPolicy, JobError, JobResult};
use crucible_notify::{NoticeKind, Notification, SeriesLabel};
use crucible_records::{AlertRuleId, Timestamp, Uuid};
use serde_json::{Value, json};

use crate::machine::{Mutant, Notice, Observation, Series, step};
use crate::schedule::{current_seq, tick_at, window_of};
use crate::source::{Observed, SeriesSource, series_values};
use crate::spec::{MAX_SERIES, RuleSpec, display_value};

/// How the queue of evaluations is claimed: the default 60 s claim, and few attempts, because a tick that
/// cannot be applied is followed by the next one.
pub const EVALUATION_POLICY: ClaimPolicy = ClaimPolicy {
    lease_for: Duration::from_secs(60),
    max_attempts: 6,
};
/// Failed ticks in a row after which the rule's channels are told once.
pub const FAILING_AFTER: u32 = 5;
/// Attempts of a row that give it back when the read fails, before the failure is recorded.
const READ_RETRIES: u32 = 2;

/// The row of one tick.
pub fn evaluation_job(id: Uuid, rule_id: AlertRuleId, interval_secs: u32, seq: u64) -> NewJob {
    NewJob {
        id,
        // The interval is part of the key: ticks are numbered by it, so a rule whose interval was edited
        // starts a new numbering that cannot meet a row of the old one.
        dedup_key: format!("{rule_id}:{interval_secs}:{seq}"),
        payload: json!({ "rule": rule_id.to_string(), "interval": interval_secs, "seq": seq })
            .to_string(),
        run_at: tick_at(rule_id, interval_secs, seq),
    }
}

fn parse(payload: &str) -> Option<(AlertRuleId, u32, u64)> {
    let value: Value = serde_json::from_str(payload).ok()?;
    let rule = AlertRuleId::parse(value.get("rule")?.as_str()?).ok()?;
    let interval = u32::try_from(value.get("interval")?.as_u64()?).ok()?;
    Some((rule, interval, value.get("seq")?.as_u64()?))
}

/// What evaluations run with.
#[derive(Clone, Debug)]
pub struct Evaluator {
    source: Arc<dyn SeriesSource>,
    mutant: Option<Mutant>,
}

/// How a tick ended, for the logs and the simulation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Applied {
    /// The transaction committed the tick.
    Now,
    /// The tick, or a later one, was applied before; or the rule is gone, off or was edited meanwhile.
    Skipped,
}

fn notice_kind(notice: Notice) -> NoticeKind {
    match notice {
        Notice::Firing => NoticeKind::Firing,
        Notice::Renotify => NoticeKind::Renotify,
        Notice::Resolved => NoticeKind::Resolved,
        Notice::NoData => NoticeKind::NoData,
    }
}

fn to_series(state: &AlertSeriesState) -> Series {
    Series {
        status: state.status,
        since: state.since,
        last_rows_at: state.last_rows_at,
        last_notified_at: state.last_notified_at,
        value: state.value,
    }
}

/// The notification of one notice for one channel.
fn notification(
    tick: &Tick<'_>,
    channel: Uuid,
    kind: NoticeKind,
    series_key: &str,
    value: Option<i64>,
) -> Notification {
    let Tick {
        rule,
        spec,
        seq,
        at,
    } = *tick;
    Notification {
        channel_id: channel,
        project_id: rule.project_id.uuid(),
        rule_id: rule.id.uuid(),
        rule_name: rule.name.clone(),
        kind,
        series: spec
            .dimensions
            .iter()
            .zip(series_values(series_key))
            .map(|(dimension, value)| SeriesLabel {
                dimension: dimension.as_str().to_owned(),
                value,
            })
            .collect(),
        measure: spec.measure.as_str().to_owned(),
        aggregation: spec.aggregation.as_str().to_owned(),
        comparison: spec.comparison.as_str().to_owned(),
        threshold: display_value(spec.measure, spec.threshold),
        value: value.map(|value| display_value(spec.measure, value)),
        window_secs: spec.window_secs,
        at_micros: at.micros(),
        eval_seq: seq,
    }
}

impl Evaluator {
    /// An evaluator that reads through `source`.
    pub fn new(source: Arc<dyn SeriesSource>) -> Self {
        Self {
            source,
            mutant: None,
        }
    }

    /// The same evaluator with a seeded defect, for the simulation that must catch it.
    #[cfg(feature = "test-mutants")]
    #[must_use]
    pub fn with_mutant(mut self, mutant: Option<Mutant>) -> Self {
        self.mutant = mutant;
        self
    }

    /// Handles one claimed row.
    pub async fn handle(&self, ctx: ClaimCtx) -> JobResult {
        let Some((rule_id, interval, seq)) = parse(&ctx.claim.payload) else {
            tracing::warn!(id = %ctx.claim.id, "an evaluation row names no rule and tick; it is dropped");
            return Ok(());
        };
        let store = ctx.store.as_ref();
        let Some(rule) = store.alert_rules().get(rule_id).await? else {
            return Ok(());
        };
        // A row of another interval is a tick of the schedule the rule had before an edit.
        if !rule.enabled
            || rule.interval_secs != interval
            || rule.last_eval_seq.is_some_and(|last| last >= seq)
        {
            return Ok(());
        }
        // The database clock at the claim, from the claim itself: a node's wall clock is never compared
        // with a tick. After downtime only the latest tick is evaluated; this one is counted as skipped by
        // the tick that is.
        let lease = i64::try_from(EVALUATION_POLICY.lease_for.as_micros()).unwrap_or(i64::MAX);
        let claimed_at = ctx.claim.lease_until.saturating_add_micros(-lease);
        if current_seq(rule_id, rule.interval_secs, claimed_at).is_some_and(|due| due > seq) {
            return Ok(());
        }
        let spec = match RuleSpec::from_doc(&rule.spec) {
            Ok(spec) => spec,
            Err(error) => {
                tracing::warn!(rule = %rule_id, %error, "a stored alert rule does not parse; it is not evaluated");
                return Ok(());
            }
        };
        let Some(project) = store.projects().get(rule.project_id).await? else {
            return Ok(());
        };
        if project.deleted_at.is_some() {
            return Ok(());
        }
        let window = window_of(rule_id, rule.interval_secs, seq, spec.window_secs)
            .map_err(|_| JobError::Failed("the tick has no window".to_owned()))?;
        let observed = match self.source.read(&project, &spec, window).await {
            Ok(observed) => Some(observed),
            Err(failure) if ctx.claim.attempt <= READ_RETRIES => {
                return Err(JobError::Failed(format!("the read failed: {failure}")));
            }
            Err(failure) => {
                tracing::warn!(rule = %rule_id, seq, %failure, "an alert evaluation could not read; its state is unchanged");
                None
            }
        };
        let tick = Tick {
            rule: &rule,
            spec: &spec,
            seq,
            at: tick_at(rule_id, rule.interval_secs, seq),
        };
        let tx = store.begin().await?;
        let (applied, after) = match self
            .apply(&ctx, tx.as_ref(), &tick, observed.as_ref())
            .await
        {
            Ok(done) => done,
            Err(error) => {
                tx.rollback().await;
                return Err(error.into());
            }
        };
        tx.commit().await?;
        if applied == Applied::Now {
            // Only the seeded defect has rows here: outbox rows written after the commit, which a crash
            // between the two loses.
            for job in after {
                store
                    .claims()
                    .enqueue(Queue::NotificationOutbox, &job)
                    .await?;
            }
        }
        Ok(())
    }

    /// Applies the tick inside `tx`. Returns the outbox rows that the seeded defect writes afterwards.
    async fn apply(
        &self,
        ctx: &ClaimCtx,
        tx: &dyn ControlTx,
        tick: &Tick<'_>,
        observed: Option<&Observed>,
    ) -> Result<(Applied, Vec<NewJob>), TxError> {
        let Tick {
            rule,
            spec,
            seq,
            at,
        } = *tick;
        let Some(stored) = tx.alert_rules().get(rule.id).await? else {
            return Ok((Applied::Skipped, Vec::new()));
        };
        // The guard of exactly-once: the tick is applied only by the transaction that moves the rule's last
        // tick past it, and two such transactions conflict on the rule row.
        if stored.last_eval_seq.is_some_and(|last| last >= seq)
            || !stored.enabled
            || stored.version != rule.version
        {
            return Ok((Applied::Skipped, Vec::new()));
        }
        let skipped = stored
            .skipped_ticks
            .saturating_add(match stored.last_eval_seq {
                Some(last) => seq.saturating_sub(last).saturating_sub(1),
                None => 0,
            });
        let mut outbox = Vec::new();
        let mut enqueue = |kind: NoticeKind, series_key: &str, value: Option<i64>| {
            for channel in &spec.channels {
                let notification = notification(tick, channel.uuid(), kind, series_key, value);
                outbox.push(notification.outbox_job(ctx.env.new_id(), series_key));
            }
        };
        let event = |series_key: &str, kind, from, to, value, notified| AlertEvent {
            id: ctx.env.new_id(),
            rule_id: rule.id,
            series_key: series_key.to_owned(),
            kind,
            from_status: from,
            to_status: to,
            value,
            notified,
            eval_seq: seq,
            at,
        };
        let Some(observed) = observed else {
            let failures = stored.eval_failures.saturating_add(1);
            if failures == FAILING_AFTER {
                enqueue(NoticeKind::EvaluationFailing, "", None);
                let row = event(
                    "",
                    AlertEventKind::EvaluationFailing,
                    AlertStatus::Ok,
                    AlertStatus::Ok,
                    None,
                    !spec.channels.is_empty(),
                );
                tx.alert_events().insert(&row).await?;
            }
            tx.alert_rules()
                .record_evaluation(rule.id, Some(seq), failures, skipped)
                .await?;
            return self.finish(tx, outbox).await;
        };

        let states: BTreeMap<String, AlertSeriesState> = tx
            .alert_state()
            .list(rule.id)
            .await?
            .into_iter()
            .map(|state| (state.series_key.clone(), state))
            .collect();
        let values: BTreeMap<&str, i64> = observed
            .series
            .iter()
            .map(|(key, value)| (key.as_str(), *value))
            .collect();
        let keys: BTreeSet<&str> = states
            .keys()
            .map(String::as_str)
            .chain(values.keys().copied())
            .collect();
        let params = spec.params();
        let mut tracked = states.len();
        for key in keys {
            let prev = states.get(key);
            // A rule tracks at most 50 series: a new one waits until another is forgotten.
            if prev.is_none() && tracked >= MAX_SERIES as usize {
                continue;
            }
            let observation = match values.get(key) {
                Some(value) => Observation::Value {
                    value: *value,
                    breach: spec.comparison.holds(*value, spec.threshold),
                },
                None => Observation::NoRows,
            };
            let before = prev.map(to_series);
            let outcome = step(&params, before, observation, at, self.mutant);
            if outcome.next == before && outcome.notice.is_none() {
                continue;
            }
            match outcome.next {
                Some(next) => {
                    tracked += usize::from(prev.is_none());
                    tx.alert_state()
                        .put(&AlertSeriesState {
                            rule_id: rule.id,
                            series_key: key.to_owned(),
                            status: next.status,
                            since: next.since,
                            last_rows_at: next.last_rows_at,
                            last_notified_at: next.last_notified_at,
                            value: next.value,
                        })
                        .await?;
                }
                None => {
                    tracked = tracked.saturating_sub(1);
                    tx.alert_state().delete(rule.id, key).await?;
                }
            }
            let from = before.map_or(AlertStatus::Ok, |series| series.status);
            let to = outcome.next.map_or(AlertStatus::Ok, |series| series.status);
            let value = values.get(key).copied();
            if let Some(notice) = outcome.notice {
                enqueue(notice_kind(notice), key, value);
            }
            if from != to || outcome.notice.is_some() {
                let kind = match outcome.notice {
                    Some(Notice::Renotify) => AlertEventKind::Renotify,
                    _ => AlertEventKind::Transition,
                };
                let notified = outcome.notice.is_some() && !spec.channels.is_empty();
                tx.alert_events()
                    .insert(&event(key, kind, from, to, value, notified))
                    .await?;
            }
        }
        tx.alert_rules()
            .record_evaluation(rule.id, Some(seq), 0, skipped)
            .await?;
        self.finish(tx, outbox).await
    }

    /// Writes the outbox rows in the transaction, as the contract requires; the seeded defect returns them
    /// to be written after it.
    async fn finish(
        &self,
        tx: &dyn ControlTx,
        outbox: Vec<Result<NewJob, TxError>>,
    ) -> Result<(Applied, Vec<NewJob>), TxError> {
        let jobs = outbox.into_iter().collect::<Result<Vec<_>, _>>()?;
        if self.mutant == Some(Mutant::NotifyOutsideTransaction) {
            return Ok((Applied::Now, jobs));
        }
        for job in &jobs {
            tx.claims().enqueue(Queue::NotificationOutbox, job).await?;
        }
        Ok((Applied::Now, Vec::new()))
    }
}

/// One tick of one rule.
#[derive(Clone, Copy)]
struct Tick<'a> {
    rule: &'a AlertRule,
    spec: &'a RuleSpec,
    seq: u64,
    at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_names_its_rule_and_tick() {
        let rule = AlertRuleId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000, 5));
        let job = evaluation_job(Uuid::from_v7_parts(1, 1), rule, 60, 30_000_000);
        assert_eq!(parse(&job.payload), Some((rule, 60, 30_000_000)));
        assert_eq!(job.dedup_key, format!("{rule}:60:30000000"));
        assert_eq!(job.run_at, tick_at(rule, 60, 30_000_000));
        assert_eq!(parse("{}"), None);
        assert_eq!(parse("not json"), None);
    }
}
