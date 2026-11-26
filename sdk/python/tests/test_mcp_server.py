"""The server middleware over the mcp library's in-memory transports, in both protocol eras."""

from __future__ import annotations

import contextlib
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

import anyio
import pytest
from mcp import Client
from mcp.client._memory import InMemoryTransport
from mcp.server import Server, ServerRequestContext
from mcp.server.auth.middleware.auth_context import auth_context_var
from mcp.server.auth.middleware.bearer_auth import AuthenticatedUser
from mcp.server.auth.provider import AccessToken
from mcp.server.mcpserver import Context, MCPServer
from mcp.shared.exceptions import MCPError
from mcp_types import CallToolRequestParams, CallToolResult, ElicitResult, Implementation, TextContent

import crucible_sdk
import crucible_sdk.mcp
from conformance import reference_server
from crucible_sdk import _keys
from crucible_sdk.mcp import _seam
from crucible_sdk.mcp._middleware import CrucibleMiddleware
from tests.collector import Collector, ExportedSpan
from tests.conftest import SECRET_KEY

SERVER, CLIENT = 2, 3
MODES = ["legacy", "modern-direct", "modern-stream"]
CLIENT_INFO = Implementation(name="crucible-test-client", version="1.0.0")


async def confirm(*_: Any) -> ElicitResult:
    return ElicitResult(action="accept", content={"confirmed": True})


@asynccontextmanager
async def connect(server: MCPServer[Any] | Server[Any], mode: str, **options: Any) -> AsyncIterator[Client]:
    """`legacy`: the handshake over memory streams; `modern-direct`: the in-process modern path, which skips
    framing; `modern-stream`: the modern envelope over memory streams."""
    options.setdefault("client_info", CLIENT_INFO)
    options.setdefault("elicitation_callback", confirm)
    if mode == "legacy":
        client = Client(server, mode="legacy", **options)
    elif mode == "modern-direct":
        client = Client(server, mode="2026-07-28", **options)
    else:
        client = Client(InMemoryTransport(server), mode="2026-07-28", **options)
    async with client:
        yield client


def mcp_spans(collector: Collector, method: str | None = None, kind: int = SERVER) -> list[ExportedSpan]:
    assert crucible_sdk.flush()
    return [
        s
        for s in collector.spans()
        if s.kind == kind and "mcp.method.name" in s.attributes and method in (None, s.attributes["mcp.method.name"])
    ]


def era(mode: str) -> str:
    return "legacy" if mode == "legacy" else "modern"


@pytest.fixture(params=MODES)
def mode(request: pytest.FixtureRequest) -> str:
    return str(request.param)


@pytest.mark.anyio
async def test_echo_enriches_the_one_builtin_span(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        result = await client.call_tool("echo", {"text": "hello"})
    assert result.content[0].text == "hello"  # type: ignore[union-attr]
    [call] = mcp_spans(sdk, "tools/call")
    a = call.attributes
    assert a["crucible.mcp.capture_source"] == "sdk"
    assert a["crucible.mcp.era"] == era(mode)
    assert a["crucible.mcp.transport"] == "stdio"
    assert a["crucible.mcp.server.name"] == reference_server.SERVER_NAME
    assert a["crucible.mcp.server.version"] == reference_server.SERVER_VERSION
    assert a["crucible.mcp.client.name"] == "crucible-test-client"
    assert a["crucible.mcp.client.version"] == "1.0.0"
    assert isinstance(a["crucible.mcp.client.capabilities_hash"], int)
    assert a["gen_ai.tool.name"] == "echo"
    assert a["crucible.mcp.content.blocks.text"] == 1
    assert "jsonrpc.request.id" in a
    for absent in ("gen_ai.tool.call.arguments", "gen_ai.tool.call.result", "crucible.mcp.content.captured"):
        assert absent not in a
    assert "mcp.session.id" not in a
    assert "error.type" not in a
    assert call.resource["crucible.deploy.id"] == "deploy-1"


@pytest.mark.anyio
async def test_every_message_has_exactly_one_span(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        await client.list_tools()
        for text in ("a", "b", "c"):
            await client.call_tool("echo", {"text": text})
    spans = mcp_spans(sdk)
    requests = [(s.attributes["mcp.method.name"], s.attributes.get("jsonrpc.request.id")) for s in spans]
    requests = [r for r in requests if r[1] is not None]
    assert len(requests) == len(set(requests))
    assert len([r for r in requests if r[0] == "tools/call"]) == 3
    assert all(s.attributes["crucible.mcp.capture_source"] == "sdk" for s in spans)


@pytest.mark.anyio
async def test_tool_error_carries_the_annotated_subclass(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        result = await client.call_tool("fail", {"reason": "upstream down"})
    assert result.is_error
    [call] = mcp_spans(sdk, "tools/call")
    assert call.attributes["error.type"] == "tool_error"
    assert call.attributes["crucible.mcp.error.subclass"] == "upstream"
    assert "crucible.mcp.error.class" not in call.attributes


@pytest.mark.anyio
async def test_a_raising_tool_is_a_tool_error(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        result = await client.call_tool("throw", {"message": "boom"})
    assert result.is_error
    [call] = mcp_spans(sdk, "tools/call")
    assert call.attributes["error.type"] == "tool_error"
    assert "crucible.mcp.error.subclass" not in call.attributes


@pytest.mark.anyio
async def test_calls_refused_before_the_tool_ran_are_the_callers_fault(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        results = [
            await client.call_tool("echo", {"text": 42}),
            await client.call_tool("cancel_order", {}),
            await client.call_tool("throw", {"message": "boom"}),
        ]
    # mcp answers all three with an isError result.
    assert all(result.is_error for result in results)
    calls = {s.attributes["gen_ai.tool.name"]: s.attributes for s in mcp_spans(sdk, "tools/call")}
    # The arguments failed the tool's input model, and no tool has that name: the -32602 each stands for.
    assert calls["echo"]["crucible.mcp.error.class"] == "validation"
    assert calls["cancel_order"]["crucible.mcp.error.class"] == "not_found"
    for refused in (calls["echo"], calls["cancel_order"]):
        assert refused["rpc.response.status_code"] == "-32602"
        assert refused["error.type"] == "tool_error"
    # A tool that ran and raised keeps its class.
    assert "crucible.mcp.error.class" not in calls["throw"]
    assert "rpc.response.status_code" not in calls["throw"]


@pytest.mark.anyio
async def test_handle_id_comes_from_the_named_argument(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        await client.call_tool("lookup_order", {"order_id": "ord-42"})
        await client.call_tool("echo", {"text": "x"})
    by_tool = {s.attributes["gen_ai.tool.name"]: s.attributes for s in mcp_spans(sdk, "tools/call")}
    assert by_tool["lookup_order"]["crucible.mcp.handle.id"] == "ord-42"
    assert "crucible.mcp.handle.id" not in by_tool["echo"]
    assert "crucible.mcp.identity.source" not in by_tool["lookup_order"]


@pytest.mark.anyio
async def test_secret_content_is_captured_scrubbed(sdk: Collector, mode: str) -> None:
    note = "mail jane.doe@example.com card 4111 1111 1111 1111 key " + SECRET_KEY
    async with connect(reference_server.instrumented(), mode) as client:
        await client.call_tool("secret", {"note": note})
    [call] = mcp_spans(sdk, "tools/call")
    a = call.attributes
    assert a["crucible.mcp.content.captured"] is True
    assert a["crucible.mcp.content.sample_rate"] == 1.0
    assert "crucible.mcp.content.truncation" not in a
    assert "[redacted:email]" in a["gen_ai.tool.call.arguments"]
    assert "[redacted:card]" in a["gen_ai.tool.call.result"]
    raw = sdk.raw()
    for planted in (b"jane.doe@example.com", b"4111 1111 1111 1111", SECRET_KEY.encode()):
        assert planted not in raw


@pytest.mark.anyio
@pytest.mark.parametrize("mode", ["modern-direct", "modern-stream"])
async def test_multi_round_calls_chain_by_state_hash(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        result = await client.call_tool("needs_input", {"item": "lamp"})
    assert result.content[0].text == "confirmed lamp"  # type: ignore[union-attr]
    first, second = sorted(mcp_spans(sdk, "tools/call"), key=lambda s: "crucible.mcp.mrtr.state_in" in s.attributes)
    assert first.attributes["crucible.mcp.result_type"] == "input_required"
    state_out = first.attributes["crucible.mcp.mrtr.state_out"]
    assert len(state_out) == 32
    assert second.attributes["crucible.mcp.mrtr.state_in"] == state_out
    assert "crucible.mcp.result_type" not in second.attributes
    assert reference_server.REQUEST_STATE.encode() not in sdk.raw()


def edge_server(**options: Any) -> MCPServer:
    """Tools for the annotations, identity and trace context; not part of the conformance set."""
    server = MCPServer("crucible-edge-server", version="0.9.0")

    @server.tool()
    def invalid(value: str) -> CallToolResult:
        crucible_sdk.mcp.mark_validation()
        return CallToolResult(content=[TextContent(type="text", text="bad value")], is_error=True)

    @server.tool()
    def missing(value: str) -> CallToolResult:
        crucible_sdk.mcp.mark_not_found()
        return CallToolResult(content=[TextContent(type="text", text="no such order")], is_error=True)

    @server.tool()
    def missing_param(value: str) -> str:
        crucible_sdk.mcp.mark_not_found()
        raise MCPError(code=-32602, message="unknown id")

    @server.tool()
    def whoami() -> str:
        crucible_sdk.mcp.identify(user_id="u-7", tenant="acme")
        return "ok"

    @server.tool()
    def received_traceparent(ctx: Context) -> str:
        return str((ctx.request_context.meta or {}).get("traceparent", ""))

    return crucible_sdk.mcp.instrument(server, **options)


def raising_server() -> Server[Any]:
    """A low-level server whose tool handler raises: an unhandled exception (contract 9.2, row 9)."""

    async def call_tool(ctx: ServerRequestContext[Any, Any], params: CallToolRequestParams) -> CallToolResult:
        raise RuntimeError("db at 203.0.113.9 refused jane.doe@example.com")

    return crucible_sdk.mcp.instrument(Server("crucible-raising-server", version="3.0.0", on_call_tool=call_tool))


def by_tool(collector: Collector) -> dict[str, dict[str, Any]]:
    return {s.attributes["gen_ai.tool.name"]: s.attributes for s in mcp_spans(collector, "tools/call")}


@pytest.mark.anyio
async def test_marks_resolve_to_an_explicit_class(sdk: Collector, mode: str) -> None:
    async with connect(edge_server(), mode) as client:
        await client.call_tool("invalid", {"value": "x"})
        await client.call_tool("missing", {"value": "x"})
        with pytest.raises(MCPError):
            await client.call_tool("missing_param", {"value": "x"})
    spans = by_tool(sdk)
    assert spans["invalid"]["crucible.mcp.error.class"] == "validation"
    assert spans["missing"]["crucible.mcp.error.class"] == "not_found"
    assert spans["missing_param"]["crucible.mcp.error.class"] == "not_found"
    assert spans["missing_param"]["error.type"] == "-32602"


@pytest.mark.anyio
async def test_an_unhandled_exception_is_internal_and_its_text_is_scrubbed(sdk: Collector, mode: str) -> None:
    async with connect(raising_server(), mode) as client:
        with pytest.raises(MCPError):
            await client.call_tool("anything", {})
    [call] = mcp_spans(sdk, "tools/call")
    assert call.attributes["crucible.mcp.error.class"] == "internal"
    assert call.attributes["error.type"] == "RuntimeError"
    assert "[redacted:email]" in call.status_message
    assert any(name == "exception" for name, _ in call.events)
    raw = sdk.raw()
    assert b"jane.doe@example.com" not in raw
    assert b"203.0.113.9" not in raw


@pytest.mark.anyio
async def test_identity_from_the_hook_and_from_the_handler(sdk: Collector, mode: str) -> None:
    async def hook(ctx: ServerRequestContext[Any, Any]) -> crucible_sdk.mcp.Identity:
        return crucible_sdk.mcp.Identity(user_id="u-1", issuer="https://issuer.test", subject="sub-1", tenant="t-1")

    async with connect(edge_server(identify=hook), mode) as client:
        await client.call_tool("whoami", {})
    a = by_tool(sdk)["whoami"]
    assert a["crucible.mcp.identity.source"] == "identify_hook"
    assert a["user.id"] == "u-7"
    assert a["crucible.mcp.tenant"] == "acme"
    assert a["crucible.mcp.user.issuer"] == "https://issuer.test"
    assert a["crucible.mcp.user.subject"] == "sub-1"


@pytest.mark.anyio
async def test_a_verified_token_identifies_the_caller(sdk: Collector, mode: str) -> None:
    token = AccessToken(
        token="opaque-token-value", client_id="client-1", scopes=[], subject="sub-9", claims={"iss": "https://idp.test"}
    )
    # The bearer middleware of an HTTP server sets this once it has verified the token.
    reset = auth_context_var.set(AuthenticatedUser(token))
    try:
        async with connect(edge_server(), mode) as client:
            await client.call_tool("invalid", {"value": "x"})
    finally:
        auth_context_var.reset(reset)
    a = by_tool(sdk)["invalid"]
    assert a["crucible.mcp.identity.source"] == "token_verified"
    assert a["crucible.mcp.oauth.client_id"] == "client-1"
    assert a["crucible.mcp.user.issuer"] == "https://idp.test"
    assert a["crucible.mcp.user.subject"] == "sub-9"
    assert b"opaque-token-value" not in sdk.raw()


@pytest.mark.anyio
async def test_a_raising_identify_hook_does_not_fail_the_call(sdk: Collector, mode: str) -> None:
    def hook(ctx: ServerRequestContext[Any, Any]) -> crucible_sdk.mcp.Identity:
        raise ValueError("hook bug")

    async with connect(edge_server(identify=hook), mode) as client:
        result = await client.call_tool("invalid", {"value": "x"})
    assert result.is_error
    assert "crucible.mcp.identity.source" not in by_tool(sdk)["invalid"]


@pytest.mark.anyio
async def test_identity_none_sends_no_identity_key(sdk: Collector, mode: str) -> None:
    def hook(ctx: ServerRequestContext[Any, Any]) -> crucible_sdk.mcp.Identity:
        return crucible_sdk.mcp.Identity(user_id="u-1", handle_id="h-1")

    server = edge_server(identify=hook, identity="none", handle_params={"invalid": "value"})
    async with connect(server, mode) as client:
        await client.call_tool("whoami", {})
        await client.call_tool("invalid", {"value": "handle-1"})
    for span in mcp_spans(sdk):
        assert not set(span.attributes) & _keys.IDENTITY_KEYS


@pytest.mark.anyio
async def test_cancelled_by_notification(sdk: Collector) -> None:
    async with connect(reference_server.instrumented(), "legacy") as client:
        with pytest.raises(MCPError):
            await client.call_tool("slow", {"ms": 5000, "steps": 0}, read_timeout_seconds=0.2)
        await client.call_tool("echo", {"text": "after"})
    assert by_tool(sdk)["slow"]["crucible.mcp.cancel.source"] == "notification"


@pytest.mark.anyio
async def test_cancelled_by_stream_close(sdk: Collector) -> None:
    async def call(client: Client) -> None:
        with contextlib.suppress(Exception):
            await client.call_tool("slow", {"ms": 5000, "steps": 0})

    async with anyio.create_task_group() as tg, connect(reference_server.instrumented(), "legacy") as client:
        tg.start_soon(call, client)
        await anyio.sleep(0.3)
    assert by_tool(sdk)["slow"]["crucible.mcp.cancel.source"] == "stream_close"


@pytest.mark.anyio
async def test_a_removed_builtin_span_is_restored(sdk: Collector, mode: str) -> None:
    server = reference_server.build()
    server.middleware[:] = [m for m in server.middleware if not isinstance(m, _seam.OpenTelemetryMiddleware)]
    crucible_sdk.mcp.instrument(server)
    crucible_sdk.mcp.instrument(server)
    assert sum(isinstance(m, CrucibleMiddleware) for m in server.middleware) == 1
    async with connect(server, mode) as client:
        await client.call_tool("echo", {"text": "x"})
    [call] = mcp_spans(sdk, "tools/call")
    assert call.attributes["crucible.mcp.capture_source"] == "sdk"


@pytest.mark.anyio
async def test_no_planted_secret_reaches_the_collector(sdk: Collector) -> None:
    planted = ["jane.doe@example.com", "+1 415 555 0100", "4111111111111111", SECRET_KEY, "203.0.113.9"]
    async with connect(reference_server.instrumented(), "modern-stream") as client:
        await client.call_tool("secret", {"note": " / ".join(planted)})
        await client.call_tool("needs_input", {"item": "lamp"})
    async with connect(raising_server(), "legacy") as client:
        with pytest.raises(MCPError):
            await client.call_tool("anything", {})
    assert len(mcp_spans(sdk, "tools/call")) == 4
    raw = sdk.raw()
    for value in [*planted, reference_server.REQUEST_STATE]:
        assert value.encode() not in raw, value
