//! The plan nodes of contract section 8.1.
//!
//! Every node has private fields and validating constructors, so a node that exists is well-formed: its columns
//! belong to its table, its literals fit their columns, and aggregating tables are only reachable through
//! `Aggregate`. Nodes hold no SQL text; the renderer derives every identifier from the schema.

use crucible_records::{TableId, TimeWindow, Timestamp};
use crucible_schema::{AggFn, ColumnRef, IndexKind, TableKind, Ty, table};

use crate::error::PlanError;
use crate::literal::Literal;
use crate::pred::{CmpOp, Pred};

/// The default cap on the ids a semi-join may carry; exceeding it is `query_too_broad`.
pub const DEFAULT_MAX_IDS: u32 = 10_000;
/// The most tokens a text search with snippets may hold: the server's position function takes a bounded list.
pub const MAX_SNIPPET_TOKENS: usize = 64;

/// Ascending or descending.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Direction {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// A row order over columns of the table, all in one direction.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Order {
    by: Vec<ColumnRef>,
    direction: Direction,
}

impl Order {
    /// An order over the given columns.
    pub fn by(
        columns: impl IntoIterator<Item = impl Into<ColumnRef>>,
        direction: Direction,
    ) -> Self {
        Self {
            by: columns.into_iter().map(Into::into).collect(),
            direction,
        }
    }

    /// The table's key order: its `ORDER BY` columns after `project_id`.
    pub fn key(table_id: TableId, direction: Direction) -> Self {
        Self {
            by: table(table_id).key_columns(),
            direction,
        }
    }

    /// The order columns.
    pub fn columns(&self) -> &[ColumnRef] {
        &self.by
    }

    /// The direction.
    pub const fn direction(&self) -> Direction {
        self.direction
    }
}

/// Bounds on the arrival time of plain rows: `received_at <= as_of` keeps pages stable, `received_at > since`
/// selects what arrived after a head cursor.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ArrivalBounds {
    /// `received_at <= as_of`.
    pub as_of: Option<Timestamp>,
    /// `received_at > since`.
    pub since: Option<Timestamp>,
}

fn own(table_id: TableId, columns: &[ColumnRef]) -> Result<(), PlanError> {
    match columns.iter().find(|c| c.table() != table_id) {
        Some(column) => Err(PlanError::ForeignColumn {
            table: table_id,
            column: column.name(),
        }),
        None => Ok(()),
    }
}

/// Only project tables can be read through a plan: the renderer adds `project_id = {scope}` to every table
/// reference, and `dedup_selftest` has no such column.
fn scoped(table_id: TableId) -> Result<(), PlanError> {
    if table(table_id).is_scoped() {
        Ok(())
    } else {
        Err(PlanError::Table(table_id))
    }
}

fn plain(table_id: TableId) -> Result<(), PlanError> {
    scoped(table_id)?;
    match table(table_id).kind {
        TableKind::Plain => Ok(()),
        TableKind::Aggregating => Err(PlanError::AggregatingTable(table_id)),
    }
}

fn fits(columns: &[ColumnRef], values: &[Literal]) -> bool {
    columns.len() == values.len() && columns.iter().zip(values).all(|(c, v)| v.fits(c.ty()))
}

fn refs(columns: impl IntoIterator<Item = impl Into<ColumnRef>>) -> Vec<ColumnRef> {
    columns.into_iter().map(Into::into).collect()
}

/// A filtered read of a plain table, optionally ordered, limited and resumed from a keyset cursor.
#[derive(Clone, PartialEq, Debug)]
pub struct Scan {
    table: TableId,
    columns: Vec<ColumnRef>,
    pred: Pred,
    order: Option<Order>,
    limit: Option<u32>,
    cursor: Option<Vec<Literal>>,
    arrival: ArrivalBounds,
}

impl Scan {
    /// A scan of `table_id` returning `columns` for the rows that match `pred`.
    pub fn new(
        table_id: TableId,
        columns: impl IntoIterator<Item = impl Into<ColumnRef>>,
        pred: Pred,
    ) -> Result<Self, PlanError> {
        let columns = refs(columns);
        plain(table_id)?;
        own(table_id, &columns)?;
        pred.check(table_id)?;
        if columns.is_empty() {
            return Err(PlanError::Empty);
        }
        Ok(Self {
            table: table_id,
            columns,
            pred,
            order: None,
            limit: None,
            cursor: None,
            arrival: ArrivalBounds::default(),
        })
    }

    /// Orders the rows.
    pub fn ordered(mut self, order: Order) -> Result<Self, PlanError> {
        own(self.table, &order.by)?;
        if order.by.is_empty() {
            return Err(PlanError::Empty);
        }
        self.order = Some(order);
        Ok(self)
    }

    /// Returns at most `limit` rows.
    pub fn limited(mut self, limit: u32) -> Result<Self, PlanError> {
        if limit == 0 {
            return Err(PlanError::Limit);
        }
        self.limit = Some(limit);
        Ok(self)
    }

    /// Resumes strictly after the row whose order columns hold `after`. Requires an order.
    pub fn after(mut self, after: Vec<Literal>) -> Result<Self, PlanError> {
        match &self.order {
            Some(order) if fits(&order.by, &after) => {
                self.cursor = Some(after);
                Ok(self)
            }
            _ => Err(PlanError::Cursor),
        }
    }

    /// Bounds the rows by arrival time.
    pub fn arrived(mut self, arrival: ArrivalBounds) -> Result<Self, PlanError> {
        if table(self.table).column("received_at").is_none() {
            return Err(PlanError::NoArrivalTime(self.table));
        }
        self.arrival = arrival;
        Ok(self)
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The output columns.
    pub fn columns(&self) -> &[ColumnRef] {
        &self.columns
    }

    /// The row filter.
    pub const fn pred(&self) -> &Pred {
        &self.pred
    }

    /// The order, if any.
    pub const fn order(&self) -> Option<&Order> {
        self.order.as_ref()
    }

    /// The row limit, if any.
    pub const fn limit(&self) -> Option<u32> {
        self.limit
    }

    /// The keyset cursor: the order-column values of the last row already returned.
    pub fn cursor(&self) -> Option<&[Literal]> {
        self.cursor.as_deref()
    }

    /// The arrival bounds.
    pub const fn arrival(&self) -> ArrivalBounds {
        self.arrival
    }
}

/// A lookup of rows of a plain table by a list of keys.
#[derive(Clone, PartialEq, Debug)]
pub struct PointLookup {
    table: TableId,
    key_columns: Vec<ColumnRef>,
    keys: Vec<Vec<Literal>>,
    columns: Vec<ColumnRef>,
}

impl PointLookup {
    /// Rows whose `key_columns` equal one of `keys`.
    pub fn new(
        table_id: TableId,
        key_columns: impl IntoIterator<Item = impl Into<ColumnRef>>,
        keys: Vec<Vec<Literal>>,
        columns: impl IntoIterator<Item = impl Into<ColumnRef>>,
    ) -> Result<Self, PlanError> {
        let (key_columns, columns) = (refs(key_columns), refs(columns));
        plain(table_id)?;
        own(table_id, &key_columns)?;
        own(table_id, &columns)?;
        if key_columns.is_empty() || keys.is_empty() || columns.is_empty() {
            return Err(PlanError::Empty);
        }
        if !keys.iter().all(|key| fits(&key_columns, key)) {
            return Err(PlanError::Key);
        }
        Ok(Self {
            table: table_id,
            key_columns,
            keys,
            columns,
        })
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The columns the keys are matched on.
    pub fn key_columns(&self) -> &[ColumnRef] {
        &self.key_columns
    }

    /// The keys.
    pub fn keys(&self) -> &[Vec<Literal>] {
        &self.keys
    }

    /// The output columns.
    pub fn columns(&self) -> &[ColumnRef] {
        &self.columns
    }
}

/// A measure of the closed set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Measure {
    /// The number of rows of a plain table.
    Count,
    /// The sum of a numeric column, or the merge of a `sum` partial.
    Sum(ColumnRef),
    /// The minimum of a column, or the merge of a `min` partial.
    Min(ColumnRef),
    /// The maximum of a column, or the merge of a `max` partial.
    Max(ColumnRef),
    /// The distinct count of a column (`uniqCombined64`).
    Uniq(ColumnRef),
    /// The merge of a histogram partial (`sumMap`); quantiles are a post-operator.
    HistogramMerge(ColumnRef),
    /// The merge of a rank-prefixed pick partial; resolution is a post-operator.
    PickMerge(ColumnRef),
    /// The value of a fact column on the row with the greatest `(version, fact_id)` in each group.
    LatestByVersion(ColumnRef),
    /// The merge of a bounded set partial (`groupUniqArrayArray(n)`): the union of the stored sets as an array
    /// in ascending order, cut to the column's bound `n`. Which elements a stored partial kept above its bound
    /// is the store's choice.
    SetMerge(ColumnRef),
}

impl Measure {
    fn column(&self) -> Option<&ColumnRef> {
        match self {
            Self::Count => None,
            Self::Sum(c)
            | Self::Min(c)
            | Self::Max(c)
            | Self::Uniq(c)
            | Self::HistogramMerge(c)
            | Self::PickMerge(c)
            | Self::LatestByVersion(c)
            | Self::SetMerge(c) => Some(c),
        }
    }

    /// Whether `value` has the type of the measure's result, so that a condition or a cursor can compare them:
    /// unsigned for a count, a distinct count or an unsigned sum, the column's type for a minimum, maximum or
    /// latest value. Histogram, pick and set merges cannot be compared.
    fn comparable(&self, value: &Literal) -> bool {
        match self {
            Self::Count | Self::Uniq(_) => matches!(value, Literal::UInt(_)),
            Self::Sum(c) => matches!(
                (c.ty().scalar(), value),
                (Ty::Int32 | Ty::Int64, Literal::Int(_))
                    | (Ty::Float32 | Ty::Float64, Literal::Float(_))
                    | (
                        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64,
                        Literal::UInt(_)
                    )
            ),
            Self::Min(c) | Self::Max(c) | Self::LatestByVersion(c) => value.fits(c.ty()),
            Self::HistogramMerge(_) | Self::PickMerge(_) | Self::SetMerge(_) => false,
        }
    }

    fn check(&self, table_id: TableId) -> Result<(), PlanError> {
        let def = table(table_id);
        let aggregating = def.kind == TableKind::Aggregating;
        let Some(column) = self.column() else {
            // Counting the partial rows of an aggregating table means nothing: its counts are `sum` partials.
            return if aggregating {
                Err(PlanError::Measure("count"))
            } else {
                Ok(())
            };
        };
        own(table_id, core::slice::from_ref(column))?;
        let numeric = matches!(
            column.ty().scalar(),
            Ty::UInt8
                | Ty::UInt16
                | Ty::UInt32
                | Ty::UInt64
                | Ty::Int32
                | Ty::Int64
                | Ty::Float32
                | Ty::Float64
        );
        let partial = column.ty().agg_fn();
        let scalar = !matches!(column.ty(), Ty::Array(_) | Ty::Tuple2(..));
        let ok = match self {
            Self::Count => true,
            Self::Sum(_) if aggregating => partial == Some(AggFn::Sum),
            Self::Sum(_) => numeric,
            Self::Min(_) => partial == Some(AggFn::Min) || (partial.is_none() && scalar),
            Self::Max(_) => partial == Some(AggFn::Max) || (partial.is_none() && scalar),
            Self::Uniq(_) => partial.is_none() && scalar,
            Self::HistogramMerge(_) => partial == Some(AggFn::SumMap),
            Self::PickMerge(_) => {
                partial == Some(AggFn::Max) && *column.ty().scalar() == Ty::String
            }
            Self::LatestByVersion(_) => {
                def.column("version").is_some() && def.column("fact_id").is_some()
            }
            Self::SetMerge(_) => matches!(partial, Some(AggFn::GroupUniqArrayArray(_))),
        };
        if ok {
            Ok(())
        } else {
            Err(PlanError::Measure(column.name()))
        }
    }
}

/// A condition on one measure of an aggregate.
#[derive(Clone, PartialEq, Debug)]
pub struct Having {
    /// The index of the measure in the aggregate's measure list.
    pub measure: usize,
    /// The comparison.
    pub op: CmpOp,
    /// The value compared with.
    pub value: Literal,
}

/// An output of an aggregate to order by.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AggOutput {
    /// The group dimension at this index.
    Group(usize),
    /// The measure at this index.
    Measure(usize),
}

/// A grouped read. The only way to read an aggregating table: its partial rows are merged per group at read
/// time, and no statement uses `FINAL`.
#[derive(Clone, PartialEq, Debug)]
pub struct Aggregate {
    table: TableId,
    pred: Pred,
    group: Vec<ColumnRef>,
    measures: Vec<Measure>,
    having: Vec<Having>,
    order: Vec<(AggOutput, Direction)>,
    limit: Option<u32>,
    cursor: Option<Vec<Literal>>,
}

impl Aggregate {
    /// Groups the rows of `table_id` that match `pred` by `group` and computes `measures`. On an aggregating
    /// table every group dimension must be a key column.
    pub fn new(
        table_id: TableId,
        pred: Pred,
        group: impl IntoIterator<Item = impl Into<ColumnRef>>,
        measures: Vec<Measure>,
    ) -> Result<Self, PlanError> {
        let group = refs(group);
        scoped(table_id)?;
        own(table_id, &group)?;
        pred.check(table_id)?;
        let aggregating = table(table_id).kind == TableKind::Aggregating;
        for column in &group {
            let marker = column.name().starts_with("marker_");
            if column.ty().agg_fn().is_some() || marker || (aggregating && !column.is_key()) {
                return Err(PlanError::GroupDimension(column.name()));
            }
        }
        if measures.is_empty() {
            return Err(PlanError::Empty);
        }
        measures.iter().try_for_each(|m| m.check(table_id))?;
        Ok(Self {
            table: table_id,
            pred,
            group,
            measures,
            having: Vec::new(),
            order: Vec::new(),
            limit: None,
            cursor: None,
        })
    }

    /// Keeps the groups whose measures satisfy every condition. A condition's value must have the type of its
    /// measure: unsigned for a count, a distinct count or an unsigned sum, the column's type for a minimum,
    /// maximum or latest value. Histogram, pick and set merges cannot be compared.
    pub fn having(mut self, having: Vec<Having>) -> Result<Self, PlanError> {
        for condition in &having {
            let measure = self
                .measures
                .get(condition.measure)
                .ok_or(PlanError::OutputIndex)?;
            let fits = measure.comparable(&condition.value);
            if !fits {
                return Err(PlanError::LiteralType(
                    measure.column().map_or("count", |c| c.name()),
                ));
            }
        }
        self.having = having;
        Ok(self)
    }

    /// Orders the groups.
    pub fn ordered(mut self, order: Vec<(AggOutput, Direction)>) -> Result<Self, PlanError> {
        let valid = |output: &AggOutput| match output {
            AggOutput::Group(i) => *i < self.group.len(),
            AggOutput::Measure(i) => *i < self.measures.len(),
        };
        if !order.iter().all(|(output, _)| valid(output)) {
            return Err(PlanError::OutputIndex);
        }
        self.order = order;
        self.cursor = None;
        Ok(self)
    }

    /// Returns at most `limit` groups.
    pub fn limited(mut self, limit: u32) -> Result<Self, PlanError> {
        if limit == 0 {
            return Err(PlanError::Limit);
        }
        self.limit = Some(limit);
        Ok(self)
    }

    /// Resumes strictly after the group whose order outputs hold `after`: the keyset cursor of a grouped list.
    /// Requires an order whose outputs all run in one direction, and one value per order output, of its type.
    /// The order must be set first; setting it again drops the cursor.
    pub fn after(mut self, after: Vec<Literal>) -> Result<Self, PlanError> {
        let one_direction = match self.order.split_first() {
            Some(((_, first), rest)) => rest.iter().all(|(_, direction)| direction == first),
            None => false,
        };
        let fits = |(output, value): (&(AggOutput, Direction), &Literal)| match output.0 {
            AggOutput::Group(i) => self.group.get(i).is_some_and(|c| value.fits(c.ty())),
            AggOutput::Measure(i) => self.measures.get(i).is_some_and(|m| m.comparable(value)),
        };
        if !one_direction
            || after.len() != self.order.len()
            || !self.order.iter().zip(&after).all(fits)
        {
            return Err(PlanError::Cursor);
        }
        self.cursor = Some(after);
        Ok(self)
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The row filter, applied before grouping.
    pub const fn pred(&self) -> &Pred {
        &self.pred
    }

    /// The group dimensions.
    pub fn group(&self) -> &[ColumnRef] {
        &self.group
    }

    /// The measures.
    pub fn measures(&self) -> &[Measure] {
        &self.measures
    }

    /// The conditions on measures.
    pub fn having_conditions(&self) -> &[Having] {
        &self.having
    }

    /// The group order.
    pub fn order(&self) -> &[(AggOutput, Direction)] {
        &self.order
    }

    /// The group limit, if any.
    pub const fn limit(&self) -> Option<u32> {
        self.limit
    }

    /// The keyset cursor: the order-output values of the last group already returned.
    pub fn cursor(&self) -> Option<&[Literal]> {
        self.cursor.as_deref()
    }
}

/// A scan restricted to the ids that another node produces.
#[derive(Clone, PartialEq, Debug)]
pub struct SemiJoin {
    left: Scan,
    on: ColumnRef,
    right: Box<Node>,
    max_ids: u32,
}

impl SemiJoin {
    /// Restricts `left` to the rows whose `on` column is among the ids `right` produces. `right` must be a scan
    /// with one output column or an aggregate with one group dimension, of the same type as `on`.
    pub fn new(
        left: Scan,
        on: impl Into<ColumnRef>,
        right: Node,
        max_ids: u32,
    ) -> Result<Self, PlanError> {
        let on = on.into();
        own(left.table, core::slice::from_ref(&on))?;
        let produced = match &right {
            Node::Scan(scan) => scan.columns.as_slice(),
            Node::Aggregate(aggregate) => aggregate.group.as_slice(),
            _ => &[],
        };
        match produced {
            [id] if id.ty().scalar() == on.ty().scalar() && max_ids > 0 => Ok(Self {
                left,
                on,
                right: Box::new(right),
                max_ids,
            }),
            _ => Err(PlanError::SemiJoin),
        }
    }

    /// The restricted scan.
    pub const fn left(&self) -> &Scan {
        &self.left
    }

    /// The column of the left table matched against the ids.
    pub const fn on(&self) -> ColumnRef {
        self.on
    }

    /// The node that produces the ids.
    pub fn right(&self) -> &Node {
        &self.right
    }

    /// The most ids the right side may produce before the query is refused as too broad.
    pub const fn max_ids(&self) -> u32 {
        self.max_ids
    }
}

/// How the tokens of a text query combine.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenMode {
    /// Every token occurs.
    All,
    /// At least one token occurs.
    Any,
    /// The tokens occur as a phrase.
    Phrase,
}

/// The part of each searched string field a text search returns for highlighting.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Snippet {
    /// How many bytes before the first occurrence of a token the snippet starts.
    pub before: u32,
    /// The most bytes of the field the snippet holds.
    pub len: u32,
}

/// A full-text search over text-indexed fields of a payload table; it returns row keys and, when asked, a
/// snippet of each searched string field. Like a scan, it can be ordered by the table's key, resumed from a
/// keyset cursor and bounded by arrival time, which is how search pages.
#[derive(Clone, PartialEq, Debug)]
pub struct TextSearch {
    table: TableId,
    fields: Vec<ColumnRef>,
    mode: TokenMode,
    tokens: Vec<String>,
    pred: Pred,
    limit: u32,
    snippet: Option<Snippet>,
    order: Option<Order>,
    cursor: Option<Vec<Literal>>,
    arrival: ArrivalBounds,
}

impl TextSearch {
    /// Searches `fields`, each of which must carry a text index, for `tokens`.
    pub fn new(
        table_id: TableId,
        fields: impl IntoIterator<Item = impl Into<ColumnRef>>,
        mode: TokenMode,
        tokens: Vec<String>,
        pred: Pred,
        limit: u32,
    ) -> Result<Self, PlanError> {
        let fields = refs(fields);
        plain(table_id)?;
        own(table_id, &fields)?;
        pred.check(table_id)?;
        if fields.is_empty() || tokens.is_empty() || tokens.iter().any(String::is_empty) {
            return Err(PlanError::Empty);
        }
        if limit == 0 {
            return Err(PlanError::Limit);
        }
        let indexed = |field: &ColumnRef| {
            table(table_id)
                .indexes
                .iter()
                .any(|i| i.column == field.name() && matches!(i.kind, IndexKind::Text { .. }))
        };
        if let Some(field) = fields.iter().find(|f| !indexed(f)) {
            return Err(PlanError::UnsupportedPred(field.name()));
        }
        Ok(Self {
            table: table_id,
            fields,
            mode,
            tokens,
            pred,
            limit,
            snippet: None,
            order: None,
            cursor: None,
            arrival: ArrivalBounds::default(),
        })
    }

    /// Orders the hits by the table's key columns ([`Order::key`]) in `direction`.
    pub fn ordered(mut self, direction: Direction) -> Self {
        self.order = Some(Order::key(self.table, direction));
        self
    }

    /// Resumes strictly after the hit whose key columns hold `after`. Requires an order.
    pub fn after(mut self, after: Vec<Literal>) -> Result<Self, PlanError> {
        match &self.order {
            Some(order) if fits(&order.by, &after) => {
                self.cursor = Some(after);
                Ok(self)
            }
            _ => Err(PlanError::Cursor),
        }
    }

    /// Bounds the rows by arrival time.
    pub fn arrived(mut self, arrival: ArrivalBounds) -> Result<Self, PlanError> {
        if table(self.table).column("received_at").is_none() {
            return Err(PlanError::NoArrivalTime(self.table));
        }
        self.arrival = arrival;
        Ok(self)
    }

    /// Also returns, for each searched `String` field in field order, two columns: `<field>_match`, the byte
    /// position (from 1) of the first occurrence of a needle in the field as a substring, compared without
    /// regard to ASCII case, or 0 when none occurs; and `<field>_snippet`, at most `snippet.len` bytes of the
    /// field starting `snippet.before` bytes before that position (at the field's first byte when the position
    /// is 0 or closer to the start). The snippet's first byte is byte `max(match, before + 1) - before` of the
    /// field. The needles are the tokens, except in [`TokenMode::Phrase`], where the one needle is the tokens
    /// joined by single spaces ([`TextSearch::needles`]), so that a phrase found later in a long field is
    /// inside its snippet. A hit is decided by tokens and the position by substrings, so a field can hold a
    /// position without being the field that matched; an array field has no snippet.
    pub fn with_snippets(mut self, snippet: Snippet) -> Result<Self, PlanError> {
        if snippet.len == 0 {
            return Err(PlanError::Limit);
        }
        if self.tokens.len() > MAX_SNIPPET_TOKENS {
            return Err(PlanError::ListLength("tokens"));
        }
        self.snippet = Some(snippet);
        Ok(self)
    }

    /// The searched fields that carry a snippet: the `String` ones, in field order.
    pub fn snippet_fields(&self) -> Vec<ColumnRef> {
        match self.snippet {
            Some(_) => self
                .fields
                .iter()
                .filter(|f| matches!(f.ty(), Ty::String))
                .copied()
                .collect(),
            None => Vec::new(),
        }
    }

    /// The snippet returned for each searched string field, if any.
    pub const fn snippet(&self) -> Option<Snippet> {
        self.snippet
    }

    /// What the snippet positions look for: the tokens, or in [`TokenMode::Phrase`] the one phrase of the
    /// tokens joined by single spaces.
    pub fn needles(&self) -> Vec<String> {
        match self.mode {
            TokenMode::Phrase => vec![self.tokens.join(" ")],
            TokenMode::All | TokenMode::Any => self.tokens.clone(),
        }
    }

    /// The order of the hits, if any: the table's key columns.
    pub const fn order(&self) -> Option<&Order> {
        self.order.as_ref()
    }

    /// The keyset cursor: the key of the last hit already returned.
    pub fn cursor(&self) -> Option<&[Literal]> {
        self.cursor.as_deref()
    }

    /// The arrival bounds.
    pub const fn arrival(&self) -> ArrivalBounds {
        self.arrival
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The searched fields.
    pub fn fields(&self) -> &[ColumnRef] {
        &self.fields
    }

    /// How the tokens combine.
    pub const fn mode(&self) -> TokenMode {
        self.mode
    }

    /// The tokens.
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// The row filter.
    pub const fn pred(&self) -> &Pred {
        &self.pred
    }

    /// The row limit.
    pub const fn limit(&self) -> u32 {
        self.limit
    }
}

/// The bucket width of a time series.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Step {
    /// One minute.
    Minute,
    /// Five minutes.
    FiveMinutes,
    /// Fifteen minutes.
    FifteenMinutes,
    /// One hour.
    Hour,
    /// One day.
    Day,
    /// One week.
    Week,
}

impl Step {
    /// Every step, finest first.
    pub const ALL: [Self; 6] = [
        Self::Minute,
        Self::FiveMinutes,
        Self::FifteenMinutes,
        Self::Hour,
        Self::Day,
        Self::Week,
    ];

    /// The width of a bucket in seconds. Buckets are aligned to the UTC epoch, and weeks start on Monday.
    pub const fn seconds(self) -> i64 {
        match self {
            Self::Minute => 60,
            Self::FiveMinutes => 300,
            Self::FifteenMinutes => 900,
            Self::Hour => 3_600,
            Self::Day => 86_400,
            Self::Week => 604_800,
        }
    }

    /// The first instant of the bucket that holds `t`.
    pub fn bucket(self, t: Timestamp) -> Timestamp {
        const MICROS: i64 = 1_000_000;
        // 1970-01-01 was a Thursday: the first Monday of the epoch is three days before it.
        let shift = match self {
            Self::Week => 3 * 86_400,
            _ => 0,
        };
        let width = self.seconds();
        let seconds = t.micros().div_euclid(MICROS);
        let start = (seconds + shift).div_euclid(width) * width - shift;
        Timestamp::from_micros(start.saturating_mul(MICROS))
    }

    /// The buckets a window overlaps: the first instant of the first, and how many there are. This is the one
    /// definition the caps of the metrics compiler, the fill bounds of the renderer and the store model share.
    pub fn buckets(self, window: TimeWindow) -> (Timestamp, u64) {
        let first = self.bucket(window.from());
        let last = self.bucket(window.to().saturating_add_micros(-1));
        let span = last.micros().saturating_sub(first.micros()) / 1_000_000;
        (first, (span / self.seconds()).unsigned_abs() + 1)
    }
}

/// What an empty bucket holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Fill {
    /// Empty buckets are omitted.
    None,
    /// Every bucket the plan's window overlaps is returned for every series, in `(group dimensions, bucket)`
    /// ascending order. A series is a combination of group dimension values with at least one matching row;
    /// an aggregate without group dimensions is one series, which exists even when no row matches. An empty
    /// bucket holds the series' dimension values, 0 for `count` and `sum`, null for `min`, `max`, `uniq`,
    /// pick and latest-by-version measures, the empty histogram `([], [])` for a histogram merge (the floor
    /// server version has no nullable tuple) and the empty array for a set merge.
    Zero,
}

/// An aggregate bucketed by time, with gap fill.
#[derive(Clone, PartialEq, Debug)]
pub struct TimeSeries {
    aggregate: Aggregate,
    time: ColumnRef,
    step: Step,
    fill: Fill,
}

impl TimeSeries {
    /// Buckets `aggregate` by `time`, a key time column of its table, in steps of `step`.
    pub fn new(
        aggregate: Aggregate,
        time: impl Into<ColumnRef>,
        step: Step,
        fill: Fill,
    ) -> Result<Self, PlanError> {
        let time = time.into();
        own(aggregate.table, core::slice::from_ref(&time))?;
        // A filled series has its own order, and a condition, a limit or a cursor would have to apply to
        // buckets that hold no row.
        let shaped = !aggregate.having.is_empty()
            || !aggregate.order.is_empty()
            || aggregate.limit.is_some()
            || aggregate.cursor.is_some();
        if fill == Fill::Zero && shaped {
            return Err(PlanError::ZeroFill);
        }
        match time.ty() {
            Ty::DateTime | Ty::DateTime64 | Ty::Date => Ok(Self {
                aggregate,
                time,
                step,
                fill,
            }),
            _ => Err(PlanError::GroupDimension(time.name())),
        }
    }

    /// The bucketed aggregate.
    pub const fn aggregate(&self) -> &Aggregate {
        &self.aggregate
    }

    /// The time column.
    pub const fn time(&self) -> ColumnRef {
        self.time
    }

    /// The bucket width.
    pub const fn step(&self) -> Step {
        self.step
    }

    /// The gap fill.
    pub const fn fill(&self) -> Fill {
        self.fill
    }
}

/// A byte range of one string field of one row of a plain table.
#[derive(Clone, PartialEq, Debug)]
pub struct FieldRange {
    table: TableId,
    key_columns: Vec<ColumnRef>,
    key: Vec<Literal>,
    field: ColumnRef,
    offset: u64,
    len: u64,
    total: bool,
}

impl FieldRange {
    /// Bytes `[offset, offset + len)` of `field` on the row with this key.
    pub fn new(
        table_id: TableId,
        key_columns: impl IntoIterator<Item = impl Into<ColumnRef>>,
        key: Vec<Literal>,
        field: impl Into<ColumnRef>,
        offset: u64,
        len: u64,
    ) -> Result<Self, PlanError> {
        let (key_columns, field) = (refs(key_columns), field.into());
        plain(table_id)?;
        own(table_id, &key_columns)?;
        own(table_id, core::slice::from_ref(&field))?;
        if key_columns.is_empty() || !fits(&key_columns, &key) {
            return Err(PlanError::Key);
        }
        if *field.ty() != Ty::String {
            return Err(PlanError::UnsupportedPred(field.name()));
        }
        if len == 0 || offset.checked_add(len).is_none() {
            return Err(PlanError::Limit);
        }
        Ok(Self {
            table: table_id,
            key_columns,
            key,
            field,
            offset,
            len,
            total: false,
        })
    }

    /// Also returns `<field>_bytes`, the length of the whole field in bytes, as a second column: what a byte
    /// range response needs for its total length.
    pub fn with_total(mut self) -> Self {
        self.total = true;
        self
    }

    /// Whether the length of the whole field is returned too.
    pub const fn total(&self) -> bool {
        self.total
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The columns the key is matched on.
    pub fn key_columns(&self) -> &[ColumnRef] {
        &self.key_columns
    }

    /// The key.
    pub fn key(&self) -> &[Literal] {
        &self.key
    }

    /// The field.
    pub const fn field(&self) -> ColumnRef {
        self.field
    }

    /// The first byte.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// The number of bytes.
    pub const fn byte_len(&self) -> u64 {
        self.len
    }
}

/// The root of a [`crate::QueryPlan`]. `BatchProbe` is not a variant: the one unscoped read is the separate
/// [`crate::BatchProbe`] type, so no scoped plan can contain it.
#[derive(Clone, PartialEq, Debug)]
pub enum Node {
    /// A filtered read of a plain table.
    Scan(Scan),
    /// A lookup by keys.
    PointLookup(PointLookup),
    /// A grouped read.
    Aggregate(Aggregate),
    /// A scan restricted to ids from another node.
    SemiJoin(SemiJoin),
    /// A full-text search.
    TextSearch(TextSearch),
    /// A time-bucketed aggregate.
    TimeSeries(TimeSeries),
    /// A byte range of one field.
    FieldRange(FieldRange),
}

impl Node {
    /// Narrows every predicate of the node to the rows that no pending deletion hides.
    pub(crate) fn hide(&mut self, hidden: &[crate::hidden::Hidden]) {
        use crate::hidden::keep;
        match self {
            Self::Scan(n) => keep(n.table, &mut n.pred, hidden),
            Self::Aggregate(n) => keep(n.table, &mut n.pred, hidden),
            Self::SemiJoin(n) => {
                keep(n.left.table, &mut n.left.pred, hidden);
                n.right.hide(hidden);
            }
            Self::TextSearch(n) => keep(n.table, &mut n.pred, hidden),
            Self::TimeSeries(n) => keep(n.aggregate.table, &mut n.aggregate.pred, hidden),
            // Lookups by key carry no predicate: the reads that use them find their keys through a plan
            // that does.
            Self::PointLookup(_) | Self::FieldRange(_) => {}
        }
    }

    /// Every table the node reads, those of its predicates' sets included. The renderer adds the project
    /// predicate and the window predicate to each.
    pub fn tables(&self) -> Vec<TableId> {
        let with_sets = |table: TableId, pred: &Pred| {
            let mut tables = vec![table];
            tables.extend(pred.set_tables());
            tables
        };
        match self {
            Self::Scan(n) => with_sets(n.table, &n.pred),
            Self::PointLookup(n) => vec![n.table],
            Self::Aggregate(n) => with_sets(n.table, &n.pred),
            Self::SemiJoin(n) => {
                let mut tables = with_sets(n.left.table, &n.left.pred);
                tables.extend(n.right.tables());
                tables
            }
            Self::TextSearch(n) => with_sets(n.table, &n.pred),
            Self::TimeSeries(n) => with_sets(n.aggregate.table, &n.aggregate.pred),
            Self::FieldRange(n) => vec![n.table],
        }
    }
}
