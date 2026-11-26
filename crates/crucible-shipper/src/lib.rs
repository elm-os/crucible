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
//! The task keeps the order-dependent work: reading the log, stage B, the rows and the WAL's records. While the log
//! holds durable frames past those the task works on, stage A, pure per frame (contract 3.2), runs on the frames
//! the task reads ahead, up to twice the stage A share of the CPU budget and `normalize.cache_bytes`, each as a
//! blocking job, and stage B takes the frames strictly in LSN order with the records their jobs made; a shipper
//! that keeps up runs stage A on its task. Each block of a sealed batch is encoded by a blocking job under the
//! encode share, and the bytes are kept until the pair resolves, so every retry sends them unchanged (contract
//! 5.2). A batch seals only once every batch before it is encoded, so besides the open batch only the batch being
//! encoded holds rows. While ingest is busy the task does both itself, and so does the simulation's, whose spawner
//! runs blocking jobs in place.
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
//! An `IngestRequest` frame whose spans carry `mcp.method.name` also yields the rows of the six MCP tables
//! (contract 3.5 to 3.7, and the hourly tier of 3.10). Stage B hashes a user subject with the salt of the frame's
//! project, which the shipper takes from the [`ProjectSalts`] hook before it admits the frame; a frame whose salt
//! cannot be had waits.
//!
//! Stage B drops the spans, MCP operations and facts that a deletion request selects (contract 13.3, step 3):
//! those of every `Tombstone` frame it has passed, and those of every request in every state that the
//! [`DeletionRequests`] hook gives before stage B admits a frame the request can select, so that a replay after a
//! restart never inserts what a request completed while the node was down deleted. The read is bounded by the
//! frames' arrival: a deletion selects data received before its `requested_at`. A `Tombstone` frame also evicts
//! what its deletion selects from the recent-span index. The drops are counted by kind
//! (`crucible_shipper_deletion_drops_total`).

mod batch;
mod config;
mod cpu;
mod error;
mod frames;
mod hooks;
mod limits;
mod pair;
mod phase;
mod probe;
mod quarantine;
mod recent_hook;
mod shipper;
pub mod tables;
pub mod value;
mod window;

pub use batch::{BatchId, OpenBatch, Pair, pair_block, pair_token};
pub use config::{
    DEFAULT_CACHE_BYTES, DEFAULT_ENCODED_BYTES, DEFAULT_SHIPPER_PERMITS, FrameFault, Mutant,
    POISON_LIMIT, PROBE_VALID_FOR, ShipperConfig,
};
pub use error::ShipError;
pub use hooks::{
    Barrier, Barriers, DeletionRequests, DeletionsUnavailable, IngestLoad, KnownDeletion,
    NoBarriers, NoDeletions, NoRecent, NoSalts, ProjectSalts, RecentIndex, SaltUnavailable,
};
pub use limits::SealLimits;
pub use quarantine::{
    FRAMES_FILE, FrameManifest, MANIFEST_FILE, frame_quarantine_ref, quarantine_ref,
};
pub use recent_hook::RecentHook;
pub use shipper::{Hooks, Shipper, ShipperStats, StatsSnapshot};
