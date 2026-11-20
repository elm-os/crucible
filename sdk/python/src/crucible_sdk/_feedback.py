"""Feedback (contract 12.6): a score with source `feedback` on a trace, a span or a session.

Scores go to `POST <api_url>/api/v1/projects/<project_id>/feedback` with the project secret key, which needs the
`write` scope (contract 11.3). `feedback()` only queues the score, so the request leaves the caller's path as spans do
(contract 9.10). One daemon thread posts the queue in call order, one request at a time, because the latest score of
a name wins. Before it posts, it flushes the span queue: the route answers 404 for a trace Crucible has not received,
so the spans that ended before the call go first. A failed connection, a timeout, 408, 429, 5xx and a `conflict` 409
are retried, three attempts in all, after `Retry-After` (at most 10 s) or else 1 s and 2 s, with the same body and
Idempotency-Key, so Crucible writes the score once. Nothing is raised to the caller: a score that cannot be queued
counts as dropped, one that Crucible refuses or that fails every attempt as failed, and the first of each is logged
with what to change.
"""

from __future__ import annotations

import http.client
import json
import logging
import threading
import time
import urllib.error
import urllib.request
import uuid
from collections import deque
from collections.abc import Callable, Mapping
from dataclasses import dataclass

from opentelemetry import context as context_api

logger = logging.getLogger("crucible_sdk")

QUEUE_LIMIT = 1024
"""Scores waiting to be sent, the one being sent included."""
ATTEMPTS = 3
BACKOFF = (1.0, 2.0)
"""The waits before the second and third attempts when the answer has no `Retry-After`."""
RETRY_AFTER_CAP = 10.0
SETTINGS = (
    "scores go to api_url= (CRUCIBLE_API_URL), the API listener, under project_id= (CRUCIBLE_PROJECT_ID), the key's "
    "project, with a secret key that has the write scope"
)


@dataclass(frozen=True)
class _Item:
    seq: int
    body: bytes
    key: str


@dataclass(frozen=True)
class _Outcome:
    """How one request ended: accepted, refused for good (with why), or worth another attempt."""

    sent: bool = False
    retry: bool = False
    retry_after: float | None = None
    reason: str = ""


class FeedbackSender:
    """A bounded queue of scores drained by one thread, started with the first score."""

    def __init__(
        self,
        url: str | None,
        secret_key: str | None,
        user_agent: str,
        timeout: float,
        flush_spans: Callable[[float], bool],
    ) -> None:
        self._url = url
        self._headers = {"Content-Type": "application/json", "User-Agent": user_agent}
        if secret_key:
            self._headers["Authorization"] = f"Bearer {secret_key}"
        self._timeout = timeout
        self._flush_spans = flush_spans
        self._cond = threading.Condition(threading.Lock())
        self._queue: deque[_Item] = deque()
        self._seq = 0
        self._flushed = 0
        self._handled = 0
        self._sent = 0
        self._dropped = 0
        self._failed = 0
        self._closed = False
        self._stopped = threading.Event()
        self._thread: threading.Thread | None = None
        self._warned_drop = False
        self._warned_failure = False

    def counts(self) -> tuple[int, int, int]:
        """Sent, dropped and failed scores."""
        with self._cond:
            return self._sent, self._dropped, self._failed

    def add(self, payload: Mapping[str, object], key: str | None) -> None:
        """Queues one score, or drops and counts it. Never raises."""
        if self._url is None:
            self._drop(
                "crucible_sdk.feedback: no project id, so the score was dropped; pass project_id= to "
                "crucible_sdk.init() or set CRUCIBLE_PROJECT_ID to the key's project (crucible admin create-key prints "
                "it as project_id)"
            )
            return
        try:
            body = json.dumps(payload, separators=(",", ":"), allow_nan=False).encode()
        except (TypeError, ValueError) as error:
            self._drop(f"crucible_sdk.feedback: the score is not JSON ({error}), so it was dropped")
            return
        with self._cond:
            if self._closed or len(self._queue) >= QUEUE_LIMIT:
                full = not self._closed
            else:
                self._seq += 1
                self._queue.append(_Item(self._seq, body, key or uuid.uuid4().hex))
                if self._thread is None:
                    self._thread = threading.Thread(target=self._run, name="crucible-sdk-feedback", daemon=True)
                    self._thread.start()
                self._cond.notify_all()
                return
        if full:
            self._drop(
                f"crucible_sdk.feedback: the feedback queue is full ({QUEUE_LIMIT} scores), so scores are being "
                f"dropped; check that {self._url} answers"
            )
        else:
            self._drop("")

    def flush(self, timeout: float) -> bool:
        """Waits until every score queued before the call is handled, or `timeout` seconds pass."""
        deadline = time.monotonic() + timeout
        with self._cond:
            target = self._seq
            while self._handled < target:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or self._thread is None or not self._thread.is_alive():
                    return False
                self._cond.wait(remaining)
        return True

    def shutdown(self, timeout: float) -> None:
        """Sends what is queued, waiting at most `timeout` seconds, then stops; what is left counts as dropped."""
        with self._cond:
            self._closed = True
            self._cond.notify_all()
            thread = self._thread
        if thread is not None:
            thread.join(timeout)
        self._stopped.set()
        with self._cond:
            self._cond.notify_all()

    def _drop(self, message: str) -> None:
        with self._cond:
            self._dropped += 1
            first = not self._warned_drop and bool(message)
            self._warned_drop = self._warned_drop or first
        if first:
            logger.warning("%s", message)

    def _run(self) -> None:
        while True:
            with self._cond:
                while not self._queue and not self._closed:
                    self._cond.wait()
                if not self._queue:
                    return
                if self._stopped.is_set():
                    self._dropped += len(self._queue)
                    self._handled += len(self._queue)
                    self._queue.clear()
                    self._cond.notify_all()
                    return
                item = self._queue[0]
                flush = item.seq > self._flushed
                if flush:
                    self._flushed = self._seq
            if flush:
                self._flush_spans(self._timeout)
            outcome = self._post(item)
            with self._cond:
                self._queue.popleft()
                self._handled += 1
                if outcome.sent:
                    self._sent += 1
                elif self._stopped.is_set():
                    self._dropped += 1
                else:
                    self._failed += 1
                warn = not outcome.sent and not self._stopped.is_set() and not self._warned_failure
                self._warned_failure = self._warned_failure or warn
                self._cond.notify_all()
            if warn:
                logger.warning("crucible_sdk.feedback: %s; %s", outcome.reason, SETTINGS)
            elif not outcome.sent:
                logger.debug("crucible_sdk.feedback: %s", outcome.reason)

    def _post(self, item: _Item) -> _Outcome:
        outcome = _Outcome()
        for attempt in range(ATTEMPTS):
            if attempt:
                wait = outcome.retry_after if outcome.retry_after is not None else BACKOFF[attempt - 1]
                if self._stopped.wait(wait):
                    return outcome
            outcome = self._request(item)
            if outcome.sent or not outcome.retry:
                return outcome
        return outcome

    def _request(self, item: _Item) -> _Outcome:
        assert self._url is not None
        request = urllib.request.Request(  # noqa: S310 - the configured API URL, checked to be http or https
            self._url, data=item.body, method="POST", headers={**self._headers, "Idempotency-Key": item.key}
        )
        # The SDK's own request must not produce spans in an instrumented application.
        token = context_api.attach(context_api.set_value(context_api._SUPPRESS_INSTRUMENTATION_KEY, True))
        try:
            with urllib.request.urlopen(request, timeout=self._timeout) as response:  # noqa: S310 - as above
                response.read()
            return _Outcome(sent=True)
        except urllib.error.HTTPError as error:
            with error:
                text = error.read().decode("utf-8", "replace")
            retry = _retryable(error.code, text)
            return _Outcome(
                retry=retry,
                retry_after=_retry_after(error.headers.get("Retry-After")) if retry else None,
                reason=f"{self._url} answered {error.code}{_detail(text)}",
            )
        except (OSError, http.client.HTTPException, ValueError) as error:
            return _Outcome(retry=True, reason=f"{self._url} could not be reached: {error}")
        finally:
            context_api.detach(token)


def _retryable(status: int, body: str) -> bool:
    """Whether an answer may change on a later attempt: a timeout, throttling, a server fault or a key in flight."""
    if status in (408, 429) or status >= 500:
        return True
    if status != 409:
        return False
    # `conflict`: a request with this key is still running. `idempotency_key_reused` never changes.
    return _problem(body).get("code") == "conflict"


def _problem(body: str) -> dict[str, object]:
    try:
        parsed = json.loads(body)
    except ValueError:
        return {}
    return parsed if isinstance(parsed, dict) else {}


def _detail(body: str) -> str:
    problem = _problem(body)
    parts = [str(problem[k]) for k in ("code", "detail") if isinstance(problem.get(k), str)]
    return f" ({': '.join(parts)})" if parts else ""


def _retry_after(header: str | None) -> float | None:
    try:
        seconds = float(header) if header else -1.0
    except ValueError:
        return None
    return min(seconds, RETRY_AFTER_CAP) if seconds >= 0 else None
