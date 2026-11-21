"""The shared scrub rules and the capped JSON encoder (contract 9.10)."""

from __future__ import annotations

import base64
import datetime
import enum
import json
import re
import time
from importlib import resources
from pathlib import Path
from random import Random
from typing import Any

import pytest
from mcp_types import (
    Annotations,
    AudioContent,
    BlobResourceContents,
    CallToolResult,
    ContentBlock,
    EmbeddedResource,
    ImageContent,
    ResourceLink,
    TextContent,
    TextResourceContents,
)

from crucible_sdk import _scrub
from crucible_sdk._pattern import facts
from crucible_sdk._scrub import SCHEMA, Scrubber
from crucible_sdk.mcp._middleware import Capture, CrucibleMiddleware

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
        # A pair is shown as what it finds in a sample.
        pairs = f.pairs
        finds = None if pairs is None else [s for s in ("a:", "1.", "Z@", "0-", "x") if pairs.search(s)]
        found[rule["name"]] = (
            f.min_length,
            f.literals,
            None if f.lead is None else (f.lead.prefix, f.lead.anchor),
            finds,
        )
    assert found == {
        "pem": (32, ("-----BEGIN ",), None, ["0-"]),
        "jwt": (7, ("eyJ",), ("eyJ", "."), ["1."]),
        "bearer": (23, None, None, None),
        "secret_key": (12, ("csk_",), None, None),
        "card": (14, None, None, None),
        "email": (6, ("@",), ("", "@"), ["Z@"]),
        "ipv6": (6, (":", ":"), None, ["a:"]),
        "ipv4": (7, (".",), None, ["1."]),
        "phone": (8, None, None, None),
    }


@pytest.mark.parametrize("text", STEP_INPUTS.values(), ids=STEP_INPUTS.keys())
def test_matcher_steps_stay_linear_in_the_length(text: str) -> None:
    assert matcher_steps(text) <= 2 * len(RULES["rules"]) * len(text)


# Pieces of matches, near misses and separators that the generated text is built from.
FRAGMENTS = [
    *" a07@.:-_+()/=%\néé😀",
    "eyJ",
    "csk_",
    "bearer ",
    "Bearer\t",
    "-----BEGIN ",
    "-----END ",
    "-----",
    "KEY",
    "::",
    "@@",
    "a@b",
    "a@b.c",
    "x@y.zz",
    "ada.lovelace@example.com",
    "ops+alerts@mail.example.co.uk",
    "+1 415 555 0134",
    "(415) 555-0134",
    "4111 1111 1111 1111",
    "4111-1111-1111-1112",
    "378282246310005",
    "Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0",
    "csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo",
    "203.0.113.42",
    "2001:db8::8a2e:370:7334",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhZGEifQ.c2lnbmF0dXJl",
    "eyJa.b",
    "eyJeyJ.x.",
    "eyJ.",
    "-----BEGIN KEY-----\nMIIB\n-----END KEY-----",
]
RUN_UNITS = ["a", "Z", "9", "f", ".", "-", "_", "+", "%", "ab@", "eyJ", "1 "]


def generated(random: Random) -> str:
    parts = []
    for _ in range(random.randrange(14)):
        if random.random() < 0.7:
            parts.append(random.choice(FRAGMENTS))
        else:
            parts.append(random.choice(RUN_UNITS) * random.randint(1, 40))
    return "".join(parts)


def test_guarded_application_equals_plain_application_over_generated_text() -> None:
    random = Random(20261118)
    mismatches: list[str] = []
    for _ in range(3000):
        text = generated(random)
        for rule in SCRUBBER.rules:
            if rule.apply(text) != rule.apply_plain(text):
                mismatches.append(f"{rule.name}: {text!r}")
        if SCRUBBER.text(text) != SCRUBBER.text_plain(text):
            mismatches.append(f"all rules: {text!r}")
        if matcher_steps(text) > 2 * len(RULES["rules"]) * max(len(text), 1):
            mismatches.append(f"steps: {text!r}")
    assert mismatches[:5] == []


# The literals of the rules with a pair, after characters that make the pair and after ones that do not: at the start
# of a text, after a space or a letter outside the pair's class, and after characters outside ASCII.
PAIR_FRAGMENTS = [
    *":.@-",
    *"a1Zg_%x \n",
    "é:",
    "😀-",
    "é.",
    "😀@",
    "g:",
    "F:",
    "_@",
    "%@",
    " -",
    "A-",
    "fe80",
    "::1",
    "1.2.3.4",
    "203.0.113.42",
    "2001:db8::1",
    "a@b.co",
    "eyJ",
    "eyJa.b.c",
    "-----BEGIN ",
    "-----BEGIN é",
    "-----BEGIN A-----",
    "-----END A-----",
]


def test_the_pair_guard_skips_only_text_its_rule_leaves_alone() -> None:
    random = Random(20261121)
    mismatches: list[str] = []
    # Per rule, texts that pass the length and literal guards and lack the pair, so that the pair guard alone decides.
    decided: dict[str, int] = {}
    for _ in range(5000):
        text = "".join(random.choice(PAIR_FRAGMENTS) for _ in range(random.randrange(1, 12)))
        for rule in SCRUBBER.rules:
            f = rule.facts
            if f.pairs is None:
                continue
            literal = f.literals is None or any(lit in text for lit in f.literals)
            if len(text) >= f.min_length and literal and f.pairs.search(text) is None:
                decided[rule.name] = decided.get(rule.name, 0) + 1
            if rule.apply(text) != rule.apply_plain(text):
                mismatches.append(f"{rule.name}: {text!r}")
        if SCRUBBER.text(text) != SCRUBBER.text_plain(text):
            mismatches.append(f"all rules: {text!r}")
    assert mismatches[:5] == []
    paired = [rule.name for rule in SCRUBBER.rules if rule.facts.pairs is not None]
    assert all(decided.get(name, 0) >= 50 for name in paired), decided


def test_a_run_is_the_characters_every_match_is_made_of_and_its_fewest() -> None:
    runs = {}
    for rule in SCRUBBER.rules:
        f = rule.facts
        runs[rule.name] = (
            None
            if f.runs is None
            else sorted((len(r.needle), "".join(chr(code) for code in range(256) if r.members[code])) for r in f.runs)
        )
    assert runs["card"] == [(14, " -0123456789")]
    assert runs["phone"] == [(8, " +-.0123456789"), (12, " ()-.0123456789")]
    assert runs["ipv4"] == [(7, ".0123456789")]
    # A negated class lets a match hold any character.
    assert runs["pem"] is None


# Runs of digits and separators a little shorter and longer than a card or a phone number, and the characters that
# end them: letters, spaces, other punctuation and characters outside ASCII.
RUN_FRAGMENTS = [
    *"0123456789",
    *" -.+()",
    *"aZx\n_/",
    "é",
    "😀",
    "\ud800",
    "4111",
    "1111 ",
    "1111-",
    "555 0134",
    "(415) ",
    "+1 ",
    "4111 1111 1111 1111",
    "378282246310005",
    "+1 415 555 0134",
    "(415) 555-0134",
    "0123456789abcdef",
    # A card and a phone number as short as a match can be.
    "30569309025904",
    "+1 23 45",
]


def test_the_run_guard_skips_only_text_its_rule_leaves_alone() -> None:
    random = Random(20261122)
    mismatches: list[str] = []
    # Per rule, texts that pass the other guards and hold no run, so that the run guard alone decides.
    decided: dict[str, int] = {}
    for _ in range(5000):
        text = "".join(random.choice(RUN_FRAGMENTS) for _ in range(random.randrange(1, 16)))
        for rule in SCRUBBER.rules:
            f = rule.facts
            if f.runs is None:
                continue
            literal = f.literals is None or any(lit in text for lit in f.literals)
            paired = f.pairs is None or f.pairs.search(text) is not None
            held = any(r.held_by(text) for r in f.runs)
            if len(text) >= f.min_length and literal and paired and not held:
                decided[rule.name] = decided.get(rule.name, 0) + 1
            if rule.apply(text) != rule.apply_plain(text):
                mismatches.append(f"{rule.name}: {text!r}")
        if SCRUBBER.text(text) != SCRUBBER.text_plain(text):
            mismatches.append(f"all rules: {text!r}")
    assert mismatches[:5] == []
    assert decided.get("card", 0) >= 50, decided
    assert decided.get("phone", 0) >= 50, decided


def test_a_model_result_is_serialized_no_further_than_the_cap_and_the_margin() -> None:
    middleware = CrucibleMiddleware(
        server_name="s",
        server_version="1",
        capture=(),
        handle_params={},
        identify=None,
        identity="auto",
        transport=None,
    )
    result = CallToolResult(
        content=[TextContent(type="text", text="x" * (5 << 20))], structured_content={"rows": list(range(100_000))}
    )
    work: list[int] = []
    _scrub.set_work_hook(lambda serialized, _: work.append(serialized))
    try:
        middleware._result_capture(result, Capture())
    finally:
        _scrub.set_work_hook(None)
    assert max(work) <= 16384 + 8192


class Tone(enum.Enum):
    WARM = "warm"


def random_json(random: Random, depth: int) -> Any:
    kind = random.randrange(6 if depth > 2 else 9)
    if kind == 0:
        return None
    if kind == 1:
        return random.choice([True, False, 0, -7, 2**60, 1.5, -0.0, float("nan"), float("inf")])
    if kind == 2:
        return generated(random)
    if kind == 3:
        return random.choice([datetime.datetime(2026, 11, 18, 9, 30), Tone.WARM])
    if kind == 4:
        return TextContent(type="text", text=generated(random))
    if kind == 5:
        return "x" * random.randrange(3000)
    if kind == 6:
        return [random_json(random, depth + 1) for _ in range(random.randrange(5))]
    if kind == 7:
        return tuple(random_json(random, depth + 1) for _ in range(random.randrange(3)))
    return {
        (random.randrange(3) if random.random() < 0.2 else generated(random)[:8]): random_json(random, depth + 1)
        for _ in range(random.randrange(5))
    }


def random_block(random: Random) -> ContentBlock:
    meta = random.choice([None, {"k": "v", "n": None}])
    annotations = random.choice([None, Annotations(audience=["user"], priority=0.5)])
    kind = random.randrange(5)
    if kind == 0:
        return TextContent(type="text", text=generated(random) * random.randrange(1, 80), annotations=annotations)
    if kind == 1:
        return ImageContent(type="image", data="iVBORw0K" * random.randrange(400), mime_type="image/png", _meta=meta)
    if kind == 2:
        return AudioContent(type="audio", data="UklGRg" * random.randrange(50), mime_type="audio/wav")
    if kind == 3:
        resource: TextResourceContents | BlobResourceContents = (
            TextResourceContents(uri="file:///a.txt", text=generated(random), mime_type=None)
            if random.random() < 0.5
            else BlobResourceContents(uri="file:///b.bin", blob="AAEC" * random.randrange(30), mime_type="x/y")
        )
        return EmbeddedResource(type="resource", resource=resource, annotations=annotations)
    return ResourceLink(
        type="resource_link",
        name="n",
        uri="file:///x.txt",
        title=random.choice([None, "Title"]),
        size=random.choice([None, 3]),
        _meta=meta,
    )


def test_the_model_walk_equals_a_dump_followed_by_the_capped_encoder() -> None:
    middleware = CrucibleMiddleware(
        server_name="s",
        server_version="1",
        capture=(),
        handle_params={},
        identify=None,
        identity="auto",
        transport=None,
    )
    random = Random(18112026)
    mismatches: list[str] = []
    for _ in range(1500):
        result = CallToolResult(
            content=[random_block(random) for _ in range(random.randrange(5))],
            structured_content=random_json(random, 0),
            is_error=random.random() < 0.3,
            _meta=random.choice([None, {"progress": 1}]),
        )
        capture = Capture(max_bytes=random.choice([1, 17, 100, 600, 5000, 1 << 20]))
        dumped = result.model_dump(mode="json", by_alias=True, exclude_none=True)
        walked = middleware._result_capture(result, capture)
        if walked != middleware._result_capture(dumped, capture):
            mismatches.append(repr(result)[:300])
    assert mismatches[:3] == []


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
