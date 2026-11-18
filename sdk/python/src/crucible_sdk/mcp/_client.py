"""The MCP client wrapper: trace context in `params._meta`, and Crucible keys on the mcp library's CLIENT span.

Over its stream transports the mcp client opens one CLIENT span per request and writes that span's trace context into
`params._meta` itself. The wrapper never opens a second span with `mcp.method.name` (that would be a second MCP row
per call); it tags the library's span as it starts and writes the caller's trace context into `_meta` so that the
in-process transport, which opens no span, propagates it too.
"""

from __future__ import annotations

from collections.abc import Iterator, Mapping
from contextlib import contextmanager
from types import TracebackType
from typing import Any, cast

from mcp import Client
from mcp.client.stdio import StdioServerParameters
from mcp_types import CallToolResult, GetPromptResult, ReadResourceResult, RequestParamsMeta
from opentelemetry.trace.propagation.tracecontext import TraceContextTextMapPropagator
from opentelemetry.util.types import AttributeValue

from crucible_sdk import _context, _keys
from crucible_sdk.mcp import _seam

# Only W3C trace context: the global propagator also writes baggage, which is never sent (contract 9.1).
_PROPAGATOR = TraceContextTextMapPropagator()


def with_trace_context(meta: RequestParamsMeta | None) -> RequestParamsMeta | None:
    """`meta` with `traceparent` (and `tracestate`) of the current span added; unchanged when there is none."""
    carrier: dict[str, Any] = dict(meta) if meta else {}
    _PROPAGATOR.inject(carrier)
    return cast("RequestParamsMeta", carrier) if carrier else meta


def _transport_of(server: object) -> str:
    if isinstance(server, str):
        return "streamable_http"
    if isinstance(server, StdioServerParameters):
        return "stdio"
    return "unknown"


class InstrumentedClient:
    """Wraps an `mcp.Client`. Every attribute not defined here is the wrapped client's."""

    def __init__(self, client: Client, *, transport: str | None = None) -> None:
        self._client = client
        self._transport = transport or _transport_of(client.server)

    def __getattr__(self, name: str) -> Any:
        return getattr(self._client, name)

    async def __aenter__(self) -> InstrumentedClient:
        await self._client.__aenter__()
        return self

    async def __aexit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        await self._client.__aexit__(exc_type, exc, tb)

    @contextmanager
    def _tagging(self, extra: Mapping[str, AttributeValue]) -> Iterator[None]:
        version = self._client.protocol_version
        tags: dict[str, AttributeValue] = {
            _keys.CAPTURE_SOURCE: "sdk",
            _keys.ERA: "modern" if version in _seam.MODERN_PROTOCOL_VERSIONS else "legacy",
            _keys.TRANSPORT: self._transport,
            _keys.MCP_PROTOCOL_VERSION: version,
            **extra,
        }
        info = self._client.server_info
        if info is not None:
            tags[_keys.SERVER_NAME] = info.name
            if info.version:
                tags[_keys.SERVER_VERSION] = info.version
        token = _context.CLIENT_CALL.set(tags)
        try:
            yield
        finally:
            _context.CLIENT_CALL.reset(token)

    async def call_tool(
        self,
        name: str,
        arguments: dict[str, Any] | None = None,
        *args: Any,
        meta: RequestParamsMeta | None = None,
        **kwargs: Any,
    ) -> CallToolResult:
        """`Client.call_tool`, with the caller's trace context in `_meta`."""
        with self._tagging({_keys.TOOL_NAME: name}):
            return await self._client.call_tool(name, arguments, *args, meta=with_trace_context(meta), **kwargs)

    async def get_prompt(
        self,
        name: str,
        arguments: dict[str, str] | None = None,
        *args: Any,
        meta: RequestParamsMeta | None = None,
        **kwargs: Any,
    ) -> GetPromptResult:
        """`Client.get_prompt`, with the caller's trace context in `_meta`."""
        with self._tagging({"gen_ai.prompt.name": name}):
            return await self._client.get_prompt(name, arguments, *args, meta=with_trace_context(meta), **kwargs)

    async def read_resource(self, uri: str, *args: Any, **kwargs: Any) -> ReadResourceResult:
        """`Client.read_resource`. `_meta` is left as given: a request with `_meta` skips the client's cache."""
        with self._tagging({}):
            return await self._client.read_resource(uri, *args, **kwargs)
