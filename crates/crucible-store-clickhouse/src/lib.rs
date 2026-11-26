//! The `TelemetryStore` over the ClickHouse HTTP interface (contract 2.6).
//!
//! - [`ClickHouseStore`] implements `crucible_env::TelemetryStore` with one client per service identity
//!   (contract 7.1). Every connection goes through `Env::net`, every task through the `Env` spawner, every
//!   deadline through the `Env` clock, and every statement carries a query id minted by `Env::new_id`.
//! - [`render`] turns the typed forms of `crucible-plan` into SQL with server-side parameters only
//!   (contract 8.3). It is pure, so it is tested without a server.
//! - [`ClickHouseSqlStore`] implements `crucible_env::SqlWorkspaceStore`: user-written SQL as a project's own
//!   ClickHouse identity (contracts 2.9 and 7.4).
//! - [`codes`] is the error-code table of contract 5.6 as data.
//!
//! This is the only crate that names `clickhouse::Client`. The live checks against real servers are in
//! `crucible-conformance`.

pub mod codes;
mod connect;
mod decode;
#[cfg(feature = "test-support")]
mod raw;
pub mod render;
mod sqlws;
mod store;

#[cfg(feature = "test-support")]
pub use raw::{RawRead, RawReply};
pub use sqlws::{ClickHouseSqlStore, SqlPasswords, SqlStoreConfig, classify_sql, parse_grid};
pub use store::{
    ClickHouseStore, ClusterFacts, Credentials, QUERY_SETTINGS, ReplicaHoldings, StoreConfig,
    TABLE_SETTINGS, TEXT_SEARCH_FUNCTIONS,
};
