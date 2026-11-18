"""Context carried to every span at start: session, user, tags and metadata (contracts 3.3 and 10.6)."""

from __future__ import annotations

import functools
import inspect
from collections.abc import Callable, Mapping
from contextvars import ContextVar, Token
from dataclasses import dataclass, replace
from types import TracebackType
from typing import Any, TypeVar, cast

from opentelemetry.sdk.trace import Span
from opentelemetry.trace import SpanKind
from opentelemetry.util.types import AttributeValue

from crucible_sdk import _keys

F = TypeVar("F", bound=Callable[..., Any])

# The instrumentation scope of the spans the mcp library opens itself.
MCP_LIBRARY_SCOPE = "mcp-python-sdk"


@dataclass(frozen=True)
class Scope:
    """The values every span started in this context carries."""

    session_id: str | None = None
    user_id: str | None = None
    tags: tuple[str, ...] = ()
    metadata: tuple[tuple[str, AttributeValue], ...] = ()


_EMPTY = Scope()
SCOPE: ContextVar[Scope] = ContextVar("crucible_sdk.scope", default=_EMPTY)

# Attributes for the mcp library's CLIENT spans started while an instrumented client call runs.
CLIENT_CALL: ContextVar[Mapping[str, AttributeValue] | None] = ContextVar("crucible_sdk.client_call", default=None)


def apply_to_span(span: Span) -> None:
    """Copies the scope onto a starting span, keeping any value the span already set."""
    scope = SCOPE.get()
    attributes = span.attributes or {}
    if scope.session_id is not None and _keys.SESSION_ID not in attributes:
        span.set_attribute(_keys.SESSION_ID, scope.session_id)
    if scope.user_id is not None and _keys.USER_ID not in attributes:
        span.set_attribute(_keys.USER_ID, scope.user_id)
    if scope.tags and _keys.TAGS not in attributes:
        span.set_attribute(_keys.TAGS, list(scope.tags))
    for key, value in scope.metadata:
        name = _keys.METADATA_PREFIX + key
        if name not in attributes:
            span.set_attribute(name, value)
    tags = CLIENT_CALL.get()
    if (
        tags is not None
        and span.kind is SpanKind.CLIENT
        and span.instrumentation_scope is not None
        and span.instrumentation_scope.name == MCP_LIBRARY_SCOPE
    ):
        method = attributes.get(_keys.MCP_METHOD)
        span.set_attributes({k: v for k, v in tags.items() if k != _keys.TOOL_NAME or method == "tools/call"})


class ScopeUpdate:
    """Sets scope values for a block (`with`) or for every call of a function (decorator)."""

    def __init__(self, update: Callable[[Scope], Scope]) -> None:
        self._update = update
        self._tokens: list[Token[Scope]] = []

    def __enter__(self) -> None:
        self._tokens.append(SCOPE.set(self._update(SCOPE.get())))

    def __exit__(
        self, exc_type: type[BaseException] | None, exc: BaseException | None, tb: TracebackType | None
    ) -> None:
        SCOPE.reset(self._tokens.pop())

    def __call__(self, func: F) -> F:
        update = self._update
        if inspect.iscoroutinefunction(func):

            @functools.wraps(func)
            async def run_async(*args: Any, **kwargs: Any) -> Any:
                token = SCOPE.set(update(SCOPE.get()))
                try:
                    return await func(*args, **kwargs)
                finally:
                    SCOPE.reset(token)

            return cast("F", run_async)

        @functools.wraps(func)
        def run(*args: Any, **kwargs: Any) -> Any:
            token = SCOPE.set(update(SCOPE.get()))
            try:
                return func(*args, **kwargs)
            finally:
                SCOPE.reset(token)

        return cast("F", run)


def session(session_id: str) -> ScopeUpdate:
    """Every span started inside carries this session id (`crucible.session.id`)."""
    return ScopeUpdate(lambda s: replace(s, session_id=session_id))


def user(user_id: str) -> ScopeUpdate:
    """Every span started inside carries this user id (`crucible.user.id`)."""
    return ScopeUpdate(lambda s: replace(s, user_id=user_id))


def tags(*values: str) -> ScopeUpdate:
    """Every span started inside carries these tags (`crucible.tags`), after any tags of the enclosing scope."""
    return ScopeUpdate(lambda s: replace(s, tags=tuple(dict.fromkeys((*s.tags, *values)))))


def metadata(**values: AttributeValue) -> ScopeUpdate:
    """Every span started inside carries these metadata entries (`crucible.metadata.<key>`)."""
    return ScopeUpdate(lambda s: replace(s, metadata=tuple({**dict(s.metadata), **values}.items())))
