//! Alert rule management (contract 12.6): what the API's alert routes do.

use std::sync::Arc;

use crucible_control::{
    AlertEvent, AlertRule, AlertSeriesState, ControlStore, Project, TxError,
};
use crucible_env::Env;
use crucible_records::{AlertRuleId, ProjectId, TimeWindow, Timestamp, Uuid};

use crate::schedule::EVALUATION_LAG;
use crate::source::SeriesSource;
use crate::spec::{NAME_MAX_CHARS, RuleSpec, SpecIssue};

/// Why an alert request failed.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum AlertError {
    /// No such rule in the project.
    #[error("no such alert rule")]
    NotFound,
    /// The rule changed since the version the caller holds.
    #[error("the rule changed since it was read")]
    VersionMismatch,
    /// The rule is not allowed; each issue names the member to change.
    #[error("the rule is not valid")]
    Invalid(Vec<SpecIssue>),
    /// The node has no telemetry store, so a rule cannot be previewed.
    #[error("no telemetry store is configured")]
    NoTelemetry,
    /// The preview's read failed.
    #[error("the read failed: {0}")]
    Read(String),
    /// The store failed.
    #[error(transparent)]
    Store(#[from] TxError),
}

/// A rule with its definition and the state of its series.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RuleView {
    /// The row.
    pub rule: AlertRule,
    /// The definition.
    pub spec: RuleSpec,
    /// The series the rule tracks, ordered by key.
    pub series: Vec<AlertSeriesState>,
}

/// What a person sets on a rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RuleInput {
    /// The name.
    pub name: String,
    /// Whether the rule is evaluated.
    pub enabled: bool,
    /// The definition.
    pub spec: RuleSpec,
}

/// One series of a preview.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PreviewSeries {
    /// The series key.
    pub key: String,
    /// The value over the window.
    pub value: i64,
    /// Whether it breaches the threshold.
    pub breach: bool,
}

/// What a rule would see if it were evaluated now.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Preview {
    /// The window that was read.
    pub window: TimeWindow,
    /// The series with rows.
    pub series: Vec<PreviewSeries>,
    /// Whether the rule has more series than it tracks.
    pub truncated: bool,
}

/// Alert rule management over the control store.
#[derive(Clone, Debug)]
pub struct Alerts {
    env: Env,
    store: Arc<dyn ControlStore>,
    source: Option<Arc<dyn SeriesSource>>,
}

fn issue(field: &'static str, message: impl Into<String>) -> SpecIssue {
    SpecIssue {
        field,
        message: message.into(),
    }
}

impl Alerts {
    /// Management over `store`. `source` is the read of a preview; `None` on a node without a telemetry
    /// store.
    pub fn new(
        env: &Env,
        store: Arc<dyn ControlStore>,
        source: Option<Arc<dyn SeriesSource>>,
    ) -> Self {
        Self {
            env: env.clone(),
            store,
            source,
        }
    }

    async fn view(&self, rule: AlertRule) -> Result<RuleView, AlertError> {
        Ok(RuleView {
            spec: RuleSpec::from_doc(&rule.spec)?,
            series: self.store.alert_state().list(rule.id).await?,
            rule,
        })
    }

    async fn find(&self, project_id: ProjectId, id: AlertRuleId) -> Result<AlertRule, AlertError> {
        match self.store.alert_rules().get(id).await? {
            Some(rule) if rule.project_id == project_id => Ok(rule),
            _ => Err(AlertError::NotFound),
        }
    }

    /// Everything about the input that is not allowed.
    async fn check(&self, project_id: ProjectId, input: &RuleInput) -> Result<String, AlertError> {
        let mut issues = input.spec.issues();
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > NAME_MAX_CHARS || name.chars().any(char::is_control) {
            issues.push(issue("name", "a name of 1 to 120 characters is required"));
        }
        let channels = self.store.channels().list(project_id).await?;
        for wanted in &input.spec.channels {
            if !channels.iter().any(|channel| channel.id == *wanted) {
                issues.push(issue(
                    "channels",
                    format!("the channel {wanted} does not exist in this project"),
                ));
            }
        }
        if issues.is_empty() {
            Ok(name.to_owned())
        } else {
            Err(AlertError::Invalid(issues))
        }
    }

    /// The rules of a project.
    pub async fn list(&self, project_id: ProjectId) -> Result<Vec<RuleView>, AlertError> {
        let mut views = Vec::new();
        for rule in self.store.alert_rules().list(project_id).await? {
            views.push(self.view(rule).await?);
        }
        Ok(views)
    }

    /// One rule.
    pub async fn get(&self, project_id: ProjectId, id: AlertRuleId) -> Result<RuleView, AlertError> {
        let rule = self.find(project_id, id).await?;
        self.view(rule).await
    }

    /// Creates a rule. Its first evaluation is the next tick of its schedule.
    pub async fn create(&self, project_id: ProjectId, input: RuleInput) -> Result<RuleView, AlertError> {
        let name = self.check(project_id, &input).await?;
        let now = self.env.clock.wall_now();
        let rule = AlertRule {
            id: AlertRuleId::from_uuid(self.env.new_id()),
            project_id,
            name,
            enabled: input.enabled,
            spec: input.spec.to_doc(),
            interval_secs: input.spec.interval_secs,
            version: 1,
            created_at: now,
            updated_at: now,
            last_eval_seq: None,
            eval_failures: 0,
            skipped_ticks: 0,
        };
        self.store.alert_rules().insert(&rule).await?;
        Ok(RuleView {
            rule,
            spec: input.spec,
            series: Vec::new(),
        })
    }

    /// Replaces the name, switch and definition of a rule whose stored version is `expected`. A change to what
    /// the rule measures or when starts its series afresh: their states were reached under another rule. A
    /// change to the channels or to `renotify` alone keeps them.
    pub async fn update(
        &self,
        project_id: ProjectId,
        id: AlertRuleId,
        expected: u64,
        input: RuleInput,
    ) -> Result<RuleView, AlertError> {
        let mut rule = self.find(project_id, id).await?;
        if rule.version != expected {
            return Err(AlertError::VersionMismatch);
        }
        let name = self.check(project_id, &input).await?;
        let before = RuleSpec::from_doc(&rule.spec)?;
        let same_measurement = RuleSpec {
            channels: before.channels.clone(),
            renotify_secs: before.renotify_secs,
            ..input.spec.clone()
        } == before;
        rule.name = name;
        rule.enabled = input.enabled;
        rule.spec = input.spec.to_doc();
        rule.interval_secs = input.spec.interval_secs;
        rule.updated_at = self.env.clock.wall_now();
        let tx = self.store.begin().await?;
        if !tx.alert_rules().update_if_version(&rule, expected).await? {
            tx.rollback().await;
            return Err(AlertError::VersionMismatch);
        }
        if !same_measurement {
            for state in tx.alert_state().list(id).await? {
                tx.alert_state().delete(id, &state.series_key).await?;
            }
            // Ticks are numbered by the interval, so the last tick of the old schedule means nothing now.
            tx.alert_rules()
                .record_evaluation(id, None, 0, rule.skipped_ticks)
                .await?;
            rule.last_eval_seq = None;
            rule.eval_failures = 0;
        }
        tx.commit().await?;
        rule.version = expected + 1;
        self.view(rule).await
    }

    /// Deletes a rule whose stored version is `expected`, with its state and history.
    pub async fn delete(
        &self,
        project_id: ProjectId,
        id: AlertRuleId,
        expected: u64,
    ) -> Result<(), AlertError> {
        self.find(project_id, id).await?;
        if self.store.alert_rules().delete_if_version(id, expected).await? {
            Ok(())
        } else {
            Err(AlertError::VersionMismatch)
        }
    }

    /// A page of a rule's history, newest first.
    pub async fn history(
        &self,
        project_id: ProjectId,
        id: AlertRuleId,
        before: Option<(Timestamp, Uuid)>,
        limit: u32,
    ) -> Result<Vec<AlertEvent>, AlertError> {
        self.find(project_id, id).await?;
        Ok(self.store.alert_events().list(id, before, limit).await?)
    }

    /// What `spec` would see if it were evaluated now: the series of the window that ends at the last whole
    /// minute at least 30 s ago, each with whether it breaches. Nothing is stored and nobody is notified.
    pub async fn preview(&self, project: &Project, spec: &RuleSpec) -> Result<Preview, AlertError> {
        let issues = spec.issues();
        if !issues.is_empty() {
            return Err(AlertError::Invalid(issues));
        }
        let source = self.source.as_ref().ok_or(AlertError::NoTelemetry)?;
        let lag = i64::try_from(EVALUATION_LAG.as_micros()).unwrap_or(i64::MAX);
        let end = self
            .env
            .clock
            .wall_now()
            .saturating_add_micros(-lag)
            .floor_minute();
        let start = end.saturating_add_micros(-i64::from(spec.window_secs).saturating_mul(1_000_000));
        let window = TimeWindow::new(start, end)
            .map_err(|_| AlertError::Read("the window is empty".to_owned()))?;
        let observed = source
            .read(project, spec, window)
            .await
            .map_err(|failure| AlertError::Read(failure.0))?;
        Ok(Preview {
            window,
            series: observed
                .series
                .into_iter()
                .map(|(key, value)| PreviewSeries {
                    breach: spec.comparison.holds(value, spec.threshold),
                    key,
                    value,
                })
                .collect(),
            truncated: observed.truncated,
        })
    }
}
