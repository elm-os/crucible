//! The SQL workspace's access objects (contract sections 7.4 and 7.5): access step A2 rendered from
//! [`access_workspace_shape`](crate::access_workspace_shape), the per-project statements the identity
//! reconciler issues, the settings profile, the quota, and the curated views of `crucible_sql`.
//!
//! Isolation between projects is carried by the row policies rendered here and by nothing else. Every
//! predicate exists twice, `AS PERMISSIVE` and `AS RESTRICTIVE`, on two layers:
//!
//! - the static layer, addressed to the role `crucible_sql_reader`, compares `project_id` with the project id
//!   that the caller's own user name carries;
//! - the per-project layer, addressed to one user, names the project id as a literal and carries the
//!   retention term.
//!
//! A row is visible only when both layers admit it, whichever way the server switch
//! `users_without_row_policies_can_read_rows` is set.

use core::fmt::Write as _;

use crucible_records::{ProjectId, TableId};

use crate::access::{PasswordHash, service_policy_check};
use crate::ddl::{DATABASE, Ident, SQL_DATABASE, Variant};
use crate::migrate::{
    ACCESS_WORKSPACE, MigrationStep, PolicyForm, StepKind, WorkspaceAction, access_workspace_shape,
};
use crate::partition::RetentionSettings;
use crate::table::{TableDef, TableKind};
use crate::tables::{TABLES, table};
use crate::types::Ty;

/// The role every workspace identity holds.
pub const SQL_READER_ROLE: &str = "crucible_sql_reader";
/// The settings profile of every workspace identity.
pub const SQL_PROFILE: &str = "crucible_sql_profile";
/// The quota of the workspace role, keyed by user name.
pub const SQL_QUOTA: &str = "crucible_sql_quota";
/// The prefix of a workspace user name; the project id follows as 32 lowercase hex characters.
pub const SQL_USER_PREFIX: &str = "crucible_sql_";

/// The two reserved probe project ids (contract 7.5). Their timestamp field is the largest a UUIDv7 can hold,
/// so no minted id can equal them.
pub const PROBE_PROJECTS: [ProjectId; 2] = [
    ProjectId::from_bytes([
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xbf, 0xff, 0, 0, 0, 0, 0, 0x0a,
    ]),
    ProjectId::from_bytes([
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0xff, 0xbf, 0xff, 0, 0, 0, 0, 0, 0x0b,
    ]),
];

/// The canary a probe project's row of `sql_probe` holds.
pub fn probe_canary(project: ProjectId) -> String {
    format!("crucible-probe-canary-{}", project_hex(project))
}

/// A project id as 32 lowercase hex characters.
pub fn project_hex(project: ProjectId) -> String {
    project
        .to_bytes()
        .iter()
        .fold(String::with_capacity(32), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// The user name of a project's workspace identity: `crucible_sql_<project hex>` (contract 7.1).
pub fn workspace_user(project: ProjectId) -> String {
    format!("{SQL_USER_PREFIX}{}", project_hex(project))
}

/// The hourly limits of [`SQL_QUOTA`] (contract 7.4).
pub const SQL_QUOTA_QUERIES_PER_HOUR: u64 = 600;
/// Errors per hour.
pub const SQL_QUOTA_ERRORS_PER_HOUR: u64 = 300;
/// Rows read per hour.
pub const SQL_QUOTA_READ_ROWS_PER_HOUR: u64 = 20_000_000_000;
/// Execution seconds per hour.
pub const SQL_QUOTA_EXECUTION_SECONDS_PER_HOUR: u64 = 1_800;

/// The tunable values of `crucible_sql_profile` (contract 7.4). They are deployment defaults, fixed before
/// provisioning; every rendered setting is declared `CONST`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SqlProfile {
    /// `max_execution_time`, in seconds.
    pub max_execution_time_secs: u32,
    /// `max_memory_usage`, in bytes.
    pub max_memory_usage: u64,
    /// `max_memory_usage_for_user`, in bytes.
    pub max_memory_usage_for_user: u64,
    /// `max_rows_to_read`.
    pub max_rows_to_read: u64,
    /// `max_bytes_to_read`.
    pub max_bytes_to_read: u64,
    /// `max_columns_to_read`.
    pub max_columns_to_read: u32,
    /// `max_result_rows`: one more than the grid shows, so a cut result is told from a full one.
    pub max_result_rows: u64,
    /// `max_result_bytes`.
    pub max_result_bytes: u64,
    /// `max_ast_depth`.
    pub max_ast_depth: u32,
    /// `max_ast_elements`.
    pub max_ast_elements: u32,
    /// `max_expanded_ast_elements`.
    pub max_expanded_ast_elements: u32,
    /// `max_subquery_depth`.
    pub max_subquery_depth: u32,
    /// `max_query_size`, in bytes.
    pub max_query_size: u32,
    /// The row cap of joins, sets and group-by states.
    pub max_state_rows: u64,
    /// The byte cap of joins and sets.
    pub max_state_bytes: u64,
    /// `max_threads`.
    pub max_threads: u32,
    /// `max_concurrent_queries_for_user`.
    pub max_concurrent_queries_for_user: u32,
    /// `max_partitions_to_read`, a window backstop. Rendered with `force_index_by_date` only when the
    /// backstops are on (contract 7.4, conformance script W13).
    pub max_partitions_to_read: u32,
    /// Whether the window backstops are rendered.
    pub window_backstops: bool,
}

impl Default for SqlProfile {
    fn default() -> Self {
        Self {
            max_execution_time_secs: 30,
            max_memory_usage: 2 << 30,
            max_memory_usage_for_user: 4 << 30,
            max_rows_to_read: 1_000_000_000,
            max_bytes_to_read: 50 << 30,
            max_columns_to_read: 200,
            max_result_rows: 10_001,
            max_result_bytes: 32 << 20,
            max_ast_depth: 200,
            max_ast_elements: 10_000,
            max_expanded_ast_elements: 100_000,
            max_subquery_depth: 10,
            max_query_size: 128 << 10,
            max_state_rows: 10_000_000,
            max_state_bytes: 1 << 30,
            max_threads: 4,
            max_concurrent_queries_for_user: 2,
            // A window of the default maximum, 30 days, with room for the retention term (default; revisit).
            max_partitions_to_read: 100,
            window_backstops: true,
        }
    }
}

impl SqlProfile {
    /// Every setting of the profile with its value, in rendering order. `readonly = 1` comes first.
    pub fn settings(&self) -> Vec<(&'static str, String)> {
        let n = |value: u64| value.to_string();
        let text = |value: &str| format!("'{value}'");
        let mut settings = vec![
            // Mode.
            ("readonly", n(1)),
            ("allow_ddl", n(0)),
            ("allow_introspection_functions", n(0)),
            ("serialize_query_plan", n(0)),
            ("use_query_cache", n(0)),
            // Time and memory; nothing spills to disk.
            (
                "max_execution_time",
                n(u64::from(self.max_execution_time_secs)),
            ),
            ("timeout_overflow_mode", text("throw")),
            ("max_memory_usage", n(self.max_memory_usage)),
            (
                "max_memory_usage_for_user",
                n(self.max_memory_usage_for_user),
            ),
            ("max_bytes_before_external_sort", n(0)),
            ("max_bytes_before_external_group_by", n(0)),
            ("max_bytes_ratio_before_external_sort", n(0)),
            ("max_bytes_ratio_before_external_group_by", n(0)),
            // Read volume.
            ("max_rows_to_read", n(self.max_rows_to_read)),
            ("max_bytes_to_read", n(self.max_bytes_to_read)),
            (
                "max_columns_to_read",
                n(u64::from(self.max_columns_to_read)),
            ),
            ("read_overflow_mode", text("throw")),
            // Result.
            ("max_result_rows", n(self.max_result_rows)),
            ("max_result_bytes", n(self.max_result_bytes)),
            ("result_overflow_mode", text("break")),
            // Complexity.
            ("max_ast_depth", n(u64::from(self.max_ast_depth))),
            ("max_ast_elements", n(u64::from(self.max_ast_elements))),
            (
                "max_expanded_ast_elements",
                n(u64::from(self.max_expanded_ast_elements)),
            ),
            ("max_subquery_depth", n(u64::from(self.max_subquery_depth))),
            ("max_query_size", n(u64::from(self.max_query_size))),
            ("max_rows_in_join", n(self.max_state_rows)),
            ("max_bytes_in_join", n(self.max_state_bytes)),
            ("join_overflow_mode", text("throw")),
            ("max_rows_in_set", n(self.max_state_rows)),
            ("max_bytes_in_set", n(self.max_state_bytes)),
            ("set_overflow_mode", text("throw")),
            ("max_rows_to_group_by", n(self.max_state_rows)),
            ("group_by_overflow_mode", text("throw")),
            // The sleep functions refuse anything above one microsecond per block; zero would mean no limit.
            ("function_sleep_max_microseconds_per_block", n(1)),
            // Concurrency.
            ("max_threads", n(u64::from(self.max_threads))),
            (
                "max_concurrent_queries_for_user",
                n(u64::from(self.max_concurrent_queries_for_user)),
            ),
            // HTTP.
            ("cancel_http_readonly_queries_on_client_close", n(1)),
            ("output_format_json_quote_64bit_integers", n(1)),
            ("date_time_output_format", text("iso")),
        ];
        if self.window_backstops {
            settings.push(("force_index_by_date", n(1)));
            settings.push((
                "max_partitions_to_read",
                n(u64::from(self.max_partitions_to_read)),
            ));
        }
        settings
    }

    fn clause(&self) -> String {
        self.settings()
            .iter()
            .map(|(name, value)| format!("{name} = {value} CONST"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The inputs of access step A2 that are not schema data.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WorkspaceParams {
    /// The operator-supplied admin user, as in access step A1.
    pub admin_user: Ident,
    /// The settings profile.
    pub profile: SqlProfile,
    /// The password digests of the two probe identities, in the order of [`PROBE_PROJECTS`].
    pub probe: [PasswordHash; 2],
}

fn form_keyword(form: PolicyForm) -> &'static str {
    match form {
        PolicyForm::Permissive => "PERMISSIVE",
        PolicyForm::Restrictive => "RESTRICTIVE",
    }
}

fn form_suffix(form: PolicyForm) -> &'static str {
    match form {
        PolicyForm::Permissive => "permissive",
        PolicyForm::Restrictive => "restrictive",
    }
}

const FORMS: [PolicyForm; 2] = [PolicyForm::Permissive, PolicyForm::Restrictive];

/// The SQL-readable base tables.
pub fn sql_tables() -> impl Iterator<Item = &'static TableDef> {
    TABLES.iter().filter(|def| def.sql_readable)
}

/// The name of a table's static, role-wide policy in one form.
pub fn static_policy_name(form: PolicyForm) -> String {
    format!("crucible_sql_static_{}", form_suffix(form))
}

/// The name of a project's policy in one form; the table completes the policy's identity.
pub fn project_policy_name(project: ProjectId, form: PolicyForm) -> String {
    format!("{}_{}", workspace_user(project), form_suffix(form))
}

/// The predicate of the static layer: the row's project is the one the caller's user name carries, and it is
/// never the nil project. A user name without a 32-hex suffix parses to the nil UUID, so it matches no row.
pub const STATIC_PREDICATE: &str = "project_id = toUUIDOrZero(substring(currentUser(), 14)) AND project_id != \
                                    toUUID('00000000-0000-0000-0000-000000000000')";

/// The static, role-wide policy of one table in one form.
pub fn static_policy(table_id: TableId, form: PolicyForm, variant: &Variant) -> String {
    format!(
        "CREATE ROW POLICY IF NOT EXISTS {}{} ON {DATABASE}.{} AS {} FOR SELECT USING {STATIC_PREDICATE} TO \
         {SQL_READER_ROLE}",
        static_policy_name(form),
        variant.on_cluster(),
        table_id.name(),
        form_keyword(form),
    )
}

/// The predicate of a project's policy on one table: the project id as a literal, and the retention term on the
/// table's time column unless that retention is unset (contract 7.4).
pub fn project_predicate(
    def: &TableDef,
    project: ProjectId,
    retention: &RetentionSettings,
) -> String {
    let mut predicate = format!("project_id = toUUID('{}')", project.uuid());
    if let Some(days) = retention.days(def.retention.binding) {
        let _ = write!(
            predicate,
            " AND {} >= now() - toIntervalDay({days})",
            def.retention.time_column
        );
    }
    predicate
}

/// The statements that create or rewrite a project's policies: both forms on every SQL-readable table. Each
/// policy is created if it is missing and then altered to the current predicate, so a retention change never
/// drops a policy.
pub fn project_policies(
    project: ProjectId,
    retention: &RetentionSettings,
    variant: &Variant,
) -> Vec<String> {
    let on = variant.on_cluster();
    let user = workspace_user(project);
    let mut out = Vec::new();
    for def in sql_tables() {
        let predicate = project_predicate(def, project, retention);
        for form in FORMS {
            let name = project_policy_name(project, form);
            let table = def.name();
            out.push(format!(
                "CREATE ROW POLICY IF NOT EXISTS {name}{on} ON {DATABASE}.{table} AS {} FOR SELECT USING \
                 {predicate} TO {user}",
                form_keyword(form)
            ));
            out.push(format!(
                "ALTER ROW POLICY {name}{on} ON {DATABASE}.{table} AS {} FOR SELECT USING {predicate} TO {user}",
                form_keyword(form)
            ));
        }
    }
    out
}

/// The statements that provision one workspace identity, in the binding order of contract 7.4 as the server
/// allows it:
///
/// 1. the user, with its profile and **without** the role, so it can read nothing. ClickHouse refuses a row
///    policy addressed to a user that does not exist (code 511), so the user row has to come first;
/// 2. the project's policies, both forms on every table;
/// 3. the role grant, which is the step that lets the identity read.
///
/// A crash between any two statements leaves an identity that reads nothing or one that is fully fenced. Every
/// statement is idempotent; running the list again with a new `hash` rotates the password.
pub fn provision_identity(
    project: ProjectId,
    hash: &PasswordHash,
    retention: &RetentionSettings,
    variant: &Variant,
) -> Vec<String> {
    let on = variant.on_cluster();
    let user = workspace_user(project);
    let hash = hash.as_str();
    let mut out = vec![
        format!(
            "CREATE USER IF NOT EXISTS {user}{on} IDENTIFIED WITH sha256_hash BY '{hash}' DEFAULT DATABASE \
             {SQL_DATABASE} SETTINGS PROFILE '{SQL_PROFILE}'"
        ),
        format!(
            "ALTER USER {user}{on} IDENTIFIED WITH sha256_hash BY '{hash}' DEFAULT DATABASE {SQL_DATABASE} \
             SETTINGS PROFILE '{SQL_PROFILE}'"
        ),
    ];
    out.extend(project_policies(project, retention, variant));
    out.push(format!("GRANT{on} {SQL_READER_ROLE} TO {user}"));
    out
}

/// The statements that remove one workspace identity (contract 2.9): the user first, which takes the role
/// grant with it and ends every login at once, then the project's policies. Every statement is idempotent.
pub fn deprovision_identity(project: ProjectId, variant: &Variant) -> Vec<String> {
    let on = variant.on_cluster();
    let mut out = vec![format!(
        "DROP USER IF EXISTS {}{on}",
        workspace_user(project)
    )];
    for def in sql_tables() {
        for form in FORMS {
            out.push(format!(
                "DROP ROW POLICY IF EXISTS {}{on} ON {DATABASE}.{}",
                project_policy_name(project, form),
                def.name()
            ));
        }
    }
    out
}

/// The grant of a table's SQL-readable columns to the role.
fn grant_columns(def: &TableDef, variant: &Variant) -> String {
    let columns: Vec<&str> = def.sql_columns().map(|c| c.name).collect();
    format!(
        "GRANT{} SELECT({}) ON {DATABASE}.{} TO {SQL_READER_ROLE}",
        variant.on_cluster(),
        columns.join(", "),
        def.name()
    )
}

/// One curated view of `crucible_sql`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CuratedView {
    /// The view name inside `crucible_sql`.
    pub name: &'static str,
    /// The base table it reads.
    pub table: TableId,
    /// One sentence for the schema browser.
    pub description: &'static str,
}

/// The curated views (contract 7.4), in the order the schema browser lists them.
pub const CURATED_VIEWS: [CuratedView; 15] = [
    CuratedView {
        name: "spans",
        table: TableId::SpanCore,
        description: "One row per span: timing, names, model, token usage and cost.",
    },
    CuratedView {
        name: "span_payloads",
        table: TableId::SpanPayload,
        description: "The large fields of a span: input, output, metadata and attributes.",
    },
    CuratedView {
        name: "traces",
        table: TableId::TraceHeads,
        description: "One row per trace head: the root span's name, user, session and tags.",
    },
    CuratedView {
        name: "sessions",
        table: TableId::SessionRollup,
        description: "Per day and session: trace, span and error counts, tokens and cost.",
    },
    CuratedView {
        name: "users",
        table: TableId::UserRollup,
        description: "Per day and user: trace, span and error counts, tokens and cost.",
    },
    CuratedView {
        name: "mcp_operations",
        table: TableId::McpOps,
        description: "One row per MCP operation: method, target, status, duration and sizes.",
    },
    CuratedView {
        name: "mcp_contents",
        table: TableId::McpContent,
        description: "The arguments and results of MCP operations.",
    },
    CuratedView {
        name: "scores",
        table: TableId::FactScores,
        description: "Scores attached to traces, spans and sessions.",
    },
    CuratedView {
        name: "tags",
        table: TableId::FactTags,
        description: "Tag additions and removals.",
    },
    CuratedView {
        name: "flags",
        table: TableId::FactFlags,
        description: "Flags such as bookmarks and public sharing.",
    },
    CuratedView {
        name: "corrections",
        table: TableId::FactCorrections,
        description: "Corrected outputs attached to spans.",
    },
    CuratedView {
        name: "span_metrics",
        table: TableId::SpanMetrics,
        description: "Per minute and dimension: span counts, errors, tokens, cost and latency histograms.",
    },
    CuratedView {
        name: "mcp_tool_metrics",
        table: TableId::McpToolRollup,
        description: "Per minute and MCP target: call counts, sizes and latency histograms.",
    },
    CuratedView {
        name: "mcp_tools",
        table: TableId::McpToolInventory,
        description: "The tools, prompts and resources each MCP server has listed.",
    },
    CuratedView {
        name: "mcp_deploys",
        table: TableId::McpDeploys,
        description: "The deploys of each MCP server with their first and last sighting.",
    },
];

/// One column of a curated view, as the schema browser shows it.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct ViewColumn {
    /// The column name.
    pub name: &'static str,
    /// The ClickHouse type the view exposes.
    pub ty: String,
}

/// Whether a column holds a binary id that views expose as lowercase hex.
fn is_binary_id(ty: &Ty) -> bool {
    matches!(ty, Ty::FixedString(_))
}

impl CuratedView {
    /// The columns the view exposes: the table's SQL-readable columns, binary ids as hex strings and partial
    /// aggregates finalized.
    pub fn columns(&self) -> Vec<ViewColumn> {
        table(self.table)
            .sql_columns()
            .map(|column| ViewColumn {
                name: column.name,
                ty: if is_binary_id(column.ty.scalar()) {
                    "String".to_owned()
                } else {
                    column.ty.scalar().to_string()
                },
            })
            .collect()
    }

    /// The `SELECT` the view runs. Plain tables are projected; aggregating tables are grouped by their sort key
    /// and every other column is merged with its own function, which finalizes the partial rows. The time
    /// column stays a plain key, so a window filter on the view reaches the base table's partition key.
    fn select(&self) -> String {
        let def = table(self.table);
        let grouped = def.kind == TableKind::Aggregating;
        let mut items = Vec::new();
        let mut keys = Vec::new();
        for column in def.sql_columns() {
            let name = column.name;
            let item = match column.ty.agg_fn() {
                Some(function) if grouped => format!("{function}({name}) AS {name}"),
                _ => {
                    if grouped {
                        keys.push(name);
                    }
                    if is_binary_id(column.ty.scalar()) {
                        format!("lower(hex({name})) AS {name}")
                    } else {
                        name.to_owned()
                    }
                }
            };
            items.push(item);
        }
        let mut select = format!("SELECT {} FROM {DATABASE}.{}", items.join(", "), def.name());
        if grouped {
            let _ = write!(select, " GROUP BY {}", keys.join(", "));
        }
        select
    }

    /// The statement that creates or replaces the view. The view runs with invoker rights, so the caller's
    /// base-table policies apply.
    pub fn create(&self, variant: &Variant) -> String {
        format!(
            "CREATE OR REPLACE VIEW {SQL_DATABASE}.{}{} SQL SECURITY INVOKER AS {}",
            self.name,
            variant.on_cluster(),
            self.select()
        )
    }
}

/// The idempotent insert of a probe project's canary row into `sql_probe`: nothing is written when the project
/// already has a row.
fn probe_canary_insert(project: ProjectId) -> String {
    let id = project.uuid();
    format!(
        "INSERT INTO {DATABASE}.sql_probe (project_id, id, canary, start_time) SELECT toUUID('{id}'), 1, '{}', \
         toDateTime64('2026-01-01 00:00:00', 6, 'UTC') WHERE (SELECT count() FROM {DATABASE}.sql_probe WHERE \
         project_id = toUUID('{id}')) = 0",
        probe_canary(project)
    )
}

/// Access step A2 (contracts 7.3 and 7.4), rendered from [`access_workspace_shape`]: the check that A1's
/// service policies exist on every base table, the role, the profile, the quota, the column grants, the static
/// policies in both forms, the curated views, and the two probe identities with their canary rows. Every
/// statement is idempotent. The profile and the quota are created and then altered, never replaced: replacing it would give it
/// a new id and detach every user from its limits.
pub fn access_workspace(variant: &Variant, params: &WorkspaceParams) -> MigrationStep {
    let on = variant.on_cluster();
    let mut statements = Vec::new();
    for action in access_workspace_shape() {
        match action {
            WorkspaceAction::VerifyServicePolicies => statements.push(format!(
                "SELECT throwIf(({}) != {}, 'access step A1 has not created the service policy of every base \
                 table. Apply 0002_access_service first')",
                service_policy_check(&params.admin_user),
                TABLES.len()
            )),
            WorkspaceAction::CreateRole => {
                statements.push(format!("CREATE ROLE IF NOT EXISTS {SQL_READER_ROLE}{on}"));
            }
            WorkspaceAction::CreateProfile => {
                let clause = params.profile.clause();
                statements.push(format!(
                    "CREATE SETTINGS PROFILE IF NOT EXISTS {SQL_PROFILE}{on} SETTINGS {clause}"
                ));
                statements.push(format!(
                    "ALTER SETTINGS PROFILE {SQL_PROFILE}{on} SETTINGS {clause}"
                ));
            }
            WorkspaceAction::CreateQuota => {
                let limits = format!(
                    "KEYED BY user_name FOR INTERVAL 1 hour MAX queries = {SQL_QUOTA_QUERIES_PER_HOUR}, errors = \
                     {SQL_QUOTA_ERRORS_PER_HOUR}, read_rows = {SQL_QUOTA_READ_ROWS_PER_HOUR}, execution_time = \
                     {SQL_QUOTA_EXECUTION_SECONDS_PER_HOUR} TO {SQL_READER_ROLE}"
                );
                statements.push(format!(
                    "CREATE QUOTA IF NOT EXISTS {SQL_QUOTA}{on} {limits}"
                ));
                // Altering keeps the usage counters and brings a drifted quota back to its limits.
                statements.push(format!("ALTER QUOTA {SQL_QUOTA}{on} {limits}"));
            }
            WorkspaceAction::GrantColumns(id) => statements.push(grant_columns(table(id), variant)),
            WorkspaceAction::StaticPolicy(id, form) => {
                statements.push(static_policy(id, form, variant));
            }
            WorkspaceAction::CuratedViews => {
                statements.extend(CURATED_VIEWS.iter().map(|view| view.create(variant)));
                statements.push(format!(
                    "GRANT{on} SELECT ON {SQL_DATABASE}.* TO {SQL_READER_ROLE}"
                ));
            }
            WorkspaceAction::ProbeIdentities => {
                for (project, hash) in PROBE_PROJECTS.iter().zip(&params.probe) {
                    statements.push(probe_canary_insert(*project));
                    statements.extend(provision_identity(
                        *project,
                        hash,
                        &RetentionSettings::default(),
                        variant,
                    ));
                }
            }
        }
    }
    MigrationStep {
        id: ACCESS_WORKSPACE,
        kind: StepKind::Access,
        statements,
        settings: variant.ddl_settings(),
    }
}

/// A query that lists the workspace policies that exist, one `name<TAB>table<TAB>restrictive` line each. The
/// reconciler and the isolation suite compare it with [`expected_policies`].
pub fn policy_inventory() -> String {
    format!(
        "SELECT short_name, table, is_restrictive FROM system.row_policies WHERE database = '{DATABASE}' AND \
         startsWith(short_name, '{SQL_USER_PREFIX}') ORDER BY short_name, table"
    )
}

/// The `(policy name, table, restrictive)` triples that must exist for the static layer and for each of
/// `projects`, sorted like [`policy_inventory`].
pub fn expected_policies(projects: &[ProjectId]) -> Vec<(String, &'static str, bool)> {
    let mut out = Vec::new();
    for def in sql_tables() {
        for form in FORMS {
            let restrictive = form == PolicyForm::Restrictive;
            out.push((static_policy_name(form), def.name(), restrictive));
            for project in projects {
                out.push((project_policy_name(*project, form), def.name(), restrictive));
            }
        }
    }
    out.sort();
    out
}

// `STATIC_PREDICATE` reads the user name from its 14th character on.
const _: () = assert!(SQL_USER_PREFIX.len() == 13);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::RetentionBinding;

    fn hash(c: &str) -> PasswordHash {
        PasswordHash::new(&c.repeat(64)).unwrap()
    }

    fn params() -> WorkspaceParams {
        WorkspaceParams {
            admin_user: Ident::new("crucible_admin").unwrap(),
            profile: SqlProfile::default(),
            probe: [hash("a"), hash("b")],
        }
    }

    fn project() -> ProjectId {
        ProjectId::parse("0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b").unwrap()
    }

    #[test]
    fn user_names_carry_the_project_id_where_the_static_predicate_reads_it() {
        let user = workspace_user(project());
        assert_eq!(user, "crucible_sql_0193a1b27c3d7e4f8a5b6c7d8e9f0a1b");
        // `substring(currentUser(), 14)` is one-based: it starts right after the prefix.
        assert_eq!(&user[13..], project_hex(project()));
        assert!(Ident::new(&user).is_ok());
        for probe in PROBE_PROJECTS {
            assert!(!probe.is_nil());
            // The largest timestamp a UUIDv7 can carry: no minted id reaches it before the year 10889.
            assert_eq!(
                probe.uuid().v7_parts().map(|(ms, _)| ms),
                Some((1 << 48) - 1)
            );
        }
        assert_ne!(PROBE_PROJECTS[0], PROBE_PROJECTS[1]);
    }

    #[test]
    fn the_step_follows_the_shape_and_verifies_a1_first() {
        let step = access_workspace(&Variant::SingleNode, &params());
        assert_eq!((step.id, step.kind), (ACCESS_WORKSPACE, StepKind::Access));
        assert!(step.statements[0].starts_with("SELECT throwIf("));
        assert!(step.statements[0].contains("!= 18"));
        let position = |needle: &str| {
            step.statements
                .iter()
                .position(|s| s.starts_with(needle))
                .unwrap_or_else(|| panic!("{needle}"))
        };
        let role = position("CREATE ROLE IF NOT EXISTS crucible_sql_reader");
        let profile = position("CREATE SETTINGS PROFILE IF NOT EXISTS crucible_sql_profile");
        let quota = position("CREATE QUOTA IF NOT EXISTS crucible_sql_quota");
        let grant = position("GRANT SELECT(");
        let policy = position("CREATE ROW POLICY IF NOT EXISTS crucible_sql_static_permissive");
        let view = position("CREATE OR REPLACE VIEW crucible_sql.");
        let probe = position("INSERT INTO crucible.sql_probe");
        assert!(role < profile && profile < quota && quota < grant && grant < policy);
        assert!(policy < view && view < probe);
        for statement in &step.statements {
            assert!(!statement.contains(';'), "{statement}");
            assert!(
                !statement.contains("PROFILE OR REPLACE")
                    && !statement.contains("OR REPLACE SETTINGS"),
                "replacing the profile detaches its users: {statement}"
            );
        }
    }

    #[test]
    fn every_readable_table_gets_both_static_forms_and_a_curated_column_grant() {
        let step = access_workspace(&Variant::SingleNode, &params());
        for def in &TABLES {
            let grant = format!(" ON crucible.{} TO crucible_sql_reader", def.name());
            let granted: Vec<&String> = step
                .statements
                .iter()
                .filter(|s| s.starts_with("GRANT SELECT(") && s.ends_with(&grant))
                .collect();
            assert_eq!(
                granted.len(),
                usize::from(def.sql_readable),
                "{}",
                def.name()
            );
            for statement in granted {
                for hidden in [
                    "wal_epoch",
                    "batch_seq",
                    "lsn",
                    "received_at",
                    "marker_epoch",
                    "marker_batch_seq",
                    "marker_at",
                    "api_key_id",
                    "row_digest",
                ] {
                    assert!(
                        !statement.contains(&format!("{hidden},"))
                            && !statement.contains(&format!("{hidden})")),
                        "{statement}"
                    );
                }
                assert!(statement.contains("SELECT(project_id, "));
            }
            for form in FORMS {
                let policy = static_policy(def.id, form, &Variant::SingleNode);
                assert_eq!(step.statements.contains(&policy), def.sql_readable);
                assert!(policy.ends_with(" TO crucible_sql_reader"));
            }
        }
        // No wildcard grant on the base database, and nothing but SELECT.
        assert!(!step.statements.iter().any(|s| s.contains("ON crucible.*")));
        assert!(
            step.statements
                .iter()
                .filter(|s| s.starts_with("GRANT"))
                .all(|s| s.starts_with("GRANT SELECT")
                    || s.starts_with("GRANT crucible_sql_reader TO"))
        );
        assert!(!step.statements.iter().any(|s| s.contains("dedup_selftest")));
    }

    #[test]
    fn the_profile_is_readonly_one_and_every_setting_is_const() {
        let profile = SqlProfile::default();
        let settings = profile.settings();
        assert_eq!(settings[0], ("readonly", "1".to_owned()));
        let get = |name: &str| {
            settings
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("max_execution_time"), Some("30"));
        assert_eq!(get("max_memory_usage"), Some("2147483648"));
        assert_eq!(get("max_memory_usage_for_user"), Some("4294967296"));
        assert_eq!(get("max_rows_to_read"), Some("1000000000"));
        assert_eq!(get("max_bytes_to_read"), Some("53687091200"));
        assert_eq!(get("max_result_rows"), Some("10001"));
        assert_eq!(get("max_result_bytes"), Some("33554432"));
        assert_eq!(get("result_overflow_mode"), Some("'break'"));
        assert_eq!(get("max_query_size"), Some("131072"));
        assert_eq!(get("max_concurrent_queries_for_user"), Some("2"));
        assert_eq!(get("force_index_by_date"), Some("1"));
        let clause = profile.clause();
        assert_eq!(clause.matches(" CONST").count(), settings.len());
        let off = SqlProfile {
            window_backstops: false,
            ..profile
        };
        assert!(!off.clause().contains("force_index_by_date"));
        assert!(!off.clause().contains("max_partitions_to_read"));
    }

    #[test]
    fn provisioning_creates_the_user_without_the_role_then_policies_then_the_grant() {
        let retention = RetentionSettings {
            retention_days: Some(30),
            payload_retention_days: Some(7),
            mcp_content_retention_days: None,
        };
        let statements =
            provision_identity(project(), &hash("c"), &retention, &Variant::SingleNode);
        let user = workspace_user(project());
        assert!(statements[0].starts_with(&format!("CREATE USER IF NOT EXISTS {user} ")));
        assert!(statements[0].contains("SETTINGS PROFILE 'crucible_sql_profile'"));
        assert!(!statements[0].contains("crucible_sql_reader"));
        assert_eq!(
            statements.last().map(String::as_str),
            Some(format!("GRANT crucible_sql_reader TO {user}").as_str())
        );
        let grants = statements.iter().filter(|s| s.starts_with("GRANT")).count();
        assert_eq!(grants, 1);
        let readable = sql_tables().count();
        let created = statements
            .iter()
            .filter(|s| s.starts_with("CREATE ROW POLICY IF NOT EXISTS"))
            .count();
        assert_eq!(created, readable * 2);
        // Every policy names the project as a literal and is addressed to the user alone.
        for statement in statements.iter().filter(|s| s.contains("ROW POLICY")) {
            assert!(
                statement.contains("project_id = toUUID('0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b')")
            );
            assert!(statement.ends_with(&format!(" TO {user}")));
        }
        let on = |table: &str| {
            statements
                .iter()
                .find(|s| {
                    s.starts_with("ALTER ROW POLICY") && s.contains(&format!("crucible.{table} "))
                })
                .unwrap()
        };
        assert!(on("span_core").contains("start_time >= now() - toIntervalDay(30)"));
        assert!(on("span_payload").contains("start_time >= now() - toIntervalDay(7)"));
        assert!(on("mcp_content").contains("start_time >= now() - toIntervalDay(30)"));
        assert!(on("fact_scores").contains("target_time >= now() - toIntervalDay(30)"));
        assert!(on("trace_stats").contains("row_day >= now() - toIntervalDay(30)"));
        assert!(on("mcp_deploys").contains("last_seen >= now() - toIntervalDay(30)"));
        assert!(!on("sql_probe").contains("toIntervalDay"));

        let forever = provision_identity(
            project(),
            &hash("c"),
            &RetentionSettings::default(),
            &Variant::SingleNode,
        );
        assert!(!forever.iter().any(|s| s.contains("toIntervalDay")));
    }

    #[test]
    fn deprovisioning_drops_the_user_first_and_is_idempotent() {
        let statements = deprovision_identity(project(), &Variant::SingleNode);
        let user = workspace_user(project());
        assert_eq!(statements[0], format!("DROP USER IF EXISTS {user}"));
        assert_eq!(statements.len(), 1 + sql_tables().count() * 2);
        assert!(statements.iter().all(|s| s.contains(" IF EXISTS ")));
    }

    #[test]
    fn clustered_statements_run_on_the_cluster() {
        let variant = Variant::Clustered {
            cluster: Ident::new("crucible").unwrap(),
        };
        let mut statements = access_workspace(&variant, &params()).statements;
        statements.extend(deprovision_identity(project(), &variant));
        for statement in &statements {
            let local =
                statement.starts_with("SELECT throwIf(") || statement.starts_with("INSERT INTO");
            assert_eq!(
                statement.contains(" ON CLUSTER crucible"),
                !local,
                "{statement}"
            );
        }
    }

    #[test]
    fn views_cover_the_contract_list_finalize_aggregates_and_show_ids_as_hex() {
        let names: Vec<&str> = CURATED_VIEWS.iter().map(|v| v.name).collect();
        assert_eq!(
            names,
            [
                "spans",
                "span_payloads",
                "traces",
                "sessions",
                "users",
                "mcp_operations",
                "mcp_contents",
                "scores",
                "tags",
                "flags",
                "corrections",
                "span_metrics",
                "mcp_tool_metrics",
                "mcp_tools",
                "mcp_deploys"
            ]
        );
        for view in CURATED_VIEWS {
            let def = table(view.table);
            assert!(def.sql_readable);
            let create = view.create(&Variant::SingleNode);
            assert!(create.contains(" SQL SECURITY INVOKER AS SELECT project_id, "));
            assert_eq!(
                create.contains(" GROUP BY project_id"),
                def.kind == TableKind::Aggregating,
                "{create}"
            );
            for hidden in [
                "wal_epoch",
                "marker_epoch",
                "received_at",
                "row_digest",
                "api_key_id",
            ] {
                assert!(!create.contains(hidden), "{create}");
            }
            // The retention time column stays a plain column or a group key, so window filters push down.
            let time = def.retention.time_column;
            if def.retention.binding != RetentionBinding::None && def.order_by.contains(&time) {
                assert!(!create.contains(&format!("({time})")), "{create}");
            }
            for column in view.columns() {
                assert!(!column.ty.starts_with("FixedString"));
                assert!(!column.ty.starts_with("SimpleAggregateFunction"));
            }
        }
        let spans = CURATED_VIEWS[0].create(&Variant::SingleNode);
        assert!(spans.contains("lower(hex(trace_id)) AS trace_id"));
        assert!(spans.contains("lower(hex(span_id)) AS span_id"));
        let sessions = CURATED_VIEWS[3].create(&Variant::SingleNode);
        assert!(sessions.contains("sum(span_count) AS span_count"));
        assert!(sessions.contains("min(first_seen) AS first_seen"));
        assert!(sessions.contains("GROUP BY project_id, day, session_id"));
    }

    #[test]
    fn the_expected_inventory_names_every_policy() {
        let none = expected_policies(&[]);
        assert_eq!(none.len(), sql_tables().count() * 2);
        let one = expected_policies(&[project()]);
        assert_eq!(one.len(), sql_tables().count() * 4);
        let mut sorted = one.clone();
        sorted.sort();
        assert_eq!(one, sorted);
    }
}
