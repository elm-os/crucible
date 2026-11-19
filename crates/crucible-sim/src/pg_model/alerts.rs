//! The alert repositories of the model (migration V3): channels, alert rules, the state of each series and the
//! history.

use async_trait::async_trait;
use crucible_control::{
    AlertEvent, AlertEventRepo, AlertRule, AlertRuleRepo, AlertSeriesState, AlertStateRepo,
    Channel, ChannelRepo, JsonDoc, TxError,
};
use crucible_records::{AlertRuleId, ChannelId, ProjectId, Timestamp, Uuid};

use super::{Key, Pool, Row, Runner, Tbl, uuid_key};

fn state_key(rule_id: AlertRuleId, series_key: &str) -> Key {
    [&rule_id.to_bytes()[..], series_key.as_bytes()].concat()
}

#[async_trait]
impl ChannelRepo for Runner {
    async fn get(&self, id: ChannelId) -> Result<Option<Channel>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::Channels,
                &uuid_key(id.to_bytes()),
                Row::Channel
            ))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<Channel>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut channels = all!(cx, Tbl::Channels, Row::Channel);
            channels.retain(|channel| channel.project_id == project_id);
            Ok(channels)
        })
        .await
    }

    async fn insert(&self, channel: &Channel) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::Channels,
                uuid_key(channel.id.to_bytes()),
                Row::Channel(channel.clone()),
                "channels_pkey",
            )
        })
        .await
    }

    async fn update_if_version(&self, channel: &Channel, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(channel.id.to_bytes());
            match one!(cx, Tbl::Channels, &key, Row::Channel) {
                Some(stored) if stored.version == expected => {
                    let mut next = channel.clone();
                    next.version = expected + 1;
                    next.last_test = stored.last_test;
                    cx.put(Tbl::Channels, key, Some(Row::Channel(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn delete_if_version(&self, id: ChannelId, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::Channels, &key, Row::Channel) {
                Some(stored) if stored.version == expected => {
                    cx.put(Tbl::Channels, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn set_last_test(&self, id: ChannelId, outcome: &JsonDoc) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            let Some(mut channel) = one!(cx, Tbl::Channels, &key, Row::Channel) else {
                return Ok(false);
            };
            channel.last_test = Some(outcome.clone());
            cx.put(Tbl::Channels, key, Some(Row::Channel(channel)))?;
            Ok(true)
        })
        .await
    }

    async fn record_failure(
        &self,
        id: ChannelId,
        reason: &str,
        disable_at: u32,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            let Some(mut channel) = one!(cx, Tbl::Channels, &key, Row::Channel) else {
                return Ok(false);
            };
            if !channel.enabled {
                return Ok(false);
            }
            channel.failures += 1;
            let disabled = channel.failures >= disable_at;
            if disabled {
                channel.enabled = false;
                channel.disabled_reason = Some(reason.to_owned());
                channel.version += 1;
            }
            cx.put(Tbl::Channels, key, Some(Row::Channel(channel)))?;
            Ok(disabled)
        })
        .await
    }

    async fn record_success(&self, id: ChannelId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::Channels, &key, Row::Channel) {
                Some(mut channel) if channel.failures != 0 => {
                    channel.failures = 0;
                    cx.put(Tbl::Channels, key, Some(Row::Channel(channel)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}

#[async_trait]
impl AlertRuleRepo for Runner {
    async fn get(&self, id: AlertRuleId) -> Result<Option<AlertRule>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::AlertRules,
                &uuid_key(id.to_bytes()),
                Row::AlertRule
            ))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<AlertRule>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut rules = all!(cx, Tbl::AlertRules, Row::AlertRule);
            rules.retain(|rule| rule.project_id == project_id);
            Ok(rules)
        })
        .await
    }

    async fn list_enabled(&self) -> Result<Vec<AlertRule>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut rules = all!(cx, Tbl::AlertRules, Row::AlertRule);
            rules.retain(|rule| rule.enabled);
            Ok(rules)
        })
        .await
    }

    async fn insert(&self, rule: &AlertRule) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::AlertRules,
                uuid_key(rule.id.to_bytes()),
                Row::AlertRule(rule.clone()),
                "alert_rules_pkey",
            )
        })
        .await
    }

    async fn update_if_version(&self, rule: &AlertRule, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(rule.id.to_bytes());
            match one!(cx, Tbl::AlertRules, &key, Row::AlertRule) {
                Some(stored) if stored.version == expected => {
                    let next = AlertRule {
                        name: rule.name.clone(),
                        enabled: rule.enabled,
                        spec: rule.spec.clone(),
                        interval_secs: rule.interval_secs,
                        updated_at: rule.updated_at,
                        version: expected + 1,
                        ..stored
                    };
                    cx.put(Tbl::AlertRules, key, Some(Row::AlertRule(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn delete_if_version(&self, id: AlertRuleId, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::AlertRules, &key, Row::AlertRule) {
                Some(stored) if stored.version == expected => {
                    cx.put(Tbl::AlertRules, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn record_evaluation(
        &self,
        id: AlertRuleId,
        last_eval_seq: Option<u64>,
        eval_failures: u32,
        skipped_ticks: u64,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            let Some(mut rule) = one!(cx, Tbl::AlertRules, &key, Row::AlertRule) else {
                return Ok(false);
            };
            rule.last_eval_seq = last_eval_seq;
            rule.eval_failures = eval_failures;
            rule.skipped_ticks = skipped_ticks;
            cx.put(Tbl::AlertRules, key, Some(Row::AlertRule(rule)))?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl AlertStateRepo for Runner {
    async fn list(&self, rule_id: AlertRuleId) -> Result<Vec<AlertSeriesState>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut states = all!(cx, Tbl::AlertState, Row::AlertState);
            states.retain(|state| state.rule_id == rule_id);
            Ok(states)
        })
        .await
    }

    async fn put(&self, state: &AlertSeriesState) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.put(
                Tbl::AlertState,
                state_key(state.rule_id, &state.series_key),
                Some(Row::AlertState(state.clone())),
            )
        })
        .await
    }

    async fn delete(&self, rule_id: AlertRuleId, series_key: &str) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = state_key(rule_id, series_key);
            if cx.get(Tbl::AlertState, &key).is_none() {
                return Ok(false);
            }
            cx.put(Tbl::AlertState, key, None)?;
            Ok(true)
        })
        .await
    }
}

#[async_trait]
impl AlertEventRepo for Runner {
    async fn insert(&self, event: &AlertEvent) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::AlertEvents,
                uuid_key(event.id.to_bytes()),
                Row::AlertEvent(event.clone()),
                "alert_events_pkey",
            )
        })
        .await
    }

    async fn list(
        &self,
        rule_id: AlertRuleId,
        before: Option<(Timestamp, Uuid)>,
        limit: u32,
    ) -> Result<Vec<AlertEvent>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut events = all!(cx, Tbl::AlertEvents, Row::AlertEvent);
            events.retain(|event| {
                event.rule_id == rule_id && before.is_none_or(|before| (event.at, event.id) < before)
            });
            events.sort_by_key(|event| std::cmp::Reverse((event.at, event.id)));
            events.truncate(limit as usize);
            Ok(events)
        })
        .await
    }
}
