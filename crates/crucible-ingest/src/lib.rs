//! OTLP/HTTP trace ingest in front of the WAL (contract sections 4.3 to 4.8, 11.3 and 12.1).
//!
//! [`Ingest::router`] serves `POST /v1/traces` with protobuf or JSON bodies, optionally gzip-compressed. A
//! request goes through, in order:
//!
//! 1. Authentication: a secret key as `Authorization: Bearer` (401).
//! 2. Header checks: the media type and the content coding (415) and the declared length (413).
//! 3. Admission (contract 4.8): the request's bytes are reserved in the `ingest.inflight_bytes` budget and the
//!    decision table runs over the WAL's signals, giving 429 or 503 with `Retry-After`. The reservation grows
//!    while the body is read, inflated (under the canonical body cap) and transcoded; when the budget cannot
//!    hold it, the request is refused the same way.
//! 4. Scan (contract 4.8): the canonical body is validated in one pass, and spans with a missing or all-zero id
//!    or older than the age cutoff are rejected through OTLP partial success (400 or 413 refuse the request).
//! 5. The `IngestRequest` payload: the envelope of contract 4.4 (with the rejected spans in its bitmap, by
//!    ordinal) and the canonical body, prepared as a frame off the async threads.
//! 6. [`crucible_wal::Wal::append`]. Only after it resolves, which under `wal.durability = group` means the
//!    frame is durable, does the client get 200 with an `ExportTraceServiceResponse` (contract 4.5).
//!
//! Facts take the same path without HTTP: [`Ingest::append_facts`] validates a write, admits it under the same
//! budget and decision table, appends it as `FactBatch` frames and returns once they are durable (contract 3.8).
//! The routes of contract 12.6 call it.
//!
//! Responses match the request's format: protobuf or JSON. Refusals carry a `google.rpc.Status`, the body
//! OTLP/HTTP specifies for every 4xx and 5xx.

mod admission;
mod body;
mod facts;
mod reply;
mod service;

pub use crucible_admission::BucketConfig;
pub use facts::{AcceptedFact, FactAuthor, FactWriteError, NewFact};
pub use service::{DEFAULT_INFLIGHT_BYTES, DEFAULT_MAX_EVENT_AGE, Ingest, IngestConfig, Mutant};
