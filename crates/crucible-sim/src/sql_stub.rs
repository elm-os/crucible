//! The scripted `SqlWorkspaceStore` of the simulation (contract 2.9): rows, each error class, slow, hang and
//! disconnect, on the simulated clock.
//!
//! It records every call with the project the scope named, so a test can check that a run reached the store as
//! the requesting project and with the text unchanged. It decides nothing about isolation: that is verified only
//! against real ClickHouse, by the `sqliso` suite.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_env::{
    Env, ProbeBreach, ProbeReport, SqlCell, SqlColumn, SqlError, SqlEstimate, SqlExportFormat,
    SqlGrid, SqlLimits, SqlQuery, SqlStats, SqlWorkspaceStore,
};
use crucible_records::{ProjectId, ProjectScope, Uuid};
use crucible_schema::RetentionSettings;

/// What the store does on its next `run` or `export`. With no step queued it answers [`SqlScript::Rows`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SqlScript {
    /// Answers the fixed grid at once.
    Rows,
    /// Fails with this error.
    Fail(SqlError),
    /// Answers the fixed grid after this long on the simulated clock.
    Slow(Duration),
    /// Never answers; the caller's deadline or its drop ends the call.
    Hang,
    /// The connection is lost mid-answer.
    Disconnect,
}

/// The calls the store received, in order.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct SqlCalls {
    /// `provision`: the project, the generation and the retention.
    pub provisions: Vec<(ProjectId, u32, RetentionSettings)>,
    /// `deprovision`.
    pub deprovisions: Vec<ProjectId>,
    /// `validate`: the project of the scope and the query.
    pub validated: Vec<(ProjectId, SqlQuery)>,
    /// `run` and `export`: the project of the scope and the query.
    pub ran: Vec<(ProjectId, SqlQuery)>,
    /// `cancel`: the project of the scope and the query id.
    pub cancelled: Vec<(ProjectId, Uuid)>,
    /// How many times `probe` ran.
    pub probes: u32,
}

/// The scripted store.
#[derive(Debug)]
pub struct ScriptedSqlStore {
    env: Env,
    calls: Mutex<SqlCalls>,
    script: Mutex<VecDeque<SqlScript>>,
    probe: Mutex<Result<Vec<ProbeBreach>, SqlError>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ScriptedSqlStore {
    /// A store on `env`'s clock whose probe is clean.
    pub fn new(env: &Env) -> Arc<Self> {
        Arc::new(Self {
            env: env.clone(),
            calls: Mutex::default(),
            script: Mutex::default(),
            probe: Mutex::new(Ok(Vec::new())),
        })
    }

    /// Queues what the next runs do.
    pub fn script(&self, steps: impl IntoIterator<Item = SqlScript>) {
        lock(&self.script).extend(steps);
    }

    /// Sets what every later probe reports: its breaches, or the error that kept it from running.
    pub fn set_probe(&self, outcome: Result<Vec<ProbeBreach>, SqlError>) {
        *lock(&self.probe) = outcome;
    }

    /// The calls so far.
    pub fn calls(&self) -> SqlCalls {
        lock(&self.calls).clone()
    }

    /// The grid every successful run answers: two rows, one of which a spreadsheet would read as a formula.
    pub fn grid() -> SqlGrid {
        let text = |value: &str| SqlCell::String(value.to_owned());
        SqlGrid {
            columns: vec![
                SqlColumn {
                    name: "name".to_owned(),
                    ty: "String".to_owned(),
                },
                SqlColumn {
                    name: "n".to_owned(),
                    ty: "Int64".to_owned(),
                },
            ],
            rows: vec![
                vec![text("=cmd()"), text("-3")],
                vec![text("plain"), text("4")],
            ],
            truncated: false,
            cut_cells: 0,
            stats: SqlStats {
                rows_read: 10,
                bytes_read: 100,
                elapsed_ms: 3,
            },
        }
    }

    async fn next(&self) -> Result<(), SqlError> {
        let step = lock(&self.script).pop_front();
        match step.unwrap_or(SqlScript::Rows) {
            SqlScript::Rows => Ok(()),
            SqlScript::Fail(error) => Err(error),
            SqlScript::Slow(d) => {
                self.env.clock.sleep(d).await;
                Ok(())
            }
            SqlScript::Hang => std::future::pending().await,
            SqlScript::Disconnect => Err(SqlError::Unavailable(
                "the connection was lost while the answer was read".to_owned(),
            )),
        }
    }
}

#[async_trait]
impl SqlWorkspaceStore for ScriptedSqlStore {
    async fn validate(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
    ) -> Result<SqlEstimate, SqlError> {
        lock(&self.calls)
            .validated
            .push((scope.project_id(), query.clone()));
        Ok(SqlEstimate {
            parts: 1,
            rows: 10,
            marks: 1,
        })
    }

    async fn run(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        _limits: SqlLimits,
    ) -> Result<SqlGrid, SqlError> {
        lock(&self.calls)
            .ran
            .push((scope.project_id(), query.clone()));
        self.next().await?;
        Ok(Self::grid())
    }

    async fn export(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        format: SqlExportFormat,
        _limits: SqlLimits,
    ) -> Result<Bytes, SqlError> {
        lock(&self.calls)
            .ran
            .push((scope.project_id(), query.clone()));
        self.next().await?;
        Ok(Bytes::from(format!("{format:?}")))
    }

    async fn cancel(&self, scope: ProjectScope, query_id: Uuid) -> Result<(), SqlError> {
        lock(&self.calls)
            .cancelled
            .push((scope.project_id(), query_id));
        Ok(())
    }

    async fn provision(
        &self,
        project_id: ProjectId,
        generation: u32,
        retention: &RetentionSettings,
    ) -> Result<(), SqlError> {
        lock(&self.calls)
            .provisions
            .push((project_id, generation, *retention));
        Ok(())
    }

    async fn deprovision(&self, project_id: ProjectId) -> Result<(), SqlError> {
        lock(&self.calls).deprovisions.push(project_id);
        Ok(())
    }

    async fn probe(&self) -> Result<ProbeReport, SqlError> {
        lock(&self.calls).probes += 1;
        lock(&self.probe).clone().map(|breaches| ProbeReport {
            version: "26.3.1.1".to_owned(),
            statements: 29,
            breaches,
        })
    }
}
