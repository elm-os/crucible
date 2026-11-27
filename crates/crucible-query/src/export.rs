//! Export reads (contracts 8.7 and 12.8): a streaming `Scan(span_core)` in key order under a fixed `as_of`,
//! joined in Rust with the payload rows, the MCP rows and the effective facts of each page, every read in class
//! `export`.
//!
//! - **Pages.** [`QueryService::export_page`] reads up to [`ExportLimits::rows`] core rows after a position and
//!   keeps a prefix whose payloads stay under [`ExportLimits::bytes`] (by each span's `event_bytes`), so one page
//!   holds a bounded amount of payload. Rows that tie on the key order (a span sent twice, or two spans whose
//!   trace hashes and span ids collide) are never split between pages: a run of ties is kept or left whole, and a
//!   page made of one run reads the run completely.
//! - **Duplicates.** A span sent twice is exported once: the copy with the greatest `lsn`, with the payload row
//!   of that copy.
//! - **Statements.** Every statement stays within what one ClickHouse request carries: a selection of more than
//!   [`MAX_LIST`] traces is scanned one chunk of traces per plan and the chunks' rows merged in key order, and a
//!   page's spans are looked up [`EXPORT_LOOKUP_SPANS`] at a time and its traces' facts [`MAX_LIST`] at a time.
//! - **Completeness.** The store is the only source, so [`QueryService::export_wait`] says when a span
//!   acknowledged before `as_of` may not be in the store yet; the caller waits instead of writing a short file.
//!   A store that does not answer fails the page.

use std::collections::{BTreeMap, BTreeSet};

use bytes::Bytes;
use crucible_plan::{
    EXPORT_LOOKUP_SPANS, EXPORT_PAGE_MAX, ExportKey, ExportSelection, ExportSpanKey, FactTable,
    MAX_LIST, Row, Value, export_facts, export_mcp_content, export_mcp_ops, export_payloads,
    export_spans, export_ties,
};
use crucible_recent::RecentSpan;
use crucible_records::{
    ApiKeyId, CostSource, FactId, FactVersion, Ingestion, IoShape, Level, Lsn, McpSide, NanoUsd,
    ObservationType, PayloadField, PayloadFields, SourceFormat, SpanCore, SpanId, SpanKind,
    SpanPayload, SpanRecord, StatusCode, TableId, TimeWindow, Timestamp, TraceId, TracePublic,
    Truncation, UsageEntry, Uuid, WalEpoch, fold_facts,
};
use crucible_schema::{
    Column, FactCorrectionsCol, FactFlagsCol, FactScoresCol, FactTagsCol, McpContentCol, McpOpsCol,
    SpanCoreCol, SpanPayloadCol, Ty,
};

use crate::context::{QueryServiceError, ReadContext};
use crate::service::{QueryService, Read, all_time};

/// The tables an export reads, for the quarantine rule.
const EXPORT_TABLES: [TableId; 2] = [TableId::SpanCore, TableId::SpanPayload];

/// What an export reads: the spans that start inside the window and arrived at or before `as_of`, narrowed by
/// the optional trace, user and session conditions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ExportQuery {
    /// The window over `start_time`.
    pub window: TimeWindow,
    /// Only these traces; every trace when empty.
    pub trace_ids: Vec<TraceId>,
    /// Only spans of this user.
    pub user_id: Option<String>,
    /// Only spans of this session.
    pub session_id: Option<String>,
    /// The arrival bound fixed when the export was created.
    pub as_of: Timestamp,
}

impl ExportQuery {
    /// The selections the page scan reads, one plan each: the window's spans when no trace is named, else one
    /// chunk of at most [`MAX_LIST`] of `traces`, the distinct named traces.
    fn selections<'a>(&'a self, traces: &'a [TraceId]) -> Vec<ExportSelection<'a>> {
        let narrowed = |trace_ids| ExportSelection {
            trace_ids,
            user_id: self.user_id.as_deref(),
            session_id: self.session_id.as_deref(),
        };
        if traces.is_empty() {
            vec![narrowed(&[])]
        } else {
            traces.chunks(MAX_LIST).map(narrowed).collect()
        }
    }

    /// The named traces, each once, in order.
    fn traces(&self) -> Vec<TraceId> {
        self.trace_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn selects(&self, span: &RecentSpan) -> bool {
        let s = &span.summary;
        self.window.contains(s.start_time)
            && (self.trace_ids.is_empty() || self.trace_ids.contains(&s.trace_id))
            && self.user_id.as_deref().is_none_or(|u| *s.user_id == *u)
            && self
                .session_id
                .as_deref()
                .is_none_or(|v| *s.session_id == *v)
    }
}

/// How much one page reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ExportLimits {
    /// The most core rows one page reads, 1 to [`EXPORT_PAGE_MAX`].
    pub rows: u32,
    /// The payload bytes a page aims to stay under, by the spans' `event_bytes`. A page always holds at least
    /// one span, and a run of tied rows whole.
    pub bytes: u64,
}

impl Default for ExportLimits {
    /// 1,000 rows and 32 MiB of payload.
    fn default() -> Self {
        Self {
            rows: 1_000,
            bytes: 32 << 20,
        }
    }
}

/// Why an export cannot be read whole yet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportWait {
    /// A span of the export acknowledged by this node is not confirmed in the store yet.
    Shipping,
    /// The recent index dropped spans of the project during a store outage that the store may not hold yet.
    Gap,
    /// The project has spans in a quarantined batch, which the store does not hold.
    Quarantined,
}

impl ExportWait {
    /// What the wait is for, for logs and the job's state.
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Shipping => {
                "spans acknowledged before the export's as_of are still on their way to the store"
            }
            Self::Gap => "the store has not caught up after an outage",
            Self::Quarantined => {
                "some of the project's spans are in a quarantined batch that the store does not hold"
            }
        }
    }
}

/// One column of a stored row: its name, type and value.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportColumn {
    /// The column name.
    pub name: &'static str,
    /// The column type.
    pub ty: Ty,
    /// The value.
    pub value: Value,
}

/// The MCP operation of a span (contract 3.5): its `mcp_ops` row and, when content was captured, its
/// `mcp_content` row, each without the key and ingestion columns the span already carries.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportMcp {
    /// The `mcp_ops` columns.
    pub op: Vec<ExportColumn>,
    /// The `mcp_content` columns.
    pub content: Option<Vec<ExportColumn>>,
}

/// One effective fact (contract 3.8): the row that won its `(target, key)`, with every stored column but the
/// ingestion group.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportFact {
    /// The fact table.
    pub table: FactTable,
    /// The columns, in the table's order.
    pub columns: Vec<ExportColumn>,
}

/// One exported span: both tiers, its MCP operation and the effective facts of the span and its trace.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportRecord {
    /// The record as stored. `payload.metadata` is empty: the store keeps the object only as its flattened keys
    /// and values (`metadata_keys`, `metadata_values`).
    pub span: SpanRecord,
    /// Whether the payload row was found: `false` when it expired under the payload retention.
    pub payload_stored: bool,
    /// The MCP operation, for a span with `mcp_side` other than `none`.
    pub mcp: Option<ExportMcp>,
    /// The effective facts of the trace and of the span, ordered by table, target and key.
    pub facts: Vec<ExportFact>,
}

/// One page of an export.
#[derive(Clone, PartialEq, Debug)]
pub struct ExportPage {
    /// The spans, in key order.
    pub records: Vec<ExportRecord>,
    /// Where the next page starts; `None` when this was the last.
    pub next: Option<ExportKey>,
}

type Decoded<T> = Result<T, QueryServiceError>;

fn bad(what: &str) -> QueryServiceError {
    QueryServiceError::Internal(format!(
        "the store returned an export row without a valid {what}"
    ))
}

/// The values of a row of a plan that selects every column of `C`, read by column.
struct Cols<'a, C>(&'a [Value], std::marker::PhantomData<C>);

impl<'a, C: Column + PartialEq + std::fmt::Debug> Cols<'a, C> {
    fn of(row: &'a Row) -> Self {
        Self(row.values(), std::marker::PhantomData)
    }

    fn get(&self, column: C) -> Decoded<&'a Value> {
        C::ALL
            .iter()
            .position(|c| *c == column)
            .and_then(|at| self.0.get(at))
            .ok_or_else(|| bad(&format!("{column:?}")))
    }

    fn uint(&self, column: C) -> Decoded<u64> {
        match self.get(column)? {
            Value::UInt(v) => Ok(*v),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn int(&self, column: C) -> Decoded<i64> {
        match self.get(column)? {
            Value::Int(v) => Ok(*v),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn small<T: TryFrom<u64>>(&self, column: C) -> Decoded<T> {
        T::try_from(self.uint(column)?).map_err(|_| bad(&format!("{column:?}")))
    }

    fn flag(&self, column: C) -> Decoded<bool> {
        Ok(self.uint(column)? != 0)
    }

    fn enum8<T>(&self, column: C, from: fn(u8) -> Option<T>) -> Decoded<T> {
        match self.get(column)? {
            Value::Enum(v) => from(*v).ok_or_else(|| bad(&format!("{column:?}"))),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn time(&self, column: C) -> Decoded<Timestamp> {
        match self.get(column)? {
            Value::Time(t) => Ok(*t),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn opt_time(&self, column: C) -> Decoded<Option<Timestamp>> {
        match self.get(column)? {
            Value::Time(t) => Ok(Some(*t)),
            Value::Null => Ok(None),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn uuid(&self, column: C) -> Decoded<Uuid> {
        match self.get(column)? {
            Value::Uuid(id) => Ok(*id),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    /// A UUID column where the nil UUID means absent.
    fn opt_uuid(&self, column: C) -> Decoded<Option<Uuid>> {
        Ok(Some(self.uuid(column)?).filter(|id| *id != Uuid::NIL))
    }

    fn bytes(&self, column: C) -> Decoded<&'a [u8]> {
        match self.get(column)? {
            Value::String(text) => Ok(text.as_bytes()),
            Value::Bytes(bytes) => Ok(bytes),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn fixed<const N: usize>(&self, column: C) -> Decoded<[u8; N]> {
        self.bytes(column)?
            .try_into()
            .map_err(|_| bad(&format!("{column:?}")))
    }

    fn text(&self, column: C) -> Decoded<Box<str>> {
        Ok(String::from_utf8_lossy(self.bytes(column)?).into())
    }

    fn body(&self, column: C) -> Decoded<Bytes> {
        Ok(Bytes::copy_from_slice(self.bytes(column)?))
    }

    fn array(&self, column: C) -> Decoded<&'a [Value]> {
        match self.get(column)? {
            Value::Array(items) => Ok(items),
            _ => Err(bad(&format!("{column:?}"))),
        }
    }

    fn texts(&self, column: C) -> Decoded<Vec<Box<str>>> {
        self.array(column)?
            .iter()
            .map(|item| match item {
                Value::String(text) => Ok(text.as_str().into()),
                Value::Bytes(bytes) => Ok(String::from_utf8_lossy(bytes).into()),
                _ => Err(bad(&format!("{column:?}"))),
            })
            .collect()
    }

    fn uints(&self, column: C) -> Decoded<Vec<u64>> {
        self.array(column)?
            .iter()
            .map(|item| match item {
                Value::UInt(v) => Ok(*v),
                _ => Err(bad(&format!("{column:?}"))),
            })
            .collect()
    }

    fn ints(&self, column: C) -> Decoded<Vec<i64>> {
        self.array(column)?
            .iter()
            .map(|item| match item {
                Value::Int(v) => Ok(*v),
                _ => Err(bad(&format!("{column:?}"))),
            })
            .collect()
    }

    fn trace_id(&self, column: C) -> Decoded<TraceId> {
        Ok(TraceId::from_bytes(self.fixed(column)?))
    }

    fn span_id(&self, column: C) -> Decoded<SpanId> {
        Ok(SpanId::from_bytes(self.fixed(column)?))
    }

    fn lsn(&self, column: C) -> Decoded<Lsn> {
        Ok(Lsn::new(self.uint(column)?))
    }

    /// Every column but `skip`, with its name and type, in the table's order.
    fn columns(&self, skip: &[C]) -> Vec<ExportColumn> {
        C::ALL
            .iter()
            .zip(self.0)
            .filter(|(c, _)| !skip.contains(c))
            .map(|(c, value)| {
                let column = c.column_ref();
                ExportColumn {
                    name: column.name(),
                    ty: *column.ty(),
                    value: value.clone(),
                }
            })
            .collect()
    }
}

/// The position of a core row in key order.
fn key_of(row: &Row) -> Decoded<ExportKey> {
    let c = Cols::<SpanCoreCol>::of(row);
    Ok((
        c.time(SpanCoreCol::StartMinute)?,
        c.small(SpanCoreCol::TraceHash)?,
        c.time(SpanCoreCol::StartTime)?,
        c.span_id(SpanCoreCol::SpanId)?,
    ))
}

/// The core tier of a `span_core` row.
fn core(row: &Row, ctx: &ReadContext) -> Decoded<SpanCore> {
    use SpanCoreCol as C;
    let c = Cols::<C>::of(row);
    let usage_keys = c.texts(C::UsageKeys)?;
    let usage_units = c.uints(C::UsageValues)?;
    let usage_costs = c.ints(C::CostValues)?;
    if usage_keys.len() != usage_units.len() || usage_keys.len() != usage_costs.len() {
        return Err(bad("usage list"));
    }
    let usage = usage_keys
        .into_iter()
        .zip(usage_units)
        .zip(usage_costs)
        .map(|((key, units), cost)| UsageEntry {
            key,
            units,
            cost: NanoUsd::from_nanos(cost),
        })
        .collect();
    let digest = |column| -> Decoded<Option<[u8; 32]>> {
        Ok(Some(c.fixed::<32>(column)?).filter(|hash| *hash != [0; 32]))
    };
    Ok(SpanCore {
        project_id: ctx.scope().project_id(),
        trace_id: c.trace_id(C::TraceId)?,
        span_id: c.span_id(C::SpanId)?,
        parent_span_id: c.span_id(C::ParentSpanId)?,
        is_root: c.flag(C::IsRoot)?,
        is_app_root: c.flag(C::IsAppRoot)?,
        kind: c.enum8(C::Kind, SpanKind::from_value)?,
        name: c.text(C::Name)?,
        start_time: c.time(C::StartTime)?,
        end_time: c.time(C::EndTime)?,
        completion_start_time: c.opt_time(C::CompletionStartTime)?,
        status_code: c.enum8(C::StatusCode, StatusCode::from_value)?,
        status_message: c.text(C::StatusMessage)?,
        service_name: c.text(C::ServiceName)?,
        service_version: c.text(C::ServiceVersion)?,
        scope_name: c.text(C::ScopeName)?,
        scope_version: c.text(C::ScopeVersion)?,
        telemetry_sdk_name: c.text(C::TelemetrySdkName)?,
        telemetry_sdk_language: c.text(C::TelemetrySdkLanguage)?,
        telemetry_sdk_version: c.text(C::TelemetrySdkVersion)?,
        sdk_name: c.text(C::SdkName)?,
        sdk_version: c.text(C::SdkVersion)?,
        observation_type: c.enum8(C::ObservationType, ObservationType::from_value)?,
        level: c.enum8(C::Level, Level::from_value)?,
        environment: c.text(C::Environment)?,
        release: c.text(C::Release)?,
        version: c.text(C::Version)?,
        trace_name: c.text(C::TraceName)?,
        user_id: c.text(C::UserId)?,
        session_id: c.text(C::SessionId)?,
        trace_tags: c.texts(C::TraceTags)?,
        trace_public: c.enum8(C::TracePublic, TracePublic::from_value)?,
        tags: c.texts(C::Tags)?,
        model_requested: c.text(C::ModelRequested)?,
        model_response: c.text(C::ModelResponse)?,
        provider: c.text(C::Provider)?,
        finish_reasons: c.texts(C::FinishReasons)?,
        usage,
        input_tokens: c.uint(C::InputTokens)?,
        output_tokens: c.uint(C::OutputTokens)?,
        total_tokens: c.uint(C::TotalTokens)?,
        total_cost_nusd: NanoUsd::from_nanos(c.int(C::TotalCostNusd)?),
        cost_source: c.enum8(C::CostSource, CostSource::from_value)?,
        price_model_id: c.opt_uuid(C::PriceModelId)?,
        price_tier_id: c.opt_uuid(C::PriceTierId)?,
        usage_aggregate: c.flag(C::UsageAggregate)?,
        prompt_name: c.text(C::PromptName)?,
        prompt_version: c.text(C::PromptVersion)?,
        tool_names: c.texts(C::ToolNames)?,
        tool_call_count: c.small(C::ToolCallCount)?,
        mcp_side: c.enum8(C::McpSide, McpSide::from_value)?,
        io_shape: c.enum8(C::IoShape, IoShape::from_value)?,
        input_preview: c.text(C::InputPreview)?,
        output_preview: c.text(C::OutputPreview)?,
        input_bytes: c.uint(C::InputBytes)?,
        output_bytes: c.uint(C::OutputBytes)?,
        input_truncation: c.enum8(C::InputTruncation, Truncation::from_value)?,
        output_truncation: c.enum8(C::OutputTruncation, Truncation::from_value)?,
        input_sha256: digest(C::InputSha256)?,
        output_sha256: digest(C::OutputSha256)?,
        source_format: c.enum8(C::SourceFormat, SourceFormat::from_value)?,
        compat_dialect: c.small(C::CompatDialect)?,
        normalizer_version: c.small(C::NormalizerVersion)?,
        clock_skew: c.flag(C::ClockSkew)?,
        ingestion: Ingestion {
            wal_epoch: WalEpoch::from_uuid(c.uuid(C::WalEpoch)?),
            batch_seq: c.lsn(C::BatchSeq)?,
            lsn: c.lsn(C::Lsn)?,
            received_at: c.time(C::ReceivedAt)?,
        },
        api_key_id: ApiKeyId::from_uuid(c.uuid(C::ApiKeyId)?),
        row_digest: c.uint(C::RowDigest)?,
        event_bytes: c.small(C::EventBytes)?,
    })
}

/// The payload tier of a `span_payload` row.
fn payload(row: &Row) -> Decoded<SpanPayload> {
    use SpanPayloadCol as C;
    let c = Cols::<C>::of(row);
    let metadata_keys = c.texts(C::MetadataKeys)?;
    let metadata_values = c.texts(C::MetadataValues)?;
    if metadata_keys.len() != metadata_values.len() {
        return Err(bad("metadata"));
    }
    let pairs = |keys: C, values: Vec<u64>| -> Decoded<Vec<(Box<str>, u64)>> {
        let keys = c.texts(keys)?;
        if keys.len() != values.len() {
            return Err(bad("usage map"));
        }
        Ok(keys.into_iter().zip(values).collect())
    };
    let usage_provided = pairs(C::UsageProvidedKeys, c.uints(C::UsageProvidedValues)?)?;
    let cost_keys = c.texts(C::CostProvidedKeys)?;
    let cost_values = c.ints(C::CostProvidedValues)?;
    if cost_keys.len() != cost_values.len() {
        return Err(bad("cost map"));
    }
    let mut truncated_fields = PayloadFields::EMPTY;
    for name in c.texts(C::TruncatedFields)? {
        if let Some(field) = PayloadField::ALL.into_iter().find(|f| f.name() == &*name) {
            truncated_fields.insert(field);
        }
    }
    Ok(SpanPayload {
        input: c.body(C::Input)?,
        output: c.body(C::Output)?,
        system_instructions: c.body(C::SystemInstructions)?,
        // The store keeps `metadata` only flattened to its keys and values; the writers rebuild the object.
        metadata: Bytes::new(),
        metadata_keys,
        metadata_values,
        metadata_kv: c.texts(C::MetadataKv)?,
        trace_metadata: c.body(C::TraceMetadata)?,
        attributes: c.body(C::Attributes)?,
        resource_attributes: c.body(C::ResourceAttributes)?,
        events: c.body(C::Events)?,
        links: c.body(C::Links)?,
        tool_definitions: c.body(C::ToolDefinitions)?,
        tool_calls: c.body(C::ToolCalls)?,
        model_parameters: c.body(C::ModelParameters)?,
        usage_provided,
        cost_provided: cost_keys
            .into_iter()
            .zip(cost_values.into_iter().map(NanoUsd::from_nanos))
            .collect(),
        trace_state: c.body(C::TraceState)?,
        truncated_fields,
    })
}

/// A fact row's identity for resolution: within one trace, `(table, target kind, span, key)`.
type FactKey = (FactTable, u8, SpanId, Box<str>);

/// One candidate fact: its trace, its resolution key, its version and its columns.
type Candidate = (TraceId, FactKey, FactVersion, Vec<ExportColumn>);

/// The candidate facts of one table, by trace: each row's resolution key, version and columns.
fn fact_candidates(table: FactTable, rows: Vec<Row>) -> Decoded<Vec<Candidate>> {
    macro_rules! decode {
        ($c:ident) => {{
            let mut out = Vec::with_capacity(rows.len());
            for row in &rows {
                let c = Cols::<$c>::of(row);
                let kind = match c.get($c::TargetKind)? {
                    Value::Enum(v) => *v,
                    _ => return Err(bad("fact target kind")),
                };
                let version = FactVersion {
                    version: c.uint($c::Version)?,
                    fact_id: FactId::from_uuid(c.uuid($c::FactId)?),
                    retracted: c.flag($c::Retracted)?,
                };
                let key = (table, kind, c.span_id($c::SpanId)?, c.text($c::Key)?);
                let skip = [
                    $c::TargetDay,
                    $c::WalEpoch,
                    $c::BatchSeq,
                    $c::Lsn,
                    $c::ReceivedAt,
                ];
                out.push((c.trace_id($c::TraceId)?, key, version, c.columns(&skip)));
            }
            Ok(out)
        }};
    }
    match table {
        FactTable::Scores => decode!(FactScoresCol),
        FactTable::Tags => decode!(FactTagsCol),
        FactTable::Flags => decode!(FactFlagsCol),
        FactTable::Corrections => decode!(FactCorrectionsCol),
    }
}

/// Core rows and their keys, in key order: the order of one scan, for the rows of several scans together. Rows
/// that tie keep their order.
fn in_key_order(rows: Vec<Row>) -> Decoded<(Vec<Row>, Vec<ExportKey>)> {
    let mut keyed = rows
        .into_iter()
        .map(|row| Ok((key_of(&row)?, row)))
        .collect::<Decoded<Vec<_>>>()?;
    keyed.sort_by_key(|(key, _)| *key);
    Ok(keyed.into_iter().map(|(key, row)| (row, key)).unzip())
}

/// The run of rows that ends at `end` (exclusive) and ties with the row before it starts at the returned index.
fn run_start(keys: &[ExportKey], end: usize) -> usize {
    let Some(last) = end.checked_sub(1).and_then(|at| keys.get(at)) else {
        return 0;
    };
    keys[..end]
        .iter()
        .rposition(|key| key != last)
        .map_or(0, |at| at + 1)
}

impl QueryService {
    /// Why the export cannot be read whole yet, or `None` when it can: every span this node acknowledged at or
    /// before `as_of` that the export selects is confirmed in the store, and the project has no open gap and no
    /// quarantined span. A caller that gets `Some` waits and asks again instead of writing a file that is short.
    pub fn export_wait(&self, ctx: &ReadContext, query: &ExportQuery) -> Option<ExportWait> {
        let state = self.index.read_state(ctx.scope);
        if state.gap {
            return Some(ExportWait::Gap);
        }
        if state.quarantined(&EXPORT_TABLES) {
            return Some(ExportWait::Quarantined);
        }
        let (pending, _) = self.index.since(ctx.scope, state.watermark);
        let unshipped = pending
            .iter()
            .any(|span| span.received_at <= query.as_of && query.selects(span));
        unshipped.then_some(ExportWait::Shipping)
    }

    /// One page of an export, starting after `after` (the first page when `None`). See the module documentation
    /// for the page rules. A store that does not answer fails the page with `telemetry_store_unavailable`.
    pub async fn export_page(
        &self,
        ctx: &ReadContext,
        query: &ExportQuery,
        after: Option<ExportKey>,
        limits: ExportLimits,
    ) -> Result<ExportPage, QueryServiceError> {
        if limits.rows == 0 || limits.rows > EXPORT_PAGE_MAX {
            return Err(QueryServiceError::Validation("limit"));
        }
        let mut read = self.read(ctx, "export_page");
        let scope = ctx.scope;
        let traces = query.traces();
        let selections = query.selections(&traces);
        // Each chunk of traces gives its first rows after `after`; merged in key order, the first `limit` of them
        // are the page, because a chunk's later rows follow its first `limit` in key order.
        let mut rows = Vec::new();
        for selection in &selections {
            let plan = export_spans(
                scope,
                query.window,
                &read.clamp,
                *selection,
                query.as_of,
                after,
                limits.rows,
            );
            rows.extend(read.rows(plan).await?);
        }
        let (mut rows, mut keys) = in_key_order(rows)?;
        let full = rows.len() >= limits.rows as usize;
        rows.truncate(limits.rows as usize);
        keys.truncate(limits.rows as usize);
        let mut next = None;
        if full {
            let start = run_start(&keys, keys.len());
            match keys.get(start).copied() {
                Some(key) if start == 0 => {
                    // One run fills the page: read the run whole.
                    let mut tied = Vec::new();
                    for selection in &selections {
                        let plan = export_ties(
                            scope,
                            query.window,
                            &read.clamp,
                            *selection,
                            query.as_of,
                            key,
                        );
                        tied.extend(read.rows(plan).await?);
                    }
                    (rows, keys) = in_key_order(tied)?;
                    next = Some(key);
                }
                _ => {
                    rows.truncate(start);
                    keys.truncate(start);
                    next = keys.last().copied();
                }
            }
        }
        // The byte budget: keep a prefix of whole runs.
        let mut spent = 0u64;
        let mut cut = rows.len();
        for (at, row) in rows.iter().enumerate() {
            let size = Cols::<SpanCoreCol>::of(row).uint(SpanCoreCol::EventBytes)?;
            spent = spent.saturating_add(size);
            if spent > limits.bytes && at > 0 {
                cut = at;
                break;
            }
        }
        if cut < rows.len() {
            let start = run_start(&keys, cut + 1);
            cut = if start == 0 {
                keys.iter()
                    .position(|key| *key != keys[0])
                    .unwrap_or(keys.len())
            } else if keys[cut] == keys[cut - 1] {
                start
            } else {
                cut
            };
            rows.truncate(cut);
            keys.truncate(cut);
            next = keys.last().copied();
        }
        let records = self.records(&mut read, ctx, rows).await?;
        let state = self.index.read_state(scope);
        read.finish(
            query.as_of,
            self.consistency(),
            state,
            &EXPORT_TABLES,
            false,
        )?;
        Ok(ExportPage { records, next })
    }

    /// The records of a page's core rows: duplicates dropped, payloads, MCP rows and facts joined.
    async fn records(
        &self,
        read: &mut Read<'_>,
        ctx: &ReadContext,
        rows: Vec<Row>,
    ) -> Result<Vec<ExportRecord>, QueryServiceError> {
        // A span sent twice: the copy with the greatest LSN, at the position of its first copy.
        let mut cores: Vec<SpanCore> = Vec::with_capacity(rows.len());
        let mut at: BTreeMap<(TraceId, SpanId), usize> = BTreeMap::new();
        for row in &rows {
            let core = core(row, ctx)?;
            match at.get(&(core.trace_id, core.span_id)) {
                Some(&i) if cores[i].ingestion.lsn >= core.ingestion.lsn => {}
                Some(&i) => cores[i] = core,
                None => {
                    at.insert((core.trace_id, core.span_id), cores.len());
                    cores.push(core);
                }
            }
        }
        if cores.is_empty() {
            return Ok(Vec::new());
        }
        let scope = ctx.scope;
        let keys: Vec<ExportSpanKey> = cores
            .iter()
            .map(|c| (c.start_time, c.trace_id, c.span_id))
            .collect();

        // Payloads: the row of the copy that was kept, else the newest.
        let mut payloads: BTreeMap<(TraceId, SpanId), (Lsn, Row)> = BTreeMap::new();
        for chunk in keys.chunks(EXPORT_LOOKUP_SPANS) {
            for row in read
                .rows(export_payloads(scope, &read.clamp, chunk))
                .await?
            {
                let c = Cols::<SpanPayloadCol>::of(&row);
                let id = (
                    c.trace_id(SpanPayloadCol::TraceId)?,
                    c.span_id(SpanPayloadCol::SpanId)?,
                );
                let lsn = c.lsn(SpanPayloadCol::Lsn)?;
                let wanted = at.get(&id).map(|&i| cores[i].ingestion.lsn);
                let better = match payloads.get(&id) {
                    None => true,
                    Some((held, _)) if Some(*held) == wanted => false,
                    Some((held, _)) => Some(lsn) == wanted || lsn > *held,
                };
                if better {
                    payloads.insert(id, (lsn, row));
                }
            }
        }

        // MCP operations and their captured content.
        let mcp_keys: Vec<ExportSpanKey> = cores
            .iter()
            .filter(|c| c.mcp_side != McpSide::None)
            .map(|c| (c.start_time, c.trace_id, c.span_id))
            .collect();
        let mut ops: BTreeMap<(TraceId, SpanId), Vec<ExportColumn>> = BTreeMap::new();
        let mut contents: BTreeMap<(TraceId, SpanId), Vec<ExportColumn>> = BTreeMap::new();
        for chunk in mcp_keys.chunks(EXPORT_LOOKUP_SPANS) {
            for row in read.rows(export_mcp_ops(scope, &read.clamp, chunk)).await? {
                use McpOpsCol as M;
                let c = Cols::<M>::of(&row);
                let skip = [
                    M::StartMinute,
                    M::TraceHash,
                    M::StartTime,
                    M::SpanId,
                    M::TraceId,
                    M::ParentSpanId,
                    M::WalEpoch,
                    M::BatchSeq,
                    M::Lsn,
                    M::ReceivedAt,
                ];
                ops.insert(
                    (c.trace_id(M::TraceId)?, c.span_id(M::SpanId)?),
                    c.columns(&skip),
                );
            }
            for row in read
                .rows(export_mcp_content(scope, &read.clamp, chunk))
                .await?
            {
                use McpContentCol as M;
                let c = Cols::<M>::of(&row);
                let skip = [
                    M::StartMinute,
                    M::TraceHash,
                    M::StartTime,
                    M::SpanId,
                    M::TraceId,
                    M::UserId,
                    M::WalEpoch,
                    M::BatchSeq,
                    M::Lsn,
                    M::ReceivedAt,
                ];
                contents.insert(
                    (c.trace_id(M::TraceId)?, c.span_id(M::SpanId)?),
                    c.columns(&skip),
                );
            }
        }

        // Facts of the page's traces, about the traces and their spans.
        let traces: Vec<TraceId> = cores
            .iter()
            .map(|c| c.trace_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut candidates = Vec::new();
        for table in FactTable::ALL {
            for chunk in traces.chunks(MAX_LIST) {
                let plan = export_facts(scope, all_time()?, &read.clamp, table, chunk);
                candidates.extend(fact_candidates(table, read.rows(plan).await?)?);
            }
        }
        let resolved = fold_facts(
            candidates
                .into_iter()
                .map(|(trace, key, version, columns)| ((trace, key), version, columns)),
        );
        let mut facts: BTreeMap<TraceId, Vec<(FactKey, Vec<ExportColumn>)>> = BTreeMap::new();
        for ((trace, key), columns) in resolved {
            facts.entry(trace).or_default().push((key, columns));
        }

        let span_kind = crucible_records::FactTargetKind::Span.value();
        cores
            .into_iter()
            .map(|core| {
                let id = (core.trace_id, core.span_id);
                let (payload, payload_stored) = match payloads.remove(&id) {
                    Some((_, row)) => (payload(&row)?, true),
                    None => (SpanPayload::default(), false),
                };
                let mcp = ops.remove(&id).map(|op| ExportMcp {
                    op,
                    content: contents.remove(&id),
                });
                let facts = facts
                    .get(&core.trace_id)
                    .into_iter()
                    .flatten()
                    .filter(|((_, kind, span, _), _)| *kind != span_kind || *span == core.span_id)
                    .map(|((table, ..), columns)| ExportFact {
                        table: *table,
                        columns: columns.clone(),
                    })
                    .collect();
                Ok(ExportRecord {
                    span: SpanRecord { core, payload },
                    payload_stored,
                    mcp,
                    facts,
                })
            })
            .collect()
    }
}
