//! Fuzzes the lexical check of a query text with arbitrary UTF-8.
//!
//! The check must never panic. A text it accepts starts with `SELECT` or `WITH` after whitespace and comments,
//! names both window parameters, and lists every parameter once. An error points at a line of the text.
#![no_main]

use crucible_sqlws::lexer::{MAX_SQL_BYTES, check};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match check(text) {
        Ok(checked) => {
            assert!(text.len() <= MAX_SQL_BYTES);
            for name in ["from", "to"] {
                assert!(checked.params.iter().any(|param| param.name == name));
            }
            for (n, param) in checked.params.iter().enumerate() {
                assert!(!checked.params[..n].iter().any(|earlier| earlier.name == param.name));
                assert!(text.contains(&param.name) && text.contains(&param.ty));
            }
        }
        Err(error) => {
            let lines = text.matches('\n').count() + 1;
            assert!(error.line >= 1 && error.line as usize <= lines);
            assert!(error.column >= 1);
            assert!(!error.kind.fix().is_empty());
        }
    }
});
