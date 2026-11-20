//! The read path of the MCP analytics in simulation (invariant I7 per capture source): seeded traffic of every
//! capture source, landed by the shipper's row builders in the store model and read back through the real
//! query service, against a reference computed from the records; and an `mcp.error_rate` alert rule through one
//! evaluation by the production evaluator.

// Test code throughout: lets the assertions use `panic!` under the workspace lints.
#![cfg(test)]

use crucible_sim::mcp_reads::{run_many, run_mcp_alert, run_mcp_reads};

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
    // Three windows; two sides: four choices of servers twice, and metrics, the three totals of an alert and
    // operations once.
    assert_eq!(outcome.reads, 3 * 2 * (8 + 4 + 12 + 4), "{outcome:?}");
    assert!(outcome.violations.is_empty(), "{outcome:?}");
    assert_eq!(run_mcp_reads(7), outcome, "a seed replays");
}

#[test]
fn an_mcp_error_rate_rule_fires_for_the_server_above_its_threshold_only() {
    for seed in 0..16 {
        let outcome = run_mcp_alert(seed);
        assert!(outcome.violations.is_empty(), "{outcome:#?}");
        assert_eq!(outcome.firing, [r#"["alpha"]"#], "{outcome:#?}");
    }
    assert_eq!(run_mcp_alert(3), run_mcp_alert(3), "a seed replays");
}
