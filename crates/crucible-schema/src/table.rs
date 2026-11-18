//! `TableDef`: one ClickHouse table as data (contract sections 6.1 to 6.4).

use crucible_records::TableId;

use crate::types::{Codec, Ty};

/// One column of a table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColumnDef {
    /// The column name.
    pub name: &'static str,
    /// The column type.
    pub ty: Ty,
    /// The compression codec.
    pub codec: Codec,
    /// Whether the SQL workspace role may read the column (contract section 7.4): ingestion and marker columns,
    /// `api_key_id` and `row_digest` are excluded.
    pub sql_readable: bool,
}

const fn str_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Columns that the SQL workspace never reads (contract section 7.4).
const SQL_HIDDEN: [&str; 9] = [
    "wal_epoch",
    "batch_seq",
    "lsn",
    "received_at",
    "marker_epoch",
    "marker_batch_seq",
    "marker_at",
    "api_key_id",
    "row_digest",
];

impl ColumnDef {
    pub(crate) const fn new(name: &'static str, ty: Ty, codec: Codec) -> Self {
        let mut sql_readable = true;
        let mut i = 0;
        while i < SQL_HIDDEN.len() {
            if str_eq(name, SQL_HIDDEN[i]) {
                sql_readable = false;
            }
            i += 1;
        }
        Self {
            name,
            ty,
            codec,
            sql_readable,
        }
    }
}

/// Plain rows or mergeable partial rows.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TableKind {
    /// `MergeTree` family; rows are immutable and carry ingestion columns.
    Plain,
    /// `AggregatingMergeTree` family; every non-key column is a `SimpleAggregateFunction` and every block
    /// carries one marker row.
    Aggregating,
}

/// A table's partition expression at the default `day` granularity (contract sections 5.2 and 6.3).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PartitionExpr {
    /// No `PARTITION BY`; the single partition id is `all`.
    None,
    /// `toDate(column)` over a `DateTime64` column; `toMonday(toDate(column))` with the `week` option.
    Day(&'static str),
    /// A `Date` key column used directly; `toMonday(column)` with the `week` option.
    DateColumn(&'static str),
    /// `toYYYYMM(column)`; unaffected by the granularity option.
    Month(&'static str),
}

impl PartitionExpr {
    /// The column that routes a row, if the table is partitioned.
    pub const fn column(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Day(c) | Self::DateColumn(c) | Self::Month(c) => Some(c),
        }
    }
}

/// The tokenizer of a text index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Tokenizer {
    /// `ngrams(n)`.
    Ngrams(u8),
    /// `splitByNonAlpha`.
    SplitByNonAlpha,
    /// `array`: each array element is one token.
    Array,
}

/// The type of a data-skipping index.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum IndexKind {
    /// `bloom_filter(0.01)`.
    Bloom,
    /// `set(n)`.
    Set(u32),
    /// `minmax`.
    MinMax,
    /// `text(tokenizer = ..., preprocessor = lower(column))`, built on insert.
    Text {
        /// The tokenizer.
        tokenizer: Tokenizer,
        /// Whether the column is lowercased before tokenizing.
        lower: bool,
    },
}

/// A data-skipping index over one column. The index name is `idx_<column>`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SkipIndex {
    /// The indexed column.
    pub column: &'static str,
    /// The index type.
    pub kind: IndexKind,
    /// `GRANULARITY`.
    pub granularity: u32,
}

/// The granularity ClickHouse assigns to every text index.
pub const TEXT_INDEX_GRANULARITY: u32 = 100_000_000;

impl SkipIndex {
    pub(crate) const fn bloom(column: &'static str, granularity: u32) -> Self {
        Self {
            column,
            kind: IndexKind::Bloom,
            granularity,
        }
    }

    pub(crate) const fn set100(column: &'static str, granularity: u32) -> Self {
        Self {
            column,
            kind: IndexKind::Set(100),
            granularity,
        }
    }

    pub(crate) const fn minmax(column: &'static str) -> Self {
        Self {
            column,
            kind: IndexKind::MinMax,
            granularity: 1,
        }
    }

    pub(crate) const fn text(column: &'static str, tokenizer: Tokenizer, lower: bool) -> Self {
        Self {
            column,
            kind: IndexKind::Text { tokenizer, lower },
            granularity: TEXT_INDEX_GRANULARITY,
        }
    }
}

/// The project setting that bounds a table's rows (contract section 13.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RetentionBinding {
    /// `retention_days`.
    RetentionDays,
    /// `payload_retention_days`.
    PayloadRetentionDays,
    /// `mcp_content_retention_days`.
    McpContentRetentionDays,
    /// Rows older than one day are purged (`dedup_selftest`).
    OneDay,
    /// No retention (`sql_probe`).
    None,
}

/// A table's retention binding and the time column it is evaluated on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Retention {
    /// Which setting applies.
    pub binding: RetentionBinding,
    /// The column the cut-off is compared with.
    pub time_column: &'static str,
}

/// One table of contract section 6.3.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TableDef {
    /// The stable table id; it also gives the table name.
    pub id: TableId,
    /// Plain or aggregating.
    pub kind: TableKind,
    /// Every column, in block order.
    pub columns: &'static [ColumnDef],
    /// `PARTITION BY`.
    pub partition: PartitionExpr,
    /// `ORDER BY`, as column names.
    pub order_by: &'static [&'static str],
    /// How many leading `ORDER BY` columns form the `PRIMARY KEY`.
    pub primary_key_len: usize,
    /// Data-skipping indexes.
    pub indexes: &'static [SkipIndex],
    /// Table settings other than the deduplication window, which depends on the variant.
    pub settings: &'static [(&'static str, u64)],
    /// The retention binding.
    pub retention: Retention,
    /// Whether the SQL workspace role may read the table at all (contract section 7.4).
    pub sql_readable: bool,
}

impl TableDef {
    /// The table name inside the `crucible` database.
    pub const fn name(&self) -> &'static str {
        self.id.name()
    }

    /// The `PRIMARY KEY` columns.
    pub fn primary_key(&self) -> &'static [&'static str] {
        self.order_by.get(..self.primary_key_len).unwrap_or(&[])
    }

    /// The column with this name.
    pub fn column(&self, name: &str) -> Option<&'static ColumnDef> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Whether rows belong to a project (every table except `dedup_selftest`).
    pub fn is_scoped(&self) -> bool {
        self.columns.first().is_some_and(|c| c.name == PROJECT_ID)
    }

    /// The `ORDER BY` columns that a plan may name: every sort-key column except `project_id`.
    pub fn key_columns(&self) -> Vec<ColumnRef> {
        self.order_by
            .iter()
            .filter(|name| **name != PROJECT_ID)
            .filter_map(|name| self.column_ref(name))
            .collect()
    }

    /// The columns the SQL workspace role is granted, empty for a table it may not read.
    pub fn sql_columns(&self) -> impl Iterator<Item = &'static ColumnDef> {
        let readable = self.sql_readable;
        self.columns
            .iter()
            .filter(move |c| readable && c.sql_readable)
    }

    /// A reference to a named column other than `project_id`.
    pub(crate) fn column_ref(&self, name: &str) -> Option<ColumnRef> {
        if name == PROJECT_ID {
            return None;
        }
        let index = self.columns.iter().position(|c| c.name == name)?;
        Some(ColumnRef {
            table: self.id,
            index: u16::try_from(index).ok()?,
        })
    }
}

/// The name of the scope column. No column enum and no [`ColumnRef`] can name it: the plan renderer adds the
/// project predicate itself (contract section 8.1).
pub const PROJECT_ID: &str = "project_id";

/// A column of one table that a plan may name. Implemented only by the per-table enums of this crate, which list
/// every column except `project_id`.
pub trait Column: Copy + Eq + core::fmt::Debug + 'static {
    /// The table the column belongs to.
    const TABLE: TableId;
    /// Every nameable column of the table, in `TableDef` order.
    const ALL: &'static [Self];

    /// The type-erased reference.
    fn column_ref(self) -> ColumnRef;

    /// The column name.
    fn name(self) -> &'static str {
        self.column_ref().name()
    }
}

/// A type-erased reference to a real column of a real table. It can be obtained only from a [`Column`] enum or
/// from [`TableDef::key_columns`], so it never names `project_id` and never names a column that does not exist.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct ColumnRef {
    table: TableId,
    index: u16,
}

impl<C: Column> From<C> for ColumnRef {
    fn from(column: C) -> Self {
        column.column_ref()
    }
}

impl ColumnRef {
    pub(crate) const fn new(table: TableId, index: u16) -> Self {
        Self { table, index }
    }

    /// The table.
    pub const fn table(&self) -> TableId {
        self.table
    }

    /// The column definition.
    pub fn def(&self) -> &'static ColumnDef {
        &crate::tables::table(self.table).columns[usize::from(self.index)]
    }

    /// The column name.
    pub fn name(&self) -> &'static str {
        self.def().name
    }

    /// The column type.
    pub fn ty(&self) -> &'static Ty {
        &self.def().ty
    }

    /// Whether the column is in the table's `ORDER BY`.
    pub fn is_key(&self) -> bool {
        crate::tables::table(self.table)
            .order_by
            .contains(&self.name())
    }
}
