"""A slow or down collector adds no latency to the caller (`conformance/export-latency.json`, contract 9.10).

The same calls run against a collector that answers, one that never answers and a port where nothing listens. A
call that waited for an export would take the export timeout; every call stays far below it, and `flush` and
`shutdown` keep to their deadlines. The added p99 over the healthy run is printed, and gated only on the laptop rig
(`CRUCIBLE_CAPTURE_GATE=1`).
"""

from __future__ import annotations

import json
import os
import socket
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest
from mcp import Client
from mcp.server.mcpserver import MCPServer

import crucible_sdk
import crucible_sdk.mcp
from tests.collector import Collector, running
from tests.conftest import SECRET_KEY

CONFORMANCE = Path(__file__).resolve().parent.parent.parent.parent / "conformance"
pytestmark = pytest.mark.skipif(
    not (CONFORMANCE / "export-latency.json").exists(), reason="the SDK is checked out without conformance/"
)
GATED = os.environ.get("CRUCIBLE_CAPTURE_GATE") == "1"


def load(name: str) -> dict[str, Any]:
    data: dict[str, Any] = json.loads((CONFORMANCE / name).read_text("utf-8"))
    return data


@contextmanager
def collector_in(state: str) -> Iterator[str]:
    """The endpoint of a collector in `state`."""
    if state == "refused":
        # A port that was free a moment ago: nothing listens on it.
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        yield f"http://127.0.0.1:{port}"
        return
    with running(Collector(hang=state == "stalled")) as collector:
        yield collector.url


def quantile(samples: list[float], q: float) -> float:
    """In milliseconds."""
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))] * 1000


@pytest.mark.anyio
async def test_a_stalled_or_refusing_collector_adds_no_latency_to_calls() -> None:
    scenario = load("export-latency.json")
    assert scenario["schema"] == "crucible.conformance.export-latency/v1"
    assert scenario["calls"] > 512, "the calls must fill a batch, so that an export runs while they do"
    prose: str = load("capture-overhead.json")["fills"]["prose"]
    result_bytes: int = scenario["result_bytes"]
    text: str = (prose * (result_bytes // len(prose) + 1))[:result_bytes]
    deadline = scenario["export_timeout_ms"] / 1000
    slack = scenario["slack_ms"] / 1000
    p99: dict[str, float] = {}
    for state in scenario["collectors"]:
        server = MCPServer("export-latency", version="1.0.0")

        @server.tool(name=scenario["tool"])
        def shape() -> str:
            return text

        crucible_sdk.mcp.instrument(
            server, capture=[crucible_sdk.mcp.Capture(tool=scenario["tool"], max_bytes=scenario["max_bytes"])]
        )
        samples: list[float] = []
        with collector_in(state) as endpoint:
            crucible_sdk.init(endpoint=endpoint, secret_key=SECRET_KEY)
            async with Client(server, mode="2026-07-28") as client:
                for _ in range(scenario["warmup"]):
                    await client.call_tool(scenario["tool"], {})
                for _ in range(scenario["calls"]):
                    start = time.perf_counter()
                    await client.call_tool(scenario["tool"], {})
                    samples.append(time.perf_counter() - start)
            start = time.perf_counter()
            crucible_sdk.flush(deadline)
            flushed = time.perf_counter() - start
            start = time.perf_counter()
            crucible_sdk.shutdown(deadline)
            stopped = time.perf_counter() - start
        p99[state] = quantile(samples, 0.99)
        slowest = max(samples) * 1000
        print(
            f"export-latency python {state}: p50 {quantile(samples, 0.5):.3f} ms, p99 {p99[state]:.3f} ms, slowest "
            f"{slowest:.3f} ms, flush {flushed * 1000:.0f} ms, shutdown {stopped * 1000:.0f} ms"
        )
        if state != "healthy":
            assert slowest <= scenario["call_ceiling_ms"], f"{state}: a call took {slowest:.1f} ms"
        assert flushed <= deadline + slack, f"{state}: flush took {flushed:.1f} s"
        assert stopped <= deadline + slack, f"{state}: shutdown took {stopped:.1f} s"
    for state in ("stalled", "refused"):
        added = p99[state] - p99["healthy"]
        print(f"export-latency python {state}: added p99 {added:.3f} ms over the healthy collector")
        if GATED:
            assert added <= scenario["gate_added_p99_ms"], f"{state}: added p99 {added:.3f} ms"
