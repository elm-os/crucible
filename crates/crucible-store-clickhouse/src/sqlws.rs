//! [`ClickHouseSqlStore`]: the `SqlWorkspaceStore` over the ClickHouse HTTP interface (contracts 2.9 and 7.4).
//!
//! A statement of a project runs as that project's own ClickHouse user, `crucible_sql_<project hex>`, whose
//! name is computed here from the scope and whose password is derived by [`SqlPasswords`]. The user's text is
//! the whole statement; the request carries besides it only the query id, the output format and the `param_*`
//! values. Nothing in this file decides what a project may read: the server's row policies, grants and
//! settings profile do.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use crucible_env::{
    CELL_CUT_MARKER, ProbeBreach, ProbeReport, SqlCell, SqlColumn, SqlError, SqlEstimate,
    SqlExportFormat, SqlGrid, SqlLimits, SqlParam, SqlQuery, SqlStats, SqlWorkspaceStore,
    StoreError,
};
use crucible_records::{ProjectId, ProjectScope, Uuid};
use crucible_schema::{
    Ident, JOIN_SPILL_SETTINGS, PROBE_PROJECTS, PasswordHash, RetentionSettings, ServiceIdentity,
    SqlProfile, TABLES, Variant, WorkspaceParams, alter_quota, deprovision_identity, probe_canary,
    provision_identity, service_policy_check, workspace_user,
};
use serde::de::{DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};

use crate::codes::Failure;
use crate::render::{Binder, escape};
use crate::store::{CHANGE_DEADLINE, ClickHouseStore, LOOKUP_SECS, Reading};

/// The largest answer body the store buffers. The profile's `max_result_bytes` stops the server well before it.
const BODY_CAP: usize = 64 << 20;
/// The Rust-side deadline of an `EXPLAIN` and of one probe statement.
const SHORT_DEADLINE: Duration = Duration::from_secs(15);

/// Derives the password of a workspace identity (contract 7.1): HKDF-SHA256 of the master key with the user name
/// and the generation. The binary owns the master key, so the derivation lives there.
pub trait SqlPasswords: fmt::Debug + Send + Sync {
    /// The password of `user` at `generation`.
    fn password(&self, user: &str, generation: u32) -> String;
}

/// What the workspace store needs besides the service store.
#[derive(Clone, Debug)]
pub struct SqlStoreConfig {
    /// The password derivation.
    pub passwords: Arc<dyn SqlPasswords>,
    /// The deployment variant, which decides `ON CLUSTER` and the DDL settings.
    pub variant: Variant,
    /// The operator-supplied admin user, named by the service policies.
    pub admin_user: Ident,
    /// The statements an identity may send per hour (`sql.statements_per_hour`).
    pub statements_per_hour: u64,
}

/// The production `SqlWorkspaceStore`, over the connection pool of a [`ClickHouseStore`].
#[derive(Clone, Debug)]
pub struct ClickHouseSqlStore {
    store: ClickHouseStore,
    config: SqlStoreConfig,
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The place of a syntax error: `(line N, col M)`, which the server writes for a text of several lines, or
/// `failed at position P` on the one line of a single-line text.
fn position(message: &str) -> (Option<u32>, Option<u32>) {
    let number = |text: &str| -> Option<u32> {
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        text[..digits].parse().ok()
    };
    let Some(start) = message.find("(line ") else {
        let marker = "failed at position ";
        let column = message
            .find(marker)
            .and_then(|at| number(&message[at + marker.len()..]));
        return (column.map(|_| 1), column);
    };
    let rest = &message[start + "(line ".len()..];
    let line = number(rest);
    let column = rest
        .find(", col ")
        .and_then(|at| number(&rest[at + ", col ".len()..]));
    (line, column)
}

/// The server's description without the `Code: N. DB::Exception: ` prefix and the version suffix.
fn description(message: &str) -> String {
    let text = message
        .split_once("DB::Exception: ")
        .map_or(message, |(_, rest)| rest);
    let text = text.split(" (version ").next().unwrap_or(text);
    text.trim().to_owned()
}

/// The class of a statement the server refused, by its code. A code that is not listed is the query's own
/// fault (`sql_invalid`), because that is what an unknown identifier, a wrong argument type or a bad cast are.
pub fn classify_sql(code: u32, message: &str) -> SqlError {
    let text = description(message);
    match code {
        // READONLY, QUERY_IS_PROHIBITED, FUNCTION_NOT_ALLOWED, SETTING_CONSTRAINT_VIOLATION,
        // DICTIONARY_ACCESS_DENIED, ACCESS_DENIED, DATABASE_ACCESS_DENIED, SUPPORT_IS_DISABLED.
        164 | 392 | 446 | 452 | 482 | 497 | 291 | 344 => SqlError::Forbidden {
            code,
            message: text,
        },
        // INDEX_NOT_USED: `force_index_by_date` found no condition on the partition's time column.
        277 => SqlError::TimeWindowRequired,
        // TOO_MANY_ROWS, TOO_MANY_BYTES, TOO_MANY_ROWS_OR_BYTES, SET_SIZE_LIMIT_EXCEEDED, TOO_SLOW,
        // TOO_MANY_COLUMNS, TOO_DEEP_SUBQUERIES, TOO_DEEP_AST, TOO_BIG_AST, TOO_MANY_PARTITIONS,
        // TOO_MANY_TEMPORARY_COLUMNS, TOO_MANY_TEMPORARY_NON_CONST_COLUMNS.
        158 | 307 | 396 | 191 | 160 | 161 | 162 | 167 | 168 | 565 | 165 | 166 => {
            SqlError::TooBroad {
                code,
                message: text,
            }
        }
        159 => SqlError::Timeout,
        241 => SqlError::MemoryExceeded,
        // QUOTA_EXCEEDED, TOO_MANY_SIMULTANEOUS_QUERIES.
        201 | 202 => SqlError::QuotaExceeded { message: text },
        394 => SqlError::Cancelled,
        // UNKNOWN_USER, AUTHENTICATION_FAILED, WRONG_PASSWORD, REQUIRED_PASSWORD: the identity is not
        // provisioned at this generation.
        192 | 516 | 193 | 194 => {
            SqlError::Unavailable("the project's workspace identity is not provisioned".to_owned())
        }
        // SOCKET_TIMEOUT, NETWORK_ERROR, ALL_CONNECTION_TRIES_FAILED, ABORTED, the Keeper codes.
        209 | 210 | 279 | 236 | 999 => SqlError::Unavailable(text),
        // LOGICAL_ERROR and the server's own bugs.
        49 | 1000..=1002 => SqlError::Internal(text),
        _ => {
            let (line, column) = position(&text);
            SqlError::Invalid {
                code,
                message: text,
                line,
                column,
            }
        }
    }
}

fn sql_error(failure: Failure) -> SqlError {
    match failure {
        Failure::Server { code, message } => classify_sql(code, &message),
        Failure::NotSent(message) | Failure::Network(message) => SqlError::Unavailable(message),
        // A workspace statement may run as long as its profile allows, so the deadline stays a timeout.
        Failure::Timeout | Failure::NoAnswer => SqlError::Timeout,
        Failure::Client(message) => SqlError::Internal(message),
    }
}

/// Whether a parameter name can be a `param_<name>` field: an ASCII identifier.
fn is_param_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && name.len() <= 64
}

/// Cuts one cell at `max` bytes. A string is cut at a character boundary; any other value that is longer in
/// its JSON text becomes that text, cut. Returns the cell and the bytes it counts for.
fn cut_cell(cell: SqlCell, max: usize) -> (SqlCell, usize, bool) {
    let cut = |text: &str| {
        let mut end = max.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{CELL_CUT_MARKER}", &text[..end])
    };
    match cell {
        SqlCell::String(text) if text.len() > max => {
            let text = cut(&text);
            let len = text.len();
            (SqlCell::String(text), len, true)
        }
        SqlCell::String(text) => {
            let len = text.len();
            (SqlCell::String(text), len, false)
        }
        SqlCell::Null | SqlCell::Bool(_) | SqlCell::Number(_) => (cell, 8, false),
        nested => {
            let text = nested.to_string();
            if text.len() > max {
                let text = cut(&text);
                let len = text.len();
                (SqlCell::String(text), len, true)
            } else {
                let len = text.len();
                (nested, len, false)
            }
        }
    }
}

/// The `data` array of a `JSONCompact` answer, read row by row within the limits: rows past a limit are
/// skipped without being built.
struct Rows<'a> {
    limits: SqlLimits,
    grid: &'a mut SqlGrid,
    bytes: &'a mut usize,
}

impl<'de> DeserializeSeed<'de> for Rows<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Rows<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an array of rows")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        loop {
            let full = self.grid.rows.len() >= self.limits.max_rows
                || *self.bytes >= self.limits.max_bytes;
            if full {
                if seq.next_element::<IgnoredAny>()?.is_none() {
                    return Ok(());
                }
                self.grid.truncated = true;
                continue;
            }
            let Some(row) = seq.next_element::<Vec<SqlCell>>()? else {
                return Ok(());
            };
            let mut cells = Vec::with_capacity(row.len());
            for cell in row {
                let (cell, len, cut) = cut_cell(cell, self.limits.max_cell_bytes);
                *self.bytes += len;
                self.grid.cut_cells += u64::from(cut);
                cells.push(cell);
            }
            self.grid.rows.push(cells);
        }
    }
}

/// A whole `JSONCompact` answer.
struct Answer {
    limits: SqlLimits,
}

impl<'de> Visitor<'de> for Answer {
    type Value = (SqlGrid, Option<String>);

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSONCompact answer")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut grid = SqlGrid {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
            cut_cells: 0,
            stats: SqlStats::default(),
        };
        let mut bytes = 0;
        let mut exception = None;
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "meta" => {
                    let meta: Vec<BTreeMap<String, String>> = map.next_value()?;
                    grid.columns = meta
                        .into_iter()
                        .map(|mut column| SqlColumn {
                            name: column.remove("name").unwrap_or_default(),
                            ty: column.remove("type").unwrap_or_default(),
                        })
                        .collect();
                }
                "data" => map.next_value_seed(Rows {
                    limits: self.limits,
                    grid: &mut grid,
                    bytes: &mut bytes,
                })?,
                "statistics" => {
                    let stats: BTreeMap<String, f64> = map.next_value()?;
                    let whole = |name: &str, scale: f64| {
                        let value = stats.get(name).copied().unwrap_or_default() * scale;
                        // Statistics are small non-negative numbers; the cast saturates otherwise.
                        #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let value = value.max(0.0) as u64;
                        value
                    };
                    grid.stats = SqlStats {
                        rows_read: whole("rows_read", 1.0),
                        bytes_read: whole("bytes_read", 1.0),
                        elapsed_ms: whole("elapsed", 1000.0),
                    };
                }
                "exception" => exception = Some(map.next_value::<String>()?),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok((grid, exception))
    }
}

/// Parses a `JSONCompact` answer into a grid within `limits`.
pub fn parse_grid(body: &[u8], limits: SqlLimits) -> Result<SqlGrid, SqlError> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let (grid, exception) = deserializer
        .deserialize_map(Answer { limits })
        .map_err(|error| {
            SqlError::Internal(format!("the store's answer is not a result grid: {error}"))
        })?;
    if let Some(text) = exception {
        return Err(match crate::codes::parse_code(&text) {
            Some(code) => classify_sql(code, &text),
            None => SqlError::Internal(text),
        });
    }
    Ok(grid)
}

/// What a probe statement must do.
enum Expect {
    /// The server must refuse it.
    Refused,
    /// It must answer exactly this text.
    Answer(String),
    /// It must be refused, or answer exactly this text.
    RefusedOrAnswer(&'static str),
}

struct Check {
    name: &'static str,
    sql: String,
    /// A setting sent as a URL parameter with the statement.
    setting: Option<(&'static str, &'static str)>,
    expect: Expect,
}

/// The statements of the runtime self-probe (contract 7.5), run as the first probe identity. The second probe
/// project is "the other project": nothing of it may be visible.
fn probe_checks() -> Vec<Check> {
    let own = PROBE_PROJECTS[0];
    let other = PROBE_PROJECTS[1];
    let other_id = other.uuid();
    let other_canary = probe_canary(other);
    let refused = |name: &'static str, sql: &str| Check {
        name,
        sql: sql.to_owned(),
        setting: None,
        expect: Expect::Refused,
    };
    let answer = |name: &'static str, sql: String, expected: String| Check {
        name,
        sql,
        setting: None,
        expect: Expect::Answer(expected),
    };
    vec![
        answer(
            "identity",
            "SELECT currentUser()".to_owned(),
            workspace_user(own),
        ),
        answer(
            "own_rows_readable",
            "SELECT canary FROM crucible.sql_probe".to_owned(),
            probe_canary(own),
        ),
        answer(
            "cross_project_read",
            format!(
                "SELECT count() FROM crucible.sql_probe WHERE project_id = toUUID('{other_id}')"
            ),
            "0".to_owned(),
        ),
        answer(
            "cross_project_canary",
            format!("SELECT count() FROM crucible.sql_probe WHERE canary = '{other_canary}'"),
            "0".to_owned(),
        ),
        answer(
            "cross_project_total",
            "SELECT count() FROM crucible.sql_probe".to_owned(),
            "1".to_owned(),
        ),
        // A plain MergeTree table refuses FINAL; an engine that takes it must still count one row.
        Check {
            name: "cross_project_final",
            sql: "SELECT count() FROM crucible.sql_probe FINAL".to_owned(),
            setting: None,
            expect: Expect::RefusedOrAnswer("1"),
        },
        answer(
            "cross_project_subquery",
            format!(
                "SELECT count() FROM (SELECT canary FROM crucible.sql_probe UNION ALL SELECT canary FROM \
                 crucible.sql_probe) WHERE canary = '{other_canary}'"
            ),
            "0".to_owned(),
        ),
        Check {
            name: "merge_table_function",
            sql: format!(
                "SELECT count() FROM merge('crucible', '^sql_probe$') WHERE project_id = toUUID('{other_id}')"
            ),
            setting: None,
            expect: Expect::RefusedOrAnswer("0"),
        },
        answer(
            "readonly_mode",
            "SELECT getSetting('readonly')".to_owned(),
            "1".to_owned(),
        ),
        refused("system_query_log", "SELECT count() FROM system.query_log"),
        refused("system_processes", "SELECT count() FROM system.processes"),
        refused("system_users", "SELECT count() FROM system.users"),
        refused(
            "system_row_policies",
            "SELECT count() FROM system.row_policies",
        ),
        refused(
            "ungranted_table",
            "SELECT count() FROM crucible.dedup_selftest",
        ),
        refused(
            "url_table_function",
            "SELECT * FROM url('http://127.0.0.1:8123/ping', 'LineAsString', 'line String')",
        ),
        refused(
            "file_table_function",
            "SELECT * FROM file('probe.csv', 'CSV', 'line String')",
        ),
        refused(
            "remote_table_function",
            "SELECT count() FROM remote('127.0.0.1', 'crucible', 'sql_probe')",
        ),
        refused("set_statement", "SET max_execution_time = 600"),
        refused("set_readonly", "SET readonly = 0"),
        refused(
            "settings_clause",
            "SELECT 1 SETTINGS max_memory_usage = 100000000000",
        ),
        Check {
            name: "settings_override",
            sql: "SELECT 1".to_owned(),
            setting: Some(("max_result_rows", "1000000")),
            expect: Expect::Refused,
        },
        refused(
            "insert",
            "INSERT INTO crucible.sql_probe (project_id, id, canary, start_time) VALUES \
             ('00000000-0000-0000-0000-000000000001', 9, 'x', now64(6))",
        ),
        refused(
            "create_table",
            "CREATE TABLE crucible.sql_probe_escape (x UInt8) ENGINE = Memory",
        ),
        refused(
            "create_temporary_table",
            "CREATE TEMPORARY TABLE sql_probe_escape (x UInt8)",
        ),
        refused(
            "drop_row_policy",
            "DROP ROW POLICY IF EXISTS crucible_sql_static_restrictive ON crucible.sql_probe",
        ),
        refused("kill_query", "KILL QUERY WHERE 1"),
        refused("system_statement", "SYSTEM FLUSH LOGS"),
        refused("sleep", "SELECT sleep(3)"),
    ]
}

impl ClickHouseSqlStore {
    /// A workspace store over `store`'s connection pool.
    pub fn new(store: ClickHouseStore, config: SqlStoreConfig) -> Self {
        Self { store, config }
    }

    fn password_hash(&self, project: ProjectId, generation: u32) -> Result<PasswordHash, SqlError> {
        let password = self
            .config
            .passwords
            .password(&workspace_user(project), generation);
        PasswordHash::new(&sha256_hex(&password)).map_err(|e| SqlError::Internal(e.to_string()))
    }

    /// `base` fitted to this server: the settings that only newer servers know are switched on when the server
    /// lists them.
    pub async fn server_profile(&self, base: SqlProfile) -> Result<SqlProfile, SqlError> {
        let mut b = Binder::default();
        let names = b.strings(&JOIN_SPILL_SETTINGS);
        let statement = b.finish(format!(
            "SELECT count() FROM system.settings WHERE has({names}, name)"
        ));
        let known = self
            .store
            .text(ServiceIdentity::Admin, &statement, LOOKUP_SECS)
            .await
            .map_err(admin_error)?;
        Ok(SqlProfile {
            no_join_spill: known.trim().parse::<usize>().ok() == Some(JOIN_SPILL_SETTINGS.len()),
            ..base
        })
    }

    /// Brings the existing settings profile to `profile`, as `crucible_admin`. A server upgrade can add
    /// settings the profile should pin; this repairs that without a new migration step.
    pub async fn apply_profile(&self, profile: &SqlProfile) -> Result<(), SqlError> {
        self.admin(&[profile.alter(&self.config.variant)]).await
    }

    /// Brings the existing quota to the configured statements per hour, as `crucible_admin`. The migration
    /// ledger applies access step A2 once, so a changed `sql.statements_per_hour` reaches the server here.
    pub async fn apply_quota(&self) -> Result<(), SqlError> {
        self.admin(&[alter_quota(
            &self.config.variant,
            self.config.statements_per_hour,
        )])
        .await
    }

    /// The inputs of access step A2: the admin user, the profile and the digests of the two probe identities'
    /// passwords at generation 0.
    pub fn workspace_params(&self, profile: SqlProfile) -> Result<WorkspaceParams, SqlError> {
        Ok(WorkspaceParams {
            admin_user: self.config.admin_user.clone(),
            profile,
            statements_per_hour: self.config.statements_per_hour,
            probe: [
                self.password_hash(PROBE_PROJECTS[0], 0)?,
                self.password_hash(PROBE_PROJECTS[1], 0)?,
            ],
        })
    }

    /// The statement as the project's own identity: the text verbatim, with the query id and the parameters.
    fn as_project(
        &self,
        project: ProjectId,
        generation: u32,
        sql: &str,
        params: &[SqlParam],
        query_id: Uuid,
    ) -> Result<clickhouse::query::Query, SqlError> {
        let user = workspace_user(project);
        let password = self.config.passwords.password(&user, generation);
        let mut query = self
            .store
            .workspace_client()
            .with_user(user)
            .with_password(password)
            .query_raw(sql)
            .with_setting("query_id", query_id.to_string());
        for param in params {
            if !is_param_name(&param.name) {
                return Err(SqlError::Internal(format!(
                    "{:?} is not a parameter name",
                    param.name
                )));
            }
            query = query.with_setting(format!("param_{}", param.name), escape(&param.value));
        }
        Ok(query)
    }

    /// Reads a whole answer of at most `cap` bytes.
    async fn collect(mut reading: Reading, cap: usize) -> Result<Bytes, SqlError> {
        let mut body = BytesMut::new();
        while let Some(chunk) = reading.next_chunk().await {
            let chunk = chunk.map_err(sql_error)?;
            if body.len() + chunk.len() > cap {
                reading.abandon();
                return Err(SqlError::TooBroad {
                    code: 0,
                    message: format!("the result is larger than {} MiB", cap >> 20),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }

    /// A short statement as a project identity, answered as tab-separated text.
    async fn text(
        &self,
        project: ProjectId,
        generation: u32,
        sql: &str,
        params: &[SqlParam],
        setting: Option<(&str, &str)>,
    ) -> Result<String, SqlError> {
        let query_id = self.store.env().new_id();
        let mut query = self.as_project(project, generation, sql, params, query_id)?;
        if let Some((name, value)) = setting {
            query = query.with_setting(name, value);
        }
        let reading = self
            .store
            .reading(query, query_id, "TabSeparated", SHORT_DEADLINE, false)
            .map_err(sql_error)?;
        let body = Self::collect(reading, BODY_CAP).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn admin(&self, statements: &[String]) -> Result<(), SqlError> {
        // Every replica runs these ON CLUSTER statements, so one that is down is named at once.
        if let Variant::Clustered { cluster } = &self.config.variant {
            self.store
                .every_replica_answers(cluster, "try again")
                .await
                .map_err(|error| match error {
                    StoreError::Unavailable(message) => SqlError::Unavailable(message),
                    other => SqlError::Internal(other.to_string()),
                })?;
        }
        let settings: Vec<(&str, String)> = self
            .config
            .variant
            .ddl_settings()
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect();
        for sql in statements {
            let statement = Binder::default().finish(sql.clone());
            self.store
                .command(
                    ServiceIdentity::Admin,
                    &statement,
                    &settings,
                    CHANGE_DEADLINE,
                )
                .await
                .map_err(admin_error)?;
        }
        Ok(())
    }

    /// The probe as a project identity at a generation; [`SqlWorkspaceStore::probe`] runs it as the first probe
    /// identity. The isolation suite runs it against deliberately broken deployments.
    pub async fn probe_as(
        &self,
        project: ProjectId,
        generation: u32,
    ) -> Result<ProbeReport, SqlError> {
        let version = self
            .text(project, generation, "SELECT version()", &[], None)
            .await?
            .trim()
            .to_owned();
        let mut breaches = Vec::new();
        let checks = probe_checks();
        let mut statements = 1;
        for check in &checks {
            statements += 1;
            let outcome = self
                .text(project, generation, &check.sql, &[], check.setting)
                .await;
            let breach = match (outcome, &check.expect) {
                // The server refused the statement itself.
                (
                    Err(
                        SqlError::Forbidden { .. }
                        | SqlError::Invalid { .. }
                        | SqlError::TooBroad { .. }
                        | SqlError::TimeWindowRequired,
                    ),
                    Expect::Refused | Expect::RefusedOrAnswer(_),
                ) => None,
                (Err(SqlError::Forbidden { .. } | SqlError::Invalid { .. }), Expect::Answer(_)) => {
                    Some("the statement was refused".to_owned())
                }
                // The probe could not run; that says nothing about the access scheme.
                (Err(error), _) => return Err(error),
                (Ok(_), Expect::Refused) => Some("the statement succeeded".to_owned()),
                (Ok(text), Expect::Answer(expected)) => (text.trim_end() != expected.as_str())
                    .then(|| "the statement answered something else".to_owned()),
                (Ok(text), Expect::RefusedOrAnswer(expected)) => (text.trim_end() != *expected)
                    .then(|| "the statement answered something else".to_owned()),
            };
            if let Some(detail) = breach {
                breaches.push(ProbeBreach {
                    check: check.name,
                    detail,
                });
            }
        }
        Ok(ProbeReport {
            version,
            statements,
            breaches,
        })
    }
}

fn admin_error(failure: Failure) -> SqlError {
    match failure {
        Failure::Server { code, message } => SqlError::Internal(format!(
            "the admin identity was refused (code {code}): {}",
            description(&message)
        )),
        other => sql_error(other),
    }
}

/// The sum of the `parts`, `rows` and `marks` columns of an `EXPLAIN ESTIMATE` answer.
fn estimate(text: &str) -> SqlEstimate {
    let mut total = SqlEstimate::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if let [_, _, parts, rows, marks] = fields.as_slice() {
            total.parts += parts.parse::<u64>().unwrap_or_default();
            total.rows += rows.parse::<u64>().unwrap_or_default();
            total.marks += marks.parse::<u64>().unwrap_or_default();
        }
    }
    total
}

#[async_trait]
impl SqlWorkspaceStore for ClickHouseSqlStore {
    async fn validate(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
    ) -> Result<SqlEstimate, SqlError> {
        let project = scope.project_id();
        // The keyword sits on a line of its own, so the server reports a syntax error by line and column, and
        // the user's first line keeps its columns. The line is corrected below.
        for explain in ["EXPLAIN AST", "EXPLAIN ESTIMATE"] {
            let sql = format!("{explain}\n{}", query.sql);
            let text = self
                .text(project, query.generation, &sql, &query.params, None)
                .await
                .map_err(|error| match error {
                    SqlError::Invalid {
                        code,
                        message,
                        line: Some(line),
                        column,
                    } => SqlError::Invalid {
                        code,
                        message,
                        line: Some(line.saturating_sub(1).max(1)),
                        column,
                    },
                    other => other,
                })?;
            if explain == "EXPLAIN ESTIMATE" {
                return Ok(estimate(&text));
            }
        }
        Ok(SqlEstimate::default())
    }

    async fn run(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        limits: SqlLimits,
    ) -> Result<SqlGrid, SqlError> {
        let prepared = self.as_project(
            scope.project_id(),
            query.generation,
            &query.sql,
            &query.params,
            query.query_id,
        )?;
        let reading = self
            .store
            .reading(
                prepared,
                query.query_id,
                "JSONCompact",
                limits.deadline,
                true,
            )
            .map_err(sql_error)?;
        let body = Self::collect(reading, BODY_CAP).await?;
        parse_grid(&body, limits)
    }

    async fn export(
        &self,
        scope: ProjectScope,
        query: &SqlQuery,
        format: SqlExportFormat,
        limits: SqlLimits,
    ) -> Result<Bytes, SqlError> {
        let prepared = self.as_project(
            scope.project_id(),
            query.generation,
            &query.sql,
            &query.params,
            query.query_id,
        )?;
        let format = match format {
            SqlExportFormat::JsonLines => "JSONEachRow",
            SqlExportFormat::Parquet => "Parquet",
        };
        let reading = self
            .store
            .reading(prepared, query.query_id, format, limits.deadline, true)
            .map_err(sql_error)?;
        Self::collect(reading, limits.max_bytes).await
    }

    async fn cancel(&self, scope: ProjectScope, query_id: Uuid) -> Result<(), SqlError> {
        let mut b = Binder::default();
        let id = b.string(&query_id.to_string());
        let user = b.string(&workspace_user(scope.project_id()));
        // The user term binds the kill to this project's identity: a query id of another project matches
        // nothing.
        let statement = b.finish(format!(
            "KILL QUERY WHERE query_id = {id} AND user = {user} ASYNC"
        ));
        self.store
            .command(
                ServiceIdentity::Maint,
                &statement,
                &[],
                Duration::from_secs(LOOKUP_SECS),
            )
            .await
            .map_err(admin_error)
    }

    async fn provision(
        &self,
        project_id: ProjectId,
        generation: u32,
        retention: &RetentionSettings,
    ) -> Result<(), SqlError> {
        // Rule 1 of contract 7.3: no workspace policy before every table's service policy.
        let check = Binder::default().finish(service_policy_check(&self.config.admin_user));
        let answer = self
            .store
            .text(ServiceIdentity::Admin, &check, LOOKUP_SECS)
            .await
            .map_err(admin_error)?;
        if answer.trim().parse::<usize>().ok() != Some(TABLES.len()) {
            return Err(SqlError::Unavailable(
                "a base table has no service policy; run `crucible migrate` first".to_owned(),
            ));
        }
        let hash = self.password_hash(project_id, generation)?;
        self.admin(&provision_identity(
            project_id,
            &hash,
            retention,
            &self.config.variant,
        ))
        .await
    }

    async fn deprovision(&self, project_id: ProjectId) -> Result<(), SqlError> {
        self.admin(&deprovision_identity(project_id, &self.config.variant))
            .await
    }

    async fn probe(&self) -> Result<ProbeReport, SqlError> {
        self.probe_as(PROBE_PROJECTS[0], 0).await
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const SMALL: SqlLimits = SqlLimits {
        max_rows: 3,
        max_bytes: 64,
        max_cell_bytes: 8,
        deadline: Duration::from_secs(1),
    };

    fn answer(rows: &str) -> String {
        format!(
            r#"{{"meta":[{{"name":"a","type":"String"}},{{"name":"n","type":"UInt64"}}],"data":[{rows}],"rows":1,
            "statistics":{{"elapsed":0.0125,"rows_read":42,"bytes_read":1000}}}}"#
        )
    }

    #[test]
    fn a_grid_carries_columns_rows_and_statistics() {
        let grid = parse_grid(answer(r#"["x","7"],[null,"8"]"#).as_bytes(), SMALL).unwrap();
        assert_eq!(
            grid.columns,
            [
                SqlColumn {
                    name: "a".into(),
                    ty: "String".into()
                },
                SqlColumn {
                    name: "n".into(),
                    ty: "UInt64".into()
                }
            ]
        );
        assert_eq!(grid.rows.len(), 2);
        assert_eq!(grid.rows[1][0], SqlCell::Null);
        assert!(!grid.truncated);
        assert_eq!(
            grid.stats,
            SqlStats {
                rows_read: 42,
                bytes_read: 1000,
                elapsed_ms: 12
            }
        );
    }

    #[test]
    fn rows_past_the_limit_are_dropped_and_long_cells_are_cut() {
        let rows =
            r#"["0123456789abc","1"],["é123456789","2"],[["a","b","c","d","e"],"3"],["x","4"]"#;
        let grid = parse_grid(answer(rows).as_bytes(), SMALL).unwrap();
        assert_eq!(grid.rows.len(), 3);
        assert!(grid.truncated);
        assert_eq!(grid.cut_cells, 3);
        assert_eq!(
            grid.rows[0][0],
            SqlCell::String(format!("01234567{CELL_CUT_MARKER}"))
        );
        // The cut falls back to a character boundary.
        assert_eq!(
            grid.rows[1][0],
            SqlCell::String(format!("é123456{CELL_CUT_MARKER}"))
        );
        assert_eq!(
            grid.rows[2][0],
            SqlCell::String(format!("[\"a\",\"b\"{CELL_CUT_MARKER}"))
        );
    }

    #[test]
    fn the_byte_limit_stops_the_grid() {
        let limits = SqlLimits {
            max_rows: 100,
            max_bytes: 10,
            max_cell_bytes: 100,
            deadline: Duration::from_secs(1),
        };
        let rows = r#"["aaaaaaaaaaaa","1"],["b","2"]"#;
        let grid = parse_grid(answer(rows).as_bytes(), limits).unwrap();
        assert_eq!(grid.rows.len(), 1);
        assert!(grid.truncated);
    }

    #[test]
    fn an_exception_inside_the_answer_is_an_error() {
        let body =
            r#"{"meta":[],"data":[],"exception":"Code: 159. DB::Exception: Timeout exceeded"}"#;
        assert_eq!(parse_grid(body.as_bytes(), SMALL), Err(SqlError::Timeout));
        assert!(matches!(
            parse_grid(b"not json", SMALL),
            Err(SqlError::Internal(_))
        ));
    }

    #[test]
    fn server_codes_map_to_the_problem_codes() {
        let code = |code: u32| classify_sql(code, "Code: 1. DB::Exception: x").problem_code();
        for forbidden in [164, 497, 392, 452, 446] {
            assert_eq!(code(forbidden), "sql_forbidden", "{forbidden}");
        }
        assert_eq!(code(277), "sql_time_window_required");
        for broad in [158, 307, 396, 191, 160, 565, 167, 168] {
            assert_eq!(code(broad), "query_too_broad", "{broad}");
        }
        assert_eq!(code(159), "query_timeout");
        assert_eq!(code(241), "query_memory_exceeded");
        assert_eq!(code(201), "sql_quota_exceeded");
        assert_eq!(code(202), "sql_quota_exceeded");
        assert_eq!(code(516), "sql_workspace_unavailable");
        for invalid in [62, 47, 60, 46, 43] {
            assert_eq!(code(invalid), "sql_invalid", "{invalid}");
        }
        assert_eq!(code(49), "internal_error");
    }

    #[test]
    fn a_syntax_error_carries_its_line_and_column() {
        let message = "Code: 62. DB::Exception: Syntax error: failed at position 8 (line 2, col 3): FROMM x. \
                       Expected one of: token (version 26.3.1.1 (official build))";
        let SqlError::Invalid {
            code,
            message,
            line,
            column,
        } = classify_sql(62, message)
        else {
            panic!("not invalid");
        };
        assert_eq!((code, line, column), (62, Some(2), Some(3)));
        assert!(message.starts_with("Syntax error"));
        assert!(!message.contains("version"));
        // A single-line text has a position only.
        let single = "Code: 62. DB::Exception: Syntax error: failed at position 13 (FROMM): FROMM x. Expected one of";
        assert!(matches!(
            classify_sql(62, single),
            SqlError::Invalid {
                line: Some(1),
                column: Some(13),
                ..
            }
        ));
    }

    #[test]
    fn parameter_names_are_identifiers() {
        for good in ["from", "to", "_x", "a1", "user_id"] {
            assert!(is_param_name(good), "{good}");
        }
        for bad in ["", "1a", "a-b", "a b", "a&b=1", "a\"b", "é"] {
            assert!(!is_param_name(bad), "{bad}");
        }
    }

    #[test]
    fn the_estimate_sums_every_table() {
        let text = "crucible\tspan_core\t2\t100\t3\ncrucible\tfacts\t1\t5\t1\n";
        assert_eq!(
            estimate(text),
            SqlEstimate {
                parts: 3,
                rows: 105,
                marks: 4
            }
        );
        assert_eq!(estimate(""), SqlEstimate::default());
    }

    #[test]
    fn the_probe_has_about_25_statements_with_unique_names() {
        let checks = probe_checks();
        assert!((25..=30).contains(&checks.len()), "{}", checks.len());
        let mut names: Vec<&str> = checks.iter().map(|c| c.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), checks.len());
        for class in [
            "cross_project_read",
            "system_query_log",
            "url_table_function",
            "set_statement",
            "insert",
            "settings_override",
        ] {
            assert!(names.contains(&class), "{class}");
        }
    }

    proptest! {
        /// Whatever the answer holds, the grid stays inside its limits and a cut cell stays valid text.
        #[test]
        fn a_grid_never_exceeds_its_limits(cells in prop::collection::vec(".{0,40}", 0..40), max_rows in 1usize..8, max_cell in 1usize..16) {
            let limits = SqlLimits { max_rows, max_bytes: 200, max_cell_bytes: max_cell, deadline: Duration::from_secs(1) };
            let rows: Vec<String> = cells
                .iter()
                .map(|cell| format!("[{},\"1\"]", serde_json::Value::String(cell.clone())))
                .collect();
            let grid = parse_grid(answer(&rows.join(",")).as_bytes(), limits).unwrap();
            prop_assert!(grid.rows.len() <= max_rows);
            prop_assert_eq!(grid.truncated, grid.rows.len() < cells.len());
            for row in &grid.rows {
                let SqlCell::String(text) = &row[0] else { panic!("not a string") };
                prop_assert!(text.len() <= max_cell + CELL_CUT_MARKER.len());
            }
        }
    }
}
