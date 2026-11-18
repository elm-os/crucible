//! The DDL renderer for the three variants of contract section 6.2.
//!
//! The output is laid out the way ClickHouse prints `SHOW CREATE TABLE`, so the conformance comparison only has
//! to drop `IF NOT EXISTS` and `ON CLUSTER` and collapse whitespace.

use core::fmt::{self, Write as _};

use crate::table::{IndexKind, PartitionExpr, SkipIndex, TableDef, TableKind, Tokenizer};

/// The database that holds the base tables.
pub const DATABASE: &str = "crucible";
/// The database that holds the curated views of the SQL workspace.
pub const SQL_DATABASE: &str = "crucible_sql";

/// The count window of the deduplication log on every table.
pub const DEDUP_WINDOW: u32 = 20_000;
/// The age window of the deduplication log on replicated engines, in seconds.
pub const DEDUP_WINDOW_SECONDS: u32 = 7_200;

/// The partition granularity, a provisioning parameter chosen before the first migration.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum Granularity {
    /// Daily partitions.
    #[default]
    Day,
    /// Weekly partitions: every daily partition expression becomes `toMonday(...)` of its date.
    Week,
}

/// A name that is not a plain identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a plain identifier: {0:?}")]
pub struct InvalidIdent(pub String);

/// A plain SQL identifier (`[A-Za-z_][A-Za-z0-9_]*`, at most 64 bytes), safe to print unquoted.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Ident(String);

impl Ident {
    /// Validates a name.
    pub fn new(name: &str) -> Result<Self, InvalidIdent> {
        let mut bytes = name.bytes();
        let head_ok = bytes
            .next()
            .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_');
        if head_ok && name.len() <= 64 && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            Ok(Self(name.to_owned()))
        } else {
            Err(InvalidIdent(name.to_owned()))
        }
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The deployment flavour a statement is rendered for.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Variant {
    /// One server: `MergeTree` engines.
    SingleNode,
    /// One shard with N replicas: `Replicated*` engines, every statement `ON CLUSTER`.
    Clustered {
        /// The cluster name from the server's `remote_servers` configuration.
        cluster: Ident,
    },
    /// The managed service: plain engine names, which the service converts to its shared engines.
    Cloud,
}

impl Variant {
    /// ` ON CLUSTER <name>` for the clustered variant, empty otherwise.
    pub(crate) fn on_cluster(&self) -> String {
        match self {
            Self::Clustered { cluster } => format!(" ON CLUSTER {cluster}"),
            Self::SingleNode | Self::Cloud => String::new(),
        }
    }

    /// The settings every DDL and access statement is sent with (contract section 6.2): clustered and Cloud
    /// targets set `alter_sync` and wait for the end of the query (`wait_end_of_query` is a parameter of the HTTP
    /// interface).
    ///
    /// The clustered variant also raises `distributed_ddl_entry_format_version`: ClickHouse refuses an
    /// `ON CLUSTER` statement that creates a text index with a preprocessor unless the entry format is at least 8
    /// and every host sets the server setting `distributed_ddl_use_initial_user_and_roles`. The server setting is
    /// a deployment requirement that the operator configures.
    pub fn ddl_settings(&self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::SingleNode => &[],
            Self::Clustered { .. } => &[
                ("alter_sync", "2"),
                ("wait_end_of_query", "1"),
                ("distributed_ddl_entry_format_version", "8"),
            ],
            Self::Cloud => &[("alter_sync", "2"), ("wait_end_of_query", "1")],
        }
    }
}

/// The replication path and replica name of the clustered variant; ClickHouse expands the macros per replica.
const REPLICATED_ARGS: &str = "('/clickhouse/tables/{shard}/{database}/{table}', '{replica}')";

fn engine(kind: TableKind, variant: &Variant) -> String {
    let base = match kind {
        TableKind::Plain => "MergeTree",
        TableKind::Aggregating => "AggregatingMergeTree",
    };
    match variant {
        Variant::SingleNode | Variant::Cloud => base.to_owned(),
        Variant::Clustered { .. } => format!("Replicated{base}{REPLICATED_ARGS}"),
    }
}

/// The partition expression at a granularity, or `None` for an unpartitioned table.
pub fn partition_by(expr: PartitionExpr, granularity: Granularity) -> Option<String> {
    match (expr, granularity) {
        (PartitionExpr::None, _) => None,
        (PartitionExpr::Day(c), Granularity::Day) => Some(format!("toDate({c})")),
        (PartitionExpr::DateColumn(c), Granularity::Day) => Some(c.to_owned()),
        // `toMonday` is applied to the `Date`, never to the `DateTime64`: on a `Date` the first days of 1970
        // saturate to 1970-01-01, on a `DateTime64` they wrap to a date in 2149.
        (PartitionExpr::Day(c), Granularity::Week) => Some(format!("toMonday(toDate({c}))")),
        (PartitionExpr::DateColumn(c), Granularity::Week) => Some(format!("toMonday({c})")),
        (PartitionExpr::Month(c), _) => Some(format!("toYYYYMM({c})")),
    }
}

fn index_type(index: &SkipIndex) -> String {
    match index.kind {
        IndexKind::Bloom => "bloom_filter(0.01)".to_owned(),
        IndexKind::Set(n) => format!("set({n})"),
        IndexKind::MinMax => "minmax".to_owned(),
        IndexKind::Text { tokenizer, lower } => {
            let tokenizer = match tokenizer {
                Tokenizer::Ngrams(n) => format!("ngrams({n})"),
                Tokenizer::SplitByNonAlpha => "splitByNonAlpha".to_owned(),
                Tokenizer::Array => "array".to_owned(),
            };
            if lower {
                format!(
                    "text(tokenizer = {tokenizer}, preprocessor = lower({}))",
                    index.column
                )
            } else {
                format!("text(tokenizer = {tokenizer})")
            }
        }
    }
}

fn key_list(columns: &[&str]) -> String {
    match columns {
        [one] => (*one).to_owned(),
        many => format!("({})", many.join(", ")),
    }
}

/// `CREATE TABLE IF NOT EXISTS crucible.<table>` for one variant and granularity.
pub fn create_table(def: &TableDef, variant: &Variant, granularity: Granularity) -> String {
    let mut sql = format!(
        "CREATE TABLE IF NOT EXISTS {DATABASE}.{}{}\n(\n",
        def.name(),
        variant.on_cluster()
    );
    let mut lines: Vec<String> = def
        .columns
        .iter()
        .map(|c| format!("    `{}` {} CODEC({})", c.name, c.ty, c.codec))
        .collect();
    lines.extend(def.indexes.iter().map(|i| {
        format!(
            "    INDEX idx_{} {} TYPE {} GRANULARITY {}",
            i.column,
            i.column,
            index_type(i),
            i.granularity
        )
    }));
    sql.push_str(&lines.join(",\n"));
    // Writing to a String cannot fail.
    let _ = write!(sql, "\n)\nENGINE = {}", engine(def.kind, variant));
    if let Some(expr) = partition_by(def.partition, granularity) {
        let _ = write!(sql, "\nPARTITION BY {expr}");
    }
    let _ = write!(sql, "\nPRIMARY KEY {}", key_list(def.primary_key()));
    let _ = write!(sql, "\nORDER BY {}", key_list(def.order_by));
    let mut settings: Vec<String> = def
        .settings
        .iter()
        .map(|(name, value)| format!("{name} = {value}"))
        .collect();
    match variant {
        Variant::SingleNode => {
            settings.push(format!(
                "non_replicated_deduplication_window = {DEDUP_WINDOW}"
            ));
        }
        Variant::Clustered { .. } | Variant::Cloud => {
            settings.push(format!("replicated_deduplication_window = {DEDUP_WINDOW}"));
            settings.push(format!(
                "replicated_deduplication_window_seconds = {DEDUP_WINDOW_SECONDS}"
            ));
        }
    }
    let _ = write!(sql, "\nSETTINGS {}", settings.join(", "));
    sql
}

/// `CREATE DATABASE IF NOT EXISTS <name>`.
pub fn create_database(name: &str, variant: &Variant) -> String {
    format!(
        "CREATE DATABASE IF NOT EXISTS {name}{}",
        variant.on_cluster()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_are_plain() {
        assert!(Ident::new("crucible_admin").is_ok());
        assert!(Ident::new("_c1").is_ok());
        for bad in [
            "",
            "1abc",
            "a-b",
            "a b",
            "a'b",
            "a;b",
            "a`b",
            &"x".repeat(65),
        ] {
            assert!(Ident::new(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn granularity_changes_only_daily_partitions() {
        use PartitionExpr::{DateColumn, Day, Month, None};
        assert_eq!(
            partition_by(Day("t"), Granularity::Day).as_deref(),
            Some("toDate(t)")
        );
        assert_eq!(
            partition_by(Day("t"), Granularity::Week).as_deref(),
            Some("toMonday(toDate(t))")
        );
        assert_eq!(
            partition_by(DateColumn("d"), Granularity::Day).as_deref(),
            Some("d")
        );
        assert_eq!(
            partition_by(DateColumn("d"), Granularity::Week).as_deref(),
            Some("toMonday(d)")
        );
        assert_eq!(
            partition_by(Month("t"), Granularity::Week).as_deref(),
            Some("toYYYYMM(t)")
        );
        assert_eq!(partition_by(None, Granularity::Week), Option::None);
    }
}
