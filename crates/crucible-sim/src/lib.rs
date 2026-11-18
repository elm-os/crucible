//! Deterministic simulation harness, fault models, system models and invariants.
//!
//! This wave holds the foundations and the WAL suite:
//!
//! - [`run`], [`Sim`] and [`Host`]: a single-threaded runtime with a paused virtual clock, a root
//!   ChaCha20 stream with labelled forks, and hosts that can be crashed at any await point.
//! - [`SimDisk`]: the `Disk` fault model of contract section 2.4.
//! - [`wal_sim`]: the WAL workload and the oracle for invariants I1, I12, I13 and I14.
//!
//! The network (turmoil hosts), the ClickHouse and Postgres models and the ingest and shipper invariants
//! join in a later wave and reuse these pieces.
//!
//! This crate implements the `Env` traits over tokio, so the determinism lints are switched off for it:
//! what makes a run deterministic here is the paused clock, the single thread and the seeded streams.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod boot;
pub mod ch_model;
mod disk;
mod runtime;
pub mod ship_sim;
mod trace;
pub mod wal_sim;

pub use disk::{BLOCK, DiskFaults, SimDisk};
pub use runtime::{Host, SIM_EPOCH_MICROS, Sim, SimClock, SimSpawner, run};
pub use trace::Trace;
