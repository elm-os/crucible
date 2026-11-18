"""MCP instrumentation for servers and clients built on the official `mcp` package, 2.x (extra `mcp`).

`instrument(server)` adds one middleware that enriches the SERVER span the mcp library already opens for every
inbound message; it never opens a second span. Inside a handler, `annotate_error`, `mark_validation`,
`mark_not_found` and `identify` refine the operation being handled; outside one they do nothing.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any, Literal, TypeVar

try:
    import mcp  # noqa: F401
except ImportError as error:
    raise ImportError(
        "crucible_sdk.mcp needs the mcp package, 2.3 or newer: pip install 'crucible-sdk[mcp]'"
    ) from error

from mcp import Client
from mcp.server import MCPServer, Server

from crucible_sdk.mcp import _seam
from crucible_sdk.mcp._client import InstrumentedClient, with_trace_context
from crucible_sdk.mcp._middleware import (
    OPERATION,
    Capture,
    CrucibleMiddleware,
    ErrorSubclass,
    IdentifyHook,
    Identity,
    Transport,
)

__all__ = [
    "Capture",
    "ErrorSubclass",
    "IdentifyHook",
    "Identity",
    "InstrumentedClient",
    "Transport",
    "annotate_error",
    "identify",
    "instrument",
    "instrument_client",
    "mark_not_found",
    "mark_validation",
    "with_trace_context",
]

S = TypeVar("S", Server[Any], MCPServer[Any])

_SUBCLASSES = ("auth", "rate_limit", "upstream", "business")


def instrument(
    server: S,
    *,
    capture: Sequence[Capture] = (),
    handle_params: Mapping[str, str] | None = None,
    identify: IdentifyHook | None = None,
    identity: Literal["auto", "none"] = "auto",
    transport: Transport | None = None,
    server_name: str | None = None,
    server_version: str | None = None,
) -> S:
    """Instruments an `mcp` `Server` or `MCPServer` in place and returns it.

    Args:
        capture: Content capture rules (off when empty). Arguments and results of matching tools are sampled,
            capped and scrubbed before they are attached.
        handle_params: Tool name to the argument whose string value is the call's handle id (contract 9.8).
        identify: Called with the request context of each inbound message; returns an `Identity` or None. May be
            async.
        identity: "none" sends no identity keys at all: no user, tenant, handle, token, user agent or IP prefix.
        transport: The transport label; by default HTTP is detected from the request and anything else is stdio.
        server_name: Defaults to the server's name.
        server_version: Defaults to the server's version.
    """
    chain = _seam.middleware_of(server)
    chain[:] = [m for m in chain if not isinstance(m, CrucibleMiddleware)]
    builtin = next((i for i, m in enumerate(chain) if isinstance(m, _seam.OpenTelemetryMiddleware)), None)
    if builtin is None:
        # The built-in span was removed: put it back, so there is still exactly one span per message.
        chain.insert(0, _seam.OpenTelemetryMiddleware())
        builtin = 0
    name, version = _seam.name_and_version(server)
    chain.insert(
        builtin + 1,
        CrucibleMiddleware(
            server_name=server_name or name,
            server_version=version if server_version is None else server_version,
            capture=capture,
            handle_params=handle_params or {},
            identify=identify,
            identity=identity,
            transport=transport,
        ),
    )
    return server


def instrument_client(client: Client, *, transport: str | None = None) -> InstrumentedClient:
    """Wraps an `mcp.Client` so that its calls carry the caller's trace context and Crucible's client keys."""
    return InstrumentedClient(client, transport=transport)


def annotate_error(subclass: ErrorSubclass) -> None:
    """Refines a tool error (`isError: true`) of the current operation: auth, rate_limit, upstream or business."""
    if subclass not in _SUBCLASSES:
        raise ValueError(f"annotate_error: subclass must be one of {', '.join(_SUBCLASSES)}, got {subclass!r}")
    operation = OPERATION.get()
    if operation is not None:
        operation.subclass = subclass


def mark_validation() -> None:
    """Classifies the current operation's tool error as a validation failure (contract 9.2, row 10)."""
    operation = OPERATION.get()
    if operation is not None:
        operation.validation = True


def mark_not_found() -> None:
    """Classifies the current operation's tool error or -32602 error as not found (contract 9.2, rows 7 and 11)."""
    operation = OPERATION.get()
    if operation is not None:
        operation.not_found = True


def identify(
    *,
    user_id: str | None = None,
    issuer: str | None = None,
    subject: str | None = None,
    tenant: str | None = None,
    handle_id: str | None = None,
) -> None:
    """Sets who made the current operation, from inside its handler (contract 9.3)."""
    operation = OPERATION.get()
    if operation is None:
        return
    values = {"user_id": user_id, "issuer": issuer, "subject": subject, "tenant": tenant, "handle_id": handle_id}
    operation.identity.update({k: v for k, v in values.items() if v})
