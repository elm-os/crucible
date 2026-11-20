"""Feedback: each call posts one score to the project's feedback route with the secret key, in call order and after
the spans that ended before it. The call never blocks or raises; a transient failure is retried with the same
Idempotency-Key and body, and every score that is not sent is counted."""

from __future__ import annotations

import json
import logging
import re
import time
from collections.abc import Callable, Iterator
from typing import Any

import pytest

import crucible_sdk
from tests.collector import Answer, Collector, Received
from tests.conftest import SECRET_KEY

PROJECT = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b"
TRACE = "4bf92f3577b34da6a3ce929d0e0e4736"
ROUTE = f"/api/v1/projects/{PROJECT}/feedback"


def start(collector: Collector, **options: Any) -> None:
    values: dict[str, Any] = {
        "endpoint": collector.url,
        "secret_key": SECRET_KEY,
        "project_id": PROJECT,
        "flush_interval": 60.0,
    }
    values.update(options)
    crucible_sdk.init(**values)


@pytest.fixture
def api(collector: Collector, monkeypatch: pytest.MonkeyPatch) -> Iterator[Collector]:
    """The SDK with a project id; spans leave only through flush, shutdown or the feedback sender."""
    monkeypatch.delenv("CRUCIBLE_PROJECT_ID", raising=False)
    monkeypatch.delenv("CRUCIBLE_API_URL", raising=False)
    start(collector)
    yield collector
    crucible_sdk.shutdown()


def body(request: Received) -> Any:
    return json.loads(request.body)


def until(condition: Callable[[], bool], timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    while not condition():
        assert time.monotonic() < deadline, "the condition did not hold in time"
        time.sleep(0.01)


def test_feedback_posts_one_score_to_the_project_route_with_the_secret_key(api: Collector) -> None:
    returned = crucible_sdk.feedback(  # type: ignore[func-returns-value]
        "helpful", 1, trace_id=TRACE, comment="good answer", idempotency_key="thumbs-4bf9"
    )
    assert returned is None
    assert crucible_sdk.flush(10)
    [request] = api.feedback()
    assert request.path == ROUTE
    assert request.headers["authorization"] == f"Bearer {SECRET_KEY}"
    assert request.headers["content-type"] == "application/json"
    assert request.headers["idempotency-key"] == "thumbs-4bf9"
    assert body(request) == {"trace_id": TRACE, "name": "helpful", "value": 1, "comment": "good answer"}
    stats = crucible_sdk.export_stats()
    assert (stats.feedback_sent, stats.feedback_dropped, stats.feedback_failed) == (1, 0, 0)


def test_every_field_goes_out_under_the_route_name_and_each_call_gets_its_own_key(api: Collector) -> None:
    crucible_sdk.feedback(
        "tone",
        "friendly",
        trace_id=TRACE,
        span_id="00f067aa0ba902b7",
        score_id="reviewer-2",
        data_type="categorical",
        metadata={"source": "thumbs", "rating": 4},
        retracted=False,
    )
    crucible_sdk.feedback("resolved", True, session_id="conversation-17")
    assert crucible_sdk.flush(10)
    span, session = api.feedback()
    assert body(span) == {
        "trace_id": TRACE,
        "span_id": "00f067aa0ba902b7",
        "name": "tone",
        "value": "friendly",
        "score_id": "reviewer-2",
        "data_type": "categorical",
        "metadata": {"source": "thumbs", "rating": 4},
        "retracted": False,
    }
    assert body(session) == {"session_id": "conversation-17", "name": "resolved", "value": True}
    keys = [request.headers["idempotency-key"] for request in api.feedback()]
    assert all(re.fullmatch(r"[0-9a-f]{32}", key) for key in keys)
    assert len(set(keys)) == 2


def test_spans_that_ended_before_the_call_reach_crucible_first(api: Collector) -> None:
    with crucible_sdk.observe("answer", type="agent"):
        pass
    crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
    until(lambda: len(api.feedback()) == 1)
    paths = [request.path for request in api.requests]
    assert "/v1/traces" in paths
    assert paths.index("/v1/traces") < paths.index(ROUTE)


def test_a_503_with_retry_after_is_retried_with_the_same_key_and_body(api: Collector) -> None:
    api.feedback_answers.append(Answer(503, {"retry-after": "0"}))
    crucible_sdk.feedback("helpful", 0.5, trace_id=TRACE)
    assert crucible_sdk.flush(10)
    first, second = api.feedback()
    assert first.headers["idempotency-key"] == second.headers["idempotency-key"]
    assert first.body == second.body
    stats = crucible_sdk.export_stats()
    assert (stats.feedback_sent, stats.feedback_failed) == (1, 0)


def test_a_409_conflict_is_retried_and_a_reused_key_is_not(api: Collector) -> None:
    def problem(code: str) -> bytes:
        return json.dumps({"code": code, "status": 409}).encode()

    api.feedback_answers.extend(
        [
            Answer(409, {"retry-after": "0"}, problem("conflict")),
            Answer(201),
            Answer(409, {}, problem("idempotency_key_reused")),
        ]
    )
    crucible_sdk.feedback("first", 1, trace_id=TRACE)
    crucible_sdk.feedback("second", 1, trace_id=TRACE)
    assert crucible_sdk.flush(10)
    assert [body(request)["name"] for request in api.feedback()] == ["first", "first", "second"]
    stats = crucible_sdk.export_stats()
    assert (stats.feedback_sent, stats.feedback_failed) == (1, 1)


def test_a_refusal_is_not_retried_is_counted_and_logged_once(api: Collector, caplog: pytest.LogCaptureFixture) -> None:
    problem = b'{"code":"not_found","detail":"the project has not sent this trace"}'
    api.feedback_answers.extend([Answer(404, {}, problem), Answer(404, {}, problem)])
    with caplog.at_level(logging.WARNING, logger="crucible_sdk"):
        crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
        crucible_sdk.feedback("helpful", 2, trace_id=TRACE)
        assert crucible_sdk.flush(10)
    assert len(api.feedback()) == 2
    assert crucible_sdk.export_stats().feedback_failed == 2
    warnings = [r.getMessage() for r in caplog.records if r.levelno == logging.WARNING]
    assert len(warnings) == 1
    assert "404" in warnings[0]
    assert "the project has not sent this trace" in warnings[0]
    assert "api_url" in warnings[0]


def test_a_failing_connection_never_reaches_the_caller(api: Collector) -> None:
    api.feedback_answers.extend([Answer(0), Answer(0), Answer(0)])
    crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
    until(lambda: crucible_sdk.export_stats().feedback_failed == 1, timeout=15)
    assert len(api.feedback()) == 3
    assert crucible_sdk.export_stats().feedback_sent == 0


def test_feedback_returns_at_once_while_the_api_is_not_answering(collector: Collector) -> None:
    start(collector)
    try:
        collector.hang = True
        started = time.perf_counter()
        for i in range(100):
            crucible_sdk.feedback(f"stalled-{i}", i, trace_id=TRACE)
        assert time.perf_counter() - started < 1.0
        stats = crucible_sdk.export_stats()
        assert stats.feedback_sent + stats.feedback_failed == 0
    finally:
        collector.release.set()
        crucible_sdk.shutdown(0.5)
    # Shutdown gave up after its timeout: what it did not send is counted, not raised.


def test_a_full_queue_drops_the_score_and_counts_it(api: Collector, caplog: pytest.LogCaptureFixture) -> None:
    api.hang = True
    with caplog.at_level(logging.WARNING, logger="crucible_sdk"):
        for i in range(1100):
            crucible_sdk.feedback(f"burst-{i}", i, trace_id=TRACE)
    assert crucible_sdk.export_stats().feedback_dropped == 76
    assert any("feedback queue is full" in r.getMessage() for r in caplog.records)
    api.hang = False
    api.release.set()
    until(lambda: crucible_sdk.export_stats().feedback_sent == 1024, timeout=60)


def test_init_checks_only_an_api_url_that_is_given(monkeypatch: pytest.MonkeyPatch) -> None:
    # The endpoint was never checked by init; a feedback setting must not start refusing it under another name.
    monkeypatch.delenv("CRUCIBLE_API_URL", raising=False)
    crucible_sdk.init(endpoint="localhost:4318", secret_key=SECRET_KEY, flush_interval=60.0)
    crucible_sdk.shutdown(0.1)
    monkeypatch.setenv("CRUCIBLE_API_URL", "localhost:8080")
    with pytest.raises(ValueError, match="CRUCIBLE_API_URL"):
        crucible_sdk.init(endpoint="http://localhost:4318", secret_key=SECRET_KEY, flush_interval=60.0)


def test_a_redirect_is_not_followed_and_counts_as_failed(api: Collector) -> None:
    # Following it would send the secret key to another URL and turn the post into a GET.
    api.feedback_answers.append(Answer(302, {"location": "/elsewhere"}))
    crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
    assert crucible_sdk.flush(10)
    assert [request.path for request in api.requests if request.path != "/v1/traces"] == [ROUTE]
    stats = crucible_sdk.export_stats()
    assert (stats.feedback_sent, stats.feedback_failed) == (0, 1)


@pytest.mark.parametrize(
    ("options", "message"),
    [
        ({"project_id": "default"}, "project_id"),
        ({"api_url": "ftp://crucible"}, "api_url"),
    ],
)
def test_init_refuses_bad_feedback_settings_with_the_setting_named(
    collector: Collector, options: dict[str, str], message: str
) -> None:
    with pytest.raises(ValueError, match=message):
        start(collector, **options)


def test_without_a_project_id_feedback_is_dropped_counted_and_logged(
    collector: Collector, monkeypatch: pytest.MonkeyPatch, caplog: pytest.LogCaptureFixture
) -> None:
    monkeypatch.delenv("CRUCIBLE_PROJECT_ID", raising=False)
    crucible_sdk.init(endpoint=collector.url, secret_key=SECRET_KEY, flush_interval=60.0)
    try:
        with caplog.at_level(logging.WARNING, logger="crucible_sdk"):
            crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
            crucible_sdk.feedback("helpful", 2, trace_id=TRACE)
        assert crucible_sdk.flush(5)
        assert collector.feedback() == []
        assert crucible_sdk.export_stats().feedback_dropped == 2
        warnings = [r.getMessage() for r in caplog.records if r.levelno == logging.WARNING]
        assert len(warnings) == 1
        assert "CRUCIBLE_PROJECT_ID" in warnings[0]
    finally:
        crucible_sdk.shutdown()


def test_the_api_url_defaults_to_the_endpoint_and_can_be_set(collector: Collector) -> None:
    start(collector, project_id=PROJECT.upper())
    crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
    assert crucible_sdk.flush(10)
    crucible_sdk.shutdown()
    assert [request.path for request in collector.feedback()] == [ROUTE]
    collector.requests.clear()
    start(collector, endpoint="http://127.0.0.1:9", api_url=collector.url + "/")
    try:
        crucible_sdk.feedback("helpful", 1, trace_id=TRACE)
        until(lambda: len(collector.feedback()) == 1)
    finally:
        crucible_sdk.shutdown(1.0)


def test_shutdown_sends_what_is_queued_and_later_feedback_is_not_sent(collector: Collector) -> None:
    start(collector)
    crucible_sdk.feedback("last-words", 1, trace_id=TRACE)
    crucible_sdk.shutdown()
    crucible_sdk.feedback("too-late", 1, trace_id=TRACE)
    time.sleep(0.1)
    assert [body(request)["name"] for request in collector.feedback()] == ["last-words"]
