//! `sqliso`: the isolation suite of the SQL workspace (contract 7.5). Real ClickHouse only.
//!
//! The workspace's isolation is carried by ClickHouse access control alone, so the suite attacks a real
//! server as a project's own ClickHouse identity:
//!
//! - **Layer A** ([`layer_a`]): direct attacks W1 to W15 as project A's identity, with no workspace code in the
//!   path.
//! - **Layer B** ([`fuzz`]): a seeded grammar generator and mutations of an escape-attempt corpus, judged by
//!   canary oracles.
//! - **Layer D** ([`mutants`]): deliberately broken deployments, each of which must make the suite fail.
//!
//! Layer C, the attacks through the API, needs the whole server and lives in the binary crate's tests
//! (`crates/crucible/tests/sqliso_api.rs`).
//!
//! Fixtures: projects A and B with one row in every SQL-readable table. Every string column of B's rows holds
//! B's canary, a random token; an admin-only table and the text of an admin query hold a second canary. The
//! oracle of every statement sent as A is the same: neither canary may appear in the answer or in the error
//! text.

mod fuzz;
mod layer_a;
mod mutants;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use crucible_auth::test_support::scope;
use crucible_env::{Env, SqlError, SqlWorkspaceStore as _, TelemetryStore as _};
use crucible_records::ProjectId;
use crucible_schema::{
    CURATED_VIEWS, Granularity, Ident, MigrationStep, PROBE_PROJECTS, PolicyForm,
    RetentionSettings, SQL_PROFILE, SQL_QUOTA, SQL_QUOTA_ERRORS_PER_HOUR,
    SQL_QUOTA_EXECUTION_SECONDS_PER_HOUR, SQL_QUOTA_QUERIES_PER_HOUR, SQL_QUOTA_READ_ROWS_PER_HOUR,
    SQL_READER_ROLE, SqlProfile, TableDef, Ty, access_service, access_workspace, expected_policies,
    policy_inventory, schema_base, sql_tables, without_secrets, workspace_user,
};
use crucible_store_clickhouse::{ClickHouseSqlStore, RawReply, SqlPasswords, SqlStoreConfig};

use crate::{Ctx, Outcome, Target, expect};

/// How the server treats a user that has no row policy on a table that has some
/// (`users_without_row_policies_can_read_rows`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Switch {
    /// Such a user reads no row.
    Strict,
    /// Such a user reads every row. This is the server's shipped default.
    Permissive,
}

/// What to run.
#[derive(Clone, Debug)]
pub struct Options {
    /// The value of the server switch the deployment is declared to run with. The suite checks that the server
    /// behaves that way.
    pub switch: Switch,
    /// Layer A.
    pub layer_a: bool,
    /// Layer B with this many statements; 0 skips it.
    pub fuzz: u32,
    /// The seed of layer B.
    pub seed: u64,
    /// Layer D.
    pub layer_d: bool,
    /// How many extra identities W14 provisions.
    pub identities: u32,
}

/// The suite's passwords: a fixed derivation, so the suite can sign in as any workspace identity.
#[derive(Debug)]
struct Passwords;

impl SqlPasswords for Passwords {
    fn password(&self, user: &str, generation: u32) -> String {
        format!("sqliso-{user}-{generation}")
    }
}

/// The passwords a node with another master key derives.
#[derive(Debug)]
struct OtherKey;

impl SqlPasswords for OtherKey {
    fn password(&self, user: &str, generation: u32) -> String {
        format!("other-key-{user}-{generation}")
    }
}

/// A statement's answer as the attacker sees it.
#[derive(Clone, Debug)]
pub(crate) enum Reply {
    /// The tab-separated rows.
    Rows(String),
    /// The server's refusal.
    Refused { code: u32, message: String },
}

impl Reply {
    fn text(&self) -> &str {
        match self {
            Self::Rows(text) => text,
            Self::Refused { message, .. } => message,
        }
    }

    pub(crate) fn rows(&self) -> Option<&str> {
        match self {
            Self::Rows(text) => Some(text.trim_end()),
            Self::Refused { .. } => None,
        }
    }

    pub(crate) fn code(&self) -> Option<u32> {
        match self {
            Self::Rows(_) => None,
            Self::Refused { code, .. } => Some(*code),
        }
    }

    pub(crate) fn brief(&self) -> String {
        match self {
            Self::Rows(text) => {
                format!("answered {:?}", text.chars().take(120).collect::<String>())
            }
            Self::Refused { code, message } => format!(
                "was refused with code {code}: {}",
                message.chars().take(160).collect::<String>()
            ),
        }
    }
}

/// The fixtures and the two ways to reach the server: as the admin and as a workspace identity.
pub(crate) struct Iso {
    pub ctx: Ctx,
    pub sql: ClickHouseSqlStore,
    pub switch: Switch,
    pub a: ProjectId,
    pub b: ProjectId,
    pub canary_a: String,
    pub canary_b: String,
    pub canary_admin: String,
    /// The settings profile, fitted to the server under test.
    pub profile: SqlProfile,
    /// Set once the hourly quota is lifted for the run; W0 then skips the quota's limits.
    pub quota_lifted: std::sync::atomic::AtomicBool,
}

/// The condition that narrows a table's time column to the fixtures' day, which the window backstop asks for.
pub(crate) fn window(def: &TableDef) -> String {
    format!("{} >= now() - INTERVAL 2 DAY", def.retention.time_column)
}

/// The first SQL-readable string column of a table, where a fixture row holds its canary.
pub(crate) fn canary_column(def: &TableDef) -> Option<&'static str> {
    def.sql_columns()
        .find(|c| matches!(c.ty, Ty::String | Ty::LowCardinalityString))
        .map(|c| c.name)
}

impl Iso {
    async fn new(env: &Env, target: &Target, switch: Switch) -> Result<Self> {
        let ctx = Ctx::new(env, target)?;
        let sql = ClickHouseSqlStore::new(
            ctx.store.clone(),
            SqlStoreConfig {
                passwords: Arc::new(Passwords),
                variant: ctx.variant.clone(),
                admin_user: Ident::new(&ctx.config.admin.user)?,
                statements_per_hour: SQL_QUOTA_QUERIES_PER_HOUR,
            },
        );
        let profile = sql
            .server_profile(SqlProfile::default())
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let token =
            |role: &str| format!("cnry_{role}_{}", env.new_id().to_string().replace('-', ""));
        Ok(Self {
            a: ctx.project(),
            b: ctx.project(),
            canary_a: token("a"),
            canary_b: token("b"),
            canary_admin: token("admin"),
            quota_lifted: std::sync::atomic::AtomicBool::new(false),
            profile,
            ctx,
            sql,
            switch,
        })
    }

    /// Schema, access steps A1 and A2 (each twice: they are idempotent), the two projects' identities and the
    /// fixture rows.
    async fn bootstrap(&self) -> Result<String> {
        let schema = schema_base(&self.ctx.variant, Granularity::Day);
        let service = access_service(&self.ctx.variant, &self.ctx.access_params()?);
        let params = self
            .sql
            .workspace_params(self.profile)
            .map_err(|e| anyhow!("{e}"))?;
        let workspace = access_workspace(&self.ctx.variant, &params);
        for step in [&schema, &service, &workspace, &workspace] {
            self.ctx
                .store
                .apply_migration(step)
                .await
                .with_context(|| format!("migration step {}", step.id))?;
        }
        for project in [self.a, self.b] {
            self.provision(project).await?;
            // Provisioning is idempotent.
            self.provision(project).await?;
        }
        self.ctx
            .admin(
                "CREATE TABLE IF NOT EXISTS crucible.sqliso_admin_only (project_id UUID, canary String) ENGINE = \
                 MergeTree ORDER BY project_id",
            )
            .await?;
        self.ctx
            .admin(format!(
                "INSERT INTO crucible.sqliso_admin_only VALUES ('{}', '{}')",
                self.b.uuid(),
                self.canary_admin
            ))
            .await?;
        // The admin canary also sits in the text of an admin query, where the query log and the process list
        // would show it.
        self.ctx
            .admin_text(format!("SELECT '{}' FORMAT Null", self.canary_admin))
            .await?;
        let mut rows = 0;
        for def in sql_tables() {
            if def.name() == "sql_probe" {
                continue;
            }
            for (project, canary) in [(self.a, &self.canary_a), (self.b, &self.canary_b)] {
                self.ctx.admin(fixture_row(def, project, canary)).await?;
                rows += 1;
            }
        }
        // Rule 1 of contract 7.3 with the workspace policies in place: every service identity still sees every
        // row. Detection includes that check.
        self.ctx
            .store
            .detect()
            .await
            .context("service identities after the workspace policies")?;
        Ok(format!(
            "schema, A1 and A2 applied (A2 twice), projects A and B provisioned twice each, {rows} fixture rows \
             with canaries in every string column, an admin-only table and an admin query text; every service \
             identity still sees every row"
        ))
    }

    pub(crate) async fn provision(&self, project: ProjectId) -> Result<()> {
        self.sql
            .provision(project, 0, &RetentionSettings::default())
            .await
            .map_err(|e| anyhow!("provision {project}: {e}"))
    }

    /// Sends a statement as a project's workspace identity, with settings as URL parameters.
    pub(crate) async fn send(
        &self,
        project: ProjectId,
        sql: &str,
        settings: &[(&str, &str)],
    ) -> Result<Reply> {
        let user = workspace_user(project);
        let reply = self
            .ctx
            .store
            .raw_as(
                &user,
                &Passwords.password(&user, 0),
                sql,
                settings,
                Duration::from_secs(60),
            )
            .await
            .with_context(|| format!("as {user}: {sql}"))?;
        let reply = match reply {
            RawReply::Rows(text) => Reply::Rows(text),
            RawReply::Refused { code, message } => Reply::Refused { code, message },
        };
        self.no_leak(sql, &reply)?;
        Ok(reply)
    }

    /// Sends a statement as project A. Every answer passes through the canary oracle.
    pub(crate) async fn attack(&self, sql: &str) -> Result<Reply> {
        self.send(self.a, sql, &[]).await
    }

    /// The canary oracle: nothing that A receives, rows or error text, may hold B's canary or the admin's.
    fn no_leak(&self, sql: &str, reply: &Reply) -> Result<()> {
        for (owner, canary) in [
            ("project B", &self.canary_b),
            ("the admin", &self.canary_admin),
        ] {
            if reply.text().contains(canary.as_str()) {
                bail!("LEAK: the canary of {owner} reached project A through: {sql}");
            }
        }
        Ok(())
    }

    /// A must be refused.
    pub(crate) async fn refused(&self, sql: &str) -> Result<u32> {
        let reply = self.attack(sql).await?;
        reply
            .code()
            .ok_or_else(|| anyhow!("expected a refusal, but {sql:?} {}", reply.brief()))
    }

    /// A must get exactly `expected`.
    pub(crate) async fn answers(&self, sql: &str, expected: &str) -> Result<()> {
        let reply = self.attack(sql).await?;
        expect(reply.rows() == Some(expected), || {
            format!("expected {expected:?}, but {sql:?} {}", reply.brief())
        })
    }

    /// A must be refused or get exactly `expected`.
    pub(crate) async fn refused_or(&self, sql: &str, expected: &str) -> Result<()> {
        let reply = self.attack(sql).await?;
        expect(reply.rows().is_none_or(|rows| rows == expected), || {
            format!(
                "expected a refusal or {expected:?}, but {sql:?} {}",
                reply.brief()
            )
        })
    }

    /// What the admin sees of the catalogue and of the tables a workspace identity must never change.
    pub(crate) async fn catalogue(&self) -> Result<String> {
        self.ctx
            .admin_text(
                "SELECT (SELECT count() FROM system.tables WHERE database LIKE 'crucible%'), (SELECT count() FROM \
                 system.databases), (SELECT count() FROM system.users), (SELECT count() FROM system.roles), \
                 (SELECT count() FROM system.row_policies), (SELECT count() FROM system.grants), (SELECT count() \
                 FROM system.quotas), (SELECT count() FROM system.settings_profiles), (SELECT count() FROM \
                 system.settings_profile_elements), (SELECT count() FROM crucible.sql_probe), (SELECT count() \
                 FROM crucible.sqliso_admin_only), (SELECT sum(total_rows) FROM system.tables WHERE database = \
                 'crucible')",
            )
            .await
    }

    /// The admin still gets an answer: no attack took the server down.
    pub(crate) async fn admin_ping(&self) -> Result<()> {
        let answer = self.ctx.admin_text("SELECT 1").await?;
        expect(answer.trim() == "1", || "the admin ping failed".to_owned())
    }

    /// W0: the deployment is the one the suite was told about. These checks read the catalogue as the admin;
    /// each of them fails for at least one broken deployment of layer D.
    pub(crate) async fn deployment(&self) -> Result<String> {
        // Every policy of both layers exists, for the static layer and for A and B.
        let inventory = self.ctx.admin_text(policy_inventory()).await?;
        let found: Vec<(String, String, bool)> = inventory
            .lines()
            .filter_map(|line| {
                let mut fields = line.split('\t');
                Some((
                    fields.next()?.to_owned(),
                    fields.next()?.to_owned(),
                    fields.next()? == "1",
                ))
            })
            .collect();
        let mut projects = vec![self.a, self.b];
        projects.extend(crucible_schema::PROBE_PROJECTS);
        for (name, table, restrictive) in expected_policies(&projects) {
            expect(
                found
                    .iter()
                    .any(|(n, t, r)| *n == name && t == table && *r == restrictive),
                || format!("row policy {name} on {table} (restrictive: {restrictive}) is missing"),
            )?;
        }
        // The role holds exactly its column grants and the view database: no source, no system table.
        let grants = self
            .ctx
            .admin_text(format!(
                "SELECT access_type, ifNull(database, ''), ifNull(table, ''), count() FROM system.grants WHERE \
                 role_name = '{SQL_READER_ROLE}' GROUP BY ALL ORDER BY ALL"
            ))
            .await?;
        for line in grants.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            let ok = matches!(fields.as_slice(), ["SELECT", "crucible", table, _] if !table.is_empty())
                || matches!(fields.as_slice(), ["SELECT", "crucible_sql", "", _]);
            expect(ok, || format!("the role holds an unexpected grant: {line}"))?;
        }
        let direct = self
            .ctx
            .admin_count(
                "SELECT count() FROM system.grants WHERE startsWith(ifNull(user_name, ''), 'crucible_sql_')",
            )
            .await?;
        expect(direct == 0, || {
            format!("{direct} grants are addressed to a workspace user directly")
        })?;
        // The profile: read-only mode 1 and every element constant.
        let elements = self
            .ctx
            .admin_text(format!(
                "SELECT setting_name, value, writability FROM system.settings_profile_elements WHERE profile_name \
                 = '{SQL_PROFILE}' ORDER BY setting_name"
            ))
            .await?;
        let expected = self.profile.settings();
        for (name, value) in &expected {
            let value = value.trim_matches('\'');
            let line = elements
                .lines()
                .find(|line| line.split('\t').next() == Some(name))
                .ok_or_else(|| anyhow!("the profile has no element {name}"))?;
            let fields: Vec<&str> = line.split('\t').collect();
            expect(
                fields.get(1) == Some(&value) && fields.get(2) == Some(&"CONST"),
                || format!("profile element {name} should be {value} CONST, found {line:?}"),
            )?;
        }
        expect(elements.lines().count() == expected.len(), || {
            format!(
                "the profile has {} elements, expected {}",
                elements.lines().count(),
                expected.len()
            )
        })?;
        let profile = self
            .ctx
            .admin_count(format!(
                "SELECT count() FROM system.users WHERE startsWith(name, 'crucible_sql_') AND name NOT IN (SELECT                  assumeNotNull(user_name) FROM system.settings_profile_elements WHERE inherit_profile =                  '{SQL_PROFILE}' AND user_name IS NOT NULL)"
            ))
            .await?;
        expect(profile == 0, || {
            format!("{profile} workspace users do not carry the settings profile")
        })?;
        // The quota.
        let quota = self
            .ctx
            .admin_text(format!(
                "SELECT max_queries, max_errors, max_read_rows, toUInt64(max_execution_time) FROM \
                 system.quota_limits WHERE quota_name = '{SQL_QUOTA}'"
            ))
            .await?;
        let wanted = format!(
            "{SQL_QUOTA_QUERIES_PER_HOUR}\t{SQL_QUOTA_ERRORS_PER_HOUR}\t{SQL_QUOTA_READ_ROWS_PER_HOUR}\t\
             {SQL_QUOTA_EXECUTION_SECONDS_PER_HOUR}"
        );
        let lifted = self.quota_lifted.load(std::sync::atomic::Ordering::Relaxed);
        expect(lifted || quota.trim_end() == wanted, || {
            format!("the quota limits are {quota:?}, expected {wanted:?}")
        })?;
        // The server switch behaves as declared. A scratch user with a grant and no policy of its own shows it:
        // on a table that has policies it reads every row (permissive) or none (strict).
        self.ctx
            .admin("CREATE USER IF NOT EXISTS sqliso_switch_probe IDENTIFIED WITH sha256_password BY 'sqliso-switch'")
            .await?;
        self.ctx
            .admin("GRANT SELECT ON crucible.sql_probe TO sqliso_switch_probe")
            .await?;
        let seen = self
            .ctx
            .store
            .raw_as(
                "sqliso_switch_probe",
                "sqliso-switch",
                "SELECT count() FROM crucible.sql_probe",
                &[],
                Duration::from_secs(30),
            )
            .await?;
        self.ctx
            .admin("DROP USER IF EXISTS sqliso_switch_probe")
            .await?;
        let observed = match &seen {
            RawReply::Rows(text) if text.trim() == "0" => Switch::Strict,
            RawReply::Rows(_) => Switch::Permissive,
            RawReply::Refused { message, .. } => bail!("the switch probe was refused: {message}"),
        };
        expect(observed == self.switch, || {
            format!(
                "the server switch behaves as {observed:?}, but the deployment is declared {:?}",
                self.switch
            )
        })?;
        Ok(format!(
            "{} policies in both forms, the role's grants are column grants and the view database only, {} \
             profile elements all CONST with readonly = 1, the quota limits, and the server switch observed as \
             {observed:?}",
            found.len(),
            expected.len()
        ))
    }

    /// The runtime self-probe (contract 7.5) through the production adapter.
    pub(crate) async fn runtime_probe(&self) -> Result<String> {
        let report = self.sql.probe().await.map_err(|e| anyhow!("{e}"))?;
        expect(report.breaches.is_empty(), || {
            format!("the runtime self-probe reports {:?}", report.breaches)
        })?;
        // The probe identities are outside the hourly quota: a probe is about twenty refused statements, so
        // more probes than the error allowance of an hour must all still run (several nodes probe at once).
        if !self.quota_lifted.load(std::sync::atomic::Ordering::Relaxed) {
            for n in 0..20 {
                let again = self
                    .sql
                    .probe()
                    .await
                    .map_err(|e| anyhow!("probe {n} of 20 in a row: {e}"))?;
                expect(again.breaches.is_empty(), || {
                    format!("probe {n} reports {:?}", again.breaches)
                })?;
            }
            let used = self
                .ctx
                .admin_count(format!(
                    "SELECT count() FROM system.quotas_usage WHERE quota_name = '{SQL_QUOTA}' AND quota_key = '{}'",
                    workspace_user(crucible_schema::PROBE_PROJECTS[0])
                ))
                .await?;
            expect(used == 0, || {
                "the probe identity is counted against the workspace quota".to_owned()
            })?;
        }
        Ok(format!(
            "{} statements as the probe identity on {}, no breach; 21 probes in a row at the real quota all ran, \
             and the probe identity has no quota usage",
            report.statements, report.version
        ))
    }

    /// Cancelling a statement that is not running succeeds (contract 2.9), as when a run ends just before its
    /// cancel lands. From 26.11 ClickHouse refuses a `KILL QUERY` that matches no statement, with code 785.
    pub(crate) async fn cancel_ended(&self) -> Result<String> {
        let scope = scope(self.a).map_err(|e| anyhow!("{e}"))?;
        self.sql
            .cancel(scope, self.ctx.env.new_id())
            .await
            .map_err(|e| anyhow!("{e}"))?;
        Ok("cancelling a statement of project A that is not running succeeds".to_owned())
    }

    /// One start with a wrong master key under `--migrate` (contracts 6.6 and 7.5). Access step A2 rendered under
    /// another key differs from the deployed one only in the probe identities' password digests, and its ledger
    /// form is the same, so the ledger does not apply it again. Applied anyway, as an earlier version did, it leaves
    /// the probe identity refusing the right key's password and the probe unable to run. Provisioning the probe
    /// identities again with the right key, as `crucible migrate` and the identity reconciler do, repairs it.
    pub(crate) async fn wrong_master_key(&self) -> Result<String> {
        let other = ClickHouseSqlStore::new(
            self.ctx.store.clone(),
            SqlStoreConfig {
                passwords: Arc::new(OtherKey),
                variant: self.ctx.variant.clone(),
                admin_user: Ident::new(&self.ctx.config.admin.user)?,
                statements_per_hour: SQL_QUOTA_QUERIES_PER_HOUR,
            },
        );
        let step = |store: &ClickHouseSqlStore| -> Result<MigrationStep> {
            let params = store
                .workspace_params(self.profile)
                .map_err(|e| anyhow!("{e}"))?;
            Ok(access_workspace(&self.ctx.variant, &params))
        };
        let (right, wrong) = (step(&self.sql)?, step(&other)?);
        let ledger = |step: &MigrationStep| -> Vec<String> {
            step.statements
                .iter()
                .map(|statement| without_secrets(statement).into_owned())
                .collect()
        };
        expect(right.statements != wrong.statements, || {
            "access step A2 does not carry the probe identities' password digests".to_owned()
        })?;
        expect(ledger(&right) == ledger(&wrong), || {
            "the ledger form of access step A2 depends on the master key".to_owned()
        })?;
        self.ctx
            .store
            .apply_migration(&wrong)
            .await
            .context("access step A2 under another key")?;
        match self.sql.probe().await {
            Err(SqlError::Unavailable(_)) => {}
            other => bail!(
                "with another key's passwords on the probe identities the probe should not run, but it gave \
                 {other:?}"
            ),
        }
        for project in PROBE_PROJECTS {
            self.sql
                .provision(project, 0, &RetentionSettings::default())
                .await
                .map_err(|e| anyhow!("provisioning the probe identity {project}: {e}"))?;
        }
        let report = self
            .sql
            .probe()
            .await
            .map_err(|e| anyhow!("the probe after the repair: {e}"))?;
        expect(report.breaches.is_empty(), || {
            format!("the probe after the repair reports {:?}", report.breaches)
        })?;
        Ok(format!(
            "A2 under another key differs only in its digests and has the same ledger form; applied, it left the \
             probe unable to sign in; provisioning the probe identities with the right key restored a clean probe \
             of {} statements",
            report.statements
        ))
    }
}

/// One row of `project` in a table: its time column at the current time, every string column the canary, and
/// every other column its type's default.
pub(crate) fn fixture_row(def: &TableDef, project: ProjectId, canary: &str) -> String {
    let mut columns = vec!["project_id".to_owned()];
    let mut values = vec![format!("toUUID('{}')", project.uuid())];
    for column in def.columns {
        if column.name == "project_id" {
            continue;
        }
        let value = if column.name == def.retention.time_column
            || def.partition.column() == Some(column.name)
        {
            "now()".to_owned()
        } else {
            match column.ty {
                Ty::String | Ty::LowCardinalityString | Ty::Nullable(&Ty::String) => {
                    format!("'{canary}'")
                }
                Ty::Array(&Ty::String) => format!("['{canary}']"),
                _ => continue,
            }
        };
        columns.push(column.name.to_owned());
        values.push(value);
    }
    format!(
        "INSERT INTO crucible.{} ({}) SELECT {}",
        def.name(),
        columns.join(", "),
        values.join(", ")
    )
}

/// Runs the suite and returns one outcome per row. The first outcome is the bootstrap; when it fails no other
/// row runs.
pub async fn run(env: &Env, target: &Target, options: &Options) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    macro_rules! row {
        ($name:expr, $future:expr) => {{
            let result: Result<String> = $future.await;
            outcomes.push(Outcome {
                row: $name,
                result: result.map_err(|error| format!("{error:#}")),
            });
        }};
    }
    let iso = match Iso::new(env, target, options.switch).await {
        Ok(iso) => iso,
        Err(error) => {
            return vec![Outcome {
                row: "sqliso bootstrap",
                result: Err(format!("{error:#}")),
            }];
        }
    };
    row!("sqliso bootstrap", iso.bootstrap());
    if outcomes.iter().any(|o| o.result.is_err()) {
        return outcomes;
    }
    row!("W0 deployment as declared", iso.deployment());
    row!("Runtime self-probe (7.5)", iso.runtime_probe());
    row!("Cancel of a statement that has ended (2.9)", iso.cancel_ended());
    row!(
        "A start with a wrong master key (6.6, 7.5)",
        iso.wrong_master_key()
    );
    if options.layer_a {
        for (name, result) in layer_a::run(&iso, options).await {
            outcomes.push(Outcome {
                row: name,
                result: result.map_err(|error| format!("{error:#}")),
            });
        }
    } else {
        // Layers B and D send thousands of statements as one identity; the hourly quota is not an isolation
        // mechanism, and W8 is where it is tested.
        row!("Quota lifted for the run", layer_a::lift_quota(&iso));
    }
    if options.fuzz > 0 {
        row!(
            "Layer B grammar fuzzing with canary oracles",
            fuzz::run(&iso, options.fuzz, options.seed)
        );
    }
    if options.layer_d {
        for (name, result) in mutants::run(&iso).await {
            outcomes.push(Outcome {
                row: name,
                result: result.map_err(|error| format!("{error:#}")),
            });
        }
    }
    row!("Admin ping after the suite", async {
        iso.admin_ping().await?;
        Ok("the server still answers the admin".to_owned())
    });
    outcomes
}

/// The names of both policy forms, for the mutants.
pub(crate) const FORMS: [PolicyForm; 2] = [PolicyForm::Permissive, PolicyForm::Restrictive];

/// The curated view of a table.
pub(crate) fn view_of(def: &TableDef) -> Option<&'static str> {
    CURATED_VIEWS
        .iter()
        .find(|view| view.table == def.id)
        .map(|view| view.name)
}
