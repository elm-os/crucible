"""The shared scrub rules and the capped JSON encoder (contract 9.10)."""

from __future__ import annotations

import json
import re
import time
from importlib import resources
from pathlib import Path
from typing import Any

import pytest

from crucible_sdk._scrub import Scrubber

RULES: dict[str, Any] = json.loads(resources.files("crucible_sdk").joinpath("scrub_rules.json").read_text("utf-8"))
SCRUBBER = Scrubber(RULES)
# The repository root; `.parent` stops at the filesystem root when the SDK is checked out on its own.
SHARED = Path(__file__).resolve().parent.parent.parent.parent / "conformance" / "scrub-rules.json"


@pytest.mark.parametrize("vector", RULES["vectors"], ids=lambda v: v["input"][:40])
def test_every_vector(vector: dict[str, str]) -> None:
    assert SCRUBBER.text(vector["input"]) == vector["output"]


def test_scrubbing_is_idempotent() -> None:
    for vector in RULES["vectors"]:
        once = SCRUBBER.text(vector["input"])
        assert SCRUBBER.text(once) == once


@pytest.mark.skipif(not SHARED.exists(), reason="conformance/scrub-rules.json has not landed yet")
def test_the_package_copy_equals_the_shared_rules() -> None:
    assert json.loads(SHARED.read_text("utf-8")) == RULES


@pytest.mark.parametrize(
    "pattern", ["(?=x)a", "(?!x)a", "(?<=x)a", "(?<!x)a", "(a)\\1", "(?P<n>a)(?P=n)", "a*+", "(?>a)"]
)
def test_rules_outside_the_re2_subset_are_refused(pattern: str) -> None:
    document = {
        "schema": "crucible.scrub-rules/v1",
        "patterns": [{"name": "bad", "pattern": pattern, "replacement": "x"}],
    }
    with pytest.raises(ValueError, match="not allowed"):
        Scrubber(document)


def test_a_wrong_schema_is_refused_with_the_fix() -> None:
    with pytest.raises(ValueError, match=re.escape("'schema' must be 'crucible.scrub-rules/v1'")):
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
    "sk-" + "a-" * 8000,
]


@pytest.mark.parametrize("text", ADVERSARIAL, ids=lambda t: t[:12])
def test_adversarial_16_kib_inputs_scrub_in_linear_time(text: str) -> None:
    start = time.perf_counter()
    SCRUBBER.text(text)
    # A super-linear rule takes seconds to hours here; linear matching takes milliseconds.
    assert time.perf_counter() - start < 0.5


def test_capped_json_within_the_budget_is_valid_json() -> None:
    value = {"q": "mail jane.doe@example.com", "n": 3, "ok": True, "none": None, "list": [1, "x", {"y": 2.5}]}
    text, cut = SCRUBBER.capped_json(value, 16384)
    assert not cut
    assert json.loads(text) == {**value, "q": "mail [REDACTED:email]"}


def test_denied_keys_are_replaced_whole_at_any_depth() -> None:
    value = {"password": {"nested": "x"}, "outer": [{"API-Key": "k", "requestState": "s", "keep": "v"}]}
    text, _ = SCRUBBER.capped_json(value, 16384)
    assert json.loads(text) == {
        "password": "[REDACTED]",
        "outer": [{"API-Key": "[REDACTED]", "requestState": "[REDACTED]", "keep": "v"}],
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
