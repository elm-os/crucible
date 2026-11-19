//! Deterministic simulation harness, fault models, system models and invariants.
//!
//! - [`run`], [`Sim`] and [`Host`]: a single-threaded runtime with a paused virtual clock, a root
//!   ChaCha20 stream with labelled forks, and hosts that can be crashed at any await point.
//! - [`SimDisk`]: the `Disk` fault model of contract section 2.4.
//! - [`wal_sim`]: the WAL workload and the oracle for invariants I1, I12, I13 and I14.
//! - [`ch_model`]: the simulated ClickHouse: inserts, deduplication, the probe, the typed-plan interpreter,
//!   merges, deletes and partition drops.
//! - [`ship_sim`]: the real shipper over the WAL and the store model, with crashes at its barriers, and
//!   the oracle for invariants I1, I2, I12 and I13.
//! - [`system_sim`]: the whole node of milestone 1 (ingest router, WAL, shipper, recent-span index, query
//!   service) over the disk and the store model, and the oracle for invariants I1, I2, I3, I6, I12 and I13.
//! - [`swarm`]: the coin-flip dimensions of the system simulation, its run records and the swarm report.
//!
//! - [`pg_model`]: the simulated Postgres behind `ControlStore`: snapshot transactions, constraints, leases,
//!   claims, the key feed and its fault model.
//! - [`lease_sim`]: leader election with leases over `pg_model` under its faults, and the oracle for the
//!   fencing rules of contract 11.7.
//! - [`jobs_sim`]: the job runners of `crucible-jobs` over `pg_model`: crashes, freezes, clock skew, lost
//!   connections and lease steals, and the oracle for the job invariants J1 to J3.
//! - [`alerts_sim`]: alerting end to end over `pg_model` and rollup rows in `ch_model`: crashes, downtime,
//!   clock skips, store faults and receivers that are down, and the oracle for the alert invariants A1 to A3.
//!
//! The network (turmoil hosts) joins in a later wave and reuses these pieces.
//!
//! This crate implements the `Env` traits over tokio, so the determinism lints are switched off for it:
//! what makes a run deterministic here is the paused clock, the single thread and the seeded streams.

#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod alerts_sim;
mod boot;
pub mod ch_model;
mod ch_read;
mod disk;
pub mod jobs_sim;
pub mod lease_sim;
pub mod pg_model;
mod runtime;
pub mod ship_sim;
pub mod sql_stub;
pub mod swarm;
mod system_oracle;
pub mod system_sim;
mod trace;
pub mod wal_sim;

pub use disk::{BLOCK, DiskFaults, SimDisk};
pub use runtime::{Host, SIM_EPOCH_MICROS, Sim, SimClock, SimSpawner, run};
pub use trace::Trace;
