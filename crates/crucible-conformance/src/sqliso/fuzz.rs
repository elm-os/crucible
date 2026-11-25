//! Layer B: grammar fuzzing with canary oracles (contract 7.5).
//!
//! A seeded generator writes statements from a small grammar of the server's `SELECT` surface over the base
//! tables, the curated views, table functions and system tables; every third statement is a mutation of an
//! escape-attempt corpus. Each statement is sent as project A's identity. The oracle is the one of
//! [`Iso::send`]: no canary of project B or of the admin in any answer or error text. The admin's view of the
//! catalogue must be the same after the run as before it.
//!
//! No generated statement holds a canary itself (errors quote the statement), so searches use the canaries'
//! shared prefix `cnry_`.

use anyhow::Result;
use crucible_schema::{CURATED_VIEWS, TableDef, Variant, sql_tables, workspace_user};

use super::{Iso, canary_column, window};
use crate::expect;

/// SplitMix64: the suite's own generator, so a seed replays a run exactly.
pub(crate) struct Prng(u64);

impl Prng {
    pub(crate) const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n.max(1)).unwrap_or(1)).unwrap_or(0)
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn pick<'a, T: ?Sized>(&mut self, items: &[&'a T]) -> &'a T {
        items[self.below(items.len())]
    }
}

/// Statements that try to leave the project by a known route. The mutator rewrites them.
const CORPUS: &[&str] = &[
    "SELECT * FROM crucible.sqliso_admin_only",
    "SELECT canary FROM crucible.sqliso_admin_only WHERE canary LIKE 'cnry_%'",
    "SELECT * FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT name FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY AND project_id = toUUID('B')",
    "SELECT name FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY OR 1",
    "SELECT name FROM crucible.span_core PREWHERE 1 WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT name FROM crucible.span_core FINAL WHERE start_time >= now() - INTERVAL 2 DAY SETTINGS force_index_by_date = 0",
    "SELECT * FROM merge('crucible', '.*') WHERE project_id = toUUID('B')",
    "SELECT * FROM merge(REGEXP('cru.*'), 'span.*')",
    "SELECT * FROM view(SELECT * FROM crucible.sqliso_admin_only)",
    "SELECT * FROM remote('127.0.0.1', 'crucible', 'span_core')",
    "SELECT * FROM remote('127.0.0.1:9000', crucible.sqliso_admin_only, 'crucible_admin', 'conformance-admin')",
    "SELECT * FROM cluster('default', crucible.span_core)",
    "SELECT * FROM clusterAllReplicas('default', system.query_log)",
    "SELECT * FROM url('http://127.0.0.1:8123/?query=SELECT+*+FROM+crucible.sqliso_admin_only&user=crucible_admin&password=conformance-admin', 'LineAsString', 'l String')",
    "SELECT * FROM file('/var/lib/clickhouse/access/users.list', 'LineAsString', 'l String')",
    "SELECT * FROM mergeTreeIndex('crucible', 'span_core')",
    "SELECT * FROM mergeTreeIndex('crucible', 'sessions_rollup', with_marks = true)",
    "SELECT * FROM loop(crucible.span_core) LIMIT 5",
    "SELECT query FROM system.query_log WHERE query LIKE '%cnry_%'",
    "SELECT query FROM system.processes",
    "SELECT * FROM system.query_thread_log LIMIT 10",
    "SELECT * FROM system.text_log LIMIT 10",
    "SELECT * FROM system.users",
    "SELECT * FROM system.row_policies",
    "SELECT * FROM system.grants",
    "SELECT * FROM system.parts WHERE database = 'crucible'",
    "SELECT * FROM system.parts_columns WHERE database = 'crucible'",
    "SELECT * FROM system.tables WHERE database = 'crucible'",
    "SELECT * FROM system.columns WHERE database = 'crucible'",
    "SELECT * FROM system.data_skipping_indices",
    "SELECT * FROM system.projection_parts",
    "SELECT * FROM system.dictionaries",
    "SELECT * FROM system.zookeeper WHERE path = '/'",
    "SELECT * FROM information_schema.tables",
    "SHOW TABLES FROM crucible",
    "SHOW CREATE TABLE crucible.span_core",
    "SHOW CREATE TABLE crucible.sqliso_admin_only",
    "SHOW CREATE USER crucible_admin",
    "SHOW CREATE ROW POLICY crucible_sql_static_permissive ON crucible.span_core",
    "SHOW GRANTS",
    "SHOW GRANTS FOR crucible_admin",
    "SHOW ROW POLICIES",
    "SHOW PROCESSLIST",
    "SHOW ACCESS",
    "DESCRIBE TABLE crucible.sqliso_admin_only",
    "DESCRIBE (SELECT * FROM crucible.sqliso_admin_only)",
    "EXISTS TABLE crucible.sqliso_admin_only",
    "EXPLAIN SYNTAX SELECT * FROM crucible.sqliso_admin_only",
    "EXPLAIN PLAN indexes = 1 SELECT * FROM crucible.span_core WHERE project_id = toUUID('B')",
    "EXPLAIN ESTIMATE SELECT * FROM crucible.sqliso_admin_only",
    "EXPLAIN QUERY TREE SELECT canary FROM crucible.sqliso_admin_only",
    "SET readonly = 0",
    "SET ROLE ALL",
    "SET additional_table_filters = {'crucible.span_core': '1'}",
    "SELECT 1 SETTINGS readonly = 0; SELECT * FROM crucible.sqliso_admin_only",
    "SELECT 1; SELECT * FROM crucible.sqliso_admin_only",
    "SELECT 1 /* */; DROP ROW POLICY crucible_sql_static_restrictive ON crucible.span_core",
    "EXECUTE AS crucible_admin SELECT * FROM crucible.sqliso_admin_only",
    "SELECT * FROM crucible.span_core SETTINGS additional_result_filter = '1', force_index_by_date = 0",
    "SELECT dictGet('crucible.sqliso_admin_only', 'canary', 1)",
    "SELECT joinGet('crucible.sqliso_admin_only', 'canary', 1)",
    "SELECT * FROM (SELECT * FROM crucible.sqliso_admin_only)",
    "WITH x AS (SELECT * FROM crucible.sqliso_admin_only) SELECT * FROM x",
    "SELECT (SELECT groupArray(canary) FROM crucible.sqliso_admin_only)",
    "SELECT 1 WHERE EXISTS (SELECT 1 FROM crucible.sqliso_admin_only WHERE canary LIKE 'cnry_admin_%')",
    "SELECT 1 FROM system.one WHERE (SELECT count() FROM crucible.span_core WHERE project_id = toUUID('B')) > 0",
    "SELECT throwIf(count() > 0, 'seen') FROM crucible.span_core WHERE project_id = toUUID('B')",
    "SELECT * FROM crucible.span_core AS s INNER JOIN crucible.sqliso_admin_only AS a ON s.project_id = a.project_id",
    "SELECT * FROM crucible.span_core AS s ANY LEFT JOIN crucible.span_payload AS p USING (project_id) WHERE s.start_time >= now() - INTERVAL 2 DAY AND p.start_time >= now() - INTERVAL 2 DAY",
    "SELECT currentUser(), currentRoles(), currentProfiles(), enabledRoles(), user()",
    "SELECT getSetting('readonly'), getServerSetting('users_without_row_policies_can_read_rows')",
    "SELECT * FROM crucible.span_core INTO OUTFILE '/tmp/x' FORMAT TSV",
    "SELECT * FROM crucible.span_core FORMAT Native",
    "SELECT initialQueryID(), queryID(), hostName(), serverUUID(), version()",
    "SELECT * FROM crucible_sql.spans WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT * FROM crucible_sql.sessions",
    "SELECT * FROM crucible_sql.traces AS t, crucible_sql.spans AS s WHERE t.start_time >= now() - INTERVAL 2 DAY AND s.start_time >= now() - INTERVAL 2 DAY",
    "INSERT INTO crucible.sql_probe SELECT * FROM crucible.sql_probe",
    "KILL QUERY WHERE user = 'crucible_admin'",
    "SYSTEM FLUSH LOGS",
    "CREATE TEMPORARY TABLE t AS SELECT * FROM crucible.sqliso_admin_only",
    "ATTACH TABLE crucible.sqliso_admin_only",
    "SELECT * FROM crucible.`sqliso_admin_only`",
    "SELECT * FROM `crucible`.`span_core` WHERE 1 = 1 AND start_time >= now() - INTERVAL 2 DAY",
    "SELECT * FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY AND project_id IN (SELECT project_id FROM crucible.sqliso_admin_only)",
    "SELECT * FROM crucible.span_core ARRAY JOIN [1, 2] AS x WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT _part, _partition_id, _part_offset, _row_exists, _table FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT * FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY LIMIT 1 BY project_id",
    "SELECT project_id, count() FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY GROUP BY project_id WITH TOTALS",
    "SELECT project_id, count() FROM crucible.span_core WHERE start_time >= now() - INTERVAL 2 DAY GROUP BY project_id WITH ROLLUP",
    "SELECT DISTINCT ON (project_id) * FROM crucible_sql.spans WHERE start_time >= now() - INTERVAL 2 DAY",
    "SELECT count() OVER (PARTITION BY project_id) FROM crucible_sql.spans WHERE start_time >= now() - INTERVAL 2 DAY",
];

/// Scalar expressions for select lists and conditions.
const EXPRESSIONS: &[&str] = &[
    "*",
    "count()",
    "project_id",
    "toString(project_id)",
    "uniqExact(project_id)",
    "groupUniqArray(project_id)",
    "any(project_id)",
    "min(project_id), max(project_id)",
    "_part",
    "_partition_id",
    "_table",
    "currentUser()",
    "1",
    "now()",
    "rowNumberInAllBlocks()",
    "cityHash64(*)",
    "toJSONString(tuple(*))",
    "COLUMNS('.*')",
    "* EXCEPT (project_id)",
    "arrayJoin([1, 2])",
];

const SYSTEM_TABLES: &[&str] = &[
    "system.one",
    "system.numbers LIMIT 3",
    "system.tables",
    "system.columns",
    "system.parts",
    "system.query_log",
    "system.processes",
    "system.users",
    "system.row_policies",
    "system.settings",
    "system.functions",
    "system.databases",
    "information_schema.columns",
];

const SETTINGS: &[&str] = &[
    "readonly = 0",
    "force_index_by_date = 0",
    "max_rows_to_read = 0",
    "additional_table_filters = {'crucible.span_core': '1'}",
    "allow_experimental_analyzer = 0",
    "enable_analyzer = 0",
    "optimize_trivial_count_query = 1",
    "use_query_cache = 1",
    "max_threads = 1",
    "optimize_move_to_prewhere = 0",
    "query_plan_optimize_prewhere = 0",
    "row_policy_filter = 0",
];

struct Grammar<'a> {
    rng: Prng,
    tables: Vec<&'static TableDef>,
    a: String,
    b: String,
    other_user: &'a str,
    /// The cluster that the corpus's remote table functions name: the target's own, or `default`.
    cluster: &'a str,
}

impl Grammar<'_> {
    /// A source, and the table behind it when it is a base table or a curated view.
    fn source(&mut self, depth: usize) -> (String, Option<&'static TableDef>) {
        let def = self.tables[self.rng.below(self.tables.len())];
        match self.rng.below(14) {
            0..=4 => (format!("crucible.{}", def.name()), Some(def)),
            5..=8 => {
                let view = CURATED_VIEWS[self.rng.below(CURATED_VIEWS.len())];
                (
                    format!("crucible_sql.{}", view.name),
                    Some(crucible_schema::table(view.table)),
                )
            }
            9 => (format!("merge('crucible', '^{}$')", def.name()), Some(def)),
            10 => (self.rng.pick(SYSTEM_TABLES).to_owned(), None),
            11 => (
                self.rng
                    .pick(&[
                        "crucible.sqliso_admin_only",
                        "crucible.dedup_selftest",
                        "numbers(3)",
                        "remote('127.0.0.1', 'crucible', 'span_core')",
                        "url('http://127.0.0.1:8123/ping', 'LineAsString', 'l String')",
                    ])
                    .to_owned(),
                None,
            ),
            _ if depth < 3 => (format!("({})", self.select(depth + 1)), None),
            _ => (format!("crucible.{}", def.name()), Some(def)),
        }
    }

    fn condition(&mut self, def: Option<&TableDef>, depth: usize) -> String {
        let mut terms = Vec::new();
        if let Some(def) = def
            && self.rng.chance(90)
        {
            terms.push(window(def));
        }
        if def.is_some() && self.rng.chance(50) {
            let b = &self.b;
            let a = &self.a;
            let term = match self.rng.below(9) {
                0 => format!("project_id = toUUID('{b}')"),
                1 => format!("project_id != toUUID('{a}')"),
                2 => format!("project_id IN (toUUID('{a}'), toUUID('{b}'))"),
                3 => format!("toString(project_id) LIKE '{}%'", &b[..8]),
                4 => "1".to_owned(),
                5 => format!("NOT (project_id = toUUID('{a}'))"),
                6 => format!("project_id > toUUID('{a}') OR project_id < toUUID('{a}')"),
                7 if depth < 3 => format!(
                    "project_id IN (SELECT project_id FROM ({}))",
                    self.select(depth + 1)
                ),
                _ => format!("currentUser() != '{}'", self.other_user),
            };
            terms.push(term);
        }
        if terms.is_empty() {
            return String::new();
        }
        let joiner = if self.rng.chance(15) { " OR " } else { " AND " };
        format!(" WHERE {}", terms.join(joiner))
    }

    fn select(&mut self, depth: usize) -> String {
        let (source, def) = self.source(depth);
        let base = source.starts_with("crucible.") || source.starts_with("merge(");
        let expressions = match def {
            // The table's canary column: B's canary would show here if a policy failed.
            Some(def) if base && self.rng.chance(50) => match canary_column(def) {
                Some(column) => format!(
                    "count(), any({column}), groupUniqArray({column}), uniqExact(project_id)"
                ),
                None => "count(), uniqExact(project_id)".to_owned(),
            },
            Some(_) => self.rng.pick(EXPRESSIONS).to_owned(),
            None => self.rng.pick(&["*", "count()", "1"]).to_owned(),
        };
        let mut sql = format!("SELECT {expressions} FROM {source}");
        if self.rng.chance(6) {
            sql.push_str(" FINAL");
        }
        if self.rng.chance(4) {
            sql.push_str(" SAMPLE 0.5");
        }
        if def.is_some() && self.rng.chance(10) {
            let prewhere = self.rng.pick(&[
                "1",
                "project_id != toUUID('00000000-0000-0000-0000-000000000000')",
            ]);
            sql.push_str(&format!(" PREWHERE {prewhere}"));
        }
        sql.push_str(&self.condition(def, depth));
        if expressions == "project_id" && self.rng.chance(40) {
            sql.push_str(" GROUP BY project_id");
            if self.rng.chance(30) {
                sql.push_str(" WITH TOTALS");
            }
        }
        if self.rng.chance(15) {
            sql.push_str(" ORDER BY 1");
        }
        if self.rng.chance(40) {
            sql.push_str(&format!(" LIMIT {}", 1 + self.rng.below(50)));
        }
        if depth == 0 && self.rng.chance(10) {
            sql.push_str(&format!(" SETTINGS {}", self.rng.pick(SETTINGS)));
        }
        if depth < 2 && self.rng.chance(8) {
            // Both sides count, so the set operation is valid whatever the sources are.
            let op = self
                .rng
                .pick(&["UNION ALL", "UNION DISTINCT", "INTERSECT", "EXCEPT"]);
            let other = self.select(depth + 1);
            sql = format!("SELECT count() FROM ({sql}) {op} SELECT count() FROM ({other})");
        }
        if depth < 2 && self.rng.chance(8) {
            let kind = self.rng.pick(&[
                "INNER JOIN",
                "LEFT JOIN",
                "FULL JOIN",
                "ANY LEFT JOIN",
                "SEMI LEFT JOIN",
            ]);
            let table = self.tables[self.rng.below(self.tables.len())];
            sql = format!(
                "SELECT count(), uniqExact(l.project_id), uniqExact(r.project_id) FROM (SELECT project_id FROM \
                 crucible.{} WHERE {}) AS l {kind} (SELECT project_id FROM crucible.{} WHERE {} OR project_id = \
                 toUUID('{}')) AS r ON l.project_id = r.project_id OR r.project_id = toUUID('{}')",
                table.name(),
                window(table),
                table.name(),
                window(table),
                self.b,
                self.b
            );
        }
        sql
    }

    fn statement(&mut self) -> String {
        let select = self.select(0);
        match self.rng.below(20) {
            0 => format!(
                "EXPLAIN {} {select}",
                self.rng.pick(&[
                    "AST",
                    "SYNTAX",
                    "PLAN",
                    "PIPELINE",
                    "ESTIMATE",
                    "QUERY TREE"
                ])
            ),
            1 => format!("DESCRIBE ({select})"),
            2 => format!("WITH q AS ({select}) SELECT * FROM q"),
            3 => format!("SELECT * FROM view({select})"),
            4 => format!("SELECT count() FROM ({select})"),
            _ => select,
        }
    }

    /// A corpus statement, rewritten: the placeholder project replaced, and then a few random edits.
    fn mutation(&mut self) -> String {
        let base = CORPUS[self.rng.below(CORPUS.len())]
            .replace("toUUID('B')", &format!("toUUID('{}')", self.b))
            .replace("('default',", &format!("('{}',", self.cluster));
        let mut tokens: Vec<String> = base.split(' ').map(str::to_owned).collect();
        for _ in 0..self.rng.below(4) {
            let at = self.rng.below(tokens.len());
            match self.rng.below(9) {
                0 => tokens[at] = tokens[at].to_uppercase(),
                1 => tokens[at] = tokens[at].to_lowercase(),
                2 => tokens.insert(at, "/* x */".to_owned()),
                3 => {
                    let token = tokens[at].clone();
                    tokens.insert(at, token);
                }
                4 if tokens.len() > 3 => {
                    tokens.remove(at);
                }
                5 => tokens[at] = tokens[at].replace("crucible.", "crucible_sql."),
                6 => tokens[at] = tokens[at].replace("span_core", "sqliso_admin_only"),
                7 => tokens.push(format!("SETTINGS {}", self.rng.pick(SETTINGS))),
                _ => {
                    let other = self.rng.below(tokens.len());
                    tokens.swap(at, other);
                }
            }
        }
        let mut sql = tokens.join(" ");
        if self.rng.chance(10) {
            let tail = self.select(2);
            sql = format!("{sql} UNION ALL {tail}");
        }
        sql
    }
}

/// Sends `statements` generated statements as project A.
pub(crate) async fn run(iso: &Iso, statements: u32, seed: u64) -> Result<String> {
    let before = iso.catalogue().await?;
    let other_user = workspace_user(iso.b);
    let cluster = match &iso.ctx.variant {
        Variant::Clustered { cluster } => cluster.as_str(),
        Variant::SingleNode | Variant::Cloud => "default",
    };
    let mut grammar = Grammar {
        rng: Prng::new(seed),
        tables: sql_tables().collect(),
        a: iso.a.uuid().to_string(),
        b: iso.b.uuid().to_string(),
        other_user: &other_user,
        cluster,
    };
    let (mut answered, mut refused, mut rows) = (0u32, 0u32, 0usize);
    for n in 0..statements {
        let sql = if n % 3 == 2 {
            grammar.mutation()
        } else {
            grammar.statement()
        };
        debug_assert!(!sql.contains(&iso.canary_b) && !sql.contains(&iso.canary_admin));
        let reply = iso.attack(&sql).await?;
        match reply.rows() {
            Some(text) => {
                answered += 1;
                rows += text.lines().count();
            }
            None => refused += 1,
        }
        if n % 1000 == 999 {
            iso.admin_ping().await?;
        }
    }
    let after = iso.catalogue().await?;
    expect(before == after, || {
        format!("the catalogue changed: {before:?} became {after:?}")
    })?;
    // The generator is not vacuous: a good share of its statements read rows.
    expect(answered * 10 >= statements, || {
        format!("only {answered} of {statements} statements answered; the grammar is too narrow")
    })?;
    Ok(format!(
        "seed {seed}: {statements} statements as project A ({answered} answered with {rows} rows in total, \
         {refused} refused), a third of them mutations of a {}-statement escape corpus; no canary of project B or \
         of the admin in any answer or error text, and the admin sees an unchanged catalogue",
        CORPUS.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_generator_is_deterministic_per_seed() {
        let statements = |seed: u64| {
            let mut grammar = Grammar {
                rng: Prng::new(seed),
                tables: sql_tables().collect(),
                a: "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b".to_owned(),
                b: "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1c".to_owned(),
                other_user: "crucible_sql_x",
                cluster: "default",
            };
            (0..200)
                .map(|n| {
                    if n % 3 == 2 {
                        grammar.mutation()
                    } else {
                        grammar.statement()
                    }
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(statements(7), statements(7));
        assert_ne!(statements(7), statements(8));
        // No generated statement can hold a canary: canaries are random tokens with the prefix `cnry_<role>_`
        // followed by 32 hex characters, and the grammar only ever writes the prefix.
        for sql in statements(7) {
            assert!(
                !sql.contains("cnry_b_0") && !sql.contains("cnry_admin_0"),
                "{sql}"
            );
        }
    }
}
