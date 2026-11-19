//! Conformance of the production ClickHouse adapter against real servers (contract sections 5 to 8;
//! `docs/testing/invariants.md`, "Conformance rows that the contract names").
//!
//! [`run`] bootstraps a server through `TelemetryStore::apply_migration` and then runs every row through
//! `crucible_store_clickhouse::ClickHouseStore`, the adapter product code uses. Scratch tables, fault setups and
//! reads of system tables go through the adapter's `test-support` statements, as `crucible_admin`.
//!
//! The suite is also the differential check of the simulation's store model (`crucible_sim::ch_model::ChModel`):
//! the typed-plan reads insert their blocks into the model too and compare every answer ([`differential`]).
//!
//! The binary of this crate runs the suite against the ClickHouse URL it is given; `scripts/conformance.sh`
//! starts ClickHouse 26.3, 26.8 and latest in Docker and runs it against each.

pub mod blocks;
mod cancel;
mod codes;
pub mod control;
pub mod control_pg;
mod differential;
mod inserts;
mod keep_alive;
mod m2;
mod maintenance;
mod reads;
mod startup;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use crucible_env::{Env, InsertError, InsertSettings, StoreFlavour, TelemetryStore, TlsRoots};
use crucible_plan::profile_caps;
use crucible_records::{BatchToken, Lsn, PartitionId, ProjectId, TableId, Timestamp, WalEpoch};
use crucible_schema::{
    AccessParams, Granularity, Ident, PasswordHash, ServiceIdentity, Variant, access_service,
    route, schema_base, schema_span_usage_aggregate,
};
use crucible_sim::ch_model::ChModel;
use crucible_store_clickhouse::render::Statement;
use crucible_store_clickhouse::{ClickHouseStore, Credentials, StoreConfig};
use sha2::{Digest, Sha256};

use crate::blocks::Rows;

/// The server under test.
#[derive(Clone, Debug)]
pub struct Target {
    /// The HTTP endpoint.
    pub url: String,
    /// The operator-supplied admin user, which must hold access management.
    pub admin: Credentials,
    /// The cluster name for a one-shard replicated target; `None` for a single node.
    pub cluster: Option<String>,
}

/// The result of one conformance row.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The row, as `docs/testing/invariants.md` names it.
    pub row: &'static str,
    /// What was observed, or why the row failed.
    pub result: Result<String, String>,
}

/// The instant the fixtures are placed at: 2026-11-17T09:30:00Z.
pub(crate) const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

/// What every row works with.
pub(crate) struct Ctx {
    pub env: Env,
    pub store: ClickHouseStore,
    pub config: StoreConfig,
    pub flavour: StoreFlavour,
    pub variant: Variant,
    /// The simulation's store model, which the differential rows feed the same blocks and plans.
    pub model: ChModel,
    /// Plans whose answers were compared between the server and the model.
    pub compared: AtomicU64,
}

pub(crate) fn statement(sql: impl Into<String>) -> Statement {
    Statement {
        sql: sql.into(),
        params: Vec::new(),
        settings: Vec::new(),
    }
}

pub(crate) fn with_settings(
    sql: impl Into<String>,
    settings: &[(&'static str, &str)],
) -> Statement {
    Statement {
        settings: settings
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect(),
        ..statement(sql)
    }
}

fn password(identity: ServiceIdentity) -> String {
    format!("conformance-{}", identity.name())
}

fn password_hash(identity: ServiceIdentity) -> Result<PasswordHash> {
    let digest = Sha256::digest(password(identity).as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(PasswordHash::new(&hex)?)
}

impl Ctx {
    fn new(env: &Env, target: &Target) -> Result<Self> {
        let generated = |identity: ServiceIdentity| Credentials {
            user: identity.name().to_owned(),
            password: password(identity),
        };
        let (flavour, variant) = match &target.cluster {
            Some(cluster) => (
                StoreFlavour::Clustered,
                Variant::Clustered {
                    cluster: Ident::new(cluster)?,
                },
            ),
            None => (StoreFlavour::SingleNode, Variant::SingleNode),
        };
        let config = StoreConfig {
            url: target.url.clone(),
            read_url: None,
            admin: target.admin.clone(),
            ingest: generated(ServiceIdentity::Ingest),
            read_ui: generated(ServiceIdentity::ReadUi),
            read_agent: generated(ServiceIdentity::ReadAgent),
            maint: generated(ServiceIdentity::Maint),
            flavour: Some(flavour),
            tls_roots: TlsRoots::WebPki,
            connect_timeout: Duration::from_secs(10),
            idle_timeout: None,
        };
        Ok(Self {
            env: env.clone(),
            store: ClickHouseStore::new(env.clone(), config.clone()),
            config,
            flavour,
            variant,
            model: ChModel::new(
                env.rng.as_ref(),
                flavour,
                Granularity::Day,
                Duration::from_secs(60),
            ),
            compared: AtomicU64::new(0),
        })
    }

    pub(crate) fn access_params(&self) -> Result<AccessParams> {
        let caps = |identity| profile_caps(identity).context("a read identity has class limits");
        Ok(AccessParams {
            admin_user: Ident::new(&self.config.admin.user)?,
            ingest: password_hash(ServiceIdentity::Ingest)?,
            read_ui: password_hash(ServiceIdentity::ReadUi)?,
            read_agent: password_hash(ServiceIdentity::ReadAgent)?,
            maint: password_hash(ServiceIdentity::Maint)?,
            read_ui_caps: caps(ServiceIdentity::ReadUi)?,
            read_agent_caps: caps(ServiceIdentity::ReadAgent)?,
        })
    }

    /// Applies the base schema, access step A1 and the later schema steps, each twice: the steps are idempotent.
    pub(crate) async fn migrate(&self) -> Result<()> {
        let schema = schema_base(&self.variant, Granularity::Day);
        let access = access_service(&self.variant, &self.access_params()?);
        let column = schema_span_usage_aggregate(&self.variant);
        for step in [&schema, &schema, &access, &access, &column, &column] {
            self.store
                .apply_migration(step)
                .await
                .with_context(|| format!("migration step {}", step.id))?;
        }
        Ok(())
    }

    /// Runs a statement as `crucible_admin`.
    pub(crate) async fn admin(&self, sql: impl Into<String>) -> Result<()> {
        let sql = sql.into();
        self.store
            .raw_command(ServiceIdentity::Admin, &statement(sql.clone()))
            .await
            .with_context(|| sql)
    }

    /// The tab-separated answer of a read as `crucible_admin`.
    pub(crate) async fn admin_text(&self, sql: impl Into<String>) -> Result<String> {
        let sql = sql.into();
        self.store
            .raw_text(ServiceIdentity::Admin, &statement(sql.clone()))
            .await
            .with_context(|| sql)
    }

    /// A single number read as `crucible_admin`.
    pub(crate) async fn admin_count(&self, sql: impl Into<String>) -> Result<u64> {
        let answer = self.admin_text(sql).await?;
        answer
            .trim()
            .parse()
            .map_err(|_| anyhow!("not a number: {answer:?}"))
    }

    /// A fresh project id.
    pub(crate) fn project(&self) -> ProjectId {
        ProjectId::from_uuid(self.env.new_id())
    }

    /// A fresh WAL epoch.
    pub(crate) fn epoch(&self) -> WalEpoch {
        WalEpoch::from_uuid(self.env.new_id())
    }

    /// The token of batch `seq` of `epoch` for the partition of `table` that holds `time`.
    pub(crate) fn token(
        &self,
        epoch: WalEpoch,
        seq: u64,
        table: TableId,
        time: Timestamp,
    ) -> Result<BatchToken> {
        let partition = route(table, Granularity::Day, time)?;
        self.token_in(epoch, seq, table, partition)
    }

    pub(crate) fn token_in(
        &self,
        epoch: WalEpoch,
        seq: u64,
        table: TableId,
        partition: PartitionId,
    ) -> Result<BatchToken> {
        Ok(BatchToken::new(
            epoch,
            Lsn::new(seq),
            Lsn::new(seq),
            table,
            partition,
        )?)
    }

    /// Inserts a block through the production adapter with the settings of contract 5.4.
    pub(crate) async fn insert(&self, rows: &Rows, token: BatchToken) -> Result<(), InsertError> {
        let block = rows.encode().map_err(|error| InsertError::Poison {
            code: 0,
            message: format!("the fixture block could not be built: {error:#}"),
        })?;
        self.store
            .insert_block(block, InsertSettings::new(token, self.flavour))
            .await
    }

    /// Stops background merges of a table, so part and row counts show what each insert did.
    pub(crate) async fn stop_merges(&self, table: &str) -> Result<()> {
        self.admin(format!("SYSTEM STOP MERGES crucible.{table}"))
            .await
    }
}

pub(crate) fn expect(condition: bool, what: impl FnOnce() -> String) -> Result<()> {
    if condition { Ok(()) } else { bail!(what()) }
}

/// Runs the suite against `target` and returns one outcome per row. The first outcome is the bootstrap; when it
/// fails no other row runs.
pub async fn run(env: &Env, target: &Target) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let ctx = match Ctx::new(env, target) {
        Ok(ctx) => Arc::new(ctx),
        Err(error) => {
            return vec![Outcome {
                row: "bootstrap",
                result: Err(format!("{error:#}")),
            }];
        }
    };
    macro_rules! row {
        ($name:literal, $future:expr) => {{
            let result: Result<String> = $future.await;
            outcomes.push(Outcome {
                row: $name,
                result: result.map_err(|error| format!("{error:#}")),
            });
        }};
    }
    row!(
        "Startup detection and self-test (5.8)",
        startup::startup(&ctx)
    );
    if outcomes.iter().any(|o| o.result.is_err()) {
        return outcomes;
    }
    let single = target.cluster.is_none();
    row!("C1 token dedup", inserts::c1(&ctx));
    if single {
        row!("C2 plain-table window", inserts::c2(&ctx));
        row!("C3 bounded logs", inserts::c3(&ctx));
    }
    row!("C4 token overrides content hash", inserts::c4(&ctx));
    row!(
        "Single Native block not re-split",
        inserts::single_block(&ctx)
    );
    row!(
        "Two-partition block rejected",
        inserts::two_partitions(&ctx)
    );
    if single {
        row!("C12 stable error codes", codes::c12(&ctx));
        row!("C13 cancel-on-close and kill", cancel::c13(&ctx));
    }
    row!(
        "Typed plans on the real server (8.1, 8.3)",
        reads::reads(&ctx)
    );
    row!("Maintenance calls (2.6)", maintenance::maintenance(&ctx));
    row!("ChModel differential: typed plans (2.6, 8.1)", async {
        let extra = differential::extra(&ctx).await?;
        let violations = ctx.model.violations();
        expect(violations.is_empty(), || {
            format!("the model recorded {violations:?}")
        })?;
        Ok(format!(
            "{} plans returned the same rows on the server and on the store model (order-insensitive where the \
             plan leaves the order undefined), {extra} of them reads of null handling, the UUID order, text \
             tokens, ties under a limit, window edges and the semi-join cap, run before and after the model \
             merged its parts",
            ctx.compared.load(Ordering::Relaxed)
        ))
    });
    row!(
        "ChModel differential: maintenance (2.6)",
        differential::maintenance(&ctx)
    );
    if single {
        row!(
            "Service identities see all rows (7.3)",
            startup::visibility(&ctx)
        );
    }
    row!(
        "Idle connections: reads 3 s apart share one connection",
        keep_alive::reuse(&ctx)
    );
    row!(
        "Idle connections: a pool timeout above the server's fails no read",
        keep_alive::above_server(&ctx)
    );
    row!(
        "A read that could not be sent is sent once more",
        keep_alive::resend(&ctx)
    );
    outcomes
}
