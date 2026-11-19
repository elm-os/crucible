//! The definition of an alert rule (contract 12.6): one measure of the span rollup or of the MCP rollup, at most
//! two dimensions, a comparison with a threshold, and the timing of its evaluation; an MCP rule also names its
//! side and capture source (contract 9.5). It is stored as the JSON document of the rule row's `spec` column.

use crucible_control::{JsonDoc, TxError};
use crucible_plan::{
    Aggregation, DimensionId, Fill, Filter, Granularity, MeasureId, MetricsQuery, Source,
    SourceChoice,
};
use crucible_records::{ChannelId, McpSide};
use serde_json::{Value, json};

use crate::machine::Params;

/// The most series one rule tracks.
pub const MAX_SERIES: u32 = 50;
/// The most channels one rule notifies.
pub const MAX_CHANNELS: usize = 10;
/// The shortest and the longest window, in seconds.
pub const WINDOW_SECS: (u32, u32) = (60, 86_400);
/// The shortest and the longest interval between evaluations, in seconds.
pub const INTERVAL_SECS: (u32, u32) = (60, 86_400);
/// The longest `for`, in seconds.
pub const FOR_MAX_SECS: u32 = 86_400;
/// The shortest and the longest time between repeated notifications of a firing series, in seconds.
pub const RENOTIFY_SECS: (u32, u32) = (300, 604_800);
/// The default of `renotify`: four hours.
pub const RENOTIFY_DEFAULT_SECS: u32 = 14_400;
/// The longest time without rows before the no-data policy applies, in seconds.
pub const NO_DATA_FOR_MAX_SECS: u32 = 86_400;
/// The default of `no_data_for`: five minutes.
pub const NO_DATA_FOR_DEFAULT_SECS: u32 = 300;
/// The longest name of a rule, in characters.
pub const NAME_MAX_CHARS: usize = 120;

/// How a value is compared with the threshold.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Comparison {
    /// Above the threshold.
    Gt,
    /// At or above the threshold.
    Gte,
    /// Below the threshold.
    Lt,
    /// At or below the threshold.
    Lte,
}

impl Comparison {
    /// Every comparison.
    pub const ALL: [Self; 4] = [Self::Gt, Self::Gte, Self::Lt, Self::Lte];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
        }
    }

    /// The comparison with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == name)
    }

    /// Whether `value` breaches `threshold`.
    pub const fn holds(self, value: i64, threshold: i64) -> bool {
        match self {
            Self::Gt => value > threshold,
            Self::Gte => value >= threshold,
            Self::Lt => value < threshold,
            Self::Lte => value <= threshold,
        }
    }

    /// Whether larger values are nearer to a breach.
    pub const fn upward(self) -> bool {
        matches!(self, Self::Gt | Self::Gte)
    }
}

/// What a series becomes when it has had no rows for the rule's no-data time.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NoDataPolicy {
    /// The state `no_data`, with one notification.
    NoData,
    /// As if the condition did not hold.
    Ok,
    /// The series keeps its state.
    Keep,
}

impl NoDataPolicy {
    /// Every policy.
    pub const ALL: [Self; 3] = [Self::NoData, Self::Ok, Self::Keep];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoData => "no_data",
            Self::Ok => "ok",
            Self::Keep => "keep",
        }
    }

    /// The policy with this wire name.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// What an `mcp_ops` rule reads (contract 9.5): one side, under a capture source. `auto` is resolved at each
/// evaluation, for that evaluation's window, so the rule follows the sources each window has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct McpRuleRead {
    /// The side: `client` or `server`.
    pub side: McpSide,
    /// `auto` or one capture-source class.
    pub source: SourceChoice,
}

/// The definition of a rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RuleSpec {
    /// The source: `spans` (the span rollup) or `mcp_ops` (the MCP rollup).
    pub source: Source,
    /// The side and capture source of an `mcp_ops` rule; `None` for a `spans` rule.
    pub mcp: Option<McpRuleRead>,
    /// The measure.
    pub measure: MeasureId,
    /// How it is aggregated over the window.
    pub aggregation: Aggregation,
    /// Up to two dimensions; each key of them is one series.
    pub dimensions: Vec<DimensionId>,
    /// The comparison.
    pub comparison: Comparison,
    /// The threshold, in the measure's stored unit: a count, microseconds or nano-USD.
    pub threshold: i64,
    /// The window each evaluation looks at, in seconds: whole minutes from one minute to 24 hours.
    pub window_secs: u32,
    /// The time between evaluations, in seconds; at least 60.
    pub interval_secs: u32,
    /// How long the condition must hold before the series fires, in seconds.
    pub for_secs: u32,
    /// The no-data policy.
    pub no_data: NoDataPolicy,
    /// How long a series may have no rows before the policy applies, in seconds.
    pub no_data_for_secs: u32,
    /// How long after a notification a still-firing series is notified again, in seconds.
    pub renotify_secs: u32,
    /// The channels notified.
    pub channels: Vec<ChannelId>,
}

/// A part of a rule that the registry or a limit does not allow.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SpecIssue {
    /// The member of the rule to change.
    pub field: &'static str,
    /// What is wrong and what is allowed.
    pub message: String,
}

fn issue(field: &'static str, message: impl Into<String>) -> SpecIssue {
    SpecIssue {
        field,
        message: message.into(),
    }
}

impl RuleSpec {
    /// Everything about the definition that is not allowed; empty when it can be saved.
    pub fn issues(&self) -> Vec<SpecIssue> {
        let mut issues = Vec::new();
        match (self.source, self.mcp) {
            (Source::Spans, None) => {}
            (Source::McpOps, Some(mcp)) if mcp.side != McpSide::None => {}
            (Source::McpOps, _) => issues.push(issue(
                "side",
                "an mcp_ops rule reads one side: `client` or `server`",
            )),
            (Source::Spans, Some(_)) => issues.push(issue(
                "side",
                "only an mcp_ops rule takes a side and a capture source",
            )),
            _ => issues.push(issue(
                "source",
                "alert rules read the span rollup or the MCP rollup; use the source `spans` or `mcp_ops`",
            )),
        }
        if self.dimensions.iter().any(|d| d.source() != self.source) {
            issues.push(issue(
                "dimensions",
                "a dimension belongs to another source; GET metrics/schema lists each source's dimensions",
            ));
        }
        if self.measure.source() != self.source {
            issues.push(issue("measure", "the measure belongs to another source"));
        } else if !self.measure.aggregations().contains(&self.aggregation) {
            let allowed: Vec<&str> = self
                .measure
                .aggregations()
                .iter()
                .map(|a| a.as_str())
                .collect();
            issues.push(issue(
                "aggregation",
                format!(
                    "the measure `{}` allows {}",
                    self.measure.as_str(),
                    allowed.join(", ")
                ),
            ));
        }
        match self.dimensions.as_slice() {
            [] | [_] => {}
            [first, second] if first != second => {}
            [_, _] => issues.push(issue("dimensions", "a dimension is listed twice")),
            _ => issues.push(issue("dimensions", "at most two dimensions are allowed")),
        }
        let (shortest, longest) = WINDOW_SECS;
        if !(shortest..=longest).contains(&self.window_secs) || !self.window_secs.is_multiple_of(60)
        {
            issues.push(issue(
                "window",
                "the window is a whole number of minutes from 1 minute to 24 hours",
            ));
        }
        let (shortest, longest) = INTERVAL_SECS;
        if !(shortest..=longest).contains(&self.interval_secs) {
            issues.push(issue(
                "interval",
                "the interval is from 60 seconds to 24 hours",
            ));
        }
        if self.for_secs > FOR_MAX_SECS {
            issues.push(issue("for", "`for` is at most 24 hours"));
        }
        if self.no_data_for_secs > NO_DATA_FOR_MAX_SECS {
            issues.push(issue("no_data_for", "`no_data_for` is at most 24 hours"));
        }
        let (shortest, longest) = RENOTIFY_SECS;
        if !(shortest..=longest).contains(&self.renotify_secs) {
            issues.push(issue("renotify", "`renotify` is from 5 minutes to 7 days"));
        }
        if self.channels.len() > MAX_CHANNELS {
            issues.push(issue("channels", "a rule notifies at most 10 channels"));
        }
        let mut seen = self.channels.clone();
        seen.sort_unstable();
        seen.dedup();
        if seen.len() != self.channels.len() {
            issues.push(issue("channels", "a channel is listed twice"));
        }
        issues
    }

    /// The query of the rule's read.
    pub fn query(&self) -> MetricsQuery {
        MetricsQuery {
            source: self.source,
            measures: vec![(self.measure, self.aggregation)],
            dimensions: self.dimensions.clone(),
            filter: Filter::all(),
            granularity: Granularity::Auto,
            fill: Fill::Zero,
            top_n: None,
        }
    }

    /// What the state machine needs of the rule.
    pub const fn params(&self) -> Params {
        Params {
            for_secs: self.for_secs,
            renotify_secs: self.renotify_secs,
            no_data: self.no_data,
            no_data_for_secs: self.no_data_for_secs,
        }
    }

    /// The stored document. An `mcp_ops` rule's side and capture source are the members `side` and
    /// `capture_source`, absent for a `spans` rule.
    pub fn to_doc(&self) -> JsonDoc {
        let mut value = json!({
            "source": self.source.as_str(),
            "measure": self.measure.as_str(),
            "aggregation": self.aggregation.as_str(),
            "dimensions": self.dimensions.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
            "comparison": self.comparison.as_str(),
            "threshold": self.threshold,
            "window_secs": self.window_secs,
            "interval_secs": self.interval_secs,
            "for_secs": self.for_secs,
            "no_data": self.no_data.as_str(),
            "no_data_for_secs": self.no_data_for_secs,
            "renotify_secs": self.renotify_secs,
            "channels": self.channels.iter().map(ToString::to_string).collect::<Vec<_>>(),
        });
        if let (Some(mcp), Some(members)) = (self.mcp, value.as_object_mut()) {
            members.insert("side".to_owned(), json!(mcp.side.as_str()));
            members.insert("capture_source".to_owned(), json!(mcp.source.as_str()));
        }
        // A document built from a `json!` object always parses.
        JsonDoc::parse(&value.to_string()).unwrap_or_else(|_| JsonDoc::empty())
    }

    /// The definition a stored document holds.
    pub fn from_doc(doc: &JsonDoc) -> Result<Self, TxError> {
        let value: Value = serde_json::from_str(&doc.to_text())
            .map_err(|error| TxError::Invalid(format!("alert rule: {error}")))?;
        let bad = |member: &str| TxError::Invalid(format!("alert rule: the stored `{member}`"));
        let text = |member: &str| {
            value
                .get(member)
                .and_then(Value::as_str)
                .ok_or_else(|| bad(member))
        };
        let seconds = |member: &str| {
            value
                .get(member)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| bad(member))
        };
        let list = |member: &str| {
            value
                .get(member)
                .and_then(Value::as_array)
                .ok_or_else(|| bad(member))?
                .iter()
                .map(|item| item.as_str().ok_or_else(|| bad(member)))
                .collect::<Result<Vec<&str>, TxError>>()
        };
        let mcp = match value.get("side") {
            None => None,
            Some(_) => Some(McpRuleRead {
                side: McpSide::parse(text("side")?).ok_or_else(|| bad("side"))?,
                source: SourceChoice::parse(text("capture_source")?)
                    .ok_or_else(|| bad("capture_source"))?,
            }),
        };
        Ok(Self {
            source: Source::parse(text("source")?).ok_or_else(|| bad("source"))?,
            mcp,
            measure: MeasureId::parse(text("measure")?).ok_or_else(|| bad("measure"))?,
            aggregation: Aggregation::parse(text("aggregation")?)
                .ok_or_else(|| bad("aggregation"))?,
            dimensions: list("dimensions")?
                .into_iter()
                .map(|d| DimensionId::parse(d).ok_or_else(|| bad("dimensions")))
                .collect::<Result<_, _>>()?,
            comparison: Comparison::parse(text("comparison")?).ok_or_else(|| bad("comparison"))?,
            threshold: value
                .get("threshold")
                .and_then(Value::as_i64)
                .ok_or_else(|| bad("threshold"))?,
            window_secs: seconds("window_secs")?,
            interval_secs: seconds("interval_secs")?,
            for_secs: seconds("for_secs")?,
            no_data: NoDataPolicy::parse(text("no_data")?).ok_or_else(|| bad("no_data"))?,
            no_data_for_secs: seconds("no_data_for_secs")?,
            renotify_secs: seconds("renotify_secs")?,
            channels: list("channels")?
                .into_iter()
                .map(|c| ChannelId::parse(c).map_err(|_| bad("channels")))
                .collect::<Result<_, _>>()?,
        })
    }
}

/// A stored value of `measure` as people read it: a count as it is, microseconds as a duration, nano-USD as
/// dollars, parts per million as a percentage.
pub fn display_value(measure: MeasureId, value: i64) -> String {
    match measure.unit() {
        "ppm" => {
            let sign = if value < 0 { "-" } else { "" };
            let ppm = value.unsigned_abs();
            let fraction = format!("{:04}", ppm % 10_000);
            match fraction.trim_end_matches('0') {
                "" => format!("{sign}{}%", ppm / 10_000),
                digits => format!("{sign}{}.{digits}%", ppm / 10_000),
            }
        }
        "nusd" => {
            let sign = if value < 0 { "-" } else { "" };
            let nanos = value.unsigned_abs();
            let text = format!("{}.{:09}", nanos / 1_000_000_000, nanos % 1_000_000_000);
            let trimmed = text.trim_end_matches('0');
            // Keep cents: "$1.5" reads worse than "$1.50".
            let dot = trimmed.find('.').unwrap_or(trimmed.len());
            let decimals = trimmed.len().saturating_sub(dot + 1);
            let padded = format!("{trimmed}{}", "0".repeat(2usize.saturating_sub(decimals)));
            format!("{sign}${padded}")
        }
        "us" => match value {
            v if v.abs() >= 1_000_000 => {
                format!("{}.{:02} s", v / 1_000_000, (v.abs() % 1_000_000) / 10_000)
            }
            v if v.abs() >= 1_000 => format!("{}.{} ms", v / 1_000, (v.abs() % 1_000) / 100),
            v => format!("{v} µs"),
        },
        _ => value.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crucible_records::Uuid;

    use super::*;

    pub(crate) fn spec() -> RuleSpec {
        RuleSpec {
            source: Source::Spans,
            mcp: None,
            measure: MeasureId::ErrorCount,
            aggregation: Aggregation::Sum,
            dimensions: vec![DimensionId::Environment, DimensionId::Model],
            comparison: Comparison::Gt,
            threshold: 5,
            window_secs: 300,
            interval_secs: 60,
            for_secs: 120,
            no_data: NoDataPolicy::NoData,
            no_data_for_secs: NO_DATA_FOR_DEFAULT_SECS,
            renotify_secs: RENOTIFY_DEFAULT_SECS,
            channels: vec![ChannelId::from_uuid(Uuid::from_v7_parts(
                1_800_000_000_000,
                7,
            ))],
        }
    }

    #[test]
    fn a_definition_survives_its_document() {
        let spec = spec();
        assert!(spec.issues().is_empty());
        assert_eq!(RuleSpec::from_doc(&spec.to_doc()).unwrap(), spec);
        assert!(RuleSpec::from_doc(&JsonDoc::empty()).is_err());
    }

    #[test]
    fn each_limit_names_its_member() {
        let fields = |change: fn(&mut RuleSpec)| {
            let mut spec = spec();
            change(&mut spec);
            spec.issues()
                .into_iter()
                .map(|i| i.field)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            fields(|s| s.source = Source::Traces),
            ["source", "dimensions", "measure"]
        );
        assert_eq!(
            fields(|s| s.dimensions = vec![DimensionId::McpServer]),
            ["dimensions"]
        );
        assert_eq!(fields(|s| s.mcp = mcp_spec().mcp), ["side"]);
        assert_eq!(
            fields(|s| s.aggregation = Aggregation::P95),
            ["aggregation"]
        );
        assert_eq!(fields(|s| s.measure = MeasureId::Traces), ["measure"]);
        assert_eq!(
            fields(|s| s.dimensions = vec![DimensionId::Level, DimensionId::Level]),
            ["dimensions"]
        );
        assert_eq!(
            fields(|s| s.dimensions =
                vec![DimensionId::Level, DimensionId::Model, DimensionId::Release]),
            ["dimensions"]
        );
        assert_eq!(fields(|s| s.window_secs = 59), ["window"]);
        assert_eq!(fields(|s| s.window_secs = 90), ["window"]);
        assert_eq!(fields(|s| s.window_secs = 86_460), ["window"]);
        assert_eq!(fields(|s| s.interval_secs = 59), ["interval"]);
        assert_eq!(fields(|s| s.for_secs = 86_401), ["for"]);
        assert_eq!(fields(|s| s.renotify_secs = 299), ["renotify"]);
        assert_eq!(fields(|s| s.no_data_for_secs = 86_401), ["no_data_for"]);
        assert_eq!(fields(|s| s.channels.push(s.channels[0])), ["channels"]);
        assert!(
            fields(|s| {
                s.measure = MeasureId::Duration;
                s.aggregation = Aggregation::P95;
            })
            .is_empty()
        );
    }

    /// An `mcp_ops` rule: the server-side error rate per server, above 5%.
    pub(crate) fn mcp_spec() -> RuleSpec {
        RuleSpec {
            source: Source::McpOps,
            mcp: Some(McpRuleRead {
                side: McpSide::Server,
                source: SourceChoice::Auto,
            }),
            measure: MeasureId::McpErrorRate,
            aggregation: Aggregation::Rate,
            dimensions: vec![DimensionId::McpServer],
            threshold: 50_000,
            ..spec()
        }
    }

    #[test]
    fn an_mcp_rule_names_its_side_and_reads_its_own_measures_and_dimensions() {
        let rule = mcp_spec();
        assert!(rule.issues().is_empty());
        assert_eq!(RuleSpec::from_doc(&rule.to_doc()).unwrap(), rule);
        let proxy = RuleSpec {
            mcp: Some(McpRuleRead {
                side: McpSide::Client,
                source: SourceChoice::Class(crucible_plan::CaptureClass::Proxy),
            }),
            ..mcp_spec()
        };
        assert_eq!(RuleSpec::from_doc(&proxy.to_doc()).unwrap(), proxy);
        // A spans rule's document has no side, and reads back without one.
        assert!(!spec().to_doc().to_text().contains("side"));
        let fields = |change: fn(&mut RuleSpec)| {
            let mut rule = mcp_spec();
            change(&mut rule);
            rule.issues()
                .into_iter()
                .map(|i| i.field)
                .collect::<Vec<_>>()
        };
        assert_eq!(fields(|s| s.mcp = None), ["side"]);
        assert_eq!(
            fields(|s| s.mcp = Some(McpRuleRead {
                side: McpSide::None,
                source: SourceChoice::Auto,
            })),
            ["side"]
        );
        assert_eq!(
            fields(|s| s.dimensions = vec![DimensionId::Environment]),
            ["dimensions"]
        );
        assert_eq!(fields(|s| s.measure = MeasureId::ErrorCount), ["measure"]);
        assert_eq!(
            fields(|s| s.aggregation = Aggregation::Sum),
            ["aggregation"]
        );
        assert!(
            fields(|s| {
                s.measure = MeasureId::McpDurationP99;
                s.aggregation = Aggregation::P99;
                s.dimensions = vec![DimensionId::McpServer, DimensionId::McpTarget];
            })
            .is_empty()
        );
    }

    #[test]
    fn comparisons_hold_as_named() {
        assert!(Comparison::Gt.holds(6, 5) && !Comparison::Gt.holds(5, 5));
        assert!(Comparison::Gte.holds(5, 5) && !Comparison::Gte.holds(4, 5));
        assert!(Comparison::Lt.holds(4, 5) && !Comparison::Lt.holds(5, 5));
        assert!(Comparison::Lte.holds(5, 5) && !Comparison::Lte.holds(6, 5));
    }

    #[test]
    fn values_read_in_their_unit() {
        assert_eq!(display_value(MeasureId::Count, 1_234), "1234");
        assert_eq!(display_value(MeasureId::TotalCost, 1_500_000_000), "$1.50");
        assert_eq!(display_value(MeasureId::TotalCost, 12_340_000), "$0.01234");
        assert_eq!(display_value(MeasureId::TotalCost, 0), "$0.00");
        assert_eq!(display_value(MeasureId::Duration, 2_340_000), "2.34 s");
        assert_eq!(display_value(MeasureId::Duration, 340_500), "340.5 ms");
        assert_eq!(display_value(MeasureId::Duration, 950), "950 µs");
        assert_eq!(display_value(MeasureId::McpErrorRate, 12_500), "1.25%");
        assert_eq!(display_value(MeasureId::McpErrorRate, 50_000), "5%");
        assert_eq!(display_value(MeasureId::McpErrorRate, 1_000_000), "100%");
        assert_eq!(display_value(MeasureId::McpErrorRate, 5), "0.0005%");
    }
}
