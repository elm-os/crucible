"""The server middleware behind streamable HTTP on a loopback port: a legacy session and modern stateless calls."""

from __future__ import annotations

import json
import sys
import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

import pytest
import uvicorn
from mcp import Client
from mcp.client.stdio import StdioServerParameters
from mcp.server.mcpserver import MCPServer

from conformance import reference_server
from crucible_sdk import _keys
from crucible_sdk.mcp._middleware import session_hash
from tests.collector import Collector
from tests.conftest import SECRET_KEY
from tests.test_mcp_server import CLIENT_INFO, mcp_spans

ROOT = Path(__file__).resolve().parents[1]


@contextmanager
def serving(server: MCPServer, seen_sessions: list[str]) -> Iterator[str]:
    app = server.streamable_http_app()

    async def recording(scope: dict[str, Any], receive: Any, send: Any) -> None:
        if scope["type"] == "http":
            seen_sessions.extend(v.decode() for k, v in scope["headers"] if k == b"mcp-session-id")
        await app(scope, receive, send)

    http = uvicorn.Server(uvicorn.Config(recording, host="127.0.0.1", port=0, log_level="warning"))
    thread = threading.Thread(target=http.run, daemon=True)
    thread.start()
    deadline = time.monotonic() + 10
    while not http.started:
        assert thread.is_alive(), "the HTTP server stopped before it started"
        assert time.monotonic() < deadline, "the HTTP server did not start in 10 s"
        time.sleep(0.01)
    port = http.servers[0].sockets[0].getsockname()[1]
    try:
        yield f"http://127.0.0.1:{port}/mcp"
    finally:
        http.should_exit = True
        thread.join(10)


@pytest.mark.anyio
async def test_a_legacy_http_session(sdk: Collector) -> None:
    sessions: list[str] = []
    with serving(reference_server.instrumented(), sessions) as url:
        async with Client(url, mode="legacy", client_info=CLIENT_INFO) as client:
            await client.call_tool("echo", {"text": "hi"})
    spans = {s.attributes["mcp.method.name"]: s.attributes for s in mcp_spans(sdk)}
    call = spans["tools/call"]
    assert call["crucible.mcp.transport"] == "streamable_http"
    assert call["crucible.mcp.era"] == "legacy"
    assert sessions, "the client sent no session id"
    assert call["mcp.session.id"] == session_hash(sessions[0])
    assert sessions[0].encode() not in sdk.raw()
    assert "mcp.session.id" not in spans["initialize"]
    assert call["crucible.mcp.client.ip_prefix"] == "127.0.0.0/24"
    assert call["user_agent.original"]
    assert call["crucible.mcp.request.bytes"] > 0
    assert call["crucible.mcp.client.name"] == "crucible-test-client"


@pytest.mark.anyio
async def test_modern_http_calls_are_stateless(sdk: Collector) -> None:
    with serving(reference_server.instrumented(), []) as url:
        async with Client(url, mode="2026-07-28", client_info=CLIENT_INFO) as client:
            await client.call_tool("echo", {"text": "hi"})
    [call] = mcp_spans(sdk, "tools/call")
    a = call.attributes
    assert a["crucible.mcp.transport"] == "streamable_http"
    assert a["crucible.mcp.era"] == "modern"
    assert "mcp.session.id" not in a
    assert a["crucible.mcp.client.name"] == "crucible-test-client"
    assert a["crucible.mcp.request.bytes"] > 0


@pytest.mark.anyio
@pytest.mark.parametrize("mode", ["legacy", "2026-07-28"])
async def test_a_stdio_server_process_flushes_at_exit(collector: Collector, mode: str) -> None:
    config = {
        "transport": "stdio",
        "endpoint": collector.url,
        "secret_key": SECRET_KEY,
        "sdk": {"service_name": "stdio-server", "flush_interval_ms": 60000},
        "server": {"name": reference_server.SERVER_NAME, "version": reference_server.SERVER_VERSION},
    }
    params = StdioServerParameters(
        command=sys.executable, args=["-m", "conformance.server", json.dumps(config)], cwd=str(ROOT)
    )
    async with Client(params, mode=mode, client_info=CLIENT_INFO) as client:
        await client.call_tool("echo", {"text": "over a pipe"})
    spans = [s for s in collector.wait_for(1, timeout=20) if s.attributes.get("mcp.method.name") == "tools/call"]
    [call] = spans
    assert call.attributes["crucible.mcp.transport"] == "stdio"
    assert call.attributes["crucible.mcp.era"] == ("legacy" if mode == "legacy" else "modern")
    assert call.resource["service.name"] == "stdio-server"


@pytest.mark.anyio
async def test_identity_none_drops_the_http_identity_keys(sdk: Collector) -> None:
    server = reference_server.build()
    import crucible_sdk.mcp

    crucible_sdk.mcp.instrument(server, identity="none")
    with serving(server, []) as url:
        async with Client(url, mode="2026-07-28") as client:
            await client.call_tool("echo", {"text": "hi"})
    [call] = mcp_spans(sdk, "tools/call")
    assert not set(call.attributes) & _keys.IDENTITY_KEYS
