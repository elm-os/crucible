//! The shipper (contract section 5): durable WAL frames become exactly-once ClickHouse inserts.
//!
//! One task, [`Shipper::run`], tails the durable log through `Wal::replay`, runs the normalizer (stage A, then
//! stage B in LSN order), and fills per-(table, partition) row builders. A batch seals at the linger, row or byte
//! threshold of contract 5.1; its `BatchSeal` frame is durable before any insert is sent. Each pair is then one
//! Native block and one insert whose deduplication token is the batch identity plus the pair, sent through
//! `Stores::telemetry` with the explicit settings of contract 5.4. Outcomes are handled per contract 5.6 under
//! the probe rules of 5.5, including the two the ingest model found (`docs/testing/formal-model.md`): once an
//! attempt of a pair is in doubt every later send is preceded by a probe, and a pair in doubt is quarantined only
//! after a probe read absent. A `Checkpoint` frame follows once a batch and every earlier batch are resolved, and
//! garbage collection of the WAL runs from there.
//!
//! After a restart, the sealed batches that no checkpoint covers are shipped again with their recorded boundaries
//! and identical tokens, every pair probed first; pairs named by a `Quarantine` frame stay resolved; the unsealed
//! tail is sealed with fresh boundaries.
//!
//! A `FactBatch` frame yields one row per fact in its fact table. A durable frame that this binary cannot decode
//! or normalize does not stop shipping: it is copied to the quarantine root by itself, alerted and counted, and
//! then passes as a frame without rows (contract 5.6, frame quarantine).
//!
//! The row builders ([`value`]) are generic over `TableDef`: a table needs one function from its record to a row
//! of cells ([`tables`] has those of the span tables, the span rollups, the MCP tables and the fact tables).
//!
//! An `IngestRequest` frame whose spans carry `mcp.method.name` also yields the rows of the five MCP tables
//! (contract 3.5 to 3.7). Stage B hashes a user subject with the salt of the frame's project, which the shipper
//! takes from the [`ProjectSalts`] hook before it admits the frame; a frame whose salt cannot be had waits.

mod batch;
mod config;
mod error;
mod frames;
mod hooks;
mod limits;
mod pair;
mod probe;
mod quarantine;
mod recent_hook;
mod shipper;
pub mod tables;
pub mod value;

pub use batch::{BatchId, OpenBatch, Pair, pair_block, pair_token};
pub use config::{
    DEFAULT_ENCODED_BYTES, FrameFault, Mutant, POISON_LIMIT, PROBE_VALID_FOR, ShipperConfig,
};
pub use error::ShipError;
pub use hooks::{
    Barrier, Barriers, NoBarriers, NoRecent, NoSalts, ProjectSalts, RecentIndex, SaltUnavailable,
};
pub use limits::SealLimits;
pub use quarantine::{
    FRAMES_FILE, FrameManifest, MANIFEST_FILE, frame_quarantine_ref, quarantine_ref,
};
pub use recent_hook::RecentHook;
pub use shipper::{Hooks, Shipper, ShipperStats, StatsSnapshot};
