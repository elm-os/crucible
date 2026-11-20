"""The Python SDK's conformance runner (docs/testing/sdk-conformance.md, runner contract v1).

The driver writes one run spec to stdin. Agent cases call the SDK in this process. MCP cases start a fresh reference
server process (`conformance.server`) and play the steps against it over stdio or streamable HTTP. Every completed
`send` step prints one `response` event on stdout; nothing else is printed there.
"""

from __future__ import annotations

import contextlib
import http.client
import json
import socket
import subprocess
import sys
import threading
import time
from collections.abc import Callable, Mapping
from decimal import Decimal
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from opentelemetry import trace

import crucible_sdk

SDK_DIR = Path(__file__).resolve().parents[1]
TIMEOUT = 30.0
USAGE = {
    "input_tokens": "input",
    "output_tokens": "output",
    "cache_read.input_tokens": "cache_read",
    "cache_creation.input_tokens": "cache_write",
    "reasoning.output_tokens": "reasoning",
}
COSTS = {"total", "input", "output", "cache_read", "cache_write", "reasoning"}


class Exchange:
    """One sent message and what came back for it."""

    def __init__(self, step: str) -> None:
        self.step = step
        self.done = threading.Event()
        self.message: Any = None
        self.notifications = 0
        self.http_status: int | None = None
        self.session_id: str | None = None
        self.error: BaseException | None = None
        self.abort: Callable[[], None] = lambda: None


class Stdio:
    """Messages as JSON lines on the server's stdin; responses and server messages from its stdout."""

    def __init__(self, process: subprocess.Popen[str]) -> None:
        self._process = process
        self._lock = threading.Lock()
        self._pending: dict[str, Exchange] = {}
        self._reader = threading.Thread(target=self._read, daemon=True)
        self._reader.start()

    def _read(self) -> None:
        assert self._process.stdout is not None
        for line in self._process.stdout:
            try:
                message = json.loads(line)
            except ValueError:
                continue
            is_response = isinstance(message, dict) and "id" in message and ("result" in message or "error" in message)
            with self._lock:
                exchange = self._pending.pop(json.dumps(message["id"]), None) if is_response else None
                if exchange is None:
                    for waiting in self._pending.values():
                        waiting.notifications += 1
                    continue
            exchange.message = message
            exchange.done.set()
        with self._lock:
            for exchange in self._pending.values():
                exchange.done.set()

    def send(self, step: str, message: Mapping[str, Any], headers: Mapping[str, str]) -> Exchange:
        exchange = Exchange(step)
        if "id" in message:
            with self._lock:
                self._pending[json.dumps(message["id"])] = exchange
        else:
            exchange.done.set()
        assert self._process.stdin is not None
        self._process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self._process.stdin.flush()
        return exchange

    def finish(self) -> None:
        assert self._process.stdin is not None
        self._process.stdin.close()
        self._process.wait(TIMEOUT)
        self._reader.join(TIMEOUT)


class Http:
    """Every message as its own POST to `/mcp`; a JSON body or an event stream back."""

    def __init__(self, process: subprocess.Popen[str], port: int) -> None:
        self._process = process
        self._port = port
        self._session: str | None = None
        self._open: list[Exchange] = []

    def send(self, step: str, message: Mapping[str, Any], headers: Mapping[str, str]) -> Exchange:
        exchange = Exchange(step)
        connection = http.client.HTTPConnection("127.0.0.1", self._port, timeout=TIMEOUT)

        def abort() -> None:
            # Closes the response stream under the reading thread: the server sees the client go away.
            if connection.sock is not None:
                with contextlib.suppress(OSError):
                    connection.sock.shutdown(socket.SHUT_RDWR)
            connection.close()

        exchange.abort = abort
        self._open.append(exchange)
        threading.Thread(target=self._post, args=(connection, exchange, message, headers), daemon=True).start()
        return exchange

    def _post(
        self,
        connection: http.client.HTTPConnection,
        exchange: Exchange,
        message: Mapping[str, Any],
        headers: Mapping[str, str],
    ) -> None:
        sent = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream", **headers}
        if self._session is not None and not any(k.lower() == "mcp-session-id" for k in sent):
            sent["Mcp-Session-Id"] = self._session
        try:
            connection.request("POST", "/mcp", json.dumps(message).encode(), sent)
            response = connection.getresponse()
            exchange.http_status = response.status
            exchange.session_id = response.getheader("mcp-session-id")
            if message.get("method") == "initialize" and exchange.session_id:
                self._session = exchange.session_id
            if "text/event-stream" in (response.getheader("content-type") or ""):
                self._read_stream(response, exchange, message.get("id"))
            else:
                body = response.read()
                if body.strip():
                    exchange.message = json.loads(body)
        except (OSError, http.client.HTTPException, ValueError) as error:
            exchange.error = error
        finally:
            connection.close()
            exchange.done.set()

    @staticmethod
    def _read_stream(response: http.client.HTTPResponse, exchange: Exchange, request_id: object) -> None:
        data: list[str] = []
        while line := response.readline():
            text = line.decode().rstrip("\r\n")
            if text.startswith("data:"):
                data.append(text[5:].removeprefix(" "))
            elif not text and data:
                message = json.loads("\n".join(data))
                data = []
                if (
                    isinstance(message, dict)
                    and message.get("id") == request_id
                    and ("result" in message or "error" in message)
                ):
                    exchange.message = message
                    return
                exchange.notifications += 1

    def finish(self) -> None:
        for exchange in self._open:
            if not exchange.done.is_set():
                exchange.abort()
        assert self._process.stdin is not None
        self._process.stdin.close()
        self._process.wait(TIMEOUT)


def pointer(responses: Mapping[str, Any], reference: str) -> Any:
    """The value an RFC 6901 pointer selects in the response of an earlier step: `<step>/<pointer>`."""
    step, _, path = reference.partition("/")
    value = responses[step]
    for token in path.split("/") if path else ():
        token = token.replace("~1", "/").replace("~0", "~")
        value = value[int(token)] if isinstance(value, list) else value[token]
    return value


def resolve(value: Any, responses: Mapping[str, Any]) -> Any:
    if isinstance(value, dict):
        if set(value) == {"$from"}:
            return pointer(responses, value["$from"])
        return {k: resolve(v, responses) for k, v in value.items()}
    if isinstance(value, list):
        return [resolve(v, responses) for v in value]
    return value


def emit(exchange: Exchange) -> None:
    event = {
        "event": "response",
        "step": exchange.step,
        "http_status": exchange.http_status,
        "session_id": exchange.session_id,
        "message": exchange.message,
        "notifications": exchange.notifications,
    }
    sys.stdout.write(json.dumps(event) + "\n")
    sys.stdout.flush()


def run_mcp(spec: Mapping[str, Any]) -> None:
    http_mode = spec["transport"] == "streamable_http"
    config = {
        "transport": spec["transport"],
        "endpoint": spec["otlp"]["endpoint"],
        "secret_key": spec["keys"]["secret_key"],
        "sdk": spec["sdk"],
        "server": spec["server"],
    }
    process = subprocess.Popen(  # noqa: S603 - this interpreter, running the SDK's own reference server
        [sys.executable, "-m", "conformance.server", json.dumps(config)],
        cwd=SDK_DIR,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        bufsize=1,
    )
    transport: Stdio | Http
    if http_mode:
        assert process.stdout is not None
        transport = Http(process, int(json.loads(process.stdout.readline())["port"]))
    else:
        transport = Stdio(process)
    exchanges: dict[str, Exchange] = {}
    responses: dict[str, Any] = {}

    def complete(exchange: Exchange) -> None:
        if not exchange.done.wait(TIMEOUT):
            raise TimeoutError(f"step {exchange.step!r}: no response in {TIMEOUT} s")
        if exchange.error is not None:
            raise RuntimeError(f"step {exchange.step!r} failed") from exchange.error
        responses[exchange.step] = exchange.message
        emit(exchange)

    try:
        for step in spec["steps"]:
            if "send" in step:
                message = resolve(step["send"], responses)
                exchange = transport.send(step["step"], message, step.get("headers", {}))
                exchanges[step["step"]] = exchange
                if "id" not in message or step.get("wait", True):
                    complete(exchange)
            elif "await" in step:
                complete(exchanges[step["await"]])
            elif "sleep_ms" in step:
                time.sleep(step["sleep_ms"] / 1000)
            elif "close" in step:
                exchanges[step["close"]].abort()
            else:
                raise ValueError(f"step {step['step']!r}: no MCP action this runner knows")
    finally:
        transport.finish()


def checkpoint(url: str, name: str) -> None:
    parts = urlsplit(url)
    connection = http.client.HTTPConnection(parts.hostname or "127.0.0.1", parts.port or 80, timeout=TIMEOUT)
    try:
        connection.request("POST", parts.path, json.dumps({"name": name}), {"Content-Type": "application/json"})
        response = connection.getresponse()
        response.read()
        if response.status != 204:
            raise RuntimeError(f"checkpoint {name!r}: the collector answered {response.status}")
    finally:
        connection.close()


def observe(node: Mapping[str, Any]) -> None:
    with contextlib.ExitStack() as stack:
        context = node.get("context") or {}
        if "session_id" in context:
            stack.enter_context(crucible_sdk.session(context["session_id"]))
        if "user_id" in context:
            stack.enter_context(crucible_sdk.user(context["user_id"]))
        if context.get("tags"):
            stack.enter_context(crucible_sdk.tags(*context["tags"]))
        if context.get("metadata"):
            stack.enter_context(crucible_sdk.metadata(**context["metadata"]))
        if node["type"] == "generation":
            stack.enter_context(
                crucible_sdk.generation(
                    node["name"],
                    model=node.get("model"),
                    provider=node.get("provider"),
                    trace_name=node.get("trace_name"),
                )
            )
        else:
            span = stack.enter_context(
                crucible_sdk.observe(node["name"], type=node["type"], trace_name=node.get("trace_name"))
            )
            for key, attribute in (("model", "gen_ai.request.model"), ("provider", "gen_ai.provider.name")):
                if key in node:
                    span.set_attribute(attribute, node[key])
        if "input" in node:
            crucible_sdk.set_input(node["input"])
        messages = {k: node[f"{k}_messages"] for k in ("input", "output") if f"{k}_messages" in node}
        if messages:
            crucible_sdk.set_messages(**messages)
        if node.get("finish_reasons"):
            crucible_sdk.set_finish_reasons(*node["finish_reasons"])
        current = trace.get_current_span()
        usage = node.get("usage") or {}
        crucible_sdk.set_usage(**{USAGE[k]: v for k, v in usage.items() if k in USAGE})
        for key, value in usage.items():
            if key not in USAGE:
                current.set_attribute(f"gen_ai.usage.{key}", value)
        costs = node.get("cost_usd") or {}
        crucible_sdk.set_cost(**{k: Decimal(str(v)) for k, v in costs.items() if k in COSTS})
        for key, value in costs.items():
            if key not in COSTS:
                current.set_attribute(f"crucible.cost.{key}", str(value))
        for child in node.get("children", ()):
            observe(child)
        if "output" in node:
            crucible_sdk.set_output(node["output"])


def run_agent(spec: Mapping[str, Any]) -> None:
    sdk = spec["sdk"]
    crucible_sdk.init(
        endpoint=spec["otlp"]["endpoint"],
        api_url=spec["api_url"],
        project_id=spec["project_id"],
        secret_key=spec["keys"]["secret_key"],
        service_name=sdk.get("service_name"),
        environment=sdk.get("environment"),
        release=sdk.get("release"),
        deploy_id=sdk.get("deploy_id"),
        flush_interval=max(int(sdk.get("flush_interval_ms", 1000)), 1) / 1000,
    )
    for step in spec["steps"]:
        if "observe" in step:
            observe(step["observe"])
        elif "flush" in step:
            if not crucible_sdk.flush(TIMEOUT):
                raise TimeoutError(f"step {step['step']!r}: flush did not finish in {TIMEOUT} s")
        elif "checkpoint" in step:
            checkpoint(spec["checkpoint_url"], step["checkpoint"])
        elif "feedback" in step:
            # The step's keys are the route's body fields and the Idempotency-Key, as the SDK names its arguments.
            score = dict(step["feedback"])
            crucible_sdk.feedback(score.pop("name"), score.pop("value"), **score)
        elif "shutdown" in step:
            crucible_sdk.shutdown(TIMEOUT)
        elif "sleep_ms" in step:
            time.sleep(step["sleep_ms"] / 1000)
        else:
            raise ValueError(f"step {step['step']!r}: no agent action this runner knows")
    crucible_sdk.shutdown(TIMEOUT)


def main() -> None:
    spec = json.load(sys.stdin)
    if spec["kind"] == "agent":
        run_agent(spec)
    else:
        run_mcp(spec)


if __name__ == "__main__":
    main()
