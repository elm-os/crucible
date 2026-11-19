"""The server middleware: it enriches the mcp library's own SERVER span with the contract 9.1 keys.

It runs directly inside the built-in `OpenTelemetryMiddleware`, ahead of every other middleware, so it sees each
inbound message in its wire form (including requests a later middleware refuses) and never opens a span of its own.
Request-side keys are set before the handler runs; outcome keys after. Instrumentation never fails a call: every
step that reads untrusted params or user hooks is guarded.
"""

from __future__ import annotations

import dataclasses
import fnmatch
import hashlib
import hmac
import inspect
import ipaddress
import json
import logging
from collections.abc import Awaitable, Callable, Mapping, Sequence
from contextvars import ContextVar
from dataclasses import dataclass, field
from typing import Any, Literal

import anyio
from mcp.server import ServerRequestContext
from mcp.server.context import CallNext, HandlerResult
from mcp.shared.exceptions import MCPError
from opentelemetry import trace
from opentelemetry.trace import Span
from opentelemetry.util.types import AttributeValue
from pydantic import BaseModel, ValidationError

from crucible_sdk import _keys, _pipeline, _scrub
from crucible_sdk.mcp import _seam

logger = logging.getLogger("crucible_sdk")

INVALID_PARAMS = -32602
HANDLE_ID_BYTES = 256
TARGET_NAME_BYTES = 512
MRTR_LABEL = b"crucible.mrtr.v1\x00"

Transport = Literal["stdio", "streamable_http", "sse"]
ErrorSubclass = Literal["auth", "rate_limit", "upstream", "business"]


@dataclass(frozen=True)
class Capture:
    """A content capture rule (contract 9.10): which tools, which fields, how often and how much."""

    tool: str = "*"
    """A glob over tool names; the first rule whose glob matches applies."""
    args: bool = True
    result: bool = True
    sample_rate: float = 1.0
    """The fraction of calls captured, decided before anything is copied."""
    max_bytes: int = 16384
    """The cap per field, in UTF-8 bytes of the scrubbed JSON text."""

    def __post_init__(self) -> None:
        if not 0.0 <= self.sample_rate <= 1.0:
            raise ValueError(f"Capture: sample_rate must be between 0 and 1, got {self.sample_rate!r}")
        if self.max_bytes < 1:
            raise ValueError(f"Capture: max_bytes must be at least 1, got {self.max_bytes!r}")


@dataclass(frozen=True)
class Identity:
    """Who made a call (contract 9.3). `user_id` reaches Crucible only from here."""

    user_id: str | None = None
    issuer: str | None = None
    subject: str | None = None
    tenant: str | None = None
    handle_id: str | None = None


IdentifyHook = Callable[[ServerRequestContext[Any, Any]], "Identity | Awaitable[Identity | None] | None"]


@dataclass
class Operation:
    """What a handler adds to its operation through the module functions."""

    validation: bool = False
    not_found: bool = False
    subclass: ErrorSubclass | None = None
    identity: dict[str, str] = field(default_factory=dict)


OPERATION: ContextVar[Operation | None] = ContextVar("crucible_sdk.mcp.operation", default=None)


def mrtr_hash(secret_key: str, request_state: str) -> str:
    """The contract 9.6 hash of a `requestState`, keyed with the sender's secret key."""
    digest = hmac.new(secret_key.encode(), MRTR_LABEL + request_state.encode(), hashlib.sha256).digest()
    return digest[:16].hex()


def session_hash(session_id: str) -> str:
    """The contract 9.4 form of a session id: the first 16 bytes of its SHA-256, in hex."""
    return hashlib.sha256(session_id.encode()).digest()[:16].hex()


def ip_prefix(host: str) -> str | None:
    """The /24 (IPv4) or /48 (IPv6) network of an address; the address itself is never sent."""
    try:
        address = ipaddress.ip_address(host)
    except ValueError:
        return None
    if isinstance(address, ipaddress.IPv6Address) and address.ipv4_mapped is not None:
        address = address.ipv4_mapped
    bits = 24 if address.version == 4 else 48
    network = ipaddress.ip_network(f"{address}/{bits}", strict=False)
    return f"{network.network_address}/{bits}"


def capabilities_hash(capabilities: object) -> int:
    """A stable 63-bit hash of the client's capabilities (canonical JSON, SHA-256), so it fits an OTLP int."""
    if isinstance(capabilities, BaseModel):
        capabilities = capabilities.model_dump(mode="json", by_alias=True, exclude_none=True)
    canonical = json.dumps(capabilities, sort_keys=True, separators=(",", ":"), default=str)
    return int.from_bytes(hashlib.sha256(canonical.encode()).digest()[:8], "big") >> 1


def _cut(text: str, max_bytes: int) -> str:
    return text.encode()[:max_bytes].decode("utf-8", "ignore")


def _field(result: object, wire: str, attr: str) -> Any:
    if isinstance(result, Mapping):
        return result.get(wire)
    return getattr(result, attr, None)


def _block_type(block: object) -> object:
    return block.get("type") if isinstance(block, Mapping) else getattr(block, "type", None)


class CrucibleMiddleware:
    """An mcp `ServerMiddleware` that enriches the built-in span; see `crucible_sdk.mcp.instrument`."""

    def __init__(
        self,
        *,
        server_name: str,
        server_version: str,
        capture: Sequence[Capture],
        handle_params: Mapping[str, str],
        identify: IdentifyHook | None,
        identity: Literal["auto", "none"],
        transport: Transport | None,
    ) -> None:
        self._server = {_keys.SERVER_NAME: server_name, _keys.SERVER_VERSION: server_version}
        self._capture = tuple(capture)
        self._capture_by_tool: dict[str, Capture | None] = {}
        self._handle_params = dict(handle_params)
        self._identify = identify
        self._identity = identity != "none"
        self._transport = transport

    async def __call__(self, ctx: ServerRequestContext[Any, Any], call_next: CallNext) -> HandlerResult:
        span = trace.get_current_span()
        if not span.is_recording():
            return await call_next(ctx)
        operation = Operation()
        capture: Capture | None = None
        try:
            capture = self._before(ctx, span)
        except Exception:  # instrumentation must never fail a call
            logger.debug("crucible_sdk: could not annotate %s", ctx.method, exc_info=True)
        if self._identify is not None and self._identity:
            await self._run_identify_hook(ctx, span)
        token = OPERATION.set(operation)
        try:
            result = await call_next(ctx)
        except MCPError as error:
            self._guard(self._after_error, ctx, span, operation, error.error.code)
            raise
        except ValidationError:
            self._guard(self._after_error, ctx, span, operation, INVALID_PARAMS)
            raise
        except anyio.get_cancelled_exc_class():
            self._guard(self._after_cancel, ctx, span, operation)
            raise
        except Exception:
            self._guard(self._after_exception, ctx, span, operation)
            raise
        finally:
            OPERATION.reset(token)
        self._guard(self._after_result, ctx, span, operation, result, capture)
        return result

    def _guard(self, step: Callable[..., None], *args: Any) -> None:
        try:
            step(*args)
        except Exception:  # instrumentation must never fail a call
            logger.debug("crucible_sdk: could not annotate an outcome", exc_info=True)

    # Before the handler.

    def _before(self, ctx: ServerRequestContext[Any, Any], span: Span) -> Capture | None:
        modern = ctx.protocol_version in _seam.MODERN_PROTOCOL_VERSIONS
        core: dict[str, AttributeValue] = {
            _keys.CAPTURE_SOURCE: "sdk",
            _keys.ERA: "modern" if modern else "legacy",
            _keys.TRANSPORT: self._transport_of(ctx),
            **self._server,
        }
        if ctx.request_id is None:
            core[_keys.IS_NOTIFICATION] = True
        span.set_attributes(core)
        params = ctx.params if isinstance(ctx.params, Mapping) else {}
        attributes: dict[str, AttributeValue] = {}
        attributes.update(_safely(self._client_info, ctx, modern))
        attributes.update(_safely(self._http, ctx, modern))
        attributes.update(_safely(self._state_in, params))
        if self._identity:
            attributes.update(_safely(self._token_identity))
        capture: Capture | None = None
        tool = params.get("name")
        if ctx.method == "tools/call" and ctx.request_id is not None and isinstance(tool, str):
            arguments = params.get("arguments")
            if self._identity:
                attributes.update(_safely(self._handle_param, tool, arguments))
            capture = self._capture_for(tool, span)
            if capture is not None and capture.args:
                text, cut = _pipeline.SCRUBBER.capped_json(
                    arguments if arguments is not None else {}, capture.max_bytes
                )
                attributes[_keys.TOOL_CALL_ARGUMENTS] = text
                attributes.update(self._capture_flags(capture, cut))
        span.set_attributes(attributes)
        return capture

    @staticmethod
    def _state_in(params: Mapping[str, Any]) -> dict[str, AttributeValue]:
        state = params.get("requestState")
        key = _pipeline.secret_key()
        return {_keys.MRTR_STATE_IN: mrtr_hash(key, state)} if isinstance(state, str) and key else {}

    def _transport_of(self, ctx: ServerRequestContext[Any, Any]) -> Transport:
        if self._transport is not None:
            return self._transport
        request = ctx.request
        if request is None:
            return "stdio"
        query = getattr(request, "query_params", None)
        return "sse" if query is not None and "session_id" in query else "streamable_http"

    def _client_info(self, ctx: ServerRequestContext[Any, Any], modern: bool) -> dict[str, AttributeValue]:
        info: object = None
        capabilities: object = None
        if ctx.method == "initialize" and isinstance(ctx.params, Mapping):
            info, capabilities = ctx.params.get("clientInfo"), ctx.params.get("capabilities")
        else:
            client = ctx.session.client_params
            info = client.client_info if client is not None else None
            capabilities = ctx.session.client_capabilities
            if info is None and modern and isinstance(ctx.meta, Mapping):
                info = ctx.meta.get(_seam.CLIENT_INFO_META_KEY)
        out: dict[str, AttributeValue] = {}
        name, version = _field(info, "name", "name"), _field(info, "version", "version")
        if isinstance(name, str) and name:
            out[_keys.CLIENT_NAME] = name
        if isinstance(version, str) and version:
            out[_keys.CLIENT_VERSION] = version
        if capabilities is not None:
            out[_keys.CLIENT_CAPABILITIES_HASH] = capabilities_hash(capabilities)
        return out

    def _http(self, ctx: ServerRequestContext[Any, Any], modern: bool) -> dict[str, AttributeValue]:
        request = ctx.request
        headers = getattr(request, "headers", None)
        if request is None or headers is None:
            return {}
        out: dict[str, AttributeValue] = {}
        if not modern:
            query = getattr(request, "query_params", None)
            raw = headers.get("mcp-session-id") or (query.get("session_id") if query is not None else None)
            if raw:
                out[_keys.MCP_SESSION_ID] = session_hash(raw)
        length = headers.get("content-length")
        if length is not None and length.isdigit():
            out[_keys.REQUEST_BYTES] = int(length)
        if self._identity:
            agent = headers.get("user-agent")
            if agent:
                out[_keys.USER_AGENT] = agent
            client = getattr(request, "client", None)
            prefix = ip_prefix(client.host) if client is not None and client.host else None
            if prefix is not None:
                out[_keys.CLIENT_IP_PREFIX] = prefix
        return out

    def _token_identity(self) -> dict[str, AttributeValue]:
        token = _seam.token_identity()
        if token is None:
            return {}
        client_id, issuer, subject = token
        out: dict[str, AttributeValue] = {_keys.IDENTITY_SOURCE: "token_verified"}
        if client_id:
            out[_keys.OAUTH_CLIENT_ID] = client_id
        if issuer:
            out[_keys.USER_ISSUER] = issuer
        if subject:
            out[_keys.USER_SUBJECT] = subject
        return out

    def _handle_param(self, tool: str, arguments: object) -> dict[str, AttributeValue]:
        name = self._handle_params.get(tool)
        if name is None or not isinstance(arguments, Mapping):
            return {}
        value = arguments.get(name)
        if not isinstance(value, str) or not value:
            return {}
        return {_keys.HANDLE_ID: _cut(_pipeline.SCRUBBER.bounded_text(value, HANDLE_ID_BYTES), HANDLE_ID_BYTES)}

    def _capture_for(self, tool: str, span: Span) -> Capture | None:
        if len(tool) > TARGET_NAME_BYTES:
            # Longer than any stored target name (contract 3.5): never a registered tool, never matched or cached.
            return None
        if tool not in self._capture_by_tool:
            if len(self._capture_by_tool) > 4096:
                self._capture_by_tool.clear()
            self._capture_by_tool[tool] = next((c for c in self._capture if fnmatch.fnmatchcase(tool, c.tool)), None)
        rule = self._capture_by_tool[tool]
        if rule is None or not (rule.args or rule.result):
            return None
        # Sampling before any copy: the span id is random, so its bits decide without another random draw.
        sample = (span.get_span_context().span_id & ((1 << 53) - 1)) / (1 << 53)
        return rule if sample < rule.sample_rate else None

    @staticmethod
    def _capture_flags(rule: Capture, cut: bool) -> dict[str, AttributeValue]:
        flags: dict[str, AttributeValue] = {
            _keys.CONTENT_CAPTURED: True,
            _keys.CONTENT_SAMPLE_RATE: rule.sample_rate,
        }
        if cut:
            flags[_keys.CONTENT_TRUNCATION] = "cap_exceeded"
        return flags

    async def _run_identify_hook(self, ctx: ServerRequestContext[Any, Any], span: Span) -> None:
        assert self._identify is not None
        try:
            found = self._identify(ctx)
            if inspect.isawaitable(found):
                found = await found
            if isinstance(found, Identity):
                span.set_attributes(_identity_attributes(dataclasses.asdict(found)))
        except Exception:  # a failing hook must not fail the call
            logger.debug("crucible_sdk: the identify hook raised", exc_info=True)

    # After the handler.

    def _finish(self, span: Span, operation: Operation) -> None:
        if operation.identity and self._identity:
            span.set_attributes(_identity_attributes(operation.identity))

    def _after_result(
        self,
        ctx: ServerRequestContext[Any, Any],
        span: Span,
        operation: Operation,
        result: HandlerResult,
        capture: Capture | None,
    ) -> None:
        self._finish(span, operation)
        if ctx.request_id is None or result is None:
            return
        attributes: dict[str, AttributeValue] = {}
        result_type = _field(result, "resultType", "result_type")
        if result_type in ("input_required", "task"):
            attributes[_keys.RESULT_TYPE] = result_type
        state = _field(result, "requestState", "request_state")
        key = _pipeline.secret_key()
        if result_type == "input_required" and isinstance(state, str) and key:
            attributes[_keys.MRTR_STATE_OUT] = mrtr_hash(key, state)
        if ctx.method == "tools/call":
            if _field(result, "isError", "is_error") is True:
                if operation.validation:
                    attributes[_keys.ERROR_CLASS] = "validation"
                elif operation.not_found:
                    attributes[_keys.ERROR_CLASS] = "not_found"
                elif operation.subclass is not None:
                    attributes[_keys.ERROR_SUBCLASS] = operation.subclass
            content = _field(result, "content", "content")
            if isinstance(content, Sequence) and not isinstance(content, str):
                counts: dict[str, int] = {}
                for block in content:
                    kind = _block_type(block)
                    if isinstance(kind, str) and (kind in counts or len(counts) < 16):
                        counts[kind] = counts.get(kind, 0) + 1
                for kind, count in counts.items():
                    attributes[_keys.CONTENT_BLOCKS_PREFIX + kind] = count
            if capture is not None and capture.result and result_type != "input_required":
                attributes.update(self._result_capture(result, capture))
        span.set_attributes(attributes)

    def _result_capture(self, result: object, capture: Capture) -> dict[str, AttributeValue]:
        # Only the content: an envelope can carry `requestState` and `_meta`, which are never captured.
        if isinstance(result, BaseModel):
            result = result.model_dump(mode="json", by_alias=True, exclude_none=True)
            if _scrub.work_hooked():
                _scrub.note_work(len(json.dumps(result, ensure_ascii=False, separators=(",", ":"))), 0)
        if not isinstance(result, Mapping):
            return {}
        content = {k: result[k] for k in ("content", "structuredContent", "isError") if k in result}
        text, cut = _pipeline.SCRUBBER.capped_json(content, capture.max_bytes)
        attributes: dict[str, AttributeValue] = {_keys.TOOL_CALL_RESULT: text}
        attributes.update(self._capture_flags(capture, cut))
        return attributes

    def _after_error(self, ctx: ServerRequestContext[Any, Any], span: Span, operation: Operation, code: int) -> None:
        self._finish(span, operation)
        if code == INVALID_PARAMS and operation.not_found:
            span.set_attribute(_keys.ERROR_CLASS, "not_found")

    def _after_exception(self, ctx: ServerRequestContext[Any, Any], span: Span, operation: Operation) -> None:
        self._finish(span, operation)
        span.set_attribute(_keys.ERROR_CLASS, "internal")

    def _after_cancel(self, ctx: ServerRequestContext[Any, Any], span: Span, operation: Operation) -> None:
        self._finish(span, operation)
        source = "notification" if _seam.cancel_requested(ctx) else "stream_close"
        span.set_attribute(_keys.CANCEL_SOURCE, source)


def _safely(step: Callable[..., dict[str, AttributeValue]], *args: Any) -> dict[str, AttributeValue]:
    try:
        return step(*args)
    except Exception:  # instrumentation must never fail a call
        logger.debug("crucible_sdk: could not read %s", getattr(step, "__name__", step), exc_info=True)
        return {}


def _identity_attributes(values: Mapping[str, str | None]) -> dict[str, AttributeValue]:
    keys = {
        "user_id": _keys.MCP_USER_ID,
        "issuer": _keys.USER_ISSUER,
        "subject": _keys.USER_SUBJECT,
        "tenant": _keys.TENANT,
        "handle_id": _keys.HANDLE_ID,
    }
    out: dict[str, AttributeValue] = {}
    for name, key in keys.items():
        value = values.get(name)
        if isinstance(value, str) and value:
            out[key] = _cut(value, HANDLE_ID_BYTES) if name == "handle_id" else value
    if out:
        out[_keys.IDENTITY_SOURCE] = "identify_hook"
    return out
