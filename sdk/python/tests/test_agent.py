"""Agent helpers: every span carries the contract 10.6 keys, and the export request is well formed."""

from __future__ import annotations

import json
from decimal import Decimal
from typing import Any

import pytest
from opentelemetry import trace

import crucible_sdk
from tests.collector import Collector, ExportedSpan
from tests.conftest import SECRET_KEY


def exported(collector: Collector) -> dict[str, ExportedSpan]:
    assert crucible_sdk.flush()
    return {s.name: s for s in collector.spans()}


def test_observe_as_a_context_manager_with_setters(sdk: Collector) -> None:
    with crucible_sdk.observe("plan", type="chain", metadata={"step": 1, "cfg": {"a": 1}}, tags=["x"]) as span:
        crucible_sdk.set_input({"question": "why"})
        crucible_sdk.set_output("because")
        assert span.is_recording()
    a = exported(sdk)["plan"].attributes
    assert a["crucible.observation.type"] == "chain"
    assert json.loads(a["crucible.input"]) == {"question": "why"}
    assert a["crucible.output"] == "because"
    assert a["crucible.metadata.step"] == 1
    assert json.loads(a["crucible.metadata.cfg"]) == {"a": 1}
    assert a["crucible.tags"] == ["x"]


def test_generation_usage_and_cost(sdk: Collector) -> None:
    with crucible_sdk.generation("chat", model="model-a", provider="provider-x"):
        crucible_sdk.set_usage(input=120, output=30, cache_read=100, cache_write=20, reasoning=10)
        crucible_sdk.set_cost(total=Decimal("0.0042"), input=0.003, output=Decimal("1E-7"))
    a = exported(sdk)["chat"].attributes
    assert a["crucible.observation.type"] == "generation"
    assert a["gen_ai.request.model"] == "model-a"
    assert a["gen_ai.provider.name"] == "provider-x"
    assert a["gen_ai.usage.input_tokens"] == 120
    assert a["gen_ai.usage.output_tokens"] == 30
    assert a["gen_ai.usage.cache_read.input_tokens"] == 100
    assert a["gen_ai.usage.cache_creation.input_tokens"] == 20
    assert a["gen_ai.usage.reasoning.output_tokens"] == 10
    assert a["crucible.cost.total"] == "0.0042"
    assert a["crucible.cost.input"] == 0.003
    assert a["crucible.cost.output"] == "1E-7"


@pytest.mark.parametrize("bad", [-1, True, 1.5])
def test_usage_must_be_a_non_negative_int(sdk: Collector, bad: Any) -> None:
    with crucible_sdk.observe("x"), pytest.raises(ValueError, match="non-negative int"):
        crucible_sdk.set_usage(input=bad)


def test_a_cost_must_be_finite(sdk: Collector) -> None:
    with crucible_sdk.observe("x"), pytest.raises(ValueError, match="finite"):
        crucible_sdk.set_cost(total=Decimal("NaN"))


def test_decorators_record_arguments_and_return_values(sdk: Collector) -> None:
    @crucible_sdk.observe
    def plain(a: int, b: str = "z") -> dict[str, Any]:
        return {"sum": a, "b": b}

    @crucible_sdk.observe("tool step", type="tool", capture_output=False)
    def step(self: object, q: str) -> str:
        return "hidden"

    plain(1, b="y")
    step(object(), "q")
    spans = exported(sdk)
    a = spans["test_decorators_record_arguments_and_return_values.<locals>.plain"].attributes
    assert json.loads(a["crucible.input"]) == {"a": 1, "b": "y"}
    assert json.loads(a["crucible.output"]) == {"sum": 1, "b": "y"}
    assert a["crucible.observation.type"] == "span"
    b = spans["tool step"].attributes
    assert json.loads(b["crucible.input"]) == {"q": "q"}
    assert "crucible.output" not in b


@pytest.mark.anyio
async def test_async_decorators_and_context(sdk: Collector) -> None:
    @crucible_sdk.session("s-9")
    @crucible_sdk.observe(type="agent")
    async def run(task: str) -> str:
        return task.upper()

    assert await run("go") == "GO"
    a = exported(sdk)["test_async_decorators_and_context.<locals>.run"].attributes
    assert a["crucible.session.id"] == "s-9"
    assert a["crucible.output"] == "GO"


def test_every_span_inside_a_context_carries_it(sdk: Collector) -> None:
    other = trace.get_tracer("another-library")
    with crucible_sdk.session("s-1"), crucible_sdk.user("u-1"), crucible_sdk.tags("a", "b"):
        with crucible_sdk.tags("b", "c"), crucible_sdk.metadata(region="eu", attempt=2):
            with crucible_sdk.observe("inner"):
                pass
            with other.start_as_current_span("library span", attributes={"crucible.session.id": "own"}):
                pass
        with crucible_sdk.observe("outer"):
            pass
    with crucible_sdk.observe("outside"):
        pass
    spans = exported(sdk)
    inner = spans["inner"].attributes
    assert inner["crucible.session.id"] == "s-1"
    assert inner["crucible.user.id"] == "u-1"
    assert inner["crucible.tags"] == ["a", "b", "c"]
    assert inner["crucible.metadata.region"] == "eu"
    assert inner["crucible.metadata.attempt"] == 2
    library = spans["library span"].attributes
    assert library["crucible.session.id"] == "own"
    assert library["crucible.user.id"] == "u-1"
    assert spans["outer"].attributes["crucible.tags"] == ["a", "b"]
    assert "crucible.metadata.region" not in spans["outer"].attributes
    assert not {k for k in spans["outside"].attributes if k.startswith(("crucible.session", "crucible.user"))}


def test_an_exception_marks_the_span_and_propagates(sdk: Collector) -> None:
    with pytest.raises(KeyError), crucible_sdk.observe("broken"):
        raise KeyError("k")
    span = exported(sdk)["broken"]
    assert span.status_code == 2
    assert [name for name, _ in span.events] == ["exception"]


def test_values_that_are_not_json_are_still_recorded(sdk: Collector) -> None:
    loop: list[Any] = []
    loop.append(loop)
    with crucible_sdk.observe("odd", input={"when": object, "set": {1}}):
        crucible_sdk.set_output(loop)
    a = exported(sdk)["odd"].attributes
    assert "<class 'object'>" in a["crucible.input"]
    assert a["crucible.output"] == "[[...]]"


def test_the_export_request_and_resource(sdk: Collector) -> None:
    with crucible_sdk.observe("one"):
        pass
    span = exported(sdk)["one"]
    request = sdk.requests[0]
    assert request.path == "/v1/traces"
    assert request.headers["content-type"] == "application/x-protobuf"
    assert request.headers["authorization"] == f"Bearer {SECRET_KEY}"
    assert request.headers["user-agent"].split()[0] == f"crucible-sdk-python/{crucible_sdk.__version__}"
    assert span.resource["service.name"] == "crucible-test"
    assert span.resource["deployment.environment.name"] == "test"
    assert span.resource["service.version"] == "2.0.0"
    assert span.resource["crucible.deploy.id"] == "deploy-1"
    assert len(span.resource["service.instance.id"]) == 36
    assert span.scope == "crucible-sdk"
    assert SECRET_KEY.encode() not in sdk.raw()


def test_a_span_after_shutdown_is_not_exported_and_does_not_raise(sdk: Collector) -> None:
    crucible_sdk.shutdown()
    with crucible_sdk.observe("late"):
        crucible_sdk.set_input("x")
    assert crucible_sdk.flush()
    assert "late" not in {s.name for s in sdk.spans()}
