"""The reference server as its own process, instrumented with a run spec's `sdk` options.

Run as `python -m conformance.server <config>` from `sdk/python`, where `<config>` is JSON with `transport`,
`endpoint`, `secret_key`, `sdk` and `server`. Over stdio it serves until its stdin closes. Over streamable HTTP it binds
`127.0.0.1` on a free port, prints `{"port": N}` on stdout and serves `/mcp` until its stdin closes. Either way it
flushes the SDK before it exits.
"""

from __future__ import annotations

import json
import socket
import sys
import threading
from typing import Any

import uvicorn
from mcp.server.mcpserver import MCPServer

import crucible_sdk
import crucible_sdk.mcp
from conformance import reference_server


def serve_http(server: MCPServer) -> None:
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    # Listening before the port is printed: a connection that arrives before uvicorn accepts waits in the backlog.
    sock.listen(128)
    http = uvicorn.Server(
        uvicorn.Config(server.streamable_http_app(), log_level="warning", timeout_graceful_shutdown=5)
    )

    def stop_when_stdin_closes() -> None:
        sys.stdin.read()
        http.should_exit = True

    threading.Thread(target=stop_when_stdin_closes, daemon=True).start()
    print(json.dumps({"port": sock.getsockname()[1]}), flush=True)
    http.run(sockets=[sock])


def main(config: dict[str, Any]) -> None:
    sdk = config["sdk"]
    crucible_sdk.init(
        endpoint=config["endpoint"],
        secret_key=config["secret_key"],
        service_name=sdk.get("service_name"),
        deploy_id=sdk.get("deploy_id"),
        flush_interval=max(int(sdk.get("flush_interval_ms", 1000)), 1) / 1000,
    )
    identity = config["server"]
    server = crucible_sdk.mcp.instrument(
        reference_server.build(identity["name"], identity["version"]),
        capture=[crucible_sdk.mcp.Capture(**rule) for rule in sdk.get("capture", ())],
        handle_params=sdk.get("handle_params") or {},
        transport=config["transport"],
    )
    try:
        if config["transport"] == "stdio":
            server.run("stdio")
        else:
            serve_http(server)
    finally:
        crucible_sdk.shutdown(10.0)


if __name__ == "__main__":
    main(json.loads(sys.argv[1]))
