//! The Crucible MCP proxy (contract 9.10, invariant I11).
//!
//! The proxy sits between an MCP client and an MCP server, forwards every byte unchanged and at once in both
//! directions, and reports what it saw as OTLP spans. A copy of the traffic goes through a bounded queue that
//! never blocks; when the queue or the collector cannot keep up, telemetry is dropped and counted, and the
//! session is not touched. The proxy never injects anything.
//!
//! - [`core`]: [`ProxyCore`], the state machine with no IO, and its forwarding face [`Tap`].
//! - [`scan`] and [`sse`]: the bounded streaming readers that find JSON-RPC fields in the copy.
//! - [`encode`]: operations to OTLP spans.
//! - [`live`]: the telemetry task and the exporter, off the forwarding path.
//! - [`stdio`]: the pumps of `crucible proxy stdio`.

pub mod core;
pub mod encode;
pub mod live;
pub mod net;
pub mod scan;
pub mod sse;
pub mod stdio;

pub use crate::core::{
    Batch, BodyKind, CoreConfig, EndReason, ExportResult, HeaderView, Identity, Now, Op, ProxyCore,
    Stats, StreamId, Tap,
};
