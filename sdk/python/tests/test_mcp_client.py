"""The client wrapper: trace context in `params._meta`, and Crucible keys on the mcp library's CLIENT span."""

from __future__ import annotations

from typing import cast

import pytest
from mcp_types import RequestParamsMeta
from opentelemetry import trace

import crucible_sdk
import crucible_sdk.mcp
from conformance import reference_server
from tests.collector import Collector
from tests.test_mcp_server import CLIENT, MODES, connect, edge_server, era, mcp_spans

TRACER = trace.get_tracer("tests")


def traceparent(span: trace.Span) -> str:
    context = span.get_span_context()
    return f"00-{context.trace_id:032x}-{context.span_id:016x}-{context.trace_flags:02x}"


@pytest.mark.anyio
@pytest.mark.parametrize("mode", MODES)
async def test_the_server_receives_the_callers_trace_context(sdk: Collector, mode: str) -> None:
    async with connect(edge_server(), mode) as client:
        traced = crucible_sdk.mcp.instrument_client(client)
        with TRACER.start_as_current_span("agent step") as step:
            result = await traced.call_tool("received_traceparent", {})
    received = result.content[0].text  # type: ignore[union-attr]
    step_context = step.get_span_context()
    if mode == "modern-direct":
        # In process the mcp client opens no span, so the wrapper's injection is what the server receives.
        assert received == traceparent(step)
        return
    # Over streams the mcp client's own CLIENT span is the parent it sends; that span is the step's child.
    [client_span] = mcp_spans(sdk, "tools/call", kind=CLIENT)
    assert received == f"00-{step_context.trace_id:032x}-{client_span.span_id}-{step_context.trace_flags:02x}"
    assert client_span.parent_span_id == f"{step_context.span_id:016x}"


@pytest.mark.anyio
@pytest.mark.parametrize("mode", ["legacy", "modern-stream"])
async def test_the_library_client_span_is_tagged_not_duplicated(sdk: Collector, mode: str) -> None:
    async with connect(reference_server.instrumented(), mode) as client:
        traced = crucible_sdk.mcp.instrument_client(client, transport="stdio")
        await traced.call_tool("echo", {"text": "x"})
    calls = mcp_spans(sdk, "tools/call", kind=CLIENT)
    assert len(calls) == 1
    a = calls[0].attributes
    assert a["crucible.mcp.capture_source"] == "sdk"
    assert a["crucible.mcp.era"] == era(mode)
    assert a["crucible.mcp.transport"] == "stdio"
    if mode == "legacy":
        # A modern-era client learns the server's name only from a response, after the first call started.
        assert a["crucible.mcp.server.name"] == reference_server.SERVER_NAME
    assert a["gen_ai.tool.name"] == "echo"
    assert a["mcp.protocol.version"] == ("2025-11-25" if mode == "legacy" else "2026-07-28")
    # The validation listing the client sends inside call_tool is tagged too, but never as a tool call.
    for listing in mcp_spans(sdk, "tools/list", kind=CLIENT):
        assert listing.attributes["crucible.mcp.capture_source"] == "sdk"
        assert "gen_ai.tool.name" not in listing.attributes


@pytest.mark.anyio
async def test_an_untraced_call_carries_no_trace_context(sdk: Collector) -> None:
    async with connect(edge_server(), "modern-direct") as client:
        result = await crucible_sdk.mcp.instrument_client(client).call_tool("received_traceparent", {})
    assert result.content[0].text == ""  # type: ignore[union-attr]


def test_with_trace_context_keeps_the_callers_meta_and_writes_no_baggage(sdk: Collector) -> None:
    from opentelemetry import baggage, context

    token = context.attach(baggage.set_baggage("user", "secret-user"))
    try:
        with TRACER.start_as_current_span("step") as step:
            meta = crucible_sdk.mcp.with_trace_context(cast("RequestParamsMeta", {"progressToken": 7}))
    finally:
        context.detach(token)
    assert meta == {"progressToken": 7, "traceparent": traceparent(step)}
    assert crucible_sdk.mcp.with_trace_context(None) is None
