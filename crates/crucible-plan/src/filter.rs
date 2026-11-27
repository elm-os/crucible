//! The list filter AST (contract sections 8.8 and 12.4): what the trace list, the span list, search and the
//! metrics query filter by, its canonical form and hash for cursor binding, and its compilation to a [`Pred`]
//! over the table a read is over.
//!
//! A filter is compiled for one [`FilterTarget`]. A target evaluates only the fields its table carries, and a
//! condition on any other field is refused with [`PlanError::UnsupportedFilter`], which names the field: no
//! condition is ever dropped silently. [`Filter::split`] is for the reads that span two tables (the span list
//! with a metadata condition, search with conditions on `span_core`): it separates the top-level conjuncts a
//! target can evaluate from the rest, which the caller must hand to the other table or refuse.
//!
//! # Canonical form
//!
//! Two filters that differ only in the following have the same canonical form, and so the same bytes and hash:
//!
//! - the order and repetition of the operands of `And` and `Or`, and of the values of a value list;
//! - nesting of `And` in `And` and of `Or` in `Or`;
//! - an `And` or `Or` around a single operand;
//! - a double negation.
//!
//! Nothing else is normalised: `Not(Or(a, b))` and `And(Not(a), Not(b))` are different filters. The canonical
//! bytes tag every node and prefix every list and string with its length, so no two canonical forms share their
//! bytes. The filter that keeps every row has the empty byte string.

use crucible_records::{FactTargetKind, Level, ObservationType, TableId, TagOp};
use crucible_schema::{
    Column, ColumnRef, FactTagsCol, SpanCoreCol, SpanMetricsCol, SpanPayloadCol, TraceHeadsCol,
};
use sha2::{Digest, Sha256};

use crate::error::PlanError;
use crate::literal::Literal;
use crate::node::{Aggregate, Having, Measure};
use crate::pred::{CmpOp, Pred};

/// A string field a filter tests for membership in a list of values.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum TextField {
    /// `environment`.
    Environment,
    /// `release`.
    Release,
    /// `version`.
    Version,
    /// `user_id`.
    UserId,
    /// `session_id`.
    SessionId,
}

/// A field a filter can test. The wire names are those of the metrics schema's filter fields.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum FilterField {
    /// One of the string fields.
    Text(TextField),
    /// `level`.
    Level,
    /// `observation_type`.
    ObservationType,
    /// `tags`.
    Tags,
    /// `metadata`: a key with a value.
    Metadata,
    /// `name`: a text match.
    Name,
}

impl FilterField {
    /// Every field.
    pub const ALL: [Self; 10] = [
        Self::Text(TextField::Environment),
        Self::Text(TextField::Release),
        Self::Text(TextField::Version),
        Self::Level,
        Self::ObservationType,
        Self::Text(TextField::UserId),
        Self::Text(TextField::SessionId),
        Self::Tags,
        Self::Metadata,
        Self::Name,
    ];

    /// The wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text(TextField::Environment) => "environment",
            Self::Text(TextField::Release) => "release",
            Self::Text(TextField::Version) => "version",
            Self::Text(TextField::UserId) => "user_id",
            Self::Text(TextField::SessionId) => "session_id",
            Self::Level => "level",
            Self::ObservationType => "observation_type",
            Self::Tags => "tags",
            Self::Metadata => "metadata",
            Self::Name => "name",
        }
    }

    /// The operator of the field's condition: `in` (one of a list of values), `has_any` (shares a value with
    /// a list), `eq` (a metadata key with a value) or `contains` (a substring without regard to ASCII case).
    /// Every condition can be negated and combined with `and` and `or`.
    pub const fn operator(self) -> &'static str {
        match self {
            Self::Text(_) | Self::Level | Self::ObservationType => "in",
            Self::Tags => "has_any",
            Self::Metadata => "eq",
            Self::Name => "contains",
        }
    }

    const fn tag(self) -> u8 {
        match self {
            Self::Text(TextField::Environment) => 10,
            Self::Text(TextField::Release) => 11,
            Self::Text(TextField::Version) => 12,
            Self::Text(TextField::UserId) => 13,
            Self::Text(TextField::SessionId) => 14,
            Self::Level => 20,
            Self::ObservationType => 21,
            Self::Tags => 22,
            Self::Metadata => 23,
            Self::Name => 24,
        }
    }
}

/// What a filter is compiled for: the table of a read, which decides the fields it can evaluate.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FilterTarget {
    /// `trace_heads` (the trace list): every field but `observation_type` and `metadata`. `tags` tests the
    /// trace's effective tags, its tag edits applied (contract 3.8).
    TraceHeads,
    /// `span_core` (the span list): every field but `metadata`.
    SpanCore,
    /// `span_payload` (search, and the metadata conditions of the span list): `user_id`, `session_id` and
    /// `metadata`.
    SpanPayload,
    /// `span_metrics` (the metrics query): `environment`, `release`, `level` and `observation_type`.
    SpanMetrics,
}

impl FilterTarget {
    /// What the target reads, for refusals.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TraceHeads => "the trace list",
            Self::SpanCore => "the span list",
            Self::SpanPayload => "span payloads",
            Self::SpanMetrics => "the span metrics rollup",
        }
    }

    /// The column that carries a field, for the fields that are one column.
    pub(crate) fn column(self, field: FilterField) -> Option<ColumnRef> {
        use FilterField::{Level, ObservationType, Tags, Text};
        use TextField::{Environment, Release, SessionId, UserId, Version};
        Some(match (self, field) {
            (Self::TraceHeads, Text(Environment)) => TraceHeadsCol::Environment.column_ref(),
            (Self::TraceHeads, Text(Release)) => TraceHeadsCol::Release.column_ref(),
            (Self::TraceHeads, Text(Version)) => TraceHeadsCol::Version.column_ref(),
            (Self::TraceHeads, Text(UserId)) => TraceHeadsCol::UserId.column_ref(),
            (Self::TraceHeads, Text(SessionId)) => TraceHeadsCol::SessionId.column_ref(),
            (Self::TraceHeads, Level) => TraceHeadsCol::Level.column_ref(),
            (Self::TraceHeads, Tags) => TraceHeadsCol::Tags.column_ref(),
            (Self::SpanCore, Text(Environment)) => SpanCoreCol::Environment.column_ref(),
            (Self::SpanCore, Text(Release)) => SpanCoreCol::Release.column_ref(),
            (Self::SpanCore, Text(Version)) => SpanCoreCol::Version.column_ref(),
            (Self::SpanCore, Text(UserId)) => SpanCoreCol::UserId.column_ref(),
            (Self::SpanCore, Text(SessionId)) => SpanCoreCol::SessionId.column_ref(),
            (Self::SpanCore, Level) => SpanCoreCol::Level.column_ref(),
            (Self::SpanCore, ObservationType) => SpanCoreCol::ObservationType.column_ref(),
            (Self::SpanCore, Tags) => SpanCoreCol::Tags.column_ref(),
            (Self::SpanPayload, Text(UserId)) => SpanPayloadCol::UserId.column_ref(),
            (Self::SpanPayload, Text(SessionId)) => SpanPayloadCol::SessionId.column_ref(),
            (Self::SpanMetrics, Text(Environment)) => SpanMetricsCol::Environment.column_ref(),
            (Self::SpanMetrics, Text(Release)) => SpanMetricsCol::Release.column_ref(),
            (Self::SpanMetrics, Level) => SpanMetricsCol::Level.column_ref(),
            (Self::SpanMetrics, ObservationType) => SpanMetricsCol::ObservationType.column_ref(),
            _ => return None,
        })
    }

    /// Whether the target can evaluate a condition on `field`.
    pub fn supports(self, field: FilterField) -> bool {
        match field {
            FilterField::Metadata => self == Self::SpanPayload,
            FilterField::Name => matches!(self, Self::TraceHeads | Self::SpanCore),
            other => self.column(other).is_some(),
        }
    }

    /// The fields the target can evaluate, in the order of [`FilterField::ALL`].
    pub fn fields(self) -> Vec<FilterField> {
        FilterField::ALL
            .into_iter()
            .filter(|field| self.supports(*field))
            .collect()
    }
}

/// A filter over the rows of a list, a search or a metrics query.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Filter {
    /// Every operand holds; the empty conjunction keeps every row.
    And(Vec<Filter>),
    /// Any operand holds.
    Or(Vec<Filter>),
    /// The operand does not hold.
    Not(Box<Filter>),
    /// A string field equal to one of the values.
    Text(TextField, Vec<String>),
    /// A level among the values.
    Level(Vec<Level>),
    /// An observation type among the values.
    ObservationType(Vec<ObservationType>),
    /// At least one of the tags.
    Tags(Vec<String>),
    /// A metadata entry `key = value`.
    Metadata {
        /// The metadata key.
        key: String,
        /// The metadata value.
        value: String,
    },
    /// A name that contains the text, without regard to ASCII case.
    Name(String),
}

/// The most values one member of a list filter takes: every value is a bound value of the read's statement.
pub const MAX_FILTER_VALUES: usize = 100;

/// The most tags a trace list filter tests. Each is resolved over the traces' tag edits with two sets of its own,
/// about twenty bound values of the statement, and a ClickHouse request holds at most 1,000 values and settings.
pub const TRACE_LIST_MAX_TAGS: usize = 20;

/// The most values a trace list filter holds in all its members together ([`Filter::values`]). The trace list
/// reads its heads and its span conditions in one statement, where each value is a bound value and each tag about
/// twenty; above this a request could carry more fields than ClickHouse takes.
pub const TRACE_LIST_MAX_VALUES: usize = 256;

/// `tags` on the trace list: the trace's effective tags have one of `values` (contract 3.8). Effective tags are the
/// head's ingest tags, plus the tags whose latest edit added them, minus those whose latest edit removed them. Per
/// tag, the traces whose latest trace-level edit in the window is an `add` or a `remove` are resolved over
/// `fact_tags` first (an edit whose latest row is a retraction counts as none), then semi-joined:
/// `(tags has the tag AND trace_id NOT IN removed) OR trace_id IN added`. More than [`TRACE_LIST_MAX_TAGS`] tags
/// are refused.
fn effective_tags(values: &[String]) -> Result<Pred, PlanError> {
    if values.len() > TRACE_LIST_MAX_TAGS {
        return Err(PlanError::ListLength("tags"));
    }
    let edited = |tag: &str, op: TagOp| -> Result<Pred, PlanError> {
        let latest = Aggregate::new(
            TableId::FactTags,
            Pred::And(vec![
                Pred::eq(
                    FactTagsCol::TargetKind,
                    Literal::Enum(FactTargetKind::Trace.value()),
                ),
                Pred::eq(FactTagsCol::Key, tag),
            ]),
            [FactTagsCol::TraceId],
            vec![
                Measure::LatestByVersion(FactTagsCol::Op.column_ref()),
                Measure::LatestByVersion(FactTagsCol::Retracted.column_ref()),
            ],
        )?
        .having(vec![
            Having {
                measure: 0,
                op: CmpOp::Eq,
                value: Literal::Enum(op.value()),
            },
            Having {
                measure: 1,
                op: CmpOp::Eq,
                value: Literal::UInt(0),
            },
        ])?;
        Ok(Pred::InSet(
            TraceHeadsCol::TraceId.column_ref(),
            Box::new(latest),
        ))
    };
    let mut any = Vec::with_capacity(values.len());
    for tag in values {
        let ingest = Pred::HasAny(
            TraceHeadsCol::Tags.column_ref(),
            vec![Literal::String(tag.clone())],
        );
        any.push(Pred::Or(vec![
            Pred::And(vec![
                ingest,
                Pred::Not(Box::new(edited(tag, TagOp::Remove)?)),
            ]),
            edited(tag, TagOp::Add)?,
        ]));
    }
    Ok(Pred::Or(any))
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u64).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn put_strs(out: &mut Vec<u8>, values: &[String]) {
    out.extend_from_slice(&(values.len() as u64).to_be_bytes());
    values.iter().for_each(|value| put_str(out, value));
}

fn put_bytes(out: &mut Vec<u8>, values: impl ExactSizeIterator<Item = u8>) {
    out.extend_from_slice(&(values.len() as u64).to_be_bytes());
    out.extend(values);
}

fn set<T: Ord + Clone>(values: &[T]) -> Vec<T> {
    let mut values = values.to_vec();
    values.sort_unstable();
    values.dedup();
    values
}

impl Filter {
    /// The filter that keeps every row.
    pub const fn all() -> Self {
        Self::And(Vec::new())
    }

    /// The values the filter names: each text value, tag and metadata entry. Levels and observation types, whose
    /// sets are small, and the name do not count.
    pub fn values(&self) -> usize {
        match self {
            Self::And(operands) | Self::Or(operands) => operands.iter().map(Self::values).sum(),
            Self::Not(operand) => operand.values(),
            Self::Text(_, values) | Self::Tags(values) => values.len(),
            Self::Metadata { .. } => 1,
            Self::Level(_) | Self::ObservationType(_) | Self::Name(_) => 0,
        }
    }

    /// The field of a condition; `None` for `And`, `Or` and `Not`.
    const fn field(&self) -> Option<FilterField> {
        match self {
            Self::And(_) | Self::Or(_) | Self::Not(_) => None,
            Self::Text(field, _) => Some(FilterField::Text(*field)),
            Self::Level(_) => Some(FilterField::Level),
            Self::ObservationType(_) => Some(FilterField::ObservationType),
            Self::Tags(_) => Some(FilterField::Tags),
            Self::Metadata { .. } => Some(FilterField::Metadata),
            Self::Name(_) => Some(FilterField::Name),
        }
    }

    /// The canonical form (see the module documentation).
    pub fn canonical(&self) -> Self {
        match self {
            Self::And(operands) | Self::Or(operands) => {
                let conjunction = matches!(self, Self::And(_));
                let mut flat = Vec::with_capacity(operands.len());
                for operand in operands {
                    match (operand.canonical(), conjunction) {
                        (Self::And(inner), true) | (Self::Or(inner), false) => flat.extend(inner),
                        (other, _) => flat.push(other),
                    }
                }
                flat.sort_by_cached_key(Self::encoded);
                flat.dedup();
                match (flat.len(), conjunction) {
                    (1, _) => flat.swap_remove(0),
                    (_, true) => Self::And(flat),
                    (_, false) => Self::Or(flat),
                }
            }
            Self::Not(operand) => match operand.canonical() {
                Self::Not(inner) => *inner,
                other => Self::Not(Box::new(other)),
            },
            Self::Text(field, values) => Self::Text(*field, set(values)),
            Self::Level(values) => Self::Level(set(values)),
            Self::ObservationType(values) => Self::ObservationType(set(values)),
            Self::Tags(values) => Self::Tags(set(values)),
            Self::Metadata { .. } | Self::Name(_) => self.clone(),
        }
    }

    /// The bytes of this filter as it stands: a tag per node, a length before every list and string.
    fn encoded(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::And(operands) | Self::Or(operands) => {
                out.push(if matches!(self, Self::And(_)) { 1 } else { 2 });
                out.extend_from_slice(&(operands.len() as u64).to_be_bytes());
                operands.iter().for_each(|operand| operand.encode(out));
            }
            Self::Not(operand) => {
                out.push(3);
                operand.encode(out);
            }
            Self::Text(field, values) => {
                out.push(FilterField::Text(*field).tag());
                put_strs(out, values);
            }
            Self::Level(values) => {
                out.push(FilterField::Level.tag());
                put_bytes(out, values.iter().map(|v| v.value()));
            }
            Self::ObservationType(values) => {
                out.push(FilterField::ObservationType.tag());
                put_bytes(out, values.iter().map(|v| v.value()));
            }
            Self::Tags(values) => {
                out.push(FilterField::Tags.tag());
                put_strs(out, values);
            }
            Self::Metadata { key, value } => {
                out.push(FilterField::Metadata.tag());
                put_str(out, key);
                put_str(out, value);
            }
            Self::Name(text) => {
                out.push(FilterField::Name.tag());
                put_str(out, text);
            }
        }
    }

    /// The bytes of the canonical form: what a cursor's filter hash covers. Empty for the filter that keeps
    /// every row.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        match self.canonical() {
            Self::And(operands) if operands.is_empty() => Vec::new(),
            canonical => canonical.encoded(),
        }
    }

    /// The filter hash of contract 12.4: the first 16 bytes of SHA-256 over [`Filter::canonical_bytes`]. It is
    /// the value `crucible_cursor::FilterHash::of` computes from the same bytes.
    pub fn hash(&self) -> [u8; 16] {
        let digest = Sha256::digest(self.canonical_bytes());
        let mut hash = [0; 16];
        hash.copy_from_slice(&digest[..16]);
        hash
    }

    /// The first field, in the filter's own order, that `target` cannot evaluate.
    pub fn unsupported(&self, target: FilterTarget) -> Option<FilterField> {
        match self {
            Self::And(operands) | Self::Or(operands) => operands
                .iter()
                .find_map(|operand| operand.unsupported(target)),
            Self::Not(operand) => operand.unsupported(target),
            condition => condition.field().filter(|field| !target.supports(*field)),
        }
    }

    /// The predicate of this filter's canonical form over the table of `target`, so equivalent filters give one
    /// statement. A condition on a field the target cannot evaluate is refused; the error names the field.
    pub fn pred(&self, target: FilterTarget) -> Result<Pred, PlanError> {
        self.canonical().compile(target)
    }

    fn compile(&self, target: FilterTarget) -> Result<Pred, PlanError> {
        let refused = |field: FilterField| PlanError::UnsupportedFilter {
            field: field.as_str(),
            target: target.as_str(),
        };
        let column = |field: FilterField| target.column(field).ok_or_else(|| refused(field));
        let all = |operands: &[Self]| -> Result<Vec<Pred>, PlanError> {
            operands
                .iter()
                .map(|operand| operand.compile(target))
                .collect()
        };
        Ok(match self {
            Self::And(operands) => Pred::And(all(operands)?),
            Self::Or(operands) => Pred::Or(all(operands)?),
            Self::Not(operand) => Pred::Not(Box::new(operand.compile(target)?)),
            Self::Text(field, values) => Pred::In(
                column(FilterField::Text(*field))?,
                values.iter().map(|v| Literal::String(v.clone())).collect(),
            ),
            Self::Level(values) => Pred::In(
                column(FilterField::Level)?,
                values.iter().map(|v| Literal::Enum(v.value())).collect(),
            ),
            Self::ObservationType(values) => Pred::In(
                column(FilterField::ObservationType)?,
                values.iter().map(|v| Literal::Enum(v.value())).collect(),
            ),
            Self::Tags(values) if target == FilterTarget::TraceHeads => effective_tags(values)?,
            Self::Tags(values) => Pred::HasAny(
                column(FilterField::Tags)?,
                values.iter().map(|v| Literal::String(v.clone())).collect(),
            ),
            Self::Metadata { key, value } if target.supports(FilterField::Metadata) => {
                Pred::MetadataEq {
                    key: key.clone(),
                    value: value.clone(),
                }
            }
            Self::Metadata { .. } => return Err(refused(FilterField::Metadata)),
            Self::Name(text) if target.supports(FilterField::Name) => Pred::TextMatch(text.clone()),
            Self::Name(_) => return Err(refused(FilterField::Name)),
        })
    }

    /// Splits the filter at its top-level conjunction: the conjuncts `target` can evaluate, and the rest. The
    /// two together are the filter, so the caller must apply the rest elsewhere or refuse it. A condition under
    /// an `Or` or a `Not` stays with its whole conjunct.
    pub fn split(&self, target: FilterTarget) -> (Self, Self) {
        let conjuncts = match self.canonical() {
            Self::And(operands) => operands,
            other => vec![other],
        };
        let (here, rest): (Vec<Self>, Vec<Self>) = conjuncts
            .into_iter()
            .partition(|conjunct| conjunct.unsupported(target).is_none());
        (Self::And(here), Self::And(rest))
    }

    /// Whether the filter keeps every row.
    pub fn is_all(&self) -> bool {
        self.canonical() == Self::all()
    }
}
