"""Content scrubbing with the shared rules (contract 9.10).

`scrub_rules.json` is a copy of the suite's `conformance/scrub-rules.json`: regular expressions in a subset that every
SDK's engine matches identically, applied in file order. Captured content is encoded under a byte budget, so the
scrubbing work is bounded by the cap however large the input is.
"""

from __future__ import annotations

import json
import re
from collections.abc import Callable, Mapping, Sequence
from dataclasses import dataclass
from importlib import resources
from typing import Any

from crucible_sdk._pattern import Facts, Lead, facts

SCHEMA = "crucible.conformance.scrub/v1"

WorkHook = Callable[[int, int], None]
"""Called with the work one step of capturing a field cost, in characters: the JSON text serialized for it before
any cut, and the text handed to the scrubber."""

_work_hook: WorkHook | None = None


def set_work_hook(hook: WorkHook | None) -> None:
    """Installs, or with None removes, the hook the capture-overhead scenario reads its work bound from."""
    global _work_hook
    _work_hook = hook


def note_work(serialized: int, scrubbed: int) -> None:
    """Reports work to the hook, if one is installed."""
    if _work_hook is not None:
        _work_hook(serialized, scrubbed)


def work_hooked() -> bool:
    """Whether a work hook is installed, so that a caller measures only then."""
    return _work_hook is not None


MatcherHook = Callable[[str, int], None]
"""Called once per matcher call with the rule's name and the call's steps (see `_search_steps`)."""

_matcher_hook: MatcherHook | None = None


def set_matcher_hook(hook: MatcherHook | None) -> None:
    """Installs, or with None removes, the hook the scrubbing work checks read matcher steps from."""
    global _matcher_hook
    _matcher_hook = hook


def _search_steps(lead: Lead | None, text: str, start: int, stop: int) -> int:
    """The steps of an unanchored search that tried the starts from `start` up to `stop`, as a backtracking matcher
    takes them at the least: one per start, and from a start where a lead pattern's prefix matches, the prefix and
    the whole class run after it, which the greedy repetition consumes before it backtracks."""
    if lead is None:
        return stop - start
    p = len(lead.prefix)
    end = min(len(text), stop + p)
    while end < len(text) and lead.in_class(text[end]):
        end += 1
    runs = [0] * (end - start + 1)
    for k in range(end - 1, start - 1, -1):
        runs[k - start] = runs[k - start + 1] + 1 if lead.in_class(text[k]) else 0
    steps = 0
    for s in range(start, stop):
        steps += 1
        if text.startswith(lead.prefix, s):
            steps += p + (runs[s + p - start] if s + p - start < len(runs) else 0)
    return steps


# Values under these JSON object keys are never emitted (contract 3.5); keys compare lowercased, without '-', '_'
# and '.'.
_DENIED_KEYS = frozenset({"authorization", "proxyauthorization", "cookie", "setcookie", "requeststate"})
_REDACTED = "[redacted]"

# Nesting deeper than this is cut, like the byte cap.
_MAX_DEPTH = 64
# A string is cut this many characters past what is still needed before it is scrubbed, so a secret that straddles
# the cut (a PEM key, which must end in its END line to match) is still matched whole.
_MARGIN = 4096


def _ascii_alnum(c: str) -> bool:
    return c.isascii() and c.isalnum()


def _normalize_key(key: str) -> str:
    return key.lower().replace("-", "").replace("_", "").replace(".", "")


def _luhn(text: str) -> bool:
    digits = [c for c in text if "0" <= c <= "9"]
    if len(digits) < 2:
        return False
    total = 0
    for i, ch in enumerate(reversed(digits)):
        digit = ord(ch) - 48
        if i % 2:
            digit = digit * 2 - 9 if digit > 4 else digit * 2
        total += digit
    return total % 10 == 0


def outside_subset(pattern: str) -> str | None:
    """What in `pattern` the shared subset does not allow, or None.

    The subset (from the rules file): literals, escaped punctuation, bracket classes, `(?:...)` groups, alternation
    and greedy quantifiers; no `.`, class escapes, anchors, inline flags, lookaround, backreferences, other groups,
    lazy or possessive quantifiers, or an unbounded quantifier on a group that holds one.
    """
    printable = next((c for c in pattern if not (" " <= c <= "~" or c == "	")), None)
    if printable is not None:
        return f"{printable!r}, which is not printable ASCII,"
    groups: list[bool] = []  # per open group: whether it holds an unbounded quantifier
    closed_unbounded = False  # the group that just closed held one
    i = 0
    while i < len(pattern):
        c = pattern[i]
        if c == "\\":
            escaped = pattern[i + 1 : i + 2]
            if escaped.isalnum():
                return f"the escape \\{escaped}"
            i += 2
        elif c == "[":
            end = pattern.find("]", i + 2 if pattern[i + 1 : i + 2] in "^]" else i + 1)
            if end < 0:
                return "an unclosed bracket class"
            body = pattern[i + 1 : end]
            if re.search(r"\\[A-Za-z0-9]|\[:", body):
                return "a class escape or POSIX class inside brackets"
            i = end + 1
        elif c == ".":
            return "'.'"
        elif c in "^$":
            return f"the anchor {c!r}"
        elif c == "(":
            if pattern[i + 1 : i + 3] != "?:":
                return "a group other than (?:...)"
            groups.append(False)
            i += 3
            continue
        elif c == ")":
            if not groups:
                return "an unbalanced ')'"
            closed_unbounded = groups.pop()
            if closed_unbounded and groups:
                groups[-1] = True
            i += 1
            continue
        elif c in "*+?{":
            end = pattern.find("}", i) if c == "{" else i
            if end < 0:
                return "an unclosed '{'"
            unbounded = c in "*+" or (c == "{" and pattern[end - 1] == ",")
            if pattern[end + 1 : end + 2] in ("?", "+"):
                return "a lazy or possessive quantifier"
            if unbounded and pattern[i - 1 : i] == ")" and closed_unbounded:
                return "an unbounded quantifier on a group that holds one"
            if unbounded and groups:
                groups[-1] = True
            i = end + 1
            continue
        else:
            i += 1
        closed_unbounded = False
    return None


def _attempt_steps(lead: Lead, text: str, at: int, match: re.Match[str] | None) -> int:
    """The characters an anchored attempt of a lead pattern's rest can read: its match, or the anchor, the class run
    after it and the character that ends the run."""
    if match is not None:
        return match.end() - at
    end = at + 1
    while end < len(text) and lead.in_class(text[end]):
        end += 1
    return end - at + 1


@dataclass(frozen=True)
class _Rule:
    name: str
    regex: re.Pattern[str]
    replacement: str
    luhn: bool
    facts: Facts
    rest: re.Pattern[str] | None
    """For a lead pattern without a validator: the pattern from its anchor on, tried at one position."""

    def _searched(self, text: str, start: int, stop: int) -> None:
        if _matcher_hook is not None:
            _matcher_hook(self.name, _search_steps(self.facts.lead, text, start, stop))

    def apply(self, text: str) -> str:
        """The rule behind guards that take time linear in the text and change nothing: text shorter than any match,
        or without the rule's required literal, is left alone, and a lead pattern is tried once per anchor."""
        f = self.facts
        if len(text) < f.min_length or (f.literals is not None and not any(lit in text for lit in f.literals)):
            return text
        if f.lead is not None and self.rest is not None:
            return self._apply_lead(f.lead, self.rest, text)
        return self.apply_plain(text)

    def _apply_lead(self, lead: Lead, rest: re.Pattern[str], text: str) -> str:
        """The matches of a lead pattern, found from its anchors. A match ends its class run at an anchor, and every
        start in that run meets the same anchor, so only the run's leftmost possible start is a candidate and the
        rest of the pattern is tried once, anchored there. Each position is scanned a bounded number of times."""
        out: list[str] = []
        copied = floor = 0
        at = text.find(lead.anchor)
        while at != -1:
            start = at
            while start > floor and lead.in_class(text[start - 1]):
                start -= 1
            if not lead.prefix:
                begin = start if start < at else -1
            else:
                k = text.find(lead.prefix, start, at)
                begin = k if k != -1 and k + len(lead.prefix) < at else -1
            if begin != -1:
                match = rest.match(text, at)
                if _matcher_hook is not None:
                    _matcher_hook(self.name, _attempt_steps(lead, text, at, match))
                if match is not None:
                    out += (text[copied:begin], self.replacement)
                    copied = floor = match.end()
                    at = text.find(lead.anchor, floor)
                    continue
            at = text.find(lead.anchor, at + 1)
        out.append(text[copied:])
        return "".join(out)

    def apply_plain(self, text: str) -> str:
        """The rule without its guards: the reference the differential tests hold `apply` to."""
        if not self.luhn:
            self._searched(text, 0, len(text))
            return self.regex.sub(self.replacement, text)
        # After a match the validator rejects, the search resumes after the match's first character that is neither
        # an ASCII letter nor a digit (after the match when it has none), so a rejected run of digit groups cannot
        # hide an accepted one that starts at a later group.
        out: list[str] = []
        copied = at = 0
        while (match := self.regex.search(text, at)) is not None:
            self._searched(text, at, match.start() + 1)
            if not _luhn(match.group()):
                separator = next((i for i, c in enumerate(match.group()) if not _ascii_alnum(c)), None)
                at = match.end() if separator is None else match.start() + separator + 1
                continue
            out += (text[copied : match.start()], self.replacement)
            copied = match.end()
            at = match.end() if match.end() > match.start() else match.end() + 1
            if at >= len(text):
                break
        else:
            self._searched(text, at, len(text))
        out.append(text[copied:])
        return "".join(out)


class Scrubber:
    """Applies one rules document to text and to JSON values."""

    def __init__(self, document: Mapping[str, Any]) -> None:
        if document.get("schema") != SCHEMA:
            raise ValueError(f"scrub rules: 'schema' must be {SCHEMA!r}")
        rules: list[_Rule] = []
        for entry in document.get("rules", ()):
            name, pattern = str(entry["name"]), str(entry["pattern"])
            problem = outside_subset(pattern)
            if problem is not None:
                raise ValueError(f"scrub rule {name!r}: {problem} is outside the shared pattern subset")
            validator = entry.get("validator")
            if validator not in (None, "luhn"):
                raise ValueError(f"scrub rule {name!r}: unknown validator {validator!r}; use 'luhn' or leave it out")
            found = facts(pattern)
            rest = None if found.lead is None or validator == "luhn" else re.compile(found.lead.rest)
            rules.append(_Rule(name, re.compile(pattern), str(entry["replacement"]), validator == "luhn", found, rest))
        self.rules = tuple(rules)
        # No rule matches text shorter than this.
        self._shortest = min((rule.facts.min_length for rule in self.rules), default=0)

    @classmethod
    def builtin(cls) -> Scrubber:
        """The rules shipped in this package."""
        text = resources.files("crucible_sdk").joinpath("scrub_rules.json").read_text(encoding="utf-8")
        return cls(json.loads(text))

    def text(self, value: str) -> str:
        """`value` with every rule applied in order."""
        if len(value) < self._shortest:
            return value
        for rule in self.rules:
            value = rule.apply(value)
        return value

    def text_plain(self, value: str) -> str:
        """`value` with every rule applied in order without its guards, for the differential tests."""
        for rule in self.rules:
            value = rule.apply_plain(value)
        return value

    def bounded_text(self, value: str, max_chars: int) -> str:
        """`value` cut to `max_chars` characters and scrubbed, matching across the cut first."""
        if len(value) <= max_chars:
            return self.text(value)
        return self.text(value[: max_chars + _MARGIN])[:max_chars]

    @staticmethod
    def is_denied_key(key: str) -> bool:
        """Whether the value under this JSON object key is replaced whole."""
        return _normalize_key(key) in _DENIED_KEYS

    def capped_json(self, value: object, max_bytes: int) -> tuple[str, bool]:
        """`value` as scrubbed JSON text of at most `max_bytes` UTF-8 bytes, and whether it was cut.

        A cut value is a prefix of the full encoding, so it is no longer valid JSON (contract 3.1).
        """
        encoder = _CappedJson(self, max_bytes)
        encoder.value(value, 0)
        note_work(encoder.serialized, encoder.scrubbed)
        return b"".join(encoder.parts).decode("utf-8", "ignore"), encoder.cut


class _CappedJson:
    def __init__(self, scrubber: Scrubber, max_bytes: int) -> None:
        self._scrubber = scrubber
        self._left = max_bytes
        self.parts: list[bytes] = []
        self.cut = False
        self.serialized = 0
        self.scrubbed = 0

    def _emit(self, text: str) -> bool:
        self.serialized += len(text)
        data = text.encode("utf-8", "replace")
        if len(data) <= self._left:
            self.parts.append(data)
            self._left -= len(data)
            return True
        self.parts.append(data[: self._left])
        self._left = 0
        self.cut = True
        return False

    def value(self, value: object, depth: int) -> bool:
        if value is None or isinstance(value, bool | int | float):
            return self._emit(json.dumps(value))
        if isinstance(value, str):
            return self._string(value)
        if depth >= _MAX_DEPTH:
            self.cut = True
            return False
        if isinstance(value, Mapping):
            return self._object(value, depth)
        if isinstance(value, list | tuple):
            return self._array(value, depth)
        return self._string(str(value))

    def _string(self, value: str, suffix: str = "") -> bool:
        """Emits a string (an object key with `suffix` ":"), scrubbing no more of it than the budget can hold."""
        raw = value[: self._left + _MARGIN]
        self.scrubbed += len(raw)
        text = json.dumps(self._scrubber.text(raw), ensure_ascii=False)
        if len(raw) < len(value):
            # Cut before scrubbing: emit it without the closing quote, as a prefix of the full encoding.
            self._emit(text[:-1])
            self.cut = True
            return False
        return self._emit(text + suffix)

    def _object(self, value: Mapping[Any, Any], depth: int) -> bool:
        if not self._emit("{"):
            return False
        for i, (key, item) in enumerate(value.items()):
            name = key if isinstance(key, str) else str(key)
            if i and not self._emit(","):
                return False
            if not self._string(name, ":"):
                return False
            if Scrubber.is_denied_key(name):
                if not self._emit(json.dumps(_REDACTED)):
                    return False
            elif not self.value(item, depth + 1):
                return False
        return self._emit("}")

    def _array(self, value: Sequence[Any], depth: int) -> bool:
        if not self._emit("["):
            return False
        for i, item in enumerate(value):
            if i and not self._emit(","):
                return False
            if not self.value(item, depth + 1):
                return False
        return self._emit("]")
