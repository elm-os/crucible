"""A fake OTLP HTTP collector: `http.server` on a loopback port, in a thread, recording every request.

It also answers the feedback route, `POST /api/v1/projects/<id>/feedback`: from `feedback_answers` while that list
holds any, else with 201.
"""

from __future__ import annotations

import gzip
import re
import threading
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any

from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import ExportTraceServiceRequest
from opentelemetry.proto.common.v1.common_pb2 import AnyValue


def any_value(value: AnyValue) -> Any:
    kind = value.WhichOneof("value")
    if kind == "array_value":
        return [any_value(v) for v in value.array_value.values]
    if kind == "kvlist_value":
        return {kv.key: any_value(kv.value) for kv in value.kvlist_value.values}
    return None if kind is None else getattr(value, kind)


@dataclass
class ExportedSpan:
    name: str
    kind: int
    trace_id: str
    span_id: str
    parent_span_id: str
    attributes: dict[str, Any]
    resource: dict[str, Any]
    scope: str
    status_code: int
    status_message: str
    events: list[tuple[str, dict[str, Any]]]


@dataclass
class Received:
    path: str
    headers: dict[str, str]
    body: bytes


FEEDBACK_PATH = re.compile(r"^/api/v1/projects/[^/]+/feedback$")


@dataclass
class Answer:
    """One answer to a feedback post. Status 0 closes the connection without answering."""

    status: int
    headers: dict[str, str] = field(default_factory=dict)
    body: bytes = b""


@dataclass
class Collector:
    """Records requests; `status` sets the reply, `hang` makes it accept and never answer."""

    url: str = ""
    status: int = 200
    hang: bool = False
    requests: list[Received] = field(default_factory=list)
    feedback_answers: list[Answer] = field(default_factory=list)
    release: threading.Event = field(default_factory=threading.Event)
    lock: threading.Lock = field(default_factory=threading.Lock)

    def feedback(self) -> list[Received]:
        """The feedback posts, in arrival order."""
        with self.lock:
            return [r for r in self.requests if FEEDBACK_PATH.match(r.path)]

    def spans(self) -> list[ExportedSpan]:
        out: list[ExportedSpan] = []
        with self.lock:
            bodies = [r.body for r in self.requests if r.path == "/v1/traces"]
        for body in bodies:
            request = ExportTraceServiceRequest()
            request.ParseFromString(body)
            for rs in request.resource_spans:
                resource = {kv.key: any_value(kv.value) for kv in rs.resource.attributes}
                for ss in rs.scope_spans:
                    for s in ss.spans:
                        out.append(
                            ExportedSpan(
                                name=s.name,
                                kind=s.kind,
                                trace_id=s.trace_id.hex(),
                                span_id=s.span_id.hex(),
                                parent_span_id=s.parent_span_id.hex(),
                                attributes={kv.key: any_value(kv.value) for kv in s.attributes},
                                resource=resource,
                                scope=ss.scope.name,
                                status_code=s.status.code,
                                status_message=s.status.message,
                                events=[
                                    (e.name, {kv.key: any_value(kv.value) for kv in e.attributes}) for e in s.events
                                ],
                            )
                        )
        return out

    def raw(self) -> bytes:
        with self.lock:
            return b"".join(r.body for r in self.requests if r.path == "/v1/traces")

    def wait_for(self, count: int, timeout: float = 5.0) -> list[ExportedSpan]:
        deadline = time.monotonic() + timeout
        while True:
            spans = self.spans()
            if len(spans) >= count or time.monotonic() > deadline:
                return spans
            time.sleep(0.01)


@contextmanager
def running(collector: Collector | None = None) -> Iterator[Collector]:
    state = collector or Collector()

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self) -> None:
            body = self.rfile.read(int(self.headers.get("content-length", "0")))
            if self.headers.get("content-encoding") == "gzip":
                body = gzip.decompress(body)
            with state.lock:
                state.requests.append(Received(self.path, {k.lower(): v for k, v in self.headers.items()}, body))
                is_feedback = FEEDBACK_PATH.match(self.path) is not None
                answer = state.feedback_answers.pop(0) if is_feedback and state.feedback_answers else None
            if is_feedback and not state.hang:
                self.answer_feedback(answer or Answer(201, {"content-type": "application/json"}, b'{"key":"k"}'))
                return
            if state.hang:
                state.release.wait(30)
                return
            self.send_response(state.status)
            self.send_header("content-type", "application/x-protobuf")
            self.send_header("content-length", "0")
            self.end_headers()

        def answer_feedback(self, answer: Answer) -> None:
            if answer.status == 0:
                self.close_connection = True
                self.connection.close()
                return
            self.send_response(answer.status)
            for name, value in answer.headers.items():
                self.send_header(name, value)
            self.send_header("content-length", str(len(answer.body)))
            self.end_headers()
            self.wfile.write(answer.body)

        def log_message(self, format: str, *args: Any) -> None:
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    state.url = f"http://127.0.0.1:{server.server_address[1]}"
    thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    try:
        yield state
    finally:
        state.release.set()
        server.shutdown()
        server.server_close()
