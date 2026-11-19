"""The capture-overhead scenario of the shared SDK suite (`conformance/capture-overhead.json`, contract 9.10).

The same tool with capture off and on, in paired alternating rounds over fixed result shapes. The work hook bounds
what the capture path serializes and scrubs per field; the added p99 is printed, and gated only on the laptop rig
(`CRUCIBLE_CAPTURE_GATE=1`).
"""

from __future__ import annotations

import base64
import json
import os
import time
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import pytest
from mcp import Client
from mcp.server.mcpserver import MCPServer
from mcp_types import CallToolResult, ContentBlock, ImageContent, TextContent

import crucible_sdk
import crucible_sdk.mcp
from crucible_sdk import _pipeline, _scrub
from tests.collector import Collector, running
from tests.conftest import SECRET_KEY

_SCRUBBER_RULES = _pipeline.SCRUBBER.rules

SCENARIO_FILE = Path(__file__).resolve().parent.parent.parent.parent / "conformance" / "capture-overhead.json"
pytestmark = pytest.mark.skipif(not SCENARIO_FILE.exists(), reason="the SDK is checked out without conformance/")
SCENARIO: dict[str, Any] = json.loads(SCENARIO_FILE.read_text("utf-8")) if SCENARIO_FILE.exists() else {"shapes": []}
GATED = os.environ.get("CRUCIBLE_CAPTURE_GATE") == "1"


def repeat_to(unit: str, length: int) -> str:
    return (unit * (length // len(unit) + 1))[:length]


def base64_fill(length: int) -> str:
    return base64.b64encode(bytes(i % 256 for i in range(length // 4 * 3))).decode()


def result_of(shape: dict[str, Any]) -> CallToolResult:
    content: list[ContentBlock] = []
    for block in shape["blocks"]:
        if block["type"] == "image":
            content.append(
                ImageContent(type="image", data=base64_fill(block["length"]), mime_type=block.get("mime_type"))
            )
        else:
            content.append(TextContent(type="text", text=repeat_to(SCENARIO["fills"][block["fill"]], block["length"])))
    records = shape.get("records")
    if records is None:
        return CallToolResult(content=content)
    rows = [{"id": i, "sku": f"SKU-{i:05d}", "qty": i % 7} for i in range(1, records + 1)]
    return CallToolResult(content=content, structured_content={"records": rows})


RESULTS = {shape["id"]: result_of(shape) for shape in SCENARIO["shapes"]}


def shape_server() -> MCPServer:
    server = MCPServer("capture-overhead", version="1.0.0")

    @server.tool(name=SCENARIO.get("tool", "shape"))
    def shape(shape: str) -> CallToolResult:
        return RESULTS[shape]

    return server


@pytest.fixture(scope="module")
def stalled() -> Iterator[Collector]:
    """The SDK exporting to a collector that never answers, so the queue fills and drops instead of exporting."""
    with running(Collector(hang=True)) as collector:
        crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, flush_interval=60.0)
        yield collector
    crucible_sdk.shutdown()


def quantile(samples: list[float], q: float) -> float:
    """In milliseconds."""
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))] * 1000


def p99(samples: list[float]) -> float:
    return quantile(samples, 0.99)


def test_the_scenario_file_is_the_version_this_test_reads() -> None:
    assert SCENARIO["schema"] == "crucible.conformance.capture-overhead/v1"


@pytest.mark.anyio
@pytest.mark.parametrize("shape", SCENARIO["shapes"], ids=lambda s: s["id"])
async def test_the_capture_path_stays_within_the_work_bound(stalled: Collector, shape: dict[str, Any]) -> None:
    run = SCENARIO["runs"]["laptop" if GATED else "ci"]
    plain = crucible_sdk.mcp.instrument(shape_server())
    captured = crucible_sdk.mcp.instrument(
        shape_server(), capture=[crucible_sdk.mcp.Capture(tool=SCENARIO["tool"], max_bytes=SCENARIO["max_bytes"])]
    )
    arguments = {"shape": shape["id"]}
    work: list[tuple[int, int]] = []
    call_steps: list[int] = []
    without: list[float] = []
    with_capture: list[float] = []
    async with Client(plain, mode="2026-07-28") as off, Client(captured, mode="2026-07-28") as on:

        async def call(client: Client) -> float:
            start = time.perf_counter()
            await client.call_tool(SCENARIO["tool"], arguments)
            return time.perf_counter() - start

        for _ in range(run["warmup"]):
            await call(off)
            await call(on)
        # The work of one captured call, with the hooks installed; the timed rounds run without them.
        _scrub.set_work_hook(lambda serialized, scrubbed: work.append((serialized, scrubbed)))
        _scrub.set_matcher_hook(lambda _, s: call_steps.append(s))
        try:
            await call(on)
        finally:
            _scrub.set_work_hook(None)
            _scrub.set_matcher_hook(None)
        for round_ in range(run["rounds"]):
            for client in (off, on) if round_ % 2 == 0 else (on, off):
                for _ in range(run["calls"]):
                    (with_capture if client is on else without).append(await call(client))
    serialized = max(s for s, _ in work)
    scrubbed = max(c for _, c in work)
    steps = sum(call_steps)
    steps_per_char = steps / max(sum(c for _, c in work), 1)
    added = p99(with_capture) - p99(without)
    excess = max(with_capture) * 1000 - quantile(without, 0.5)
    print(
        f"capture-overhead python {shape['id']}: added p99 {added:.3f} ms (off {p99(without):.3f}, on "
        f"{p99(with_capture):.3f}), slowest excess {excess:.3f} ms, serialized {serialized}, scrubbed {scrubbed}, "
        f"matcher steps {steps} ({steps_per_char:.1f} per character)"
    )
    bound = SCENARIO["work_bound"]
    assert serialized <= bound, f"characters serialized for one field: {serialized} > {bound}"
    assert scrubbed <= bound, f"characters handed to the scrubber for one field: {scrubbed} > {bound}"
    step_bound = SCENARIO["matcher_steps_per_rule_char"] * len(_SCRUBBER_RULES)
    assert steps_per_char <= step_bound, f"matcher steps per character scrubbed: {steps_per_char:.1f} > {step_bound}"
    assert excess <= SCENARIO["ceiling_ms"], f"slowest captured call over the median uncaptured one: {excess:.1f} ms"
    if GATED:
        assert added <= SCENARIO["gate_added_p99_ms"], f"added p99: {added:.3f} ms"
