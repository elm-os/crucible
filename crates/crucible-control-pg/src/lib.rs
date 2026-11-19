//! The Postgres adapter of the control plane (contracts 2.7 and 11): [`PgControlStore`] implements
//! `crucible_control::ControlStore`.
//!
//! - Connections are tokio-postgres over `env.net` (`connect_raw`), their protocol tasks run on `env.spawner`,
//!   and every deadline is on `env.clock`, so the adapter opens no socket and reads no clock of its own.
//! - Two pools: a main pool and a reserved pool of two for the key feed, leases, session lookups and the
//!   database clock.
//! - No session state: statements are sent with their parameter types and are never prepared by name, there is
//!   no LISTEN and no session advisory lock, so transaction-mode poolers work. The statement timeout and the
//!   idle-in-transaction timeout are 10 s.
//! - Transactions run at repeatable read (snapshot isolation) and must commit within 10 s.
//! - [`migrate`] applies the forward-only migrations in `migrations/` and records them in the ledger.
//!
//! The Postgres client is allowed in this crate only (contract 1.5).

mod conn;
mod exports;
mod identity;
mod lifecycle;
mod migrate;
mod sql;
mod store;

pub use conn::{PgConfig, RESERVED_POOL};
pub use migrate::migrate;
pub use store::{PgControlStore, PgTx};
