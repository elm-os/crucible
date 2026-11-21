"""The capped JSON encoder and the model walk as they were before the fast encoder: the reference its property
tests hold `Scrubber.capped_json` to. It encodes token by token, which is simple and slow; the SDK's encoder must
give the same text and the same cut flag for every input."""

from __future__ import annotations

import json
import math
from collections.abc import Iterable, Iterator, Mapping
from typing import Any

from pydantic import BaseModel
from pydantic_core import to_jsonable_python

from crucible_sdk._scrub import LazyArray, LazyObject, Scrubber

_REDACTED = "[redacted]"
_MAX_DEPTH = 64
_MARGIN = 4096


class ReferenceCappedJson:
    def __init__(self, scrubber: Scrubber, max_bytes: int) -> None:
        self._scrubber = scrubber
        self._left = max_bytes
        self.parts: list[bytes] = []
        self.cut = False

    def text(self) -> str:
        return b"".join(self.parts).decode("utf-8", "ignore")

    def _emit(self, text: str) -> bool:
        data = text.encode("utf-8", "replace")
        if len(data) <= self._left:
            self.parts.append(data)
            self._left -= len(data)
            return True
        self.parts.append(data[: self._left])
        self._left = 0
        self.cut = True
        return False

    def value(self, value: object, depth: int) -> bool:
        if value is None or isinstance(value, bool | int | float):
            return self._emit(json.dumps(value))
        if isinstance(value, str):
            return self._string(value)
        if depth >= _MAX_DEPTH:
            self.cut = True
            return False
        if isinstance(value, Mapping):
            return self._object(value.items(), depth)
        if isinstance(value, LazyObject):
            return self._object(value.members, depth)
        if isinstance(value, list | tuple):
            return self._array(value, depth)
        if isinstance(value, LazyArray):
            return self._array(value.items, depth)
        return self._string(str(value))

    def _string(self, value: str, suffix: str = "") -> bool:
        raw = value[: self._left + _MARGIN]
        text = json.dumps(self._scrubber.text(raw), ensure_ascii=False)
        if len(raw) < len(value):
            self._emit(text[:-1])
            self.cut = True
            return False
        return self._emit(text + suffix)

    def _object(self, members: Iterable[tuple[Any, Any]], depth: int) -> bool:
        if not self._emit("{"):
            return False
        for i, (key, item) in enumerate(members):
            name = key if isinstance(key, str) else str(key)
            if i and not self._emit(","):
                return False
            if not self._string(name, ":"):
                return False
            if Scrubber.is_denied_key(name):
                if not self._emit(json.dumps(_REDACTED)):
                    return False
            elif not self.value(item, depth + 1):
                return False
        return self._emit("}")

    def _array(self, value: Iterable[Any], depth: int) -> bool:
        if not self._emit("["):
            return False
        for i, item in enumerate(value):
            if i and not self._emit(","):
                return False
            if not self.value(item, depth + 1):
                return False
        return self._emit("]")


def reference_capped_json(scrubber: Scrubber, value: object, max_bytes: int) -> tuple[str, bool]:
    """`value` as the reference encoder writes it."""
    encoder = ReferenceCappedJson(scrubber, max_bytes)
    encoder.value(value, 0)
    return encoder.text(), encoder.cut


def reference_model_members(model: BaseModel) -> Iterator[tuple[str, object]]:
    cls = type(model)
    decorators = getattr(cls, "__pydantic_decorators__", None)
    custom = decorators is not None and bool(decorators.model_serializers or decorators.field_serializers)
    if custom or cls.model_computed_fields:
        yield from model.model_dump(mode="json", by_alias=True, exclude_none=True).items()
        return
    for name, info in cls.model_fields.items():
        value = getattr(model, name)
        if value is not None and not info.exclude:
            yield info.serialization_alias or info.alias or name, reference_lazy(value)
    for name, value in (model.model_extra or {}).items():
        if value is not None:
            yield name, reference_lazy(value)


def reference_lazy(value: object) -> object:
    """`value` as `model_dump(mode="json")` has it, every container wrapped to be converted while read."""
    if isinstance(value, BaseModel):
        return LazyObject(reference_model_members(value))
    if isinstance(value, Mapping):
        return LazyObject((reference_json_key(k), reference_lazy(v)) for k, v in value.items())
    if isinstance(value, list | tuple):
        return LazyArray(reference_lazy(v) for v in value)
    if value is None or isinstance(value, str | bool | int):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else None
    return to_jsonable_python(value)


def reference_json_key(key: object) -> str:
    return key if isinstance(key, str) else str(to_jsonable_python(key))


def reference_result(scrubber: Scrubber, result: object, max_bytes: int) -> tuple[str, bool] | None:
    """The captured result text of a tool result, as the middleware made it before the fast encoder."""
    content: object
    if isinstance(result, BaseModel):
        members = dict(reference_model_members(result))
        content = LazyObject((k, members[k]) for k in ("content", "structuredContent", "isError") if k in members)
    elif isinstance(result, Mapping):
        content = {k: result[k] for k in ("content", "structuredContent", "isError") if k in result}
    else:
        return None
    return reference_capped_json(scrubber, content, max_bytes)
