//! Content capture (contract 9.10), with the SDKs' rules: the arguments and results of the tool calls an operator
//! opts in. Nothing is captured unless a rule's tool glob matches the called tool; the first matching rule applies;
//! the call is sampled against the rule's rate before anything of it is kept; and each field becomes JSON text,
//! scrubbed with the shared rules over the cap plus 1 KiB and then cut at the cap (16 KiB unless the rule sets
//! another).
//!
//! A rule is written as its tool glob, then options separated by commas, the way `crucible proxy --capture` and
//! `CRUCIBLE_PROXY_CAPTURE` take it: `search_*`, or `lookup_order,result=false,sample_rate=0.1,max_bytes=4096`.
//! The options are the SDKs' rule fields: `args`, `result`, `sample_rate` and `max_bytes` (`tool=<glob>` is also
//! accepted for the glob).

use std::fmt;
use std::sync::Arc;

use crucible_env::Rng;
use crucible_records::caps::MCP_CONTENT_MAX_BYTES;

use crate::scrub;

/// The cap per captured field when a rule sets none, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 16 * 1024;

/// The share of matching calls that are captured: a number from 0 to 1.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct SampleRate(f64);

// A rate is never NaN, so equality is reflexive.
impl Eq for SampleRate {}

impl SampleRate {
    /// Every call.
    pub const ALL: Self = Self(1.0);

    /// The rate `rate`, when it is a number from 0 to 1.
    pub fn new(rate: f64) -> Option<Self> {
        (0.0..=1.0).contains(&rate).then_some(Self(rate))
    }

    /// The rate as a number from 0 to 1.
    pub const fn get(self) -> f64 {
        self.0
    }

    /// Whether a call is captured: one draw from `rng`, unless the rate decides alone.
    fn sampled(self, rng: &dyn Rng) -> bool {
        if self.0 >= 1.0 {
            return true;
        }
        if self.0 <= 0.0 {
            return false;
        }
        // The top 53 bits as a uniform fraction in [0, 1).
        let fraction = (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        fraction < self.0
    }
}

/// One capture rule: which tools, which fields, how often and how much.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CaptureRule {
    /// The tool name glob: `*` matches any run of characters and `?` one character.
    pub tool: String,
    /// Capture `params.arguments` as `gen_ai.tool.call.arguments`.
    pub args: bool,
    /// Capture the result's `content`, `structuredContent` and `isError` as `gen_ai.tool.call.result`.
    pub result: bool,
    /// The share of matching calls captured.
    pub sample_rate: SampleRate,
    /// The cap per field, in bytes of scrubbed JSON text.
    pub max_bytes: usize,
}

impl Default for CaptureRule {
    fn default() -> Self {
        Self {
            tool: "*".to_owned(),
            args: true,
            result: true,
            sample_rate: SampleRate::ALL,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Why a rule was refused, and how to write it.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("capture rule {rule:?}: {problem}")]
pub struct CaptureRuleError {
    /// The rule as written.
    pub rule: String,
    /// What is wrong, and the fix.
    pub problem: String,
}

impl CaptureRule {
    /// Parses a rule written as `<tool glob>[,<option>=<value>]...` (see the module documentation).
    pub fn parse(text: &str) -> Result<Self, CaptureRuleError> {
        let refuse = |problem: String| CaptureRuleError {
            rule: text.to_owned(),
            problem,
        };
        let mut rule = Self::default();
        let mut seen: Vec<&str> = Vec::new();
        for item in text.split(',').map(str::trim) {
            let (key, value) = match item.split_once('=') {
                Some((key, value)) => (key.trim(), value.trim()),
                None => ("tool", item),
            };
            if seen.contains(&key) {
                return Err(refuse(format!(
                    "{key} is given twice; write the tool glob first, then each option once"
                )));
            }
            seen.push(key);
            let flag = |value: &str| match value {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => Err(refuse(format!(
                    "{key} must be true or false, not {value:?}"
                ))),
            };
            match key {
                "tool" if value.is_empty() => {
                    return Err(refuse(
                        "the tool glob is empty; write a tool name or a glob such as search_*, or * for every tool"
                            .to_owned(),
                    ));
                }
                "tool" => value.clone_into(&mut rule.tool),
                "args" => rule.args = flag(value)?,
                "result" => rule.result = flag(value)?,
                "sample_rate" => {
                    rule.sample_rate =
                        value
                            .parse()
                            .ok()
                            .and_then(SampleRate::new)
                            .ok_or_else(|| {
                                refuse(format!(
                                    "sample_rate must be a number from 0 to 1, not {value:?}"
                                ))
                            })?;
                }
                "max_bytes" => {
                    rule.max_bytes = value
                        .parse()
                        .ok()
                        .filter(|max| (1..=MCP_CONTENT_MAX_BYTES).contains(max))
                        .ok_or_else(|| {
                            refuse(format!(
                                "max_bytes must be a whole number from 1 to {MCP_CONTENT_MAX_BYTES} (the most \
                                 Crucible keeps of a field), not {value:?}"
                            ))
                        })?;
                }
                other => {
                    return Err(refuse(format!(
                        "unknown option {other:?}; the options are args, result, sample_rate and max_bytes"
                    )));
                }
            }
        }
        Ok(rule)
    }

    /// The most canonical JSON text read of one field: the cap and the scrubbing lookahead past it, so that a
    /// secret which straddles the cap is still matched whole.
    pub const fn window(&self) -> usize {
        scrub::window(self.max_bytes)
    }
}

impl fmt::Display for CaptureRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{},args={},result={},sample_rate={},max_bytes={}",
            self.tool,
            self.args,
            self.result,
            self.sample_rate.get(),
            self.max_bytes
        )
    }
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters and `?` any one character. Linear in
/// the product of the lengths at worst.
pub fn glob(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
}

/// The rules in force and the stream of sampling draws, shared by the scanners of one core.
#[derive(Clone, Debug)]
pub struct ContentCapture {
    rules: Arc<[CaptureRule]>,
    rng: Arc<dyn Rng>,
}

impl ContentCapture {
    /// Capture with these rules and draws; `None` without rules, when capture is off.
    pub fn new(rules: &[CaptureRule], rng: Arc<dyn Rng>) -> Option<Self> {
        (!rules.is_empty()).then(|| Self {
            rules: rules.into(),
            rng,
        })
    }

    /// The index of the rule that captures a call of `tool`, when the call is sampled: the first rule whose glob
    /// matches, then one draw against its rate. `None` means that nothing of the call is kept.
    pub fn decide(&self, tool: &str) -> Option<usize> {
        let (index, rule) = self
            .rules
            .iter()
            .enumerate()
            .find(|(_, rule)| glob(&rule.tool, tool))?;
        rule.sample_rate.sampled(self.rng.as_ref()).then_some(index)
    }

    /// The rule at `index`.
    pub fn rule(&self, index: usize) -> Option<&CaptureRule> {
        self.rules.get(index)
    }

    /// The largest window of a rule that captures arguments; 0 when none does.
    pub fn args_window(&self) -> usize {
        self.windows(|rule| rule.args)
    }

    /// The largest window of a rule that captures results; 0 when none does.
    pub fn result_window(&self) -> usize {
        self.windows(|rule| rule.result)
    }

    fn windows(&self, applies: fn(&CaptureRule) -> bool) -> usize {
        self.rules
            .iter()
            .filter(|rule| applies(rule))
            .map(CaptureRule::window)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use crucible_env::ChaChaRng;

    use super::*;

    #[test]
    fn rules_parse_with_the_sdks_defaults() {
        assert_eq!(CaptureRule::parse("*").unwrap(), CaptureRule::default());
        let rule = CaptureRule::parse("lookup_order, result=false,sample_rate=0.25,max_bytes=4096")
            .unwrap();
        assert_eq!(
            rule,
            CaptureRule {
                tool: "lookup_order".to_owned(),
                args: true,
                result: false,
                sample_rate: SampleRate::new(0.25).unwrap(),
                max_bytes: 4096,
            }
        );
        // The written form parses back to the same rule.
        assert_eq!(CaptureRule::parse(&rule.to_string()).unwrap(), rule);
        assert_eq!(
            CaptureRule::parse("tool=search_*,args=false").unwrap().tool,
            "search_*"
        );
    }

    #[test]
    fn a_refused_rule_says_how_to_write_it() {
        for (text, fix) in [
            (
                "secret,arg=true",
                "the options are args, result, sample_rate and max_bytes",
            ),
            ("secret,args=yes", "args must be true or false"),
            (
                "secret,sample_rate=2",
                "sample_rate must be a number from 0 to 1",
            ),
            (
                "secret,sample_rate=NaN",
                "sample_rate must be a number from 0 to 1",
            ),
            (
                "secret,max_bytes=0",
                "max_bytes must be a whole number from 1 to 8388608",
            ),
            (
                "secret,max_bytes=9000000",
                "max_bytes must be a whole number from 1 to 8388608",
            ),
            ("secret,other", "tool is given twice"),
            ("", "the tool glob is empty"),
        ] {
            let error = CaptureRule::parse(text).unwrap_err();
            assert!(error.to_string().contains(fix), "{text:?}: {error}");
        }
    }

    #[test]
    fn globs_match_like_shell_patterns() {
        assert!(glob("*", "anything"));
        assert!(glob("*", ""));
        assert!(glob("secret", "secret"));
        assert!(!glob("secret", "secrets"));
        assert!(glob("search_*", "search_docs"));
        assert!(!glob("search_*", "lookup"));
        assert!(glob("a?c", "abc"));
        assert!(!glob("a?c", "ac"));
        assert!(glob("*_order*", "lookup_order_v2"));
        assert!(glob("**x", "abx"));
        assert!(!glob("", "x"));
    }

    #[test]
    fn the_first_matching_rule_decides_and_its_rate_samples() {
        let rules = [
            CaptureRule::parse("search_*,sample_rate=0").unwrap(),
            CaptureRule::parse("*,sample_rate=0.5").unwrap(),
        ];
        let capture = ContentCapture::new(&rules, ChaChaRng::from_u64(3).fork("capture")).unwrap();
        // The first rule matches and never samples, so the second never gets a say.
        assert!((0..100).all(|_| capture.decide("search_docs").is_none()));
        let captured = (0..10_000)
            .filter(|_| capture.decide("echo") == Some(1))
            .count();
        assert!((4_500..5_500).contains(&captured), "{captured}");
        assert!(ContentCapture::new(&[], ChaChaRng::from_u64(3).fork("capture")).is_none());
        assert_eq!(capture.args_window(), DEFAULT_MAX_BYTES + 1024);
    }
}
