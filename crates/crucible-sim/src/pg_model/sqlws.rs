//! The SQL workspace's repositories in the model: `ch_sql_identities`, `saved_queries` and `sql_runs`, with the
//! semantics of the Postgres statements in `crucible-control-pg`.

use async_trait::async_trait;
use crucible_control::{
    SavedQuery, SavedQueryRepo, SqlIdentity, SqlIdentityRepo, SqlIdentityState, SqlRun, SqlRunEnd,
    SqlRunRepo, TxError,
};
use crucible_records::{ProjectId, SavedQueryId, SqlRunId, Timestamp};

use super::{Pool, Row, Runner, Tbl, uuid_key};

#[async_trait]
impl SqlIdentityRepo for Runner {
    async fn get(&self, project_id: ProjectId) -> Result<Option<SqlIdentity>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::SqlIdentities,
                &uuid_key(project_id.to_bytes()),
                Row::SqlIdentity
            ))
        })
        .await
    }

    async fn list(&self) -> Result<Vec<SqlIdentity>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(all!(cx, Tbl::SqlIdentities, Row::SqlIdentity))
        })
        .await
    }

    async fn insert(&self, identity: &SqlIdentity) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::SqlIdentities,
                uuid_key(identity.project_id.to_bytes()),
                Row::SqlIdentity(identity.clone()),
                "ch_sql_identities_pkey",
            )
        })
        .await
    }

    async fn update_if(
        &self,
        identity: &SqlIdentity,
        expected_state: SqlIdentityState,
        expected_generation: u32,
    ) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(identity.project_id.to_bytes());
            match one!(cx, Tbl::SqlIdentities, &key, Row::SqlIdentity) {
                Some(stored)
                    if stored.state == expected_state
                        && stored.generation == expected_generation =>
                {
                    let mut next = identity.clone();
                    next.target_generation = next.target_generation.max(stored.target_generation);
                    next.last_used_at = next.last_used_at.max(stored.last_used_at);
                    cx.put(Tbl::SqlIdentities, key, Some(Row::SqlIdentity(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn touch(&self, project_id: ProjectId, at: Timestamp) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(project_id.to_bytes());
            match one!(cx, Tbl::SqlIdentities, &key, Row::SqlIdentity) {
                Some(mut stored) => {
                    stored.last_used_at = stored.last_used_at.max(at);
                    cx.put(Tbl::SqlIdentities, key, Some(Row::SqlIdentity(stored)))?;
                    Ok(true)
                }
                None => Ok(false),
            }
        })
        .await
    }

    async fn rotate_all(&self) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut changed = 0;
            for mut stored in all!(cx, Tbl::SqlIdentities, Row::SqlIdentity) {
                stored.target_generation += 1;
                cx.put(
                    Tbl::SqlIdentities,
                    uuid_key(stored.project_id.to_bytes()),
                    Some(Row::SqlIdentity(stored)),
                )?;
                changed += 1;
            }
            Ok(changed)
        })
        .await
    }

    async fn delete_dropping(&self, project_id: ProjectId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(project_id.to_bytes());
            match one!(cx, Tbl::SqlIdentities, &key, Row::SqlIdentity) {
                Some(stored) if stored.state == SqlIdentityState::Dropping => {
                    cx.put(Tbl::SqlIdentities, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}

#[async_trait]
impl SavedQueryRepo for Runner {
    async fn get(
        &self,
        project_id: ProjectId,
        id: SavedQueryId,
    ) -> Result<Option<SavedQuery>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(one!(
                cx,
                Tbl::SavedQueries,
                &uuid_key(id.to_bytes()),
                Row::SavedQuery
            )
            .filter(|query| query.project_id == project_id))
        })
        .await
    }

    async fn list(&self, project_id: ProjectId) -> Result<Vec<SavedQuery>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut queries = all!(cx, Tbl::SavedQueries, Row::SavedQuery);
            queries.retain(|query| query.project_id == project_id);
            Ok(queries)
        })
        .await
    }

    async fn insert(&self, query: &SavedQuery) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            cx.insert(
                Tbl::SavedQueries,
                uuid_key(query.id.to_bytes()),
                Row::SavedQuery(query.clone()),
                "saved_queries_pkey",
            )
        })
        .await
    }

    async fn update_if_version(&self, query: &SavedQuery, expected: u64) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(query.id.to_bytes());
            match one!(cx, Tbl::SavedQueries, &key, Row::SavedQuery) {
                Some(stored)
                    if stored.version == expected && stored.project_id == query.project_id =>
                {
                    // The statement leaves the owner and the creation time alone.
                    let mut next = query.clone();
                    next.version = expected + 1;
                    next.owner = stored.owner;
                    next.created_at = stored.created_at;
                    cx.put(Tbl::SavedQueries, key, Some(Row::SavedQuery(next)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn delete(&self, project_id: ProjectId, id: SavedQueryId) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::SavedQueries, &key, Row::SavedQuery) {
                Some(stored) if stored.project_id == project_id => {
                    cx.put(Tbl::SavedQueries, key, None)?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }
}

#[async_trait]
impl SqlRunRepo for Runner {
    async fn get(&self, project_id: ProjectId, id: SqlRunId) -> Result<Option<SqlRun>, TxError> {
        self.run(Pool::Main, false, |cx| {
            Ok(
                one!(cx, Tbl::SqlRuns, &uuid_key(id.to_bytes()), Row::SqlRun)
                    .filter(|run| run.project_id == project_id),
            )
        })
        .await
    }

    async fn list(&self, project_id: ProjectId, limit: u32) -> Result<Vec<SqlRun>, TxError> {
        self.run(Pool::Main, false, |cx| {
            let mut runs = all!(cx, Tbl::SqlRuns, Row::SqlRun);
            runs.retain(|run| run.project_id == project_id);
            runs.sort_by_key(|run| std::cmp::Reverse((run.started_at, run.id)));
            runs.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            Ok(runs)
        })
        .await
    }

    async fn insert(&self, run: &SqlRun) -> Result<(), TxError> {
        self.run(Pool::Main, true, |cx| {
            // The statement always writes a running row.
            let mut row = run.clone();
            row.end = None;
            cx.insert(
                Tbl::SqlRuns,
                uuid_key(run.id.to_bytes()),
                Row::SqlRun(row),
                "sql_runs_pkey",
            )
        })
        .await
    }

    async fn finish(&self, id: SqlRunId, end: &SqlRunEnd) -> Result<bool, TxError> {
        self.run(Pool::Main, true, |cx| {
            let key = uuid_key(id.to_bytes());
            match one!(cx, Tbl::SqlRuns, &key, Row::SqlRun) {
                Some(mut stored) if stored.end.is_none() => {
                    stored.end = Some(end.clone());
                    cx.put(Tbl::SqlRuns, key, Some(Row::SqlRun(stored)))?;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
        .await
    }

    async fn delete_before(&self, before: Timestamp) -> Result<u64, TxError> {
        self.run(Pool::Main, true, |cx| {
            let mut deleted = 0;
            for run in all!(cx, Tbl::SqlRuns, Row::SqlRun) {
                if run.started_at < before {
                    cx.put(Tbl::SqlRuns, uuid_key(run.id.to_bytes()), None)?;
                    deleted += 1;
                }
            }
            Ok(deleted)
        })
        .await
    }
}
