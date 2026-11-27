"""Every use of mcp's provisional or private surface, in one module (mcp >= 2.3, < 3).

`ServerMiddleware` is provisional in mcp 2.x, so anything that depends on its shape or on mcp internals lives here
and is pinned by tests.
"""

from __future__ import annotations

import re
from collections.abc import MutableSequence
from typing import Any, Literal

import anyio
from mcp.server import MCPServer, Server, ServerRequestContext
from mcp.server._otel import OpenTelemetryMiddleware
from mcp.server.auth.middleware.auth_context import get_access_token
from mcp.server.auth.provider import principal_components
from mcp_types import CLIENT_INFO_META_KEY
from mcp_types.version import MODERN_PROTOCOL_VERSIONS

__all__ = [
    "CLIENT_INFO_META_KEY",
    "MODERN_PROTOCOL_VERSIONS",
    "OpenTelemetryMiddleware",
    "cancel_requested",
    "middleware_of",
    "name_and_version",
    "refusal",
    "token_identity",
]

# pydantic's text for a model that refused its input: "1 validation error for echoArguments", ...
_VALIDATION_ERRORS = re.compile(r"[0-9]+ validation errors? for ")


def _lowlevel(server: Server[Any] | MCPServer[Any]) -> Server[Any]:
    # MCPServer keeps its low-level server private (mcp's own in-memory transport reads it the same way).
    return server._lowlevel_server if isinstance(server, MCPServer) else server  # pyright: ignore[reportPrivateUsage]


def middleware_of(server: Server[Any] | MCPServer[Any]) -> MutableSequence[Any]:
    """The server's middleware chain, outermost first. The built-in `OpenTelemetryMiddleware` opens the span."""
    return server.middleware


def name_and_version(server: Server[Any] | MCPServer[Any]) -> tuple[str, str]:
    low = _lowlevel(server)
    return low.name, low.version or ""


def cancel_requested(ctx: ServerRequestContext[Any, Any]) -> bool:
    """Whether the peer sent `notifications/cancelled` for this request.

    The dispatcher sets this event before it cancels the handler, but the request context does not expose it; the
    session holds the dispatch context privately.
    """
    dispatch = getattr(ctx.session, "_request_outbound", None)
    event = getattr(dispatch, "cancel_requested", None)
    return isinstance(event, anyio.Event) and event.is_set()


def refusal(tool: str, text: str) -> Literal["validation", "not_found"] | None:
    """Why `MCPServer` refused a `tools/call` before the tool ran, from the text of the isError result it answers
    with: no tool has the name, or the arguments failed the tool's input model. None for any other tool error."""
    if text == f"Unknown tool: {tool}":
        return "not_found"
    prefix = f"Error executing tool {tool}: "
    if text.startswith(prefix) and _VALIDATION_ERRORS.match(text, len(prefix)):
        return "validation"
    return None


def token_identity() -> tuple[str, str | None, str | None] | None:
    """The (client id, issuer, subject) of the access token the hosting server verified, if any."""
    token = get_access_token()
    return None if token is None else principal_components(token)
