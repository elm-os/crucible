"""What a scrub rule's pattern requires of every match, read once when the rules load.

Patterns are in the shared subset of `conformance/scrub-rules.json` (literals, escaped punctuation, bracket classes,
`(?:...)` groups, alternation and greedy quantifiers), which the conformance driver checks before any SDK loads it.
"""

from __future__ import annotations

import math
import re
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from itertools import pairwise


@dataclass(frozen=True)
class _Class:
    members: frozenset[str]
    negated: bool

    def __contains__(self, char: str) -> bool:
        return self.negated != (char in self.members)


@dataclass(frozen=True)
class _Group:
    branches: tuple[tuple[_Item, ...], ...]


@dataclass(frozen=True)
class _Item:
    atom: str | _Class | _Group  # a str is one literal character
    min: int
    max: float
    start: int  # offset in the pattern


@dataclass(frozen=True)
class Lead:
    """A pattern that starts with a literal prefix, then an unbounded repetition of a class that holds the prefix's
    characters, then a literal anchor character outside that class: `[A-Za-z0-9._%+-]+@...` (empty prefix) or
    `eyJ[A-Za-z0-9_-]+\\....`. A match can start only inside the class run that ends at an anchor, and every start
    in one run meets the same anchor, so a matcher needs one anchored attempt per anchor."""

    prefix: str
    in_class: Callable[[str], bool]
    anchor: str
    rest: str
    """The pattern from the anchor on."""


@dataclass(frozen=True)
class Run:
    """The characters every match of an alternative is made of, and the fewest it has: `table` maps each of them to
    one of them, so a text holds such a run when its translation holds `needle`, that one character as many times
    as a match has characters at the fewest (`[0-9 -]` 14 times for the card rule)."""

    table: dict[int, str]
    needle: str


@dataclass(frozen=True)
class Facts:
    min_length: int
    """The fewest characters a match has."""
    literals: tuple[str, ...] | None
    """For each top-level alternative, a literal every match of it contains; None when one has none."""
    lead: Lead | None
    pairs: re.Pattern[str] | None
    """A regular expression that finds, in any text that holds a match, a member of a class followed by a literal
    character that a top-level alternative always holds (`[0-9A-Fa-f]:` for the ipv6 rule); None when an
    alternative holds no such pair."""
    runs: tuple[Run, ...] | None
    """The runs of the top-level alternatives, one per set of characters; None when an alternative can hold any
    character (a negated class)."""


_NO_GUARD = Facts(0, None, None, None, None)


def facts(pattern: str) -> Facts:
    """The facts of `pattern`, or none (no guard) when it does not parse as the subset."""
    try:
        branches = _Parser(pattern).parse()
    except ValueError:
        return _NO_GUARD
    literals = [_literal_of(items) for items in branches]
    pairs = [_pair_of(items) for items in branches]
    return Facts(
        min_length=_min_length(branches),
        literals=tuple(lit for lit in literals if lit is not None) if None not in literals else None,
        lead=_lead_of(branches, pattern),
        pairs=re.compile("|".join(p for p in pairs if p is not None)) if None not in pairs else None,
        runs=_runs_of(branches),
    )


class _Parser:
    def __init__(self, pattern: str) -> None:
        self.pattern = pattern
        self.i = 0

    def _at(self, k: int = 0) -> str:
        return self.pattern[self.i + k] if self.i + k < len(self.pattern) else ""

    def _fail(self) -> ValueError:
        return ValueError(f"pattern {self.pattern!r} is outside the scrub subset at {self.i}")

    def _unescape(self, c: str) -> str:
        if not c:
            raise self._fail()
        return {"n": "\n", "t": "\t"}.get(c, c)

    def parse(self) -> tuple[tuple[_Item, ...], ...]:
        branches = self._alternation()
        if self.i != len(self.pattern):
            raise self._fail()
        return branches

    def _alternation(self) -> tuple[tuple[_Item, ...], ...]:
        branches: list[list[_Item]] = [[]]
        while self.i < len(self.pattern) and self._at() != ")":
            if self._at() == "|":
                self.i += 1
                branches.append([])
                continue
            start = self.i
            atom: str | _Class | _Group
            if self.pattern.startswith("(?:", self.i):
                self.i += 3
                inner = self._alternation()
                if self._at() != ")":
                    raise self._fail()
                self.i += 1
                atom = _Group(inner)
            elif self._at() == "[":
                atom = self._class()
            elif self._at() == "\\":
                atom = self._unescape(self._at(1))
                self.i += 2
            else:
                atom = self._at()
                self.i += 1
            low, high = self._quantifier()
            branches[-1].append(_Item(atom, low, high, start))
        return tuple(tuple(b) for b in branches)

    def _class(self) -> _Class:
        self.i += 1
        negated = self._at() == "^"
        if negated:
            self.i += 1
        members: set[str] = set()
        first = self.i
        while self._at() != "]" or self.i == first:
            low = self._at()
            if not low:
                raise self._fail()
            if low == "\\":
                self.i += 1
                low = self._unescape(self._at())
            self.i += 1
            high = low
            if self._at() == "-" and self._at(1) not in ("]", ""):
                if self._at(1) == "\\":
                    high = self._unescape(self._at(2))
                    self.i += 3
                else:
                    high = self._at(1)
                    self.i += 2
            members.update(chr(code) for code in range(ord(low), min(ord(high), 127) + 1))
        self.i += 1
        return _Class(frozenset(members), negated)

    def _quantifier(self) -> tuple[int, float]:
        c = self._at()
        if c in ("?", "*", "+"):
            self.i += 1
            return {"?": (0, 1.0), "*": (0, math.inf), "+": (1, math.inf)}[c]
        if c != "{":
            return 1, 1.0
        close = self.pattern.find("}", self.i)
        if close < 0:
            raise self._fail()
        low, _, high = self.pattern[self.i + 1 : close].partition(",")
        bounded = "," not in self.pattern[self.i + 1 : close]
        self.i = close + 1
        if not low.isdigit() or not (bounded or high == "" or high.isdigit()):
            raise self._fail()
        if bounded:
            return int(low), float(low)
        return int(low), math.inf if high == "" else float(high)


def _min_length(branches: Sequence[Sequence[_Item]]) -> int:
    return min(
        sum(item.min * (_min_length(item.atom.branches) if isinstance(item.atom, _Group) else 1) for item in items)
        for items in branches
    )


def _single(atom: str | _Class | _Group) -> str | None:
    """The character an atom always matches: a literal, or a class with one member."""
    if isinstance(atom, str):
        return atom
    if isinstance(atom, _Class) and not atom.negated and len(atom.members) == 1:
        return next(iter(atom.members))
    return None


def _literal_of(items: Sequence[_Item]) -> str | None:
    """The longest run of literal characters every match of `items` contains, if any."""
    runs: list[str] = []
    run = ""
    for item in items:
        char = _single(item.atom)
        if char is not None and item.min >= 1:
            run += char * item.min
            if item.max != item.min:
                runs.append(run)
                run = ""
            continue
        if run:
            runs.append(run)
        run = ""
        if isinstance(item.atom, _Group) and item.min >= 1 and len(item.atom.branches) == 1:
            inner = _literal_of(item.atom.branches[0])
            if inner is not None:
                runs.append(inner)
    if run:
        runs.append(run)
    best: str | None = None
    for r in runs:
        if r and (best is None or len(r) > len(best)):
            best = r
    return best


def _escaped(char: str) -> str:
    """The source of a pattern that finds one character, written as an escape so that no character is special."""
    code = ord(char)
    return f"\\x{code:02x}" if code < 256 else f"\\u{code:04x}"


def _pair_of(items: Sequence[_Item]) -> str | None:
    """A class item followed by a literal character item, both matched at least once, that every match of `items`
    holds, as the source of a pattern that finds the pair: the last character the class item matches comes right
    before the literal. A group matched at least once with one alternative holds the pairs of its items."""
    for first, second in pairwise(items):
        if isinstance(first.atom, _Class) and first.min >= 1 and isinstance(second.atom, str) and second.min >= 1:
            members = "".join(_escaped(c) for c in sorted(first.atom.members))
            return f"[{'^' if first.atom.negated else ''}{members}]{_escaped(second.atom)}"
    for item in items:
        if isinstance(item.atom, _Group) and item.min >= 1 and len(item.atom.branches) == 1:
            inner = _pair_of(item.atom.branches[0])
            if inner is not None:
                return inner
    return None


def _chars_of(items: Sequence[_Item]) -> frozenset[str] | None:
    """Every character a match of `items` can hold; None when a negated class lets it hold any other."""
    chars: set[str] = set()
    for item in items:
        atom = item.atom
        if isinstance(atom, str):
            chars.add(atom)
        elif isinstance(atom, _Class):
            if atom.negated:
                return None
            chars |= atom.members
        else:
            for branch in atom.branches:
                inner = _chars_of(branch)
                if inner is None:
                    return None
                chars |= inner
    return frozenset(chars)


def _runs_of(branches: Sequence[Sequence[_Item]]) -> tuple[Run, ...] | None:
    """The run of each alternative: every match of it is a run of at least its fewest characters, all of them
    characters it can hold. Alternatives made of the same characters share the shorter run."""
    fewest: dict[frozenset[str], int] = {}
    for items in branches:
        chars = _chars_of(items)
        if not chars:
            return None
        length = _min_length((items,))
        fewest[chars] = min(length, fewest.get(chars, length))
    runs = []
    for chars, length in fewest.items():
        marker = min(chars)
        runs.append(Run({ord(c): marker for c in chars}, marker * length))
    return tuple(runs)


def _once(item: _Item | None) -> str | None:
    """The character of an item that is one literal character, matched once."""
    if item is not None and isinstance(item.atom, str) and item.min == 1 and item.max == 1:
        return item.atom
    return None


def _lead_of(branches: Sequence[Sequence[_Item]], pattern: str) -> Lead | None:
    if len(branches) != 1:
        return None
    items = branches[0]
    k = 0
    prefix = ""
    while (char := _once(items[k] if k < len(items) else None)) is not None:
        prefix += char
        k += 1
    if k + 1 >= len(items):
        return None
    repeated, anchor_item = items[k], items[k + 1]
    anchor = _once(anchor_item)
    cls = repeated.atom
    if not isinstance(cls, _Class) or repeated.min < 1 or repeated.max != math.inf or anchor is None:
        return None
    if anchor in cls or any(c not in cls for c in prefix):
        return None
    return Lead(prefix, cls.__contains__, anchor, pattern[anchor_item.start :])
