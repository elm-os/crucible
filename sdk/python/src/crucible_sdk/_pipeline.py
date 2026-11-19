"""The export path: one span processor, a bounded queue and a background exporter (contract 9.10).

`on_end` only appends to the queue; a full queue drops the span and counts it, and never blocks. One daemon thread
batches spans and sends them as OTLP HTTP protobuf to `/v1/traces`.

OpenTelemetry's global tracer provider can be set only once per process, so the processor is registered once and
`init` and `shutdown` replace the pipeline behind it.
"""

from __future__ import annotations

import atexit
import logging
import os
import threading
import time
import uuid
from collections import deque
from collections.abc import Mapping, Sequence
from dataclasses import dataclass

from opentelemetry import context as context_api
from opentelemetry import trace
from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import Event, ReadableSpan, Span, SpanProcessor, TracerProvider
from opentelemetry.sdk.trace.export import SpanExporter, SpanExportResult
from opentelemetry.trace import Status

from crucible_sdk import _context, _keys
from crucible_sdk._scrub import Scrubber
from crucible_sdk._version import __version__

logger = logging.getLogger("crucible_sdk")

DEFAULT_ENDPOINT = "http://localhost:4318"
BATCH_SIZE = 512
# The proxy's telemetry buffer size (contract 9.10).
QUEUE_BYTES = 8 << 20
EXPORT_TIMEOUT = 10.0

SCRUBBER = Scrubber.builtin()
# Error text of MCP spans is cut before it is scrubbed, which bounds the scrubbing work (contract 3.1 keeps 8 KiB of a
# status message).
STATUS_CHARS = 8192
EVENT_CHARS = 16384


@dataclass(frozen=True)
class ExportStats:
    """Counters of the current pipeline."""

    queued: int
    """Spans waiting in the queue."""
    dropped: int
    """Spans dropped because the queue was full."""
    exported: int
    """Spans the collector accepted."""
    failed: int
    """Spans in export requests that failed."""
    queued_bytes: int = 0
    """The estimated size of the queued spans and of the batch being exported (see `span_bytes`)."""


# What a span costs beyond its strings: ids, times, kind, status code, links and the resource and scope refs.
_SPAN_OVERHEAD_BYTES = 256


def _attribute_bytes(attributes: Mapping[str, object] | None) -> int:
    size = 0
    for key, value in (attributes or {}).items():
        size += len(key)
        if isinstance(value, str):
            size += len(value)
        elif isinstance(value, tuple | list):
            size += sum(len(item) if isinstance(item, str) else 8 for item in value)
        else:
            size += 8
    return size


def span_bytes(span: ReadableSpan) -> int:
    """A span's size, estimated from lengths already known (its name, attributes, events and status message) plus
    a fixed allowance; the attribute count limit bounds the work."""
    size = _SPAN_OVERHEAD_BYTES + len(span.name) + len(span.status.description or "")
    size += _attribute_bytes(span.attributes)
    for event in span.events:
        size += len(event.name) + _attribute_bytes(event.attributes)
    return size


@dataclass(frozen=True)
class Config:
    """Settings of one pipeline."""

    endpoint: str
    secret_key: str | None
    resource: Mapping[str, str]
    sample_rate: float
    queue_size: int
    flush_interval: float
    batch_size: int = BATCH_SIZE
    export_timeout: float = EXPORT_TIMEOUT
    queue_bytes: int = QUEUE_BYTES
    """Estimated bytes of the queued spans and the batch being exported (see `span_bytes`) held before dropping."""


def _exporter(config: Config) -> SpanExporter:
    headers = {"User-Agent": f"crucible-sdk-python/{__version__}"}
    if config.secret_key:
        headers["Authorization"] = f"Bearer {config.secret_key}"
    return OTLPSpanExporter(
        endpoint=config.endpoint.rstrip("/") + "/v1/traces", headers=headers, timeout=config.export_timeout
    )


class Pipeline:
    """A bounded queue drained by one export thread."""

    def __init__(self, config: Config, exporter: SpanExporter | None = None) -> None:
        self.config = config
        self._exporter = exporter if exporter is not None else _exporter(config)
        self._cond = threading.Condition(threading.Lock())
        self._queue: deque[tuple[ReadableSpan, int]] = deque()
        self._queued_bytes = 0
        self._accepted = 0
        self._handled = 0
        self._flush_target = 0
        self._dropped = 0
        self._exported = 0
        self._failed = 0
        self._stopping = False
        # `service.instance.id` is per process, so a pipeline started in a forked child gets its own.
        self._overlay = {**config.resource, _keys.SERVICE_INSTANCE_ID: str(uuid.uuid4())}
        self._resources: dict[int, tuple[Resource, Resource]] = {}
        self._thread = threading.Thread(target=self._run, name="crucible-sdk-export", daemon=True)
        self._thread.start()

    def enqueue(self, span: ReadableSpan) -> None:
        size = span_bytes(span)
        with self._cond:
            if self._stopping:
                return
            if len(self._queue) >= self.config.queue_size or self._queued_bytes + size > self.config.queue_bytes:
                self._dropped += 1
                first_drop = self._dropped == 1
            else:
                self._queue.append((span, size))
                self._queued_bytes += size
                self._accepted += 1
                if len(self._queue) >= self.config.batch_size:
                    self._cond.notify_all()
                return
        if first_drop:
            logger.warning(
                "crucible_sdk: the export queue is full (queue_size=%d spans, queue_bytes=%d bytes), so spans are "
                "being dropped; raise queue_size or queue_bytes in crucible_sdk.init() or check that %s accepts spans",
                self.config.queue_size,
                self.config.queue_bytes,
                self.config.endpoint,
            )

    def stats(self) -> ExportStats:
        with self._cond:
            return ExportStats(len(self._queue), self._dropped, self._exported, self._failed, self._queued_bytes)

    def flush(self, timeout: float) -> bool:
        """Waits until every span queued before the call has been sent, or `timeout` seconds pass."""
        deadline = time.monotonic() + timeout
        with self._cond:
            target = self._accepted
            self._flush_target = max(self._flush_target, target)
            self._cond.notify_all()
            while self._handled < target:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not self._thread.is_alive():
                    return False
                self._cond.wait(remaining)
        return True

    def shutdown(self, timeout: float) -> None:
        """Sends what is queued, waiting at most `timeout` seconds, then stops."""
        with self._cond:
            if self._stopping:
                return
            self._stopping = True
            self._flush_target = self._accepted
            self._cond.notify_all()
        self._thread.join(timeout)
        self._exporter.shutdown()

    def _ready(self) -> bool:
        return self._stopping or len(self._queue) >= self.config.batch_size or self._handled < self._flush_target

    def _run(self) -> None:
        while True:
            with self._cond:
                self._cond.wait_for(self._ready, timeout=self.config.flush_interval)
                if not self._queue:
                    if self._stopping:
                        return
                    continue
                batch = [self._queue.popleft() for _ in range(min(len(self._queue), self.config.batch_size))]
            ok = self._send([span for span, _ in batch])
            with self._cond:
                self._queued_bytes -= sum(size for _, size in batch)
                self._handled += len(batch)
                if ok:
                    self._exported += len(batch)
                else:
                    self._failed += len(batch)
                self._cond.notify_all()

    def _send(self, batch: Sequence[ReadableSpan]) -> bool:
        # The exporter's own HTTP spans, if anything instruments it, must not feed back into this queue.
        token = context_api.attach(context_api.set_value(context_api._SUPPRESS_INSTRUMENTATION_KEY, True))
        try:
            return self._exporter.export([self._prepare(span) for span in batch]) is SpanExportResult.SUCCESS
        except Exception:
            logger.debug("crucible_sdk: export failed", exc_info=True)
            return False
        finally:
            context_api.detach(token)

    def _prepare(self, span: ReadableSpan) -> ReadableSpan:
        """The span as Crucible receives it: the init resource overlaid, MCP error text scrubbed."""
        resource = self._resource(span.resource)
        is_mcp = _keys.MCP_METHOD in (span.attributes or {})
        if resource is span.resource and not is_mcp:
            return span
        status, events = span.status, span.events
        if is_mcp:
            if status.description:
                status = Status(status.status_code, SCRUBBER.bounded_text(status.description, STATUS_CHARS))
            events = tuple(_scrub_event(event) for event in events)
        return ReadableSpan(
            name=span.name,
            context=span.context,
            parent=span.parent,
            resource=resource,
            attributes=span.attributes,
            events=events,
            links=span.links,
            kind=span.kind,
            status=status,
            start_time=span.start_time,
            end_time=span.end_time,
            instrumentation_scope=span.instrumentation_scope,
        )

    def _resource(self, resource: Resource) -> Resource:
        cached = self._resources.get(id(resource))
        if cached is not None and cached[0] is resource:
            return cached[1]
        current = resource.attributes
        updates = {k: v for k, v in self._overlay.items() if current.get(k) != v}
        if _keys.SERVICE_INSTANCE_ID in current:
            updates.pop(_keys.SERVICE_INSTANCE_ID, None)
        merged = resource.merge(Resource(updates)) if updates else resource
        self._resources[id(resource)] = (resource, merged)
        return merged


def _scrub_event(event: Event) -> Event:
    if event.name != "exception" or not event.attributes:
        return event
    attributes = {
        k: SCRUBBER.bounded_text(v, EVENT_CHARS) if isinstance(v, str) else v for k, v in event.attributes.items()
    }
    return Event(event.name, attributes, event.timestamp)


class _State:
    lock = threading.Lock()
    pipeline: Pipeline | None = None
    provider: TracerProvider | None = None
    hooks_registered = False


class CrucibleProcessor(SpanProcessor):
    """The one processor the SDK registers. It copies the scope onto starting spans and queues ended ones."""

    def on_start(self, span: Span, parent_context: context_api.Context | None = None) -> None:
        if _State.pipeline is not None:
            _context.apply_to_span(span)

    def on_end(self, span: ReadableSpan) -> None:
        pipeline = _State.pipeline
        if pipeline is None:
            return
        rate = pipeline.config.sample_rate
        if rate < 1.0 and span.context is not None and (span.context.trace_id & 0xFFFFFFFFFFFFFFFF) >= rate * 2**64:
            return
        pipeline.enqueue(span)

    def shutdown(self) -> None:
        shutdown()

    def force_flush(self, timeout_millis: int = 30000) -> bool:
        return flush(timeout_millis / 1000)


PROCESSOR = CrucibleProcessor()


def _attach(resource: Mapping[str, str]) -> None:
    """Registers the processor on the global SDK provider, creating that provider when none is set."""
    provider = trace.get_tracer_provider()
    if isinstance(provider, trace.ProxyTracerProvider):
        created = TracerProvider(resource=Resource.create(dict(resource)))
        trace.set_tracer_provider(created)
        provider = trace.get_tracer_provider()
    if not isinstance(provider, TracerProvider):
        logger.warning(
            "crucible_sdk.init: the global tracer provider is a %s, not an opentelemetry-sdk TracerProvider, so no "
            "span reaches Crucible; set an opentelemetry.sdk.trace.TracerProvider as the global provider before "
            "calling crucible_sdk.init()",
            type(provider).__name__,
        )
        return
    if _State.provider is not provider:
        provider.add_span_processor(PROCESSOR)
        _State.provider = provider


def init(config: Config) -> None:
    with _State.lock:
        old, _State.pipeline = _State.pipeline, None
        if old is not None:
            old.shutdown(5.0)
        _attach(config.resource)
        _State.pipeline = Pipeline(config)
        if not _State.hooks_registered:
            _State.hooks_registered = True
            atexit.register(shutdown)
            if hasattr(os, "register_at_fork"):
                os.register_at_fork(after_in_child=_after_fork_in_child)


def flush(timeout: float = 5.0) -> bool:
    pipeline = _State.pipeline
    return True if pipeline is None else pipeline.flush(timeout)


def shutdown(timeout: float = 5.0) -> None:
    with _State.lock:
        pipeline, _State.pipeline = _State.pipeline, None
    if pipeline is not None:
        pipeline.shutdown(timeout)


def stats() -> ExportStats:
    pipeline = _State.pipeline
    return ExportStats(0, 0, 0, 0) if pipeline is None else pipeline.stats()


def secret_key() -> str | None:
    """The secret key of the current pipeline, which keys the MRTR state hashes (contract 9.6)."""
    pipeline = _State.pipeline
    return None if pipeline is None else pipeline.config.secret_key


def _after_fork_in_child() -> None:
    # Threads and held locks do not survive fork: start a fresh pipeline without touching the parent's locks.
    _State.lock = threading.Lock()
    old = _State.pipeline
    if old is not None:
        _State.pipeline = Pipeline(old.config)


def default_resource(
    service_name: str | None, environment: str | None, release: str | None, deploy_id: str | None
) -> dict[str, str]:
    values = {
        _keys.SERVICE_NAME: service_name,
        _keys.SERVICE_VERSION: release,
        _keys.ENVIRONMENT: environment,
        _keys.DEPLOY_ID: deploy_id,
    }
    return {k: v for k, v in values.items() if v}
