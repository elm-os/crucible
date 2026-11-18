"""Runs the reference server over stdio, exporting to the collector URL given as the first argument."""

from __future__ import annotations

import sys

import crucible_sdk
from tests import reference_server
from tests.conftest import SECRET_KEY

if __name__ == "__main__":
    crucible_sdk.init(endpoint=sys.argv[1], secret_key=SECRET_KEY, service_name="stdio-server", flush_interval=60)
    reference_server.instrumented().run("stdio")
