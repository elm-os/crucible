//! The read services against an in-memory store and a real recent index (contract 8.2, 8.6 to 8.8, invariant
//! I3).

// Test code throughout: lets the helpers use `unwrap`, `expect` and `panic!` under the workspace lints.
#![cfg(test)]

mod export;
mod facts;
mod hidden;
mod list;
mod metrics;
mod payload;
mod properties;
mod search;
mod sessions;
mod span_list;
mod store;
mod trace;
mod world;
