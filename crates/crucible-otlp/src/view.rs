//! Views over a validated request. They borrow the request bytes and decode lazily; iterating
//! attributes yields borrowed keys and typed values and never allocates.
//!
//! Every view comes from a [`TraceRequest`](crate::TraceRequest), whose scan already decoded each of
//! these bytes without error, so decoding here cannot fail. Should it ever fail, iteration ends.

use crucible_records::{SpanId, SpanKind, StatusCode, Timestamp, TraceId};

use crate::scan::{Rejection, rejection};
use crate::schema::{self, Msg, SPAN_ID_LEN, TRACE_ID_LEN};
use crate::wire::{Fields, RECURSION_LIMIT, Value, WireError};

/// Views decode bytes that the scan already accepted, so they never need the nesting limit; they
/// pass the top level, which accepts every depth the scan accepted.
const ANY_DEPTH: u32 = RECURSION_LIMIT;

fn no_child(_: Msg, _: &[u8]) -> Result<(), WireError> {
    Ok(())
}

fn trace_id(bytes: &[u8]) -> TraceId {
    <[u8; TRACE_ID_LEN]>::try_from(bytes).map_or(TraceId::ZERO, TraceId::from_bytes)
}

fn span_id(bytes: &[u8]) -> SpanId {
    <[u8; SPAN_ID_LEN]>::try_from(bytes).map_or(SpanId::ZERO, SpanId::from_bytes)
}

/// The length-delimited occurrences of one field of a message, in wire order.
#[derive(Debug, Clone)]
struct Repeated<'a> {
    fields: Fields<'a>,
    number: u32,
}

impl<'a> Repeated<'a> {
    fn new(message: &'a [u8], number: u32) -> Self {
        Self {
            fields: Fields::new(message, ANY_DEPTH),
            number,
        }
    }
}

impl<'a> Iterator for Repeated<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        loop {
            match self.fields.next()? {
                Ok((n, Value::Len(bytes))) if n == self.number => return Some(bytes),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    }
}

/// An attribute value. Arrays and key-value lists iterate lazily over the request bytes.
#[derive(Debug, Clone)]
pub enum AnyValueView<'a> {
    /// No value set (an empty `AnyValue`, an absent value, or a profiles-only string reference).
    Empty,
    /// A string.
    String(&'a str),
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A double, bit for bit as sent.
    Double(f64),
    /// Raw bytes.
    Bytes(&'a [u8]),
    /// An array of values.
    Array(Values<'a>),
    /// A list of key-value pairs.
    KvList(Attributes<'a>),
}

impl<'a> AnyValueView<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        Some(match schema::any_value(bytes, ANY_DEPTH, no_child).ok()? {
            schema::AnyValue::Empty => Self::Empty,
            schema::AnyValue::String(s) => Self::String(s),
            schema::AnyValue::Bool(b) => Self::Bool(b),
            schema::AnyValue::Int(i) => Self::Int(i),
            schema::AnyValue::Double(d) => Self::Double(d),
            schema::AnyValue::Bytes(b) => Self::Bytes(b),
            schema::AnyValue::Array(a) => Self::Array(Values(Repeated::new(a, 1))),
            schema::AnyValue::KvList(k) => Self::KvList(Attributes(Repeated::new(k, 1))),
        })
    }
}

/// The elements of an array value.
#[derive(Debug, Clone)]
pub struct Values<'a>(Repeated<'a>);

impl<'a> Iterator for Values<'a> {
    type Item = AnyValueView<'a>;

    fn next(&mut self) -> Option<AnyValueView<'a>> {
        AnyValueView::new(self.0.next()?)
    }
}

/// Key-value pairs: the attributes of a span, event, link, resource or scope, or a key-value list
/// value. Keys and values borrow the request bytes.
#[derive(Debug, Clone)]
pub struct Attributes<'a>(Repeated<'a>);

impl<'a> Iterator for Attributes<'a> {
    type Item = (&'a str, AnyValueView<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let pair = schema::key_value(self.0.next()?, ANY_DEPTH).ok()?;
        let value = match pair.value {
            Some(bytes) => AnyValueView::new(bytes)?,
            None => AnyValueView::Empty,
        };
        Some((pair.key, value))
    }
}

/// The resource of a span.
#[derive(Debug, Clone, Copy)]
pub struct ResourceView<'a> {
    bytes: &'a [u8],
}

impl<'a> ResourceView<'a> {
    /// The resource attributes.
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes(Repeated::new(self.bytes, 1))
    }
}

/// The instrumentation scope of a span.
#[derive(Debug, Clone, Copy)]
pub struct ScopeView<'a> {
    bytes: &'a [u8],
    name: &'a str,
    version: &'a str,
}

impl<'a> ScopeView<'a> {
    fn new(bytes: &'a [u8]) -> Option<Self> {
        let scope = schema::scope(bytes, ANY_DEPTH, no_child).ok()?;
        Some(Self {
            bytes,
            name: scope.name,
            version: scope.version,
        })
    }

    /// The scope name; empty when unknown.
    pub const fn name(&self) -> &'a str {
        self.name
    }

    /// The scope version; empty when unknown.
    pub const fn version(&self) -> &'a str {
        self.version
    }

    /// The scope attributes.
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes(Repeated::new(self.bytes, 3))
    }
}

/// The status of a span.
#[derive(Debug, Clone, Copy)]
pub struct StatusView<'a> {
    code: StatusCode,
    message: &'a str,
}

impl<'a> StatusView<'a> {
    /// The status code; values outside the OTel enum read as `unset`.
    pub const fn code(&self) -> StatusCode {
        self.code
    }

    /// The status message.
    pub const fn message(&self) -> &'a str {
        self.message
    }
}

/// An event of a span.
#[derive(Debug, Clone, Copy)]
pub struct EventView<'a> {
    bytes: &'a [u8],
    time: Timestamp,
    name: &'a str,
}

impl<'a> EventView<'a> {
    /// The event time, truncated to microseconds (contract 1.2).
    pub const fn time(&self) -> Timestamp {
        self.time
    }

    /// The event name.
    pub const fn name(&self) -> &'a str {
        self.name
    }

    /// The event attributes.
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes(Repeated::new(self.bytes, 3))
    }
}

/// The events of a span, in wire order.
#[derive(Debug, Clone)]
pub struct Events<'a>(Repeated<'a>);

impl<'a> Iterator for Events<'a> {
    type Item = EventView<'a>;

    fn next(&mut self) -> Option<EventView<'a>> {
        let bytes = self.0.next()?;
        let event = schema::event(bytes, ANY_DEPTH, no_child).ok()?;
        Some(EventView {
            bytes,
            time: Timestamp::from_unix_nanos(event.time_unix_nano),
            name: event.name,
        })
    }
}

/// A link of a span.
#[derive(Debug, Clone, Copy)]
pub struct LinkView<'a> {
    bytes: &'a [u8],
    trace_id: TraceId,
    span_id: SpanId,
    trace_state: &'a str,
}

impl<'a> LinkView<'a> {
    /// The linked trace id; all zero when absent.
    pub const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// The linked span id; all zero when absent.
    pub const fn span_id(&self) -> SpanId {
        self.span_id
    }

    /// The W3C trace state of the link.
    pub const fn trace_state(&self) -> &'a str {
        self.trace_state
    }

    /// The link attributes.
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes(Repeated::new(self.bytes, 4))
    }
}

/// The links of a span, in wire order.
#[derive(Debug, Clone)]
pub struct Links<'a>(Repeated<'a>);

impl<'a> Iterator for Links<'a> {
    type Item = LinkView<'a>;

    fn next(&mut self) -> Option<LinkView<'a>> {
        let bytes = self.0.next()?;
        let link = schema::link(bytes, ANY_DEPTH, no_child).ok()?;
        Some(LinkView {
            bytes,
            trace_id: trace_id(link.trace_id),
            span_id: span_id(link.span_id),
            trace_state: link.trace_state,
        })
    }
}

/// One span of a validated request, with its resource and instrumentation scope.
#[derive(Debug, Clone, Copy)]
pub struct SpanView<'a> {
    bytes: &'a [u8],
    ordinal: u32,
    span: schema::Span<'a>,
    status: StatusView<'a>,
    resource: ResourceView<'a>,
    scope: ScopeView<'a>,
    rejection: Option<Rejection>,
}

impl<'a> SpanView<'a> {
    /// The position of the span in wire order across the whole request, from 0: the bit index in the
    /// WAL envelope's `rejected` bitmap.
    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    /// Why the span is rejected through partial success, or `None` when it is accepted.
    pub const fn rejection(&self) -> Option<Rejection> {
        self.rejection
    }

    /// The trace id; all zero when absent (and the span is then rejected).
    pub fn trace_id(&self) -> TraceId {
        trace_id(self.span.trace_id)
    }

    /// The span id; all zero when absent (and the span is then rejected).
    pub fn span_id(&self) -> SpanId {
        span_id(self.span.span_id)
    }

    /// The parent span id; `None` when absent or all zero, which means "no parent" (contract 1.1).
    pub fn parent_span_id(&self) -> Option<SpanId> {
        span_id(self.span.parent_span_id).non_zero()
    }

    /// The W3C trace state.
    pub const fn trace_state(&self) -> &'a str {
        self.span.trace_state
    }

    /// The span name.
    pub const fn name(&self) -> &'a str {
        self.span.name
    }

    /// The span kind; values outside the OTel enum read as `unspecified`.
    pub fn kind(&self) -> SpanKind {
        u8::try_from(self.span.kind)
            .ok()
            .and_then(SpanKind::from_value)
            .unwrap_or(SpanKind::Unspecified)
    }

    /// The start time, truncated to microseconds (contract 1.2). It is not clamped:
    /// [`Timestamp::clamp_valid`] applies the valid range and reports `clock_skew`.
    pub const fn start_time(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.span.start_time_unix_nano)
    }

    /// The end time, truncated to microseconds and not clamped, like [`SpanView::start_time`].
    pub const fn end_time(&self) -> Timestamp {
        Timestamp::from_unix_nanos(self.span.end_time_unix_nano)
    }

    /// The span status; `unset` with an empty message when absent.
    pub const fn status(&self) -> StatusView<'a> {
        self.status
    }

    /// The span attributes.
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes(Repeated::new(self.bytes, 9))
    }

    /// The span events.
    pub fn events(&self) -> Events<'a> {
        Events(Repeated::new(self.bytes, 11))
    }

    /// The span links.
    pub fn links(&self) -> Links<'a> {
        Links(Repeated::new(self.bytes, 13))
    }

    /// The resource the span belongs to.
    pub const fn resource(&self) -> ResourceView<'a> {
        self.resource
    }

    /// The instrumentation scope the span belongs to.
    pub const fn scope(&self) -> ScopeView<'a> {
        self.scope
    }

    /// The size of the span message in the canonical request, in bytes (`event_bytes`, contract 3.1).
    pub const fn encoded_len(&self) -> usize {
        self.bytes.len()
    }
}

/// Every span of a request in wire order: resource spans, then scope spans, then spans.
#[derive(Debug, Clone)]
pub struct Spans<'a> {
    resource_spans: Repeated<'a>,
    scope_spans: Repeated<'a>,
    spans: Repeated<'a>,
    resource: ResourceView<'a>,
    scope: ScopeView<'a>,
    ordinal: u32,
    min_start: Option<Timestamp>,
}

impl<'a> Spans<'a> {
    pub(crate) fn new(body: &'a [u8], min_start: Option<Timestamp>) -> Self {
        Self {
            resource_spans: Repeated::new(body, 1),
            scope_spans: Repeated::new(&[], 2),
            spans: Repeated::new(&[], 2),
            resource: ResourceView { bytes: &[] },
            scope: ScopeView {
                bytes: &[],
                name: "",
                version: "",
            },
            ordinal: 0,
            min_start,
        }
    }

    fn span(&self, bytes: &'a [u8]) -> Option<SpanView<'a>> {
        let span = schema::span(bytes, ANY_DEPTH, no_child).ok()?;
        let status = match span.status {
            Some(status) => schema::status(status, ANY_DEPTH).ok()?,
            None => schema::Status::default(),
        };
        let code = u8::try_from(status.code)
            .ok()
            .and_then(StatusCode::from_value)
            .unwrap_or(StatusCode::Unset);
        let start = Timestamp::from_unix_nanos(span.start_time_unix_nano);
        Some(SpanView {
            bytes,
            ordinal: self.ordinal,
            span,
            status: StatusView {
                code,
                message: status.message,
            },
            resource: self.resource,
            scope: self.scope,
            rejection: rejection(span.trace_id, span.span_id, start, self.min_start),
        })
    }
}

impl<'a> Iterator for Spans<'a> {
    type Item = SpanView<'a>;

    fn next(&mut self) -> Option<SpanView<'a>> {
        loop {
            if let Some(bytes) = self.spans.next() {
                let view = self.span(bytes)?;
                self.ordinal = self.ordinal.saturating_add(1);
                return Some(view);
            }
            if let Some(bytes) = self.scope_spans.next() {
                let fields = schema::scope_spans(bytes, ANY_DEPTH, no_child).ok()?;
                self.scope = ScopeView::new(fields.scope.unwrap_or_default())?;
                self.spans = Repeated::new(bytes, 2);
                continue;
            }
            let bytes = self.resource_spans.next()?;
            let fields = schema::resource_spans(bytes, ANY_DEPTH, no_child).ok()?;
            self.resource = ResourceView {
                bytes: fields.resource.unwrap_or_default(),
            };
            self.scope_spans = Repeated::new(bytes, 2);
        }
    }
}
