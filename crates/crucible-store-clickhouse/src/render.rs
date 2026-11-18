//! The renderer from the typed forms of `crucible-plan` to SQL (contract 8.3). It is a pure function.
//!
//! - Every value is a server-side parameter `{pN:Type}`; no value is formatted into the SQL text. The statement is
//!   sent with client-side binding disabled.
//! - Identifiers (tables, columns, functions) come only from the schema and from this module.
//! - The SQL text depends on the plan's shape and on list arity only; lists are padded to a power of two.
//! - Every table reference carries `project_id = {scope}` and the window predicates, both added here: a plan
//!   cannot name `project_id`, and the window is the plan's.
//! - No statement contains `FINAL`. Aggregating tables are read through key-column grouping.
//!
//! # Values
//!
//! | Literal | Placeholder | Text |
//! |---|---|---|
//! | `Uuid` | `{pN:UUID}` | hyphenated lowercase |
//! | `Bytes` (a `FixedString`) | `unhex({pN:String})` | lowercase hex, so any byte survives |
//! | `String` | `{pN:String}` | the string, with backslash, tab, line feed, carriage return and NUL escaped |
//! | `UInt`, `Int` | `{pN:<column type>}` | decimal |
//! | `Float` | `{pN:Float64}` | shortest decimal, `nan`, `inf`, `-inf` |
//! | `Time` | `{pN:DateTime64(6, 'UTC')}` or `{pN:DateTime('UTC')}` | Unix seconds, with six fraction digits for the former |
//! | `Date` | `{pN:Date}` | `YYYY-MM-DD` |
//! | `Enum` | `{pN:String}` | the variant's name, which the server converts to the column's enum |
//!
//! # Window predicates
//!
//! The window `[from, to)` becomes, per table, a predicate on the table's time column (the column its retention
//! is bound to, which is also the column its partition expression reads, so the predicate prunes partitions):
//!
//! | Time column | Predicate |
//! |---|---|
//! | `DateTime64` (`start_time`, `target_time`) | `t >= from AND t < to` |
//! | `DateTime` (`minute`) | `t >= minute(from) AND t <= minute(to - 1 us)` |
//! | `Date` (`row_day`, `day`) | `t >= date(from) AND t <= date(to - 1 us)` |
//! | `SimpleAggregateFunction(max, ...)` (`last_seen`) | after grouping: `max(last_seen) >= from AND min(first_seen) < to` |
//!
//! and, for the sort key, the same bounds on `start_minute` and `target_day` where the table has them.

use std::fmt::{self, Write as _};

use crucible_plan::{
    AggOutput, Aggregate, BatchProbe, CmpOp, DeletePlan, DeleteTarget, Direction, FieldRange, Fill,
    Literal, Measure, Node, PointLookup, Pred, ProbeKind, QueryPlan, Scan, SemiJoin, Step,
    TextSearch, TimeSeries, TokenMode, pad_in_list,
};
use crucible_records::{ProjectId, TableId, TimeWindow, Timestamp, Uuid, truncate_str};
use crucible_schema::{ColumnRef, DATABASE, PROJECT_ID, TableDef, Ty, table};

/// The byte cap of one `metadata_kv` item (contract 3.1).
const METADATA_KV_CAP: usize = 1_024;
/// The shortest needle the `ngrams(3)` text index of `name` can serve.
const NAME_NGRAM: u8 = 3;

/// One bound value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Param {
    /// The ClickHouse type in the placeholder.
    pub ty: String,
    /// The value in the server's escaped text form.
    pub value: String,
}

/// A rendered statement: SQL whose only values are the placeholders `{p0:Type}`, `{p1:Type}`, ... of `params`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Statement {
    /// The SQL text.
    pub sql: String,
    /// The bound values; `params[n]` is `pN`.
    pub params: Vec<Param>,
    /// Settings the statement needs besides those of its class.
    pub settings: Vec<(&'static str, String)>,
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.sql)?;
        for (n, param) in self.params.iter().enumerate() {
            writeln!(f, "p{n}: {} = {:?}", param.ty, param.value)?;
        }
        for (name, value) in &self.settings {
            writeln!(f, "setting {name} = {value}")?;
        }
        Ok(())
    }
}

/// A plan the renderer cannot turn into a statement.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum RenderError {
    /// A literal that does not fit the type it is bound as. Plans validate their literals, so this is a defect.
    #[error("a literal does not fit column type {0}")]
    Literal(String),
    /// A table whose time column the window cannot bound.
    #[error("table {0} has no window predicate")]
    Window(&'static str),
    /// A zero-filled time series: its empty buckets hold nulls that the plan's output types cannot carry.
    #[error("zero-filled time series are not rendered")]
    ZeroFill,
}

/// Collects the bound values of one statement.
#[derive(Debug, Default)]
pub(crate) struct Binder {
    params: Vec<Param>,
    settings: Vec<(&'static str, String)>,
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            other => out.push(other),
        }
    }
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn float(value: f64) -> String {
    if value.is_nan() {
        "nan".to_owned()
    } else if value.is_infinite() {
        if value > 0.0 { "inf" } else { "-inf" }.to_owned()
    } else {
        value.to_string()
    }
}

fn seconds(t: Timestamp) -> String {
    t.micros().div_euclid(1_000_000).to_string()
}

fn micros(t: Timestamp) -> String {
    let (sign, abs) = if t.micros() < 0 {
        ("-", t.micros().unsigned_abs())
    } else {
        ("", t.micros().unsigned_abs())
    };
    format!("{sign}{}.{:06}", abs / 1_000_000, abs % 1_000_000)
}

impl Binder {
    pub(crate) fn finish(self, sql: String) -> Statement {
        Statement {
            sql,
            params: self.params,
            settings: self.settings,
        }
    }

    fn bind(&mut self, ty: impl fmt::Display, value: String) -> String {
        let n = self.params.len();
        let ty = ty.to_string();
        let placeholder = format!("{{p{n}:{ty}}}");
        self.params.push(Param { ty, value });
        placeholder
    }

    pub(crate) fn string(&mut self, value: &str) -> String {
        self.bind("String", escape(value))
    }

    pub(crate) fn uuid(&mut self, value: Uuid) -> String {
        self.bind("UUID", value.to_string())
    }

    pub(crate) fn u64(&mut self, value: u64) -> String {
        self.bind("UInt64", value.to_string())
    }

    fn time(&mut self, value: Timestamp) -> String {
        self.bind(Ty::DateTime64, micros(value))
    }

    fn minute(&mut self, value: Timestamp) -> String {
        self.bind(Ty::DateTime, seconds(value.floor_minute()))
    }

    fn date(&mut self, value: Timestamp) -> String {
        self.bind("Date", value.date().to_string())
    }

    /// `values` as an array of strings, for the store's own catalogue lookups.
    pub(crate) fn strings(&mut self, values: &[&str]) -> String {
        let items: Vec<String> = values.iter().map(|v| self.string(v)).collect();
        format!("[{}]", items.join(", "))
    }

    /// Binds a literal as a value of a column of type `ty`.
    fn literal(&mut self, ty: &Ty, literal: &Literal) -> Result<String, RenderError> {
        let scalar = ty.scalar();
        Ok(match (literal, scalar) {
            (Literal::Uuid(v), Ty::Uuid) => self.uuid(*v),
            (Literal::Bytes(v), Ty::FixedString(_)) => {
                format!("unhex({})", self.bind("String", hex(v)))
            }
            (Literal::String(v), Ty::String | Ty::LowCardinalityString) => self.string(v),
            (Literal::UInt(v), Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64) => {
                self.bind(scalar, v.to_string())
            }
            (Literal::Int(v), Ty::Int32 | Ty::Int64) => self.bind(scalar, v.to_string()),
            (Literal::Float(v), Ty::Float32 | Ty::Float64) => self.bind("Float64", float(*v)),
            (Literal::Time(v), Ty::DateTime64) => self.time(*v),
            (Literal::Time(v), Ty::DateTime) => self.bind(Ty::DateTime, seconds(*v)),
            (Literal::Date(v), Ty::Date) => self.bind("Date", v.to_string()),
            (Literal::Enum(v), Ty::Enum8(kind)) => {
                let variants = kind.variants();
                let (name, _) = variants
                    .iter()
                    .find(|(_, value)| value == v)
                    .ok_or_else(|| RenderError::Literal(ty.to_string()))?;
                self.string(name)
            }
            _ => return Err(RenderError::Literal(ty.to_string())),
        })
    }

    /// A list padded to a power of two (contract 8.3), each element bound as `ty`.
    fn list(&mut self, ty: &Ty, values: &[Literal]) -> Result<String, RenderError> {
        let items = pad_in_list(values)
            .iter()
            .map(|v| self.literal(ty, v))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(items.join(", "))
    }
}

fn op(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

fn direction(direction: Direction) -> &'static str {
    match direction {
        Direction::Asc => "ASC",
        Direction::Desc => "DESC",
    }
}

fn names(columns: &[ColumnRef]) -> String {
    columns
        .iter()
        .map(ColumnRef::name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn pred(b: &mut Binder, p: &Pred) -> Result<String, RenderError> {
    Ok(match p {
        Pred::And(operands) | Pred::Or(operands) => {
            let (joiner, empty) = if matches!(p, Pred::And(_)) {
                (" AND ", "1")
            } else {
                (" OR ", "0")
            };
            if operands.is_empty() {
                return Ok(empty.to_owned());
            }
            let parts = operands
                .iter()
                .map(|operand| pred(b, operand))
                .collect::<Result<Vec<_>, _>>()?;
            format!("({})", parts.join(joiner))
        }
        Pred::Not(operand) => format!("NOT ({})", pred(b, operand)?),
        Pred::Cmp(column, cmp, value) => {
            format!(
                "{} {} {}",
                column.name(),
                op(*cmp),
                b.literal(column.ty(), value)?
            )
        }
        Pred::In(column, values) => {
            format!("{} IN ({})", column.name(), b.list(column.ty(), values)?)
        }
        Pred::StartsWith(column, prefix) => {
            format!("startsWith({}, {})", column.name(), b.string(prefix))
        }
        Pred::HasAny(column, values) => {
            let element = match column.ty() {
                Ty::Array(element) => *element,
                other => return Err(RenderError::Literal(other.to_string())),
            };
            format!("hasAny({}, [{}])", column.name(), b.list(element, values)?)
        }
        Pred::MetadataEq { key, value } => {
            let item = format!("{key}={value}");
            format!(
                "has(metadata_kv, {})",
                b.string(truncate_str(&item, METADATA_KV_CAP))
            )
        }
        // A case-insensitive substring match. The text index of `name` (3-grams over the lowered value) serves the
        // token test; a needle shorter than one gram has no token, so only the exact test applies to it.
        Pred::TextMatch(needle) => {
            let needle = b.string(needle);
            format!(
                "(positionCaseInsensitiveUTF8(name, {needle}) > 0 AND (lengthUTF8({needle}) < {NAME_NGRAM} OR \
                 hasAllTokens(name, {needle})))"
            )
        }
    })
}

/// The window predicates of one table reference: those for `WHERE`, and those that apply after grouping.
fn window(
    b: &mut Binder,
    def: &TableDef,
    w: TimeWindow,
) -> Result<(Vec<String>, Vec<String>), RenderError> {
    let last = w.to().saturating_add_micros(-1);
    let time = def.retention.time_column;
    let ty = def
        .column(time)
        .map(|c| c.ty)
        .ok_or(RenderError::Window(def.name()))?;
    let (mut filter, mut having) = (Vec::new(), Vec::new());
    match ty {
        Ty::DateTime64 => {
            filter.push(format!("{time} >= {}", b.time(w.from())));
            filter.push(format!("{time} < {}", b.time(w.to())));
        }
        Ty::DateTime => {
            filter.push(format!("{time} >= {}", b.minute(w.from())));
            filter.push(format!("{time} <= {}", b.minute(last)));
        }
        Ty::Date => {
            filter.push(format!("{time} >= {}", b.date(w.from())));
            filter.push(format!("{time} <= {}", b.date(last)));
        }
        Ty::Saf(_, &Ty::DateTime64) => {
            having.push(format!("max({time}) >= {}", b.time(w.from())));
            if def.column("first_seen").is_some() {
                having.push(format!("min(first_seen) < {}", b.time(w.to())));
            }
        }
        _ => return Err(RenderError::Window(def.name())),
    }
    if time != "start_minute" && def.column("start_minute").is_some() {
        filter.push(format!("start_minute >= {}", b.minute(w.from())));
        filter.push(format!("start_minute <= {}", b.minute(last)));
    }
    if time != "target_day" && def.column("target_day").is_some() {
        filter.push(format!("target_day >= {}", b.date(w.from())));
        filter.push(format!("target_day <= {}", b.date(last)));
    }
    Ok((filter, having))
}

/// What every table reference carries.
struct Scope {
    project: ProjectId,
    window: TimeWindow,
}

impl Scope {
    /// `FROM crucible.<table> WHERE project_id = {scope} AND <window>`, and the window's post-grouping part.
    fn from(&self, b: &mut Binder, id: TableId) -> Result<(String, Vec<String>), RenderError> {
        let def = table(id);
        let project = b.uuid(self.project.uuid());
        let (filter, having) = window(b, def, self.window)?;
        let mut sql = format!(
            "FROM {DATABASE}.{} WHERE {PROJECT_ID} = {project}",
            def.name()
        );
        for condition in filter {
            sql.push_str(" AND ");
            sql.push_str(&condition);
        }
        Ok((sql, having))
    }
}

fn and_pred(b: &mut Binder, sql: &mut String, p: &Pred) -> Result<(), RenderError> {
    // The empty conjunction keeps every row and adds nothing.
    if *p != Pred::all() {
        sql.push_str(" AND ");
        sql.push_str(&pred(b, p)?);
    }
    Ok(())
}

fn limit(b: &mut Binder, sql: &mut String, limit: Option<u32>) {
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ");
        sql.push_str(&b.u64(u64::from(limit)));
    }
}

/// Equality of `columns` with one of `keys`, the key list padded to a power of two.
fn key_in(
    b: &mut Binder,
    columns: &[ColumnRef],
    keys: &[Vec<Literal>],
) -> Result<String, RenderError> {
    let mut padded: Vec<&Vec<Literal>> = keys.iter().collect();
    if let Some(last) = keys.last() {
        padded.resize(crucible_plan::padded_in_len(keys.len()), last);
    }
    let tuples = padded
        .into_iter()
        .map(|key| {
            let values = columns
                .iter()
                .zip(key)
                .map(|(column, value)| b.literal(column.ty(), value))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(match values.as_slice() {
                [one] => one.clone(),
                many => format!("({})", many.join(", ")),
            })
        })
        .collect::<Result<Vec<_>, RenderError>>()?;
    let left = match columns {
        [one] => one.name().to_owned(),
        many => format!("({})", names(many)),
    };
    Ok(format!("{left} IN ({})", tuples.join(", ")))
}

fn scan(
    b: &mut Binder,
    scope: &Scope,
    n: &Scan,
    restriction: Option<&str>,
) -> Result<String, RenderError> {
    let (from, _) = scope.from(b, n.table())?;
    let mut sql = format!("SELECT {} {from}", names(n.columns()));
    let arrival = n.arrival();
    if let Some(as_of) = arrival.as_of {
        let _ = write!(sql, " AND received_at <= {}", b.time(as_of));
    }
    if let Some(since) = arrival.since {
        let _ = write!(sql, " AND received_at > {}", b.time(since));
    }
    and_pred(b, &mut sql, n.pred())?;
    if let (Some(order), Some(cursor)) = (n.order(), n.cursor()) {
        let values = order
            .columns()
            .iter()
            .zip(cursor)
            .map(|(column, value)| b.literal(column.ty(), value))
            .collect::<Result<Vec<_>, _>>()?;
        let after = match order.direction() {
            Direction::Asc => ">",
            Direction::Desc => "<",
        };
        let _ = match (order.columns(), values.as_slice()) {
            ([column], [value]) => write!(sql, " AND {} {after} {value}", column.name()),
            (columns, values) => write!(
                sql,
                " AND ({}) {after} ({})",
                names(columns),
                values.join(", ")
            ),
        };
    }
    if let Some(restriction) = restriction {
        sql.push_str(" AND ");
        sql.push_str(restriction);
    }
    if let Some(order) = n.order() {
        let by: Vec<String> = order
            .columns()
            .iter()
            .map(|c| format!("{} {}", c.name(), direction(order.direction())))
            .collect();
        let _ = write!(sql, " ORDER BY {}", by.join(", "));
    }
    limit(b, &mut sql, n.limit());
    Ok(sql)
}

fn point_lookup(b: &mut Binder, scope: &Scope, n: &PointLookup) -> Result<String, RenderError> {
    let (from, _) = scope.from(b, n.table())?;
    let keys = key_in(b, n.key_columns(), n.keys())?;
    Ok(format!("SELECT {} {from} AND {keys}", names(n.columns())))
}

fn measure(m: &Measure) -> String {
    match m {
        Measure::Count => "count()".to_owned(),
        Measure::Sum(c) => format!("sum({})", c.name()),
        Measure::Min(c) => format!("min({})", c.name()),
        Measure::Uniq(c) => format!("uniqCombined64({})", c.name()),
        Measure::HistogramMerge(c) => format!("sumMap({})", c.name()),
        // A pick partial is rank-prefixed, so its merge is the maximum.
        Measure::Max(c) | Measure::PickMerge(c) => format!("max({})", c.name()),
        Measure::LatestByVersion(c) => format!("argMax({}, (version, fact_id))", c.name()),
    }
}

/// The type a `having` value is bound as: the measure's result type.
fn measure_ty(m: &Measure) -> Ty {
    match m {
        Measure::Count | Measure::Uniq(_) => Ty::UInt64,
        Measure::Sum(c) => match c.ty().scalar() {
            Ty::Int32 | Ty::Int64 => Ty::Int64,
            Ty::Float32 | Ty::Float64 => Ty::Float64,
            _ => Ty::UInt64,
        },
        Measure::Min(c)
        | Measure::Max(c)
        | Measure::HistogramMerge(c)
        | Measure::PickMerge(c)
        | Measure::LatestByVersion(c) => *c.ty(),
    }
}

/// An aggregate, optionally bucketed: `bucket` is the bucket expression of a time series.
fn aggregate(
    b: &mut Binder,
    scope: &Scope,
    n: &Aggregate,
    bucket: Option<String>,
) -> Result<String, RenderError> {
    let mut select: Vec<String> = bucket.iter().map(|e| format!("{e} AS bucket")).collect();
    select.extend(n.group().iter().map(|c| c.name().to_owned()));
    select.extend(
        n.measures()
            .iter()
            .enumerate()
            .map(|(i, m)| format!("{} AS m{i}", measure(m))),
    );
    let mut sql = format!("SELECT {} ", select.join(", "));
    let (from, mut having) = scope.from(b, n.table())?;
    sql.push_str(&from);
    and_pred(b, &mut sql, n.pred())?;

    let mut group: Vec<&str> = bucket.iter().map(|_| "bucket").collect();
    group.extend(n.group().iter().map(ColumnRef::name));
    if group.is_empty() {
        // One group: a row when at least one row matches and no row otherwise, never a row of defaults.
        having.insert(0, "count() > 0".to_owned());
    } else {
        let _ = write!(sql, " GROUP BY {}", group.join(", "));
    }
    for condition in n.having_conditions() {
        let m = n
            .measures()
            .get(condition.measure)
            .ok_or_else(|| RenderError::Literal("having".to_owned()))?;
        let value = b.literal(&measure_ty(m), &condition.value)?;
        having.push(format!(
            "m{} {} {value}",
            condition.measure,
            op(condition.op)
        ));
    }
    if !having.is_empty() {
        let _ = write!(sql, " HAVING {}", having.join(" AND "));
    }
    if !n.order().is_empty() {
        let by: Vec<String> = n
            .order()
            .iter()
            .map(|(output, dir)| {
                let name = match output {
                    AggOutput::Group(i) => n.group().get(*i).map(|c| c.name().to_owned()),
                    AggOutput::Measure(i) => Some(format!("m{i}")),
                };
                format!("{} {}", name.unwrap_or_default(), direction(*dir))
            })
            .collect();
        let _ = write!(sql, " ORDER BY {}", by.join(", "));
    }
    limit(b, &mut sql, n.limit());
    Ok(sql)
}

fn semi_join(b: &mut Binder, scope: &Scope, n: &SemiJoin) -> Result<String, RenderError> {
    // The scan's own values are bound after the subquery's, which is rendered first.
    let ids = match n.right() {
        Node::Aggregate(right) => {
            let id = right.group().first().map_or("", ColumnRef::name);
            format!("SELECT {id} FROM ({})", aggregate(b, scope, right, None)?)
        }
        right => node(b, scope, right)?,
    };
    // More ids than the cap fails the statement with the set-size code, which is `query_too_broad`.
    b.settings
        .push(("max_rows_in_set", n.max_ids().to_string()));
    b.settings.push(("set_overflow_mode", "throw".to_owned()));
    let restriction = format!("{} IN ({ids})", n.on().name());
    scan(b, scope, n.left(), Some(&restriction))
}

fn text_search(b: &mut Binder, scope: &Scope, n: &TextSearch) -> Result<String, RenderError> {
    let def = table(n.table());
    let (from, _) = scope.from(b, n.table())?;
    // The phrase function is not available on the floor version: a phrase is all its tokens, and the query
    // layer verifies the order (contract 8.7).
    let function = match n.mode() {
        TokenMode::All | TokenMode::Phrase => "hasAllTokens",
        TokenMode::Any => "hasAnyTokens",
    };
    let tokens: Vec<Literal> = n
        .tokens()
        .iter()
        .map(|t| Literal::String(t.clone()))
        .collect();
    let tokens = b.list(&Ty::String, &tokens)?;
    let fields: Vec<String> = n
        .fields()
        .iter()
        .map(|f| format!("{function}({}, [{tokens}])", f.name()))
        .collect();
    let mut sql = format!(
        "SELECT {} {from} AND ({})",
        names(&def.key_columns()),
        fields.join(" OR ")
    );
    and_pred(b, &mut sql, n.pred())?;
    limit(b, &mut sql, Some(n.limit()));
    Ok(sql)
}

fn time_series(b: &mut Binder, scope: &Scope, n: &TimeSeries) -> Result<String, RenderError> {
    if n.fill() == Fill::Zero {
        return Err(RenderError::ZeroFill);
    }
    let function = match n.step() {
        Step::Minute => "toStartOfMinute",
        Step::FiveMinutes => "toStartOfFiveMinutes",
        Step::FifteenMinutes => "toStartOfFifteenMinutes",
        Step::Hour => "toStartOfHour",
        Step::Day => "toStartOfDay",
        Step::Week => "toMonday",
    };
    let utc = b.string("UTC");
    let bucket = format!(
        "toDateTime({function}(toDateTime({}, {utc})), {utc})",
        n.time().name()
    );
    aggregate(b, scope, n.aggregate(), Some(bucket))
}

fn field_range(b: &mut Binder, scope: &Scope, n: &FieldRange) -> Result<String, RenderError> {
    let (from, _) = scope.from(b, n.table())?;
    // `substring` counts bytes from 1.
    let offset = b.u64(n.offset().saturating_add(1));
    let len = b.u64(n.byte_len());
    let mut sql = format!(
        "SELECT substring({}, {offset}, {len}) {from}",
        n.field().name()
    );
    for (column, value) in n.key_columns().iter().zip(n.key()) {
        let _ = write!(
            sql,
            " AND {} = {}",
            column.name(),
            b.literal(column.ty(), value)?
        );
    }
    sql.push_str(" LIMIT 1");
    Ok(sql)
}

fn node(b: &mut Binder, scope: &Scope, n: &Node) -> Result<String, RenderError> {
    match n {
        Node::Scan(n) => scan(b, scope, n, None),
        Node::PointLookup(n) => point_lookup(b, scope, n),
        Node::Aggregate(n) => aggregate(b, scope, n, None),
        Node::SemiJoin(n) => semi_join(b, scope, n),
        Node::TextSearch(n) => text_search(b, scope, n),
        Node::TimeSeries(n) => time_series(b, scope, n),
        Node::FieldRange(n) => field_range(b, scope, n),
    }
}

/// Renders a plan. The statement returns the columns of `plan.output()`, in order.
pub fn plan(plan: &QueryPlan) -> Result<Statement, RenderError> {
    let mut b = Binder::default();
    let scope = Scope {
        project: plan.scope().project_id(),
        window: plan.window(),
    };
    let sql = node(&mut b, &scope, plan.root())?;
    Ok(b.finish(sql))
}

/// Renders the existence probe (contract 5.5): the one statement without a project scope. It returns one
/// `count()`, and the block is present when the count is above zero.
pub fn probe(probe: &BatchProbe) -> Statement {
    let mut b = Binder::default();
    let name = probe.table().name();
    let partition = b.string(&probe.partition().to_string());
    let epoch = b.uuid(probe.epoch().uuid());
    let batch_seq = b.u64(probe.batch_seq().get());
    let sql = match probe.kind() {
        ProbeKind::BatchSeq => format!(
            "SELECT count() FROM {DATABASE}.{name} WHERE _partition_id = {partition} AND wal_epoch = {epoch} \
             AND batch_seq = {batch_seq}"
        ),
        ProbeKind::Marker => {
            let nil = b.uuid(Uuid::NIL);
            format!(
                "SELECT count() FROM {DATABASE}.{name} WHERE {PROJECT_ID} = {nil} AND marker_epoch = {epoch} AND \
                 marker_batch_seq = {batch_seq} AND _partition_id = {partition}"
            )
        }
    };
    b.finish(sql)
}

/// Renders a lightweight delete in one partition. It always carries the project predicate or the marker
/// predicate.
pub fn delete(plan: &DeletePlan) -> Result<Statement, RenderError> {
    let mut b = Binder::default();
    let partition = b.string(&plan.partition().to_string());
    let mut sql = format!(
        "DELETE FROM {DATABASE}.{} IN PARTITION ID {partition} WHERE {PROJECT_ID} = ",
        plan.table().name()
    );
    match plan.target() {
        DeleteTarget::Project(scope) => sql.push_str(&b.uuid(scope.project_id().uuid())),
        DeleteTarget::MarkerPurge { older_than } => {
            sql.push_str(&b.uuid(Uuid::NIL));
            let _ = write!(sql, " AND marker_at < {}", b.time(older_than));
        }
    }
    and_pred(&mut b, &mut sql, plan.pred())?;
    Ok(b.finish(sql))
}

/// `INSERT INTO crucible.<table> (<every column, in TableDef order>) FORMAT Native` (contract 5.2).
pub fn insert(id: TableId) -> String {
    let def = table(id);
    let columns: Vec<&str> = def.columns.iter().map(|c| c.name).collect();
    format!(
        "INSERT INTO {DATABASE}.{} ({}) FORMAT Native",
        def.name(),
        columns.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_escaped_for_the_parameter_text_form() {
        assert_eq!(escape("a\tb\\c'd\n\r\0é"), "a\\tb\\\\c'd\\n\\r\\0é");
        assert_eq!(hex(&[0, 0xff, 0x10]), "00ff10");
    }

    #[test]
    fn numbers_and_times_have_one_text_form() {
        assert_eq!(float(0.1), "0.1");
        assert_eq!(float(1e21), "1000000000000000000000");
        assert_eq!(float(f64::NAN), "nan");
        assert_eq!(float(f64::NEG_INFINITY), "-inf");
        let t = Timestamp::from_micros(1_794_907_800_000_001);
        assert_eq!(micros(t), "1794907800.000001");
        assert_eq!(seconds(t), "1794907800");
        assert_eq!(micros(Timestamp::from_micros(-500_000)), "-0.500000");
        assert_eq!(micros(Timestamp::EPOCH), "0.000000");
    }

    #[test]
    fn the_insert_statement_lists_every_column_in_table_order() {
        let sql = insert(TableId::DedupSelftest);
        assert_eq!(
            sql,
            "INSERT INTO crucible.dedup_selftest (id, wal_epoch, batch_seq, created_at) FORMAT Native"
        );
        for def in &crucible_schema::TABLES {
            let sql = insert(def.id);
            let list = sql
                .split_once('(')
                .and_then(|(_, rest)| rest.split_once(')'))
                .map(|(list, _)| list)
                .unwrap();
            let names: Vec<&str> = list.split(", ").collect();
            let expected: Vec<&str> = def.columns.iter().map(|c| c.name).collect();
            assert_eq!(names, expected);
        }
    }
}
