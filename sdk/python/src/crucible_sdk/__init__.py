"""Crucible SDK for Python: export OpenTelemetry spans to Crucible, with agent helpers.

Call `init()` once at startup. The MCP server and client instrumentation is in `crucible_sdk.mcp` (extra `mcp`).
"""

from __future__ import annotations

import logging
import os

from crucible_sdk import _pipeline
from crucible_sdk._agent import (
    Observation,
    ObservationType,
    generation,
    observe,
    set_cost,
    set_finish_reasons,
    set_input,
    set_messages,
    set_output,
    set_usage,
)
from crucible_sdk._context import metadata, session, tags, user
from crucible_sdk._pipeline import ExportStats
from crucible_sdk._version import __version__

__all__ = [
    "ExportStats",
    "Observation",
    "ObservationType",
    "__version__",
    "export_stats",
    "flush",
    "generation",
    "init",
    "metadata",
    "observe",
    "session",
    "set_cost",
    "set_finish_reasons",
    "set_input",
    "set_messages",
    "set_output",
    "set_usage",
    "shutdown",
    "tags",
    "user",
]

logger = logging.getLogger("crucible_sdk")


def init(
    *,
    endpoint: str | None = None,
    secret_key: str | None = None,
    service_name: str | None = None,
    environment: str | None = None,
    release: str | None = None,
    deploy_id: str | None = None,
    sample_rate: float = 1.0,
    queue_size: int = 2048,
    queue_bytes: int = _pipeline.QUEUE_BYTES,
    flush_interval: float = 1.0,
) -> None:
    """Starts exporting spans to Crucible.

    Uses the global OpenTelemetry SDK `TracerProvider` when one is set, adding one span processor to it, and creates
    and sets one otherwise. Calling `init` again replaces the previous settings.

    Args:
        endpoint: Base URL of Crucible's OTLP HTTP listener; spans go to `<endpoint>/v1/traces`. Defaults to
            `CRUCIBLE_ENDPOINT`, then `http://localhost:4318`.
        secret_key: A project secret key with the `ingest` scope, sent as a Bearer token. Defaults to
            `CRUCIBLE_SECRET_KEY`.
        service_name: The resource `service.name`.
        environment: The resource `deployment.environment.name`.
        release: The resource `service.version`.
        deploy_id: The resource `crucible.deploy.id` (contract 9.7).
        sample_rate: The fraction of traces exported, decided by trace id.
        queue_size: Spans held for export; when the queue is full new spans are dropped and counted.
        queue_bytes: Bytes of spans held for export, estimated from their strings, the batch being exported
            included; beyond it new spans are dropped and counted too. Defaults to 8 MiB.
        flush_interval: Seconds between exports of a partial batch.
    """
    if not 0.0 <= sample_rate <= 1.0:
        raise ValueError(f"crucible_sdk.init: sample_rate must be between 0 and 1, got {sample_rate!r}")
    if queue_size < 1:
        raise ValueError(f"crucible_sdk.init: queue_size must be at least 1, got {queue_size!r}")
    if queue_bytes < 1:
        raise ValueError(f"crucible_sdk.init: queue_bytes must be at least 1, got {queue_bytes!r}")
    if not flush_interval > 0:
        raise ValueError(f"crucible_sdk.init: flush_interval must be positive, got {flush_interval!r}")
    key = secret_key or os.environ.get("CRUCIBLE_SECRET_KEY") or None
    if key is None:
        logger.warning(
            "crucible_sdk.init: no secret key, so Crucible will refuse the spans; pass secret_key= or set "
            "CRUCIBLE_SECRET_KEY to a project secret key with the ingest scope"
        )
    _pipeline.init(
        _pipeline.Config(
            endpoint=endpoint or os.environ.get("CRUCIBLE_ENDPOINT") or _pipeline.DEFAULT_ENDPOINT,
            secret_key=key,
            resource=_pipeline.default_resource(service_name, environment, release, deploy_id),
            sample_rate=sample_rate,
            queue_size=queue_size,
            flush_interval=flush_interval,
            queue_bytes=queue_bytes,
        )
    )


def flush(timeout: float = 5.0) -> bool:
    """Waits until every span ended before the call has been sent. Returns False when `timeout` seconds pass first."""
    return _pipeline.flush(timeout)


def shutdown(timeout: float = 5.0) -> None:
    """Sends the queued spans, waiting at most `timeout` seconds, and stops exporting. Later spans are not sent."""
    _pipeline.shutdown(timeout)


def export_stats() -> ExportStats:
    """Counters of the export queue since the last `init`."""
    return _pipeline.stats()
