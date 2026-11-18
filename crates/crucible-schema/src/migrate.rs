//! Migration steps as data (contract sections 6.6 and 7.3).
//!
//! The ledger order is fixed: the base schema, then access step A1 (service identities and one `USING 1` policy
//! per base table), then access step A2 (the SQL workspace), then the later schema steps in the order they were
//! added. A1 precedes every workspace step, so no workspace policy can exist on a table before the service policy
//! does. A later schema step that adds a base table must include [`crate::service_policy`] for it.
//!
//! The base schema always creates the tables as [`TABLES`] describes them now. A later schema step brings a
//! database that an older base schema created to the same shape, and changes nothing on a new one.

use crucible_records::TableId;

use crate::access::{AccessParams, service_statements};
use crate::ddl::{DATABASE, Granularity, SQL_DATABASE, Variant, create_database, create_table};
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
pub const SCHEMA_SPAN_USAGE_AGGREGATE: &str = "0004_schema_span_usage_aggregate";

/// Every step id in ledger order.
pub const LEDGER_ORDER: [(&str, StepKind); 4] = [
    (SCHEMA_BASE, StepKind::Schema),
    (ACCESS_SERVICE, StepKind::Access),
    (ACCESS_WORKSPACE, StepKind::Access),
    (SCHEMA_SPAN_USAGE_AGGREGATE, StepKind::Schema),
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

/// The base schema: both databases and the 18 tables.
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

/// Adds the column `usage_aggregate` to `span_core` (contract 3.1): set on a span that repeats usage a descendant
/// of it also carries, which token and cost sums leave out. The column is the last of the table, where
/// `ADD COLUMN` puts it, so a migrated table and a new one have the same shape. Rows written before the step
/// read 0.
pub fn schema_span_usage_aggregate(variant: &Variant) -> MigrationStep {
    let def = table(TableId::SpanCore);
    let statements = def
        .columns
        .iter()
        .find(|c| c.name == USAGE_AGGREGATE)
        .map(|c| {
            format!(
                "ALTER TABLE {DATABASE}.{}{} ADD COLUMN IF NOT EXISTS `{}` {} CODEC({})",
                def.name(),
                variant.on_cluster(),
                c.name,
                c.ty,
                c.codec
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
                SCHEMA_SPAN_USAGE_AGGREGATE
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
            for statement in schema
                .statements
                .iter()
                .chain(&access.statements)
                .chain(&column.statements)
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

    /// The step adds the last column of the table, with the type and codec the table definition gives it, so a
    /// table the step migrated has the shape of a table the base schema creates.
    #[test]
    fn the_usage_aggregate_step_adds_the_last_column_of_span_core() {
        let def = table(TableId::SpanCore);
        let last = def.columns.last().unwrap();
        assert_eq!(last.name, "usage_aggregate");
        let step = schema_span_usage_aggregate(&Variant::SingleNode);
        assert_eq!(
            (step.id, step.kind),
            (SCHEMA_SPAN_USAGE_AGGREGATE, StepKind::Schema)
        );
        assert_eq!(
            step.statements,
            [format!(
                "ALTER TABLE crucible.span_core ADD COLUMN IF NOT EXISTS `usage_aggregate` {} CODEC({})",
                last.ty, last.codec
            )]
        );
        let base = schema_base(&Variant::SingleNode, Granularity::Day);
        assert!(
            base.statements
                .iter()
                .any(|s| s.contains("`usage_aggregate`"))
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
            let readable = def.id != TableId::DedupSelftest;
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
