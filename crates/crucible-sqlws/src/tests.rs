//! The workspace over the simulation's scripted SQL store and the control-store model, on the simulated clock: authorization,
//! the lexical gate, limits, the audit rows, cancellation, lazy provisioning, the self-probe gate and the
//! reconciler. Isolation itself is verified only against real ClickHouse, by the `sqliso` suite.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crucible_control::{
    ControlStore, JsonDoc, LEASE_TTL, LeaseName, Project, Repos as _, SavedParam, SqlActor,
    SqlIdentityState, SqlParamType, SqlRunKind, SqlRunOutcome, SqlVisibility,
};
use crucible_env::{Env, ProbeBreach, SqlError};
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use crucible_sim::pg_model::{PgFaults, PgModel};
use crucible_sim::sql_stub::{ScriptedSqlStore, SqlScript};

use crate::reconciler::{CONFIRM_EVERY, IDLE_DROP};
use crate::{ExportFormat, RunInput, SavedInput, SqlCaller, SqlProblemCode, SqlRole, SqlWorkspace};

struct World {
    env: Env,
    model: PgModel,
    stub: Arc<ScriptedSqlStore>,
    workspace: SqlWorkspace,
    project: ProjectId,
}

fn id(n: u8) -> Uuid {
    Uuid::from_bytes([n; 16])
}

impl World {
    fn caller(&self, role: SqlRole, user: u8) -> SqlCaller {
        SqlCaller {
            scope: crucible_auth::test_support::scope(self.project).unwrap(),
            actor: SqlActor::User(UserId::from_uuid(id(user))),
            role,
        }
    }

    fn member(&self) -> SqlCaller {
        self.caller(SqlRole::Member, 1)
    }

    async fn project_row(&self) -> Project {
        self.model
            .projects()
            .get(self.project)
            .await
            .unwrap()
            .unwrap()
    }
}

fn project(project_id: ProjectId, slug: &str) -> Project {
    Project {
        id: project_id,
        name: slug.to_owned(),
        slug: slug.to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

/// Runs `body` on a simulated host with one project, a clean probe already run.
fn sim<T>(body: impl AsyncFnOnce(World) -> T + 'static) -> T {
    crucible_sim::run(7, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let env = host.env.clone();
        let model = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        let project_id = ProjectId::from_uuid(id(0xa1));
        model
            .projects()
            .insert(&project(project_id, "a"))
            .await
            .unwrap();
        let stub = ScriptedSqlStore::new(&env);
        let control: Arc<dyn ControlStore> = Arc::new(model.clone());
        let workspace = SqlWorkspace::new(env.clone(), stub.clone(), control, true);
        workspace.probe().await;
        body(World {
            env,
            model,
            stub,
            workspace,
            project: project_id,
        })
        .await
    })
}

const TEXT: &str = "SELECT name, n FROM crucible_sql.spans WHERE start_time >= {from:DateTime64(6)} AND start_time < \
                    {to:DateTime64(6)}";

fn input(sql: &str) -> RunInput {
    RunInput {
        sql: sql.to_owned(),
        from: Timestamp::parse_rfc3339("2026-11-17T00:00:00Z").unwrap(),
        to: Timestamp::parse_rfc3339("2026-11-18T00:00:00Z").unwrap(),
        params: BTreeMap::new(),
    }
}

#[test]
fn the_probe_gates_the_workspace() {
    sim(async |w| {
        // A fresh workspace refuses until its first probe completes.
        let control: Arc<dyn ControlStore> = Arc::new(w.model.clone());
        let fresh = SqlWorkspace::new(w.env.clone(), w.stub.clone(), control, true);
        let status = fresh.status();
        assert_eq!(
            (status.enabled, status.reason),
            (false, Some("probe_pending"))
        );
        let problem = fresh.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::Unavailable);
        assert_eq!(problem.reason.as_deref(), Some("probe_pending"));
        // A probe that cannot run leaves it disabled, with its own reason.
        w.stub
            .set_probe(Err(SqlError::Unavailable("down".to_owned())));
        assert_eq!(fresh.probe().await.reason, Some("probe_unavailable"));
        // A clean probe enables it and records the version.
        w.stub.set_probe(Ok(Vec::new()));
        let status = fresh.probe().await;
        assert!(status.enabled && status.reason.is_none());
        assert_eq!(status.version.as_deref(), Some("26.3.1.1"));
        // A probe that cannot run does not disable a working workspace.
        w.stub
            .set_probe(Err(SqlError::Unavailable("down".to_owned())));
        assert!(fresh.probe().await.enabled);
        // Any breach disables it, names the check, and stays until a clean probe.
        w.stub.set_probe(Ok(vec![ProbeBreach {
            check: "cross_project_read",
            detail: "the statement answered something else".to_owned(),
        }]));
        let status = fresh.probe().await;
        assert_eq!(
            (status.enabled, status.reason),
            (false, Some("isolation_probe_failed"))
        );
        assert_eq!(status.failed_checks, ["cross_project_read"]);
        let problem = fresh.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.reason.as_deref(), Some("isolation_probe_failed"));
        assert!(problem.retry_after.is_none());
        assert!(w.stub.calls().ran.is_empty());
        w.stub
            .set_probe(Err(SqlError::Unavailable("down".to_owned())));
        assert_eq!(fresh.probe().await.reason, Some("isolation_probe_failed"));
        w.stub.set_probe(Ok(Vec::new()));
        assert!(fresh.probe().await.enabled);
    });
}

#[test]
fn a_run_provisions_on_first_use_sends_the_text_verbatim_and_is_recorded() {
    sim(async |w| {
        let output = w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        assert_eq!(output.grid.rows.len(), 2);
        assert_eq!(output.estimate.rows, 10);
        {
            let calls = w.stub.calls();
            assert_eq!(calls.provisions.len(), 1);
            assert_eq!(calls.provisions[0].0, w.project);
            assert_eq!(calls.provisions[0].1, 0);
            let (project, query) = &calls.ran[0];
            assert_eq!(*project, w.project);
            assert_eq!(query.sql, TEXT, "the text is sent verbatim");
            let params: Vec<(&str, &str)> = query
                .params
                .iter()
                .map(|p| (p.name.as_str(), p.value.as_str()))
                .collect();
            assert_eq!(
                params,
                [
                    ("from", "2026-11-17 00:00:00.000000"),
                    ("to", "2026-11-18 00:00:00.000000")
                ]
            );
            // The server validated the same text as the same identity generation, under another query id.
            assert_eq!(calls.validated[0].1.sql, TEXT);
            assert_eq!(calls.validated[0].0, w.project);
            assert_ne!(calls.validated[0].1.query_id, query.query_id);
        }
        let identity = w
            .model
            .sql_identities()
            .get(w.project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.state, SqlIdentityState::Active);
        let runs = w.model.sql_runs().list(w.project, 10).await.unwrap();
        assert_eq!(runs.len(), 1);
        let run = &runs[0];
        assert_eq!(
            (run.id, run.kind, run.sql.as_str()),
            (output.run_id, SqlRunKind::AdHoc, TEXT)
        );
        let end = run.end.as_ref().unwrap();
        assert_eq!(
            (end.outcome, end.result_rows, end.rows_read),
            (SqlRunOutcome::Succeeded, 2, 10)
        );
        // The second run finds the identity.
        w.env.clock.sleep(Duration::from_secs(1)).await;
        w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        assert_eq!(w.stub.calls().provisions.len(), 1);
    });
}

#[test]
fn a_refused_text_never_reaches_the_store_and_says_how_to_fix_it() {
    sim(async |w| {
        for (sql, code, hint) in [
            (
                "SELECT 1",
                SqlProblemCode::TimeWindowRequired,
                "{from:DateTime64(6)}",
            ),
            ("DROP TABLE x", SqlProblemCode::Invalid, "SELECT or WITH"),
            (
                &format!("{TEXT} SETTINGS max_threads = 9"),
                SqlProblemCode::Invalid,
                "SETTINGS",
            ),
            (
                &format!("{TEXT} FORMAT CSV"),
                SqlProblemCode::Invalid,
                "Export",
            ),
            (
                &format!("{TEXT}; SELECT 2"),
                SqlProblemCode::Invalid,
                "one statement",
            ),
        ] {
            let problem = w.workspace.run(w.member(), &input(sql)).await.unwrap_err();
            assert_eq!(problem.code, code, "{sql}");
            assert!(problem.fix.contains(hint), "{}", problem.fix);
            assert!(problem.line.is_some());
        }
        let mut with_param = input(&format!("{TEXT} AND name = {{name:String}}"));
        let problem = w.workspace.run(w.member(), &with_param).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::Validation);
        with_param.params.insert("name".to_owned(), "x".to_owned());
        assert!(w.workspace.run(w.member(), &with_param).await.is_ok());
        let mut reversed = input(TEXT);
        std::mem::swap(&mut reversed.from, &mut reversed.to);
        assert_eq!(
            w.workspace
                .run(w.member(), &reversed)
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Validation
        );
        // Only the one accepted run reached the store or the audit table.
        assert_eq!(w.stub.calls().ran.len(), 1);
        assert_eq!(
            w.model.sql_runs().list(w.project, 10).await.unwrap().len(),
            1
        );
    });
}

#[test]
fn store_errors_keep_their_class_and_are_recorded() {
    sim(async |w| {
        let errors = [
            (
                SqlError::Invalid {
                    code: 47,
                    message: "Unknown expression identifier `nme`".to_owned(),
                    line: None,
                    column: None,
                },
                "sql_invalid",
            ),
            (
                SqlError::Forbidden {
                    code: 497,
                    message: "Not enough privileges".to_owned(),
                },
                "sql_forbidden",
            ),
            (SqlError::TimeWindowRequired, "sql_time_window_required"),
            (
                SqlError::TooBroad {
                    code: 158,
                    message: "Limit for rows exceeded".to_owned(),
                },
                "query_too_broad",
            ),
            (SqlError::Timeout, "query_timeout"),
            (SqlError::MemoryExceeded, "query_memory_exceeded"),
            (
                SqlError::QuotaExceeded {
                    message: "Quota exceeded".to_owned(),
                },
                "sql_quota_exceeded",
            ),
        ];
        for (n, (error, code)) in errors.into_iter().enumerate() {
            w.stub.script([SqlScript::Fail(error)]);
            // Another actor each time, so the per-actor rate is not what refuses.
            let caller = w.caller(SqlRole::Member, 10 + n as u8);
            let problem = w.workspace.run(caller, &input(TEXT)).await.unwrap_err();
            assert_eq!(problem.code.as_str(), code);
            assert!(!problem.fix.is_empty());
            let runs = w.model.sql_runs().list(w.project, 1).await.unwrap();
            let end = runs[0].end.as_ref().unwrap();
            assert_eq!(end.outcome, SqlRunOutcome::Failed);
            assert_eq!(end.error_code.as_deref(), Some(code));
            w.env.clock.sleep(Duration::from_millis(10)).await;
        }
    });
}

#[test]
fn an_identity_missing_in_clickhouse_is_provisioned_again_once() {
    sim(async |w| {
        w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        w.stub.script([SqlScript::Fail(SqlError::Unavailable(
            "the project's workspace identity is not provisioned".to_owned(),
        ))]);
        assert!(w.workspace.run(w.member(), &input(TEXT)).await.is_ok());
        assert_eq!(w.stub.calls().provisions.len(), 2);
        // A store that stays down is reported, not retried forever.
        let down = || SqlScript::Fail(SqlError::Unavailable("connection refused".to_owned()));
        w.stub.script([down(), down()]);
        let problem = w.workspace.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::Unavailable);
        assert_eq!(problem.reason.as_deref(), Some("store_unavailable"));
    });
}

#[test]
fn viewers_run_only_project_visible_saved_queries_with_declared_parameters() {
    sim(async |w| {
        let viewer = w.caller(SqlRole::Viewer, 2);
        let problem = w.workspace.run(viewer, &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::Forbidden);
        assert!(problem.fix.contains("saved queries"));
        assert_eq!(
            w.workspace
                .export(viewer, &input(TEXT), ExportFormat::Csv)
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Forbidden
        );
        let saved = |visibility| SavedInput {
            name: "Spans by name".to_owned(),
            description: String::new(),
            visibility,
            sql: format!("{TEXT} AND name = {{name:String}} LIMIT {{n:Int64}}"),
            parameters: vec![
                SavedParam {
                    name: "name".to_owned(),
                    ty: SqlParamType::String,
                    default: None,
                },
                SavedParam {
                    name: "n".to_owned(),
                    ty: SqlParamType::Int,
                    default: Some("10".to_owned()),
                },
            ],
        };
        assert_eq!(
            w.workspace
                .create_saved(viewer, saved(SqlVisibility::Project))
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Forbidden
        );
        let shared = w
            .workspace
            .create_saved(w.member(), saved(SqlVisibility::Project))
            .await
            .unwrap();
        let private = w
            .workspace
            .create_saved(w.member(), saved(SqlVisibility::Private))
            .await
            .unwrap();
        // The viewer sees and runs the shared one, and cannot tell that the private one exists.
        let listed = w.workspace.list_saved(viewer).await.unwrap();
        assert_eq!(listed.iter().map(|q| q.id).collect::<Vec<_>>(), [shared.id]);
        assert_eq!(
            w.workspace
                .saved(viewer, private.id)
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::NotFound
        );
        let (from, to) = (input(TEXT).from, input(TEXT).to);
        let values = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect()
        };
        w.workspace
            .run_saved(viewer, shared.id, from, to, &values(&[("name", "chat")]))
            .await
            .unwrap();
        {
            let calls = w.stub.calls();
            let (_, query) = calls.ran.last().unwrap();
            assert_eq!(query.sql, shared.sql, "the stored text, not the caller's");
            assert_eq!(query.params[2].value, "chat");
            assert_eq!(query.params[3].value, "10");
        }
        let runs = w.model.sql_runs().list(w.project, 1).await.unwrap();
        assert_eq!(
            (runs[0].kind, runs[0].saved_query_id),
            (SqlRunKind::Saved, Some(shared.id))
        );
        // Only declared parameters, of the declared types.
        for bad in [
            values(&[]),
            values(&[("name", "x"), ("extra", "1")]),
            values(&[("name", "x"), ("n", "1 OR 1")]),
            values(&[("name", "x"), ("from", "2020-01-01")]),
        ] {
            w.env.clock.sleep(Duration::from_millis(10)).await;
            let problem = w
                .workspace
                .run_saved(viewer, shared.id, from, to, &bad)
                .await
                .unwrap_err();
            assert_eq!(problem.code, SqlProblemCode::Validation, "{bad:?}");
        }
        assert_eq!(
            w.workspace
                .run_saved(viewer, private.id, from, to, &values(&[("name", "x")]))
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::NotFound
        );
        // Edits: the owner with the current version; a stale version is a precondition failure.
        let mut change = saved(SqlVisibility::Project);
        change.name = "Renamed".to_owned();
        let updated = w
            .workspace
            .update_saved(w.member(), shared.id, 1, change.clone())
            .await
            .unwrap();
        assert_eq!((updated.version, updated.name.as_str()), (2, "Renamed"));
        assert_eq!(
            w.workspace
                .update_saved(w.member(), shared.id, 1, change.clone())
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::PreconditionFailed
        );
        let other = w.caller(SqlRole::Member, 3);
        assert_eq!(
            w.workspace
                .update_saved(other, shared.id, 2, change.clone())
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Forbidden
        );
        // A saved query whose text and declarations disagree is refused when it is saved.
        let mut undeclared = saved(SqlVisibility::Project);
        undeclared.parameters.pop();
        assert_eq!(
            w.workspace
                .create_saved(w.member(), undeclared)
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Validation
        );
        w.workspace
            .delete_saved(w.member(), shared.id)
            .await
            .unwrap();
        assert!(w.workspace.list_saved(viewer).await.unwrap().is_empty());
    });
}

#[test]
fn limits_refuse_a_second_run_and_a_dropped_run_is_recorded_as_cancelled() {
    sim(async |w| {
        w.stub.script([SqlScript::Slow(Duration::from_secs(20))]);
        let workspace = w.workspace.clone();
        let caller = w.member();
        let slow = w.env.spawn("slow-run", async move {
            workspace.run(caller, &input(TEXT)).await
        });
        w.env.clock.sleep(Duration::from_secs(1)).await;
        // The same actor may not start another; someone else may.
        let problem = w.workspace.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::QuotaExceeded);
        assert!(problem.fix.contains("already have a query running"));
        assert!(
            w.workspace
                .run(w.caller(SqlRole::Member, 5), &input(TEXT))
                .await
                .is_ok()
        );
        // A cancel names the run; only its starter or an admin may.
        let running = w.model.sql_runs().list(w.project, 10).await.unwrap();
        let running = running
            .iter()
            .find(|run| run.end.is_none())
            .unwrap()
            .clone();
        assert_eq!(
            w.workspace
                .cancel(w.caller(SqlRole::Member, 5), running.id)
                .await
                .unwrap_err()
                .code,
            SqlProblemCode::Forbidden
        );
        w.workspace.cancel(w.member(), running.id).await.unwrap();
        assert_eq!(w.stub.calls().cancelled, [(w.project, running.query_id)]);
        // The client goes away: the run's future is dropped, and the audit row says cancelled.
        slow.abort();
        w.env.clock.sleep(Duration::from_secs(1)).await;
        let row = w
            .model
            .sql_runs()
            .get(w.project, running.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.end.unwrap().outcome, SqlRunOutcome::Cancelled);
        // The slot is free again.
        assert!(w.workspace.run(w.member(), &input(TEXT)).await.is_ok());
        // History: a member sees its own runs, an admin everyone's.
        let own = w.workspace.history(w.member(), 50).await.unwrap();
        assert!(own.iter().all(|run| run.actor == w.member().actor));
        assert_eq!(own.len(), 2);
        assert_eq!(
            w.workspace
                .history(w.caller(SqlRole::Admin, 9), 50)
                .await
                .unwrap()
                .len(),
            3
        );
    });
}

#[test]
fn csv_exports_carry_the_formula_guard_and_other_formats_pass_through() {
    sim(async |w| {
        let csv = w
            .workspace
            .export(w.member(), &input(TEXT), ExportFormat::Csv)
            .await
            .unwrap();
        assert_eq!(csv, "name,n\r\n'=cmd(),-3\r\nplain,4\r\n");
        w.env.clock.sleep(Duration::from_millis(10)).await;
        let key = SqlCaller {
            actor: SqlActor::ApiKey(ApiKeyId::from_uuid(id(7))),
            ..w.member()
        };
        let parquet = w
            .workspace
            .export(key, &input(TEXT), ExportFormat::Parquet)
            .await
            .unwrap();
        assert_eq!(parquet, "Parquet");
        w.env.clock.sleep(Duration::from_millis(10)).await;
        let jsonl = w
            .workspace
            .export(key, &input(TEXT), ExportFormat::JsonLines)
            .await
            .unwrap();
        assert_eq!(jsonl, "JsonLines");
        let runs = w.model.sql_runs().list(w.project, 10).await.unwrap();
        assert!(runs.iter().all(|run| run.kind == SqlRunKind::Export));
        assert!(runs.iter().any(|run| run.actor == key.actor));
    });
}

#[test]
fn the_reconciler_provisions_rewrites_rotates_and_drops_under_its_lease() {
    sim(async |w| {
        // First use provisions on demand.
        w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert!(report.leader);
        assert_eq!(
            (
                report.provisioned,
                report.rewritten,
                report.rotated,
                report.dropped
            ),
            (0, 0, 0, 0)
        );
        // A retention change rewrites the project's policies with the new value.
        let mut project_row = w.project_row().await;
        project_row.retention_days = Some(30);
        assert!(
            w.model
                .projects()
                .update_if_version(&project_row, 1)
                .await
                .unwrap()
        );
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert_eq!(report.rewritten, 1);
        {
            let calls = w.stub.calls();
            let (_, generation, retention) = calls.provisions.last().unwrap();
            assert_eq!((*generation, retention.retention_days), (0, Some(30)));
        }
        // Rotation: the target generation moves, the reconciler provisions with it, and runs then use it.
        assert_eq!(w.workspace.rotate_identities().await.unwrap(), 1);
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert_eq!(report.rotated, 1);
        let identity = w
            .model
            .sql_identities()
            .get(w.project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((identity.generation, identity.target_generation), (1, 1));
        assert_eq!(w.stub.calls().provisions.last().unwrap().1, 1);
        w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        assert_eq!(w.stub.calls().ran.last().unwrap().1.generation, 1);
        // Nothing to do, then a confirmation after an hour.
        assert_eq!(
            w.workspace
                .reconcile_once("node-a")
                .await
                .unwrap()
                .confirmed,
            0
        );
        w.env.clock.sleep(CONFIRM_EVERY).await;
        assert_eq!(
            w.workspace
                .reconcile_once("node-a")
                .await
                .unwrap()
                .confirmed,
            1
        );
        // Another node holds the lease: this node does nothing.
        let other = w
            .model
            .leases()
            .acquire(LeaseName::SqlIdentities, "node-b", LEASE_TTL)
            .await
            .unwrap()
            .unwrap();
        let before = w.stub.calls().provisions.len();
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert!(!report.leader);
        assert_eq!(w.stub.calls().provisions.len(), before);
        w.model.leases().release(&other).await.unwrap();
        // Thirty idle days: the identity is dropped, and the next use provisions it again.
        w.env.clock.sleep(IDLE_DROP).await;
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert_eq!(report.dropped, 1);
        assert_eq!(w.stub.calls().deprovisions, [w.project]);
        assert!(
            w.model
                .sql_identities()
                .get(w.project)
                .await
                .unwrap()
                .is_none()
        );
        // The run rows of 30 days ago are purged with it.
        assert!(report.runs_purged >= 2);
        w.workspace.run(w.member(), &input(TEXT)).await.unwrap();
        let identity = w
            .model
            .sql_identities()
            .get(w.project)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(identity.state, SqlIdentityState::Active);
        // A deleted project loses its identity at the next pass.
        let mut project_row = w.project_row().await;
        project_row.deleted_at = Some(w.env.clock.wall_now());
        assert!(
            w.model
                .projects()
                .update_if_version(&project_row, 2)
                .await
                .unwrap()
        );
        let report = w.workspace.reconcile_once("node-a").await.unwrap();
        assert_eq!(report.dropped, 1);
        assert_eq!(w.stub.calls().deprovisions.len(), 2);
        let problem = w.workspace.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::NotFound);
    });
}

#[test]
fn the_schema_browser_lists_the_curated_views_with_their_window_column() {
    sim(async |w| {
        let _ = &w;
        let schema = crate::schema();
        assert_eq!(schema.len(), crucible_schema::CURATED_VIEWS.len());
        let spans = schema
            .iter()
            .find(|r| r.name == "crucible_sql.spans")
            .unwrap();
        assert_eq!(spans.time_column, "start_time");
        assert!(spans.window_required);
        assert!(spans.columns.iter().any(|(name, _)| *name == "project_id"));
        // Every example passes the lexical check, so a user who starts from one is never refused.
        for relation in &schema {
            assert!(
                crate::lexer::check(&relation.example).is_ok(),
                "{}",
                relation.example
            );
        }
    });
}

#[test]
fn a_hang_ends_at_the_callers_deadline_and_a_disconnect_is_reported_as_unavailable() {
    sim(async |w| {
        // The store never answers: the request's deadline drops the run, and the audit row says cancelled.
        w.stub.script([SqlScript::Hang]);
        let text = input(TEXT);
        let run = w.workspace.run(w.member(), &text);
        assert!(w.env.timeout(Duration::from_secs(40), run).await.is_err());
        w.env.clock.sleep(Duration::from_secs(1)).await;
        let runs = w.model.sql_runs().list(w.project, 1).await.unwrap();
        assert_eq!(
            runs[0].end.as_ref().unwrap().outcome,
            SqlRunOutcome::Cancelled
        );
        // A lost connection: the identity is provisioned again and the run retried once; a second loss is
        // reported with its reason, and the row says failed.
        w.stub
            .script([SqlScript::Disconnect, SqlScript::Disconnect]);
        let problem = w.workspace.run(w.member(), &input(TEXT)).await.unwrap_err();
        assert_eq!(problem.code, SqlProblemCode::Unavailable);
        assert_eq!(problem.reason.as_deref(), Some("store_unavailable"));
        let runs = w.model.sql_runs().list(w.project, 1).await.unwrap();
        let end = runs[0].end.as_ref().unwrap();
        assert_eq!(end.outcome, SqlRunOutcome::Failed);
        assert_eq!(end.error_code.as_deref(), Some("sql_workspace_unavailable"));
        // One loss, then an answer: the caller never sees it.
        w.stub.script([SqlScript::Disconnect]);
        assert!(w.workspace.run(w.member(), &input(TEXT)).await.is_ok());
        // Every call reached the store as the requesting project.
        assert!(
            w.stub
                .calls()
                .ran
                .iter()
                .all(|(project, _)| *project == w.project)
        );
    });
}
