//! CSV export of a result grid (contract 12.6), with the spreadsheet formula guard: a text cell that starts with
//! `=`, `+`, `-` or `@` (or a tab or carriage return, which spreadsheets strip before they look) gets a leading
//! apostrophe, so a spreadsheet shows it as text instead of evaluating it. Cells of numeric columns are left
//! alone: a negative number must stay a number.

use crucible_env::{SqlCell, SqlGrid};

/// What the guard puts in front of a cell that a spreadsheet would evaluate.
pub const FORMULA_GUARD: char = '\'';

/// Whether a ClickHouse type holds numbers only, under any `Nullable` or `LowCardinality` wrapper.
fn is_numeric(ty: &str) -> bool {
    let mut ty = ty.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("]
            .iter()
            .find_map(|wrapper| ty.strip_prefix(wrapper)?.strip_suffix(')'));
        match inner {
            Some(inner) => ty = inner.trim(),
            None => break,
        }
    }
    ["Int", "UInt", "Float", "Decimal"].iter().any(|family| {
        ty.strip_prefix(family).is_some_and(|rest| {
            rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_digit() || c == '(')
        })
    })
}

/// The text of a cell: strings as they are, null as nothing, anything else as JSON.
fn cell_text(cell: &SqlCell) -> String {
    match cell {
        SqlCell::Null => String::new(),
        SqlCell::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn guard(text: String, numeric: bool) -> String {
    let risky = text.starts_with(['=', '+', '-', '@', '\t', '\r']);
    if risky && !numeric {
        format!("{FORMULA_GUARD}{text}")
    } else {
        text
    }
}

fn field(out: &mut String, text: &str) {
    if text.contains(['"', ',', '\n', '\r']) || text.starts_with(' ') || text.ends_with(' ') {
        out.push('"');
        out.push_str(&text.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(text);
    }
}

/// The grid as RFC 4180 CSV with a header row and CRLF line ends. With `formula_guard`, text cells and header
/// names that a spreadsheet would evaluate are prefixed.
pub fn to_csv(grid: &SqlGrid, formula_guard: bool) -> String {
    let mut out = String::new();
    for (n, column) in grid.columns.iter().enumerate() {
        if n > 0 {
            out.push(',');
        }
        let name = if formula_guard {
            guard(column.name.clone(), false)
        } else {
            column.name.clone()
        };
        field(&mut out, &name);
    }
    out.push_str("\r\n");
    let numeric: Vec<bool> = grid.columns.iter().map(|c| is_numeric(&c.ty)).collect();
    for row in &grid.rows {
        for (n, cell) in row.iter().enumerate() {
            if n > 0 {
                out.push(',');
            }
            let text = cell_text(cell);
            let text = if formula_guard {
                guard(text, numeric.get(n).copied().unwrap_or(false))
            } else {
                text
            };
            field(&mut out, &text);
        }
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use crucible_env::{SqlColumn, SqlStats};
    use proptest::prelude::*;

    use super::*;

    fn grid(columns: &[(&str, &str)], rows: Vec<Vec<SqlCell>>) -> SqlGrid {
        SqlGrid {
            columns: columns
                .iter()
                .map(|(name, ty)| SqlColumn {
                    name: (*name).to_owned(),
                    ty: (*ty).to_owned(),
                })
                .collect(),
            rows,
            truncated: false,
            cut_cells: 0,
            stats: SqlStats::default(),
        }
    }

    fn text(value: &str) -> SqlCell {
        SqlCell::String(value.to_owned())
    }

    #[test]
    fn formulas_are_prefixed_and_numbers_are_not() {
        let grid = grid(
            &[
                ("name", "String"),
                ("delta", "Int64"),
                ("ratio", "Nullable(Float64)"),
                ("note", "LowCardinality(String)"),
            ],
            vec![
                vec![
                    text("=HYPERLINK(\"http://x\")"),
                    text("-5"),
                    SqlCell::from(-0.5),
                    text("+1"),
                ],
                vec![text("@SUM(A1)"), text("7"), SqlCell::Null, text("-x")],
                vec![
                    text("\t=1"),
                    text("0"),
                    SqlCell::from(1.5),
                    text("plain, text"),
                ],
            ],
        );
        assert_eq!(
            to_csv(&grid, true),
            "name,delta,ratio,note\r\n\
             \"'=HYPERLINK(\"\"http://x\"\")\",-5,-0.5,'+1\r\n\
             '@SUM(A1),7,,'-x\r\n\
             '\t=1,0,1.5,\"plain, text\"\r\n"
        );
        // Without the guard nothing is prefixed.
        assert!(to_csv(&grid, false).contains("\r\n@SUM(A1),7,,-x\r\n"));
    }

    #[test]
    fn numeric_types_are_recognised_under_wrappers() {
        for ty in [
            "Int64",
            "UInt8",
            "Float64",
            "Decimal(18, 4)",
            "Nullable(Int32)",
            "LowCardinality(Nullable(UInt16))",
            "Decimal64(2)",
        ] {
            assert!(is_numeric(ty), "{ty}");
        }
        for ty in [
            "String",
            "DateTime64(6)",
            "UUID",
            "Array(Int64)",
            "Interval",
            "IntervalDay",
            "Nullable(String)",
            "Map(String, Int64)",
        ] {
            assert!(!is_numeric(ty), "{ty}");
        }
    }

    #[test]
    fn nested_values_are_json_and_quotes_are_doubled() {
        let grid = grid(
            &[("=cmd", "Array(String)")],
            vec![
                vec![serde_json::json!(["a", "b\"c"])],
                vec![text("line\nbreak")],
            ],
        );
        assert_eq!(
            to_csv(&grid, true),
            "'=cmd\r\n\"[\"\"a\"\",\"\"b\\\"\"c\"\"]\"\r\n\"line\nbreak\"\r\n"
        );
    }

    proptest! {
        /// With the guard on, no text cell reaches a spreadsheet starting with a formula character.
        #[test]
        fn no_guarded_text_cell_starts_a_formula(cells in prop::collection::vec("[=+\\-@\t\r a-z\",\n]{0,12}", 1..20)) {
            let grid = grid(&[("c", "String")], cells.iter().map(|c| vec![text(c)]).collect());
            let csv = to_csv(&grid, true);
            // Parse the single-column CSV back: one record per CRLF outside quotes.
            let mut records = Vec::new();
            let (mut current, mut quoted) = (String::new(), false);
            let mut chars = csv.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '"' if quoted && chars.peek() == Some(&'"') => { chars.next(); current.push('"'); }
                    '"' => quoted = !quoted,
                    '\r' if !quoted && chars.peek() == Some(&'\n') => { chars.next(); records.push(std::mem::take(&mut current)); }
                    other => current.push(other),
                }
            }
            prop_assert_eq!(records.len(), cells.len() + 1);
            for (record, cell) in records.iter().skip(1).zip(&cells) {
                prop_assert!(!record.starts_with(['=', '+', '-', '@', '\t', '\r']), "{record:?}");
                prop_assert_eq!(record.trim_start_matches(FORMULA_GUARD), cell.trim_start_matches(FORMULA_GUARD));
            }
        }
    }
}
