//! The read path of the MCP analytics in simulation (invariant I7 per capture source): seeded traffic of every
//! capture source, landed by the shipper's row builders in the store model and read back through the real
//! query service, against a reference computed from the records.

// Test code throughout: lets the assertions use `panic!` under the workspace lints.
#![cfg(test)]

use crucible_sim::mcp_reads::{run_many, run_mcp_reads};

#[test]
fn every_read_matches_the_reference_per_capture_source() {
    let failed = run_many(0, 64);
    // The never-summed check means something only when calls were captured twice.
    let doubled: usize = (0..64)
        .map(|seed| run_mcp_reads(seed).double_captured)
        .sum();
    assert!(doubled > 64, "{doubled}");
    assert!(failed.is_empty(), "{failed:#?}");
}

#[test]
fn a_run_lands_traffic_and_compares_reads() {
    let outcome = run_mcp_reads(7);
    assert!(outcome.operations >= 20, "{outcome:?}");
    // Two sides: four choices of servers twice, and metrics and operations once.
    assert_eq!(outcome.reads, 2 * (8 + 4 + 4), "{outcome:?}");
    assert!(outcome.violations.is_empty(), "{outcome:?}");
    assert_eq!(run_mcp_reads(7), outcome, "a seed replays");
}
