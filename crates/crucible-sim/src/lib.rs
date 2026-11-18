//! Deterministic simulation harness, fault models, system models and invariants.
//!
//! - [`run`], [`Sim`] and [`Host`]: a single-threaded runtime with a paused virtual clock, a root
//!   ChaCha20 stream with labelled forks, and hosts that can be crashed at any await point.
//! - [`SimDisk`]: the `Disk` fault model of contract section 2.4.
//! - [`wal_sim`]: the WAL workload and the oracle for invariants I1, I12, I13 and I14.
//! - [`ch_model`]: the insert, deduplication and probe side of the simulated ClickHouse.
//! - [`ship_sim`]: the real shipper over the WAL and the store model, with crashes at its barriers, and
//!   the oracle for invariants I1, I2, I12 and I13.
//! - [`system_sim`]: the whole node of milestone 1 (ingest router, WAL, shipper, recent-span index, query
//!   service) over the disk and the store model, and the oracle for invariants I1, I2, I3, I6, I12 and I13.
//! - [`swarm`]: the coin-flip dimensions of the system simulation, its run records and the swarm report.
//!
//! The network (turmoil hosts) and the Postgres model join in a later wave and reuse these pieces.
//!
//! This crate implements the `Env` traits over tokio, so the determinism lints are switched off for it:
//! what makes a run deterministic here is the paused clock, the single thread and the seeded streams.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod boot;
pub mod ch_model;
mod disk;
mod runtime;
pub mod ship_sim;
pub mod swarm;
pub mod system_sim;
mod trace;
pub mod wal_sim;

pub use disk::{BLOCK, DiskFaults, SimDisk};
pub use runtime::{Host, SIM_EPOCH_MICROS, Sim, SimClock, SimSpawner, run};
pub use trace::Trace;
