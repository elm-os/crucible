"""Agent helpers: observations and generations, and their input, output, usage and cost (contract 10.6)."""

from __future__ import annotations

import dataclasses
import functools
import inspect
import json
from collections.abc import Callable, Mapping, Sequence
from contextlib import AbstractContextManager
from decimal import Decimal
from types import TracebackType
from typing import Any, Literal, TypeVar, cast, overload

from opentelemetry import trace
from opentelemetry.trace import Span
from opentelemetry.util.types import AttributeValue

from crucible_sdk import _keys
from crucible_sdk._version import __version__

F = TypeVar("F", bound=Callable[..., Any])

ObservationType = Literal[
    "span", "generation", "event", "embedding", "agent", "tool", "chain", "retriever", "evaluator", "guardrail"
]

_TRACER = trace.get_tracer("crucible-sdk", __version__)
_UNSET: Any = object()


def _fallback(value: object) -> object:
    dump = getattr(value, "model_dump", None)
    if callable(dump):
        return dump(mode="json")
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return dataclasses.asdict(value)
    return str(value)


def _text(value: object) -> str:
    """A string verbatim; any other value as JSON text."""
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, default=_fallback)
    except Exception:  # noqa: BLE001 - recording a value must never fail the caller
        return repr(value)


def _metadata_value(value: object) -> AttributeValue:
    return value if isinstance(value, str | bool | int | float) else _text(value)


def _arguments(signature: inspect.Signature, args: tuple[Any, ...], kwargs: dict[str, Any]) -> object:
    try:
        bound = signature.bind_partial(*args, **kwargs)
    except TypeError:
        return {"args": list(args), "kwargs": kwargs}
    values = dict(bound.arguments)
    first = next(iter(signature.parameters), None)
    if first in ("self", "cls"):
        values.pop(first, None)
    return values


class Observation:
    """One span: use it as a context manager (it yields the OpenTelemetry span) or as a decorator.

    As a decorator it records the call's arguments as the input and its return value as the output, unless
    `capture_input` or `capture_output` is false.
    """

    def __init__(
        self,
        name: str | None,
        type: ObservationType,
        *,
        input: object = _UNSET,
        metadata: Mapping[str, object] | None = None,
        tags: Sequence[str] | None = None,
        trace_name: str | None = None,
        model: str | None = None,
        provider: str | None = None,
        capture_input: bool = True,
        capture_output: bool = True,
    ) -> None:
        self._name = name
        self._trace_name = trace_name
        self._type = type
        self._input = input
        self._metadata = metadata or {}
        self._tags = tuple(tags or ())
        self._model = model
        self._provider = provider
        self._capture_input = capture_input
        self._capture_output = capture_output
        self._open: list[AbstractContextManager[Span]] = []

    def _attributes(self) -> dict[str, AttributeValue]:
        attributes: dict[str, AttributeValue] = {_keys.OBSERVATION_TYPE: self._type}
        if self._input is not _UNSET:
            attributes[_keys.INPUT] = _text(self._input)
        for key, value in self._metadata.items():
            attributes[_keys.METADATA_PREFIX + key] = _metadata_value(value)
        if self._tags:
            attributes[_keys.TAGS] = list(self._tags)
        if self._trace_name:
            attributes[_keys.TRACE_NAME] = self._trace_name
        if self._model:
            attributes[_keys.REQUEST_MODEL] = self._model
        if self._provider:
            attributes[_keys.PROVIDER_NAME] = self._provider
        return attributes

    def __enter__(self) -> Span:
        manager = _TRACER.start_as_current_span(self._name or self._type, attributes=self._attributes())
        span = manager.__enter__()
        self._open.append(manager)
        return span

    def __exit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        self._open.pop().__exit__(exc_type, exc, tb)

    def __call__(self, func: F) -> F:
        name = self._name or func.__qualname__
        signature = inspect.signature(func)
        capture_input = self._capture_input and self._input is _UNSET
        capture_output = self._capture_output

        def start(args: tuple[Any, ...], kwargs: dict[str, Any]) -> AbstractContextManager[Span]:
            attributes = self._attributes()
            if capture_input:
                attributes[_keys.INPUT] = _text(_arguments(signature, args, kwargs))
            return _TRACER.start_as_current_span(name, attributes=attributes)

        if inspect.iscoroutinefunction(func):

            @functools.wraps(func)
            async def run_async(*args: Any, **kwargs: Any) -> Any:
                with start(args, kwargs) as span:
                    result = await func(*args, **kwargs)
                    if capture_output and result is not None:
                        span.set_attribute(_keys.OUTPUT, _text(result))
                    return result

            return cast("F", run_async)

        @functools.wraps(func)
        def run(*args: Any, **kwargs: Any) -> Any:
            with start(args, kwargs) as span:
                result = func(*args, **kwargs)
                if capture_output and result is not None:
                    span.set_attribute(_keys.OUTPUT, _text(result))
                return result

        return cast("F", run)


@overload
def observe(name: F, /) -> F: ...


@overload
def observe(
    name: str | None = None,
    /,
    *,
    type: ObservationType = "span",
    input: object = _UNSET,
    metadata: Mapping[str, object] | None = None,
    tags: Sequence[str] | None = None,
    trace_name: str | None = None,
    capture_input: bool = True,
    capture_output: bool = True,
) -> Observation: ...


def observe(
    name: str | Callable[..., Any] | None = None,
    /,
    *,
    type: ObservationType = "span",
    input: object = _UNSET,
    metadata: Mapping[str, object] | None = None,
    tags: Sequence[str] | None = None,
    trace_name: str | None = None,
    capture_input: bool = True,
    capture_output: bool = True,
) -> Any:
    """A span of observation type `type` (`crucible.observation.type`). `@observe` alone uses the function name.

    `trace_name` names the whole trace (`crucible.trace.name`); set it on the root observation.
    """
    if callable(name):
        return Observation(None, "span")(name)
    return Observation(
        name,
        type,
        input=input,
        metadata=metadata,
        tags=tags,
        trace_name=trace_name,
        capture_input=capture_input,
        capture_output=capture_output,
    )


def generation(
    name: str | None = None,
    *,
    model: str | None = None,
    provider: str | None = None,
    input: object = _UNSET,
    metadata: Mapping[str, object] | None = None,
    tags: Sequence[str] | None = None,
    trace_name: str | None = None,
    capture_input: bool = True,
    capture_output: bool = True,
) -> Observation:
    """A model call: an observation of type `generation` with its requested model and provider."""
    return Observation(
        name,
        "generation",
        input=input,
        metadata=metadata,
        tags=tags,
        trace_name=trace_name,
        model=model,
        provider=provider,
        capture_input=capture_input,
        capture_output=capture_output,
    )


def set_input(value: object) -> None:
    """Sets the current span's input (`crucible.input`): a string verbatim, any other value as JSON."""
    trace.get_current_span().set_attribute(_keys.INPUT, _text(value))


def set_output(value: object) -> None:
    """Sets the current span's output (`crucible.output`): a string verbatim, any other value as JSON."""
    trace.get_current_span().set_attribute(_keys.OUTPUT, _text(value))


def set_messages(*, input: object = _UNSET, output: object = _UNSET) -> None:
    """Sets a model call's messages (`gen_ai.input.messages`, `gen_ai.output.messages`) as JSON on the current span.

    Use them instead of `set_input` and `set_output` on generations: they keep the semantic-convention message shape.
    """
    span = trace.get_current_span()
    if input is not _UNSET:
        span.set_attribute(_keys.INPUT_MESSAGES, _text(input))
    if output is not _UNSET:
        span.set_attribute(_keys.OUTPUT_MESSAGES, _text(output))


def set_finish_reasons(*reasons: str) -> None:
    """Sets why the model stopped (`gen_ai.response.finish_reasons`) on the current span."""
    trace.get_current_span().set_attribute(_keys.FINISH_REASONS, list(reasons))


def set_usage(
    *,
    input: int | None = None,
    output: int | None = None,
    cache_read: int | None = None,
    cache_write: int | None = None,
    reasoning: int | None = None,
) -> None:
    """Sets the current span's token usage. Cache reads and writes count inside `input`, reasoning inside `output`."""
    span = trace.get_current_span()
    for key, value in (
        ("input", input),
        ("output", output),
        ("cache_read", cache_read),
        ("cache_write", cache_write),
        ("reasoning", reasoning),
    ):
        if value is None:
            continue
        if isinstance(value, bool) or not isinstance(value, int) or value < 0:
            raise ValueError(f"set_usage: {key} must be a non-negative int, got {value!r}")
        span.set_attribute(_keys.USAGE[key], value)


def set_cost(
    *,
    total: float | Decimal | None = None,
    input: float | Decimal | None = None,
    output: float | Decimal | None = None,
    cache_read: float | Decimal | None = None,
    cache_write: float | Decimal | None = None,
    reasoning: float | Decimal | None = None,
) -> None:
    """Sets the current span's provided cost in USD (`crucible.cost.<key>`). A `Decimal` is sent exactly."""
    span = trace.get_current_span()
    for key, value in (
        ("total", total),
        ("input", input),
        ("output", output),
        ("cache_read", cache_read),
        ("cache_write", cache_write),
        ("reasoning", reasoning),
    ):
        if value is None:
            continue
        if isinstance(value, Decimal):
            if not value.is_finite():
                raise ValueError(f"set_cost: {key} must be finite, got {value!r}")
            span.set_attribute(_keys.COST_PREFIX + key, str(value))
        else:
            span.set_attribute(_keys.COST_PREFIX + key, value)
