//! The shared scrubbing rules (contract 9.10), applied to the error messages and the captured content the proxy
//! records before they leave the process.
//!
//! The rules are data: `scrub-rules.json` in this crate is a copy of the repository's
//! `conformance/scrub-rules.json`, which every Crucible SDK applies as well, and a test keeps the two identical.
//! Rules run in file order, each replacing every non-overlapping match, leftmost first. After a match its
//! validator rejects, the search resumes after the match's first character that is neither an ASCII letter nor a
//! digit, or after the match when it has none. The patterns stay in a subset that the `regex` crate matches in
//! time linear in the input, so a text is scrubbed in time linear in its length.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const RULES_JSON: &str = include_str!("scrub-rules.json");

/// How far past a cap the text is scrubbed, so that a secret which starts before the cap and ends after it is
/// still recognised whole and its visible prefix replaced.
const LOOKAHEAD: usize = 1024;

/// What every text becomes when the rules cannot load: scrubbing fails closed.
const FAIL_CLOSED: &str = "[redacted]";

#[derive(Debug)]
struct Rule {
    pattern: Regex,
    replacement: String,
    luhn: bool,
}

static RULES: LazyLock<Option<Vec<Rule>>> = LazyLock::new(|| parse(RULES_JSON));

fn parse(json: &str) -> Option<Vec<Rule>> {
    let document: Value = serde_json::from_str(json).ok()?;
    document
        .get("rules")?
        .as_array()?
        .iter()
        .map(|rule| {
            let text = |key: &str| rule.get(key).and_then(Value::as_str);
            let luhn = match text("validator") {
                None => false,
                Some("luhn") => true,
                Some(_) => return None,
            };
            Some(Rule {
                pattern: Regex::new(text("pattern")?).ok()?,
                replacement: text("replacement")?.to_owned(),
                luhn,
            })
        })
        .collect()
}

impl Rule {
    /// `text` with every accepted match replaced.
    fn apply(&self, text: &str) -> String {
        let mut out = String::new();
        let mut copied = 0;
        let mut at = 0;
        while at <= text.len() {
            let Some(found) = self.pattern.find_at(text, at) else {
                break;
            };
            if self.luhn && !luhn(found.as_str()) {
                at = found
                    .as_str()
                    .char_indices()
                    .find(|(_, c)| !c.is_ascii_alphanumeric())
                    .map_or(found.end(), |(i, c)| found.start() + i + c.len_utf8());
                continue;
            }
            out.push_str(&text[copied..found.start()]);
            out.push_str(&self.replacement);
            copied = found.end();
            // An empty match would not advance; step over one character.
            at = if found.is_empty() {
                text[found.end()..]
                    .chars()
                    .next()
                    .map_or(text.len() + 1, |c| found.end() + c.len_utf8())
            } else {
                found.end()
            };
        }
        out.push_str(&text[copied..]);
        out
    }
}

/// Whether the digits of `text` pass the Luhn checksum; other characters are skipped.
fn luhn(text: &str) -> bool {
    let mut sum = 0;
    let mut count = 0;
    for digit in text.bytes().rev().filter(u8::is_ascii_digit) {
        let mut d = u32::from(digit - b'0');
        if count % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
        count += 1;
    }
    count > 0 && sum % 10 == 0
}

/// `text` with every rule applied in file order.
pub fn scrub(text: &str) -> String {
    match &*RULES {
        Some(rules) => rules
            .iter()
            .fold(text.to_owned(), |text, rule| rule.apply(&text)),
        None => FAIL_CLOSED.to_owned(),
    }
}

/// The longest prefix of `text` of at most `max` bytes that ends on a character boundary.
fn prefix(text: &str, max: usize) -> &str {
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// At most `max` bytes of `text`, scrubbed. Only the first `max` plus 1 KiB bytes are read, so the cost is
/// bounded by the cap and not by the size of `text`.
pub fn capped(text: &str, max: usize) -> String {
    capped_with_cut(text, max).0
}

/// [`capped`], and whether anything of `text` was left out: past the window that is read, or past the cap after
/// scrubbing.
pub fn capped_with_cut(text: &str, max: usize) -> (String, bool) {
    let window = prefix(text, max.saturating_add(LOOKAHEAD));
    let scrubbed = scrub(window);
    let kept = prefix(&scrubbed, max);
    (
        kept.to_owned(),
        window.len() < text.len() || kept.len() < scrubbed.len(),
    )
}

/// The size of the window [`capped`] reads for the cap `max`: what the scanner keeps of a message.
pub const fn window(max: usize) -> usize {
    max.saturating_add(LOOKAHEAD)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize)]
    struct Vectors {
        rules: Vec<RuleVectors>,
        examples: Vec<Example>,
    }

    #[derive(Deserialize)]
    struct RuleVectors {
        name: String,
        positive: Vec<String>,
        negative: Vec<String>,
    }

    #[derive(Deserialize)]
    struct Example {
        input: String,
        output: String,
    }

    #[test]
    fn the_copy_equals_the_shared_rules_file() {
        let shared = include_str!("../../../conformance/scrub-rules.json");
        assert_eq!(
            RULES_JSON, shared,
            "src/scrub-rules.json differs from conformance/scrub-rules.json; copy the shared file over it"
        );
    }

    #[test]
    fn every_vector_and_example_holds() {
        let rules = parse(RULES_JSON).unwrap();
        let vectors: Vectors = serde_json::from_str(RULES_JSON).unwrap();
        assert_eq!(rules.len(), vectors.rules.len());
        for (rule, v) in rules.iter().zip(&vectors.rules) {
            for input in &v.positive {
                assert_ne!(
                    &rule.apply(input),
                    input,
                    "rule {} leaves {input:?} unchanged",
                    v.name
                );
            }
            for input in &v.negative {
                assert_eq!(
                    &rule.apply(input),
                    input,
                    "rule {} changes {input:?}",
                    v.name
                );
            }
        }
        for example in &vectors.examples {
            assert_eq!(scrub(&example.input), example.output);
        }
    }

    #[test]
    fn a_cap_never_leaves_part_of_a_secret() {
        let key = "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo";
        for before in 50..=100 {
            let text = format!("{}{key} tail", "a".repeat(before));
            let out = capped(&text, 100);
            assert!(!out.contains("csk_"), "{out:?}");
            assert!(out.len() <= 100);
        }
        // A cut falls on a character boundary.
        assert_eq!(capped(&"é".repeat(10), 5), "éé");
    }

    #[test]
    fn rules_that_do_not_load_fail_closed() {
        assert!(parse("{}").is_none());
        assert!(parse(r#"{"rules":[{"pattern":"(","replacement":"x"}]}"#).is_none());
        assert!(
            parse(r#"{"rules":[{"pattern":"a","replacement":"x","validator":"crc"}]}"#).is_none()
        );
    }
}
