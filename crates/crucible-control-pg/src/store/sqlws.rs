//! The SQL workspace's repositories over Postgres (contract 11.1): `ch_sql_identities`, `saved_queries` and
//! `sql_runs`.

use async_trait::async_trait;
use crucible_control::{
    AppliedRetention, SavedParam, SavedQuery, SavedQueryRepo, SqlActor, SqlIdentity,
    SqlIdentityRepo, SqlIdentityState, SqlRun, SqlRunEnd, SqlRunKind, SqlRunOutcome, SqlRunRepo,
    SqlVisibility, TxError,
};
use crucible_records::{ProjectId, SavedQueryId, SqlRunId, Timestamp};

use super::{Handle, every, first};
use crate::sql::{Cols, P, int};

const IDENTITY: &str = "project_id, generation, target_generation, state, retention_days, \
     payload_retention_days, mcp_content_retention_days, last_probe, last_used_at";

fn identity(c: &mut Cols<'_>) -> Result<SqlIdentity, TxError> {
    Ok(SqlIdentity {
        project_id: ProjectId::from_uuid(c.uuid()?),
        generation: c.u32()?,
        target_generation: c.u32()?,
        state: c.parsed(SqlIdentityState::parse)?,
        retention: AppliedRetention {
            retention_days: c.opt_u32()?,
            payload_retention_days: c.opt_u32()?,
            mcp_content_retention_days: c.opt_u32()?,
        },
        last_probe: c.opt_ts()?,
        last_used_at: c.ts()?,
    })
}

#[async_trait]
impl SqlIdentityRepo for Handle {
    async fn get(&self, project_id: ProjectId) -> Result<Option<SqlIdentity>, TxError> {
        let sql = format!("SELECT {IDENTITY} FROM ch_sql_identities WHERE project_id = $1");
        first(
            &self.read(&sql, &[P::Uuid(project_id.uuid())]).await?,
            identity,
        )
    }

    async fn list(&self) -> Result<Vec<SqlIdentity>, TxError> {
        let sql = format!("SELECT {IDENTITY} FROM ch_sql_identities ORDER BY project_id");
        every(&self.read(&sql, &[]).await?, identity)
    }

    async fn insert(&self, identity: &SqlIdentity) -> Result<(), TxError> {
        self.write(
            "INSERT INTO ch_sql_identities (project_id, generation, target_generation, state, retention_days, \
             payload_retention_days, mcp_content_retention_days, last_probe, last_used_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) RETURNING 1",
            &[
                P::Uuid(identity.project_id.uuid()),
                P::Int4(identity.generation),
                P::Int4(identity.target_generation),
                P::Text(identity.state.as_str()),
                P::OptInt4(identity.retention.retention_days),
                P::OptInt4(identity.retention.payload_retention_days),
                P::OptInt4(identity.retention.mcp_content_retention_days),
                P::OptTs(identity.last_probe),
                P::Ts(identity.last_used_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if(
        &self,
        identity: &SqlIdentity,
        expected_state: SqlIdentityState,
        expected_generation: u32,
    ) -> Result<bool, TxError> {
        // `last_used_at` and `target_generation` are owned by `touch` and `rotate_all`: they only move forward.
        let changed = self
            .write(
                "UPDATE ch_sql_identities SET generation = $2, target_generation = greatest(target_generation, \
                 $3), state = $4, retention_days = $5, payload_retention_days = $6, mcp_content_retention_days = \
                 $7, last_probe = $8, last_used_at = greatest(last_used_at, $9) WHERE project_id = $1 AND state = \
                 $10 AND generation = $11 RETURNING 1",
                &[
                    P::Uuid(identity.project_id.uuid()),
                    P::Int4(identity.generation),
                    P::Int4(identity.target_generation),
                    P::Text(identity.state.as_str()),
                    P::OptInt4(identity.retention.retention_days),
                    P::OptInt4(identity.retention.payload_retention_days),
                    P::OptInt4(identity.retention.mcp_content_retention_days),
                    P::OptTs(identity.last_probe),
                    P::Ts(identity.last_used_at),
                    P::Text(expected_state.as_str()),
                    P::Int4(expected_generation),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn touch(&self, project_id: ProjectId, at: Timestamp) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE ch_sql_identities SET last_used_at = greatest(last_used_at, $2) WHERE project_id = $1 \
                 RETURNING 1",
                &[P::Uuid(project_id.uuid()), P::Ts(at)],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn rotate_all(&self) -> Result<u64, TxError> {
        self.write(
            "UPDATE ch_sql_identities SET target_generation = target_generation + 1 RETURNING 1",
            &[],
        )
        .await
    }

    async fn delete_dropping(&self, project_id: ProjectId) -> Result<bool, TxError> {
        let changed = self
            .write(
                "DELETE FROM ch_sql_identities WHERE project_id = $1 AND state = $2 RETURNING 1",
                &[
                    P::Uuid(project_id.uuid()),
                    P::Text(SqlIdentityState::Dropping.as_str()),
                ],
            )
            .await?;
        Ok(changed == 1)
    }
}

const SAVED: &str = "id, project_id, owner_id, owner_key_id, name, description, visibility, sql, \
     parameters::text, version, created_at, updated_at";

fn saved(c: &mut Cols<'_>) -> Result<SavedQuery, TxError> {
    Ok(SavedQuery {
        id: SavedQueryId::from_uuid(c.uuid()?),
        project_id: ProjectId::from_uuid(c.uuid()?),
        owner: SqlActor::from_columns(c.opt_uuid()?, c.opt_uuid()?)?,
        name: c.text()?,
        description: c.text()?,
        visibility: c.parsed(SqlVisibility::parse)?,
        sql: c.text()?,
        parameters: SavedParam::list_from_json(&c.text()?)?,
        version: c.u64()?,
        created_at: c.ts()?,
        updated_at: c.ts()?,
    })
}

#[async_trait]
impl SavedQueryRepo for Handle {
    async fn get(
        &self,
        project_id: ProjectId,
        id: SavedQueryId,
    ) -> Result<Option<SavedQuery>, TxError> {
        let sql = format!("SELECT {SAVED} FROM saved_queries WHERE project_id = $1 AND id = $2");
        first(
            &self
                .read(&sql, &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())])
                .await?,
            saved,
        )
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<SavedQuery>, TxError> {
        let sql = format!("SELECT {SAVED} FROM saved_queries WHERE project_id = $1 ORDER BY id");
        every(
            &self.read(&sql, &[P::Uuid(project_id.uuid())]).await?,
            saved,
        )
    }

    async fn insert(&self, query: &SavedQuery) -> Result<(), TxError> {
        let parameters = SavedParam::list_to_json(&query.parameters)?;
        self.write(
            "INSERT INTO saved_queries (id, project_id, owner_id, owner_key_id, name, description, visibility, \
             sql, parameters, version, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::jsonb, $10, $11, $12) RETURNING 1",
            &[
                P::Uuid(query.id.uuid()),
                P::Uuid(query.project_id.uuid()),
                P::OptUuid(query.owner.user_id().map(|id| id.uuid())),
                P::OptUuid(query.owner.api_key_id().map(|id| id.uuid())),
                P::Text(&query.name),
                P::Text(&query.description),
                P::Text(query.visibility.as_str()),
                P::Text(&query.sql),
                P::Text(&parameters),
                int(query.version)?,
                P::Ts(query.created_at),
                P::Ts(query.updated_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn update_if_version(&self, query: &SavedQuery, expected: u64) -> Result<bool, TxError> {
        let parameters = SavedParam::list_to_json(&query.parameters)?;
        let changed = self
            .write(
                "UPDATE saved_queries SET name = $3, description = $4, visibility = $5, sql = $6, parameters = \
                 $7::jsonb, version = version + 1, updated_at = $8 WHERE project_id = $1 AND id = $2 AND version \
                 = $9 RETURNING 1",
                &[
                    P::Uuid(query.project_id.uuid()),
                    P::Uuid(query.id.uuid()),
                    P::Text(&query.name),
                    P::Text(&query.description),
                    P::Text(query.visibility.as_str()),
                    P::Text(&query.sql),
                    P::Text(&parameters),
                    P::Ts(query.updated_at),
                    int(expected)?,
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete(&self, project_id: ProjectId, id: SavedQueryId) -> Result<bool, TxError> {
        let changed = self
            .write(
                "DELETE FROM saved_queries WHERE project_id = $1 AND id = $2 RETURNING 1",
                &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())],
            )
            .await?;
        Ok(changed == 1)
    }
}

const RUN: &str = "id, project_id, user_id, api_key_id, query_id, kind, saved_query_id, sql, started_at, \
     outcome, error_code, result_rows, truncated, rows_read, bytes_read, elapsed_ms, finished_at";

fn run(c: &mut Cols<'_>) -> Result<SqlRun, TxError> {
    let id = SqlRunId::from_uuid(c.uuid()?);
    let project_id = ProjectId::from_uuid(c.uuid()?);
    let actor = SqlActor::from_columns(c.opt_uuid()?, c.opt_uuid()?)?;
    let query_id = c.uuid()?;
    let kind = c.parsed(SqlRunKind::parse)?;
    let saved_query_id = c.opt_uuid()?.map(SavedQueryId::from_uuid);
    let sql = c.text()?;
    let started_at = c.ts()?;
    let outcome = c.parsed(SqlRunOutcome::parse)?;
    let error_code = c.opt_text()?;
    let result_rows = c.u64()?;
    let truncated = c.bool()?;
    let rows_read = c.u64()?;
    let bytes_read = c.u64()?;
    let elapsed_ms = c.u64()?;
    let finished_at = c.opt_ts()?;
    let end = match (outcome, finished_at) {
        (SqlRunOutcome::Running, _) | (_, None) => None,
        (outcome, Some(finished_at)) => Some(SqlRunEnd {
            outcome,
            error_code,
            result_rows,
            truncated,
            rows_read,
            bytes_read,
            elapsed_ms,
            finished_at,
        }),
    };
    Ok(SqlRun {
        id,
        project_id,
        actor,
        query_id,
        kind,
        saved_query_id,
        sql,
        started_at,
        end,
    })
}

#[async_trait]
impl SqlRunRepo for Handle {
    async fn get(&self, project_id: ProjectId, id: SqlRunId) -> Result<Option<SqlRun>, TxError> {
        let sql = format!("SELECT {RUN} FROM sql_runs WHERE project_id = $1 AND id = $2");
        first(
            &self
                .read(&sql, &[P::Uuid(project_id.uuid()), P::Uuid(id.uuid())])
                .await?,
            run,
        )
    }

    async fn list(
        &self,
        project_id: ProjectId,
        actor: Option<SqlActor>,
        limit: u32,
    ) -> Result<Vec<SqlRun>, TxError> {
        // Without an actor both conditions are true for every row.
        let sql = format!(
            "SELECT {RUN} FROM sql_runs WHERE project_id = $1 AND ($2::uuid IS NULL OR user_id = $2) AND \
             ($3::uuid IS NULL OR api_key_id = $3) ORDER BY started_at DESC, id DESC LIMIT $4"
        );
        let user = actor.and_then(|actor| actor.user_id()).map(|id| id.uuid());
        let key = actor
            .and_then(|actor| actor.api_key_id())
            .map(|id| id.uuid());
        every(
            &self
                .read(
                    &sql,
                    &[
                        P::Uuid(project_id.uuid()),
                        P::OptUuid(user),
                        P::OptUuid(key),
                        P::Int(i64::from(limit)),
                    ],
                )
                .await?,
            run,
        )
    }

    async fn insert(&self, run: &SqlRun) -> Result<(), TxError> {
        self.write(
            "INSERT INTO sql_runs (id, project_id, user_id, api_key_id, query_id, kind, saved_query_id, sql, \
             started_at, outcome, error_code, result_rows, truncated, rows_read, bytes_read, elapsed_ms, \
             finished_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, NULL, 0, false, 0, 0, 0, NULL) \
             RETURNING 1",
            &[
                P::Uuid(run.id.uuid()),
                P::Uuid(run.project_id.uuid()),
                P::OptUuid(run.actor.user_id().map(|id| id.uuid())),
                P::OptUuid(run.actor.api_key_id().map(|id| id.uuid())),
                P::Uuid(run.query_id),
                P::Text(run.kind.as_str()),
                P::OptUuid(run.saved_query_id.map(|id| id.uuid())),
                P::Text(&run.sql),
                P::Ts(run.started_at),
                P::Text(SqlRunOutcome::Running.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn finish(&self, id: SqlRunId, end: &SqlRunEnd) -> Result<bool, TxError> {
        let changed = self
            .write(
                "UPDATE sql_runs SET outcome = $2, error_code = $3, result_rows = $4, truncated = $5, rows_read = \
                 $6, bytes_read = $7, elapsed_ms = $8, finished_at = $9 WHERE id = $1 AND outcome = $10 \
                 RETURNING 1",
                &[
                    P::Uuid(id.uuid()),
                    P::Text(end.outcome.as_str()),
                    P::OptText(end.error_code.as_deref()),
                    int(end.result_rows)?,
                    P::Bool(end.truncated),
                    int(end.rows_read)?,
                    int(end.bytes_read)?,
                    int(end.elapsed_ms)?,
                    P::Ts(end.finished_at),
                    P::Text(SqlRunOutcome::Running.as_str()),
                ],
            )
            .await?;
        Ok(changed == 1)
    }

    async fn delete_before(&self, before: Timestamp) -> Result<u64, TxError> {
        self.write(
            "DELETE FROM sql_runs WHERE started_at < $1 RETURNING 1",
            &[P::Ts(before)],
        )
        .await
    }
}
