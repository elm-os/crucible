//! The alert repositories over Postgres (migration V3): channels, alert rules, the state of each series and the
//! history.

use async_trait::async_trait;
use crucible_control::{
    AlertEvent, AlertEventKind, AlertEventRepo, AlertRule, AlertRuleRepo, AlertSeriesState,
    AlertStateRepo, AlertStatus, Channel, ChannelKind, ChannelRepo, JsonDoc, TxError,
};
use crucible_records::{AlertRuleId, ChannelId, ProjectId, Timestamp, Uuid};

use crate::sql::{Cols, P, int};
use crate::store::{Handle, every, first};

const CHANNEL: &str = "id, project_id, name, kind, config::text, secret_id, enabled, disabled_reason, \
     failures, last_test::text, version";

fn channel(c: &mut Cols<'_>) -> Result<Channel, TxError> {
    Ok(Channel {
        id: ChannelId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        name: c.text()?,
        kind: c.parsed(ChannelKind::parse)?,
        config: JsonDoc::parse(&c.text()?)?,
        secret_id: c.opt_uuid()?,
        enabled: c.bool()?,
        disabled_reason: c.opt_text()?,
        failures: c.u32()?,
        last_test: c
            .opt_text()?
            .map(|text| JsonDoc::parse(&text))
            .transpose()?,
        version: c.u64()?,
    })
}

#[async_trait]
impl ChannelRepo for Handle {
    async fn get(&self, id: ChannelId) -> Result<Option<Channel>, TxError> {
        let sql = format!("SELECT {CHANNEL} FROM channels WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id.uuid())]).await?, channel)
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<Channel>, TxError> {
        let sql = format!("SELECT {CHANNEL} FROM channels WHERE project_id = $1 ORDER BY id");
        every(
            &self.read(&sql, &[P::Uuid(project_id.uuid())]).await?,
            channel,
        )
    }

    async fn insert(&self, channel: &Channel) -> Result<(), TxError> {
        let config = channel.config.to_text();
        let last_test = channel.last_test.as_ref().map(JsonDoc::to_text);
        self.write(
            "INSERT INTO channels (id, project_id, name, kind, config, secret_id, enabled, disabled_reason, \
             failures, last_test, version) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9, $10::jsonb, $11) RETURNING 1",
            &[
                P::Uuid(channel.id.uuid()),
                P::Uuid(channel.project_id.uuid()),
                P::Text(&channel.name),
                P::Text(channel.kind.as_str()),
                P::Text(&config),
                P::OptUuid(channel.secret_id),
                P::Bool(channel.enabled),
                P::OptText(channel.disabled_reason.as_deref()),
                P::Int4(channel.failures),
                P::OptText(last_test.as_deref()),
                int(channel.version)?,
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(&self, channel: &Channel, expected: u64) -> Result<bool, TxError> {
        let config = channel.config.to_text();
        let changed = self
            .write(
                "UPDATE channels SET name = $2, kind = $3, config = $4::jsonb, secret_id = $5, enabled = $6, \
                 disabled_reason = $7, failures = $8, version = version + 1 \
                 WHERE id = $1 AND version = $9 RETURNING 1",
                &[
                    P::Uuid(channel.id.uuid()),
                    P::Text(&channel.name),
                    P::Text(channel.kind.as_str()),
                    P::Text(&config),
                    P::OptUuid(channel.secret_id),
                    P::Bool(channel.enabled),
                    P::OptText(channel.disabled_reason.as_deref()),
                    P::Int4(channel.failures),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete_if_version(&self, id: ChannelId, expected: u64) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM channels WHERE id = $1 AND version = $2 RETURNING 1",
                &[P::Uuid(id.uuid()), int(expected)?],
            )
            .await?;
        Ok(deleted == 1)
    }

    async fn set_last_test(&self, id: ChannelId, outcome: &JsonDoc) -> Result<bool, TxError> {
        let outcome = outcome.to_text();
        let changed = self
            .write(
                "UPDATE channels SET last_test = $2::jsonb WHERE id = $1 RETURNING 1",
                &[P::Uuid(id.uuid()), P::Text(&outcome)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn record_failure(
        &self,
        id: ChannelId,
        reason: &str,
        disable_at: u32,
    ) -> Result<bool, TxError> {
        // One statement: the count and the switch change together, so two nodes that fail at once cannot both
        // read the count below the limit. The right-hand sides read the row as it was before the statement.
        let rows = self
            .query_main(
                true,
                "UPDATE channels SET failures = failures + 1, \
                 enabled = failures + 1 < $2, \
                 disabled_reason = CASE WHEN failures + 1 < $2 THEN disabled_reason ELSE $3 END, \
                 version = CASE WHEN failures + 1 < $2 THEN version ELSE version + 1 END \
                 WHERE id = $1 AND enabled RETURNING enabled",
                &[P::Uuid(id.uuid()), P::Int4(disable_at), P::Text(reason)],
            )
            .await?;
        Ok(first(&rows, |c| c.bool())? == Some(false))
    }

    async fn record_success(&self, id: ChannelId) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE channels SET failures = 0 WHERE id = $1 AND failures <> 0 RETURNING 1",
                &[P::Uuid(id.uuid())],
            )
            .await?;
        Ok(changed == 1)
    }
}

const RULE: &str = "id, project_id, name, enabled, spec::text, interval_secs, version, created_at, updated_at, \
     last_eval_seq, eval_failures, skipped_ticks";

fn tick(value: Option<i64>) -> Result<Option<u64>, TxError> {
    value
        .map(|value| {
            u64::try_from(value)
                .map_err(|error| TxError::Invalid(format!("evaluation tick: {error}")))
        })
        .transpose()
}

fn opt_int(value: Option<u64>) -> Result<P<'static>, TxError> {
    value
        .map(|value| {
            i64::try_from(value)
                .map_err(|error| TxError::Invalid(format!("evaluation tick: {error}")))
        })
        .transpose()
        .map(P::OptInt)
}

fn rule(c: &mut Cols<'_>) -> Result<AlertRule, TxError> {
    Ok(AlertRule {
        id: AlertRuleId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        name: c.text()?,
        enabled: c.bool()?,
        spec: JsonDoc::parse(&c.text()?)?,
        interval_secs: c.u32()?,
        version: c.u64()?,
        created_at: c.ts()?,
        updated_at: c.ts()?,
        last_eval_seq: tick(c.opt_i64()?)?,
        eval_failures: c.u32()?,
        skipped_ticks: c.u64()?,
    })
}

#[async_trait]
impl AlertRuleRepo for Handle {
    async fn get(&self, id: AlertRuleId) -> Result<Option<AlertRule>, TxError> {
        let sql = format!("SELECT {RULE} FROM alert_rules WHERE id = $1");
        first(&self.read(&sql, &[P::Uuid(id.uuid())]).await?, rule)
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<AlertRule>, TxError> {
        let sql = format!("SELECT {RULE} FROM alert_rules WHERE project_id = $1 ORDER BY id");
        every(&self.read(&sql, &[P::Uuid(project_id.uuid())]).await?, rule)
    }

    async fn list_enabled(&self) -> Result<Vec<AlertRule>, TxError> {
        let sql = format!("SELECT {RULE} FROM alert_rules WHERE enabled ORDER BY id");
        every(&self.read(&sql, &[]).await?, rule)
    }

    async fn insert(&self, rule: &AlertRule) -> Result<(), TxError> {
        let spec = rule.spec.to_text();
        self.write(
            "INSERT INTO alert_rules (id, project_id, name, enabled, spec, interval_secs, version, created_at, \
             updated_at, last_eval_seq, eval_failures, skipped_ticks) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9, $10, $11, $12) RETURNING 1",
            &[
                P::Uuid(rule.id.uuid()),
                P::Uuid(rule.project_id.uuid()),
                P::Text(&rule.name),
                P::Bool(rule.enabled),
                P::Text(&spec),
                P::Int4(rule.interval_secs),
                int(rule.version)?,
                P::Ts(rule.created_at),
                P::Ts(rule.updated_at),
                opt_int(rule.last_eval_seq)?,
                P::Int4(rule.eval_failures),
                int(rule.skipped_ticks)?,
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(&self, rule: &AlertRule, expected: u64) -> Result<bool, TxError> {
        let spec = rule.spec.to_text();
        let changed = self
            .write(
                "UPDATE alert_rules SET name = $2, enabled = $3, spec = $4::jsonb, interval_secs = $5, \
                 updated_at = $6, version = version + 1 WHERE id = $1 AND version = $7 RETURNING 1",
                &[
                    P::Uuid(rule.id.uuid()),
                    P::Text(&rule.name),
                    P::Bool(rule.enabled),
                    P::Text(&spec),
                    P::Int4(rule.interval_secs),
                    P::Ts(rule.updated_at),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete_if_version(&self, id: AlertRuleId, expected: u64) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM alert_rules WHERE id = $1 AND version = $2 RETURNING 1",
                &[P::Uuid(id.uuid()), int(expected)?],
            )
            .await?;
        Ok(deleted == 1)
    }

    async fn record_evaluation(
        &self,
        id: AlertRuleId,
        last_eval_seq: Option<u64>,
        eval_failures: u32,
        skipped_ticks: u64,
    ) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE alert_rules SET last_eval_seq = $2, eval_failures = $3, skipped_ticks = $4 \
                 WHERE id = $1 RETURNING 1",
                &[
                    P::Uuid(id.uuid()),
                    opt_int(last_eval_seq)?,
                    P::Int4(eval_failures),
                    int(skipped_ticks)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }
}

fn state(c: &mut Cols<'_>) -> Result<AlertSeriesState, TxError> {
    Ok(AlertSeriesState {
        rule_id: AlertRuleId::from_uuid(c.uuid()?),
        series_key: c.text()?,
        status: c.parsed(AlertStatus::parse)?,
        since: c.ts()?,
        last_rows_at: c.ts()?,
        last_notified_at: c.opt_ts()?,
        value: c.opt_i64()?,
    })
}

#[async_trait]
impl AlertStateRepo for Handle {
    async fn list(&self, rule_id: AlertRuleId) -> Result<Vec<AlertSeriesState>, TxError> {
        // Byte order, as the model orders its keys, whatever the collation of the database.
        let rows = self
            .read(
                "SELECT rule_id, series_key, status, since, last_rows_at, last_notified_at, value \
                 FROM alert_state WHERE rule_id = $1 ORDER BY series_key COLLATE \"C\"",
                &[P::Uuid(rule_id.uuid())],
            )
            .await?;
        every(&rows, state)
    }

    async fn put(&self, state: &AlertSeriesState) -> Result<(), TxError> {
        self.write(
            "INSERT INTO alert_state (rule_id, series_key, status, since, last_rows_at, last_notified_at, value) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (rule_id, series_key) DO UPDATE SET status = EXCLUDED.status, since = EXCLUDED.since, \
             last_rows_at = EXCLUDED.last_rows_at, last_notified_at = EXCLUDED.last_notified_at, \
             value = EXCLUDED.value RETURNING 1",
            &[
                P::Uuid(state.rule_id.uuid()),
                P::Text(&state.series_key),
                P::Text(state.status.as_str()),
                P::Ts(state.since),
                P::Ts(state.last_rows_at),
                P::OptTs(state.last_notified_at),
                P::OptInt(state.value),
            ],
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, rule_id: AlertRuleId, series_key: &str) -> Result<bool, TxError> {
        let deleted = self
            .write(
                "DELETE FROM alert_state WHERE rule_id = $1 AND series_key = $2 RETURNING 1",
                &[P::Uuid(rule_id.uuid()), P::Text(series_key)],
            )
            .await?;
        Ok(deleted == 1)
    }
}

const EVENT: &str =
    "id, rule_id, series_key, kind, from_status, to_status, value, notified, eval_seq, at";

fn event(c: &mut Cols<'_>) -> Result<AlertEvent, TxError> {
    Ok(AlertEvent {
        id: c.uuid()?,
        rule_id: AlertRuleId::from_uuid(c.uuid()?),
        series_key: c.text()?,
        kind: c.parsed(AlertEventKind::parse)?,
        from_status: c.parsed(AlertStatus::parse)?,
        to_status: c.parsed(AlertStatus::parse)?,
        value: c.opt_i64()?,
        notified: c.bool()?,
        eval_seq: c.u64()?,
        at: c.ts()?,
    })
}

#[async_trait]
impl AlertEventRepo for Handle {
    async fn insert(&self, event: &AlertEvent) -> Result<(), TxError> {
        let sql = format!(
            "INSERT INTO alert_events ({EVENT}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING 1"
        );
        self.write(
            &sql,
            &[
                P::Uuid(event.id),
                P::Uuid(event.rule_id.uuid()),
                P::Text(&event.series_key),
                P::Text(event.kind.as_str()),
                P::Text(event.from_status.as_str()),
                P::Text(event.to_status.as_str()),
                P::OptInt(event.value),
                P::Bool(event.notified),
                int(event.eval_seq)?,
                P::Ts(event.at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn list(
        &self,
        rule_id: AlertRuleId,
        before: Option<(Timestamp, Uuid)>,
        limit: u32,
    ) -> Result<Vec<AlertEvent>, TxError> {
        let rows = match before {
            None => {
                let sql = format!(
                    "SELECT {EVENT} FROM alert_events WHERE rule_id = $1 ORDER BY at DESC, id DESC LIMIT $2"
                );
                self.read(&sql, &[P::Uuid(rule_id.uuid()), P::Int(i64::from(limit))])
                    .await?
            }
            Some((at, id)) => {
                let sql = format!(
                    "SELECT {EVENT} FROM alert_events WHERE rule_id = $1 AND (at, id) < ($3, $4) \
                     ORDER BY at DESC, id DESC LIMIT $2"
                );
                self.read(
                    &sql,
                    &[
                        P::Uuid(rule_id.uuid()),
                        P::Int(i64::from(limit)),
                        P::Ts(at),
                        P::Uuid(id),
                    ],
                )
                .await?
            }
        };
        every(&rows, event)
    }
}
