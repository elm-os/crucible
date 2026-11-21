//! From the moment a project is deleted, the jobs of alerting skip it (contract 13.4): the alert evaluator runs
//! none of its rules and the notification outbox sends nothing to its channels. Each check runs the same work for
//! a live project too, where the job does act, so it holds because of the deletion alone. The SQL reconciler's
//! check is in `crucible-sqlws` and the export job's in `crucible-export`.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use crucible_alerts::schedule::current_seq;
use crucible_alerts::{
    Comparison, Evaluator, NoDataPolicy, Observed, ReadFailure, RuleSpec, SeriesSource,
    evaluation_job,
};
use crucible_auth::Sealer;
use crucible_control::{
    AlertRule, Channel, ChannelKind, ControlStore, JsonDoc, Project, Queue, Repos as _,
};
use crucible_jobs::ClaimCtx;
use crucible_notify::{Delivery, NoticeKind, Notification, Notifier, Transport};
use crucible_plan::{Aggregation, MeasureId, Source};
use crucible_records::{AlertRuleId, ChannelId, ProjectId, TimeWindow, Timestamp, Uuid};
use crucible_sim::pg_model::{PgFaults, PgModel};

const LIVE: ProjectId = ProjectId::from_bytes([1; 16]);
const DELETED: ProjectId = ProjectId::from_bytes([2; 16]);

fn project(id: ProjectId, deleted_at: Option<Timestamp>) -> Project {
    Project {
        id,
        name: id.to_string(),
        slug: id.to_string(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at,
    }
}

/// A control store with a live project and a deleted one.
async fn projects(model: &PgModel, now: Timestamp) {
    for row in [project(LIVE, None), project(DELETED, Some(now))] {
        model.projects().insert(&row).await.unwrap();
    }
}

/// What a test saw a job do, per project.
#[derive(Debug, Default)]
struct Seen(Mutex<Vec<ProjectId>>);

impl Seen {
    fn push(&self, project: ProjectId) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(project);
    }

    fn all(&self) -> Vec<ProjectId> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl Transport for Seen {
    async fn deliver(
        &self,
        channel: &Channel,
        _: Option<&[u8]>,
        _: &Notification,
        _: Uuid,
    ) -> Delivery {
        self.push(channel.project_id);
        Delivery {
            ok: true,
            status_class: Some("2xx"),
            error_code: None,
            latency_ms: 0,
        }
    }

    fn has_mailer(&self) -> bool {
        false
    }
}

#[async_trait]
impl SeriesSource for Seen {
    async fn read(
        &self,
        project: &Project,
        _: &RuleSpec,
        _: TimeWindow,
    ) -> Result<Observed, ReadFailure> {
        self.push(project.id);
        Ok(Observed::default())
    }
}

fn channel(project_id: ProjectId, n: u8) -> Channel {
    Channel {
        id: ChannelId::from_bytes([n; 16]),
        project_id,
        name: format!("hook {n}"),
        kind: ChannelKind::Webhook,
        config: JsonDoc::empty(),
        secret_id: None,
        enabled: true,
        disabled_reason: None,
        failures: 0,
        last_test: None,
        version: 1,
    }
}

fn notification(channel: &Channel, at: Timestamp) -> Notification {
    Notification {
        channel_id: channel.id.uuid(),
        project_id: channel.project_id.uuid(),
        rule_id: Uuid::from_bytes([9; 16]),
        rule_name: "errors".to_owned(),
        kind: NoticeKind::Firing,
        series: Vec::new(),
        measure: "error_count".to_owned(),
        aggregation: "sum".to_owned(),
        comparison: "gt".to_owned(),
        threshold: "5".to_owned(),
        value: Some("9".to_owned()),
        window_secs: 60,
        at_micros: at.micros(),
        eval_seq: 1,
    }
}

/// Claims every due row of `queue` and hands each to `handle`.
async fn claim_all<F, Fut>(model: &PgModel, env: &crucible_env::Env, queue: Queue, handle: F)
where
    F: Fn(ClaimCtx) -> Fut,
    Fut: Future<Output = crucible_jobs::JobResult>,
{
    let store: Arc<dyn ControlStore> = Arc::new(model.clone());
    let claims = model
        .claims()
        .claim(queue, 10, Duration::from_secs(60))
        .await
        .unwrap();
    assert!(!claims.is_empty(), "nothing to claim in {queue:?}");
    for claim in claims {
        handle(ClaimCtx {
            env: env.clone(),
            store: Arc::clone(&store),
            claim,
        })
        .await
        .unwrap();
    }
}

#[test]
fn the_outbox_sends_nothing_to_the_channels_of_a_deleted_project() {
    crucible_sim::run(71, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let model = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        let now = host.env.clock.wall_now();
        projects(&model, now).await;
        for (n, project_id) in [(1, LIVE), (2, DELETED)] {
            let channel = channel(project_id, n);
            model.channels().insert(&channel).await.unwrap();
            let job = notification(&channel, now)
                .outbox_job(Uuid::from_bytes([n + 10; 16]), "")
                .unwrap();
            model
                .claims()
                .enqueue(Queue::NotificationOutbox, &job)
                .await
                .unwrap();
        }
        let seen = Arc::new(Seen::default());
        let notifier = Notifier::new(
            Arc::clone(&seen) as Arc<dyn Transport>,
            Arc::new(Sealer::new("master-0", [7; 32])),
        );
        claim_all(&model, &host.env, Queue::NotificationOutbox, |ctx| {
            let notifier = notifier.clone();
            async move { notifier.handle(ctx).await }
        })
        .await;
        assert_eq!(seen.all(), [LIVE]);
    });
}

#[test]
fn the_evaluator_runs_none_of_the_rules_of_a_deleted_project() {
    crucible_sim::run(72, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let model = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        let now = model.db_now().await.unwrap();
        projects(&model, now).await;
        let spec = RuleSpec {
            source: Source::Spans,
            mcp: None,
            measure: MeasureId::ErrorCount,
            aggregation: Aggregation::Sum,
            dimensions: Vec::new(),
            comparison: Comparison::Gt,
            threshold: 5,
            window_secs: 60,
            interval_secs: 60,
            for_secs: 0,
            no_data: NoDataPolicy::Keep,
            no_data_for_secs: 180,
            renotify_secs: 300,
            channels: Vec::new(),
        };
        let mut rules = Vec::new();
        for (n, project_id) in [(1u8, LIVE), (2, DELETED)] {
            let rule = AlertRule {
                id: AlertRuleId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000, u128::from(n))),
                project_id,
                name: format!("errors {n}"),
                enabled: true,
                spec: spec.to_doc(),
                interval_secs: 60,
                version: 1,
                created_at: now,
                updated_at: now,
                last_eval_seq: None,
                eval_failures: 0,
                skipped_ticks: 0,
            };
            model.alert_rules().insert(&rule).await.unwrap();
            // The tick due now, so that only the project's deletion can keep the evaluator from it.
            let seq = current_seq(rule.id, 60, now).unwrap();
            let job = evaluation_job(Uuid::from_bytes([n + 20; 16]), rule.id, 60, seq);
            model
                .claims()
                .enqueue(Queue::AlertEvaluations, &job)
                .await
                .unwrap();
            rules.push((rule.id, seq));
        }
        let seen = Arc::new(Seen::default());
        let evaluator = Evaluator::new(Arc::clone(&seen) as Arc<dyn SeriesSource>);
        claim_all(&model, &host.env, Queue::AlertEvaluations, |ctx| {
            let evaluator = evaluator.clone();
            async move { evaluator.handle(ctx).await }
        })
        .await;
        assert_eq!(seen.all(), [LIVE]);
        let applied = |id| {
            let model = model.clone();
            async move {
                model
                    .alert_rules()
                    .get(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .last_eval_seq
            }
        };
        assert_eq!(applied(rules[0].0).await, Some(rules[0].1));
        assert_eq!(applied(rules[1].0).await, None);
    });
}
