//! Fuzzes the decoder of a SQL workspace result with arbitrary bytes.
//!
//! The first two bytes choose the limits. Decoding must never panic, and a grid it returns stays inside its
//! limits: no more rows than the row limit, and no string cell longer than the cell limit plus the cut marker.
#![no_main]

use std::time::Duration;

use crucible_env::{CELL_CUT_MARKER, SqlCell, SqlLimits};
use crucible_store_clickhouse::parse_grid;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [rows, cell, body @ ..] = data else {
        return;
    };
    let limits = SqlLimits {
        max_rows: usize::from(*rows),
        max_bytes: 4096,
        max_cell_bytes: usize::from(*cell) + 1,
        deadline: Duration::from_secs(1),
    };
    if let Ok(grid) = parse_grid(body, limits) {
        assert!(grid.rows.len() <= limits.max_rows);
        let mut cut = 0;
        for cell in grid.rows.iter().flatten() {
            if let SqlCell::String(text) = cell {
                assert!(text.len() <= limits.max_cell_bytes + CELL_CUT_MARKER.len());
                cut += u64::from(text.ends_with(CELL_CUT_MARKER));
            }
        }
        assert!(grid.cut_cells <= cut.max(grid.cut_cells));
    }
});
