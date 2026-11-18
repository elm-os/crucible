//! [`SqlWorkspace`]: what the API calls. Each run is authorized, checked lexically, bound, admitted by the
//! limiter, given the project's ClickHouse identity, validated by the server, executed verbatim and recorded in
//! `sql_runs` (contract 7.4).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crucible_control::{
    AppliedRetention, ControlStore, Project, SavedParam, SavedQuery, SqlActor, SqlIdentity,
    SqlIdentityState, SqlRun, SqlRunEnd, SqlRunKind, SqlRunOutcome, SqlVisibility, TxError,
};
use crucible_env::{
    Env, ProbeReport, SqlError, SqlEstimate, SqlExportFormat, SqlGrid, SqlLimits, SqlParam,
    SqlQuery, SqlWorkspaceStore,
};
use crucible_records::{ProjectId, ProjectScope, SavedQueryId, SqlRunId, Timestamp};
use crucible_schema::{CURATED_VIEWS, RetentionSettings, SQL_DATABASE, table};

use crate::access::{SqlAction, SqlRole, allowed};
use crate::csv::to_csv;
use crate::lexer::{Checked, check};
use crate::limits::Limiter;
use crate::params::{bind, bind_saved, check_declarations};
use crate::problem::{SqlProblem, SqlProblemCode};

/// How often the runtime self-probe runs (contract 7.5).
pub const PROBE_EVERY: Duration = Duration::from_secs(300);
/// How soon the probe runs again while it has never completed.
pub const PROBE_RETRY: Duration = Duration::from_secs(30);
/// The most rows of history one request returns.
pub const MAX_HISTORY: u32 = 200;
/// The byte bound of an export the server encodes.
const EXPORT_BYTES: usize = 32 << 20;
/// The row bound of an export: what a grid holds.
const EXPORT_ROWS: usize = 10_000;

fn lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count()
}

/// Who calls: the scope from the permission extractor, the actor and the effective project role.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SqlCaller {
    /// The project the caller is authorized for.
    pub scope: ProjectScope,
    /// The signed-in user or the API key.
    pub actor: SqlActor,
    /// The effective project role.
    pub role: SqlRole,
}

/// Whether the workspace serves requests, and if not, why (contract 12.7, `features.sql_workspace`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SqlStatus {
    /// Whether runs are accepted.
    pub enabled: bool,
    /// Why not: `probe_pending`, `probe_unavailable` or `isolation_probe_failed`.
    pub reason: Option<&'static str>,
    /// The checks of the self-probe that failed, by name.
    pub failed_checks: Vec<&'static str>,
    /// The ClickHouse version of the last completed probe.
    pub version: Option<String>,
    /// When the probe last completed.
    pub last_probe: Option<Timestamp>,
}

/// One relation of the schema browser.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SchemaRelation {
    /// The qualified name, for example `crucible_sql.spans`.
    pub name: String,
    /// One sentence about it.
    pub description: &'static str,
    /// The column to filter with the window.
    pub time_column: &'static str,
    /// Whether an unwindowed read is refused (the relation is partitioned by time).
    pub window_required: bool,
    /// The columns: name and ClickHouse type.
    pub columns: Vec<(&'static str, String)>,
    /// A query to start from.
    pub example: String,
}

/// The text and window of an ad hoc run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RunInput {
    /// The query text.
    pub sql: String,
    /// The window start, bound to `{from}`.
    pub from: Timestamp,
    /// The window end, bound to `{to}`.
    pub to: Timestamp,
    /// The values of the other parameters.
    pub params: BTreeMap<String, String>,
}

/// A finished run.
#[derive(Clone, PartialEq, Debug)]
pub struct RunOutput {
    /// The audit row's id; a cancel names it.
    pub run_id: SqlRunId,
    /// The result.
    pub grid: SqlGrid,
    /// What the server expected to read.
    pub estimate: SqlEstimate,
}

/// The export formats of contract 12.6.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ExportFormat {
    /// CSV with the formula guard.
    Csv,
    /// One JSON object per line.
    JsonLines,
    /// Apache Parquet.
    Parquet,
}

impl ExportFormat {
    /// The media type of a download.
    pub const fn content_type(&self) -> &'static str {
        match self {
            Self::Csv => "text/csv; charset=utf-8",
            Self::JsonLines => "application/x-ndjson",
            Self::Parquet => "application/vnd.apache.parquet",
        }
    }

    /// The file extension of a download.
    pub const fn extension(&self) -> &'static str {
        match self {
            Self::Csv => "csv",
            Self::JsonLines => "jsonl",
            Self::Parquet => "parquet",
        }
    }
}

/// The fields of a saved query a caller sets.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SavedInput {
    /// The display name.
    pub name: String,
    /// What it is for.
    pub description: String,
    /// Who may see it.
    pub visibility: SqlVisibility,
    /// The query text.
    pub sql: String,
    /// The declared parameters.
    pub parameters: Vec<SavedParam>,
}

struct Inner {
    env: Env,
    sql: Arc<dyn SqlWorkspaceStore>,
    control: Arc<dyn ControlStore>,
    limiter: Limiter,
    status: Mutex<SqlStatus>,
    formula_guard: bool,
}

/// The SQL workspace of one node.
#[derive(Clone)]
pub struct SqlWorkspace {
    inner: Arc<Inner>,
}

impl fmt::Debug for SqlWorkspace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqlWorkspace")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

/// Marks a run cancelled if its future is dropped before it finished: the client went away.
struct RunGuard {
    inner: Arc<Inner>,
    id: SqlRunId,
    armed: bool,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let inner = Arc::clone(&self.inner);
        let id = self.id;
        // Detached: the record outlives the request that was dropped.
        drop(self.inner.env.spawn("sql-run-cancelled", async move {
            let end = SqlRunEnd {
                outcome: SqlRunOutcome::Cancelled,
                error_code: None,
                result_rows: 0,
                truncated: false,
                rows_read: 0,
                bytes_read: 0,
                elapsed_ms: 0,
                finished_at: inner.env.clock.wall_now(),
            };
            if let Err(error) = inner.control.sql_runs().finish(id, &end).await {
                tracing::warn!(%id, %error, "recording a cancelled SQL run failed");
            }
        }));
    }
}

fn retention(project: &Project) -> AppliedRetention {
    AppliedRetention {
        retention_days: project.retention_days,
        payload_retention_days: project.payload_retention_days,
        mcp_content_retention_days: project.mcp_content_retention_days,
    }
}

/// The schema crate's form of an applied retention.
pub(crate) fn retention_settings(applied: AppliedRetention) -> RetentionSettings {
    RetentionSettings {
        retention_days: applied.retention_days,
        payload_retention_days: applied.payload_retention_days,
        mcp_content_retention_days: applied.mcp_content_retention_days,
    }
}

fn require(ok: bool, fix: &str) -> Result<(), SqlProblem> {
    if ok {
        Ok(())
    } else {
        Err(SqlProblem::role(fix))
    }
}

enum Mode {
    Grid,
    Export(ExportFormat),
}

enum Output {
    Grid(SqlGrid, SqlEstimate),
    Bytes(Bytes),
}

/// The relations a query may read, for the schema browser.
pub fn schema() -> Vec<SchemaRelation> {
    CURATED_VIEWS
        .iter()
        .map(|view| {
            let def = table(view.table);
            let name = format!("{SQL_DATABASE}.{}", view.name);
            let time = def.retention.time_column;
            let example = format!(
                "SELECT *\nFROM {name}\nWHERE {time} >= {{from:DateTime64(6)}}\n  AND {time} < {{to:DateTime64(6)}}\nLIMIT 100"
            );
            SchemaRelation {
                description: view.description,
                time_column: time,
                window_required: def.partition.column().is_some(),
                columns: view
                    .columns()
                    .into_iter()
                    .map(|column| (column.name, column.ty))
                    .collect(),
                example,
                name,
            }
        })
        .collect()
}

impl SqlWorkspace {
    /// A workspace over the SQL store and the control store. It refuses runs until [`SqlWorkspace::probe`] has
    /// completed without a breach.
    pub fn new(
        env: Env,
        sql: Arc<dyn SqlWorkspaceStore>,
        control: Arc<dyn ControlStore>,
        formula_guard: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                env,
                sql,
                control,
                limiter: Limiter::default(),
                status: Mutex::new(SqlStatus {
                    enabled: false,
                    reason: Some("probe_pending"),
                    failed_checks: Vec::new(),
                    version: None,
                    last_probe: None,
                }),
                formula_guard,
            }),
        }
    }

    pub(crate) fn env(&self) -> &Env {
        &self.inner.env
    }

    pub(crate) fn store(&self) -> &dyn SqlWorkspaceStore {
        self.inner.sql.as_ref()
    }

    pub(crate) fn control(&self) -> &dyn ControlStore {
        self.inner.control.as_ref()
    }

    /// Whether the workspace serves requests.
    pub fn status(&self) -> SqlStatus {
        self.inner
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Runs the runtime self-probe (contract 7.5) and records its verdict. Any breach disables the workspace
    /// deployment-wide until a later probe is clean. A probe that could not run leaves an enabled workspace
    /// enabled: it says nothing about the access scheme, and runs fail by themselves while the store is down.
    pub async fn probe(&self) -> SqlStatus {
        let outcome = self.inner.sql.probe().await;
        let now = self.inner.env.clock.wall_now();
        let mut status = self
            .inner
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match outcome {
            Ok(ProbeReport {
                version, breaches, ..
            }) => {
                if status
                    .version
                    .as_ref()
                    .is_some_and(|known| *known != version)
                {
                    tracing::info!(%version, "the ClickHouse version changed; the self-probe ran against it");
                }
                status.version = Some(version);
                status.last_probe = Some(now);
                status.failed_checks = breaches.iter().map(|b| b.check).collect();
                if breaches.is_empty() {
                    status.enabled = true;
                    status.reason = None;
                } else {
                    tracing::error!(
                        ?breaches,
                        "the SQL workspace self-probe found a breach; the workspace is disabled"
                    );
                    status.enabled = false;
                    status.reason = Some("isolation_probe_failed");
                }
            }
            Err(error) => {
                tracing::warn!(%error, "the SQL workspace self-probe could not run");
                if !status.enabled && status.reason != Some("isolation_probe_failed") {
                    status.reason = Some("probe_unavailable");
                }
            }
        }
        status.clone()
    }

    /// Probes at startup and then every five minutes, sooner while no probe has completed. Each probe reports
    /// the server version, so a version change is probed within one period. Runs until the host shuts down.
    pub async fn run_probe_loop(self) {
        loop {
            let status = self.probe().await;
            let wait = if status.last_probe.is_some() {
                PROBE_EVERY
            } else {
                PROBE_RETRY
            };
            self.inner.env.clock.sleep(wait).await;
        }
    }

    fn enabled(&self) -> Result<(), SqlProblem> {
        let status = self.status();
        if status.enabled {
            return Ok(());
        }
        let reason = status.reason.unwrap_or("disabled");
        let fix = match reason {
            "isolation_probe_failed" => {
                "The SQL workspace is switched off: its isolation self-check failed on this ClickHouse server. An \
                 operator should read the health page, run `crucible migrate` and check the server's access \
                 configuration."
            }
            "probe_pending" => {
                "The SQL workspace is starting its isolation self-check. Try again in a moment."
            }
            _ => {
                "The SQL workspace cannot reach its ClickHouse identity. An operator should check that ClickHouse \
                 is up and run `crucible migrate`."
            }
        };
        Err(SqlProblem {
            retry_after: (reason != "isolation_probe_failed").then_some(PROBE_RETRY),
            ..SqlProblem::unavailable(reason, fix)
        })
    }

    /// The generation of the project's identity, provisioning it on first use (contract 7.4).
    async fn identity(&self, project_id: ProjectId) -> Result<u32, SqlProblem> {
        let repo = self.inner.control.sql_identities();
        let now = self.inner.env.clock.wall_now();
        match repo.get(project_id).await? {
            Some(identity) if identity.state == SqlIdentityState::Active => {
                // Best effort: a missed touch only makes the identity look idle a little longer.
                let _ = repo.touch(project_id, now).await;
                Ok(identity.generation)
            }
            Some(identity) if identity.state == SqlIdentityState::Dropping => Err(SqlProblem {
                retry_after: Some(Duration::from_secs(30)),
                ..SqlProblem::unavailable(
                    "identity_dropping",
                    "The project's SQL identity is being removed after a long idle time. Try again in a moment; it \
                     is created again on the next use.",
                )
            }),
            existing => self.provision(project_id, existing).await,
        }
    }

    /// Provisions a project's identity on first use, or again after it was found missing in ClickHouse.
    async fn provision(
        &self,
        project_id: ProjectId,
        existing: Option<SqlIdentity>,
    ) -> Result<u32, SqlProblem> {
        let repo = self.inner.control.sql_identities();
        let now = self.inner.env.clock.wall_now();
        let project = self
            .inner
            .control
            .projects()
            .get(project_id)
            .await?
            .filter(|project| project.deleted_at.is_none())
            .ok_or_else(|| SqlProblem::not_found("project"))?;
        let applied = retention(&project);
        let row = match existing {
            Some(row) => row,
            None => {
                let row = SqlIdentity {
                    project_id,
                    generation: 0,
                    target_generation: 0,
                    state: SqlIdentityState::Pending,
                    retention: applied,
                    last_probe: None,
                    last_used_at: now,
                };
                match repo.insert(&row).await {
                    Ok(()) => row,
                    // Another request created it first.
                    Err(TxError::UniqueViolation(_)) => repo.get(project_id).await?.unwrap_or(row),
                    Err(error) => return Err(error.into()),
                }
            }
        };
        let generation = row.target_generation.max(row.generation);
        self.inner
            .sql
            .provision(project_id, generation, &retention_settings(applied))
            .await
            .map_err(SqlProblem::from)?;
        let active = SqlIdentity {
            generation,
            target_generation: generation,
            state: SqlIdentityState::Active,
            retention: applied,
            last_probe: Some(now),
            last_used_at: now,
            ..row.clone()
        };
        // When the compare-and-set loses, the reconciler or another request wrote the row; provisioning is
        // idempotent, so the identity exists at this generation either way.
        let _ = repo.update_if(&active, row.state, row.generation).await?;
        Ok(generation)
    }

    async fn finish(&self, id: SqlRunId, end: SqlRunEnd) {
        if let Err(error) = self.inner.control.sql_runs().finish(id, &end).await {
            tracing::warn!(%id, %error, "recording the end of a SQL run failed");
        }
    }

    /// The common path of every run: limiter, identity, audit row, server-side validation, execution.
    async fn execute(
        &self,
        caller: SqlCaller,
        kind: SqlRunKind,
        saved_query_id: Option<SavedQueryId>,
        sql: &str,
        params: Vec<SqlParam>,
        mode: Mode,
    ) -> Result<(SqlRunId, Output), SqlProblem> {
        let project_id = caller.scope.project_id();
        let env = &self.inner.env;
        let _permit = self
            .inner
            .limiter
            .admit(caller.actor, project_id, env.clock.now())?;
        let mut generation = self.identity(project_id).await?;
        let run = SqlRun {
            id: SqlRunId::from_uuid(env.new_id()),
            project_id,
            actor: caller.actor,
            query_id: env.new_id(),
            kind,
            saved_query_id,
            sql: sql.to_owned(),
            started_at: env.clock.wall_now(),
            end: None,
        };
        // The audit row comes first: a run that cannot be recorded does not start.
        self.inner.control.sql_runs().insert(&run).await?;
        let mut guard = RunGuard {
            inner: Arc::clone(&self.inner),
            id: run.id,
            armed: true,
        };
        let mut attempt = 0;
        let outcome = loop {
            let query = SqlQuery {
                sql: sql.to_owned(),
                params: params.clone(),
                generation,
                // The retry carries the same id: the first attempt never ran, and a cancel names this id.
                query_id: run.query_id,
            };
            let outcome = self.attempt(caller.scope, &query, &mode).await;
            // The identity was dropped or rotated behind the row: provision it again, once.
            if attempt == 0 && matches!(outcome, Err(SqlError::Unavailable(_))) {
                let existing = self.inner.control.sql_identities().get(project_id).await?;
                if let Ok(next) = self.provision(project_id, existing).await {
                    generation = next;
                    attempt = 1;
                    continue;
                }
            }
            break outcome;
        };
        guard.armed = false;
        let finished_at = env.clock.wall_now();
        let mut end = SqlRunEnd {
            outcome: SqlRunOutcome::Succeeded,
            error_code: None,
            result_rows: 0,
            truncated: false,
            rows_read: 0,
            bytes_read: 0,
            elapsed_ms: 0,
            finished_at,
        };
        match outcome {
            Ok(output) => {
                if let Output::Grid(grid, _) = &output {
                    end.result_rows = grid.rows.len() as u64;
                    end.truncated = grid.truncated;
                    end.rows_read = grid.stats.rows_read;
                    end.bytes_read = grid.stats.bytes_read;
                    end.elapsed_ms = grid.stats.elapsed_ms;
                }
                self.finish(run.id, end).await;
                Ok((run.id, output))
            }
            Err(error) => {
                end.outcome = if error == SqlError::Cancelled {
                    SqlRunOutcome::Cancelled
                } else {
                    SqlRunOutcome::Failed
                };
                end.error_code = Some(error.problem_code().to_owned());
                self.finish(run.id, end).await;
                Err(error.into())
            }
        }
    }

    async fn attempt(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        mode: &Mode,
    ) -> Result<Output, SqlError> {
        let sql = self.inner.sql.as_ref();
        // `EXPLAIN AST` and `EXPLAIN ESTIMATE` as the project identity: the server's parser and access checks
        // decide before anything is read.
        let validation = SqlQuery {
            query_id: self.inner.env.new_id(),
            ..query.clone()
        };
        let estimate = sql.validate(scope, &validation).await?;
        match mode {
            Mode::Grid | Mode::Export(ExportFormat::Csv) => {
                let grid = sql.run(scope, query, SqlLimits::INTERACTIVE).await?;
                Ok(Output::Grid(grid, estimate))
            }
            Mode::Export(format) => {
                let format = match format {
                    ExportFormat::Parquet => SqlExportFormat::Parquet,
                    ExportFormat::JsonLines | ExportFormat::Csv => SqlExportFormat::JsonLines,
                };
                let limits = SqlLimits {
                    max_bytes: EXPORT_BYTES,
                    ..SqlLimits::INTERACTIVE
                };
                Ok(Output::Bytes(
                    sql.export(scope, query, format, limits).await?,
                ))
            }
        }
    }

    fn prepare(input: &RunInput) -> Result<(Checked, Vec<SqlParam>), SqlProblem> {
        let checked = check(&input.sql)?;
        let params = bind(&checked, input.from, input.to, &input.params)?;
        Ok((checked, params))
    }

    /// Runs ad hoc SQL. Members and admins only.
    pub async fn run(&self, caller: SqlCaller, input: &RunInput) -> Result<RunOutput, SqlProblem> {
        self.enabled()?;
        require(
            allowed(caller.role, caller.actor, SqlAction::RunAdHoc),
            "Running ad hoc SQL needs the member role on this project. Viewers can run the project's saved queries.",
        )?;
        let (_, params) = Self::prepare(input)?;
        let (run_id, output) = self
            .execute(
                caller,
                SqlRunKind::AdHoc,
                None,
                &input.sql,
                params,
                Mode::Grid,
            )
            .await?;
        Self::grid(run_id, output)
    }

    fn grid(run_id: SqlRunId, output: Output) -> Result<RunOutput, SqlProblem> {
        match output {
            Output::Grid(grid, estimate) => Ok(RunOutput {
                run_id,
                grid,
                estimate,
            }),
            Output::Bytes(_) => Err(SqlProblem::new(
                SqlProblemCode::Internal,
                "a grid run returned an export",
            )),
        }
    }

    /// Runs a saved query with its declared parameters. Viewers may run project-visible ones.
    pub async fn run_saved(
        &self,
        caller: SqlCaller,
        id: SavedQueryId,
        from: Timestamp,
        to: Timestamp,
        values: &BTreeMap<String, String>,
    ) -> Result<RunOutput, SqlProblem> {
        self.enabled()?;
        let query = self.saved(caller, id).await?;
        require(
            allowed(caller.role, caller.actor, SqlAction::RunSaved(&query)),
            "Viewers can run saved queries that are visible to the project. Ask a member to share this one.",
        )?;
        let checked = check(&query.sql)?;
        let params = bind_saved(&checked, &query.parameters, from, to, values)?;
        let (run_id, output) = self
            .execute(
                caller,
                SqlRunKind::Saved,
                Some(id),
                &query.sql,
                params,
                Mode::Grid,
            )
            .await?;
        Self::grid(run_id, output)
    }

    /// Exports the result of ad hoc SQL under the interactive limits. Members and admins only.
    pub async fn export(
        &self,
        caller: SqlCaller,
        input: &RunInput,
        format: ExportFormat,
    ) -> Result<Bytes, SqlProblem> {
        self.enabled()?;
        require(
            allowed(caller.role, caller.actor, SqlAction::Export),
            "Exporting needs the member role on this project.",
        )?;
        let (_, params) = Self::prepare(input)?;
        let (_, output) = self
            .execute(
                caller,
                SqlRunKind::Export,
                None,
                &input.sql,
                params,
                Mode::Export(format),
            )
            .await?;
        // An export under the interactive limits holds what a grid holds. A longer result is refused, never cut
        // without a word: a file that looks whole and is not would be worse than no file.
        let too_long = || {
            SqlProblem::new(
                SqlProblemCode::TooBroad,
                "The result is larger than an export holds (10,000 rows, 16 MiB). Narrow the time window, add a \
                 LIMIT, or aggregate.",
            )
        };
        match output {
            Output::Grid(grid, _) if grid.truncated => Err(too_long()),
            Output::Grid(grid, _) => Ok(Bytes::from(to_csv(&grid, self.inner.formula_guard))),
            Output::Bytes(bytes)
                if format == ExportFormat::JsonLines && lines(&bytes) > EXPORT_ROWS =>
            {
                Err(too_long())
            }
            Output::Bytes(bytes) => Ok(bytes),
        }
    }

    /// Cancels a run: `KILL QUERY` through `crucible_maint`, bound to the project's identity. The run's own
    /// request then ends with the cancellation and records it.
    pub async fn cancel(&self, caller: SqlCaller, run_id: SqlRunId) -> Result<(), SqlProblem> {
        let project_id = caller.scope.project_id();
        let run = self
            .inner
            .control
            .sql_runs()
            .get(project_id, run_id)
            .await?
            .ok_or_else(|| SqlProblem::not_found("run"))?;
        require(
            allowed(
                caller.role,
                caller.actor,
                SqlAction::Cancel { runner: run.actor },
            ),
            "Only the person who started a run, or a project admin, can cancel it.",
        )?;
        if run.end.is_some() {
            return Ok(());
        }
        self.inner
            .sql
            .cancel(caller.scope, run.query_id)
            .await
            .map_err(SqlProblem::from)
    }

    /// The project's recent runs, newest first. A caller sees its own runs; an admin sees everyone's.
    pub async fn history(&self, caller: SqlCaller, limit: u32) -> Result<Vec<SqlRun>, SqlProblem> {
        let actor = (caller.role != SqlRole::Admin).then_some(caller.actor);
        let runs = self
            .inner
            .control
            .sql_runs()
            .list(
                caller.scope.project_id(),
                actor,
                limit.clamp(1, MAX_HISTORY),
            )
            .await?;
        Ok(runs)
    }

    /// The saved queries the caller may see: the project-visible ones and its own.
    pub async fn list_saved(&self, caller: SqlCaller) -> Result<Vec<SavedQuery>, SqlProblem> {
        let mut queries = self
            .inner
            .control
            .saved_queries()
            .list(caller.scope.project_id())
            .await?;
        queries.retain(|query| allowed(caller.role, caller.actor, SqlAction::ViewSaved(query)));
        Ok(queries)
    }

    /// One saved query. A query the caller may not see is `not_found`.
    pub async fn saved(
        &self,
        caller: SqlCaller,
        id: SavedQueryId,
    ) -> Result<SavedQuery, SqlProblem> {
        self.inner
            .control
            .saved_queries()
            .get(caller.scope.project_id(), id)
            .await?
            .filter(|query| allowed(caller.role, caller.actor, SqlAction::ViewSaved(query)))
            .ok_or_else(|| SqlProblem::not_found("saved query"))
    }

    fn check_saved(input: &SavedInput) -> Result<(), SqlProblem> {
        let name = input.name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(SqlProblem::new(
                SqlProblemCode::Validation,
                "Give the saved query a name of 1 to 120 characters.",
            ));
        }
        if input.description.chars().count() > 2000 {
            return Err(SqlProblem::new(
                SqlProblemCode::Validation,
                "Keep the description under 2,000 characters.",
            ));
        }
        let checked = check(&input.sql)?;
        check_declarations(&checked, &input.parameters)?;
        Ok(())
    }

    /// Creates a saved query owned by the caller. Members and admins only.
    pub async fn create_saved(
        &self,
        caller: SqlCaller,
        input: SavedInput,
    ) -> Result<SavedQuery, SqlProblem> {
        require(
            allowed(caller.role, caller.actor, SqlAction::CreateSaved),
            "Saving queries needs the member role on this project.",
        )?;
        Self::check_saved(&input)?;
        let now = self.inner.env.clock.wall_now();
        let query = SavedQuery {
            id: SavedQueryId::from_uuid(self.inner.env.new_id()),
            project_id: caller.scope.project_id(),
            owner: caller.actor,
            name: input.name.trim().to_owned(),
            description: input.description,
            visibility: input.visibility,
            sql: input.sql,
            parameters: input.parameters,
            version: 1,
            created_at: now,
            updated_at: now,
        };
        self.inner.control.saved_queries().insert(&query).await?;
        Ok(query)
    }

    /// Changes a saved query if its version is still `expected_version`.
    pub async fn update_saved(
        &self,
        caller: SqlCaller,
        id: SavedQueryId,
        expected_version: u64,
        input: SavedInput,
    ) -> Result<SavedQuery, SqlProblem> {
        let stored = self.saved(caller, id).await?;
        require(
            allowed(caller.role, caller.actor, SqlAction::EditSaved(&stored)),
            "Only its owner, or a project admin for a project-visible query, can change a saved query.",
        )?;
        Self::check_saved(&input)?;
        let next = SavedQuery {
            name: input.name.trim().to_owned(),
            description: input.description,
            visibility: input.visibility,
            sql: input.sql,
            parameters: input.parameters,
            version: expected_version + 1,
            updated_at: self.inner.env.clock.wall_now(),
            ..stored
        };
        let changed = self
            .inner
            .control
            .saved_queries()
            .update_if_version(&next, expected_version)
            .await?;
        if changed {
            Ok(next)
        } else {
            Err(SqlProblem::new(
                SqlProblemCode::PreconditionFailed,
                "The saved query changed since you loaded it. Reload it and apply your change again.",
            ))
        }
    }

    /// Deletes a saved query.
    pub async fn delete_saved(
        &self,
        caller: SqlCaller,
        id: SavedQueryId,
    ) -> Result<(), SqlProblem> {
        let stored = self.saved(caller, id).await?;
        require(
            allowed(caller.role, caller.actor, SqlAction::EditSaved(&stored)),
            "Only its owner, or a project admin for a project-visible query, can delete a saved query.",
        )?;
        self.inner
            .control
            .saved_queries()
            .delete(caller.scope.project_id(), id)
            .await?;
        Ok(())
    }
}
