"""The export path: bounded, non-blocking, counted, and bounded in time when the collector misbehaves."""

from __future__ import annotations

import json
import logging
import os
import socket
import subprocess
import sys
import threading
import time
import warnings
from collections.abc import Sequence
from pathlib import Path

import pytest
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import ReadableSpan
from opentelemetry.sdk.trace.export import SpanExporter, SpanExportResult
from opentelemetry.trace import SpanContext, TraceFlags

import crucible_sdk
from crucible_sdk import _pipeline
from tests.collector import Collector, running
from tests.conftest import SECRET_KEY

ROOT = Path(__file__).resolve().parents[1]
RESOURCE = Resource.create({})


def span(i: int = 1) -> ReadableSpan:
    context = SpanContext(i + 1, i + 1, is_remote=False, trace_flags=TraceFlags(1))
    return ReadableSpan(name=f"s{i}", context=context, resource=RESOURCE)


class BlockingExporter(SpanExporter):
    def __init__(self) -> None:
        self.entered = threading.Event()
        self.release = threading.Event()
        self.exported: list[str] = []

    def export(self, spans: Sequence[ReadableSpan]) -> SpanExportResult:
        self.entered.set()
        self.release.wait(10)
        self.exported.extend(s.name for s in spans)
        return SpanExportResult.SUCCESS


def config(**overrides: object) -> _pipeline.Config:
    values: dict[str, object] = {
        "endpoint": "http://127.0.0.1:9",
        "secret_key": SECRET_KEY,
        "resource": {},
        "sample_rate": 1.0,
        "queue_size": 10,
        "flush_interval": 0.01,
        "batch_size": 5,
    }
    values.update(overrides)
    return _pipeline.Config(**values)  # type: ignore[arg-type]


def test_the_byte_bound_counts_the_batch_in_flight_and_drops_beyond_it() -> None:
    exporter = BlockingExporter()
    size = _pipeline.span_bytes(span(0))
    pipeline = _pipeline.Pipeline(config(queue_bytes=3 * size, batch_size=2), exporter)
    for i in range(5):
        pipeline.enqueue(span(i))
        if i == 1:
            assert exporter.entered.wait(5)
    stats = pipeline.stats()
    assert (stats.queued, stats.queued_bytes, stats.dropped) == (1, 3 * size, 2)
    exporter.release.set()
    assert pipeline.flush(5)
    pipeline.shutdown(5)


def test_a_full_queue_drops_and_counts_without_blocking(caplog: pytest.LogCaptureFixture) -> None:
    exporter = BlockingExporter()
    pipeline = _pipeline.Pipeline(config(), exporter)
    pipeline.enqueue(span(0))
    assert exporter.entered.wait(5)
    spans = [span(i) for i in range(1, 101)]
    start = time.perf_counter()
    with caplog.at_level(logging.WARNING, logger="crucible_sdk"):
        for item in spans:
            pipeline.enqueue(item)
    assert time.perf_counter() - start < 0.05
    stats = pipeline.stats()
    assert (stats.queued, stats.dropped) == (10, 90)
    assert "raise queue_size" in caplog.text
    exporter.release.set()
    assert pipeline.flush(5)
    pipeline.shutdown(5)
    assert exporter.exported == [f"s{i}" for i in range(11)]
    assert pipeline.stats().exported == 11


def test_flush_sends_everything_queued_before_it(sdk: Collector) -> None:
    for i in range(1200):
        with crucible_sdk.observe(f"span {i}"):
            pass
    assert crucible_sdk.flush()
    assert len(sdk.spans()) == 1200
    assert len(sdk.requests) == 3
    stats = crucible_sdk.export_stats()
    assert (stats.exported, stats.dropped, stats.failed, stats.queued) == (1200, 0, 0, 0)


def test_a_collector_that_never_answers_bounds_flush_and_shutdown() -> None:
    with running(Collector(hang=True)) as collector:
        crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, flush_interval=60)
        start = time.perf_counter()
        for _ in range(100):
            with crucible_sdk.observe("stuck"):
                pass
        assert time.perf_counter() - start < 0.5
        assert not crucible_sdk.flush(0.3)
        start = time.perf_counter()
        crucible_sdk.shutdown(0.3)
        assert time.perf_counter() - start < 2.0


def test_a_refused_collector_never_slows_span_end() -> None:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    pipeline = _pipeline.Pipeline(
        config(
            endpoint=f"http://127.0.0.1:{port}",
            queue_size=2048,
            batch_size=_pipeline.BATCH_SIZE,
            flush_interval=60.0,
            export_timeout=0.5,
        )
    )
    spans = [span(i) for i in range(500)]
    start = time.perf_counter()
    for item in spans:
        pipeline.enqueue(item)
    assert time.perf_counter() - start < 0.1
    assert pipeline.flush(10)
    pipeline.shutdown(1)
    assert pipeline.stats().failed == 500


def test_sample_rate_zero_exports_nothing(collector: Collector) -> None:
    crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, sample_rate=0.0, flush_interval=60)
    with crucible_sdk.observe("dropped"):
        pass
    assert crucible_sdk.flush()
    crucible_sdk.shutdown()
    assert collector.spans() == []


def test_init_again_replaces_the_settings(collector: Collector) -> None:
    with running() as other:
        crucible_sdk.init(endpoint=other.url, secret_key=SECRET_KEY, flush_interval=60)
        with crucible_sdk.observe("first"):
            pass
        crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, flush_interval=60)
        with crucible_sdk.observe("second"):
            pass
        assert crucible_sdk.flush()
        crucible_sdk.shutdown()
        assert [s.name for s in other.spans()] == ["first"]
        assert [s.name for s in collector.spans()] == ["second"]


def test_an_agent_error_message_is_kept_verbatim(sdk: Collector) -> None:
    with pytest.raises(ValueError, match="mail"), crucible_sdk.observe("agent step"):
        raise ValueError("mail jane.doe@example.com")
    assert crucible_sdk.flush()
    [step] = sdk.spans()
    assert "jane.doe@example.com" in step.status_message


@pytest.mark.parametrize(
    ("option", "value", "message"),
    [("sample_rate", 1.5, "sample_rate"), ("queue_size", 0, "queue_size"), ("flush_interval", 0, "flush_interval")],
)
def test_init_refuses_bad_settings_with_the_setting_named(option: str, value: float, message: str) -> None:
    with pytest.raises(ValueError, match=message):
        crucible_sdk.init(**{option: value})  # type: ignore[arg-type]


def test_init_without_a_key_says_how_to_set_one(collector: Collector, caplog: pytest.LogCaptureFixture) -> None:
    with caplog.at_level(logging.WARNING, logger="crucible_sdk"):
        crucible_sdk.init(endpoint=collector.url, flush_interval=60)
    crucible_sdk.shutdown()
    assert "CRUCIBLE_SECRET_KEY" in caplog.text


@pytest.mark.skipif(sys.platform == "win32", reason="fork is POSIX only")
def test_a_forked_child_exports_through_its_own_pipeline(sdk: Collector) -> None:
    if sys.platform == "win32":
        return
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", DeprecationWarning)
        pid = os.fork()
    if pid == 0:
        with crucible_sdk.observe("child"):
            pass
        os._exit(0 if crucible_sdk.flush(5) else 1)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0
    assert [s.name for s in sdk.spans()] == ["child"]


def run_python(script: str, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(  # noqa: S603 - the interpreter running the tests, with a fixed script
        [sys.executable, "-c", script, *args],
        cwd=ROOT,
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )


EXISTING_PROVIDER = """
import json, sys
import anyio
from opentelemetry import trace
from opentelemetry.sdk.resources import Resource
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter

own = InMemorySpanExporter()
provider = TracerProvider(resource=Resource.create({"service.name": "user-service"}))
provider.add_span_processor(SimpleSpanProcessor(own))
trace.set_tracer_provider(provider)

import crucible_sdk
from conformance import reference_server
from tests.test_mcp_server import connect

crucible_sdk.init(endpoint=sys.argv[1], secret_key="csk_x", deploy_id="d-7", flush_interval=60)

async def main():
    async with connect(reference_server.instrumented(), sys.argv[2]) as client:
        await client.call_tool("echo", {"text": "x"})

anyio.run(main)
assert crucible_sdk.flush()
assert trace.get_tracer_provider() is provider
print(json.dumps({
    "names": sorted(s.name for s in own.get_finished_spans()),
    "resource": dict(own.get_finished_spans()[0].resource.attributes),
}))
"""


@pytest.mark.parametrize("mode", ["legacy", "modern-direct"])
def test_an_existing_provider_is_kept_and_sees_the_same_single_spans(collector: Collector, mode: str) -> None:
    result = run_python(EXISTING_PROVIDER, collector.url, mode)
    assert result.returncode == 0, result.stderr
    own = json.loads(result.stdout.strip().splitlines()[-1])
    ours = collector.spans()
    assert own["names"] == sorted(s.name for s in ours)
    assert [s.name for s in ours if s.kind == 2].count("tools/call echo") == 1
    assert all(s.resource["crucible.deploy.id"] == "d-7" for s in ours)
    assert all(s.resource["service.name"] == "user-service" for s in ours)
    assert "crucible.deploy.id" not in own["resource"]


def test_a_non_sdk_provider_is_refused_with_the_fix(collector: Collector) -> None:
    script = """
import sys
from opentelemetry import trace
trace.set_tracer_provider(trace.NoOpTracerProvider())
import crucible_sdk
crucible_sdk.init(endpoint=sys.argv[1], secret_key="csk_x")
with crucible_sdk.observe("never"):
    pass
crucible_sdk.shutdown()
"""
    result = run_python(script, collector.url)
    assert result.returncode == 0, result.stderr
    assert "opentelemetry.sdk.trace.TracerProvider" in result.stderr
    assert collector.spans() == []


def test_the_core_imports_and_runs_without_mcp(collector: Collector) -> None:
    script = """
import sys
sys.modules["mcp"] = None
import crucible_sdk
crucible_sdk.init(endpoint=sys.argv[1], secret_key="csk_x", flush_interval=60)
with crucible_sdk.observe("no mcp"):
    pass
assert crucible_sdk.flush()
try:
    import crucible_sdk.mcp
except ImportError as error:
    print(error)
"""
    result = run_python(script, collector.url)
    assert result.returncode == 0, result.stderr
    assert "crucible-sdk[mcp]" in result.stdout
    assert [s.name for s in collector.spans()] == ["no mcp"]
