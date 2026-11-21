//! Migration steps as data (contract sections 6.6 and 7.3).
//!
//! The ledger order is fixed: the base schema, then access step A1 (service identities and one `USING 1` policy
//! per base table), then access step A2 (the SQL workspace), then the later schema steps in the order they were
//! added. A1 precedes every workspace step, so no workspace policy can exist on a table before the service policy
//! does. A later schema step that adds a base table must include [`crate::service_policy`] for it.
//!
//! The base schema always creates the tables as [`TABLES`] describes them now. A later schema step brings a
//! database that an older base schema created to the same shape, and changes nothing on a new one.

use crucible_records::{TableId, Uuid};

use crate::access::{AccessParams, ingest_grant, mask_grant, service_policy, service_statements};
use crate::ddl::{
    DATABASE, Granularity, Ident, SQL_DATABASE, Variant, create_database, create_table,
};
use crate::table::{PROJECT_ID, TableDef};
use crate::tables::{TABLES, table};

/// The column that [`schema_span_usage_aggregate`] adds.
const USAGE_AGGREGATE: &str = "usage_aggregate";

/// Schema step or access step.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum StepKind {
    /// Databases and tables (contract section 6).
    Schema,
    /// Identities, grants and row policies (contract section 7).
    Access,
}

/// The stable ledger id of the base schema step.
pub const SCHEMA_BASE: &str = "0001_schema_base";
/// The stable ledger id of access step A1.
pub const ACCESS_SERVICE: &str = "0002_access_service";
/// The stable ledger id of access step A2.
pub const ACCESS_WORKSPACE: &str = "0003_access_workspace";

/// The stable ledger id of the schema step that adds `span_core.usage_aggregate`.
pub const SCHEMA_SPAN_USAGE_AGGREGATE: &str = "0004_span_usage_aggregate";

/// The stable ledger id of the schema step that adds `mcp_tool_rollup_hourly` and copies the minute rollup
/// into it.
pub const SCHEMA_MCP_TOOL_ROLLUP_HOURLY: &str = "0005_mcp_tool_rollup_hourly";

/// Every step id in ledger order.
pub const LEDGER_ORDER: [(&str, StepKind); 5] = [
    (SCHEMA_BASE, StepKind::Schema),
    (ACCESS_SERVICE, StepKind::Access),
    (ACCESS_WORKSPACE, StepKind::Access),
    (SCHEMA_SPAN_USAGE_AGGREGATE, StepKind::Schema),
    (SCHEMA_MCP_TOOL_ROLLUP_HOURLY, StepKind::Schema),
];

/// The `marker_epoch` of the row of `mcp_tool_rollup_hourly` that records a finished copy of the minute rollup
/// ([`schema_mcp_tool_rollup_hourly`]). Like every marker row it has the nil project, so no read sees it. Its
/// time is the epoch, so it sits in partition `197001`, which holds no project's rows and which retention never
/// drops. No WAL has this epoch: a WAL epoch carries the time it was made.
pub const HOURLY_COPY_DONE: Uuid =
    Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);

/// The partition of the row that records a finished copy.
const COPY_DONE_PARTITION: &str = "197001";

/// The settings of [`schema_mcp_tool_rollup_hourly`] beyond the variant's DDL settings. The copy may run for a
/// while on a large minute rollup: the server ends it after an hour, and the client waits for that limit. It
/// spills its grouping to disk past 256 MiB and inserts in blocks of at most 262,144 rows, so it fits the 1.5 GB
/// that a small ClickHouse server is given. Content deduplication is off, so a copy that an operator started
/// again after an interrupted one (see the step) lands whole.
const COPY_SETTINGS: [(&str, &str); 5] = [
    ("max_execution_time", "3600"),
    ("max_bytes_before_external_group_by", "268435456"),
    ("min_insert_block_size_rows", "262144"),
    ("min_insert_block_size_bytes", "67108864"),
    ("insert_deduplicate", "0"),
];

/// One forward-only, idempotent migration step. `id` is the key of the Postgres `schema_migrations` ledger.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MigrationStep {
    /// The stable ledger id.
    pub id: &'static str,
    /// Schema or access.
    pub kind: StepKind,
    /// The statements, run in order as `crucible_admin`. Each is idempotent, so a step that failed part-way is
    /// simply applied again.
    pub statements: Vec<String>,
    /// Settings sent with every statement of the step.
    pub settings: &'static [(&'static str, &'static str)],
}

/// The base schema: both databases and every table.
pub fn schema_base(variant: &Variant, granularity: Granularity) -> MigrationStep {
    let mut statements = vec![
        create_database(DATABASE, variant),
        create_database(SQL_DATABASE, variant),
    ];
    statements.extend(
        TABLES
            .iter()
            .map(|def| create_table(def, variant, granularity)),
    );
    MigrationStep {
        id: SCHEMA_BASE,
        kind: StepKind::Schema,
        statements,
        settings: variant.ddl_settings(),
    }
}

/// Adds the column `usage_aggregate` to `span_core`, after `row_digest` (contracts 3.1 and 6.6): set on a span
/// with a descendant that carries usage or cost, which token and cost sums leave out. A migrated table and a new
/// one have the same shape. Rows written before the step read 0.
pub fn schema_span_usage_aggregate(variant: &Variant) -> MigrationStep {
    let def = table(TableId::SpanCore);
    let statements = def
        .columns
        .windows(2)
        .find(|pair| pair[1].name == USAGE_AGGREGATE)
        .map(|pair| {
            format!(
                "ALTER TABLE {DATABASE}.{}{} ADD COLUMN IF NOT EXISTS `{}` {} CODEC({}) AFTER `{}`",
                def.name(),
                variant.on_cluster(),
                pair[1].name,
                pair[1].ty,
                pair[1].codec,
                pair[0].name
            )
        })
        .into_iter()
        .collect();
    MigrationStep {
        id: SCHEMA_SPAN_USAGE_AGGREGATE,
        kind: StepKind::Schema,
        statements,
        settings: variant.ddl_settings(),
    }
}

/// The settings of the hourly step on a variant: the variant's DDL settings, then [`COPY_SETTINGS`].
fn hourly_step_settings(variant: &Variant) -> &'static [(&'static str, &'static str)] {
    const SINGLE: [(&str, &str); 5] = COPY_SETTINGS;
    const CLUSTERED: [(&str, &str); 8] = [
        ("alter_sync", "2"),
        ("wait_end_of_query", "1"),
        ("distributed_ddl_entry_format_version", "8"),
        COPY_SETTINGS[0],
        COPY_SETTINGS[1],
        COPY_SETTINGS[2],
        COPY_SETTINGS[3],
        COPY_SETTINGS[4],
    ];
    const CLOUD: [(&str, &str); 7] = [
        ("alter_sync", "2"),
        ("wait_end_of_query", "1"),
        COPY_SETTINGS[0],
        COPY_SETTINGS[1],
        COPY_SETTINGS[2],
        COPY_SETTINGS[3],
        COPY_SETTINGS[4],
    ];
    match variant {
        Variant::SingleNode => &SINGLE,
        Variant::Clustered { .. } => &CLUSTERED,
        Variant::Cloud => &CLOUD,
    }
}

/// `toUUID('...')` of a UUID literal.
fn uuid_sql(id: Uuid) -> String {
    format!("toUUID('{id}')")
}

/// The condition that the copy of the minute rollup into its hourly tier has not finished: the tier holds no row
/// of [`HOURLY_COPY_DONE`].
fn copy_unfinished(tier: &TableDef) -> String {
    format!(
        "(SELECT count() FROM {DATABASE}.{} WHERE {PROJECT_ID} = {} AND marker_epoch = {} AND _partition_id = \
         '{COPY_DONE_PARTITION}') = 0",
        tier.name(),
        uuid_sql(Uuid::NIL),
        uuid_sql(HOURLY_COPY_DONE)
    )
}

/// `INSERT INTO <tier> SELECT ... FROM <minute> GROUP BY ...`: every row of the minute rollup, merged per key with
/// its minute cut to the hour, so that each hourly row holds what the minute rows of its hour hold. Marker rows
/// (the nil project) keep their `marker_epoch` and `marker_batch_seq`, so the block of every batch whose minute
/// pair the copy took reads as present in the tier as well, and the shipper does not send that batch's hourly
/// rows a second time when it recovers the batch. Data rows get zero marker columns, as the shipper writes them.
/// Nothing is copied once the copy has finished.
fn copy_minute_rollup(minute: &TableDef, tier: &TableDef) -> String {
    let nil = uuid_sql(Uuid::NIL);
    let tier_time = tier.retention.time_column;
    let (mut select, mut group) = (Vec::new(), Vec::new());
    for (from, to) in minute.columns.iter().zip(tier.columns) {
        let key = match to.ty.agg_fn() {
            Some(f) => {
                select.push(format!("{f}({})", from.name));
                continue;
            }
            None => match to.name {
                name if name == tier_time => format!("toStartOfHour({})", from.name),
                "marker_epoch" => format!("if({PROJECT_ID} = {nil}, marker_epoch, {nil})"),
                "marker_batch_seq" => format!("if({PROJECT_ID} = {nil}, marker_batch_seq, 0)"),
                _ => {
                    select.push(from.name.to_owned());
                    group.push(from.name.to_owned());
                    continue;
                }
            },
        };
        // An expression is grouped by an alias that names no column of the minute rollup.
        let alias = format!("copied_{}", to.name);
        select.push(format!("{key} AS {alias}"));
        group.push(alias);
    }
    let columns: Vec<&str> = tier.columns.iter().map(|c| c.name).collect();
    format!(
        "INSERT INTO {DATABASE}.{} ({}) SELECT {} FROM {DATABASE}.{} WHERE {} GROUP BY {}",
        tier.name(),
        columns.join(", "),
        select.join(", "),
        minute.name(),
        copy_unfinished(tier),
        group.join(", ")
    )
}

/// Adds `mcp_tool_rollup_hourly`, the hourly tier of `mcp_tool_rollup` (contract 3.10), and copies the minute
/// rollup into it once:
///
/// 1. the table, the grants of `crucible_ingest` and `crucible_maint` on it, and its service policy (contract
///    7.3), each idempotent;
/// 2. a refusal when the tier holds rows but no record of a finished copy: an earlier copy stopped part-way, and
///    the message says how to start it again;
/// 3. the copy ([`copy_minute_rollup`]), unless it has finished;
/// 4. the row that records the finished copy ([`HOURLY_COPY_DONE`]), unless it is there.
///
/// Steps 3 and 4 do nothing once the copy has finished, so the step can run on every start, as it does without
/// Postgres. The shipper writes both tiers of every batch from this version on, and the step runs before the
/// shipper starts, so every operation reaches the tier once: through the copy when its minute row was stored
/// before the step, and through the shipper otherwise. Operations that a server of an earlier version ships after
/// the step reach the minute rollup only.
pub fn schema_mcp_tool_rollup_hourly(variant: &Variant, admin_user: &Ident) -> MigrationStep {
    let minute = table(TableId::McpToolRollup);
    let tier = table(TableId::McpToolRollupHourly);
    let on = variant.on_cluster();
    let unfinished = copy_unfinished(tier);
    let refuse = format!(
        "SELECT throwIf((SELECT count() FROM {DATABASE}.{name}) > 0 AND {unfinished}, 'The copy of \
         {DATABASE}.{minute} into {DATABASE}.{name} stopped before it finished, so the hourly table is \
         incomplete. Stop every Crucible server, run TRUNCATE TABLE {DATABASE}.{name} as the ClickHouse admin \
         user, then migrate again (crucible migrate, or crucible serve --migrate).')",
        name = tier.name(),
        minute = minute.name(),
    );
    let done = format!(
        "INSERT INTO {DATABASE}.{} ({PROJECT_ID}, marker_epoch, marker_at) SELECT {}, {}, now() WHERE {unfinished}",
        tier.name(),
        uuid_sql(Uuid::NIL),
        uuid_sql(HOURLY_COPY_DONE)
    );
    MigrationStep {
        id: SCHEMA_MCP_TOOL_ROLLUP_HOURLY,
        kind: StepKind::Schema,
        statements: vec![
            // The tier's partition is monthly, so the granularity option changes nothing.
            create_table(tier, variant, Granularity::Day),
            ingest_grant(tier, &on),
            mask_grant(tier, &on),
            service_policy(tier.id, variant, admin_user),
            refuse,
            copy_minute_rollup(minute, tier),
            done,
        ],
        settings: hourly_step_settings(variant),
    }
}

/// Access step A1: the four generated service identities with their profiles and quota, their grants, and the
/// service policy of every base table.
pub fn access_service(variant: &Variant, params: &AccessParams) -> MigrationStep {
    MigrationStep {
        id: ACCESS_SERVICE,
        kind: StepKind::Access,
        statements: service_statements(variant, params),
        settings: variant.ddl_settings(),
    }
}

/// Permissive or restrictive: every workspace predicate is created in both forms (contract section 7.4).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PolicyForm {
    /// `AS PERMISSIVE`.
    Permissive,
    /// `AS RESTRICTIVE`.
    Restrictive,
}

/// One action of access step A2. This is the step's shape only: the SQL workspace milestone renders the
/// statements.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum WorkspaceAction {
    /// Verify with [`crate::service_policy_check`] that every base table has its service policy. Always first.
    VerifyServicePolicies,
    /// Create the role `crucible_sql_reader`.
    CreateRole,
    /// Create the settings profile `crucible_sql_profile`.
    CreateProfile,
    /// Create the quota `crucible_sql_quota`.
    CreateQuota,
    /// Grant the role column-level `SELECT` on a table's SQL-readable columns.
    GrantColumns(TableId),
    /// Create the static, role-wide policy on a table in one form.
    StaticPolicy(TableId, PolicyForm),
    /// Create the curated views in `crucible_sql` and grant the role `SELECT` on that database.
    CuratedViews,
    /// Provision the two probe identities for the reserved probe project ids.
    ProbeIdentities,
}

/// The ordered actions of access step A2.
pub fn access_workspace_shape() -> Vec<WorkspaceAction> {
    let readable = || {
        TABLES
            .iter()
            .filter(|def| def.sql_readable)
            .map(|def| def.id)
    };
    let mut actions = vec![
        WorkspaceAction::VerifyServicePolicies,
        WorkspaceAction::CreateRole,
        WorkspaceAction::CreateProfile,
        WorkspaceAction::CreateQuota,
    ];
    actions.extend(readable().map(WorkspaceAction::GrantColumns));
    for form in [PolicyForm::Permissive, PolicyForm::Restrictive] {
        actions.extend(readable().map(|id| WorkspaceAction::StaticPolicy(id, form)));
    }
    actions.push(WorkspaceAction::CuratedViews);
    actions.push(WorkspaceAction::ProbeIdentities);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{PasswordHash, ProfileCaps, ServiceIdentity, service_policy_name};
    use crate::ddl::Ident;

    fn params() -> AccessParams {
        let hash = |c: &str| PasswordHash::new(&c.repeat(64)).unwrap();
        let caps = ProfileCaps {
            max_execution_time_secs: 300,
            max_memory_usage: 8 << 30,
            max_rows_to_read: None,
            max_threads: 8,
        };
        AccessParams {
            admin_user: Ident::new("crucible_admin").unwrap(),
            ingest: hash("1"),
            read_ui: hash("2"),
            read_agent: hash("3"),
            maint: hash("4"),
            read_ui_caps: caps,
            read_agent_caps: ProfileCaps {
                max_rows_to_read: Some(100_000_000),
                ..caps
            },
        }
    }

    fn variants() -> [Variant; 3] {
        [
            Variant::SingleNode,
            Variant::Clustered {
                cluster: Ident::new("crucible").unwrap(),
            },
            Variant::Cloud,
        ]
    }

    #[test]
    fn the_service_step_precedes_every_workspace_step() {
        let ids: Vec<&str> = LEDGER_ORDER.iter().map(|(id, _)| *id).collect();
        assert_eq!(
            ids,
            [
                SCHEMA_BASE,
                ACCESS_SERVICE,
                ACCESS_WORKSPACE,
                SCHEMA_SPAN_USAGE_AGGREGATE,
                SCHEMA_MCP_TOOL_ROLLUP_HOURLY,
            ]
        );
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, ids, "ids sort in ledger order");
        assert_eq!(
            access_workspace_shape().first(),
            Some(&WorkspaceAction::VerifyServicePolicies)
        );
        for variant in variants() {
            assert_eq!(schema_base(&variant, Granularity::Day).id, SCHEMA_BASE);
            let a1 = access_service(&variant, &params());
            assert_eq!((a1.id, a1.kind), (ACCESS_SERVICE, StepKind::Access));
        }
    }

    #[test]
    fn every_statement_is_idempotent() {
        for variant in variants() {
            let schema = schema_base(&variant, Granularity::Week);
            assert_eq!(schema.statements.len(), 2 + TABLES.len());
            let access = access_service(&variant, &params());
            let column = schema_span_usage_aggregate(&variant);
            assert_eq!(column.statements.len(), 1);
            let hourly = schema_mcp_tool_rollup_hourly(&variant, &params().admin_user);
            // The last three statements of the hourly step are the guarded copy (see its own test).
            let (hourly_ddl, _) = hourly.statements.split_at(4);
            for statement in schema
                .statements
                .iter()
                .chain(&access.statements)
                .chain(&column.statements)
                .chain(hourly_ddl)
            {
                assert!(
                    statement.starts_with("GRANT") || statement.contains(" IF NOT EXISTS "),
                    "{statement}"
                );
                assert!(!statement.contains(';'));
                let clustered = matches!(variant, Variant::Clustered { .. });
                assert_eq!(statement.contains(" ON CLUSTER crucible"), clustered);
            }
        }
    }

    /// The step adds the column after `row_digest`, with the type and codec the table definition gives it, so a
    /// table the step migrated has the shape of a table the base schema creates.
    #[test]
    fn the_usage_aggregate_step_adds_the_column_after_the_row_digest() {
        let def = table(TableId::SpanCore);
        let at = def
            .columns
            .iter()
            .position(|c| c.name == "usage_aggregate")
            .unwrap();
        assert_eq!(def.columns[at - 1].name, "row_digest");
        let column = &def.columns[at];
        let step = schema_span_usage_aggregate(&Variant::SingleNode);
        assert_eq!(
            (step.id, step.kind),
            ("0004_span_usage_aggregate", StepKind::Schema)
        );
        assert_eq!(
            step.statements,
            [format!(
                "ALTER TABLE crucible.span_core ADD COLUMN IF NOT EXISTS `usage_aggregate` {} CODEC({}) AFTER `row_digest`",
                column.ty, column.codec
            )]
        );
        let base = schema_base(&Variant::SingleNode, Granularity::Day);
        assert!(
            base.statements
                .iter()
                .any(|s| s.contains("`usage_aggregate`"))
        );
    }

    /// The hourly step creates the tier with its grants and service policy, then copies the minute rollup once:
    /// every data statement is guarded by the record of a finished copy, so the step changes nothing when it runs
    /// again, and a copy that stopped part-way is refused with the fix.
    #[test]
    fn the_hourly_step_copies_the_minute_rollup_once() {
        let admin = params().admin_user;
        for variant in variants() {
            let step = schema_mcp_tool_rollup_hourly(&variant, &admin);
            assert_eq!(
                (step.id, step.kind),
                ("0005_mcp_tool_rollup_hourly", StepKind::Schema)
            );
            assert!(step.settings.starts_with(variant.ddl_settings()));
            assert!(step.settings.ends_with(&COPY_SETTINGS));
            let tier = table(TableId::McpToolRollupHourly);
            let [create, ingest, maint, policy, refuse, copy, done] = step.statements.as_slice()
            else {
                panic!("{:#?}", step.statements);
            };
            assert_eq!(*create, create_table(tier, &variant, Granularity::Week));
            assert_eq!(*create, create_table(tier, &variant, Granularity::Day));
            assert!(ingest.starts_with("GRANT") && ingest.ends_with("TO crucible_ingest"));
            assert!(maint.contains("ALTER UPDATE(_row_exists) ON crucible.mcp_tool_rollup_hourly"));
            assert_eq!(*policy, service_policy(tier.id, &variant, &admin));
            let unfinished = copy_unfinished(tier);
            assert!(refuse.starts_with("SELECT throwIf(") && refuse.contains("TRUNCATE TABLE"));
            for guarded in [refuse, copy, done] {
                assert!(guarded.contains(&unfinished), "{guarded}");
                assert!(!guarded.contains(" ON CLUSTER "));
            }
            let columns: Vec<&str> = tier.columns.iter().map(|c| c.name).collect();
            let insert = format!(
                "INSERT INTO crucible.mcp_tool_rollup_hourly ({}) SELECT ",
                columns.join(", ")
            );
            assert!(copy.starts_with(&insert), "{copy}");
            for expected in [
                "toStartOfHour(minute) AS copied_hour",
                "sum(count)",
                "sumMap(dur_hist)",
                "sumMap(semconv_hist)",
                "max(last_seen)",
                "max(marker_at)",
                " FROM crucible.mcp_tool_rollup WHERE ",
                "GROUP BY project_id, side, copied_hour, server_name, deploy_id, capture_source, era, method, \
                 target_name, client_name, error_class, caller_fault, copied_marker_epoch, copied_marker_batch_seq",
            ] {
                assert!(copy.contains(expected), "{expected} in {copy}");
            }
            assert!(done.contains("SELECT toUUID('00000000-0000-0000-0000-000000000000'), toUUID('00000000-0000-7000-8000-000000000001'), now()"));
        }
        assert_eq!(
            HOURLY_COPY_DONE.to_string(),
            "00000000-0000-7000-8000-000000000001"
        );
    }

    #[test]
    fn a1_creates_identities_before_policies_and_covers_every_table() {
        let a1 = access_service(&Variant::SingleNode, &params());
        let position = |needle: &str| {
            a1.statements
                .iter()
                .position(|s| s.starts_with(needle))
                .unwrap_or_else(|| panic!("{needle}"))
        };
        let first_policy = position("CREATE ROW POLICY");
        for identity in ServiceIdentity::GENERATED {
            let user = position(&format!("CREATE USER IF NOT EXISTS {} ", identity.name()));
            let profile = position(&format!(
                "CREATE SETTINGS PROFILE IF NOT EXISTS {}",
                identity.profile().unwrap()
            ));
            assert!(profile < user && user < first_policy);
        }
        assert!(
            !a1.statements
                .iter()
                .any(|s| s.contains("USER IF NOT EXISTS crucible_admin"))
        );
        for def in &TABLES {
            let policy = format!(
                "CREATE ROW POLICY IF NOT EXISTS {} ON crucible.{} AS PERMISSIVE FOR SELECT USING 1 TO \
                 crucible_admin, crucible_ingest, crucible_read_ui, crucible_read_agent, crucible_maint",
                service_policy_name(def.id),
                def.name()
            );
            assert!(a1.statements.contains(&policy), "{policy}");
        }
        let policies = a1
            .statements
            .iter()
            .filter(|s| s.starts_with("CREATE ROW POLICY"))
            .count();
        assert_eq!(policies, TABLES.len());
    }

    #[test]
    fn ingest_reads_probe_columns_only_and_never_touches_the_sql_probe() {
        let a1 = access_service(&Variant::SingleNode, &params());
        let ingest: Vec<&String> = a1
            .statements
            .iter()
            .filter(|s| s.ends_with(" TO crucible_ingest"))
            .collect();
        assert_eq!(ingest.len(), TABLES.len() - 1);
        assert!(ingest.iter().all(|s| !s.contains("crucible.sql_probe ")));
        assert!(ingest.contains(&&"GRANT INSERT, SELECT(project_id, wal_epoch, batch_seq, _partition_id) ON crucible.span_core TO crucible_ingest".to_owned()));
        assert!(ingest.contains(&&"GRANT INSERT, SELECT(project_id, marker_epoch, marker_batch_seq, _partition_id) ON crucible.trace_stats TO crucible_ingest".to_owned()));
        assert!(ingest.contains(&&"GRANT INSERT, SELECT(id, wal_epoch, batch_seq) ON crucible.dedup_selftest TO crucible_ingest".to_owned()));
    }

    #[test]
    fn the_workspace_shape_covers_every_sql_readable_table_in_both_forms() {
        let shape = access_workspace_shape();
        for def in &TABLES {
            let readable = def.sql_readable;
            assert_eq!(
                readable,
                !matches!(
                    def.id,
                    TableId::DedupSelftest | TableId::McpToolRollupHourly
                )
            );
            assert_eq!(
                shape.contains(&WorkspaceAction::GrantColumns(def.id)),
                readable
            );
            for form in [PolicyForm::Permissive, PolicyForm::Restrictive] {
                assert_eq!(
                    shape.contains(&WorkspaceAction::StaticPolicy(def.id, form)),
                    readable
                );
            }
        }
    }
}
