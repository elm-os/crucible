"""The capped JSON encoder against the reference walk it replaced (`tests/capture_reference.py`): for any value, any
cap and either way of converting, the same scrubbed text and the same cut flag (contract 9.10)."""

from __future__ import annotations

import collections
import datetime
import decimal
import time
from random import Random
from typing import Any

from mcp_types import CallToolResult, TextContent

from crucible_sdk import _keys, _scrub
from crucible_sdk.mcp._middleware import Capture, CrucibleMiddleware, _lazy
from tests.capture_reference import reference_capped_json, reference_lazy, reference_result
from tests.test_scrub import RULES, SCRUBBER, Tone, generated, random_block, random_json

DENIED = ["Authorization", "set-cookie", "Request_State", "proxy.authorization", "cookie", "x-cookie"]
TEXTS = ["", "SKU-00001", "ada@example.com", "4111 1111 1111 1111", "é😀", "\ud800", "a\x00b", 'q"\\\n', "\t\x01"]
CAPS = [1, 2, 3, 7, 17, 40, 100, 333, 600, 2048, 5000, 16384, 1 << 20]


def random_key(random: Random) -> object:
    kind = random.randrange(10)
    if kind == 0:
        return random.choice(DENIED)
    if kind == 1:
        return random.choice([3, 1.5, True, None, Tone.WARM, datetime.date(2026, 11, 20)])
    if kind == 2:
        return random.choice(TEXTS) + generated(random)[:20]
    return random.choice(["id", "sku", "qty", "name", "note", "k"])


def random_scalar(random: Random) -> object:
    return random.choice(
        [
            None,
            True,
            False,
            0,
            -7,
            2**70,
            1.5,
            -0.0,
            1e300,
            float("nan"),
            float("inf"),
            float("-inf"),
            random.choice(TEXTS),
            generated(random),
            "x" * random.choice([5, 127, 128, 129, 5000]),
        ]
    )


def records(random: Random) -> list[Any]:
    """A run of flat records, now and then with one that a run cannot take."""
    rows: list[Any] = []
    for i in range(random.randrange(1, 90)):
        row: dict[Any, Any] = {"id": i, "sku": f"SKU-{i:05d}", "qty": i % 7, "price": i / 4}
        odd = random.randrange(40)
        if odd == 0:
            row["note"] = random.choice(TEXTS[1:4])
        elif odd == 1:
            row[random.choice(DENIED)] = "secret"
        elif odd == 2:
            row["ratio"] = random.choice([float("nan"), float("inf")])
        elif odd == 3:
            row["long"] = "y" * random.choice([129, 3000])
        elif odd == 4:
            row[7] = "number key"
        elif odd == 5:
            row["nested"] = {"a": 1}
        rows.append(row if odd != 6 else collections.OrderedDict(row))
    return rows


def random_value(random: Random, depth: int) -> Any:
    kind = random.randrange(5 if depth > 3 else 9)
    if kind <= 1:
        return random_scalar(random)
    if kind == 2:
        return random_json(random, 3)
    if kind == 3:
        return records(random)
    if kind == 4:
        return random.choice([datetime.datetime(2026, 11, 20, 8, 0), decimal.Decimal("1.25"), b"raw", {1, 2}])
    if kind == 5:
        return [random_value(random, depth + 1) for _ in range(random.randrange(6))]
    if kind == 6:
        return tuple(random_value(random, depth + 1) for _ in range(random.randrange(4)))
    if kind == 7:
        nested: Any = random_scalar(random)
        for _ in range(random.choice([1, 63, 64, 70])):
            nested = [nested] if random.random() < 0.5 else {"d": nested}
        return nested
    return {random_key(random): random_value(random, depth + 1) for _ in range(random.randrange(6))}


def test_plain_values_encode_as_the_reference_walk_does() -> None:
    random = Random(20261120)
    mismatches: list[str] = []
    for _ in range(4000):
        value = random_value(random, 0)
        cap = random.choice(CAPS)
        if SCRUBBER.capped_json(value, cap) != reference_capped_json(SCRUBBER, value, cap):
            mismatches.append(f"{cap}: {value!r}"[:300])
    assert mismatches[:3] == []


def test_converted_values_encode_as_the_reference_walk_does() -> None:
    # Builtin containers convert inline as the encoder reads them; the reference wraps each one to convert it.
    random = Random(20261121)
    mismatches: list[str] = []
    for _ in range(4000):
        value = random_value(random, 0)
        cap = random.choice(CAPS)
        if SCRUBBER.capped_json(_lazy(value), cap) != reference_capped_json(SCRUBBER, reference_lazy(value), cap):
            mismatches.append(f"{cap}: {value!r}"[:300])
    assert mismatches[:3] == []


def test_tool_results_capture_as_the_reference_did() -> None:
    middleware = CrucibleMiddleware(
        server_name="s",
        server_version="1",
        capture=(),
        handle_params={},
        identify=None,
        identity="auto",
        transport=None,
    )
    random = Random(20261122)
    mismatches: list[str] = []
    for _ in range(1500):
        result = CallToolResult(
            content=[random_block(random) for _ in range(random.randrange(4))],
            structured_content=random.choice(
                [None, {"records": records(random)}, {"value": random_value(random, 1), 2: "key"}]
            ),
            is_error=random.random() < 0.3,
        )
        cap = random.choice(CAPS)
        attributes = middleware._result_capture(result, Capture(max_bytes=cap))
        got = (attributes[_keys.TOOL_CALL_RESULT], _keys.CONTENT_TRUNCATION in attributes)
        if got != reference_result(SCRUBBER, result, cap):
            mismatches.append(f"{cap}: {result!r}"[:300])
    assert mismatches[:3] == []


def test_every_cut_of_a_record_run_encodes_as_the_reference_walk_does() -> None:
    rows: list[Any] = [{"id": i, "sku": f"SKU-{i:05d}", "qty": i % 7} for i in range(1, 501)]
    rows[300]["note"] = "mail ada@example.com"
    rows[400]["Authorization"] = "Bearer 9f8e7d6c5b4a39281706f5e4d3c2b1a0"
    value = {"content": [{"type": "text", "text": "é" * 40}], "structuredContent": {"records": rows}}
    for cap in range(1, 20_000, 97):
        assert SCRUBBER.capped_json(value, cap) == reference_capped_json(SCRUBBER, value, cap), cap
        assert SCRUBBER.capped_json(_lazy(value), cap) == reference_capped_json(SCRUBBER, reference_lazy(value), cap)


def test_a_joined_check_leaves_alone_only_texts_the_rules_leave_alone() -> None:
    # Joined by NUL, the texts are checked at once: never yes when a rule changes one of them, and no only because
    # of a `pem` match across texts.
    random = Random(20261123)
    for _ in range(3000):
        texts = [random.choice([*TEXTS, generated(random)]) for _ in range(random.randrange(8))]
        alone = all(SCRUBBER.text(text) == text for text in texts)
        if SCRUBBER.leaves_alone(texts):
            assert alone, texts
        elif "-----" not in "".join(texts):
            assert not alone, texts


def test_no_rule_but_pem_matches_across_a_nul() -> None:
    # `leaves_alone` joins texts by NUL on the strength of this; pinned on every rule's positive vectors.
    for rule, vectors in zip(SCRUBBER.rules, RULES["rules"], strict=True):
        if rule.name == "pem":
            continue
        for text in vectors["positive"]:
            for at in range(1, len(text)):
                joined = text[:at] + "\x00" + text[at:]
                for match in rule.regex.finditer(joined):
                    assert not match.start() <= at < match.end(), (rule.name, joined)


def test_the_work_of_a_record_capture_stays_within_the_bound() -> None:
    rows = [{"id": i, "sku": f"SKU-{i:05d}", "qty": i % 7} for i in range(1, 501)]
    result = CallToolResult(content=[TextContent(type="text", text="r" * 80)], structured_content={"records": rows})
    middleware = CrucibleMiddleware(
        server_name="s",
        server_version="1",
        capture=(),
        handle_params={},
        identify=None,
        identity="auto",
        transport=None,
    )
    work: list[tuple[int, int]] = []
    _scrub.set_work_hook(lambda serialized, scrubbed: work.append((serialized, scrubbed)))
    try:
        middleware._result_capture(result, Capture())
    finally:
        _scrub.set_work_hook(None)
    assert max(max(w) for w in work) <= 16384 + 8192
    # And it is fast: the reference walk took several milliseconds here.
    start = time.perf_counter()
    for _ in range(20):
        middleware._result_capture(result, Capture())
    assert (time.perf_counter() - start) / 20 < 0.05
