"""The shared scrub rules and the capped JSON encoder (contract 9.10)."""

from __future__ import annotations

import json
import re
import time
from importlib import resources
from pathlib import Path
from typing import Any

import pytest

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
