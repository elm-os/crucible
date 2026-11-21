"""Content scrubbing with the shared rules (contract 9.10).

`scrub_rules.json` is a copy of the suite's `conformance/scrub-rules.json`: regular expressions in a subset that every
SDK's engine matches identically, applied in file order. Captured content is encoded under a byte budget, so the
scrubbing work is bounded by the cap however large the input is.
"""

from __future__ import annotations

import json
import json.encoder as json_encoder
import math
import re
from collections.abc import Callable, Iterable, Mapping
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
        without the rule's required literal, without the class and literal pair every match holds, or without a run
        of the characters a match is made of as long as a match, is left alone, and a lead pattern is tried once per
        anchor."""
        f = self.facts
        if len(text) < f.min_length or (f.literals is not None and not any(lit in text for lit in f.literals)):
            return text
        if f.pairs is not None:
            # One search for the pair, which a matcher takes at most one step per start for.
            if _matcher_hook is not None:
                _matcher_hook(self.name, len(text))
            if f.pairs.search(text) is None:
                return text
        # A translation and a substring search, without the matcher.
        if f.runs is not None and not any(run.needle in text.translate(run.table) for run in f.runs):
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

    def leaves_alone(self, texts: list[str]) -> bool:
        """Whether the rules leave every one of `texts` unchanged, checked over all of them at once.

        The texts are joined by a NUL, which no rule but `pem` matches, so a rule that changes one of them changes the
        joined text too; a match across a NUL can only make the answer no. A rule whose matches are longer than the
        longest text cannot change any of them and is skipped.
        """
        longest = max(map(len, texts), default=0)
        if longest < self._shortest:
            return True
        joined = value = "\0".join(texts)
        for rule in self.rules:
            if rule.facts.min_length <= longest:
                value = rule.apply(value)
        return value == joined

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
        encoder.value(value, 0, None)
        note_work(encoder.serialized, encoder.scrubbed)
        return "".join(encoder.parts), encoder.cut


@dataclass(frozen=True)
class LazyObject:
    """A JSON object whose members are produced while it is encoded, so a capped encoding reads no more of them than
    the cap holds."""

    members: Iterable[tuple[str, object]]


@dataclass(frozen=True)
class LazyArray:
    """A JSON array whose items are produced while it is encoded."""

    items: Iterable[object]


@dataclass(frozen=True)
class JsonData:
    """Builtin data (dicts, lists, tuples and scalars) to encode as `convert` has it, converted while it is encoded.

    The encoder applies the rules of `model_dump(mode="json")` to the builtin types itself: strings, integers,
    booleans and None stay, a float that is not finite becomes null, and a key that is not a string goes through
    `key`. Any other value goes through `convert`, whose result is encoded as it is. So a capped encoding converts
    no more of the data than the cap holds, without wrapping each container it reads.
    """

    value: object
    convert: Callable[[object], object]
    key: Callable[[object], str]


_REDACTED_JSON = json.dumps(_REDACTED)
_encode_string = json_encoder.encode_basestring
_int_text = int.__repr__
_float_repr = float.__repr__
_isfinite = math.isfinite
# One call writes a whole run of flat objects, as the item-by-item walk would write them.
_encode_run = json.JSONEncoder(ensure_ascii=False, separators=(",", ":")).encode
# A run of flat objects is at most this many objects, with strings of at most this many characters each and this
# many in all, so that a run which does not fit costs little.
_RUN_ITEMS = 64
_RUN_KEYS = 16
_RUN_STRING = 128
_RUN_CHARS = 2048


def _scalar(value: object) -> str:
    """`json.dumps` of None, a boolean or a number."""
    if value is None:
        return "null"
    if value is True:
        return "true"
    if value is False:
        return "false"
    if isinstance(value, int):
        return _int_text(value)
    number = float(value)  # type: ignore[arg-type]
    if number != number:
        return "NaN"
    if number == math.inf:
        return "Infinity"
    if number == -math.inf:
        return "-Infinity"
    return _float_repr(number)


class _CappedJson:
    """`value` as scrubbed JSON text within a byte budget.

    It writes what a walk token by token writes (`tests/capture_reference.py` holds that walk, and property tests
    hold this class to it), with two shortcuts. Text is kept as `str` and measured without encoding when it is
    ASCII. And a run of flat objects in an array (records: string, number, boolean and null values under string
    keys) is written by the standard library's encoder in one call, when no text in the run would be scrubbed and the
    whole run fits the budget, the only case in which the token walk writes the same text without a cut.
    """

    def __init__(self, scrubber: Scrubber, max_bytes: int) -> None:
        self._scrubber = scrubber
        self._scrub = scrubber.text
        self.left = max_bytes
        self.parts: list[str] = []
        self.cut = False
        self.serialized = 0
        self.scrubbed = 0
        # Per tuple of keys: their length in all when an object with these keys may stand in a run, else -1.
        self._shapes: dict[tuple[object, ...], int] = {}

    def emit(self, text: str) -> bool:
        self.serialized += len(text)
        if text.isascii():
            if len(text) <= self.left:
                self.parts.append(text)
                self.left -= len(text)
                return True
            self.parts.append(text[: self.left])
        else:
            data = text.encode("utf-8", "replace")
            if len(data) <= self.left:
                self.parts.append(data.decode("utf-8"))
                self.left -= len(data)
                return True
            # A character the budget cuts is dropped whole.
            self.parts.append(data[: self.left].decode("utf-8", "ignore"))
        self.left = 0
        self.cut = True
        return False

    def value(self, value: object, depth: int, data: JsonData | None) -> bool:
        if data is not None:
            kind = type(value)
            if kind is float:
                if not math.isfinite(value):  # type: ignore[arg-type]
                    value = None
            elif kind is dict or kind is list or kind is tuple:
                if depth >= _MAX_DEPTH:
                    self.cut = True
                    return False
                if kind is dict:
                    return self.mapping(value.items(), depth, data)  # type: ignore[attr-defined]
                return self.array(value, depth, data)  # type: ignore[arg-type]
            elif not (kind is str or kind is int or kind is bool or value is None):
                # Not a builtin type: converted, and its result encoded as it is.
                value, data = data.convert(value), None
        if value is None or isinstance(value, bool | int | float):
            return self.emit(_scalar(value))
        if isinstance(value, str):
            return self.string(value)
        if isinstance(value, JsonData):
            return self.value(value.value, depth, value)
        if depth >= _MAX_DEPTH:
            self.cut = True
            return False
        if isinstance(value, Mapping):
            return self.mapping(value.items(), depth, None)
        if isinstance(value, LazyObject):
            return self.mapping(value.members, depth, None)
        if isinstance(value, list | tuple):
            return self.array(value, depth, None)
        if isinstance(value, LazyArray):
            return self.array(value.items, depth, None)
        return self.string(str(value))

    def string(self, value: str, suffix: str = "") -> bool:
        """Emits a string (an object key with `suffix` ":"), scrubbing no more of it than the budget can hold."""
        raw = value[: self.left + _MARGIN]
        self.scrubbed += len(raw)
        text = _encode_string(self._scrub(raw))
        if len(raw) < len(value):
            # Cut before scrubbing: emit it without the closing quote, as a prefix of the full encoding.
            self.emit(text[:-1])
            self.cut = True
            return False
        return self.emit(text + suffix)

    def mapping(self, members: Iterable[tuple[Any, Any]], depth: int, data: JsonData | None) -> bool:
        if not self.emit("{"):
            return False
        first = True
        for key, item in members:
            if data is not None and type(key) is not str:
                key = data.key(key)
            name = key if isinstance(key, str) else str(key)
            if not first and not self.emit(","):
                return False
            first = False
            if not self.string(name, ":"):
                return False
            if Scrubber.is_denied_key(name):
                if not self.emit(_REDACTED_JSON):
                    return False
            elif not self.value(item, depth + 1, data):
                return False
        return self.emit("}")

    def array(self, items: Iterable[Any], depth: int, data: JsonData | None) -> bool:
        if not self.emit("["):
            return False
        first = True
        run: list[object] = []
        strings: list[str] = []
        chars = 0
        for item in items:
            mark = len(strings)
            added = self._flat(item, depth + 1, data, strings)
            if added >= 0:
                run.append(item)
                chars += added
                if len(run) < _RUN_ITEMS and chars < _RUN_CHARS:
                    continue
            else:
                del strings[mark:]
            if run:
                if not self._run(run, strings, first, depth, data):
                    return False
                first = False
                run, strings, chars = [], [], 0
            if added < 0:
                if not first and not self.emit(","):
                    return False
                first = False
                if not self.value(item, depth + 1, data):
                    return False
        if run and not self._run(run, strings, first, depth, data):
            return False
        return self.emit("]")

    def _flat(self, item: object, depth: int, data: JsonData | None, strings: list[str]) -> int:
        """Adds the string values of `item` to `strings` and returns their length and its keys', when `item` may stand
        in a run; else returns -1."""
        if not isinstance(item, dict) or type(item) is not dict or len(item) > _RUN_KEYS or depth >= _MAX_DEPTH:
            return -1
        keys = tuple(item)
        shape = self._shapes.get(keys)
        if shape is None:
            usable = all(self._key(key) for key in keys)
            shape = self._shapes[keys] = sum(map(len, keys)) if usable else -1
        if shape < 0:
            return -1
        # The keys count toward the run's size like the strings, since a run that does not fit is written again.
        added = shape
        for value in item.values():
            kind = type(value)
            if kind is str:
                if len(value) > _RUN_STRING:
                    return -1
                strings.append(value)
                added += len(value)
            elif kind is float:
                # A float that is not finite is written as JSON has it, unless conversion makes it null.
                if data is not None and not _isfinite(value):
                    return -1
            elif kind is not int and kind is not bool and value is not None:
                return -1
        return added

    def _key(self, key: object) -> bool:
        """Whether a key may stand in a run: a string that scrubbing leaves alone and that is not denied."""
        if type(key) is not str:
            return False
        self.scrubbed += len(key)
        return len(key) <= _RUN_STRING and not Scrubber.is_denied_key(key) and self._scrub(key) == key

    def _run(self, run: list[object], strings: list[str], first: bool, depth: int, data: JsonData | None) -> bool:
        """Writes a run of flat objects in one call when the rules leave all its strings alone and it fits; else item
        by item."""
        self.scrubbed += sum(map(len, strings)) + len(strings)
        if self._scrubber.leaves_alone(strings):
            return self._clean_run(run, first, depth, data)
        return self._items(run, first, depth, data)

    def _clean_run(self, run: list[object], first: bool, depth: int, data: JsonData | None) -> bool:
        """Writes a run that scrubbing leaves alone in one call when it fits; else its halves, down to the one object
        that does not fit, which goes item by item and meets the cut."""
        text = _encode_run(run)[1:-1]
        # Counted whether it fits or not: a run that does not fit is serialized again, in halves.
        self.serialized += len(text)
        if not first:
            text = "," + text
        if text.isascii():
            size = len(text)
        else:
            encoded = text.encode("utf-8", "replace")
            size = len(encoded)
            text = encoded.decode("utf-8")
        if size <= self.left:
            self.parts.append(text)
            self.left -= size
            return True
        if len(run) == 1:
            return self._items(run, first, depth, data)
        half = len(run) // 2
        return self._clean_run(run[:half], first, depth, data) and self._clean_run(run[half:], False, depth, data)

    def _items(self, items: list[object], first: bool, depth: int, data: JsonData | None) -> bool:
        for item in items:
            if not first and not self.emit(","):
                return False
            first = False
            if not self.value(item, depth + 1, data):
                return False
        return True
