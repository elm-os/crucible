//! The control-store differential against one real Postgres (contract 2.7): every script of [`control`] runs
//! against `PgControlStore` and against the simulation's `PgModel`, and the transcripts must be equal. Rows
//! that only a real server can answer (the migrations, the timeouts, lost connections) follow.
//!
//! [`control`]: crate::control

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use crucible_control::{ControlStore, JsonDoc, MigrationKind, Project, Repos, SealedProjectSalt};
use crucible_control_pg::{PgConfig, PgControlStore, migrate};
use crucible_env::Env;
use crucible_records::{ProjectId, Uuid};
use crucible_sim::pg_model::{PgFaults, PgModel};

use crate::Outcome;
use crate::control::{ACQUIRE_TIMEOUT, Ctl, MAIN_POOL, SCRIPTS, first_difference, run_script};

fn config(url: &str) -> PgConfig {
    PgConfig {
        main_pool: MAIN_POOL,
        acquire_timeout: ACQUIRE_TIMEOUT,
        ..PgConfig::new(url)
    }
}

fn model(env: &Env) -> PgModel {
    let faults = PgFaults {
        main_pool: MAIN_POOL,
        acquire_timeout: ACQUIRE_TIMEOUT,
        ..PgFaults::default()
    };
    PgModel::new(Arc::clone(&env.clock), env.rng.as_ref(), faults)
}

async fn migrations(env: &Env, config: &PgConfig, store: &PgControlStore) -> Result<String> {
    let first = migrate(env, config).await?;
    let second = migrate(env, config).await?;
    ensure!(
        second.is_empty(),
        "a second run applied {} migrations",
        second.len()
    );
    let ledger = store.migrations().applied().await?;
    let postgres: Vec<_> = ledger
        .iter()
        .filter(|entry| entry.kind == MigrationKind::Postgres)
        .map(|entry| entry.id.as_str())
        .collect();
    ensure!(
        postgres.contains(&"V1__control_base"),
        "the ledger does not record V1__control_base: {postgres:?}"
    );
    Ok(format!(
        "{} applied by this run, ledger {postgres:?}, second run a no-op",
        first.len()
    ))
}

async fn timeouts(store: &PgControlStore) -> Result<String> {
    for setting in ["statement_timeout", "idle_in_transaction_session_timeout"] {
        let value = store.current_setting(setting, false).await?;
        ensure!(
            value == "10s",
            "{setting} is {value} on a pooled connection"
        );
        let value = store.current_setting(setting, true).await?;
        ensure!(value == "10s", "{setting} is {value} inside a transaction");
    }
    let level = store.current_setting("transaction_isolation", true).await?;
    ensure!(
        level == "repeatable read",
        "transactions run at {level}, not repeatable read"
    );
    Ok(
        "both timeouts are 10s on pooled connections and in transactions; repeatable read"
            .to_owned(),
    )
}

/// A restart or a failover closes every connection; the pools must replace them without a stale one.
async fn reconnect(env: &Env, store: &PgControlStore) -> Result<String> {
    store.db_now().await?;
    store.users().list().await?;
    let closed = store.terminate_other_backends().await?;
    ensure!(closed >= 1, "no pooled connection was open to terminate");
    env.clock.sleep(Duration::from_millis(300)).await;
    for _ in 0..4 {
        store
            .db_now()
            .await
            .context("the reserved pool after the loss")?;
        store
            .users()
            .list()
            .await
            .context("the main pool after the loss")?;
    }
    let tx = store
        .begin()
        .await
        .context("a transaction after the loss")?;
    tx.commit().await?;
    Ok(format!(
        "{closed} connections terminated; every later call succeeded"
    ))
}

/// Deleting a project's row deletes its salt (contract 13.4, `ON DELETE CASCADE`). No repository deletes a
/// project's row yet, so the model has nothing to compare here.
async fn salt_goes_with_its_project(store: &PgControlStore) -> Result<String> {
    store.reset().await?;
    let id = ProjectId::from_uuid(Uuid::from_v7_parts(1_800_000_000_900, 900));
    let project = Project {
        id,
        name: "Doomed".to_owned(),
        slug: "doomed".to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    };
    store.projects().insert(&project).await?;
    let salt = SealedProjectSalt {
        project_id: id,
        key_id: "master-0".to_owned(),
        nonce: vec![1; 24],
        ciphertext: vec![2; 48],
    };
    ensure!(
        store.project_salts().insert_if_absent(&salt).await?,
        "the salt was not stored"
    );
    ensure!(
        store.delete_project_row(id).await?,
        "the project's row was not deleted"
    );
    ensure!(
        store.project_salts().get(id).await?.is_none(),
        "the salt outlived its project's row"
    );
    store.reset().await?;
    Ok("deleting a project's row deleted its salt".to_owned())
}

/// Runs the differential and the server-only rows against the Postgres at `url`.
pub async fn run(env: &Env, url: &str) -> Vec<Outcome> {
    let config = config(url);
    let mut outcomes = Vec::new();
    let mut push = |row: &'static str, result: Result<String>| {
        outcomes.push(Outcome {
            row,
            result: result.map_err(|error| format!("{error:#}")),
        });
    };
    let store = match PgControlStore::new(env, &config) {
        Ok(store) => store,
        Err(error) => {
            push("PG0 connect", Err(error.into()));
            return outcomes;
        }
    };
    let migrated = migrations(env, &config, &store).await;
    let ready = migrated.is_ok();
    push(
        "PG1 migrations apply once and are recorded in the ledger",
        migrated,
    );
    if !ready {
        return outcomes;
    }
    push("PG2 timeouts and isolation level", timeouts(&store).await);
    for (name, script) in SCRIPTS {
        let result = async {
            store.reset().await?;
            let real = Ctl {
                store: Arc::new(store.clone()),
                clock: Arc::clone(&env.clock),
            };
            let real = run_script(&real, *script)
                .await
                .context("against postgres")?;
            let model = Ctl {
                store: Arc::new(model(env)),
                clock: Arc::clone(&env.clock),
            };
            let model = run_script(&model, *script)
                .await
                .context("against the model")?;
            match first_difference(&model, &real) {
                Some(difference) => anyhow::bail!(difference),
                None => Ok(format!("{} observations equal", real.lines().len())),
            }
        }
        .await;
        push(row_name(name), result);
    }
    push(
        "PG3 lost connections are replaced",
        reconnect(env, &store).await,
    );
    push(
        "PG4 a project's salt goes with its row",
        salt_goes_with_its_project(&store).await,
    );
    store.close();
    outcomes
}

/// The row label of a differential script.
fn row_name(script: &str) -> &'static str {
    match script {
        "repositories" => "PgModel differential: repositories",
        "transactions" => "PgModel differential: transactions",
        "key_feed" => "PgModel differential: key feed",
        "leases" => "PgModel differential: leases",
        "claims" => "PgModel differential: claims",
        "identity" => "PgModel differential: identity",
        "pools" => "PgModel differential: pools",
        "jobs" => "PgModel differential: jobs (park, key-feed prune)",
        "exports" => "PgModel differential: export jobs",
        "alerts" => "PgModel differential: alerts (channels, rules, state, history, queue prune)",
        "lifecycle" => {
            "PgModel differential: lifecycle (nodes, deletion requests, actions, floors)"
        }
        "dashboards" => "PgModel differential: dashboards (widget sets, versions, cascade)",
        _ => "PgModel differential",
    }
}
