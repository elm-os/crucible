//! The mapping engine (contract 10.2 and 10.3): one pure, table-driven function that maps a span's attributes,
//! resource attributes and events onto a draft, with every dialect table as a layer.
//!
//! - **Layers.** Every span is mapped with all layers. A candidate's rank is (layer position, source position);
//!   the lowest rank wins, and among occurrences of one source (a repeated key, several matching events) the
//!   first in wire order. A source scoped `Own` is read only when its table's dialect is the chosen dialect.
//! - **Detection.** The chosen dialect is the one with the highest detection confidence, ties going to the lowest
//!   id; a request hint counts as a detection of its dialect at confidence 60; the default is 0.
//! - **Consumption.** An attribute leaves the residual `attributes` or `resource_attributes` when at least one
//!   winning source read its whole value. A source with a member path never consumes. A family decoder (indexed,
//!   prefix) consumes the keys it used; keys it skipped stay. Losing candidates, values that do not decode and
//!   keys no table knows stay. Two sets of keys leave the residual whatever read them: the keys that must never
//!   be stored (contract 3.5), on every span, and on an MCP operation the keys its projector consumes (contract
//!   9.1). No other attribute is dropped. Events are never consumed.
//! - **Maps** (`metadata`, `trace_metadata`, `model_parameters`) merge every source that decodes: per member name
//!   the lowest rank wins, then the first occurrence. Members are written in rank order, then wire order. Members
//!   beyond a metadata map's key cap are not taken and stay in the residual attributes.
//! - **Usage and provided cost** are merged per key: for each key the lowest-rank candidate wins.
//! - **Value encoding.** On a span whose chosen dialect encodes values as JSON text, a string attribute that is
//!   JSON text of one scalar is unwrapped before a scalar decoder or a type rule reads it; when the decoder
//!   rejects the unwrapped scalar it reads the string as sent. Bodies and maps are never unwrapped.
//! - **Derived fields.** Without a `tool_calls` source, tool calls are read from an indexed output by its
//!   layout's shape, or from the tool-call parts of a GenAI message output; `finish_reasons` falls back to the
//!   per-element finish reason of an indexed output; `tool_names` derives from the tool calls unless a source
//!   filled it, and `tool_call_count` is the number of tool calls.
//! - **Media tokens.** Stored fields keep a media reference token; a preview shows a placeholder in its place.
//!
//! Instrumentation-scope attributes have no column in contract 3.1 and are not stored.

use std::borrow::Cow;

use bytes::Bytes;
use crucible_compat::{
    Confidence, Decoder, DetectOn, DialectTable, GENAI_PARTS, IndexedLayout, KeyMatch,
    MEDIA_PLACEHOLDER, MapValues, MediaToken, PrefixSpec, SdkGate, Target, TimeUnit, ToolCallShape,
    ValueEncoding,
};
use crucible_otlp::{AnyValueView, SpanView};
use crucible_records::{
    IoShape, Level, NanoUsd, ObservationType, PayloadField, Timestamp, TracePublic, caps,
    det_set_with_capacity, mcp_keys, truncate_str,
};
use smallvec::SmallVec;

use crate::index::{Hit, Index, MAPS, SCALAR_SLOTS, Slot, SourceHit};
use crate::mapper::{MapCtx, SpanDraft};
use crate::node::Node;
use crate::{indexed, json, usage};

/// The resource key that names the SDK language, for an SDK gate.
const SDK_LANGUAGE_KEY: &str = "telemetry.sdk.language";

/// How far past a token's start its closing delimiter is searched.
const MEDIA_TOKEN_MAX_BYTES: usize = 4_096;

/// The attribute occurrence that supplied a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Span(u32),
    Resource(u32),
    /// An event, a member path or a sum: nothing to consume.
    None,
}

/// A stored body.
#[derive(Clone, Debug)]
enum Body<'a> {
    /// A value: a string verbatim, anything else as JSON.
    View(AnyValueView<'a>),
    /// JSON or text built by a decoder.
    Bytes(Vec<u8>),
    /// The elements of an indexed family.
    Elements(Vec<Node>, &'static IndexedLayout),
}

/// A decoded candidate of a field that takes one value.
#[derive(Clone, Debug)]
enum Value<'a> {
    Text(Cow<'a, str>),
    List(Vec<Cow<'a, str>>),
    Body(Body<'a>, IoShape),
    /// Tool calls, each reshaped to `{id, name, arguments}`.
    Calls(Vec<Node>),
    Flag(bool),
    Level(Level),
    Time(Timestamp),
}

#[derive(Debug)]
struct Candidate<'a> {
    target: Target,
    rank: u16,
    layer: u8,
    origin: Origin,
    /// The family that supplied the value; `u16::MAX` when a single attribute did.
    family: u16,
    value: Value<'a>,
}

/// One key of `usage_provided` or `cost_provided`.
#[derive(Debug)]
struct Keyed<'a, V> {
    key: Cow<'a, str>,
    value: V,
    rank: u16,
    origin: Origin,
}

/// A map member's value.
#[derive(Clone, Debug)]
enum MemberValue<'a> {
    View(AnyValueView<'a>),
    Node(Node),
}

/// One member of a map field.
#[derive(Debug)]
struct Member<'a> {
    map: u8,
    rank: u16,
    name: Cow<'a, str>,
    origin: Origin,
    value: MemberValue<'a>,
}

/// One key of a family: its element index (indexed families), the rest of the key, its value and where it is.
type FamilyKey<'a> = (u64, &'a str, AnyValueView<'a>, Origin);

/// A scalar as a decoder reads it.
#[derive(Clone, Debug)]
enum Scalar<'a> {
    Str(Cow<'a, str>),
    Int(i64),
    Double(f64),
    Bool(bool),
}

impl Scalar<'_> {
    fn from_node(node: Node) -> Option<Scalar<'static>> {
        Some(match node {
            Node::Str(s) => Scalar::Str(Cow::Owned(s)),
            Node::Int(i) => Scalar::Int(i),
            Node::UInt(u) => Scalar::Double(u as f64),
            Node::Float(d) => Scalar::Double(d),
            Node::Bool(b) => Scalar::Bool(b),
            Node::Null | Node::Arr(_) | Node::Obj(_) => return None,
        })
    }
}

/// What a decoder reads: an attribute value, or a JSON node found by a member path.
#[derive(Clone, Copy, Debug)]
enum Val<'v, 'a> {
    View(&'v AnyValueView<'a>),
    Node(&'v Node),
}

impl<'a> Val<'_, 'a> {
    fn scalar(&self) -> Option<Scalar<'a>> {
        match *self {
            Val::View(view) => Some(match *view {
                AnyValueView::String(s) => Scalar::Str(Cow::Borrowed(s)),
                AnyValueView::Int(i) => Scalar::Int(i),
                AnyValueView::Double(d) => Scalar::Double(d),
                AnyValueView::Bool(b) => Scalar::Bool(b),
                _ => return None,
            }),
            Val::Node(node) => Scalar::from_node(node.clone()),
        }
    }

    /// The JSON tree of an object or array: a structured value, or a string holding JSON text of one.
    fn container(&self) -> Option<Node> {
        match *self {
            Val::View(AnyValueView::String(s)) => Node::parse_container(s),
            Val::View(view @ (AnyValueView::Array(_) | AnyValueView::KvList(_))) => {
                Some(Node::from_view(view.clone()))
            }
            Val::View(_) => None,
            Val::Node(Node::Str(s)) => Node::parse_container(s),
            Val::Node(node @ (Node::Arr(_) | Node::Obj(_))) => Some(node.clone()),
            Val::Node(_) => None,
        }
    }

    /// The strings of an array whose elements are all strings.
    fn strings(&self) -> Option<Vec<Cow<'a, str>>> {
        match *self {
            Val::View(AnyValueView::Array(items)) => items
                .clone()
                .map(|item| match item {
                    AnyValueView::String(s) => Some(Cow::Borrowed(s)),
                    _ => None,
                })
                .collect(),
            Val::Node(Node::Arr(items)) => items
                .iter()
                .map(|item| item.as_str().map(|s| Cow::Owned(s.to_owned())))
                .collect(),
            _ => None,
        }
    }
}

/// A growable bit set over attribute positions.
#[derive(Debug, Default)]
struct Bits(SmallVec<[u64; 2]>);

impl Bits {
    fn set(&mut self, at: u32) {
        let word = (at / 64) as usize;
        if self.0.len() <= word {
            self.0.resize(word + 1, 0);
        }
        self.0[word] |= 1 << (at % 64);
    }

    fn get(&self, at: usize) -> bool {
        self.0
            .get(at / 64)
            .is_some_and(|word| word & (1 << (at % 64)) != 0)
    }
}

const fn is_list(target: Target) -> bool {
    matches!(
        target,
        Target::FinishReasons | Target::Tags | Target::TraceTags | Target::ToolNames
    )
}

/// The text a JSON string literal encodes, when `s` is one.
fn string_literal(s: &str) -> Option<String> {
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return None;
    }
    serde_json::from_str(s).ok()
}

/// A non-empty string, or with `ints` an integer in decimal. With `unquote`, a JSON string literal is read as the
/// text it encodes.
fn text<'a>(scalar: Scalar<'a>, ints: bool, unquote: bool) -> Option<Cow<'a, str>> {
    match scalar {
        Scalar::Str(s) => {
            if unquote && let Some(inner) = string_literal(&s) {
                return (!inner.is_empty()).then_some(Cow::Owned(inner));
            }
            (!s.is_empty()).then_some(s)
        }
        Scalar::Int(i) if ints => Some(Cow::Owned(i.to_string())),
        _ => None,
    }
}

fn flag(scalar: Scalar<'_>) -> Option<bool> {
    match scalar {
        Scalar::Bool(b) => Some(b),
        Scalar::Str(s) if s == "true" => Some(true),
        Scalar::Str(s) if s == "false" => Some(false),
        _ => None,
    }
}

fn level(map: &crucible_compat::LevelMap, scalar: Scalar<'_>) -> Option<Level> {
    match scalar {
        Scalar::Str(s) => {
            let name = s.trim_matches(|c: char| c.is_ascii_whitespace());
            map.names
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|&(_, l)| l)
        }
        Scalar::Int(i) => map.numbers.iter().find(|(n, _)| *n == i).map(|&(_, l)| l),
        _ => None,
    }
}

fn instant(scalar: Scalar<'_>) -> Option<Timestamp> {
    let time = match scalar {
        Scalar::Str(s) => match Timestamp::parse_rfc3339(&s) {
            Ok(time) => time,
            Err(_) => Timestamp::parse_rfc3339(&string_literal(&s)?).ok()?,
        },
        Scalar::Int(nanos) => Timestamp::from_unix_nanos(u64::try_from(nanos).ok()?),
        _ => return None,
    };
    time.is_valid().then_some(time)
}

/// `start + x` in `unit`, rounded half to even to the microsecond, saturating, and clamped to `end` when the span
/// has a duration.
fn offset(
    start: Timestamp,
    end: Timestamp,
    unit: TimeUnit,
    scalar: Scalar<'_>,
) -> Option<Timestamp> {
    let per_unit: i64 = match unit {
        TimeUnit::Millis => 1_000,
        TimeUnit::Seconds => 1_000_000,
    };
    let micros = match scalar {
        Scalar::Int(i) if i >= 0 => i.saturating_mul(per_unit),
        // A float-to-integer cast saturates, and the value is not NaN.
        Scalar::Double(d) if d.is_finite() && d >= 0.0 => {
            (d * per_unit as f64).round_ties_even() as i64
        }
        _ => return None,
    };
    let mut time = start.saturating_add_micros(micros);
    if end > start && time > end {
        time = end;
    }
    time.is_valid().then_some(time)
}

/// A whole number of USD; negative amounts are clamped to zero and the product saturates.
fn whole_usd(usd: i64) -> NanoUsd {
    NanoUsd::from_nanos(usd.max(0).saturating_mul(1_000_000_000))
}

fn usd(scalar: Scalar<'_>) -> Option<NanoUsd> {
    match scalar {
        Scalar::Str(s) => NanoUsd::from_usd_decimal(&s),
        Scalar::Double(d) => Some(NanoUsd::from_usd_f64(d)),
        Scalar::Int(i) => Some(whole_usd(i)),
        Scalar::Bool(_) => None,
    }
}

/// Whether `s` is JSON text, checked without building a value. Text nested beyond the parser's recursion limit
/// (128 levels) counts as not JSON.
fn is_json(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

/// A tag list: trimmed, without empty or repeated tags. `None` when no tag is left.
fn tags<'a>(val: &Val<'_, 'a>, split_commas: bool, unquote: bool) -> Option<Vec<Cow<'a, str>>> {
    let items: Vec<Cow<'a, str>> = match val.strings() {
        Some(items) => items,
        None => {
            let Scalar::Str(mut s) = val.scalar()? else {
                return None;
            };
            if unquote && let Some(inner) = string_literal(&s) {
                s = Cow::Owned(inner);
            }
            match Node::parse_container(&s) {
                Some(Node::Arr(items)) if items.iter().all(|i| i.as_str().is_some()) => items
                    .into_iter()
                    .filter_map(|i| match i {
                        Node::Str(s) => Some(Cow::Owned(s)),
                        _ => None,
                    })
                    .collect(),
                _ if split_commas => match s {
                    Cow::Borrowed(s) => s.split(',').map(Cow::Borrowed).collect(),
                    Cow::Owned(s) => s.split(',').map(|t| Cow::Owned(t.to_owned())).collect(),
                },
                _ => vec![s],
            }
        }
    };
    let mut out: Vec<Cow<'a, str>> = Vec::with_capacity(items.len());
    for item in items {
        let trimmed = match item {
            Cow::Borrowed(s) => Cow::Borrowed(s.trim()),
            Cow::Owned(s) => Cow::Owned(s.trim().to_owned()),
        };
        if !trimmed.is_empty() && !out.contains(&trimmed) {
            out.push(trimmed);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// A tool call reshaped to `{id, name, arguments}`: a call without a name is dropped, a missing id or arguments
/// is omitted, and the arguments are kept as sent.
fn tool_call(call: &Node, shape: &ToolCallShape) -> Option<Node> {
    let name = call.get(shape.name)?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let mut members = Vec::with_capacity(3);
    if let Some(id) = call.get(shape.id).filter(|id| !matches!(id, Node::Null)) {
        members.push(("id".to_owned(), id.clone()));
    }
    members.push(("name".to_owned(), Node::Str(name.to_owned())));
    if let Some(arguments) = call.get(shape.arguments) {
        members.push(("arguments".to_owned(), arguments.clone()));
    }
    Some(Node::Obj(members))
}

fn tool_calls(list: &Node, shape: &ToolCallShape) -> Vec<Node> {
    match list {
        Node::Arr(items) => items.iter().filter_map(|c| tool_call(c, shape)).collect(),
        _ => Vec::new(),
    }
}

/// The preview of a body in which every media token is replaced by the placeholder, or `None` when the first
/// preview's worth of the body holds no token.
fn media_preview(body: &[u8], tokens: &[MediaToken]) -> Option<Box<str>> {
    let mut rest = core::str::from_utf8(body).ok()?;
    let mut out = String::new();
    let mut replaced = false;
    while out.len() < caps::PREVIEW_BYTES && !rest.is_empty() {
        // A token that starts beyond what the preview still shows does not matter.
        let window = truncate_str(rest, caps::PREVIEW_BYTES - out.len());
        let first = tokens
            .iter()
            .filter_map(|t| window.find(t.prefix).map(|at| (at, t)))
            .min_by_key(|&(at, _)| at);
        let Some((at, token)) = first else {
            out.push_str(window);
            break;
        };
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let inside = from.get(token.prefix.len()..).unwrap_or_default();
        let search = truncate_str(inside, MEDIA_TOKEN_MAX_BYTES);
        let end = if token.suffix.is_empty() {
            Some(
                search
                    .find(|c: char| c == '"' || c.is_whitespace())
                    .unwrap_or(search.len()),
            )
        } else {
            search.find(token.suffix).map(|i| i + token.suffix.len())
        };
        match end {
            Some(end) => {
                out.push_str(MEDIA_PLACEHOLDER);
                replaced = true;
                rest = inside.get(end..).unwrap_or_default();
            }
            None => {
                // Not a token: the prefix is ordinary text.
                out.push_str(token.prefix);
                rest = inside;
            }
        }
    }
    replaced.then(|| truncate_str(&out, caps::PREVIEW_BYTES).into())
}

fn cut(s: &str, cap: usize) -> Box<str> {
    truncate_str(s, cap).into()
}

fn list(items: &[Cow<'_, str>], max_items: usize, item_bytes: usize) -> Vec<Box<str>> {
    items
        .iter()
        .take(max_items)
        .map(|s| cut(s, item_bytes))
        .collect()
}

/// A body: a string verbatim, any other value as JSON.
fn body(value: Body<'_>) -> Bytes {
    match value {
        Body::View(AnyValueView::String(s)) => Bytes::copy_from_slice(s.as_bytes()),
        Body::View(other) => json::value(other),
        Body::Bytes(bytes) => Bytes::from(bytes),
        Body::Elements(elements, _) => Bytes::from(Node::Arr(elements).render()),
    }
}

/// Keeps the winning candidate of each key: the lowest rank, then the first offered. At most `cap` keys are kept;
/// beyond it keys are dropped in rank order, then name order. The winners come back in ascending key order.
fn merge_keyed<V>(mut candidates: Vec<Keyed<'_, V>>, cap: usize) -> Vec<Keyed<'_, V>> {
    candidates.sort_by(|a, b| (&a.key, a.rank).cmp(&(&b.key, b.rank)));
    candidates.dedup_by(|later, earlier| later.key == earlier.key);
    if candidates.len() > cap {
        candidates.sort_by(|a, b| (a.rank, &a.key).cmp(&(b.rank, &b.key)));
        candidates.truncate(cap);
        candidates.sort_by(|a, b| a.key.cmp(&b.key));
    }
    candidates
}

/// The state of one span's mapping.
struct Mapping<'x, 'a> {
    index: &'x Index,
    chosen: &'static DialectTable,
    json_text: bool,
    ctx: &'x MapCtx<'x>,
    span: &'x SpanView<'a>,
    start: Timestamp,
    end: Timestamp,
    slots: [Option<Candidate<'a>>; SCALAR_SLOTS],
    usage: Vec<Keyed<'a, u64>>,
    cost: Vec<Keyed<'a, NanoUsd>>,
    members: Vec<Member<'a>>,
    families: SmallVec<[Vec<FamilyKey<'a>>; 12]>,
    /// (rule priority, attribute position, type, consume)
    kind: Option<(u32, u32, ObservationType, bool)>,
    /// The JSON tree of each attribute a member path read, by attribute position.
    parsed: SmallVec<[(u32, Option<Node>); 2]>,
    gates: SmallVec<[(&'static SdkGate, bool); 1]>,
}

impl<'a> Mapping<'_, 'a> {
    /// Whether an SDK gate holds: the SDK is known and its major version is at least the gate's.
    fn gate(&mut self, gate: &'static SdkGate) -> bool {
        if let Some(&(_, holds)) = self.gates.iter().find(|(g, _)| core::ptr::eq(*g, gate)) {
            return holds;
        }
        let holds = self.gate_holds(gate).unwrap_or(false);
        self.gates.push((gate, holds));
        holds
    }

    fn gate_holds(&self, gate: &SdkGate) -> Option<bool> {
        let canonical = |name: &str| {
            gate.names
                .iter()
                .find(|(spelling, _)| *spelling == name)
                .map(|&(_, canonical)| canonical)
        };
        let name = canonical(self.ctx.frame.sdk_name).or_else(|| {
            self.span
                .resource()
                .attributes()
                .find_map(|(key, value)| match value {
                    AnyValueView::String(language) if key == SDK_LANGUAGE_KEY => {
                        Some(canonical(language))
                    }
                    _ => None,
                })
                .flatten()
        })?;
        let &(_, min_major) = gate.min_major.iter().find(|(n, _)| *n == name)?;
        let scope = self.span.scope();
        let version = if scope.name().starts_with(gate.scope_prefix) {
            scope.version()
        } else {
            self.ctx.frame.sdk_version
        };
        let digits = version
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(version.len());
        let major: u32 = version.get(..digits)?.parse().ok()?;
        Some(major >= min_major)
    }

    /// The JSON tree of the span attribute at `at`, parsed at most once.
    fn tree(&mut self, at: u32, value: &AnyValueView<'a>) -> Option<&Node> {
        let found = match self.parsed.iter().position(|(i, _)| *i == at) {
            Some(found) => found,
            None => {
                // Any JSON text is kept: a string literal that itself holds JSON text is read through by the
                // path.
                let node = match value {
                    AnyValueView::String(s) => Node::parse(s),
                    AnyValueView::Array(_) | AnyValueView::KvList(_) => {
                        Some(Node::from_view(value.clone()))
                    }
                    _ => None,
                };
                self.parsed.push((at, node));
                self.parsed.len() - 1
            }
        };
        self.parsed.get(found).and_then(|(_, node)| node.as_ref())
    }

    /// Reads a scalar with `read`. On a span whose dialect encodes values as JSON text, a string holding one JSON
    /// scalar is unwrapped first; when `read` rejects the unwrapped scalar it reads the string as sent.
    fn scalar<T>(&self, val: &Val<'_, 'a>, read: impl Fn(Scalar<'a>) -> Option<T>) -> Option<T> {
        if self.json_text
            && let Val::View(AnyValueView::String(s)) = *val
        {
            match Node::parse(s) {
                Some(Node::Null) => return None,
                Some(node) => {
                    if let Some(value) = Scalar::from_node(node).and_then(&read) {
                        return Some(value);
                    }
                }
                None => {}
            }
        }
        read(val.scalar()?)
    }

    /// Decodes a value for a field that takes one value.
    fn decode(&mut self, hit: &SourceHit, val: &Val<'_, 'a>) -> Option<Value<'a>> {
        let unquote = hit.literal_gate.is_some_and(|gate| self.gate(gate));
        match hit.source.decoder {
            Decoder::String if is_list(hit.target) => match val.strings() {
                Some(items) => (!items.is_empty()).then_some(Value::List(items)),
                None => self
                    .scalar(val, |s| text(s, false, unquote))
                    .map(|s| Value::List(vec![s])),
            },
            Decoder::String => self
                .scalar(val, |s| text(s, false, unquote))
                .map(Value::Text),
            Decoder::StringOrInt => self
                .scalar(val, |s| text(s, true, unquote))
                .map(Value::Text),
            Decoder::Bool => {
                let value = self.scalar(val, flag)?;
                match hit.target {
                    // Only `true` sets a root flag.
                    Target::IsRoot | Target::IsAppRoot => value.then_some(Value::Flag(true)),
                    _ => Some(Value::Flag(value)),
                }
            }
            Decoder::Level(map) => self.scalar(val, |s| level(map, s)).map(Value::Level),
            Decoder::Tags { split_commas } => tags(val, split_commas, unquote).map(Value::List),
            Decoder::Timestamp => self.scalar(val, instant).map(Value::Time),
            Decoder::OffsetFromStart(unit) => {
                let (start, end) = (self.start, self.end);
                self.scalar(val, |s| offset(start, end, unit, s))
                    .map(Value::Time)
            }
            Decoder::Json => match *val {
                Val::View(AnyValueView::Empty | AnyValueView::String("")) => None,
                Val::View(AnyValueView::String(s)) if !is_json(s) => None,
                Val::View(view) => Some(Value::Body(Body::View(view.clone()), IoShape::Raw)),
                Val::Node(Node::Str(s)) if !is_json(s) => None,
                Val::Node(Node::Str(s)) => Some(Value::Body(
                    Body::Bytes(s.clone().into_bytes()),
                    IoShape::Raw,
                )),
                Val::Node(Node::Null) => None,
                Val::Node(node) => Some(Value::Body(Body::Bytes(node.render()), IoShape::Raw)),
            },
            Decoder::JsonOrString(shape) => match *val {
                Val::View(AnyValueView::Empty | AnyValueView::String("")) => None,
                Val::View(view) => Some(Value::Body(Body::View(view.clone()), shape)),
                Val::Node(Node::Null) => None,
                Val::Node(Node::Str(s)) if s.is_empty() => None,
                Val::Node(Node::Str(s)) => {
                    Some(Value::Body(Body::Bytes(s.clone().into_bytes()), shape))
                }
                Val::Node(node) => Some(Value::Body(Body::Bytes(node.render()), shape)),
            },
            Decoder::JsonStrings => {
                let items = val.strings()?;
                if items.is_empty() {
                    return None;
                }
                let parsed = items
                    .into_iter()
                    .map(|s| Node::parse(&s).unwrap_or_else(|| Node::Str(s.into_owned())))
                    .collect();
                Some(Value::Body(
                    Body::Bytes(Node::Arr(parsed).render()),
                    IoShape::Raw,
                ))
            }
            Decoder::ToolCalls(shape) => {
                let calls = tool_calls(&val.container()?, shape);
                (!calls.is_empty()).then_some(Value::Calls(calls))
            }
            // These fill usage, cost or a map, or read a family; a validated table never pairs one with a field
            // that takes one value.
            Decoder::Integer
            | Decoder::Usd
            | Decoder::JsonObject
            | Decoder::UsageObject(_)
            | Decoder::PrefixUsage(_)
            | Decoder::CostObject(_)
            | Decoder::PrefixMap(_)
            | Decoder::MapMember(_)
            | Decoder::Indexed(_)
            | Decoder::IndexedList(_) => None,
        }
    }

    /// One member of a prefix map.
    fn prefix_member(
        &mut self,
        hit: &SourceHit,
        spec: &PrefixSpec,
        origin: Origin,
        key: &'a str,
        name: &'a str,
        value: &AnyValueView<'a>,
    ) {
        let Slot::Map(map) = hit.slot else {
            return;
        };
        if name.is_empty()
            || spec.except.contains(&key)
            || matches!(*value, AnyValueView::Empty | AnyValueView::String(""))
        {
            return;
        }
        let value = match (spec.values, value) {
            (MapValues::JsonWhen(gate), AnyValueView::String(s)) if self.gate(gate) => {
                Node::parse(s).map_or_else(|| MemberValue::View(value.clone()), MemberValue::Node)
            }
            _ => MemberValue::View(value.clone()),
        };
        self.members.push(Member {
            map,
            rank: hit.rank,
            name: Cow::Borrowed(name),
            origin,
            value,
        });
    }

    /// The members a source adds to a map.
    fn map_source(&mut self, hit: &SourceHit, map: u8, origin: Origin, val: &Val<'_, 'a>) {
        match hit.source.decoder {
            Decoder::JsonObject => {
                let Some(Node::Obj(members)) = val.container() else {
                    return;
                };
                for (name, value) in members {
                    self.members.push(Member {
                        map,
                        rank: hit.rank,
                        name: Cow::Owned(name),
                        origin,
                        value: MemberValue::Node(value),
                    });
                }
            }
            Decoder::MapMember(name) => {
                let value = match *val {
                    Val::View(
                        AnyValueView::Empty | AnyValueView::String("") | AnyValueView::KvList(_),
                    ) => return,
                    Val::View(AnyValueView::String(s)) => match Node::parse_container(s) {
                        Some(node) => MemberValue::Node(node),
                        None if self.json_text => match Node::parse(s) {
                            Some(Node::Null) => return,
                            Some(node) => MemberValue::Node(node),
                            None => MemberValue::View(AnyValueView::String(s)),
                        },
                        None => MemberValue::View(AnyValueView::String(s)),
                    },
                    Val::View(view) => MemberValue::View(view.clone()),
                    Val::Node(Node::Null | Node::Obj(_)) => return,
                    Val::Node(node) => MemberValue::Node(node.clone()),
                };
                self.members.push(Member {
                    map,
                    rank: hit.rank,
                    name: Cow::Borrowed(name),
                    origin,
                    value,
                });
            }
            _ => {}
        }
    }

    fn usage_source(&mut self, hit: &SourceHit, origin: Origin, val: &Val<'_, 'a>) {
        match (hit.target, hit.source.decoder) {
            (Target::Usage(key), Decoder::Integer) => {
                let count = self.scalar(val, |s| match s {
                    Scalar::Int(i) => u64::try_from(i).ok(),
                    _ => None,
                });
                if let Some(value) = count {
                    self.usage.push(Keyed {
                        key: Cow::Borrowed(key),
                        value,
                        rank: hit.rank,
                        origin,
                    });
                }
            }
            (Target::UsageAll, Decoder::UsageObject(map)) => {
                let Some(Node::Obj(members)) = val.container() else {
                    return;
                };
                let members = members.into_iter().map(|(k, v)| (k, v, ())).collect();
                for entry in usage::usage(map, members) {
                    self.usage.push(Keyed {
                        key: entry.key,
                        value: entry.value,
                        rank: hit.rank,
                        origin,
                    });
                }
            }
            _ => {}
        }
    }

    fn cost_source(&mut self, hit: &SourceHit, origin: Origin, name: &'a str, val: &Val<'_, 'a>) {
        match (hit.target, hit.source.decoder) {
            (Target::Cost(_) | Target::CostAll, Decoder::Usd) => {
                let key = match hit.target {
                    Target::Cost(key) => key,
                    _ => name,
                };
                if key.is_empty() {
                    return;
                }
                if let Some(value) = self.scalar(val, usd) {
                    self.cost.push(Keyed {
                        key: Cow::Borrowed(key),
                        value,
                        rank: hit.rank,
                        origin,
                    });
                }
            }
            (Target::CostAll, Decoder::CostObject(map)) => {
                let Some(Node::Obj(members)) = val.container() else {
                    return;
                };
                let members = members.into_iter().map(|(k, v)| (k, v, ())).collect();
                for entry in usage::cost(map, members) {
                    self.cost.push(Keyed {
                        key: entry.key,
                        value: entry.value,
                        rank: hit.rank,
                        origin,
                    });
                }
            }
            _ => {}
        }
    }

    /// Offers one attribute to one source that reads its key.
    fn source(&mut self, hit: &SourceHit, origin: Origin, key: &'a str, value: &AnyValueView<'a>) {
        if hit.own && hit.dialect != self.chosen.dialect {
            return;
        }
        let source = hit.source;
        // What follows the prefix of a prefix source: a member name, or the rest of a family key.
        let name = match source.key {
            KeyMatch::Prefix(prefix) => key.get(prefix.len()..).unwrap_or_default(),
            KeyMatch::Exact(_) => "",
        };
        match source.decoder {
            Decoder::PrefixMap(spec) => {
                return self.prefix_member(hit, spec, origin, key, name, value);
            }
            Decoder::Indexed(_) | Decoder::IndexedList(_) => {
                if let Some((index, rest)) = indexed::split_index(name)
                    && let Some(family) = self.families.get_mut(usize::from(hit.family))
                {
                    family.push((index, rest, value.clone(), origin));
                }
                return;
            }
            Decoder::PrefixUsage(_) => {
                if !name.is_empty()
                    && let Some(family) = self.families.get_mut(usize::from(hit.family))
                {
                    family.push((0, name, value.clone(), origin));
                }
                return;
            }
            _ => {}
        }
        let node;
        let (val, origin) = if source.path.is_empty() {
            (Val::View(value), origin)
        } else {
            let Origin::Span(at) = origin else {
                return;
            };
            let Some(found) = self.tree(at, value).and_then(|root| root.at(source.path)) else {
                return;
            };
            node = found;
            // A member path never consumes its attribute.
            (Val::Node(&node), Origin::None)
        };
        match hit.slot {
            Slot::Scalar(slot) => {
                let slot = usize::from(slot);
                if self
                    .slots
                    .get(slot)
                    .and_then(Option::as_ref)
                    .is_none_or(|c| c.rank > hit.rank)
                    && let Some(value) = self.decode(hit, &val)
                    && let Some(place) = self.slots.get_mut(slot)
                {
                    *place = Some(Candidate {
                        target: hit.target,
                        rank: hit.rank,
                        layer: hit.layer,
                        origin,
                        family: u16::MAX,
                        value,
                    });
                }
            }
            Slot::Usage => self.usage_source(hit, origin, &val),
            Slot::Cost => self.cost_source(hit, origin, name, &val),
            Slot::Map(map) => self.map_source(hit, map, origin, &val),
        }
    }

    /// Applies one observation-type rule to a span attribute.
    fn type_rule(
        &mut self,
        priority: u32,
        rule: &'static crucible_compat::TypeRule,
        at: u32,
        value: &AnyValueView<'a>,
    ) {
        if self.kind.is_some_and(|(p, ..)| p <= priority) {
            return;
        }
        let found = if rule.path.is_empty() {
            self.scalar(&Val::View(value), |s| match s {
                Scalar::Str(s) => rule.values.iter().find(|(v, _)| *v == s).map(|&(_, t)| t),
                _ => None,
            })
        } else {
            self.tree(at, value)
                .and_then(|root| root.at(rule.path))
                .and_then(|node| {
                    let text = node.as_str()?;
                    rule.values
                        .iter()
                        .find(|(v, _)| *v == text)
                        .map(|&(_, t)| t)
                })
        };
        if let Some(observation_type) = found {
            self.kind = Some((priority, at, observation_type, rule.consume));
        }
    }

    /// Turns the collected families into candidates.
    fn families(&mut self, used: &mut Vec<(u16, Vec<Origin>, bool)>) {
        let index = self.index;
        for (family, hit) in index.family_sources.iter().enumerate() {
            let keys = match self.families.get_mut(family) {
                Some(keys) if !keys.is_empty() => core::mem::take(keys),
                _ => continue,
            };
            match (hit.source.decoder, hit.slot) {
                (Decoder::Indexed(layout) | Decoder::IndexedList(layout), Slot::Scalar(slot)) => {
                    let slot = usize::from(slot);
                    if self
                        .slots
                        .get(slot)
                        .and_then(Option::as_ref)
                        .is_some_and(|c| c.rank <= hit.rank)
                    {
                        continue;
                    }
                    let Some(rebuilt) = indexed::rebuild(layout, &keys) else {
                        continue;
                    };
                    let shape = if matches!(hit.source.decoder, Decoder::Indexed(_)) {
                        IoShape::IndexedMessages
                    } else {
                        IoShape::Raw
                    };
                    used.push((family as u16, rebuilt.used, rebuilt.truncated));
                    if let Some(place) = self.slots.get_mut(slot) {
                        *place = Some(Candidate {
                            target: hit.target,
                            rank: hit.rank,
                            layer: hit.layer,
                            origin: Origin::None,
                            family: family as u16,
                            value: Value::Body(Body::Elements(rebuilt.elements, layout), shape),
                        });
                    }
                }
                (Decoder::PrefixUsage(map), Slot::Usage) => {
                    let members = keys
                        .into_iter()
                        .map(|(_, name, value, origin)| {
                            (name.to_owned(), Node::from_view(value), origin)
                        })
                        .collect();
                    for entry in usage::usage(map, members) {
                        self.usage.push(Keyed {
                            key: entry.key,
                            value: entry.value,
                            rank: hit.rank,
                            origin: entry.origin.unwrap_or(Origin::None),
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

/// The members of one map in rank order then wire order, each name once.
fn merge_members<'m, 'a>(members: &'m [Member<'a>], map: u8) -> Vec<&'m Member<'a>> {
    let mut seen = det_set_with_capacity::<&str>(0);
    members
        .iter()
        .filter(|m| m.map == map)
        .filter(|m| seen.insert(&*m.name))
        .collect()
}

fn push_member(out: &mut Vec<u8>, value: &MemberValue<'_>) {
    match value {
        MemberValue::View(view) => json::push_value(out, view.clone()),
        MemberValue::Node(node) => node.push(out),
    }
}

/// The members as a JSON object, in the order given; empty bytes (absent) when there are none.
fn object(members: &[&Member<'_>]) -> Bytes {
    if members.is_empty() {
        return Bytes::new();
    }
    let mut out = vec![b'{'];
    for (i, member) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        json::push_str(&mut out, &member.name);
        out.push(b':');
        push_member(&mut out, &member.value);
    }
    out.push(b'}');
    Bytes::from(out)
}

/// The dialect of a span (contract 10.2) among the layers of `index`: the highest detection confidence, ties going
/// to the lowest id; `hint` counts as a detection of its dialect at confidence 60; the default is 0.
pub(crate) fn detect(index: &Index, span: &SpanView<'_>, hint: u8) -> u8 {
    // (confidence, dialect)
    let mut best: (u8, u8) = (0, 0);
    let mut offer = |confidence: u8, dialect: u8| {
        if confidence > best.0 || (confidence == best.0 && dialect < best.1) {
            best = (confidence, dialect);
        }
    };
    if hint != 0 && index.layers().iter().any(|t| t.dialect == hint) {
        offer(Confidence::HINT.0, hint);
    }
    let scope = span.scope().name();
    for &(name, dialect, confidence) in &index.scopes {
        if name.matches(scope) {
            offer(confidence, dialect);
        }
    }
    for (key, _) in span.attributes() {
        index.span(key, |hit| {
            if let Hit::Detect {
                dialect,
                confidence,
            } = *hit
            {
                offer(confidence, dialect);
            }
        });
    }
    best.1
}

/// How sure one table is that a span belongs to its dialect: the highest confidence among its rules that match,
/// and 60 when the request hint names the dialect.
pub(crate) fn confidence(
    table: &DialectTable,
    scope: &str,
    span: &SpanView<'_>,
    hint: u8,
) -> Option<Confidence> {
    let hinted = (table.dialect != 0 && hint == table.dialect).then_some(Confidence::HINT);
    table
        .detect
        .iter()
        .filter(|rule| match rule.on {
            DetectOn::ScopeName(name) => name.matches(scope),
            DetectOn::SpanKey(key) => span.attributes().any(|(k, _)| key.matches(k)),
        })
        .map(|rule| rule.confidence)
        .chain(hinted)
        .max()
}

/// What the pass over a span's attributes saw of the MCP wire contract (contract 9.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Marks {
    /// The span carries `mcp.method.name`: it is an MCP operation.
    pub(crate) mcp: bool,
    /// The span carries a tool list.
    pub(crate) tools: bool,
}

/// Maps one span with the layers of `index`, `chosen` being the table of the span's dialect. Returns whether the
/// span is an MCP operation or carries a tool list, which the same pass over its attributes sees.
///
/// The residual `attributes` hold what no source consumed, less the keys that must never be stored
/// ([`mcp_keys::never_stored`]) and, on an MCP operation, the keys its projector consumes (contract 9.1).
pub(crate) fn map_span<'a>(
    index: &Index,
    chosen: &'static DialectTable,
    ctx: &MapCtx<'_>,
    span: &SpanView<'a>,
    out: &mut SpanDraft,
) -> Marks {
    let mut marks = Marks::default();
    let mut mapping = Mapping {
        index,
        chosen,
        json_text: chosen.encoding == ValueEncoding::JsonText,
        ctx,
        span,
        start: out.record.core.start_time,
        end: out.record.core.end_time,
        slots: core::array::from_fn(|_| None),
        usage: Vec::new(),
        cost: Vec::new(),
        members: Vec::new(),
        families: (0..index.families).map(|_| Vec::new()).collect(),
        kind: None,
        parsed: SmallVec::new(),
        gates: SmallVec::new(),
    };
    for (at, (key, value)) in span.resource().attributes().enumerate() {
        index.resource(key, |hit| {
            if let Hit::Source(hit) = hit {
                mapping.source(hit, Origin::Resource(at as u32), key, &value);
            }
        });
    }
    for (at, (key, value)) in span.attributes().enumerate() {
        let at = at as u32;
        marks.mcp |= key == mcp_keys::METHOD;
        marks.tools |= key == mcp_keys::TOOLS;
        index.span(key, |hit| match hit {
            Hit::Source(hit) => mapping.source(hit, Origin::Span(at), key, &value),
            Hit::Type { priority, rule } => mapping.type_rule(*priority, rule, at, &value),
            Hit::Detect { .. } => {}
        });
    }
    if !index.events.is_empty() {
        for event in span.events() {
            let name = event.name();
            for source in index.events.iter().filter(|s| s.name == name) {
                let hit = &source.hit;
                if !source.time {
                    for (key, value) in event.attributes() {
                        if hit.source.key.matches(key) {
                            mapping.source(hit, Origin::None, key, &value);
                        }
                    }
                    continue;
                }
                let Slot::Scalar(slot) = hit.slot else {
                    continue;
                };
                let live = !hit.own || hit.dialect == chosen.dialect;
                if let Some(place) = mapping.slots.get_mut(usize::from(slot))
                    && live
                    && event.time().is_valid()
                    && place.as_ref().is_none_or(|c| c.rank > hit.rank)
                {
                    *place = Some(Candidate {
                        target: hit.target,
                        rank: hit.rank,
                        layer: hit.layer,
                        origin: Origin::None,
                        family: u16::MAX,
                        value: Value::Time(event.time()),
                    });
                }
            }
        }
    }
    let mut family_keys: Vec<(u16, Vec<Origin>, bool)> = Vec::new();
    mapping.families(&mut family_keys);
    let Mapping {
        slots,
        usage,
        cost,
        mut members,
        kind,
        ..
    } = mapping;

    let (mut span_used, mut resource_used) = (Bits::default(), Bits::default());
    let mut consume = |origin: Origin| match origin {
        Origin::Span(at) => span_used.set(at),
        Origin::Resource(at) => resource_used.set(at),
        Origin::None => {}
    };
    if let Some((_, at, _, true)) = kind {
        consume(Origin::Span(at));
    }

    let record = &mut out.record;
    let (core, payload) = (&mut record.core, &mut record.payload);

    // Maps: every source that decodes, merged per member.
    members.sort_by_key(|m| (m.map, m.rank));
    for map in 0..MAPS as u8 {
        let mut merged = merge_members(&members, map);
        if map < 2 {
            merged.truncate(caps::METADATA_MAX_KEYS);
        }
        if merged.is_empty() {
            continue;
        }
        for member in &merged {
            consume(member.origin);
        }
        match map {
            0 => {
                payload.metadata = object(&merged);
                for member in &merged {
                    // A string is stored as it is, any other value as its JSON text.
                    let rendered;
                    let value: &str = match &member.value {
                        MemberValue::View(AnyValueView::String(s)) => s,
                        MemberValue::Node(Node::Str(s)) => s,
                        other => {
                            let mut text = Vec::new();
                            push_member(&mut text, other);
                            rendered = text;
                            core::str::from_utf8(&rendered).unwrap_or_default()
                        }
                    };
                    let pair = format!("{}={value}", member.name);
                    payload.metadata_keys.push((&*member.name).into());
                    payload.metadata_values.push(value.into());
                    payload
                        .metadata_kv
                        .push(cut(&pair, caps::METADATA_KV_ITEM_BYTES));
                }
            }
            1 => payload.trace_metadata = object(&merged),
            _ => payload.model_parameters = object(&merged),
        }
    }

    // Usage and provided cost: per key.
    for winner in merge_keyed(usage, caps::USAGE_MAX_KEYS) {
        consume(winner.origin);
        payload
            .usage_provided
            .push((winner.key.into(), winner.value));
    }
    for winner in merge_keyed(cost, caps::USAGE_MAX_KEYS) {
        consume(winner.origin);
        payload
            .cost_provided
            .push((winner.key.into(), winner.value));
    }

    let (mut input, mut output) = (None, None);
    let mut output_elements: Option<(Vec<Node>, &'static IndexedLayout)> = None;
    let mut calls: Option<Vec<Node>> = None;
    for candidate in slots.into_iter().flatten() {
        consume(candidate.origin);
        if let Some((_, used, truncated)) = family_keys
            .iter()
            .find(|(family, ..)| *family == candidate.family)
        {
            for origin in used {
                consume(*origin);
            }
            let field = match candidate.target {
                Target::Input => Some(PayloadField::Input),
                Target::Output => Some(PayloadField::Output),
                Target::ToolDefinitions => Some(PayloadField::ToolDefinitions),
                _ => None,
            };
            if let (true, Some(field)) = (*truncated, field) {
                payload.truncated_fields.insert(field);
            }
        }
        match (candidate.target, candidate.value) {
            (Target::ServiceName, Value::Text(s)) => {
                core.service_name = cut(&s, caps::SERVICE_BYTES)
            }
            (Target::ServiceVersion, Value::Text(s)) => {
                core.service_version = cut(&s, caps::SERVICE_BYTES);
            }
            (Target::TelemetrySdkName, Value::Text(s)) => {
                core.telemetry_sdk_name = cut(&s, caps::SDK_BYTES);
            }
            (Target::TelemetrySdkLanguage, Value::Text(s)) => {
                core.telemetry_sdk_language = cut(&s, caps::SDK_BYTES);
            }
            (Target::TelemetrySdkVersion, Value::Text(s)) => {
                core.telemetry_sdk_version = cut(&s, caps::SDK_BYTES);
            }
            (Target::UserId, Value::Text(s)) => core.user_id = cut(&s, caps::TRACE_ATTRIBUTE_BYTES),
            (Target::SessionId, Value::Text(s)) => {
                core.session_id = cut(&s, caps::TRACE_ATTRIBUTE_BYTES);
            }
            (Target::TraceName, Value::Text(s)) => {
                core.trace_name = cut(&s, caps::TRACE_ATTRIBUTE_BYTES);
            }
            (Target::Environment, Value::Text(s)) => {
                core.environment = cut(&s, caps::ENVIRONMENT_BYTES);
            }
            (Target::Release, Value::Text(s)) => core.release = cut(&s, caps::RELEASE_BYTES),
            (Target::Version, Value::Text(s)) => core.version = cut(&s, caps::RELEASE_BYTES),
            (Target::StatusMessage, Value::Text(s)) => {
                core.status_message = cut(&s, caps::STATUS_MESSAGE_BYTES);
            }
            (Target::Tags, Value::List(v)) => {
                core.tags = list(&v, caps::TAGS_MAX_ITEMS, caps::TAG_BYTES);
            }
            (Target::TraceTags, Value::List(v)) => {
                core.trace_tags = list(&v, caps::TAGS_MAX_ITEMS, caps::TAG_BYTES);
            }
            (Target::ToolNames, Value::List(v)) => {
                core.tool_names = list(&v, caps::TOOL_NAMES_MAX_ITEMS, caps::TOOL_NAME_BYTES);
            }
            (Target::FinishReasons, Value::List(v)) => {
                core.finish_reasons = list(
                    &v,
                    caps::FINISH_REASONS_MAX_ITEMS,
                    caps::FINISH_REASON_BYTES,
                );
            }
            (Target::TracePublic, Value::Flag(public)) => {
                core.trace_public = if public {
                    TracePublic::Public
                } else {
                    TracePublic::Private
                };
            }
            (Target::IsRoot, Value::Flag(true)) => core.is_root = true,
            (Target::IsAppRoot, Value::Flag(true)) => core.is_app_root = true,
            (Target::Level, Value::Level(level)) => out.level = Some(level),
            (Target::PromptVersion, Value::Text(s)) => {
                core.prompt_version = cut(&s, caps::PROMPT_VERSION_BYTES);
            }
            (Target::CompletionStartTime, Value::Time(t)) => core.completion_start_time = Some(t),
            (Target::ModelRequested, Value::Text(s)) => {
                core.model_requested = cut(&s, caps::MODEL_BYTES);
            }
            (Target::ModelResponse, Value::Text(s)) => {
                core.model_response = cut(&s, caps::MODEL_BYTES);
            }
            (Target::Provider, Value::Text(s)) => core.provider = cut(&s, caps::MODEL_BYTES),
            (Target::PromptName, Value::Text(s)) => {
                core.prompt_name = cut(&s, caps::PROMPT_NAME_BYTES);
            }
            (Target::Input, Value::Body(v, shape)) => {
                payload.input = body(v);
                input = Some((shape, candidate.layer));
            }
            (Target::Output, Value::Body(v, shape)) => {
                if let Body::Elements(elements, layout) = &v {
                    output_elements = Some((elements.clone(), *layout));
                }
                payload.output = body(v);
                output = Some((shape, candidate.layer));
            }
            (Target::SystemInstructions, Value::Body(v, _)) => {
                payload.system_instructions = body(v);
            }
            (Target::ToolDefinitions, Value::Body(v, _)) => payload.tool_definitions = body(v),
            (Target::ToolCalls, Value::Calls(list)) => calls = Some(list),
            // A validated table never pairs a target with another kind of value.
            _ => {}
        }
    }

    // Derived fields.
    if let Some((elements, layout)) = &output_elements {
        if calls.is_none()
            && let Some((member, shape)) = &layout.tool_calls
        {
            let found: Vec<Node> = elements
                .iter()
                .filter_map(|e| e.member(member))
                .flat_map(|list| tool_calls(list, shape))
                .collect();
            calls = (!found.is_empty()).then_some(found);
        }
        if core.finish_reasons.is_empty()
            && let Some(member) = layout.finish_reason
        {
            core.finish_reasons = elements
                .iter()
                .filter_map(|e| e.member(member)?.as_str())
                .filter(|reason| !reason.is_empty())
                .take(caps::FINISH_REASONS_MAX_ITEMS)
                .map(|reason| cut(reason, caps::FINISH_REASON_BYTES))
                .collect();
        }
    }
    if calls.is_none()
        && matches!(output, Some((IoShape::GenaiMessages, _)))
        && memchr::memmem::find(&payload.output, GENAI_PARTS.tool_call.as_bytes()).is_some()
        && let Some(Node::Arr(messages)) = core::str::from_utf8(&payload.output)
            .ok()
            .and_then(Node::parse_container)
    {
        let found: Vec<Node> = messages
            .iter()
            .filter_map(|message| match message.member(GENAI_PARTS.parts) {
                Some(Node::Arr(parts)) => Some(parts),
                _ => None,
            })
            .flatten()
            .filter(|part| {
                part.member(GENAI_PARTS.kind).and_then(Node::as_str) == Some(GENAI_PARTS.tool_call)
            })
            .filter_map(|part| tool_call(part, &GENAI_PARTS.shape))
            .collect();
        calls = (!found.is_empty()).then_some(found);
    }
    if let Some(calls) = calls {
        core.tool_call_count = u32::try_from(calls.len()).unwrap_or(u32::MAX);
        if core.tool_names.is_empty() {
            for call in &calls {
                let Some(name) = call.member("name").and_then(Node::as_str) else {
                    continue;
                };
                let name = cut(name, caps::TOOL_NAME_BYTES);
                if core.tool_names.len() < caps::TOOL_NAMES_MAX_ITEMS
                    && !core.tool_names.contains(&name)
                {
                    core.tool_names.push(name);
                }
            }
        }
        payload.tool_calls = Bytes::from(Node::Arr(calls).render());
    }

    core.io_shape = input.or(output).map_or(IoShape::Raw, |(shape, _)| shape);
    core.observation_type = kind.map_or(ObservationType::Span, |(_, _, t, _)| t);

    // Previews with media tokens replaced. Only the layer that filled the field and the chosen dialect apply.
    let layers = index.layers();
    let media = |filled: Option<(IoShape, u8)>, body: &Bytes| -> Option<Box<str>> {
        let (_, layer) = filled?;
        let own = layers.get(usize::from(layer)).map_or(&[][..], |t| t.media);
        if own.is_empty() && chosen.media.is_empty() {
            return None;
        }
        let tokens: SmallVec<[MediaToken; 2]> = own.iter().chain(chosen.media).copied().collect();
        media_preview(body, &tokens)
    };
    if let Some(preview) = media(input, &payload.input) {
        core.input_preview = preview;
    }
    if let Some(preview) = media(output, &payload.output) {
        core.output_preview = preview;
    }

    payload.attributes = json::object_or_empty(
        span.attributes()
            .enumerate()
            .filter(|(i, _)| !span_used.get(*i))
            .map(|(_, pair)| pair)
            .filter(|(key, _)| {
                !mcp_keys::never_stored(key) && !(marks.mcp && mcp_keys::consumed(key))
            }),
    );
    payload.resource_attributes = json::object_or_empty(
        span.resource()
            .attributes()
            .enumerate()
            .filter(|(i, _)| !resource_used.get(*i))
            .map(|(_, pair)| pair)
            .filter(|(key, _)| !mcp_keys::never_stored(key)),
    );
    payload.events = json::events(span.events());
    marks
}

#[cfg(test)]
mod tests;
