"""Content scrubbing with the shared rules (contract 9.10).

The rules are data (`scrub_rules.json`). Text rules are regular expressions in the RE2 syntax subset; `key_names`
replace the whole value under a matching JSON object key. Captured content is encoded under a byte budget, so the
scrubbing work is bounded by the cap however large the input is.
"""

from __future__ import annotations

import json
import re
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from importlib import resources
from typing import Any

# Constructs outside the RE2 subset. A backtracking matcher can take exponential time on them, so a rule that uses
# one is refused at load.
_NON_RE2 = re.compile(r"\(\?[=!>]|\(\?<[=!]|\(\?P=|\\[1-9]|[*+?}]\+")

# Keys whose values are never emitted, whatever the rules file says (contract 3.5).
_ALWAYS_DENIED_KEYS = frozenset({"authorization", "proxyauthorization", "cookie", "setcookie", "requeststate"})

# Nesting deeper than this is cut, like the byte cap.
_MAX_DEPTH = 64
# A string is cut this many characters past the remaining budget before it is scrubbed, so a secret that straddles
# the cap is still matched whole.
_MARGIN = 1024


def _normalize_key(key: str) -> str:
    return key.lower().replace("-", "").replace("_", "").replace(".", "")


def _luhn(text: str) -> bool:
    total = 0
    for i, ch in enumerate(reversed([c for c in text if "0" <= c <= "9"])):
        digit = ord(ch) - 48
        if i % 2:
            digit = digit * 2 - 9 if digit > 4 else digit * 2
        total += digit
    return total % 10 == 0


@dataclass(frozen=True)
class _Rule:
    name: str
    regex: re.Pattern[str]
    replacement: str
    luhn: bool

    def apply(self, text: str) -> str:
        if not self.luhn:
            return self.regex.sub(self.replacement, text)
        return self.regex.sub(lambda m: self.replacement if _luhn(m.group()) else m.group(), text)


class Scrubber:
    """Applies one rules document to text and to JSON values."""

    def __init__(self, document: Mapping[str, Any]) -> None:
        if document.get("schema") != "crucible.scrub-rules/v1":
            raise ValueError("scrub rules: 'schema' must be 'crucible.scrub-rules/v1'")
        rules: list[_Rule] = []
        for entry in document.get("patterns", ()):
            name, pattern = str(entry["name"]), str(entry["pattern"])
            if _NON_RE2.search(pattern):
                raise ValueError(
                    f"scrub rule {name!r}: lookaround, backreferences and possessive quantifiers are not allowed"
                )
            check = entry.get("check")
            if check not in (None, "luhn"):
                raise ValueError(f"scrub rule {name!r}: unknown check {check!r}; use 'luhn' or leave it out")
            rules.append(_Rule(name, re.compile(pattern, re.ASCII), str(entry["replacement"]), check == "luhn"))
        self._rules = tuple(rules)
        names: Sequence[str] = document.get("key_names", ())
        self._denied = _ALWAYS_DENIED_KEYS | {_normalize_key(str(n)) for n in names}
        self.key_replacement = str(document.get("key_replacement", "[REDACTED]"))

    @classmethod
    def builtin(cls) -> Scrubber:
        """The rules shipped in this package."""
        text = resources.files("crucible_sdk").joinpath("scrub_rules.json").read_text(encoding="utf-8")
        return cls(json.loads(text))

    def text(self, value: str) -> str:
        """`value` with every rule applied in order."""
        for rule in self._rules:
            value = rule.apply(value)
        return value

    def is_denied_key(self, key: str) -> bool:
        """Whether the value under this JSON object key is replaced whole."""
        return _normalize_key(key) in self._denied

    def capped_json(self, value: object, max_bytes: int) -> tuple[str, bool]:
        """`value` as scrubbed JSON text of at most `max_bytes` UTF-8 bytes, and whether it was cut.

        A cut value is a prefix of the full encoding, so it is no longer valid JSON (contract 3.1).
        """
        encoder = _CappedJson(self, max_bytes)
        encoder.value(value, 0)
        return b"".join(encoder.parts).decode("utf-8", "ignore"), encoder.cut


class _CappedJson:
    def __init__(self, scrubber: Scrubber, max_bytes: int) -> None:
        self._scrubber = scrubber
        self._left = max_bytes
        self.parts: list[bytes] = []
        self.cut = False

    def _emit(self, text: str) -> bool:
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

    def _string(self, value: str) -> bool:
        raw = value[: self._left + _MARGIN]
        text = json.dumps(self._scrubber.text(raw), ensure_ascii=False)
        if len(raw) < len(value):
            # Cut before scrubbing: emit it without the closing quote, as a prefix of the full encoding.
            self._emit(text[:-1])
            self.cut = True
            return False
        return self._emit(text)

    def _object(self, value: Mapping[Any, Any], depth: int) -> bool:
        if not self._emit("{"):
            return False
        for i, (key, item) in enumerate(value.items()):
            name = key if isinstance(key, str) else str(key)
            if i and not self._emit(","):
                return False
            if not self._emit(json.dumps(self._scrubber.text(name), ensure_ascii=False) + ":"):
                return False
            if self._scrubber.is_denied_key(name):
                if not self._emit(json.dumps(self._scrubber.key_replacement)):
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
