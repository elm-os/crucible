from __future__ import annotations

from collections.abc import Iterator

import pytest

import crucible_sdk
from tests.collector import Collector, running

# The conformance suite's key (contract 11.3 format: 43 base62 characters after the prefix).
SECRET_KEY = "csk_conformance00000000000000000000000000000000"


@pytest.fixture
def anyio_backend() -> str:
    return "asyncio"


@pytest.fixture
def collector() -> Iterator[Collector]:
    with running() as c:
        yield c


@pytest.fixture
def sdk(collector: Collector) -> Iterator[Collector]:
    """The SDK exporting to a fresh collector; spans leave only through flush, shutdown or a full batch."""
    crucible_sdk.init(
        endpoint=collector.url,
        secret_key=SECRET_KEY,
        service_name="crucible-test",
        environment="test",
        release="2.0.0",
        deploy_id="deploy-1",
        flush_interval=60.0,
    )
    yield collector
    crucible_sdk.shutdown()
