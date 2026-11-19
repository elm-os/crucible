//! Layer A: direct attacks W1 to W15 as project A's ClickHouse identity. No workspace code is in the path, so
//! what stops each attack is the server's access control alone. Every answer passes the canary oracle of
//! [`Iso::send`]; the checks here add what each script expects on top of it.

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use crucible_env::SqlWorkspaceStore as _;
use crucible_records::ProjectId;
use crucible_schema::{
    CURATED_VIEWS, RetentionSettings, SQL_QUOTA, SQL_QUOTA_QUERIES_PER_HOUR, project_policy_name,
    sql_tables, table, workspace_user,
};

use super::{FORMS, Iso, Options, Reply, canary_column, fixture_row, view_of, window};
use crate::expect;

/// Table functions that may answer: they read nothing but their own arguments or what the caller may already
/// read. Any other table function that answers fails W2.
const BENIGN_TABLE_FUNCTIONS: &[&str] = &[
    "numbers",
    "numbers_mt",
    "zeros",
    "zeros_mt",
    "primes",
    "generate_series",
    "generateSeries",
    "generateRandom",
    "values",
    "null",
    "format",
    "view",
    "viewIfPermitted",
    "merge",
    "fuzzJSON",
    "fuzzQuery",
    "loop",
    "timeSeriesSelector",
    "SQLStandardValues",
];

/// Settings a workspace identity may change although it is read-only: the server marks them changeable for
/// every user. They shape or encode the answer of the caller's own statement (the query-string forms `select`,
/// `filter`, `sort`, `limit` and the output format) or name the default database; none reads another row. Any
/// other changeable setting fails W9.
const PRESENTATION_SETTINGS: &[&str] = &[
    "compression",
    "database",
    "default_format",
    "filter",
    "format",
    "http_x_clickhouse_format_overrides_output_format",
    "implicit_table_at_top_level",
    "input_format",
    "limit",
    "offset",
    "order",
    "output_format",
    "page",
    "select",
    "sort",
];

/// System tables that must be refused whatever else the server lets a granted user read: they show other
/// users' statements, the access entities or the server's logs.
const DENIED_SYSTEM_TABLES: &[&str] = &[
    "query_log",
    "query_thread_log",
    "query_views_log",
    "processes",
    "text_log",
    "trace_log",
    "part_log",
    "session_log",
    "metric_log",
    "asynchronous_metric_log",
    "opentelemetry_span_log",
    "crash_log",
    "error_log",
    "users",
    "roles",
    "grants",
    "role_grants",
    "row_policies",
    "quotas",
    "quota_limits",
    "quotas_usage",
    "settings_profiles",
    "settings_profile_elements",
    "user_directories",
    "zookeeper",
    "mutations",
    "replicas",
    "replication_queue",
    "distributed_ddl_queue",
    "asynchronous_inserts",
    "named_collections",
    "user_processes",
    "query_cache",
    "s3queue",
    "backups",
];

/// Lifts the hourly quota for the rest of the run: the scripts below and layers B and D send thousands of
/// statements as one identity. The quota bounds cost, not visibility, and W8 tests it at its real limits first.
pub(crate) async fn lift_quota(iso: &Iso) -> Result<String> {
    iso.ctx
        .admin(format!(
            "ALTER QUOTA {SQL_QUOTA} FOR INTERVAL 1 hour MAX queries = 100000000, errors = 100000000, read_rows = \
             100000000000000, execution_time = 360000"
        ))
        .await?;
    iso.quota_lifted
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Ok("the quota's limits are raised for the remaining rows".to_owned())
}

/// W1: cross-project reads by every access path.
async fn w1(iso: &Iso) -> Result<String> {
    let a = iso.a.uuid();
    let b = iso.b.uuid();
    let mut statements = 0;
    for def in sql_tables().filter(|def| def.name() != "sql_probe") {
        let t = format!("crucible.{}", def.name());
        let w = window(def);
        // The canary column first: if the table shows another project's row, the oracle is what reports it.
        if let Some(column) = canary_column(def) {
            iso.attack(&format!("SELECT {column} FROM {t} WHERE {w}"))
                .await?;
            statements += 1;
        }
        // A reads its own row, and only that one, through every shape of read.
        iso.answers(&format!("SELECT count() FROM {t} WHERE {w}"), "1")
            .await?;
        iso.answers(
            &format!("SELECT DISTINCT toString(project_id) FROM {t} WHERE {w}"),
            &a.to_string(),
        )
        .await?;
        let zero = [
            format!("SELECT count() FROM {t} WHERE {w} AND project_id = toUUID('{b}')"),
            format!("SELECT count() FROM {t} PREWHERE project_id = toUUID('{b}') WHERE {w}"),
            format!("SELECT count() FROM {t} WHERE {w} AND project_id != toUUID('{a}')"),
            format!("SELECT count() FROM {t} WHERE {w} AND (project_id = toUUID('{b}') OR 1 = 0)"),
            format!(
                "SELECT count() FROM {t} AS x INNER JOIN {t} AS y ON x.project_id != y.project_id WHERE x.{w} AND \
                 y.{w}"
            ),
            format!(
                "SELECT count() FROM {t} WHERE {w} AND project_id IN (SELECT project_id FROM {t} WHERE {w} AND \
                 project_id = toUUID('{b}'))"
            ),
            format!(
                "WITH other AS (SELECT project_id FROM {t} WHERE {w} AND project_id = toUUID('{b}')) SELECT count() \
                 FROM other"
            ),
            format!(
                "SELECT count() FROM (SELECT project_id FROM {t} WHERE {w} UNION ALL SELECT project_id FROM {t} \
                 WHERE {w}) WHERE project_id = toUUID('{b}')"
            ),
            format!(
                "SELECT count() FROM view(SELECT project_id FROM {t} WHERE {w}) WHERE project_id = toUUID('{b}')"
            ),
            format!(
                "SELECT count() FROM {t} WHERE {w} AND _part != '' AND project_id = toUUID('{b}') SETTINGS \
                 optimize_trivial_count_query = 0"
            ),
        ];
        for sql in &zero {
            iso.refused_or(sql, "0").await?;
        }
        statements += 2 + zero.len();
        // Reads that may be refused (the engine, the window backstop or a missing sampling key), and otherwise
        // show A's project alone.
        let own = [
            format!("SELECT DISTINCT toString(project_id) FROM {t} FINAL WHERE {w}"),
            format!("SELECT DISTINCT toString(project_id) FROM {t} SAMPLE 1 WHERE {w}"),
            format!("SELECT DISTINCT toString(project_id) FROM {t}"),
            format!(
                "SELECT DISTINCT toString(project_id) FROM merge('crucible', '^{}$') WHERE {w}",
                def.name()
            ),
            format!("SELECT toString(any(project_id)) FROM {t} WHERE {w} GROUP BY _partition_id"),
            format!(
                "SELECT DISTINCT toString(project_id) FROM {t} WHERE {w} AND _row_exists IN (0, 1)"
            ),
            format!("SELECT toString(min(project_id)) FROM {t} WHERE {w} HAVING count() > 0"),
            format!("SELECT toString(max(project_id)) FROM {t} WHERE {w} HAVING count() > 0"),
        ];
        for sql in &own {
            iso.refused_or(sql, &a.to_string()).await?;
        }
        // The trivial count reads part metadata; it must count A's row only.
        iso.refused_or(&format!("SELECT count() FROM {t}"), "1")
            .await?;
        statements += own.len() + 1;
        if let Some(column) = canary_column(def) {
            iso.answers(
                &format!("SELECT DISTINCT {column} FROM {t} WHERE {w}"),
                &iso.canary_a,
            )
            .await?;
            // Searching for the other project's values finds nothing. The pattern is the canaries' shared
            // prefix, so the statement itself holds no canary.
            iso.answers(
                &format!("SELECT count() FROM {t} WHERE {w} AND {column} LIKE 'cnry_b_%'"),
                "0",
            )
            .await?;
            statements += 2;
        }
        // The ungranted columns stay ungranted.
        for hidden in def.columns.iter().filter(|c| !c.sql_readable).take(2) {
            iso.refused(&format!(
                "SELECT {} FROM {t} WHERE {w} LIMIT 1",
                hidden.name
            ))
            .await?;
            statements += 1;
        }
        iso.attack(&format!("SELECT * FROM {t} WHERE {w} LIMIT 1"))
            .await?;
        if let Some(view) = view_of(def) {
            let v = format!("crucible_sql.{view}");
            iso.answers(&format!("SELECT count() FROM {v} WHERE {w}"), "1")
                .await?;
            iso.refused_or(
                &format!("SELECT count() FROM {v} WHERE {w} AND project_id = toUUID('{b}')"),
                "0",
            )
            .await?;
            iso.answers(
                &format!("SELECT DISTINCT toString(project_id) FROM {v} WHERE {w}"),
                &a.to_string(),
            )
            .await?;
            // Every column of the view, B's canaries included if a policy failed.
            iso.attack(&format!("SELECT * FROM {v} WHERE {w}")).await?;
            statements += 4;
        }
    }
    // Paths that do not name a granted table.
    for sql in [
        "SELECT * FROM crucible.sqliso_admin_only",
        "SELECT count() FROM crucible.dedup_selftest",
        "SELECT * FROM merge('crucible', '.*') LIMIT 100",
        "SELECT * FROM merge('crucible', 'sqliso_admin_only')",
        "SELECT * FROM merge(REGEXP('.*'), '.*') LIMIT 100",
        "SELECT dictGet('crucible.sqliso_admin_only', 'canary', toUUID('00000000-0000-0000-0000-000000000000'))",
        "SELECT joinGet('crucible.sqliso_admin_only', 'canary', 1)",
        "SELECT * FROM cluster('default', 'crucible', 'sqliso_admin_only')",
        "SELECT * FROM clusterAllReplicas('default', 'crucible', 'span_core') LIMIT 10",
        "SELECT * FROM remote('127.0.0.1', 'crucible', 'sqliso_admin_only')",
        "SELECT * FROM remote('127.0.0.1', 'crucible', 'span_core', 'crucible_admin', 'conformance-admin') LIMIT 10",
        "SELECT * FROM remoteSecure('127.0.0.1', 'crucible', 'span_core') LIMIT 10",
        "SELECT * FROM mergeTreeIndex('crucible', 'sessions_rollup') LIMIT 100",
        "SELECT * FROM mergeTreeIndex('crucible', 'span_core', with_marks = true) LIMIT 100",
        "SELECT * FROM mergeTreeProjection('crucible', 'span_core', 'x') LIMIT 100",
        "SELECT * FROM loop('crucible', 'sqliso_admin_only') LIMIT 10",
        "DESCRIBE TABLE crucible.sqliso_admin_only",
        "SHOW CREATE TABLE crucible.sqliso_admin_only",
        "SELECT * FROM system.parts WHERE database = 'crucible' LIMIT 200",
        "SELECT * FROM system.parts_columns WHERE database = 'crucible' LIMIT 200",
        "SELECT * FROM system.columns WHERE database = 'crucible' LIMIT 2000",
        "SELECT * FROM system.tables WHERE database = 'crucible' LIMIT 200",
    ] {
        iso.attack(sql).await?;
        statements += 1;
    }
    for sql in [
        "SELECT * FROM crucible.sqliso_admin_only",
        "SELECT count() FROM crucible.dedup_selftest",
        "SELECT * FROM remote('127.0.0.1', 'crucible', 'sqliso_admin_only')",
    ] {
        iso.refused(sql).await?;
    }
    Ok(format!(
        "{statements} statements over {} tables and {} views: joins, unions, CTEs, subqueries, merge(), view(), \
         FINAL, SAMPLE, PREWHERE, virtual columns and the trivial count show project A's rows only",
        sql_tables().count() - 1,
        CURATED_VIEWS.len()
    ))
}

/// W2: every table function the server lists.
async fn w2(iso: &Iso) -> Result<String> {
    let listed = iso
        .ctx
        .admin_text("SELECT name FROM system.table_functions ORDER BY name")
        .await?;
    let arguments = [
        "",
        "'http://127.0.0.1:8123/?query=SELECT+1', 'LineAsString', 'line String'",
        "'127.0.0.1', 'crucible', 'sqliso_admin_only'",
        "'crucible', 'sqliso_admin_only'",
        "'/etc/passwd', 'LineAsString', 'line String'",
    ];
    let (mut refused, mut answered) = (0, Vec::new());
    for name in listed.lines() {
        let mut answers = false;
        for args in arguments {
            let reply = iso
                .attack(&format!("SELECT * FROM {name}({args}) LIMIT 5"))
                .await?;
            answers |= reply.rows().is_some();
        }
        if answers {
            expect(BENIGN_TABLE_FUNCTIONS.contains(&name), || {
                format!("table function {name} answered project A and is not on the benign list")
            })?;
            answered.push(name);
        } else {
            refused += 1;
        }
    }
    // The functions that reach outside the server, called the way that works for a privileged user.
    for sql in [
        "SELECT * FROM url('http://127.0.0.1:8123/ping', 'LineAsString', 'line String')",
        "SELECT * FROM file('probe.tsv', 'TSV', 'line String')",
        "SELECT * FROM s3('http://127.0.0.1:9000/bucket/x.csv', 'CSV', 'line String')",
        "SELECT * FROM remote('127.0.0.1', 'system', 'one')",
        "SELECT * FROM remote('127.0.0.1', system.one)",
        "SELECT * FROM cluster('default', system.one)",
        "SELECT * FROM executable('id', 'TSV', 'line String')",
        "SELECT * FROM mysql('127.0.0.1:3306', 'db', 't', 'u', 'p')",
        "SELECT * FROM postgresql('127.0.0.1:5432', 'db', 't', 'u', 'p')",
        "SELECT * FROM sqlite('/tmp/x.db', 't')",
        "SELECT * FROM input('x UInt8')",
    ] {
        let code = iso.refused(sql).await?;
        expect(code != 62, || {
            format!("{sql} was a syntax error, not a refusal")
        })?;
    }
    Ok(format!(
        "{} table functions: {refused} refused with every argument shape, {} answered and are benign ({}); url, \
         file, s3, remote, cluster, executable, mysql, postgresql, sqlite and input are refused",
        listed.lines().count(),
        answered.len(),
        answered.join(", ")
    ))
}

/// W3: every system and information-schema table.
async fn w3(iso: &Iso) -> Result<String> {
    let listed = iso
        .ctx
        .admin_text(
            "SELECT database, name FROM system.tables WHERE lower(database) IN ('system', 'information_schema') \
             ORDER BY database, name",
        )
        .await?;
    let (mut refused, mut readable) = (0, Vec::new());
    for line in listed.lines() {
        let Some((database, name)) = line.split_once('\t') else {
            continue;
        };
        let reply = iso
            .attack(&format!("SELECT * FROM `{database}`.`{name}` LIMIT 500"))
            .await?;
        if reply.rows().is_some() {
            expect(
                database != "system" || !DENIED_SYSTEM_TABLES.contains(&name),
                || format!("system.{name} is readable by project A"),
            )?;
            readable.push(format!("{database}.{name}"));
        } else {
            refused += 1;
        }
    }
    // What the metadata tables show of the base tables: the documented residual is totals, never rows.
    let b = iso.b.uuid();
    for sql in [
        format!("SELECT count() FROM system.tables WHERE create_table_query LIKE '%{b}%'"),
        format!("SELECT count() FROM system.columns WHERE default_expression LIKE '%{b}%'"),
    ] {
        iso.refused_or(&sql, "0").await?;
    }
    Ok(format!(
        "{} system tables: {refused} refused, {} readable without a canary ({})",
        listed.lines().count(),
        readable.len(),
        readable.join(", ")
    ))
}

/// W4: setting changes by SET, SETTINGS clause and URL parameter; role switching; impersonation.
async fn w4(iso: &Iso) -> Result<String> {
    let profile = iso.profile;
    let mut statements = 0;
    // A profile is applied setting by setting, so one that changes anything is refused in read-only mode. (The
    // empty `default` profile changes nothing and is accepted without effect.)
    iso.ctx
        .admin(
            "CREATE SETTINGS PROFILE IF NOT EXISTS sqliso_loose SETTINGS readonly = 0, max_block_size = 7, \
             max_execution_time = 999",
        )
        .await?;
    // A setting the profile does not list is still immutable: that is `readonly = 1`, not a constraint.
    for sql in [
        "SET max_block_size = 1".to_owned(),
        "SET readonly = 0".to_owned(),
        "SET readonly = 2".to_owned(),
        "SET allow_ddl = 1".to_owned(),
        "SET profile = 'sqliso_loose'".to_owned(),
        "SELECT 1 SETTINGS profile = 'sqliso_loose'".to_owned(),
        "SET allow_introspection_functions = 1".to_owned(),
        "SELECT 1 SETTINGS max_block_size = 1".to_owned(),
        "SELECT 1 SETTINGS readonly = 0".to_owned(),
        "SELECT 1 SETTINGS max_execution_time = 0".to_owned(),
        "SELECT 1 SETTINGS force_index_by_date = 0".to_owned(),
        "SELECT 1 SETTINGS additional_table_filters = {'crucible.span_core': '1'}".to_owned(),
        "SET ROLE crucible_admin".to_owned(),
        "SET ROLE crucible_read_ui".to_owned(),
        "EXECUTE AS crucible_admin SELECT 1".to_owned(),
        "EXECUTE AS crucible_admin".to_owned(),
        format!("EXECUTE AS {} SELECT 1", workspace_user(iso.b)),
        "GRANT crucible_sql_reader TO CURRENT_USER".to_owned(),
        "ALTER USER CURRENT_USER SETTINGS PROFILE 'default'".to_owned(),
    ] {
        iso.refused(&sql).await?;
        statements += 1;
    }
    for (name, value) in [
        ("max_block_size", "1"),
        ("readonly", "0"),
        ("max_execution_time", "0"),
        ("max_result_rows", "0"),
        ("force_index_by_date", "0"),
        ("allow_ddl", "1"),
        ("profile", "sqliso_loose"),
        ("role", "crucible_admin"),
    ] {
        let reply = iso.send(iso.a, "SELECT 1", &[(name, value)]).await?;
        expect(reply.code().is_some(), || {
            format!("the URL parameter {name}={value} was accepted")
        })?;
        statements += 1;
    }
    iso.ctx
        .admin("DROP SETTINGS PROFILE IF EXISTS sqliso_loose")
        .await?;
    // An HTTP session is not a setting and is accepted; inside it the session is as read-only as outside.
    for sql in [
        "SET max_block_size = 1",
        "CREATE TEMPORARY TABLE sqliso_session (x UInt8)",
    ] {
        let reply = iso.send(iso.a, sql, &[("session_id", "sqliso")]).await?;
        expect(reply.code().is_some(), || {
            format!("inside an HTTP session, {sql:?} {}", reply.brief())
        })?;
        statements += 1;
    }
    // Switching among the roles the user already holds, or to none, is accepted and gains nothing: over HTTP
    // it lasts for the one statement.
    for sql in [
        "SET ROLE NONE",
        "SET ROLE ALL",
        "SET ROLE crucible_sql_reader",
        "SET ROLE DEFAULT",
    ] {
        iso.attack(sql).await?;
        statements += 1;
    }
    // A SETTINGS clause inside a subquery is refused or ignored: the value it names never takes effect.
    for (sql, unchanged) in [
        (
            "SELECT * FROM (SELECT getSetting('max_threads') SETTINGS max_threads = 64)",
            "4",
        ),
        (
            "SELECT * FROM (SELECT getSetting('readonly') SETTINGS readonly = 0)",
            "1",
        ),
        (
            "SELECT (SELECT getSetting('max_threads') SETTINGS max_threads = 64)",
            "4",
        ),
        (
            "SELECT * FROM view(SELECT getSetting('max_threads') SETTINGS max_threads = 64)",
            "4",
        ),
        (
            "WITH x AS (SELECT getSetting('max_result_rows') SETTINGS max_result_rows = 0) SELECT * FROM x",
            "10001",
        ),
    ] {
        iso.refused_or(sql, unchanged).await?;
        statements += 1;
    }
    for sql in [
        "SELECT * FROM (SELECT count() FROM numbers(3000000000) SETTINGS max_rows_to_read = 0)",
        "SELECT * FROM (SELECT count() FROM crucible.span_core SETTINGS force_index_by_date = 0)",
        "WITH x AS (SELECT sleep(3) SETTINGS function_sleep_max_microseconds_per_block = 0) SELECT * FROM x",
    ] {
        iso.refused(sql).await?;
        statements += 1;
    }
    // Nothing moved, with or without the empty default profile named.
    for settings in [&[][..], &[("profile", "default")][..]] {
        let reply = iso
            .send(
                iso.a,
                "SELECT getSetting('readonly'), getSetting('max_block_size')",
                settings,
            )
            .await?;
        expect(reply.rows() == Some("1\t65409"), || {
            format!(
                "the session's settings with {settings:?}: {}",
                reply.brief()
            )
        })?;
    }
    iso.answers("SELECT getSetting('readonly')", "1").await?;
    iso.answers(
        "SELECT getSetting('max_execution_time')",
        &profile.max_execution_time_secs.to_string(),
    )
    .await?;
    iso.answers("SELECT currentUser()", &workspace_user(iso.a))
        .await?;
    // A finding of this suite: the server lets a user clear its own default roles, even in read-only mode. The
    // user then holds no role, so it reads nothing; no other project is touched, and provisioning repairs it.
    let project = iso.ctx.project();
    iso.provision(project).await?;
    let probe = "SELECT count() FROM crucible.sql_probe";
    let cleared = iso
        .send(project, "SET DEFAULT ROLE NONE TO CURRENT_USER", &[])
        .await?;
    let self_harm = if cleared.code().is_none() {
        let reply = iso.send(project, probe, &[]).await?;
        expect(reply.code() == Some(497), || {
            format!(
                "after clearing its default roles the user {}",
                reply.brief()
            )
        })?;
        iso.answers(probe, "0").await?;
        iso.provision(project).await?;
        let reply = iso.send(project, probe, &[]).await?;
        expect(reply.rows() == Some("0"), || {
            format!("after provisioning again the user {}", reply.brief())
        })?;
        "SET DEFAULT ROLE NONE TO CURRENT_USER is accepted: the user then reads nothing (497) until it is \
         provisioned again, and no other identity is affected"
    } else {
        "SET DEFAULT ROLE NONE TO CURRENT_USER is refused"
    };
    iso.sql
        .deprovision(project)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(format!(
        "{statements} attempts refused or without effect: SET, SETTINGS clauses (a clause inside a subquery is \
         accepted and ignored), URL parameters, profile and role switches and EXECUTE AS; the session still runs \
         as its own user with readonly = 1. {self_harm}"
    ))
}

/// W5: writes, DDL, temporary objects, SYSTEM, KILL and access statements.
async fn w5(iso: &Iso) -> Result<String> {
    let before = iso.catalogue().await?;
    let a = workspace_user(iso.a);
    let statements = [
        "INSERT INTO crucible.sql_probe (project_id, id, canary, start_time) VALUES (generateUUIDv4(), 7, 'x', now())"
            .to_owned(),
        "INSERT INTO crucible.span_core (project_id) VALUES (generateUUIDv4())".to_owned(),
        "INSERT INTO FUNCTION null('x UInt8') VALUES (1)".to_owned(),
        "ALTER TABLE crucible.sql_probe DELETE WHERE 1".to_owned(),
        "ALTER TABLE crucible.sql_probe UPDATE canary = 'x' WHERE 1".to_owned(),
        "DELETE FROM crucible.sql_probe WHERE 1".to_owned(),
        "UPDATE crucible.sql_probe SET canary = 'x' WHERE 1".to_owned(),
        "TRUNCATE TABLE crucible.sql_probe".to_owned(),
        "OPTIMIZE TABLE crucible.sql_probe FINAL".to_owned(),
        "ALTER TABLE crucible.sql_probe ADD COLUMN x UInt8".to_owned(),
        "ALTER TABLE crucible.sql_probe DROP PARTITION ID 'all'".to_owned(),
        "CREATE TABLE crucible.sqliso_escape (x UInt8) ENGINE = Memory".to_owned(),
        "CREATE TABLE crucible_sql.sqliso_escape (x UInt8) ENGINE = Memory".to_owned(),
        "CREATE DATABASE sqliso_escape".to_owned(),
        "CREATE VIEW crucible_sql.sqliso_escape AS SELECT * FROM crucible.sqliso_admin_only".to_owned(),
        "CREATE OR REPLACE VIEW crucible_sql.spans AS SELECT * FROM crucible.span_core".to_owned(),
        "CREATE MATERIALIZED VIEW crucible_sql.sqliso_mv ENGINE = Memory AS SELECT * FROM crucible.span_core".to_owned(),
        "CREATE TEMPORARY TABLE sqliso_escape (x UInt8)".to_owned(),
        "CREATE FUNCTION sqliso_escape AS (x) -> x".to_owned(),
        "CREATE DICTIONARY crucible.sqliso_dict (k UInt64, v String) PRIMARY KEY k SOURCE(CLICKHOUSE(TABLE \
         'sqliso_admin_only' DB 'crucible')) LAYOUT(FLAT()) LIFETIME(0)"
            .to_owned(),
        "DROP TABLE crucible.sql_probe".to_owned(),
        "DROP VIEW crucible_sql.spans".to_owned(),
        "DROP DATABASE crucible_sql".to_owned(),
        "RENAME TABLE crucible.sql_probe TO crucible.sql_probe2".to_owned(),
        "ATTACH TABLE crucible.sqliso_escape (x UInt8) ENGINE = Memory".to_owned(),
        "DETACH TABLE crucible.sql_probe".to_owned(),
        "EXCHANGE TABLES crucible.sql_probe AND crucible.span_core".to_owned(),
        "SYSTEM FLUSH LOGS".to_owned(),
        "SYSTEM DROP MARK CACHE".to_owned(),
        "SYSTEM RELOAD CONFIG".to_owned(),
        "SYSTEM SHUTDOWN".to_owned(),
        "SYSTEM STOP MERGES crucible.sql_probe".to_owned(),
        "SYSTEM STOP TTL MERGES crucible.span_core".to_owned(),
        "KILL QUERY WHERE 1".to_owned(),
        "KILL QUERY WHERE user = 'crucible_admin' SYNC".to_owned(),
        "KILL MUTATION WHERE 1".to_owned(),
        "CREATE USER sqliso_escape IDENTIFIED WITH no_password".to_owned(),
        format!("ALTER USER {a} IDENTIFIED WITH no_password"),
        format!("ALTER USER {a} SETTINGS readonly = 0"),
        "CREATE ROLE sqliso_escape".to_owned(),
        "GRANT SELECT ON *.* TO CURRENT_USER".to_owned(),
        "GRANT ALL ON crucible.* TO crucible_sql_reader".to_owned(),
        "REVOKE crucible_sql_reader FROM CURRENT_USER".to_owned(),
        "CREATE ROW POLICY sqliso_escape ON crucible.span_core AS PERMISSIVE FOR SELECT USING 1 TO ALL".to_owned(),
        "DROP ROW POLICY crucible_sql_static_restrictive ON crucible.span_core".to_owned(),
        format!(
            "ALTER ROW POLICY {} ON crucible.span_core USING 1",
            project_policy_name(iso.a, FORMS[1])
        ),
        "CREATE QUOTA sqliso_escape TO ALL".to_owned(),
        "DROP QUOTA crucible_sql_quota".to_owned(),
        "ALTER SETTINGS PROFILE crucible_sql_profile SETTINGS readonly = 0".to_owned(),
        "DROP SETTINGS PROFILE crucible_sql_profile".to_owned(),
        "CREATE NAMED COLLECTION sqliso_escape AS url = 'http://127.0.0.1'".to_owned(),
        "BACKUP TABLE crucible.span_core TO File('/tmp/sqliso')".to_owned(),
        "SELECT * FROM crucible.sql_probe INTO OUTFILE '/tmp/sqliso.tsv'".to_owned(),
        "USE system".to_owned(),
        "BEGIN TRANSACTION".to_owned(),
    ];
    for sql in &statements {
        iso.refused(sql).await?;
    }
    // Without a table the statement applies to the tables the caller may manage, which is none: it is
    // accepted and stops nothing.
    iso.attack("SYSTEM STOP MERGES").await?;
    iso.ctx
        .admin("OPTIMIZE TABLE crucible.sql_probe FINAL")
        .await?;
    let after = iso.catalogue().await?;
    expect(before == after, || {
        format!("the catalogue changed: {before:?} became {after:?}")
    })?;
    Ok(format!(
        "{} statements refused (inserts, mutations, DDL, temporary and attached objects, SYSTEM, KILL, users, \
         roles, grants, policies, quotas, profiles, backups, OUTFILE); the admin sees an unchanged catalogue",
        statements.len()
    ))
}

/// W6: every function the server lists, called with a few argument shapes.
async fn w6(iso: &Iso) -> Result<String> {
    let before = iso.catalogue().await?;
    let listed = iso
        .ctx
        .admin_text(
            "SELECT name, is_aggregate FROM system.functions WHERE origin = 'System' ORDER BY name",
        )
        .await?;
    let b = iso.b.uuid();
    let shapes = [
        String::new(),
        "1".to_owned(),
        "'crucible.sqliso_admin_only'".to_owned(),
        format!("toUUID('{b}')"),
        "'crucible', 'sqliso_admin_only', 'canary'".to_owned(),
        "1, 2".to_owned(),
    ];
    let (mut calls, mut answered) = (0u32, 0u32);
    for line in listed.lines() {
        let Some((name, _)) = line.split_once('\t') else {
            continue;
        };
        if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            continue;
        }
        for shape in &shapes {
            let reply = iso.attack(&format!("SELECT {name}({shape})")).await?;
            calls += 1;
            answered += u32::from(reply.rows().is_some());
        }
    }
    // The same over a table, so a function sees real rows of the caller.
    for name in [
        "any",
        "groupArray",
        "topK",
        "anyHeavy",
        "argMax",
        "uniqExact",
        "groupUniqArray",
    ] {
        iso.attack(&format!(
            "SELECT {name}(project_id) FROM crucible.sql_probe UNION ALL SELECT {name}(project_id) FROM \
             crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY"
        ))
        .await?;
    }
    iso.admin_ping().await?;
    let after = iso.catalogue().await?;
    expect(before == after, || {
        format!("the catalogue changed: {before:?} became {after:?}")
    })?;
    Ok(format!(
        "{} functions, {calls} calls with {} argument shapes ({answered} answered): no canary in any answer or \
         error, the catalogue is unchanged and the admin ping is healthy",
        listed.lines().count(),
        shapes.len()
    ))
}

/// W7: each resource guard trips its expected error while the admin ping stays healthy.
async fn w7(iso: &Iso) -> Result<String> {
    let profile = iso.profile;
    let code = async |sql: &str, expected: &[u32], what: &str| -> Result<()> {
        let code = iso.refused(sql).await?;
        expect(expected.contains(&code), || {
            format!("{what}: expected one of {expected:?}, got {code} for {sql:?}")
        })
    };
    code(
        "SELECT count() FROM numbers(100000000000)",
        &[158, 307],
        "max_rows_to_read",
    )
    .await?;
    code("SELECT sleep(3)", &[160], "sleep cap").await?;
    code(
        "SELECT sleepEachRow(1) FROM numbers(3)",
        &[160],
        "sleep cap",
    )
    .await?;
    let deep = format!("SELECT {}1{}", "(".repeat(400), ")".repeat(400));
    code(&deep, &[167, 168, 62, 306], "max_ast_depth").await?;
    let wide = format!("SELECT {}", vec!["1"; 20_000].join(" + "));
    code(&wide, &[168, 167, 62, 306], "max_ast_elements").await?;
    let mut nested = "SELECT 1".to_owned();
    for _ in 0..12 {
        nested = format!("SELECT * FROM ({nested})");
    }
    code(&nested, &[162], "max_subquery_depth").await?;
    let long = format!("SELECT '{}'", "x".repeat(200 * 1024));
    code(&long, &[62], "max_query_size").await?;
    code(
        "SELECT count() FROM numbers(12000000) AS x INNER JOIN numbers(12000000) AS y ON x.number = y.number",
        &[191, 158, 307, 241],
        "max_rows_in_join",
    )
    .await?;
    if profile.no_join_spill {
        iso.answers(
            "SELECT getSetting('max_bytes_ratio_before_external_join')",
            "0",
        )
        .await?;
    }
    code(
        "SELECT count() FROM (SELECT number FROM numbers(12000000) GROUP BY number)",
        &[158, 241],
        "max_rows_to_group_by",
    )
    .await?;
    code(
        "SELECT count() FROM numbers(12000000) WHERE number IN (SELECT number FROM numbers(12000000))",
        &[191, 241],
        "max_rows_in_set",
    )
    .await?;
    // The result cap cuts instead of failing.
    let reply = iso.attack("SELECT number FROM numbers(1000000)").await?;
    let rows = reply
        .rows()
        .ok_or_else(|| anyhow!("the capped result {}", reply.brief()))?
        .lines()
        .count();
    let cap = usize::try_from(profile.max_result_rows)?;
    expect(rows >= cap && rows < 200_000, || {
        format!("max_result_rows = {cap} with overflow mode break returned {rows} rows")
    })?;
    // The window backstops.
    code(
        "SELECT count() FROM crucible.span_core WHERE project_id != toUUID('00000000-0000-0000-0000-000000000000')",
        &[277],
        "force_index_by_date",
    )
    .await?;
    // The concurrency cap: with two statements running, a third is refused.
    let slow = "SELECT count() FROM numbers(900000000) WHERE sipHash64(number) = 1";
    let (first, second, third, fourth) = tokio::join!(
        iso.attack(slow),
        iso.attack(slow),
        iso.attack(slow),
        iso.attack(slow)
    );
    let replies = [first?, second?, third?, fourth?];
    let refused = replies.iter().filter(|r| r.code() == Some(202)).count();
    expect(refused >= 1, || {
        format!(
            "max_concurrent_queries_for_user = {}: four concurrent statements {}",
            profile.max_concurrent_queries_for_user,
            replies
                .iter()
                .map(Reply::brief)
                .collect::<Vec<_>>()
                .join("; ")
        )
    })?;
    // Memory and time are capped by the same constant profile; tripping them needs gigabytes or 30 s, so the
    // values are read back instead.
    for (setting, value) in [
        ("max_memory_usage", profile.max_memory_usage),
        (
            "max_memory_usage_for_user",
            profile.max_memory_usage_for_user,
        ),
        (
            "max_execution_time",
            u64::from(profile.max_execution_time_secs),
        ),
        ("max_bytes_to_read", profile.max_bytes_to_read),
        ("max_result_bytes", profile.max_result_bytes),
    ] {
        iso.answers(
            &format!("SELECT getSetting('{setting}')"),
            &value.to_string(),
        )
        .await?;
    }
    iso.admin_ping().await?;
    Ok(format!(
        "rows to read (158), sleep (160), AST depth and size, subquery depth (162), query size (62), join, \
         group-by and set caps, the result cap ({rows} rows returned for a 1,000,000-row result), \
         force_index_by_date (277) and the concurrency cap (202, {refused} of 4 refused) all trip; memory and \
         time limits read back at their profile values; the admin ping is healthy"
    ))
}

/// W8: quota exhaustion, on an identity of its own, at the real limits.
async fn w8(iso: &Iso) -> Result<String> {
    let project = iso.ctx.project();
    iso.provision(project).await?;
    let limit = SQL_QUOTA_QUERIES_PER_HOUR;
    let mut refused_at = None;
    for n in 1..=limit + 5 {
        // A statement that reads a table: the server does not count a constant `SELECT 1` against the quota.
        let reply = iso
            .send(project, "SELECT count() FROM crucible.sql_probe", &[])
            .await?;
        match reply.code() {
            None => {}
            Some(201) => {
                refused_at = Some(n);
                break;
            }
            Some(code) => bail!("statement {n} was refused with code {code}, not the quota"),
        }
    }
    let refused_at = refused_at.ok_or_else(|| {
        anyhow!(
            "{} statements ran without the quota refusing one",
            limit + 5
        )
    })?;
    expect(refused_at == limit + 1, || {
        format!(
            "the quota refused statement {refused_at}, expected {}",
            limit + 1
        )
    })?;
    // The quota is keyed by user: project A is not affected.
    iso.answers("SELECT 1", "1").await?;
    iso.sql
        .deprovision(project)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    Ok(format!(
        "statement {refused_at} of one identity is refused with code 201 after {limit} in the hour; another \
         identity still runs"
    ))
}

/// W9: what a read-only identity may still change. No profile element is `changeable_in_readonly`; whatever
/// the server itself leaves changeable must be on the presentation list, and none of it may show another
/// project's rows.
async fn w9(iso: &Iso) -> Result<String> {
    let writable = iso
        .ctx
        .admin_count(
            "SELECT count() FROM system.settings_profile_elements WHERE profile_name = 'crucible_sql_profile' AND \
             writability != 'CONST'",
        )
        .await?;
    expect(writable == 0, || {
        format!("{writable} profile elements are not CONST")
    })?;
    let reply = iso
        .attack("SELECT name FROM system.settings WHERE readonly = 0 ORDER BY name")
        .await?;
    let Some(rows) = reply.rows() else {
        return Ok("system.settings is not readable; every profile element is CONST".to_owned());
    };
    let changeable: Vec<&str> = rows.lines().filter(|line| !line.is_empty()).collect();
    for name in &changeable {
        expect(PRESENTATION_SETTINGS.contains(name), || {
            format!(
                "the setting {name} is changeable by a workspace identity and is not a presentation setting"
            )
        })?;
    }
    // The changeable ones, used against the caller's own table: they shape the answer and add no row.
    let b = iso.b.uuid();
    let read = "SELECT DISTINCT toString(project_id) FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY";
    let other = format!("project_id = toUUID('{b}')");
    for (name, value) in [
        ("filter", other.as_str()),
        ("filter", "1"),
        ("select", "*"),
        ("limit", "1000000"),
        ("offset", "0"),
        ("sort", "project_id"),
        ("implicit_table_at_top_level", "crucible.sqliso_admin_only"),
        ("implicit_table_at_top_level", "crucible.span_core"),
        ("database", "crucible"),
    ] {
        if !changeable.contains(&name) {
            continue;
        }
        let reply = iso.send(iso.a, read, &[(name, value)]).await?;
        expect(
            reply
                .rows()
                .is_none_or(|rows| rows.is_empty() || rows == iso.a.uuid().to_string()),
            || format!("with {name}={value} the read {}", reply.brief()),
        )?;
        iso.send(iso.a, "SELECT *", &[(name, value)]).await?;
    }
    Ok(format!(
        "every profile element is CONST; the server leaves {} settings changeable for a read-only identity ({}), \
         all of them presentation settings, and none shows another project's rows",
        changeable.len(),
        if changeable.is_empty() {
            "none".to_owned()
        } else {
            changeable.join(", ")
        }
    ))
}

/// W10: error-text oracles, EXPLAIN outputs and cache reuse across users.
async fn w10(iso: &Iso) -> Result<String> {
    let b = iso.b.uuid();
    let w = "start_time >= now() - INTERVAL 2 DAY";
    let mut statements = 0;
    // Errors that quote data quote the caller's rows only.
    for sql in [
        format!("SELECT toUInt64(name) FROM crucible.span_core WHERE {w}"),
        format!("SELECT throwIf(1, name) FROM crucible.span_core WHERE {w}"),
        format!(
            "SELECT throwIf(project_id = toUUID('{b}'), 'other project row seen') FROM crucible.span_core WHERE {w}"
        ),
        format!("SELECT toDate(name) FROM crucible.span_core WHERE {w}"),
        format!("SELECT CAST(name AS Enum8('x' = 1)) FROM crucible.span_core WHERE {w}"),
        format!(
            "SELECT JSONExtractString(name, 'x'), toUUID(name) FROM crucible.span_core WHERE {w}"
        ),
        format!("SELECT arrayJoin(range(toUInt64(name))) FROM crucible.span_core WHERE {w}"),
        format!("SELECT name FROM crucible.span_core WHERE {w} AND toUInt8(name) = 1"),
        format!("SELECT 1 FROM crucible.span_core PREWHERE toUInt8(name) = 1 WHERE {w}"),
    ] {
        iso.attack(&sql).await?;
        statements += 1;
    }
    let seen = iso
        .attack(&format!(
            "SELECT throwIf(project_id = toUUID('{b}'), 'other project row seen') FROM crucible.span_core WHERE {w}"
        ))
        .await?;
    expect(seen.rows().is_some(), || {
        format!(
            "a row of project B was evaluated for project A: {}",
            seen.brief()
        )
    })?;
    // EXPLAIN in every form, over a statement that asks for the other project.
    let target = format!(
        "SELECT name FROM crucible.span_core WHERE {w} AND project_id = toUUID('{b}') UNION ALL SELECT canary FROM \
         crucible.sqliso_admin_only"
    );
    let own = format!("SELECT name FROM crucible_sql.spans WHERE {w}");
    for kind in [
        "AST",
        "SYNTAX",
        "QUERY TREE",
        "PLAN",
        "PLAN indexes = 1, actions = 1",
        "PIPELINE",
        "ESTIMATE",
        "TABLE OVERRIDE",
        "CURRENT TRANSACTION",
    ] {
        iso.attack(&format!("EXPLAIN {kind} {target}")).await?;
        iso.attack(&format!("EXPLAIN {kind} {own}")).await?;
        statements += 2;
    }
    // The estimate of a project's own statement: what it shows of the table as a whole is recorded.
    let estimate = iso
        .attack(&format!(
            "EXPLAIN ESTIMATE SELECT count() FROM crucible.span_core WHERE {w}"
        ))
        .await?;
    // The query cache is off, so the same text answers each user with its own rows.
    let text = format!("SELECT DISTINCT toString(project_id) FROM crucible.span_core WHERE {w}");
    let first = iso.send(iso.b, &text, &[]).await;
    // B's own answer holds B's id, never a canary; the oracle of `send` guards A's side.
    expect(first.is_ok(), || {
        "project B could not read its own rows".to_owned()
    })?;
    iso.answers(&text, &iso.a.uuid().to_string()).await?;
    iso.refused(&format!("{text} SETTINGS use_query_cache = 1"))
        .await?;
    Ok(format!(
        "{statements} statements: conversion and throwIf errors quote project A's rows only, no row of B is \
         evaluated, EXPLAIN in nine forms shows no canary, and the same text answers A and B with their own \
         rows (the query cache is off and cannot be turned on). EXPLAIN ESTIMATE of an own-table read {}",
        estimate.brief()
    ))
}

/// W11: lifecycle races: the user before its policies, policies dropped, a retention change.
async fn w11(iso: &Iso) -> Result<String> {
    let project = iso.ctx.project();
    let user = workspace_user(project);
    let w = "start_time >= now() - INTERVAL 2 DAY";
    let count = format!("SELECT count() FROM crucible.span_core WHERE {w}");
    iso.ctx
        .admin(fixture_row(
            table(crucible_records::TableId::SpanCore),
            project,
            &iso.canary_a,
        ))
        .await?;
    // 1. The user exists and holds nothing yet: the first statement of provisioning alone.
    iso.ctx
        .admin(format!(
            "CREATE USER IF NOT EXISTS {user} IDENTIFIED WITH sha256_password BY 'sqliso-{user}-0' SETTINGS PROFILE \
             'crucible_sql_profile'"
        ))
        .await?;
    let reply = iso.send(project, &count, &[]).await?;
    expect(reply.code() == Some(497), || {
        format!("a user without the role {}", reply.brief())
    })?;
    // 2. The role without the project's policies: the static layer alone admits the user's own project and
    //    nothing else.
    iso.ctx
        .admin(format!("GRANT crucible_sql_reader TO {user}"))
        .await?;
    let reply = iso.send(project, &count, &[]).await?;
    let static_only = reply.rows().map(str::to_owned);
    expect(matches!(static_only.as_deref(), Some("0" | "1")), || {
        format!("with the static layer alone, the user {}", reply.brief())
    })?;
    let others = iso
        .send(
            project,
            &format!(
                "SELECT count() FROM crucible.span_core WHERE {w} AND project_id != toUUID('{}')",
                project.uuid()
            ),
            &[],
        )
        .await?;
    expect(others.rows() == Some("0"), || {
        format!(
            "with the static layer alone, other projects' rows {}",
            others.brief()
        )
    })?;
    // 3. Fully provisioned, then one layer dropped at a time: each layer alone still isolates.
    iso.provision(project).await?;
    let reply = iso.send(project, &count, &[]).await?;
    expect(reply.rows() == Some("1"), || {
        format!("a provisioned user {}", reply.brief())
    })?;
    for form in FORMS {
        iso.ctx
            .admin(format!(
                "DROP ROW POLICY IF EXISTS {} ON crucible.span_core",
                project_policy_name(project, form)
            ))
            .await?;
    }
    let reply = iso
        .send(
            project,
            &format!(
                "SELECT count() FROM crucible.span_core WHERE {w} AND project_id != toUUID('{}')",
                project.uuid()
            ),
            &[],
        )
        .await?;
    expect(reply.rows() == Some("0"), || {
        format!(
            "with the project's policies dropped, other projects' rows {}",
            reply.brief()
        )
    })?;
    // 4. A malformed user name in the role matches no row.
    iso.ctx
        .admin(
            "CREATE USER IF NOT EXISTS crucible_sql_notahexid IDENTIFIED WITH sha256_password BY 'sqliso-x' SETTINGS \
             PROFILE 'crucible_sql_profile'",
        )
        .await?;
    iso.ctx
        .admin("GRANT crucible_sql_reader TO crucible_sql_notahexid")
        .await?;
    let reply = iso
        .ctx
        .store
        .raw_as(
            "crucible_sql_notahexid",
            "sqliso-x",
            &count,
            &[],
            Duration::from_secs(30),
        )
        .await?;
    iso.ctx
        .admin("DROP USER IF EXISTS crucible_sql_notahexid")
        .await?;
    expect(
        matches!(&reply, crucible_store_clickhouse::RawReply::Rows(text) if text.trim() == "0"),
        || format!("a user with a malformed name read {reply:?}"),
    )?;
    // 5. A retention change hides old rows at once.
    iso.ctx
        .admin(format!(
            "INSERT INTO crucible.span_core (project_id, start_time, name) SELECT toUUID('{}'), now() - INTERVAL 40 \
             DAY, 'old'",
            project.uuid()
        ))
        .await?;
    let all = "SELECT count() FROM crucible.span_core WHERE start_time >= now() - INTERVAL 60 DAY";
    iso.provision(project).await?;
    let before = iso.send(project, all, &[]).await?;
    iso.sql
        .provision(
            project,
            0,
            &RetentionSettings {
                retention_days: Some(30),
                payload_retention_days: None,
                mcp_content_retention_days: None,
            },
        )
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let after = iso.send(project, all, &[]).await?;
    expect(
        before.rows() == Some("2") && after.rows() == Some("1"),
        || {
            format!(
                "retention: before the change the read {}, after it {}",
                before.brief(),
                after.brief()
            )
        },
    )?;
    // 6. Deprovisioned: the identity is gone and its policies with it.
    iso.sql
        .deprovision(project)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    iso.sql
        .deprovision(project)
        .await
        .map_err(|e| anyhow!("{e}"))?;
    let gone = iso
        .ctx
        .store
        .raw_as(
            &user,
            &format!("sqliso-{user}-0"),
            "SELECT 1",
            &[],
            Duration::from_secs(30),
        )
        .await?;
    expect(
        matches!(
            gone,
            crucible_store_clickhouse::RawReply::Refused {
                code: 516 | 192,
                ..
            }
        ),
        || format!("a deprovisioned identity got {gone:?}"),
    )?;
    let left = iso
        .ctx
        .admin_count(format!(
            "SELECT count() FROM system.row_policies WHERE startsWith(short_name, '{user}')"
        ))
        .await?;
    expect(left == 0, || {
        format!("{left} policies outlive the identity")
    })?;
    Ok(format!(
        "a user without the role is refused (497); with the role and the static layer alone it reads {} of its \
         own rows and none of another project; with the project's policies dropped it still reads no other \
         project; a malformed user name reads nothing; a retention change from unset to 30 days hides a 40-day-old \
         row at once; deprovisioning twice removes the user and every policy",
        static_only.unwrap_or_default()
    ))
}

/// W12: `currentUser()` inside a definer-rights view. It decides whether the grant set can move from base
/// tables to views (contract 7.4); v0 ships base-table grants either way.
async fn w12(iso: &Iso) -> Result<String> {
    let admin = &iso.ctx.config.admin.user;
    let created = iso
        .ctx
        .admin(format!(
            "CREATE OR REPLACE VIEW crucible_sql.sqliso_definer DEFINER = {admin} SQL SECURITY DEFINER AS SELECT \
             currentUser() AS who, count() AS rows FROM crucible.sql_probe"
        ))
        .await;
    if let Err(error) = created {
        return Ok(format!(
            "this server does not create the definer-rights view ({error:#}); base-table grants stay"
        ));
    }
    let reply = iso
        .attack("SELECT who, rows FROM crucible_sql.sqliso_definer")
        .await;
    iso.ctx
        .admin("DROP VIEW IF EXISTS crucible_sql.sqliso_definer")
        .await?;
    let reply = reply?;
    let observed = match reply.rows() {
        Some(rows) => {
            let (who, rows) = rows.split_once('\t').unwrap_or((rows, "?"));
            let caller = who == workspace_user(iso.a);
            format!(
                "currentUser() inside the view is the {} and the view counts {rows} probe rows",
                if caller { "caller" } else { "definer" }
            )
        }
        None => format!("the read {}", reply.brief()),
    };
    Ok(format!(
        "{observed}; v0 keeps base-table grants (contract 7.4)"
    ))
}

/// W13: the window backstops through the curated views.
async fn w13(iso: &Iso) -> Result<String> {
    let profile = iso.profile;
    let (mut enforced, mut free) = (Vec::new(), Vec::new());
    for view in &CURATED_VIEWS {
        let def = table(view.table);
        let name = format!("crucible_sql.{}", view.name);
        let narrow = iso
            .attack(&format!("SELECT count() FROM {name} WHERE {}", window(def)))
            .await?;
        expect(narrow.rows().is_some(), || {
            format!("a windowed read of {name} {}", narrow.brief())
        })?;
        let open = iso.attack(&format!("SELECT count() FROM {name}")).await?;
        match (def.partition.column(), open.code()) {
            (Some(_), Some(277)) => enforced.push(view.name),
            (None, None) => free.push(view.name),
            (partition, _) => bail!(
                "{name} (partition column {partition:?}) without a window {}",
                open.brief()
            ),
        }
    }
    // The partition cap, on a table with day partitions.
    iso.ctx
        .admin(format!(
            "INSERT INTO crucible.span_core (project_id, start_time, name) SELECT toUUID('{}'), now() - \
             toIntervalDay(number), 'spread' FROM numbers({}) SETTINGS max_partitions_per_insert_block = 0",
            iso.a.uuid(),
            profile.max_partitions_to_read + 20
        ))
        .await?;
    let wide = iso
        .attack(
            "SELECT count() FROM crucible_sql.spans WHERE start_time >= now() - INTERVAL 400 DAY",
        )
        .await?;
    expect(wide.code() == Some(565), || {
        format!(
            "a 400-day window over {} day partitions {}",
            profile.max_partitions_to_read + 20,
            wide.brief()
        )
    })?;
    let month = iso
        .attack("SELECT count() >= 30 FROM crucible_sql.spans WHERE start_time >= now() - INTERVAL 30 DAY")
        .await?;
    expect(month.rows() == Some("1"), || {
        format!("a 30-day window {}", month.brief())
    })?;
    iso.ctx
        .admin(format!(
            "ALTER TABLE crucible.span_core DELETE WHERE project_id = toUUID('{}') AND name = 'spread' SETTINGS \
             mutations_sync = 2",
            iso.a.uuid()
        ))
        .await?;
    Ok(format!(
        "force_index_by_date refuses an unwindowed read (277) through {} views over partitioned tables ({}); {} \
         views over unpartitioned tables need no window ({}); max_partitions_to_read = {} refuses a 400-day window \
         (565) and admits a 30-day one",
        enforced.len(),
        enforced.join(", "),
        free.len(),
        free.join(", "),
        profile.max_partitions_to_read
    ))
}

/// W14: many identities: provisioning latency and the cost of the policies on a read.
async fn w14(iso: &Iso, identities: u32) -> Result<String> {
    let clock = &iso.ctx.env.clock;
    let read = "SELECT count() FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY";
    let time = async |n: u32| -> Result<Duration> {
        let start = clock.now();
        for _ in 0..n {
            iso.answers(read, "1").await?;
        }
        Ok(clock.now().saturating_duration_since(start) / n)
    };
    let before = time(20).await?;
    let projects: Vec<ProjectId> = (0..identities).map(|_| iso.ctx.project()).collect();
    let start = clock.now();
    for project in &projects {
        iso.provision(*project).await?;
    }
    let provisioning = clock.now().saturating_duration_since(start);
    let policies = iso
        .ctx
        .admin_count("SELECT count() FROM system.row_policies WHERE database = 'crucible'")
        .await?;
    let after = time(20).await?;
    // Isolation holds with the crowd present.
    iso.answers(
        &format!("{read} AND project_id != toUUID('{}')", iso.a.uuid()),
        "0",
    )
    .await?;
    let start = clock.now();
    for project in &projects {
        iso.sql
            .deprovision(*project)
            .await
            .map_err(|e| anyhow!("{e}"))?;
    }
    let deprovisioning = clock.now().saturating_duration_since(start);
    let mut detail = String::new();
    let _ = write!(
        detail,
        "{identities} identities provisioned in {:.1} s ({:.0} ms each) and dropped in {:.1} s; with {policies} \
         row policies present a read as project A takes {:.1} ms against {:.1} ms before",
        provisioning.as_secs_f64(),
        provisioning.as_secs_f64() * 1000.0 / f64::from(identities.max(1)),
        deprovisioning.as_secs_f64(),
        after.as_secs_f64() * 1000.0,
        before.as_secs_f64() * 1000.0,
    );
    if identities < 2000 {
        let _ = write!(
            detail,
            " (the contract's 2,000-identity run is `--identities 2000`, scheduled nightly)"
        );
    }
    Ok(detail)
}

/// W15: Cloud specifics. They need a ClickHouse Cloud service, which this run does not have.
fn w15(iso: &Iso) -> String {
    format!(
        "not run: password rules, per-replica entity visibility and `compatibility` variations need a ClickHouse \
         Cloud target; this target is {:?} with the server switch {:?}",
        iso.ctx.flavour, iso.switch
    )
}

/// The scripts that judge isolation itself, without the long ones: layer D runs them against each broken
/// deployment.
pub(crate) async fn detectors(iso: &Iso) -> Vec<(&'static str, Result<String>)> {
    vec![
        ("W0 deployment as declared", iso.deployment().await),
        ("Runtime self-probe (7.5)", iso.runtime_probe().await),
        ("W1 cross-project reads", w1(iso).await),
        ("W2 table functions", w2(iso).await),
        ("W3 system tables", w3(iso).await),
        ("W4 setting changes", w4(iso).await),
        ("W5 writes and DDL", w5(iso).await),
    ]
}

/// Layer A in order. W8 runs first, at the real quota; the quota is lifted after it.
pub(crate) async fn run(iso: &Iso, options: &Options) -> Vec<(&'static str, Result<String>)> {
    let mut rows = vec![
        ("W8 quota exhaustion", w8(iso).await),
        ("Quota lifted for the run", lift_quota(iso).await),
        ("W1 cross-project reads", w1(iso).await),
        ("W2 table functions", w2(iso).await),
        ("W3 system tables", w3(iso).await),
        ("W4 setting changes", w4(iso).await),
        ("W5 writes and DDL", w5(iso).await),
        ("W6 every function", w6(iso).await),
        ("W7 resource guards", w7(iso).await),
        ("W9 changeable_in_readonly", w9(iso).await),
        ("W10 error text, EXPLAIN and caches", w10(iso).await),
        ("W11 lifecycle races", w11(iso).await),
        ("W12 definer-rights views", w12(iso).await),
        ("W13 window backstops through views", w13(iso).await),
        ("W14 many identities", w14(iso, options.identities).await),
    ];
    rows.push(("W15 Cloud specifics", Ok(w15(iso))));
    rows
}
