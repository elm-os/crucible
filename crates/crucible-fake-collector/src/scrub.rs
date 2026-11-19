//! The shared scrub rules (`conformance/scrub-rules.json`, contract 9.10): the portable pattern subset, the
//! scrubber, and the check that every rule's test vectors and every pipeline example hold.

use regex::Regex;
use serde::Deserialize;

use crate::pattern::{Facts, Lead, facts};
use crate::suite::{SuiteError, check_schema, parse, schema};

/// The rules file as committed, for senders built into this crate (the self-test runner).
const BUILTIN: &str = include_str!("../../../conformance/scrub-rules.json");

/// The largest repetition count the portable subset allows (RE2 and Go refuse larger ones).
const MAX_REPEAT: u32 = 1000;

/// A check on a match beyond its pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Validator {
    /// The match's digits pass the Luhn checksum.
    Luhn,
}

impl Validator {
    fn accepts(self, text: &str) -> bool {
        match self {
            Self::Luhn => luhn(text),
        }
    }
}

/// One rule.
#[derive(Clone, Debug)]
pub struct ScrubRule {
    /// The rule's name.
    pub name: String,
    /// The compiled pattern.
    pub pattern: Regex,
    /// The replacement text.
    pub replacement: String,
    /// An extra check on each match.
    pub validator: Option<Validator>,
    /// Inputs the rule changes.
    pub positive: Vec<String>,
    /// Inputs the rule leaves unchanged.
    pub negative: Vec<String>,
    /// What the pattern requires of every match.
    facts: Facts,
    /// For a lead pattern without a validator: the pattern from its anchor on, anchored where it is tried.
    rest: Option<Regex>,
}

/// The steps of an unanchored search that tried the starts from `from` up to `to`, as a backtracking matcher takes
/// them at the least: one per start, and from a start where a lead pattern's prefix matches, the prefix and the
/// whole class run after it, which the greedy repetition consumes before it backtracks.
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

impl ScrubRule {
    /// Replaces every accepted match, leftmost first, without overlaps. After a match the validator rejects, the
    /// search resumes after the match's first character that is neither an ASCII letter nor a digit (after the match
    /// when it has none): a rejected run of digit groups cannot hide an accepted one that starts at a later group,
    /// and a run without separators is not searched for sub-windows.
    pub fn apply(&self, text: &str) -> String {
        self.apply_observed(text, None)
    }

    /// [`ScrubRule::apply`], reporting the steps of every matcher call to `observe` (see [`search_steps`]). Guards
    /// that take time linear in the text and change nothing come first: text shorter than any match, or without
    /// the rule's required literal, is left alone, and a lead pattern is tried once per anchor.
    pub fn apply_observed(
        &self,
        text: &str,
        observe: Option<&mut (dyn FnMut(usize) + '_)>,
    ) -> String {
        let f = &self.facts;
        let absent = f
            .literals
            .as_ref()
            .is_some_and(|literals| !literals.iter().any(|l| text.contains(l.as_str())));
        if text.len() < f.min_length || absent {
            return text.to_owned();
        }
        match (&f.lead, &self.rest) {
            (Some(lead), Some(rest)) => self.apply_lead(lead, rest, text, observe),
            _ => self.apply_plain_observed(text, observe),
        }
    }

    /// The rule without its guards: the reference the differential tests hold [`ScrubRule::apply`] to.
    pub fn apply_plain(&self, text: &str) -> String {
        self.apply_plain_observed(text, None)
    }

    /// The matches of a lead pattern, found from its anchors. A match ends its class run at an anchor, and every
    /// start in that run meets the same anchor, so only the run's leftmost possible start is a candidate and the
    /// rest of the pattern is tried once, anchored there. Each position is scanned a bounded number of times.
    fn apply_lead(
        &self,
        lead: &Lead,
        rest: &Regex,
        text: &str,
        mut observe: Option<&mut (dyn FnMut(usize) + '_)>,
    ) -> String {
        let bytes = text.as_bytes();
        let anchor = char::from(lead.anchor);
        let find_anchor = |from: usize| text.get(from..)?.find(anchor).map(|i| from + i);
        let mut out = String::with_capacity(text.len());
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
                if let Some(observe) = observe.as_deref_mut() {
                    // The bytes the attempt can read: its match, or the anchor, the class run after it and the
                    // byte that ends the run.
                    let run = bytes[a + 1..]
                        .iter()
                        .take_while(|&&b| lead.class.has(b))
                        .count();
                    observe(found.map_or(run + 2, |m| m.end()));
                }
                if let Some(m) = found {
                    out.push_str(&text[copied..begin]);
                    out.push_str(&self.replacement);
                    copied = a + m.end();
                    floor = copied;
                    at = find_anchor(floor);
                    continue;
                }
            }
            at = find_anchor(a + 1);
        }
        out.push_str(&text[copied..]);
        out
    }

    fn apply_plain_observed(
        &self,
        text: &str,
        mut observe: Option<&mut (dyn FnMut(usize) + '_)>,
    ) -> String {
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        let mut at = 0;
        let lead = self.facts.lead.as_ref();
        let mut searched = |from: usize, to: usize| {
            if let Some(observe) = observe.as_deref_mut() {
                observe(search_steps(lead, text, from, to));
            }
        };
        loop {
            let Some(m) = self.pattern.find_at(text, at) else {
                searched(at, text.len());
                break;
            };
            searched(at, m.start() + 1);
            if self.validator.is_some_and(|v| !v.accepts(m.as_str())) {
                at = m
                    .as_str()
                    .find(|c: char| !c.is_ascii_alphanumeric())
                    .map_or(m.end(), |i| m.start() + i + 1);
                continue;
            }
            out.push_str(&text[copied..m.start()]);
            out.push_str(&self.replacement);
            copied = m.end();
            // An empty match would not advance; step over one character.
            at = if m.is_empty() {
                text[m.end()..]
                    .chars()
                    .next()
                    .map_or(text.len(), |c| m.end() + c.len_utf8())
            } else {
                m.end()
            };
            if at >= text.len() {
                break;
            }
        }
        out.push_str(&text[copied..]);
        out
    }
}

/// The parsed rules file.
#[derive(Clone, Debug)]
pub struct ScrubRules {
    /// Rules in application order.
    pub rules: Vec<ScrubRule>,
    /// Pipeline examples: input and the output of the whole rule list.
    pub examples: Vec<(String, String)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    schema: String,
    contract: String,
    semantics: Vec<String>,
    rules: Vec<RuleFile>,
    examples: Vec<ExampleFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleFile {
    name: String,
    pattern: String,
    replacement: String,
    #[serde(default)]
    validator: Option<String>,
    positive: Vec<String>,
    negative: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExampleFile {
    input: String,
    output: String,
}

impl ScrubRules {
    /// Parses the rules, checks every pattern against the portable subset, and checks every vector and example.
    pub fn parse(file: &str, bytes: &[u8]) -> Result<Self, SuiteError> {
        let raw: RulesFile = parse(file, bytes)?;
        check_schema(file, &raw.schema, schema::SCRUB)?;
        let err = |m: String| SuiteError::new(file, m);
        if raw.contract.is_empty() || raw.semantics.is_empty() {
            return Err(err("contract and semantics are required".to_owned()));
        }
        let mut rules: Vec<ScrubRule> = Vec::new();
        for r in raw.rules {
            if rules.iter().any(|x| x.name == r.name) || r.replacement.is_empty() {
                return Err(err(format!(
                    "rule {:?}: names are unique and replacements non-empty",
                    r.name
                )));
            }
            portable(&r.pattern).map_err(|e| err(format!("rule {:?}: {e}", r.name)))?;
            let pattern =
                Regex::new(&r.pattern).map_err(|e| err(format!("rule {:?}: {e}", r.name)))?;
            let validator = match r.validator.as_deref() {
                None => None,
                Some("luhn") => Some(Validator::Luhn),
                Some(other) => {
                    return Err(err(format!(
                        "rule {:?}: unknown validator {other:?}",
                        r.name
                    )));
                }
            };
            if r.positive.is_empty() || r.negative.is_empty() {
                return Err(err(format!(
                    "rule {:?}: needs positive and negative vectors",
                    r.name
                )));
            }
            let facts = facts(&r.pattern);
            let rest = match &facts.lead {
                Some(lead) if validator.is_none() => Some(
                    Regex::new(&format!("^(?:{})", lead.rest))
                        .map_err(|e| err(format!("rule {:?}: {e}", r.name)))?,
                ),
                _ => None,
            };
            rules.push(ScrubRule {
                facts,
                rest,
                name: r.name,
                pattern,
                replacement: r.replacement,
                validator,
                positive: r.positive,
                negative: r.negative,
            });
        }
        let rules = Self {
            rules,
            examples: raw
                .examples
                .into_iter()
                .map(|e| (e.input, e.output))
                .collect(),
        };
        rules.check_vectors().map_err(err)?;
        Ok(rules)
    }

    /// The committed rules file, compiled into this crate.
    pub fn builtin() -> Result<Self, SuiteError> {
        Self::parse("scrub-rules.json", BUILTIN.as_bytes())
    }

    /// Applies every rule in order.
    pub fn scrub(&self, text: &str) -> String {
        self.scrub_observed(text, None)
    }

    /// [`ScrubRules::scrub`], reporting the steps of every matcher call to `observe`.
    pub fn scrub_observed(
        &self,
        text: &str,
        mut observe: Option<&mut (dyn FnMut(usize) + '_)>,
    ) -> String {
        let mut text = text.to_owned();
        if text.len() < self.shortest() {
            return text;
        }
        for rule in &self.rules {
            text = rule.apply_observed(&text, observe.as_deref_mut());
        }
        text
    }

    /// The rules applied without their guards: the reference the differential tests hold [`ScrubRules::scrub`] to.
    pub fn scrub_plain(&self, text: &str) -> String {
        self.rules
            .iter()
            .fold(text.to_owned(), |text, rule| rule.apply_plain(&text))
    }

    /// The length no rule matches below.
    fn shortest(&self) -> usize {
        self.rules
            .iter()
            .map(|r| r.facts.min_length)
            .min()
            .unwrap_or(0)
    }

    fn check_vectors(&self) -> Result<(), String> {
        for rule in &self.rules {
            if let Some(v) = rule.positive.iter().find(|v| rule.apply(v) == **v) {
                return Err(format!(
                    "rule {:?} leaves positive vector {v:?} unchanged",
                    rule.name
                ));
            }
            if let Some(v) = rule.negative.iter().find(|v| rule.apply(v) != **v) {
                return Err(format!(
                    "rule {:?} changes negative vector {v:?}",
                    rule.name
                ));
            }
        }
        for (input, output) in &self.examples {
            let got = self.scrub(input);
            if got != *output {
                return Err(format!(
                    "example {input:?} scrubs to {got:?}, not {output:?}"
                ));
            }
        }
        Ok(())
    }
}

/// The Luhn checksum over the digits of `text`.
fn luhn(text: &str) -> bool {
    let digits: Vec<u32> = text.chars().filter_map(|c| c.to_digit(10)).collect();
    if digits.len() < 2 {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| match (i % 2, d * 2) {
            (1, doubled) if doubled > 9 => doubled - 9,
            (1, doubled) => doubled,
            _ => *d,
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Checks that a pattern stays in the portable subset described in the rules file's `semantics`.
pub fn portable(pattern: &str) -> Result<(), String> {
    let chars: Vec<char> = pattern.chars().collect();
    if let Some(c) = chars
        .iter()
        .find(|c| !(c.is_ascii_graphic() || **c == ' ' || **c == '\t'))
    {
        return Err(format!("{c:?} is not printable ASCII"));
    }
    // One entry per open group: whether it holds an unbounded quantifier.
    let mut groups: Vec<bool> = vec![false];
    // What the last atom was: None at the start of a group or after `|`; Some(inner) after an atom, where inner
    // says whether that atom was a group holding an unbounded quantifier.
    let mut atom: Option<bool> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' => {
                escape(chars.get(i + 1).copied())?;
                i += 2;
                atom = Some(false);
                continue;
            }
            '[' => {
                i = class(&chars, i)?;
                atom = Some(false);
                continue;
            }
            '(' => {
                if chars.get(i + 1..i + 3) != Some(&['?', ':'][..]) {
                    return Err("only (?:...) groups are allowed".to_owned());
                }
                groups.push(false);
                atom = None;
                i += 3;
                continue;
            }
            ')' => {
                let inner = groups.pop().ok_or("unbalanced )")?;
                if groups.is_empty() {
                    return Err("unbalanced )".to_owned());
                }
                if inner && let Some(parent) = groups.last_mut() {
                    *parent = true;
                }
                atom = Some(inner);
            }
            '|' => atom = None,
            '*' | '+' | '?' | '{' => {
                let (unbounded, end) = quantifier(&chars, i)?;
                let Some(inner) = atom else {
                    return Err(format!("quantifier at {i} follows no atom"));
                };
                if unbounded && inner {
                    return Err(
                        "an unbounded quantifier on a group that holds one can backtrack exponentially".to_owned(),
                    );
                }
                if matches!(chars.get(end), Some('?' | '+' | '*' | '{')) {
                    return Err(
                        "lazy, possessive and stacked quantifiers are not allowed".to_owned()
                    );
                }
                if unbounded && let Some(top) = groups.last_mut() {
                    *top = true;
                }
                // A quantified atom cannot be quantified again.
                atom = Some(false);
                i = end;
                continue;
            }
            '.' => return Err("'.' differs across engines on newlines; use a class".to_owned()),
            '^' | '$' => return Err("anchors are not allowed".to_owned()),
            '}' | ']' => return Err(format!("unescaped {c:?}")),
            _ => atom = Some(false),
        }
        i += 1;
    }
    if groups.len() != 1 {
        return Err("unbalanced (".to_owned());
    }
    Ok(())
}

fn escape(next: Option<char>) -> Result<(), String> {
    match next {
        Some('n' | 't') => Ok(()),
        Some(c) if c.is_ascii_punctuation() => Ok(()),
        Some(c) => Err(format!(
            "\\{c} is not portable; use an explicit class or escape punctuation only"
        )),
        None => Err("trailing backslash".to_owned()),
    }
}

/// Scans a bracket class starting at `start` (`[`); returns the index after `]`.
fn class(chars: &[char], start: usize) -> Result<usize, String> {
    let mut i = start + 1;
    if chars.get(i) == Some(&'^') {
        i += 1;
    }
    let first = i;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ']' if i > first => return Ok(i + 1),
            '\\' => {
                escape(chars.get(i + 1).copied())?;
                i += 2;
                continue;
            }
            '[' => return Err("'[' inside a class must be escaped".to_owned()),
            '&' | '-' | '|' | '~' if chars.get(i + 1) == Some(&c) => {
                return Err(format!("{c}{c} inside a class is reserved in some engines"));
            }
            _ => {}
        }
        i += 1;
    }
    Err("unterminated class".to_owned())
}

/// Scans a quantifier at `start`; returns whether it is unbounded and the index after it.
fn quantifier(chars: &[char], start: usize) -> Result<(bool, usize), String> {
    match chars[start] {
        '*' | '+' => return Ok((true, start + 1)),
        '?' => return Ok((false, start + 1)),
        _ => {}
    }
    let close = chars[start..]
        .iter()
        .position(|c| *c == '}')
        .map(|p| start + p)
        .ok_or("unterminated {")?;
    let body: String = chars[start + 1..close].iter().collect();
    let count = |s: &str| -> Result<u32, String> {
        let n: u32 = s
            .parse()
            .map_err(|_| format!("{{{body}}} is not a repetition count"))?;
        if n > MAX_REPEAT {
            return Err(format!("repetition count {n} is above {MAX_REPEAT}"));
        }
        Ok(n)
    };
    let unbounded = match body.split_once(',') {
        None => {
            count(&body)?;
            false
        }
        Some((min, "")) => {
            count(min)?;
            true
        }
        Some((min, max)) => {
            if count(min)? > count(max)? {
                return Err(format!("{{{body}}} has min above max"));
            }
            false
        }
    };
    Ok((unbounded, close + 1))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn the_portable_subset_refuses_what_engines_disagree_on() {
        let refused = [
            (r"\d{3}", "not portable"),
            (r"\bword", "not portable"),
            (r"\s+", "not portable"),
            ("a.b", "'.'"),
            ("^start", "anchors"),
            ("end$", "anchors"),
            ("(capture)", "(?:...)"),
            ("(?i)case", "(?:...)"),
            ("(?=ahead)", "(?:...)"),
            ("(?<name>x)", "(?:...)"),
            (r"(a)\1", "(?:...)"),
            ("[[:alpha:]]", "must be escaped"),
            (r"\p{L}", "not portable"),
            ("a+?", "lazy"),
            ("a++", "lazy"),
            ("a{1001}", "above 1000"),
            ("a{3,2}", "min above max"),
            ("(?:a+)+", "backtrack exponentially"),
            ("(?:(?:a*)b)*", "backtrack exponentially"),
            ("[a&&b]", "reserved"),
            ("a{x}", "not a repetition count"),
            ("*a", "follows no atom"),
            ("(?:a", "unbalanced"),
            ("a)", "unbalanced"),
            ("[abc", "unterminated"),
            ("caf\u{e9}", "printable ASCII"),
        ];
        for (pattern, needle) in refused {
            let error = portable(pattern).unwrap_err();
            assert!(error.contains(needle), "{pattern}: {error}");
        }
        for pattern in [
            "[0-9](?:[ -]?[0-9]){12,18}",
            r"(?:[0-9]{1,3}\.){3}[0-9]{1,3}",
            "[^-]*",
            "a{2,}b|c",
            r"\+[0-9]{1,3}",
            "[ \t]+",
        ] {
            portable(pattern).unwrap();
        }
    }

    fn run(unit: &str, length: usize) -> String {
        unit.repeat(length / unit.len() + 1)[..length].to_owned()
    }

    /// Inputs that make a backtracking matcher slow on the shared rules, about 16 KiB each.
    fn step_inputs() -> Vec<(&'static str, String)> {
        use base64::Engine as _;
        let counting: Vec<u8> = (0..12288).map(|i| (i % 256) as u8).collect();
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
            (
                "base64",
                base64::engine::general_purpose::STANDARD.encode(counting),
            ),
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

    fn matcher_steps(rules: &ScrubRules, text: &str) -> usize {
        let mut steps = 0;
        rules.scrub_observed(text, Some(&mut |n| steps += n));
        steps
    }

    #[test]
    fn the_rules_patterns_yield_the_facts_the_guards_use() {
        let rules = ScrubRules::builtin().unwrap();
        let found: Vec<String> = rules
            .rules
            .iter()
            .map(|r| {
                let lead = r
                    .facts
                    .lead
                    .as_ref()
                    .map(|l| (l.prefix.as_str(), char::from(l.anchor)));
                format!(
                    "{} {} {:?} {lead:?}",
                    r.name, r.facts.min_length, r.facts.literals
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                r#"pem 32 Some(["-----BEGIN "]) None"#,
                r#"jwt 7 Some(["eyJ"]) Some(("eyJ", '.'))"#,
                "bearer 23 None None",
                r#"secret_key 12 Some(["csk_"]) None"#,
                "card 14 None None",
                r#"email 6 Some(["@"]) Some(("", '@'))"#,
                r#"ipv6 6 Some([":", ":"]) None"#,
                r#"ipv4 7 Some(["."]) None"#,
                "phone 8 None None",
            ]
        );
    }

    #[test]
    fn matcher_steps_stay_linear_in_the_length() {
        let rules = ScrubRules::builtin().unwrap();
        let failures: Vec<String> = step_inputs()
            .into_iter()
            .filter_map(|(name, text)| {
                let (steps, bound) = (
                    matcher_steps(&rules, &text),
                    2 * rules.rules.len() * text.len(),
                );
                (steps > bound).then(|| format!("{name}: {steps} matcher steps > {bound}"))
            })
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
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

    static BUILTIN: std::sync::LazyLock<ScrubRules> =
        std::sync::LazyLock::new(|| ScrubRules::builtin().unwrap());

    fn generated() -> impl Strategy<Value = String> {
        let fragment = prop_oneof![
            7 => prop::sample::select(FRAGMENTS).prop_map(str::to_owned),
            3 => (prop::sample::select(RUN_UNITS), 1..40usize).prop_map(|(unit, n)| unit.repeat(n)),
        ];
        prop::collection::vec(fragment, 0..14).prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        /// The guards change nothing: guarded application equals plain application, per rule and for the whole
        /// list, and the matcher steps stay linear in the length.
        #[test]
        fn guarded_application_equals_plain_application(text in generated()) {
            let rules = &*BUILTIN;
            for rule in &rules.rules {
                prop_assert_eq!(rule.apply(&text), rule.apply_plain(&text), "rule {}", &rule.name);
            }
            prop_assert_eq!(rules.scrub(&text), rules.scrub_plain(&text));
            prop_assert!(matcher_steps(rules, &text) <= 2 * rules.rules.len() * text.len().max(1));
        }
    }

    #[test]
    fn luhn_accepts_valid_card_numbers_only() {
        assert!(luhn("4111 1111 1111 1111"));
        assert!(luhn("378282246310005"));
        assert!(!luhn("4111 1111 1111 1112"));
        assert!(!luhn("7"));
    }

    fn fragment() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-z ;,]{0,8}",
            Just("ada.lovelace@example.com".to_owned()),
            Just("+1 415 555 0134".to_owned()),
            Just("4111 1111 1111 1111".to_owned()),
            Just("Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0".to_owned()),
            Just("csk_conformance00000000000000000000000000000000".to_owned()),
            Just("203.0.113.42".to_owned()),
            Just("2001:db8::8a2e:370:7334".to_owned()),
            Just("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2ln".to_owned()),
            Just("-----BEGIN KEY-----\nMIIB\n-----END KEY-----".to_owned()),
        ]
    }

    proptest! {
        /// Scrubbing twice changes nothing more: no replacement is itself a match, and a rejected match never
        /// hides an accepted one.
        #[test]
        fn scrubbing_is_idempotent(parts in prop::collection::vec(fragment(), 0..8)) {
            let rules = ScrubRules::builtin().unwrap();
            let once = rules.scrub(&parts.join(" "));
            prop_assert_eq!(rules.scrub(&once), once);
        }
    }
}
