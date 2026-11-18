# MCP proxy

Status: planned for the first release. The proxy is implemented in `crates/crucible-proxy` and tested as described
under "How it is verified", but no release contains it yet, so this page describes what the released
`crucible proxy` will do.

The proxy will record what an MCP server does without changing the server: no SDK, no code change, any language.
It will sit between the MCP client and the server, pass every byte through unchanged in both directions, and send
one span per MCP operation to a Crucible server. Use it for servers you cannot instrument, for servers built on
the 1.x line of an official MCP SDK, or to get a first look before adding an SDK.

What an SDK sees and the proxy cannot: error subclasses, the split between validation and not-found errors,
calls the server makes to other systems, and any user identity on stdio. A server can run with both; the
analytics endpoints then pick one source per server (SDK first, then proxy), and never add the two.

## stdio servers

Put `crucible proxy stdio --` in front of the server command:

```sh
crucible proxy stdio -- npx -y @example/mcp-server --root /data
```

In an MCP client configuration this will look like:

```json
{
  "mcpServers": {
    "example": {
      "command": "crucible",
      "args": ["proxy", "stdio", "--", "npx", "-y", "@example/mcp-server", "--root", "/data"],
      "env": {
        "CRUCIBLE_ENDPOINT": "http://127.0.0.1:8080",
        "CRUCIBLE_SECRET_KEY": "csk_..."
      }
    }
  }
}
```

Behaviour:

- **Bytes.** Each chunk read from the client will be written to the server at once, and each chunk from the server
  to the client at once. Nothing will be buffered, re-framed or injected, and there will be no line length limit.
- **Standard output** will carry only the server's bytes. Everything the proxy says will go to standard error.
- **Standard error** of the server will be inherited by default. `--capture-stderr` will copy it through the proxy
  instead.
- **End of input.** When the client closes its end, the proxy will close the server's standard input.
- **Exit.** The proxy will exit with the server's exit code, after everything the server wrote has been
  delivered to the client. On Unix, when a signal ended the server, the proxy will end by the same signal.
- **Signals (Unix).** `SIGTERM`, `SIGINT` and `SIGHUP` will be forwarded to the server's process group. On Linux
  the server will also receive `SIGTERM` if the proxy itself is killed.
- **Windows.** The server will start suspended, join a Job Object with kill-on-close and then resume, so the whole
  process tree ends with the proxy. Commands will be resolved through `PATH` and `PATHEXT`, so `npx` finds
  `npx.cmd`.
- **A command that does not exist** will be refused with a message that names what was searched and how to fix
  it, and nothing will be written to standard output.

The server process inherits the proxy's environment, `CRUCIBLE_SECRET_KEY` included. Use `--secret-key-file` if
the server must not see the key.

## Streamable HTTP servers

```sh
crucible proxy http --listen 127.0.0.1:8090 --upstream http://127.0.0.1:3000
```

Clients will then use `http://127.0.0.1:8090/mcp` where they used `http://127.0.0.1:3000/mcp`. Both protocol
eras will work: legacy sessions with `Mcp-Session-Id`, and the stateless requests of the 2026-07-28 revision.

- **Requests** will be forwarded with their method, path, query and headers as they came. `Authorization` will be
  forwarded untouched.
- **Responses** will come back with the upstream's status and headers. A 401 challenge and the well-known OAuth
  metadata will pass through unchanged.
- **Event streams** will be forwarded event by event, keep-alive comments included. The proxy will add no timeout
  except the connect timeout (10 s).
- **Bodies** will be byte-identical. HTTP/1.1 framing (where chunk boundaries fall) belongs to the transport and
  may differ.
- **`Host`.** By default the upstream will see the `Host` header the client sent, so a token audience bound to the
  public name stays valid. `--host-header upstream` will send the upstream's own host instead, for upstreams that
  route by host name.
- **A client that disconnects** will close the upstream connection of that client, which aborts the request in
  flight. The operation will be recorded as cancelled with `cancel_source = stream_close`.
- **An upstream that cannot be reached** will be answered with `502` and a one-line reason. This will be the only
  response the proxy writes itself.

The listener will speak plain HTTP/1.1. To serve clients outside the host or the internal network, put the proxy
behind the TLS terminator that already fronts the MCP server. `https` upstreams will be supported.

### Identity (opt-in)

By default the proxy will record no caller identity. Two modes will be available, both of which leave the token
itself untouched and never store it:

| `--identity` | What will be recorded | Label on the row |
|---|---|---|
| `off` (default) | Nothing | none |
| `claims` | Issuer, subject, client id and tenant from the token's payload, without checking the signature | `token_claims` (unverified) |
| `verified` | The same claims, only from tokens whose signature verifies against `--jwks-url` and that have not expired; `--jwt-audience` and `--jwt-issuer` add those checks | `token_verified` |

A token that is opaque, malformed or fails verification will yield no identity. The request will be forwarded the
same in every case: the proxy observes, and the MCP server decides. The subject will be sent to the Crucible
server raw and hashed there with the project's salt (contracts 9.3).

## Telemetry settings

| Flag | Environment variable | Meaning |
|---|---|---|
| `--endpoint <url>` | `CRUCIBLE_ENDPOINT` | The Crucible server, such as `http://127.0.0.1:8080`. Without it the proxy will only forward and will say so on standard error |
| `--secret-key-file <path>` | `CRUCIBLE_SECRET_KEY_FILE`, or the key itself in `CRUCIBLE_SECRET_KEY` | A project secret key with the `ingest` scope |
| `--server-name <name>` | `CRUCIBLE_SERVER_NAME` | The name the server is reported under. Default: the command's file name (stdio) or the upstream host (http) |
| `--deploy-id <id>` | `CRUCIBLE_DEPLOY_ID` | The deploy id, for deploy comparison |

An endpoint without a key will be refused at startup, with a message that names both ways to give the key and how
to run without telemetry.

Spans will be sent as OTLP/HTTP protobuf to `/v1/traces` with `capture_source` set to `proxy_stdio` or
`proxy_http`. They will carry the OTel MCP semantic-convention keys and the `crucible.mcp.*` keys of contracts
9.1: method, tool or prompt name, JSON-RPC id, direction, era and protocol version, client and server name,
outcome class, JSON-RPC code, HTTP status, progress count, cancellation, and the exact sizes of the request and
response messages. When a request carries a `traceparent` in `params._meta`, the span will join that trace; the
proxy will read trace context and never write it.

Never sent: argument and result content, `Authorization` values, cookies, `Mcp-Param-*` values, raw session ids
(only their hash), raw IP addresses (only the /24 or /48 prefix) and `requestState`.

Not recorded by the proxy in this release: argument and result capture, error messages, multi-round state hashes,
and analysis of compressed bodies (such a request will be reported from its `Mcp-Method` and `Mcp-Name` headers
and the HTTP status only).

## When Crucible is down

The proxy will fail open. Telemetry will run on its own task, on a copy of the traffic:

- The copy will go through a bounded queue with a send that never waits. When the queue is full, the copy will be
  dropped and counted; the bytes will already be on their way.
- Finished operations will wait in a buffer of at most 8 MiB or 10,000 operations. When it is full, the oldest
  will be dropped. The number dropped will be logged on standard error at the next export that succeeds.
- Exports will be retried with a backoff of 1 s doubling to 30 s. A refusal (for example a wrong key) will drop
  the batch and say what to check.
- At exit the proxy will wait at most 500 ms for a last export.
- Requests still waiting for a response when the server exits will be recorded as transport errors.

## How it is verified

Invariant I11 (`docs/testing/invariants.md`): with the Crucible sink down, slow or erroring, the bytes exchanged
are identical to a run with no telemetry, for every chunking, and added latency stays under the bound.

| Check | Where |
|---|---|
| The proxy core as a state machine with no IO, over 10,000 seeds: a scripted server that stalls, crashes and exits, random chunk splits, analyser schedules, queue sizes and five collector states. Every chunk comes out as it went in, drops are counted, and every operation is accounted for | `crates/crucible-proxy/tests/core_dst.rs` |
| Two seeded defects that those checks must catch: one changes a byte, one holds bytes back while the queue is full | the same file, feature `test-mutants` |
| The scanner against a full JSON parse under arbitrary chunking and escaping | `crates/crucible-proxy/tests/scan_diff.rs` |
| The pumps against a scripted server task behind the `ChildProcess` trait, 150 seeds: chunk splits on both sides, stalls, pipes of every size, crashes in mid line, exit codes, and a client that starts reading three seconds after the server exited | `crates/crucible-proxy/tests/stdio_scripted.rs` |
| Real processes on Linux and Windows: collector healthy, refused, stalled and absent; lines over 1 MiB; exit codes; a crash in mid answer; signals; end of input; the process tree; a `.cmd` shim | `crates/crucible-proxy/tests/stdio_process.rs` |
| Real sockets: the same requests with and without the proxy, event streams, the 401 challenge, a legacy session, client disconnect, identity modes, and a planted token that must appear in no export | `crates/crucible-proxy/tests/http_process.rs` |
| The `crucible` binary itself: exit code, dying of the child's signal, the parent-death signal, a clean standard output; on Windows, the server's process tree ending when the proxy process is killed | `crates/crucible/tests/proxy.rs`, `crates/crucible/tests/proxy_windows.rs` |
| Every exported attribute against the key registry of the SDK conformance suite | the process tests, with `conformance/registry.json` |

### Overhead

Reproduce with:

```sh
cargo build --release -p crucible -p crucible-proxy
target/release/crucible-proxy-rig bench --crucible target/release/crucible --label "<host>"
```

The benchmark measures the round trip a client observes with and without the proxy, in alternating rounds, for
each collector state, and exits with 1 when an added p99 is over 1 ms.

Measured on 2026-11-18 on a laptop (Windows 11 host, Docker Desktop Linux container limited to 3 CPUs and 2 GiB,
release build), 100,000 calls per path and collector state in 20 alternating rounds, while other builds ran on
the same machine. Times are in milliseconds. The largest added p99 was 0.1953 ms.

| Path | Collector | Direct p50 ms | Direct p99 ms | Proxy p50 ms | Proxy p99 ms | Added p50 ms | Added p99 ms | Gate (added p99 at most 1 ms) |
|---|---|---|---|---|---|---|---|---|
| stdio, sequential | healthy | 0.0257 | 0.0681 | 0.0949 | 0.2196 | 0.0692 | 0.1515 | pass |
| stdio, pipelined (per request) | healthy | 0.0009 | 0.0026 | 0.0021 | 0.0103 | 0.0012 | 0.0076 | pass |
| stdio, sequential | refused | 0.0263 | 0.0718 | 0.0943 | 0.2322 | 0.0680 | 0.1604 | pass |
| stdio, pipelined (per request) | refused | 0.0010 | 0.0025 | 0.0022 | 0.0051 | 0.0012 | 0.0026 | pass |
| stdio, sequential | stalled | 0.0263 | 0.0693 | 0.0871 | 0.2167 | 0.0608 | 0.1474 | pass |
| stdio, pipelined (per request) | stalled | 0.0009 | 0.0015 | 0.0018 | 0.0032 | 0.0010 | 0.0017 | pass |
| http, JSON answer | healthy | 0.0341 | 0.1089 | 0.0993 | 0.2338 | 0.0652 | 0.1248 | pass |
| http, SSE first byte | healthy | 0.0339 | 0.0927 | 0.1105 | 0.2438 | 0.0765 | 0.1511 | pass |
| http, SSE whole stream | healthy | 0.0350 | 0.0994 | 0.1178 | 0.2947 | 0.0827 | 0.1953 | pass |
| http, JSON answer | refused | 0.0432 | 0.1057 | 0.1192 | 0.2308 | 0.0761 | 0.1251 | pass |
| http, SSE first byte | refused | 0.0452 | 0.1046 | 0.1270 | 0.2523 | 0.0818 | 0.1477 | pass |
| http, SSE whole stream | refused | 0.0468 | 0.1124 | 0.1360 | 0.2953 | 0.0892 | 0.1828 | pass |
| http, JSON answer | stalled | 0.0403 | 0.1010 | 0.1082 | 0.2432 | 0.0680 | 0.1423 | pass |
| http, SSE first byte | stalled | 0.0425 | 0.1107 | 0.1200 | 0.2431 | 0.0775 | 0.1324 | pass |
| http, SSE whole stream | stalled | 0.0439 | 0.1197 | 0.1307 | 0.3006 | 0.0867 | 0.1809 | pass |

"Added" is the difference of the percentiles, so it can be slightly negative where the two runs are within
noise. Absolute times will differ on other hosts; the gate is on the added p99.
