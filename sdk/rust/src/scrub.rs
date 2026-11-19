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

use crate::pattern::{Facts, Lead, facts};

const RULES_JSON: &str = include_str!("scrub-rules.json");

/// How far past a cap the text is scrubbed, so that a secret which starts before the cap and ends after it is
/// still recognised whole and its visible prefix replaced.
const LOOKAHEAD: usize = 1024;

/// The replacement used for everything when the rules cannot load: scrubbing fails closed.
const FAIL_CLOSED: &str = "[redacted]";

#[derive(Debug)]
struct Rule {
    #[cfg(any(test, feature = "test-support"))]
    name: String,
    pattern: Regex,
    replacement: String,
    luhn: bool,
    facts: Facts,
    /// For a lead pattern without a validator: the pattern from its anchor on, anchored where it is tried.
    rest: Option<Regex>,
}

/// No rule matches text shorter than this.
static SHORTEST: LazyLock<usize> = LazyLock::new(|| match &*RULES {
    Ok(rules) => rules.iter().map(|r| r.facts.min_length).min().unwrap_or(0),
    Err(_) => 0,
});

/// Called once per matcher call with the rule's name and the call's steps (see `search_steps`).
#[cfg(any(test, feature = "test-support"))]
pub type MatcherHook = std::sync::Arc<dyn Fn(&str, usize) + Send + Sync>;

#[cfg(any(test, feature = "test-support"))]
static MATCHER_HOOK: std::sync::RwLock<Option<MatcherHook>> = std::sync::RwLock::new(None);

/// Installs, or with `None` removes, the hook the scrubbing work checks read matcher steps from.
#[cfg(any(test, feature = "test-support"))]
pub fn set_matcher_hook(hook: Option<MatcherHook>) {
    *MATCHER_HOOK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = hook;
}

#[cfg(any(test, feature = "test-support"))]
fn matcher_hook() -> Option<MatcherHook> {
    MATCHER_HOOK
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// The steps of an unanchored search that tried the starts from `from` up to `to`, as a backtracking matcher takes
/// them at the least: one per start, and from a start where a lead pattern's prefix matches, the prefix and the
/// whole class run after it, which the greedy repetition consumes before it backtracks.
#[cfg(any(test, feature = "test-support"))]
fn search_steps(lead: Option<&Lead>, text: &str, from: usize, to: usize) -> usize {
    let Some(lead) = lead else {
        return to - from;
    };
    let bytes = text.as_bytes();
    let p = lead.prefix.len();
    let mut end = text.len().min(to + p);
    while end < text.len() && lead.class.has(bytes[end]) {
        end += 1;
    }
    let mut runs = vec![0usize; end - from + 1];
    for k in (from..end).rev() {
        if lead.class.has(bytes[k]) {
            runs[k - from] = runs[k - from + 1] + 1;
        }
    }
    (from..to)
        .map(|s| {
            let prefixed = bytes[s..].starts_with(lead.prefix.as_bytes());
            1 + if prefixed {
                p + runs.get(s + p - from).copied().unwrap_or(0)
            } else {
                0
            }
        })
        .sum()
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
            let facts = facts(&r.pattern);
            let rest = match &facts.lead {
                Some(lead) if !luhn => Some(
                    Regex::new(&format!("^(?:{})", lead.rest))
                        .map_err(|e| format!("rule {}: {e}", r.name))?,
                ),
                _ => None,
            };
            Ok(Rule {
                facts,
                rest,
                #[cfg(any(test, feature = "test-support"))]
                name: r.name,
                pattern,
                replacement: r.replacement,
                luhn,
            })
        })
        .collect()
}

impl Rule {
    /// The rule behind guards that take time linear in the text and change nothing: text shorter than any
    /// match, or without the rule's required literal, is left alone, and a lead pattern is tried once per anchor.
    /// `None` when nothing matched.
    fn apply(&self, text: &str) -> Option<String> {
        let f = &self.facts;
        if text.len() < f.min_length {
            return None;
        }
        if let Some(literals) = &f.literals
            && !literals.iter().any(|l| text.contains(l.as_str()))
        {
            return None;
        }
        match (&f.lead, &self.rest) {
            (Some(lead), Some(rest)) => self.apply_lead(lead, rest, text),
            _ => self.apply_plain(text),
        }
    }

    /// The matches of a lead pattern, found from its anchors. A match ends its class run at an anchor, and every
    /// start in that run meets the same anchor, so only the run's leftmost possible start is a candidate and the
    /// rest of the pattern is tried once, anchored there. Each position is scanned a bounded number of times.
    fn apply_lead(&self, lead: &Lead, rest: &Regex, text: &str) -> Option<String> {
        let bytes = text.as_bytes();
        let anchor = char::from(lead.anchor);
        let find_anchor = |from: usize| text.get(from..)?.find(anchor).map(|i| from + i);
        let mut out = String::new();
        let mut changed = false;
        let (mut copied, mut floor) = (0, 0);
        let mut at = find_anchor(0);
        while let Some(a) = at {
            let mut start = a;
            while start > floor && lead.class.has(bytes[start - 1]) {
                start -= 1;
            }
            let begin = if lead.prefix.is_empty() {
                (start < a).then_some(start)
            } else {
                text[start..a]
                    .find(lead.prefix.as_str())
                    .map(|k| start + k)
                    .filter(|&b| b + lead.prefix.len() < a)
            };
            if let Some(begin) = begin {
                let found = rest.find(&text[a..]);
                self.attempted(text, a, found.map(|m| m.end()));
                if let Some(m) = found {
                    out.push_str(&text[copied..begin]);
                    out.push_str(&self.replacement);
                    changed = true;
                    copied = a + m.end();
                    floor = copied;
                    at = find_anchor(floor);
                    continue;
                }
            }
            at = find_anchor(a + 1);
        }
        if !changed {
            return None;
        }
        out.push_str(&text[copied..]);
        Some(out)
    }

    /// Reports an anchored attempt at `at` to the matcher hook: the bytes it can read, which are its match, or the
    /// anchor, the class run after it and the byte that ends the run.
    fn attempted(&self, text: &str, at: usize, end: Option<usize>) {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hook) = matcher_hook() {
            let steps = end.unwrap_or_else(|| {
                let run = text.as_bytes()[at + 1..]
                    .iter()
                    .take_while(|&&b| self.facts.lead.as_ref().is_some_and(|l| l.class.has(b)))
                    .count();
                run + 2
            });
            hook(&self.name, steps);
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = (text, at, end);
    }

    /// Reports an unanchored search that tried the starts from `from` up to `to` to the matcher hook.
    fn searched(&self, text: &str, from: usize, to: usize) {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(hook) = matcher_hook() {
            hook(
                &self.name,
                search_steps(self.facts.lead.as_ref(), text, from, to),
            );
        }
        #[cfg(not(any(test, feature = "test-support")))]
        let _ = (text, from, to);
    }

    /// The text with every accepted match replaced, or `None` when nothing matched: the rule without its guards,
    /// the reference the differential tests hold `apply` to.
    fn apply_plain(&self, text: &str) -> Option<String> {
        let mut out = String::new();
        let mut changed = false;
        let mut copied = 0;
        let mut at = 0;
        while at <= text.len() {
            let Some(m) = self.pattern.find_at(text, at) else {
                self.searched(text, at, text.len());
                break;
            };
            self.searched(text, at, m.start() + 1);
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
        Ok(_) if text.len() < *SHORTEST => text.to_owned(),
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

    fn run(unit: &str, length: usize) -> String {
        unit.repeat(length / unit.len() + 1)[..length].to_owned()
    }

    /// The standard base64 encoding of the bytes 0, 1, ..., 255, 0, 1, ...
    fn base64_of_counting_bytes(length: usize) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes: Vec<u8> = (0..length).map(|i| (i % 256) as u8).collect();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
            for i in 0..4 {
                out.push(if i <= chunk.len() {
                    char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize])
                } else {
                    '='
                });
            }
        }
        out
    }

    /// Inputs that make a backtracking matcher slow on the shared rules, about 16 KiB each.
    fn step_inputs() -> Vec<(&'static str, String)> {
        vec![
            ("letters", "a".repeat(16384)),
            ("digits", "1".repeat(16384)),
            ("digit pairs", "1 ".repeat(8192)),
            ("at signs", "a@".repeat(8192)),
            ("dots", "a.".repeat(8192)),
            (
                "one at sign",
                format!("{}@{}", "a".repeat(8000), "a".repeat(8000)),
            ),
            ("hex", run("0123456789abcdef", 16384)),
            (
                "alphanumeric",
                run(
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                    16384,
                ),
            ),
            ("base64", base64_of_counting_bytes(12288)),
            (
                "pem header",
                format!("-----BEGIN A-----{}", "A".repeat(16000)),
            ),
            ("jwt prefixes", "eyJ".repeat(5461)),
            (
                "jwt chain",
                format!("{}{}", "eyJ".repeat(5000), ".eyJ".repeat(400)),
            ),
            ("bearer", "bearer ".repeat(2340)),
            ("colons", "1:".repeat(8192)),
            ("plus ones", "+1".repeat(8192)),
            ("phone groups", "(415) ".repeat(2730)),
            ("secret key prefixes", "csk_".repeat(4096)),
        ]
    }

    /// The tests that install the matcher hook take turns.
    static HOOK_TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Matcher steps of scrubbing `text` with the whole rule list. Tests run in parallel, so the hook counts only
    /// this thread's calls.
    fn matcher_steps(text: &str) -> usize {
        let _turn = HOOK_TURN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = steps.clone();
        let this = std::thread::current().id();
        set_matcher_hook(Some(std::sync::Arc::new(move |_, n| {
            if std::thread::current().id() == this {
                seen.fetch_add(n, std::sync::atomic::Ordering::Relaxed);
            }
        })));
        scrub(text);
        set_matcher_hook(None);
        steps.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn rule_count() -> usize {
        parse(RULES_JSON).unwrap().len()
    }

    /// A rule's name, shortest match, required literals and lead prefix and anchor.
    type RuleFacts<'a> = (&'a str, usize, Option<Vec<String>>, Option<(String, char)>);

    #[test]
    fn the_rules_patterns_yield_the_facts_the_guards_use() {
        let rules = parse(RULES_JSON).unwrap();
        let found: Vec<RuleFacts<'_>> = rules
            .iter()
            .map(|r| {
                (
                    r.name.as_str(),
                    r.facts.min_length,
                    r.facts.literals.clone(),
                    r.facts
                        .lead
                        .as_ref()
                        .map(|l| (l.prefix.clone(), char::from(l.anchor))),
                )
            })
            .collect();
        let lit = |l: &[&str]| Some(l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert_eq!(
            found,
            vec![
                ("pem", 32, lit(&["-----BEGIN "]), None),
                ("jwt", 7, lit(&["eyJ"]), Some(("eyJ".to_owned(), '.'))),
                ("bearer", 23, None, None),
                ("secret_key", 12, lit(&["csk_"]), None),
                ("card", 14, None, None),
                ("email", 6, lit(&["@"]), Some((String::new(), '@'))),
                ("ipv6", 6, lit(&[":", ":"]), None),
                ("ipv4", 7, lit(&["."]), None),
                ("phone", 8, None, None),
            ]
        );
    }

    #[test]
    fn matcher_steps_stay_linear_in_the_length() {
        let failures: Vec<String> = step_inputs()
            .into_iter()
            .filter_map(|(name, text)| {
                let (steps, bound) = (matcher_steps(&text), 2 * rule_count() * text.len());
                (steps > bound).then(|| format!("{name}: {steps} matcher steps > {bound}"))
            })
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// A seeded xorshift generator.
    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n as u64) as usize
        }
    }

    /// Pieces of matches, near misses and separators that the generated text is built from.
    const FRAGMENTS: &[&str] = &[
        " ",
        "a",
        "0",
        "7",
        "@",
        ".",
        ":",
        "-",
        "_",
        "+",
        "(",
        ")",
        "/",
        "=",
        "%",
        "\n",
        "é",
        "😀",
        "eyJ",
        "csk_",
        "bearer ",
        "Bearer\t",
        "-----BEGIN ",
        "-----END ",
        "-----",
        "KEY",
        "::",
        "@@",
        "a@b",
        "a@b.c",
        "x@y.zz",
        "ada.lovelace@example.com",
        "ops+alerts@mail.example.co.uk",
        "+1 415 555 0134",
        "(415) 555-0134",
        "4111 1111 1111 1111",
        "4111-1111-1111-1112",
        "378282246310005",
        "Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
        "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
        "203.0.113.42",
        "2001:db8::8a2e:370:7334",
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl",
        "eyJa.b",
        "eyJeyJ.x.",
        "eyJ.",
        "-----BEGIN KEY-----\nMIIB\n-----END KEY-----",
    ];
    const RUN_UNITS: &[&str] = &[
        "a", "Z", "9", "f", ".", "-", "_", "+", "%", "ab@", "eyJ", "1 ",
    ];

    fn generated(random: &mut Random) -> String {
        let mut text = String::new();
        for _ in 0..random.below(14) {
            if random.below(10) < 7 {
                text.push_str(FRAGMENTS[random.below(FRAGMENTS.len())]);
            } else {
                text.push_str(
                    &RUN_UNITS[random.below(RUN_UNITS.len())].repeat(1 + random.below(40)),
                );
            }
        }
        text
    }

    #[test]
    fn guarded_application_equals_plain_application_over_generated_text() {
        let rules = parse(RULES_JSON).unwrap();
        let mut random = Random(20_261_118);
        let mut mismatches = Vec::new();
        for _ in 0..3000 {
            let text = generated(&mut random);
            let mut plain = text.clone();
            for rule in &rules {
                if rule.apply(&text) != rule.apply_plain(&text) {
                    mismatches.push(format!("{}: {text:?}", rule.name));
                }
                if let Some(changed) = rule.apply_plain(&plain) {
                    plain = changed;
                }
            }
            if scrub(&text) != plain {
                mismatches.push(format!("all rules: {text:?}"));
            }
            if matcher_steps(&text) > 2 * rules.len() * text.len().max(1) {
                mismatches.push(format!("steps: {text:?}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "{:?}",
            &mismatches[..mismatches.len().min(3)]
        );
    }

    #[test]
    fn a_long_run_scrubs_within_a_generous_ceiling() {
        let text = format!("{}@{}", "a".repeat(65536), "a".repeat(65536));
        let started = std::time::Instant::now();
        scrub(&text);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
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
