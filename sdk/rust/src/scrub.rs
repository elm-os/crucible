//! The shared scrubbing rules (contract 9.10): secrets and personal data are replaced in captured arguments,
//! results, error messages and handle ids before they leave the process.
//!
//! The rules are data: `scrub-rules.json` in this crate is a copy of the repository's
//! `conformance/scrub-rules.json`, which every Crucible SDK and the proxy apply, and a test keeps the two
//! identical. Rules run in file order, each replacing every non-overlapping match, leftmost first. After a match
//! its validator rejects, the search resumes after the match's first character that is neither an ASCII letter
//! nor a digit, or after the match when it has none. The patterns stay in a subset that the `regex` crate
//! matches in time linear in the input.

use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;

const RULES_JSON: &str = include_str!("scrub-rules.json");

/// How far past a cap the text is scrubbed, so that a secret which starts before the cap and ends after it is
/// still recognised whole and its visible prefix replaced.
const LOOKAHEAD: usize = 1024;

/// The replacement used for everything when the rules cannot load: scrubbing fails closed.
const FAIL_CLOSED: &str = "[redacted]";

#[derive(Debug)]
struct Rule {
    pattern: Regex,
    replacement: String,
    luhn: bool,
}

#[derive(Deserialize)]
struct RulesFile {
    rules: Vec<RuleFile>,
}

#[derive(Deserialize)]
struct RuleFile {
    name: String,
    pattern: String,
    replacement: String,
    #[serde(default)]
    validator: Option<String>,
}

static RULES: LazyLock<Result<Vec<Rule>, String>> = LazyLock::new(|| parse(RULES_JSON));

fn parse(json: &str) -> Result<Vec<Rule>, String> {
    let file: RulesFile = serde_json::from_str(json).map_err(|e| e.to_string())?;
    file.rules
        .into_iter()
        .map(|r| {
            let luhn = match r.validator.as_deref() {
                None => false,
                Some("luhn") => true,
                Some(other) => return Err(format!("rule {}: unknown validator {other}", r.name)),
            };
            let pattern = Regex::new(&r.pattern).map_err(|e| format!("rule {}: {e}", r.name))?;
            Ok(Rule {
                pattern,
                replacement: r.replacement,
                luhn,
            })
        })
        .collect()
}

impl Rule {
    /// The text with every accepted match replaced, or `None` when nothing matched.
    fn apply(&self, text: &str) -> Option<String> {
        let mut out = String::new();
        let mut changed = false;
        let mut copied = 0;
        let mut at = 0;
        while at <= text.len() {
            let Some(m) = self.pattern.find_at(text, at) else {
                break;
            };
            if self.luhn && !luhn(m.as_str()) {
                at = m
                    .as_str()
                    .char_indices()
                    .find(|(_, c)| !c.is_ascii_alphanumeric())
                    .map_or(m.end(), |(i, c)| m.start() + i + c.len_utf8());
                continue;
            }
            out.push_str(&text[copied..m.start()]);
            out.push_str(&self.replacement);
            changed = true;
            copied = m.end();
            // An empty match would not advance; step over one character.
            at = if m.is_empty() {
                text[m.end()..]
                    .chars()
                    .next()
                    .map_or(text.len() + 1, |c| m.end() + c.len_utf8())
            } else {
                m.end()
            };
        }
        if !changed {
            return None;
        }
        out.push_str(&text[copied..]);
        Some(out)
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
pub(crate) fn scrub(text: &str) -> String {
    match &*RULES {
        Ok(rules) => {
            let mut text = text.to_owned();
            for rule in rules {
                if let Some(changed) = rule.apply(&text) {
                    text = changed;
                }
            }
            text
        }
        Err(_) => FAIL_CLOSED.to_owned(),
    }
}

/// The longest prefix of `text` of at most `max` bytes that ends on a character boundary, and whether it is
/// shorter than `text`.
pub(crate) fn prefix(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// At most `max` bytes of `text`, scrubbed. Only the first `max` plus 1 KiB bytes are read, so the cost is
/// bounded by the cap and not by the size of `text`. The flag says whether anything was cut.
pub(crate) fn capped(text: &str, max: usize) -> (String, bool) {
    let (window, cut) = prefix(text, max.saturating_add(LOOKAHEAD));
    let scrubbed = scrub(window);
    let (out, cut_after) = prefix(&scrubbed, max);
    (out.to_owned(), cut || cut_after)
}

/// The size of the window [`capped`] reads for the cap `max`.
#[cfg(feature = "mcp")]
pub(crate) fn window(max: usize) -> usize {
    max.saturating_add(LOOKAHEAD)
}

#[cfg(test)]
mod tests {
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
                assert!(
                    rule.apply(input).is_some(),
                    "rule {} leaves {input:?} unchanged",
                    v.name
                );
            }
            for input in &v.negative {
                assert_eq!(rule.apply(input), None, "rule {} changes {input:?}", v.name);
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
            let (out, _) = capped(&text, 100);
            assert!(!out.contains("csk_"), "{out:?}");
            assert!(out.len() <= 100);
        }
    }

    #[test]
    fn prefixes_end_on_character_boundaries() {
        let text = "aé€😀b";
        for max in 0..=text.len() + 1 {
            let (p, cut) = prefix(text, max);
            assert!(p.len() <= max);
            assert_eq!(cut, p.len() < text.len());
            assert!(text.starts_with(p));
        }
    }
}
