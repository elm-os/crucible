//! What a scrub rule's pattern requires of every match, read once when the rules load. Patterns are in the shared
//! subset of `conformance/scrub-rules.json` (literals, escaped punctuation, bracket classes, `(?:...)` groups,
//! alternation and greedy quantifiers), which the conformance driver checks before any SDK loads the file.

/// A bracket class: membership of the ASCII bytes, possibly negated. A byte of a multi-byte UTF-8 character is
/// never in a positive class.
#[derive(Clone, Debug)]
pub(crate) struct CharClass {
    members: [bool; 128],
    negated: bool,
}

impl CharClass {
    pub(crate) fn has(&self, byte: u8) -> bool {
        self.negated
            != self
                .members
                .get(usize::from(byte))
                .copied()
                .unwrap_or(false)
    }
}

#[derive(Clone, Debug)]
enum Atom {
    Char(u8),
    Class(CharClass),
    Group(Vec<Vec<Item>>),
}

#[derive(Clone, Debug)]
struct Item {
    atom: Atom,
    min: usize,
    /// `None` for an unbounded repetition.
    max: Option<usize>,
    /// Offset of the item in the pattern.
    start: usize,
}

/// A pattern that starts with a literal prefix, then an unbounded repetition of a class that holds the prefix's
/// characters, then a literal anchor outside that class: `[A-Za-z0-9._%+-]+@...` (empty prefix) or
/// `eyJ[A-Za-z0-9_-]+\....`. A match can start only inside the class run that ends at an anchor, and every start
/// in one run meets the same anchor, so a matcher needs one anchored attempt per anchor.
#[derive(Clone, Debug)]
pub(crate) struct Lead {
    pub(crate) prefix: String,
    pub(crate) class: CharClass,
    pub(crate) anchor: u8,
    /// The pattern from the anchor on.
    pub(crate) rest: String,
}

/// A member of a class followed by a literal character: `[0-9A-Fa-f]` then `:` for the ipv6 rule.
#[derive(Clone, Debug)]
pub(crate) struct Pair {
    class: CharClass,
    literal: u8,
}

/// What the scrubber's guards read.
#[derive(Clone, Debug, Default)]
pub(crate) struct Facts {
    /// The fewest bytes a match has.
    pub(crate) min_length: usize,
    /// For each top-level alternative, a literal every match of it contains; `None` when one has none.
    pub(crate) literals: Option<Vec<String>>,
    pub(crate) lead: Option<Lead>,
    /// For each top-level alternative, a pair every match of it holds; `None` when one has none.
    pub(crate) pairs: Option<Vec<Pair>>,
}

impl Facts {
    /// Whether `text` holds one of the pairs: its literal right after a member of its class. A text without any
    /// of them holds no match.
    pub(crate) fn holds_pair(&self, text: &str) -> bool {
        let bytes = text.as_bytes();
        self.pairs.iter().flatten().any(|pair| {
            bytes
                .windows(2)
                .any(|w| w[1] == pair.literal && pair.class.has(w[0]))
        })
    }
}

/// The facts of `pattern`, or none (no guard) when it does not parse as the subset.
pub(crate) fn facts(pattern: &str) -> Facts {
    let Some(branches) = Parser::parse(pattern) else {
        return Facts::default();
    };
    Facts {
        min_length: min_length(&branches),
        literals: branches.iter().map(|items| literal_of(items)).collect(),
        lead: lead_of(&branches, pattern),
        pairs: branches.iter().map(|items| pair_of(items)).collect(),
    }
}

struct Parser<'a> {
    p: &'a [u8],
    i: usize,
}

fn unescape(c: u8) -> u8 {
    match c {
        b'n' => b'\n',
        b't' => b'\t',
        other => other,
    }
}

impl Parser<'_> {
    fn parse(pattern: &str) -> Option<Vec<Vec<Item>>> {
        let mut parser = Parser {
            p: pattern.as_bytes(),
            i: 0,
        };
        let branches = parser.alternation()?;
        (parser.i == parser.p.len()).then_some(branches)
    }

    fn at(&self, k: usize) -> Option<u8> {
        self.p.get(self.i + k).copied()
    }

    fn alternation(&mut self) -> Option<Vec<Vec<Item>>> {
        let mut branches = vec![Vec::new()];
        while let Some(c) = self.at(0) {
            if c == b')' {
                break;
            }
            if c == b'|' {
                self.i += 1;
                branches.push(Vec::new());
                continue;
            }
            let start = self.i;
            let atom = if self.p[self.i..].starts_with(b"(?:") {
                self.i += 3;
                let inner = self.alternation()?;
                if self.at(0) != Some(b')') {
                    return None;
                }
                self.i += 1;
                Atom::Group(inner)
            } else if c == b'[' {
                Atom::Class(self.class()?)
            } else if c == b'\\' {
                let escaped = unescape(self.at(1)?);
                self.i += 2;
                Atom::Char(escaped)
            } else {
                self.i += 1;
                Atom::Char(c)
            };
            let (min, max) = self.quantifier()?;
            branches.last_mut()?.push(Item {
                atom,
                min,
                max,
                start,
            });
        }
        Some(branches)
    }

    fn class(&mut self) -> Option<CharClass> {
        self.i += 1;
        let negated = self.at(0) == Some(b'^');
        if negated {
            self.i += 1;
        }
        let mut members = [false; 128];
        let first = self.i;
        while self.at(0) != Some(b']') || self.i == first {
            let mut low = self.at(0)?;
            if low == b'\\' {
                self.i += 1;
                low = unescape(self.at(0)?);
            }
            self.i += 1;
            let mut high = low;
            if self.at(0) == Some(b'-') && !matches!(self.at(1), Some(b']') | None) {
                if self.at(1) == Some(b'\\') {
                    high = unescape(self.at(2)?);
                    self.i += 3;
                } else {
                    high = self.at(1)?;
                    self.i += 2;
                }
            }
            for byte in low..=high {
                if let Some(member) = members.get_mut(usize::from(byte)) {
                    *member = true;
                }
            }
        }
        self.i += 1;
        Some(CharClass { members, negated })
    }

    fn quantifier(&mut self) -> Option<(usize, Option<usize>)> {
        match self.at(0) {
            Some(b'?') => {
                self.i += 1;
                return Some((0, Some(1)));
            }
            Some(b'*') => {
                self.i += 1;
                return Some((0, None));
            }
            Some(b'+') => {
                self.i += 1;
                return Some((1, None));
            }
            Some(b'{') => {}
            _ => return Some((1, Some(1))),
        }
        let close = self.i + self.p[self.i..].iter().position(|&b| b == b'}')?;
        let body = std::str::from_utf8(&self.p[self.i + 1..close]).ok()?;
        self.i = close + 1;
        match body.split_once(',') {
            None => {
                let n = body.parse().ok()?;
                Some((n, Some(n)))
            }
            Some((low, "")) => Some((low.parse().ok()?, None)),
            Some((low, high)) => Some((low.parse().ok()?, Some(high.parse().ok()?))),
        }
    }
}

fn min_length(branches: &[Vec<Item>]) -> usize {
    branches
        .iter()
        .map(|items| {
            items
                .iter()
                .map(|item| match &item.atom {
                    Atom::Group(inner) => item.min * min_length(inner),
                    _ => item.min,
                })
                .sum()
        })
        .min()
        .unwrap_or(0)
}

/// The byte an atom always matches: a literal, or a class with one member.
fn single(atom: &Atom) -> Option<u8> {
    match atom {
        Atom::Char(c) => Some(*c),
        Atom::Class(class) if !class.negated => {
            let mut members = (0u8..128).filter(|&b| class.has(b));
            let first = members.next()?;
            members.next().is_none().then_some(first)
        }
        _ => None,
    }
}

/// The longest run of literal characters every match of `items` contains, if any.
fn literal_of(items: &[Item]) -> Option<String> {
    let mut runs: Vec<String> = Vec::new();
    let mut run = String::new();
    for item in items {
        if let Some(c) = single(&item.atom)
            && item.min >= 1
        {
            run.extend(std::iter::repeat_n(char::from(c), item.min));
            if item.max != Some(item.min) {
                runs.push(std::mem::take(&mut run));
            }
            continue;
        }
        runs.push(std::mem::take(&mut run));
        if let Atom::Group(inner) = &item.atom
            && item.min >= 1
            && let [only] = inner.as_slice()
            && let Some(lit) = literal_of(only)
        {
            runs.push(lit);
        }
    }
    runs.push(run);
    runs.into_iter()
        .filter(|r| !r.is_empty())
        .reduce(|best, r| if r.len() > best.len() { r } else { best })
}

/// A class item followed by a literal character item, both matched at least once, that every match of `items`
/// holds: the last character the class item matches comes right before the literal. A group matched at least once
/// with one alternative holds the pairs of its items.
fn pair_of(items: &[Item]) -> Option<Pair> {
    let adjacent = items.windows(2).find_map(|w| match (&w[0], &w[1]) {
        (
            Item {
                atom: Atom::Class(class),
                min: 1..,
                ..
            },
            Item {
                atom: Atom::Char(literal),
                min: 1..,
                ..
            },
        ) => Some(Pair {
            class: class.clone(),
            literal: *literal,
        }),
        _ => None,
    });
    adjacent.or_else(|| {
        items.iter().find_map(|item| match &item.atom {
            Atom::Group(inner) if item.min >= 1 => match inner.as_slice() {
                [only] => pair_of(only),
                _ => None,
            },
            _ => None,
        })
    })
}

/// The character of an item that is one literal character, matched once.
fn once(item: Option<&Item>) -> Option<u8> {
    match item {
        Some(Item {
            atom: Atom::Char(c),
            min: 1,
            max: Some(1),
            ..
        }) => Some(*c),
        _ => None,
    }
}

fn lead_of(branches: &[Vec<Item>], pattern: &str) -> Option<Lead> {
    let [items] = branches else {
        return None;
    };
    let mut k = 0;
    let mut prefix = String::new();
    while let Some(c) = once(items.get(k)) {
        prefix.push(char::from(c));
        k += 1;
    }
    let repeated = items.get(k)?;
    let anchor_item = items.get(k + 1)?;
    let anchor = once(Some(anchor_item))?;
    let Atom::Class(class) = &repeated.atom else {
        return None;
    };
    if repeated.min < 1 || repeated.max.is_some() || class.has(anchor) {
        return None;
    }
    if !prefix.bytes().all(|b| class.has(b)) {
        return None;
    }
    Some(Lead {
        prefix,
        class: class.clone(),
        anchor,
        rest: pattern.get(anchor_item.start..)?.to_owned(),
    })
}
