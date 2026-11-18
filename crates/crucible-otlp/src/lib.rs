//! Zero-copy OTLP protobuf scanner, span views and JSON transcoding (contracts 1.1, 1.2, 4.3 and 4.8).
//!
//! [`TraceRequest::scan`] validates one decompressed `ExportTraceServiceRequest` in a single pass
//! without allocating: the canonical body cap, the span cap, and the protobuf wire format of every
//! field at every nesting level. A span with a missing or all-zero trace or span id, or one that starts
//! before a caller-supplied cutoff, is rejected through OTLP partial success instead of failing the
//! request. The views a scanned request hands out borrow its bytes: walking spans, attributes, events
//! and links yields borrowed strings and typed values and never allocates.
//!
//! [`transcode_json`] turns an OTLP/JSON body into canonical protobuf, so the scanner and the WAL
//! handle a single format.
//!
//! This crate is pure: no `Env`, no IO. Bodies arrive already decompressed.

mod json;
mod scan;
mod schema;
mod view;
mod wire;

pub use json::{JsonError, transcode_json};
pub use scan::{PartialSuccess, Rejection, ScanError, TraceRequest};
pub use view::{
    AnyValueView, Attributes, EventView, Events, LinkView, Links, ResourceView, ScopeView,
    SpanView, Spans, StatusView, Values,
};
pub use wire::WireError;

/// The largest canonical OTLP body of one request: 64 MiB minus a 64 KiB envelope reserve, so that an
/// `IngestRequest` WAL payload always fits the 64 MiB frame cap uncompressed (contract 4.3).
pub const MAX_BODY_BYTES: usize = 67_043_328;

/// The most spans one request may carry (contract 4.8).
pub const MAX_SPANS: u32 = 50_000;
