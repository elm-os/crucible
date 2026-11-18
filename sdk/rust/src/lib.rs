//! Crucible SDK for Rust: sends traces from agents and MCP servers to a self-hosted Crucible over OTLP/HTTP.
//!
//! - [`init`], [`flush`], [`shutdown`] and [`stats`] run one process-wide exporter. Finished spans go on a bounded
//!   queue that a background thread exports in batches; a full queue drops and counts, and ending a span never
//!   waits for the network.
//! - The agent helpers ([`with_session`], [`with_user`], [`with_tags`], [`with_metadata`] and [`observe`]) record
//!   the steps of an agent with the span keys Crucible reads.
//! - The `mcp` feature adds [`mcp::CrucibleService`], a wrapper for MCP servers built on rmcp that records one
//!   span per operation in both protocol eras.
//! - The `mcp-client` feature adds [`mcp_client::TracedClient`], which records client calls and carries their
//!   trace context to the server in `params._meta`.
//!
//! The SDK uses its own OpenTelemetry tracer provider and never installs it globally, so an application's own
//! OpenTelemetry setup is left alone.

mod agent;
mod client;
mod export;
mod keys;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
mod scrub;

#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp-client")]
pub mod mcp_client;
#[cfg(any(feature = "mcp", feature = "mcp-client"))]
mod mcp_common;

pub use agent::{
    Cost, Observation, ObservationType, Usage, observe, with_metadata, with_session, with_tags,
    with_user,
};
pub use client::{FlushError, InitError, Options, Stats, flush, init, shutdown, stats};
/// The OpenTelemetry context type the agent helpers take and return.
pub use opentelemetry::Context;
/// Attribute values for [`with_metadata`] and [`Observation::set_attribute`].
pub use opentelemetry::{KeyValue, Value};
