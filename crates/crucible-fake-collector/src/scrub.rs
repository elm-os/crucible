//! The shared scrub rules (`conformance/scrub-rules.json`, contract 9.10): the portable pattern subset, the
//! scrubber, and the check that every rule's test vectors and every pipeline example hold.

use regex::Regex;
use serde::Deserialize;

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
}

impl ScrubRule {
    /// Replaces every accepted match, leftmost first, without overlaps.
    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut last = 0;
        for m in self.pattern.find_iter(text) {
            if self.validator.is_some_and(|v| !v.accepts(m.as_str())) {
                continue;
            }
            out.push_str(&text[last..m.start()]);
            out.push_str(&self.replacement);
            last = m.end();
        }
        out.push_str(&text[last..]);
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
            rules.push(ScrubRule {
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
        self.rules
            .iter()
            .fold(text.to_owned(), |text, rule| rule.apply(&text))
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
