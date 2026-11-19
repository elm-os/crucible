"""The stalled-collector scenario of the shared SDK suite (`conformance/export-stall.json`, contract 9.10).

With capture on and a collector that never answers, the export queue stays under its byte ceiling and drops.
"""

from __future__ import annotations

import json
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
    not (CONFORMANCE / "export-stall.json").exists(), reason="the SDK is checked out without conformance/"
)


def load(name: str) -> dict[str, Any]:
    data: dict[str, Any] = json.loads((CONFORMANCE / name).read_text("utf-8"))
    return data


@pytest.mark.anyio
async def test_a_collector_that_never_answers_keeps_the_queue_under_its_byte_ceiling() -> None:
    scenario = load("export-stall.json")
    assert scenario["schema"] == "crucible.conformance.export-stall/v1"
    assert scenario["calls"] < scenario["queue_spans"]
    prose: str = load("capture-overhead.json")["fills"]["prose"]
    text = (prose * (scenario["result_bytes"] // len(prose) + 1))[: scenario["result_bytes"]]
    server = MCPServer("export-stall", version="1.0.0")

    @server.tool(name=scenario["tool"])
    def shape() -> str:
        return text

    crucible_sdk.mcp.instrument(
        server, capture=[crucible_sdk.mcp.Capture(tool=scenario["tool"], max_bytes=scenario["max_bytes"])]
    )
    most = 0
    with running(Collector(hang=True)) as collector:
        crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, flush_interval=60.0)
        async with Client(server, mode="2026-07-28") as client:
            for _ in range(scenario["calls"]):
                await client.call_tool(scenario["tool"], {})
                most = max(most, crucible_sdk.export_stats().queued_bytes)
        dropped = crucible_sdk.export_stats().dropped
    crucible_sdk.shutdown()
    print(f"export-stall python: most queued {most} bytes (ceiling {scenario['queue_bytes']}), dropped {dropped}")
    assert most <= scenario["queue_bytes"], f"bytes queued: {most}"
    assert dropped > 0, "no span was dropped"
