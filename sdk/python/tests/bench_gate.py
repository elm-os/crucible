"""Added latency per tools/call (design 2.4 gate): the instrumented reference server minus the plain one.

Run with `uv run --locked python -m tests.bench_gate`. Calls alternate between the two servers in rounds over the
in-process transport, so drift affects both equally. These are laptop measurements, not a CI gate.
"""

from __future__ import annotations

import time
from typing import Any

import anyio
from mcp import Client
from mcp.server.mcpserver import MCPServer

import crucible_sdk
import crucible_sdk.mcp
from conformance import reference_server
from tests.collector import running

ROUNDS, PER_ROUND, WARMUP = 20, 250, 50
NOTE = ("Order 1042 for ada.lovelace@example.com shipped to Lisbon; tracking follows in a later message. " * 170)[
    :16000
]


def percentile(samples: list[float], q: float) -> float:
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * q))] * 1e6


async def timed(server: MCPServer, tool: str, args: dict[str, Any]) -> list[float]:
    samples: list[float] = []
    async with Client(server, mode="2026-07-28") as client:
        for _ in range(WARMUP):
            await client.call_tool(tool, args)
        for _ in range(PER_ROUND):
            start = time.perf_counter()
            await client.call_tool(tool, args)
            samples.append(time.perf_counter() - start)
    return samples


async def main() -> None:
    with running() as collector:
        crucible_sdk.init(endpoint=collector.url, secret_key="csk_bench", queue_size=100_000)
        plain = reference_server.build()
        wrapped = crucible_sdk.mcp.instrument(
            reference_server.build(), capture=[crucible_sdk.mcp.Capture(tool="secret")]
        )
        cases = [
            ("metadata only (echo)", "echo", {"text": "hi"}),
            ("capture on, 16 KB argument", "secret", {"note": NOTE}),
        ]
        for label, tool, args in cases:
            without: list[float] = []
            with_sdk: list[float] = []
            for _ in range(ROUNDS):
                without += await timed(plain, tool, args)
                with_sdk += await timed(wrapped, tool, args)
            added = [percentile(with_sdk, q) - percentile(without, q) for q in (0.5, 0.99)]
            print(f"{label}: added p50 {added[0]:.0f} us, p99 {added[1]:.0f} us")
        crucible_sdk.shutdown()


if __name__ == "__main__":
    anyio.run(main)
