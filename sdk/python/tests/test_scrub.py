"""The shared scrub rules and the capped JSON encoder (contract 9.10)."""

from __future__ import annotations

import base64
import json
import re
import time
from importlib import resources
from pathlib import Path
from typing import Any

import pytest

from crucible_sdk import _scrub
from crucible_sdk._pattern import facts
from crucible_sdk._scrub import SCHEMA, Scrubber

RULES: dict[str, Any] = json.loads(resources.files("crucible_sdk").joinpath("scrub_rules.json").read_text("utf-8"))
SCRUBBER = Scrubber(RULES)
# The repository root; `.parent` stops at the filesystem root when the SDK is checked out on its own.
SHARED = Path(__file__).resolve().parent.parent.parent.parent / "conformance" / "scrub-rules.json"


def only(rule: dict[str, Any]) -> Scrubber:
    return Scrubber({"schema": SCHEMA, "rules": [rule]})


@pytest.mark.parametrize("rule", RULES["rules"], ids=lambda r: r["name"])
def test_each_rule_changes_its_positives_and_keeps_its_negatives(rule: dict[str, Any]) -> None:
    scrubber = only(rule)
    for text in rule["positive"]:
        assert scrubber.text(text) != text, text
    for text in rule["negative"]:
        assert scrubber.text(text) == text, text


@pytest.mark.parametrize("example", RULES["examples"], ids=lambda e: e["input"][:30])
def test_every_example_of_the_whole_rule_list(example: dict[str, str]) -> None:
    assert SCRUBBER.text(example["input"]) == example["output"]


def test_scrubbing_is_idempotent() -> None:
    for example in RULES["examples"]:
        once = SCRUBBER.text(example["input"])
        assert SCRUBBER.text(once) == once


@pytest.mark.skipif(not SHARED.exists(), reason="conformance/scrub-rules.json has not landed yet")
def test_the_package_copy_equals_the_shared_rules() -> None:
    assert SHARED.read_bytes() == resources.files("crucible_sdk").joinpath("scrub_rules.json").read_bytes()


@pytest.mark.parametrize(
    "pattern",
    [".", "\\d", "a\\b", "^a", "a$", "(?i)a", "(?=x)a", "(?<!x)a", "(a)", "(?P<n>a)", "a+?", "a*+", "(?:a+)+", "[\\w]"],
)
def test_patterns_outside_the_shared_subset_are_refused(pattern: str) -> None:
    with pytest.raises(ValueError, match="outside the shared pattern subset"):
        only({"name": "bad", "pattern": pattern, "replacement": "x"})


def test_a_wrong_schema_is_refused_with_the_fix() -> None:
    with pytest.raises(ValueError, match=re.escape(f"'schema' must be '{SCHEMA}'")):
        Scrubber({"schema": "other"})


ADVERSARIAL = [
    "a" * 16384,
    "1" * 16384,
    "1 " * 8192,
    "a@" * 8192,
    "a." * 8192,
    "-----BEGIN A-----" + "A" * 16000,
    "eyJ" * 5000 + ".eyJ" * 400,
    "bearer " * 2300,
    "1:" * 8192,
    "+1" * 8192,
    "(415) " * 2700,
    "csk_" * 4096,
]


@pytest.mark.parametrize("text", ADVERSARIAL, ids=lambda t: t[:12])
def test_adversarial_16_kib_inputs_scrub_within_a_bound(text: str) -> None:
    start = time.perf_counter()
    SCRUBBER.text(text)
    # Exponential backtracking takes minutes here. The email rule's unbounded local part is quadratic in a
    # backtracking matcher on a long run with no '@' (about 0.15 s for 16 KiB on a laptop); the capture cap bounds it.
    assert time.perf_counter() - start < 2.0


def run(unit: str, length: int) -> str:
    return (unit * (length // len(unit) + 1))[:length]


# Inputs that make a backtracking matcher slow on the shared rules, about 16 KiB each.
STEP_INPUTS = {
    "letters": "a" * 16384,
    "digits": "1" * 16384,
    "digit pairs": "1 " * 8192,
    "at signs": "a@" * 8192,
    "dots": "a." * 8192,
    "one at sign": "a" * 8000 + "@" + "a" * 8000,
    "hex": run("0123456789abcdef", 16384),
    "alphanumeric": run("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789", 16384),
    "base64": base64.b64encode(bytes(i % 256 for i in range(12288))).decode(),
    "pem header": "-----BEGIN A-----" + "A" * 16000,
    "jwt prefixes": "eyJ" * 5461,
    "jwt chain": "eyJ" * 5000 + ".eyJ" * 400,
    "bearer": "bearer " * 2340,
    "colons": "1:" * 8192,
    "plus ones": "+1" * 8192,
    "phone groups": "(415) " * 2730,
    "secret key prefixes": "csk_" * 4096,
}


def matcher_steps(text: str) -> int:
    """Matcher steps of scrubbing `text` with the whole rule list."""
    steps: list[int] = []
    _scrub.set_matcher_hook(lambda _, s: steps.append(s))
    try:
        SCRUBBER.text(text)
    finally:
        _scrub.set_matcher_hook(None)
    return sum(steps)


def test_the_rules_patterns_yield_the_facts_the_guards_use() -> None:
    found = {}
    for rule in RULES["rules"]:
        f = facts(rule["pattern"])
        found[rule["name"]] = (f.min_length, f.literals, None if f.lead is None else (f.lead.prefix, f.lead.anchor))
    assert found == {
        "pem": (32, ("-----BEGIN ",), None),
        "jwt": (7, ("eyJ",), ("eyJ", ".")),
        "bearer": (23, None, None),
        "secret_key": (12, ("csk_",), None),
        "card": (14, None, None),
        "email": (6, ("@",), ("", "@")),
        "ipv6": (6, (":", ":"), None),
        "ipv4": (7, (".",), None),
        "phone": (8, None, None),
    }


@pytest.mark.parametrize("text", STEP_INPUTS.values(), ids=STEP_INPUTS.keys())
def test_matcher_steps_stay_linear_in_the_length(text: str) -> None:
    assert matcher_steps(text) <= 2 * len(RULES["rules"]) * len(text)


def test_a_long_run_scrubs_within_a_generous_ceiling() -> None:
    text = "a" * 65536 + "@" + "a" * 65536
    start = time.perf_counter()
    SCRUBBER.text(text)
    assert time.perf_counter() - start < 2.0


def test_capped_json_within_the_budget_is_valid_json() -> None:
    value = {"q": "mail jane.doe@example.com", "n": 3, "ok": True, "none": None, "list": [1, "x", {"y": 2.5}]}
    text, cut = SCRUBBER.capped_json(value, 16384)
    assert not cut
    assert json.loads(text) == {**value, "q": "mail [redacted:email]"}


def test_never_emitted_keys_are_replaced_whole_at_any_depth() -> None:
    value = {"Authorization": {"nested": "x"}, "outer": [{"Set-Cookie": "k", "requestState": "s", "keep": "v"}]}
    text, _ = SCRUBBER.capped_json(value, 16384)
    assert json.loads(text) == {
        "Authorization": "[redacted]",
        "outer": [{"Set-Cookie": "[redacted]", "requestState": "[redacted]", "keep": "v"}],
    }


@pytest.mark.parametrize("cap", [1, 7, 64, 1000])
def test_a_cut_value_fits_the_cap_on_a_character_boundary(cap: int) -> None:
    value = {"text": "é" * 3000, "more": list(range(500))}
    text, cut = SCRUBBER.capped_json(value, cap)
    assert cut
    assert len(text.encode()) <= cap
    full, _ = SCRUBBER.capped_json(value, 1 << 20)
    assert full.startswith(text)


def test_a_secret_across_the_cap_is_matched_before_the_cut() -> None:
    text, cut = SCRUBBER.capped_json({"note": "x" * 90 + " jane.doe@example.com"}, 100)
    assert cut
    assert "jane" not in text
    assert "doe@" not in text


def test_a_pem_key_across_the_cap_is_matched_before_the_cut() -> None:
    pem = "-----BEGIN PRIVATE KEY-----\n" + "MIIB" * 800 + "\n-----END PRIVATE KEY-----"
    # The key is longer than the cap, but it is matched whole before anything is cut, and what remains fits.
    text, cut = SCRUBBER.capped_json({"note": "x" * 40 + pem}, 200)
    assert not cut
    assert json.loads(text) == {"note": "x" * 40 + "[redacted:pem]"}


def test_bounded_text_cuts_after_matching() -> None:
    assert SCRUBBER.bounded_text("x" * 10 + " jane.doe@example.com", 14) == "x" * 10 + " [re"
    assert SCRUBBER.bounded_text("short", 100) == "short"


def test_a_long_string_is_cut_before_it_is_scrubbed() -> None:
    text, cut = SCRUBBER.capped_json("a" * 10_000_000, 16)
    assert cut
    assert text == '"' + "a" * 15


def test_deep_nesting_is_cut_like_the_byte_cap() -> None:
    value: Any = "leaf"
    for _ in range(500):
        value = [value]
    text, cut = SCRUBBER.capped_json(value, 16384)
    assert cut
    assert text == "[" * 64


def test_a_long_key_is_cut_before_it_is_scrubbed() -> None:
    start = time.perf_counter()
    text, cut = SCRUBBER.capped_json({"k" * 10_000_000: 1}, 16)
    assert time.perf_counter() - start < 1.0
    assert cut
    assert text == '{"' + "k" * 14


def test_a_rejected_match_resumes_after_its_first_separator() -> None:
    rule = {"name": "digits", "pattern": "[0-9]{4}(?: [0-9]{4}){0,3}", "validator": "luhn", "replacement": "[card]"}
    # The greedy match "1234 4111 1111 1111" fails the checksum; the search resumes after its first space, where the
    # valid "4111 1111 1111 1111" starts. A run without separators is not searched for sub-windows.
    assert only(rule).text("1234 4111 1111 1111 1111") == "1234 [card]"
    assert only(rule).text("12344111") == "12344111"
