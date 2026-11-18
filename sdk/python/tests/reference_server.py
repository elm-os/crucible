"""The SDK conformance reference server: the seven tools of the runner contract, on `MCPServer`."""

from __future__ import annotations

import anyio
from mcp.server.mcpserver import Context, MCPServer
from mcp_types import (
    CallToolResult,
    ElicitRequest,
    ElicitRequestFormParams,
    ElicitResult,
    InputRequiredResult,
    TextContent,
)

import crucible_sdk.mcp

SERVER_NAME = "crucible-conformance-server"
SERVER_VERSION = "1.2.0"
REQUEST_STATE = "crucible-conformance-request-state-1"


def build() -> MCPServer:
    server = MCPServer(SERVER_NAME, version=SERVER_VERSION)

    @server.tool()
    def echo(text: str) -> str:
        return text

    @server.tool()
    def fail(reason: str) -> CallToolResult:
        crucible_sdk.mcp.annotate_error("upstream")
        return CallToolResult(content=[TextContent(type="text", text=reason)], is_error=True)

    @server.tool()
    def throw(message: str) -> str:
        raise RuntimeError(message)

    @server.tool()
    async def slow(ms: int, steps: int, ctx: Context) -> str:
        for step in range(steps):
            await ctx.report_progress(step + 1, steps)
            await anyio.sleep(ms / 1000 / steps)
        if steps == 0:
            await anyio.sleep(ms / 1000)
        return "done"

    @server.tool()
    def secret(note: str) -> str:
        return note

    @server.tool()
    def needs_input(item: str, ctx: Context) -> InputRequiredResult | str:
        response = (ctx.input_responses or {}).get("confirm")
        if ctx.request_state is None or not isinstance(response, ElicitResult) or response.action != "accept":
            ask = ElicitRequestFormParams(
                message=f"Confirm {item}?",
                requested_schema={"type": "object", "properties": {"confirmed": {"type": "boolean"}}},
            )
            return InputRequiredResult(
                input_requests={"confirm": ElicitRequest(params=ask)}, request_state=REQUEST_STATE
            )
        return f"confirmed {item}"

    @server.tool()
    def lookup_order(order_id: str) -> str:
        return f"order {order_id}: shipped"

    return server


def instrumented() -> MCPServer:
    return crucible_sdk.mcp.instrument(
        build(),
        capture=[crucible_sdk.mcp.Capture(tool="secret")],
        handle_params={"lookup_order": "order_id"},
    )
