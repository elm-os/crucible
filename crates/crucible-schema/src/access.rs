//! ClickHouse identities and the statements of access step A1 (contract sections 7.1 to 7.3).

use core::fmt;
use std::borrow::Cow;

use crucible_records::TableId;

use crate::ddl::{DATABASE, Ident, Variant};
use crate::table::{PROJECT_ID, TableDef, TableKind};
use crate::tables::TABLES;

/// A service identity of contract section 7.1.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum ServiceIdentity {
    /// `crucible_admin`: supplied by the operator; runs migrations and access management.
    Admin,
    /// `crucible_ingest`: inserts, the existence probe and the deduplication self-test.
    Ingest,
    /// `crucible_read_ui`: typed plans for session-authenticated requests, alerts, exports and jobs.
    ReadUi,
    /// `crucible_read_agent`: typed plans for API-key principals and the MCP server.
    ReadAgent,
    /// `crucible_maint`: deletes, partition drops, mask application, detection, kill and health.
    Maint,
}

impl ServiceIdentity {
    /// The five service identities.
    pub const ALL: [Self; 5] = [
        Self::Admin,
        Self::Ingest,
        Self::ReadUi,
        Self::ReadAgent,
        Self::Maint,
    ];

    /// The four identities that access step A1 creates.
    pub const GENERATED: [Self; 4] = [Self::Ingest, Self::ReadUi, Self::ReadAgent, Self::Maint];

    /// The user name. The operator may name the admin user differently (see [`AccessParams::admin_user`]).
    pub const fn name(self) -> &'static str {
        match self {
            Self::Admin => "crucible_admin",
            Self::Ingest => "crucible_ingest",
            Self::ReadUi => "crucible_read_ui",
            Self::ReadAgent => "crucible_read_agent",
            Self::Maint => "crucible_maint",
        }
    }

    /// The settings profile, for a generated identity.
    pub const fn profile(self) -> Option<&'static str> {
        match self {
            Self::Admin => None,
            Self::Ingest => Some("crucible_ingest_profile"),
            Self::ReadUi => Some("crucible_read_ui_profile"),
            Self::ReadAgent => Some("crucible_read_agent_profile"),
            Self::Maint => Some("crucible_maint_profile"),
        }
    }
}

/// The hourly quota of `crucible_read_agent`.
pub const AGENT_QUOTA: &str = "crucible_agent_quota";
/// Queries per hour allowed by [`AGENT_QUOTA`] (default; revisit).
pub const AGENT_QUOTA_QUERIES_PER_HOUR: u32 = 100_000;
/// Execution seconds per hour allowed by [`AGENT_QUOTA`] (default; revisit).
pub const AGENT_QUOTA_EXECUTION_SECONDS_PER_HOUR: u32 = 7_200;
/// `max_concurrent_queries_for_user` of `crucible_read_agent` (default; revisit).
pub const AGENT_MAX_CONCURRENT_QUERIES: u32 = 16;
/// The ceiling on `max_execution_time` for `crucible_ingest`, in seconds.
pub const INGEST_MAX_EXECUTION_SECONDS: u32 = 60;

/// The system tables `crucible_maint` reads. Startup detection reads the engine families from `tables` and the
/// clustered server setting from `server_settings`.
pub const MAINT_SYSTEM_TABLES: [&str; 8] = [
    "mutations",
    "parts",
    "settings",
    "server_settings",
    "merge_tree_settings",
    "functions",
    "processes",
    "tables",
];

/// A password hash that is not 64 lowercase hex characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a password hash is 64 lowercase hex characters")]
pub struct InvalidPasswordHash;

/// The SHA-256 of an identity's password as lowercase hex. Passwords are derived from the master key at apply
/// time (contract section 7.1); only this digest ever appears in a statement.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct PasswordHash(String);

impl PasswordHash {
    /// Validates a digest.
    pub fn new(hex: &str) -> Result<Self, InvalidPasswordHash> {
        let ok = hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok {
            Ok(Self(hex.to_owned()))
        } else {
            Err(InvalidPasswordHash)
        }
    }

    /// The digest. It appears in `CREATE USER` and `ALTER USER` statements only.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for PasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordHash(..)")
    }
}

/// What precedes a password digest in a `CREATE USER` or `ALTER USER` statement.
const DIGEST_CLAUSE: &str = "IDENTIFIED WITH sha256_hash BY '";

/// The statement with every password digest replaced by `<digest>`; a statement without one comes back as it is.
/// The digests are derived from the master key, so the migrations ledger takes its checksums over this form
/// (contract 6.6): a start with another master key finds every step that a start with the right key recorded.
pub fn without_secrets(statement: &str) -> Cow<'_, str> {
    if !statement.contains(DIGEST_CLAUSE) {
        return Cow::Borrowed(statement);
    }
    let mut out = String::with_capacity(statement.len());
    let mut rest = statement;
    while let Some(at) = rest.find(DIGEST_CLAUSE) {
        let (head, tail) = rest.split_at(at + DIGEST_CLAUSE.len());
        out.push_str(head);
        out.push_str("<digest>");
        // A digest is hex, so the next quote closes it.
        rest = tail.find('\'').map_or("", |end| &tail[end..]);
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// The ceilings a read profile places on per-query settings: the largest values of the query classes that use
/// the identity (contract sections 7.1 and 8.4). `crucible-plan` derives them from its class table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ProfileCaps {
    /// Ceiling on `max_execution_time`, in seconds.
    pub max_execution_time_secs: u32,
    /// Ceiling on `max_memory_usage`, in bytes.
    pub max_memory_usage: u64,
    /// Ceiling on `max_rows_to_read`; `None` when a class of the identity reads without a row limit.
    pub max_rows_to_read: Option<u64>,
    /// Ceiling on `max_threads`.
    pub max_threads: u32,
}

/// The inputs of access step A1 that are not schema data.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AccessParams {
    /// The operator-supplied admin user that runs migrations.
    pub admin_user: Ident,
    /// Password digest of `crucible_ingest`.
    pub ingest: PasswordHash,
    /// Password digest of `crucible_read_ui`.
    pub read_ui: PasswordHash,
    /// Password digest of `crucible_read_agent`.
    pub read_agent: PasswordHash,
    /// Password digest of `crucible_maint`.
    pub maint: PasswordHash,
    /// Setting ceilings of `crucible_read_ui_profile`.
    pub read_ui_caps: ProfileCaps,
    /// Setting ceilings of `crucible_read_agent_profile`.
    pub read_agent_caps: ProfileCaps,
}

impl AccessParams {
    fn hash(&self, identity: ServiceIdentity) -> Option<&PasswordHash> {
        match identity {
            ServiceIdentity::Admin => None,
            ServiceIdentity::Ingest => Some(&self.ingest),
            ServiceIdentity::ReadUi => Some(&self.read_ui),
            ServiceIdentity::ReadAgent => Some(&self.read_agent),
            ServiceIdentity::Maint => Some(&self.maint),
        }
    }
}

/// The name of a base table's service-identity row policy.
pub fn service_policy_name(table: TableId) -> String {
    format!("crucible_service_{}", table.name())
}

/// The `USING 1` permissive policy that lets every service identity read every row of a base table (contract
/// section 7.3). Every schema step that creates a base table after A1 must run this in the same step.
pub fn service_policy(table: TableId, variant: &Variant, admin_user: &Ident) -> String {
    format!(
        "CREATE ROW POLICY IF NOT EXISTS {}{} ON {DATABASE}.{} AS PERMISSIVE FOR SELECT USING 1 TO {admin_user}, {}",
        service_policy_name(table),
        variant.on_cluster(),
        table.name(),
        ServiceIdentity::GENERATED
            .map(ServiceIdentity::name)
            .join(", ")
    )
}

/// A query that counts the base tables whose service policy exists and is addressed to all five service
/// identities. Access step A2 and the reconciler must see `TABLES.len()` before creating a workspace policy.
pub fn service_policy_check(admin_user: &Ident) -> String {
    let users = core::iter::once(admin_user.as_str())
        .chain(ServiceIdentity::GENERATED.map(ServiceIdentity::name))
        .map(|u| format!("'{u}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT count() FROM system.row_policies WHERE database = '{DATABASE}' \
         AND short_name = concat('crucible_service_', table) AND is_restrictive = 0 \
         AND select_filter = '1' AND hasAll(apply_to_list, [{users}])"
    )
}

fn caps_settings(caps: &ProfileCaps) -> String {
    let mut s = format!(
        "readonly = 2, max_execution_time = {t} MIN 1 MAX {t}, max_memory_usage = {m} MIN 1 MAX {m}",
        t = caps.max_execution_time_secs,
        m = caps.max_memory_usage
    );
    if let Some(rows) = caps.max_rows_to_read {
        s.push_str(&format!(", max_rows_to_read = {rows} MIN 1 MAX {rows}"));
    }
    s.push_str(&format!(
        ", max_threads = {n} MIN 1 MAX {n}",
        n = caps.max_threads
    ));
    s
}

/// The columns `crucible_ingest` may read on a table: the existence-probe columns only (contract section 7.1),
/// plus the `_partition_id` virtual column the probe filters on, which ClickHouse checks like a real column.
fn probe_columns(def: &TableDef) -> Vec<&'static str> {
    if !def.is_scoped() {
        return vec!["id", "wal_epoch", "batch_seq"];
    }
    match def.kind {
        TableKind::Plain => vec![PROJECT_ID, "wal_epoch", "batch_seq", "_partition_id"],
        TableKind::Aggregating => {
            vec![
                PROJECT_ID,
                "marker_epoch",
                "marker_batch_seq",
                "_partition_id",
            ]
        }
    }
}

/// `crucible_ingest`'s grant on a table: insert, and read the existence-probe columns (contract section 7.1).
/// Access step A1 grants it on every table the shipper writes; a later schema step that adds a table grants it
/// on that table. `on` is the variant's `ON CLUSTER` clause.
pub(crate) fn ingest_grant(def: &TableDef, on: &str) -> String {
    format!(
        "GRANT{on} INSERT, SELECT({}) ON {DATABASE}.{} TO {}",
        probe_columns(def).join(", "),
        def.name(),
        ServiceIdentity::Ingest.name()
    )
}

/// `crucible_maint`'s grant to mask a table's rows: a lightweight `DELETE` needs `ALTER UPDATE` on the
/// `_row_exists` column, which is a per-table column grant. `ALTER DELETE` on the whole database covers the rest.
pub(crate) fn mask_grant(def: &TableDef, on: &str) -> String {
    format!(
        "GRANT{on} ALTER UPDATE(_row_exists) ON {DATABASE}.{} TO {}",
        def.name(),
        ServiceIdentity::Maint.name()
    )
}

/// The statements of access step A1, in order: profiles, users, the agent quota, grants, then one service
/// policy per base table. Every statement is idempotent.
pub(crate) fn service_statements(variant: &Variant, params: &AccessParams) -> Vec<String> {
    let on = variant.on_cluster();
    let mut out = Vec::new();

    let profile_settings = |identity: ServiceIdentity| match identity {
        ServiceIdentity::Admin => None,
        ServiceIdentity::Ingest => Some(format!(
            " SETTINGS max_execution_time = {INGEST_MAX_EXECUTION_SECONDS} MIN 1 MAX {INGEST_MAX_EXECUTION_SECONDS}"
        )),
        ServiceIdentity::ReadUi => {
            Some(format!(" SETTINGS {}", caps_settings(&params.read_ui_caps)))
        }
        ServiceIdentity::ReadAgent => Some(format!(
            " SETTINGS {}, max_concurrent_queries_for_user = {AGENT_MAX_CONCURRENT_QUERIES} CONST",
            caps_settings(&params.read_agent_caps)
        )),
        ServiceIdentity::Maint => Some(String::new()),
    };
    for identity in ServiceIdentity::GENERATED {
        if let (Some(profile), Some(settings)) = (identity.profile(), profile_settings(identity)) {
            out.push(format!(
                "CREATE SETTINGS PROFILE IF NOT EXISTS {profile}{on}{settings}"
            ));
        }
    }
    for identity in ServiceIdentity::GENERATED {
        if let (Some(profile), Some(hash)) = (identity.profile(), params.hash(identity)) {
            out.push(format!(
                "CREATE USER IF NOT EXISTS {}{on} IDENTIFIED WITH sha256_hash BY '{}' SETTINGS PROFILE '{profile}'",
                identity.name(),
                hash.0
            ));
        }
    }
    out.push(format!(
        "CREATE QUOTA IF NOT EXISTS {AGENT_QUOTA}{on} FOR INTERVAL 1 hour MAX queries = {AGENT_QUOTA_QUERIES_PER_HOUR}, execution_time = {AGENT_QUOTA_EXECUTION_SECONDS_PER_HOUR} TO {}",
        ServiceIdentity::ReadAgent.name()
    ));

    for def in &TABLES {
        if def.id != TableId::SqlProbe {
            out.push(ingest_grant(def, &on));
        }
    }
    for reader in [ServiceIdentity::ReadUi, ServiceIdentity::ReadAgent] {
        out.push(format!(
            "GRANT{on} SELECT ON {DATABASE}.* TO {}",
            reader.name()
        ));
    }
    // ALTER DELETE covers mutations, DROP PARTITION and APPLY DELETED MASK; a lightweight DELETE also needs
    // ALTER UPDATE on the `_row_exists` mask column, which is a per-table column grant.
    let maint = ServiceIdentity::Maint.name();
    out.push(format!(
        "GRANT{on} SELECT, ALTER DELETE ON {DATABASE}.* TO {maint}"
    ));
    for def in &TABLES {
        out.push(mask_grant(def, &on));
    }
    out.push(format!("GRANT{on} KILL QUERY ON *.* TO {maint}"));
    for system_table in MAINT_SYSTEM_TABLES {
        out.push(format!(
            "GRANT{on} SELECT ON system.{system_table} TO {maint}"
        ));
    }

    for def in &TABLES {
        out.push(service_policy(def.id, variant, &params.admin_user));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hashes_are_validated_and_redacted() {
        let hex = "a".repeat(64);
        let hash = PasswordHash::new(&hex).unwrap();
        assert_eq!(format!("{hash:?}"), "PasswordHash(..)");
        for bad in ["", &"a".repeat(63), &"A".repeat(64), &"g".repeat(64)] {
            assert_eq!(PasswordHash::new(bad), Err(InvalidPasswordHash));
        }
        let quote = format!("{}'", "a".repeat(63));
        assert!(PasswordHash::new(&quote).is_err());
    }

    #[test]
    fn identity_names_and_profiles() {
        assert_eq!(ServiceIdentity::ALL.len(), 5);
        assert!(!ServiceIdentity::GENERATED.contains(&ServiceIdentity::Admin));
        for identity in ServiceIdentity::GENERATED {
            assert_eq!(
                identity.profile(),
                Some(format!("{}_profile", identity.name()).as_str())
            );
        }
        assert_eq!(ServiceIdentity::Admin.profile(), None);
    }
}
