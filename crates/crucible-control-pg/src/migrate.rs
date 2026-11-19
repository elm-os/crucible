//! The Postgres migrations (contract 6.6): forward-only SQL files applied by refinery on one direct
//! connection, then recorded in the `schema_migrations` ledger.
//!
//! refinery keeps its own history table (`refinery_schema_history`) with the file checksums, and refuses to
//! run when an applied file has changed or is missing. The ledger row of each migration is written after it,
//! with the time from the `Env` clock.

use crucible_control::{LedgerEntry, MigrationKind, TxError};
use crucible_env::Env;
use refinery::{Migration, Runner};
use tokio_postgres::types::Type;

use crate::conn::{Connector, PgConfig};
use crate::sql::P;

/// The migration files, in order. A change to the schema is a new entry; an applied file never changes.
const FILES: &[(&str, &str)] = &[
    (
        "V1__control_base",
        include_str!("../migrations/V1__control_base.sql"),
    ),
    (
        "V2__identity",
        include_str!("../migrations/V2__identity.sql"),
    ),
    (
        "V3__sql_workspace",
        include_str!("../migrations/V3__sql_workspace.sql"),
    ),
    ("V4__alerts", include_str!("../migrations/V4__alerts.sql")),
];

fn failed(what: &str, error: impl std::fmt::Display) -> TxError {
    TxError::Invalid(format!("postgres migrations: {what}: {error}"))
}

/// Applies every pending Postgres migration in one transaction and records each in the ledger. Returns the
/// ledger entries written by this call; an up-to-date database returns none. Safe to run from several nodes
/// at once: the loser of a race sees the winner's work and applies nothing.
pub async fn migrate(env: &Env, config: &PgConfig) -> Result<Vec<LedgerEntry>, TxError> {
    let migrations = FILES
        .iter()
        .map(|(name, sql)| Migration::unapplied(name, sql))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("a migration file name", error))?;
    let runner = Runner::new(&migrations)
        .set_grouped(true)
        .set_abort_divergent(true)
        .set_abort_missing(true);
    let mut client = Connector::new(env, config)?.connect().await?;
    if let Err(first) = runner.run_async(&mut client).await {
        // Two nodes that migrate at once race on the first statement; the loser's transaction rolled back.
        // A second run then finds the work done, or reports the real error.
        tracing::debug!(error = %first, "postgres migrations: first attempt failed, running again");
        runner
            .run_async(&mut client)
            .await
            .map_err(|error| failed("applying", error))?;
    }
    let mut recorded = Vec::new();
    for migration in runner.get_migrations() {
        let entry = LedgerEntry {
            id: format!("V{}__{}", migration.version(), migration.name()),
            kind: MigrationKind::Postgres,
            applied_at: env.clock.wall_now(),
            checksum: migration.checksum().to_string(),
        };
        let params = [
            P::Text(&entry.id),
            P::Text(entry.kind.as_str()),
            P::Ts(entry.applied_at),
            P::Text(&entry.checksum),
        ];
        let typed: Vec<(&(dyn tokio_postgres::types::ToSql + Sync), Type)> = params
            .iter()
            .map(|param| (param as _, param.ty()))
            .collect();
        let rows = client
            .query_typed(
                "INSERT INTO schema_migrations (id, kind, applied_at, checksum) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO NOTHING RETURNING 1",
                &typed,
            )
            .await
            .map_err(|error| failed("recording in the ledger", error))?;
        if !rows.is_empty() {
            recorded.push(entry);
        }
    }
    Ok(recorded)
}
