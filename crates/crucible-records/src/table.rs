//! Table ids (contract section 6.3) and ClickHouse partition ids (contract section 5.2).
//!
//! Routing a row to its partition is owned by `crucible-schema`; this module only names tables and represents,
//! formats and parses partition ids.

use core::fmt;

use crate::time::CivilDate;

/// A ClickHouse table. The number is stable: it appears in batch tokens and in the checkpoint bitmap.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u8)]
pub enum TableId {
    /// Narrow span columns and previews.
    SpanCore = 1,
    /// Full span inputs, outputs and attributes.
    SpanPayload = 2,
    /// One row per MCP operation.
    McpOps = 3,
    /// Opt-in MCP arguments and results.
    McpContent = 4,
    /// Tools seen in listings.
    McpToolInventory = 5,
    /// Score facts.
    FactScores = 6,
    /// Tag facts.
    FactTags = 7,
    /// Flag facts.
    FactFlags = 8,
    /// Correction facts.
    FactCorrections = 9,
    /// Root-anchored trace list driver.
    TraceHeads = 10,
    /// Mergeable per-trace partials.
    TraceStats = 11,
    /// Per-session daily rollup.
    SessionRollup = 12,
    /// Per-user daily rollup.
    UserRollup = 13,
    /// Per-minute span metrics.
    SpanMetrics = 14,
    /// Per-minute MCP tool rollup.
    McpToolRollup = 15,
    /// MCP deploys seen.
    McpDeploys = 16,
    /// Startup deduplication self-test.
    DedupSelftest = 17,
    /// Canary rows for the SQL workspace self-probe; never written by the shipper.
    SqlProbe = 18,
    /// Per-hour MCP tool rollup: the hourly tier of [`TableId::McpToolRollup`].
    McpToolRollupHourly = 19,
}

impl TableId {
    /// Every table in id order.
    pub const ALL: [Self; 19] = [
        Self::SpanCore,
        Self::SpanPayload,
        Self::McpOps,
        Self::McpContent,
        Self::McpToolInventory,
        Self::FactScores,
        Self::FactTags,
        Self::FactFlags,
        Self::FactCorrections,
        Self::TraceHeads,
        Self::TraceStats,
        Self::SessionRollup,
        Self::UserRollup,
        Self::SpanMetrics,
        Self::McpToolRollup,
        Self::McpDeploys,
        Self::DedupSelftest,
        Self::SqlProbe,
        Self::McpToolRollupHourly,
    ];

    /// The stable number.
    pub const fn id(self) -> u8 {
        self as u8
    }

    /// The table with this number.
    pub const fn from_id(id: u8) -> Option<Self> {
        if id >= 1 && id as usize <= Self::ALL.len() {
            Some(Self::ALL[id as usize - 1])
        } else {
            None
        }
    }

    /// The table name inside the `crucible` database.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::SpanCore => "span_core",
            Self::SpanPayload => "span_payload",
            Self::McpOps => "mcp_ops",
            Self::McpContent => "mcp_content",
            Self::McpToolInventory => "mcp_tool_inventory",
            Self::FactScores => "fact_scores",
            Self::FactTags => "fact_tags",
            Self::FactFlags => "fact_flags",
            Self::FactCorrections => "fact_corrections",
            Self::TraceHeads => "trace_heads",
            Self::TraceStats => "trace_stats",
            Self::SessionRollup => "session_rollup",
            Self::UserRollup => "user_rollup",
            Self::SpanMetrics => "span_metrics",
            Self::McpToolRollup => "mcp_tool_rollup",
            Self::McpDeploys => "mcp_deploys",
            Self::DedupSelftest => "dedup_selftest",
            Self::SqlProbe => "sql_probe",
            Self::McpToolRollupHourly => "mcp_tool_rollup_hourly",
        }
    }

    /// The table with this name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|t| t.name() == name)
    }
}

/// A partition id that failed to parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid partition id")]
pub struct PartitionParseError;

/// The ClickHouse partition id string of a table's partition expression: `YYYYMMDD` for `toDate` (and for
/// `toMonday`, the Monday's date), `YYYYMM` for `toYYYYMM`, `all` for unpartitioned tables.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct PartitionId(Repr);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum Repr {
    Date(CivilDate),
    Month { year: i32, month: u8 },
    Unpartitioned,
}

impl PartitionId {
    /// The single partition of an unpartitioned table, which ClickHouse calls `all`.
    pub const UNPARTITIONED: Self = Self(Repr::Unpartitioned);

    /// A date partition (`toDate`, or `toMonday` with the Monday's date).
    pub const fn date(date: CivilDate) -> Self {
        Self(Repr::Date(date))
    }

    /// A month partition (`toYYYYMM`), or `None` for a month outside 1 to 12.
    pub const fn month(year: i32, month: u8) -> Option<Self> {
        match month {
            1..=12 => Some(Self(Repr::Month { year, month })),
            _ => None,
        }
    }

    /// The date of a date partition.
    pub const fn as_date(&self) -> Option<CivilDate> {
        match self.0 {
            Repr::Date(d) => Some(d),
            _ => None,
        }
    }

    /// The year and month of a month partition.
    pub const fn as_month(&self) -> Option<(i32, u8)> {
        match self.0 {
            Repr::Month { year, month } => Some((year, month)),
            _ => None,
        }
    }

    /// Whether this is the `all` partition.
    pub const fn is_unpartitioned(&self) -> bool {
        matches!(self.0, Repr::Unpartitioned)
    }

    /// Parses `YYYYMMDD`, `YYYYMM` or `all`.
    pub fn parse(s: &str) -> Result<Self, PartitionParseError> {
        if s == "all" {
            return Ok(Self::UNPARTITIONED);
        }
        if !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PartitionParseError);
        }
        let num = |range: core::ops::Range<usize>| -> Result<u32, PartitionParseError> {
            s.get(range)
                .and_then(|t| t.parse::<u32>().ok())
                .ok_or(PartitionParseError)
        };
        match s.len() {
            8 => {
                let (y, m, d) = (num(0..4)?, num(4..6)?, num(6..8)?);
                let date = CivilDate::new(y as i32, m as u8, d as u8).ok_or(PartitionParseError)?;
                Ok(Self::date(date))
            }
            6 => {
                let (y, m) = (num(0..4)?, num(4..6)?);
                Self::month(y as i32, m as u8).ok_or(PartitionParseError)
            }
            _ => Err(PartitionParseError),
        }
    }
}

impl fmt::Display for PartitionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Repr::Date(d) => write!(f, "{:04}{:02}{:02}", d.year(), d.month(), d.day()),
            Repr::Month { year, month } => write!(f, "{year:04}{month:02}"),
            Repr::Unpartitioned => f.write_str("all"),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn table_ids_are_stable() {
        for (i, t) in TableId::ALL.iter().enumerate() {
            assert_eq!(usize::from(t.id()), i + 1);
            assert_eq!(TableId::from_id(t.id()), Some(*t));
            assert_eq!(TableId::from_name(t.name()), Some(*t));
        }
        assert_eq!(TableId::from_id(0), None);
        assert_eq!(TableId::from_id(20), None);
        assert_eq!(TableId::McpToolRollupHourly.id(), 19);
        assert_eq!(TableId::TraceStats.id(), 11);
        assert_eq!(TableId::McpToolRollup.name(), "mcp_tool_rollup");
    }

    #[test]
    fn partition_ids_format_like_clickhouse() {
        let d = CivilDate::new(2026, 11, 17).unwrap();
        assert_eq!(PartitionId::date(d).to_string(), "20261117");
        assert_eq!(PartitionId::month(2026, 11).unwrap().to_string(), "202611");
        assert_eq!(PartitionId::UNPARTITIONED.to_string(), "all");
        for bad in [
            "", "2026111", "20261317", "20260230", "202613", "2026-11", "ALL", "+0261117",
        ] {
            assert!(PartitionId::parse(bad).is_err(), "{bad}");
        }
    }

    proptest! {
        #[test]
        fn partition_ids_round_trip(days in 0i64..47_482, month in 1u8..=12, year in 1970i32..2100) {
            let d = PartitionId::date(CivilDate::from_day_number(days));
            prop_assert_eq!(PartitionId::parse(&d.to_string()), Ok(d));
            let m = PartitionId::month(year, month).unwrap();
            prop_assert_eq!(PartitionId::parse(&m.to_string()), Ok(m));
        }
    }
}
