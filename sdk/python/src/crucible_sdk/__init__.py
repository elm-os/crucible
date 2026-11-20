"""Crucible SDK for Python: export OpenTelemetry spans to Crucible, with agent helpers and feedback.

Call `init()` once at startup. The MCP server and client instrumentation is in `crucible_sdk.mcp` (extra `mcp`).
"""

from __future__ import annotations

import logging
import os
import re
from collections.abc import Mapping
from typing import Literal
from urllib.parse import urlsplit

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
    "feedback",
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

_UUID = re.compile(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")


def init(
    *,
    endpoint: str | None = None,
    secret_key: str | None = None,
    project_id: str | None = None,
    api_url: str | None = None,
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
        secret_key: A project secret key, sent as a Bearer token: the `ingest` scope sends spans, the `write` scope
            feedback; new keys have both. Defaults to `CRUCIBLE_SECRET_KEY`.
        project_id: The id of the key's project (a UUID), which `feedback` needs; `crucible admin create-key` prints
            it as `project_id`. Defaults to `CRUCIBLE_PROJECT_ID`.
        api_url: Base URL of Crucible's API listener, where `feedback` posts. Defaults to `CRUCIBLE_API_URL`, then
            `endpoint`. The API listener (port 8080) also takes spans; the OTLP listener (port 4318) does not serve
            the API.
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
    endpoint = endpoint or os.environ.get("CRUCIBLE_ENDPOINT") or _pipeline.DEFAULT_ENDPOINT
    feedback_url = _feedback_url(
        api_url or os.environ.get("CRUCIBLE_API_URL") or endpoint,
        project_id or os.environ.get("CRUCIBLE_PROJECT_ID") or None,
    )
    key = secret_key or os.environ.get("CRUCIBLE_SECRET_KEY") or None
    if key is None:
        logger.warning(
            "crucible_sdk.init: no secret key, so Crucible will refuse the spans; pass secret_key= or set "
            "CRUCIBLE_SECRET_KEY to a project secret key with the ingest scope"
        )
    _pipeline.init(
        _pipeline.Config(
            endpoint=endpoint,
            secret_key=key,
            resource=_pipeline.default_resource(service_name, environment, release, deploy_id),
            sample_rate=sample_rate,
            queue_size=queue_size,
            flush_interval=flush_interval,
            queue_bytes=queue_bytes,
            feedback_url=feedback_url,
        )
    )


def _feedback_url(api_url: str, project_id: str | None) -> str | None:
    parts = urlsplit(api_url)
    if parts.scheme not in ("http", "https") or not parts.netloc:
        raise ValueError(
            f"crucible_sdk.init: api_url {api_url!r} is not an http or https URL; pass api_url= or set "
            "CRUCIBLE_API_URL to the API listener, for example http://localhost:8080"
        )
    if project_id is None:
        return None
    if not _UUID.fullmatch(project_id):
        raise ValueError(
            f"crucible_sdk.init: project_id {project_id!r} is not a project id; pass the project's UUID, not its slug, "
            "or set CRUCIBLE_PROJECT_ID (crucible admin create-key prints it as project_id)"
        )
    return f"{api_url.rstrip('/')}/api/v1/projects/{project_id.lower()}/feedback"


def feedback(
    name: str,
    value: float | bool | str,
    *,
    trace_id: str | None = None,
    span_id: str | None = None,
    session_id: str | None = None,
    score_id: str | None = None,
    data_type: Literal["numeric", "boolean", "categorical", "text"] | None = None,
    comment: str | None = None,
    metadata: Mapping[str, object] | None = None,
    retracted: bool | None = None,
    idempotency_key: str | None = None,
) -> None:
    """Queues one score for the project's feedback route and returns at once; it never raises.

    The target is `trace_id`, `trace_id` with `span_id`, or `session_id`. Scores are sent in call order by a
    background thread, each after the spans that ended before it, and retried with the same `Idempotency-Key` when
    the request may succeed later. Without a project id, or when 1,024 scores are already waiting, the score is
    dropped; dropped, sent and refused scores are counted in `export_stats()`. Before `init` and after `shutdown` it
    does nothing.

    Args:
        name: The score name, for example `helpful`, without `#`. A later score of the same name (and `score_id`)
            replaces it.
        value: A number (`numeric`), a boolean (`boolean`) or a string (`categorical`, or `text` with `data_type`).
        trace_id: The trace, 32 hex characters.
        span_id: A span of the trace, 16 hex characters.
        session_id: A session, instead of a trace.
        score_id: Keeps scores of one name apart, for example one per reviewer.
        data_type: How Crucible reads `value`; by default from its type.
        comment: At most 4 KiB.
        metadata: A JSON object, at most 16 KiB.
        retracted: Removes the score of this name (and `score_id`) from the target.
        idempotency_key: Sent as `Idempotency-Key` (1 to 128 characters of `A-Z`, `a-z`, `0-9`, `_` and `-`):
            Crucible writes a score once per key and body for 24 hours. Defaults to a random key per call.
    """
    fields: dict[str, object | None] = {
        "trace_id": trace_id,
        "span_id": span_id,
        "session_id": session_id,
        "name": name,
        "score_id": score_id,
        "value": value,
        "data_type": data_type,
        "comment": comment,
        "metadata": dict(metadata) if metadata is not None else None,
        "retracted": retracted,
    }
    _pipeline.feedback({k: v for k, v in fields.items() if v is not None}, idempotency_key)


def flush(timeout: float = 5.0) -> bool:
    """Waits until every span ended and every score queued before the call has been sent. Returns False when `timeout`
    seconds pass first."""
    return _pipeline.flush(timeout)


def shutdown(timeout: float = 5.0) -> None:
    """Sends the queued spans and scores, waiting at most `timeout` seconds, and stops. Later spans and scores are not
    sent."""
    _pipeline.shutdown(timeout)


def export_stats() -> ExportStats:
    """Counters of the export queue and of feedback since the last `init`."""
    return _pipeline.stats()
