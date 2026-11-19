//! The control-store conformance scripts against the simulation's `PgModel`, on the virtual clock.
//!
//! The transcripts are pinned as snapshots. The differential (`crucible-pg-conformance`, the
//! `pg-conformance` workflow) requires real Postgres 15 and 18 to produce the same transcripts, so these
//! snapshots are also what the production adapter answers.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use crucible_conformance::control::{ACQUIRE_TIMEOUT, Ctl, MAIN_POOL, SCRIPTS, run_script};
use crucible_sim::pg_model::{PgFaults, PgModel};

fn transcript(name: &str) -> String {
    let (_, script) = SCRIPTS
        .iter()
        .find(|(script, _)| *script == name)
        .expect("a known script");
    crucible_sim::run(7, |sim| async move {
        let host = sim.host("postgres", &sim.disk("postgres"), 0);
        let faults = PgFaults {
            main_pool: MAIN_POOL,
            acquire_timeout: ACQUIRE_TIMEOUT,
            ..PgFaults::default()
        };
        let model = PgModel::new(host.clock.clone(), sim.rng("pg").as_ref(), faults);
        let ctl = Ctl {
            store: Arc::new(model),
            clock: host.clock.clone(),
        };
        run_script(&ctl, *script)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error:#}"))
            .text()
    })
}

#[test]
fn repositories() {
    insta::assert_snapshot!(transcript("repositories"));
}

#[test]
fn transactions() {
    insta::assert_snapshot!(transcript("transactions"));
}

#[test]
fn key_feed() {
    insta::assert_snapshot!(transcript("key_feed"));
}

#[test]
fn leases() {
    insta::assert_snapshot!(transcript("leases"));
}

#[test]
fn claims() {
    insta::assert_snapshot!(transcript("claims"));
}

#[test]
fn identity() {
    insta::assert_snapshot!(transcript("identity"));
}

#[test]
fn pools() {
    insta::assert_snapshot!(transcript("pools"));
}

#[test]
fn jobs() {
    insta::assert_snapshot!(transcript("jobs"));
}

#[test]
fn alerts() {
    insta::assert_snapshot!(transcript("alerts"));
}

#[test]
fn lifecycle() {
    insta::assert_snapshot!(transcript("lifecycle"));
}

#[test]
fn dashboards() {
    insta::assert_snapshot!(transcript("dashboards"));
}

#[test]
fn every_script_has_a_test() {
    assert_eq!(SCRIPTS.len(), 12);
}

#[test]
fn exports() {
    insta::assert_snapshot!(transcript("exports"));
}
