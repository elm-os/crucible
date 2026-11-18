//! Single-pass validation of an `ExportTraceServiceRequest` (contract 4.8) and the partial-success
//! outcome (contracts 1.1, 1.2 and 4.8).

use core::fmt;

use crucible_records::Timestamp;

use crate::schema::{self, Msg};
use crate::view::Spans;
use crate::wire::{RECURSION_LIMIT, WireError, descend};
use crate::{MAX_BODY_BYTES, MAX_SPANS};

/// Why a request is refused as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ScanError {
    /// The canonical body is above [`MAX_BODY_BYTES`] (HTTP 413, gRPC `INVALID_ARGUMENT`).
    #[error("request body of {len} bytes is above the cap of {max} bytes", max = MAX_BODY_BYTES)]
    BodyTooLarge {
        /// The body length.
        len: usize,
    },
    /// The request carries more than [`MAX_SPANS`] spans (HTTP 413, gRPC `INVALID_ARGUMENT`).
    #[error("request carries more than {max} spans", max = MAX_SPANS)]
    TooManySpans,
    /// The body is not a well-formed `ExportTraceServiceRequest` (HTTP 400, gRPC `INVALID_ARGUMENT`).
    #[error("malformed OTLP protobuf: {0}")]
    Malformed(#[from] WireError),
}

/// Why one span is rejected through OTLP partial success while the rest of its request is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The trace id is absent or all zero (contract 1.1).
    MissingTraceId,
    /// The span id is absent or all zero (contract 1.1).
    MissingSpanId,
    /// The span starts before the cutoff passed to [`TraceRequest::scan`]: the project's retention
    /// horizon or `ingest.max_event_age` (contract 1.2).
    TooOld,
}

/// The partial-success outcome of a request: how many spans were rejected and why. `Display` renders
/// the developer-facing `error_message` of the OTLP response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartialSuccess {
    spans: u32,
    rejected: u32,
    missing_trace_id: u32,
    missing_span_id: u32,
    too_old: u32,
}

impl PartialSuccess {
    /// The `rejected_spans` count of the OTLP response.
    pub const fn rejected_spans(&self) -> u32 {
        self.rejected
    }

    /// Spans rejected for each reason, in the order of [`Rejection`]'s variants.
    pub const fn by_reason(&self) -> [(Rejection, u32); 3] {
        [
            (Rejection::MissingTraceId, self.missing_trace_id),
            (Rejection::MissingSpanId, self.missing_span_id),
            (Rejection::TooOld, self.too_old),
        ]
    }

    fn count(&mut self, rejection: Option<Rejection>) {
        self.spans = self.spans.saturating_add(1);
        let Some(rejection) = rejection else { return };
        self.rejected = self.rejected.saturating_add(1);
        let slot = match rejection {
            Rejection::MissingTraceId => &mut self.missing_trace_id,
            Rejection::MissingSpanId => &mut self.missing_span_id,
            Rejection::TooOld => &mut self.too_old,
        };
        *slot = slot.saturating_add(1);
    }
}

impl fmt::Display for PartialSuccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "rejected {} of {} spans", self.rejected, self.spans)?;
        let mut sep = ": ";
        for (rejection, n) in self.by_reason() {
            if n == 0 {
                continue;
            }
            let why = match rejection {
                Rejection::MissingTraceId => "with a missing or all-zero trace_id",
                Rejection::MissingSpanId => "with a missing or all-zero span_id",
                Rejection::TooOld => "older than the accepted event age",
            };
            write!(f, "{sep}{n} {why}")?;
            sep = ", ";
        }
        Ok(())
    }
}

/// The rejection rule for one span, from its final ids and start time.
pub(crate) fn rejection(
    trace_id: &[u8],
    span_id: &[u8],
    start: Timestamp,
    min_start: Option<Timestamp>,
) -> Option<Rejection> {
    let missing = |id: &[u8]| id.iter().all(|b| *b == 0);
    if missing(trace_id) {
        Some(Rejection::MissingTraceId)
    } else if missing(span_id) {
        Some(Rejection::MissingSpanId)
    } else if min_start.is_some_and(|min| start < min) {
        Some(Rejection::TooOld)
    } else {
        None
    }
}

/// A validated `ExportTraceServiceRequest`. Only [`TraceRequest::scan`] builds one, so the views it
/// hands out never meet malformed bytes.
#[derive(Debug, Clone, Copy)]
pub struct TraceRequest<'a> {
    body: &'a [u8],
    tally: PartialSuccess,
    min_start: Option<Timestamp>,
}

impl<'a> TraceRequest<'a> {
    /// Validates a decompressed canonical body in one pass, without allocating.
    ///
    /// The body must be at most [`MAX_BODY_BYTES`] and carry at most [`MAX_SPANS`] spans, and every
    /// field at every nesting level must be well formed: wire types, lengths, UTF-8 in string fields,
    /// 16-byte trace ids and 8-byte span ids (or empty ones). Unknown fields are skipped. A span with
    /// a missing or all-zero trace or span id, or one that starts before `min_start`, does not fail
    /// the request: it is rejected through partial success.
    ///
    /// Pre-ack ingest passes the age cutoff, or `None` for an import key. Post-ack readers pass `None`
    /// and skip the spans that the WAL envelope's `rejected` bitmap marks.
    pub fn scan(body: &'a [u8], min_start: Option<Timestamp>) -> Result<Self, ScanError> {
        if body.len() > MAX_BODY_BYTES {
            return Err(ScanError::BodyTooLarge { len: body.len() });
        }
        let mut validator = Validator {
            tally: PartialSuccess::default(),
            min_start,
        };
        schema::request(body, RECURSION_LIMIT, |msg, bytes| {
            validator.message(msg, bytes, RECURSION_LIMIT)
        })?;
        Ok(Self {
            body,
            tally: validator.tally,
            min_start,
        })
    }

    /// The number of spans in the request, rejected ones included.
    pub const fn span_count(&self) -> u32 {
        self.tally.spans
    }

    /// The partial-success outcome, or `None` when every span is accepted.
    pub const fn partial_success(&self) -> Option<PartialSuccess> {
        if self.tally.rejected == 0 {
            None
        } else {
            Some(self.tally)
        }
    }

    /// Every span in wire order, rejected ones included (see [`crate::SpanView::rejection`]).
    pub fn spans(&self) -> Spans<'a> {
        Spans::new(self.body, self.min_start)
    }
}

/// Walks every nested message, counting spans and their rejections.
struct Validator {
    tally: PartialSuccess,
    min_start: Option<Timestamp>,
}

impl Validator {
    /// Validates the nested message `bytes` of type `msg`, held by a field at nesting level `level`.
    fn message(&mut self, msg: Msg, bytes: &[u8], level: u32) -> Result<(), ScanError> {
        let inner = descend(level)?;
        match msg {
            Msg::ResourceSpans => {
                let fields =
                    schema::resource_spans(bytes, inner, |m, b| self.message(m, b, inner))?;
                if let Some(resource) = fields.resource {
                    self.message(Msg::Resource, resource, inner)?;
                }
            }
            Msg::Resource => schema::resource(bytes, inner, |m, b| self.message(m, b, inner))?,
            Msg::EntityRef => schema::entity_ref(bytes, inner)?,
            Msg::ScopeSpans => {
                let fields = schema::scope_spans(bytes, inner, |m, b| self.message(m, b, inner))?;
                if let Some(scope) = fields.scope {
                    self.message(Msg::Scope, scope, inner)?;
                }
            }
            Msg::Scope => {
                schema::scope(bytes, inner, |m, b| self.message(m, b, inner))?;
            }
            Msg::Span => {
                if self.tally.spans >= MAX_SPANS {
                    return Err(ScanError::TooManySpans);
                }
                let span = schema::span(bytes, inner, |m, b| self.message(m, b, inner))?;
                if let Some(status) = span.status {
                    self.message(Msg::Status, status, inner)?;
                }
                let start = Timestamp::from_unix_nanos(span.start_time_unix_nano);
                let rejected = rejection(span.trace_id, span.span_id, start, self.min_start);
                self.tally.count(rejected);
            }
            Msg::Event => {
                schema::event(bytes, inner, |m, b| self.message(m, b, inner))?;
            }
            Msg::Link => {
                schema::link(bytes, inner, |m, b| self.message(m, b, inner))?;
            }
            Msg::Status => {
                schema::status(bytes, inner)?;
            }
            Msg::KeyValue => {
                if let Some(value) = schema::key_value(bytes, inner)?.value {
                    self.message(Msg::AnyValue, value, inner)?;
                }
            }
            Msg::AnyValue => {
                schema::any_value(bytes, inner, |m, b| self.message(m, b, inner))?;
            }
            Msg::ArrayValue => {
                schema::list(Msg::AnyValue, bytes, inner, |m, b| {
                    self.message(m, b, inner)
                })?;
            }
            Msg::KeyValueList => {
                schema::list(Msg::KeyValue, bytes, inner, |m, b| {
                    self.message(m, b, inner)
                })?;
            }
        }
        Ok(())
    }
}
