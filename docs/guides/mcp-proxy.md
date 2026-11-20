# MCP proxy

`crucible proxy` records what an MCP server does without changing the server: no SDK, no code change, any
language. It sits between the MCP client and the server, passes every byte through unchanged in both directions,
and sends one span per MCP operation to a Crucible server, which records each as an MCP operation with
`capture_source` `proxy_stdio` or `proxy_http`. Use it for servers you cannot instrument, for servers built on the
1.x line of an official MCP SDK, or to get a first look before adding an SDK. It is a subcommand of the `crucible`
binary and starts no server subsystem.

What an SDK sees and the proxy cannot: error subclasses, the split between validation and not-found errors,
calls the server makes to other systems, and any user identity on stdio. A server can run with both: each call is
then recorded once per capture source, and the rows stay apart by `capture_source`.

## stdio servers

Put `crucible proxy stdio --` in front of the server command:

```sh
crucible proxy stdio -- npx -y @modelcontextprotocol/server-everything
```

In an MCP client configuration:

```json
{
  "mcpServers": {
    "everything": {
      "command": "crucible",
      "args": ["proxy", "stdio", "--", "npx", "-y", "@modelcontextprotocol/server-everything"],
      "env": {
        "CRUCIBLE_ENDPOINT": "http://127.0.0.1:8080",
        "CRUCIBLE_SECRET_KEY": "csk_..."
      }
    }
  }
}
```

Behaviour:

- **Bytes.** Each chunk read from the client is written to the server at once, and each chunk from the server to
  the client at once. Nothing is buffered, re-framed or injected, and there is no line length limit.
- **Standard output** carries only the server's bytes. Everything the proxy says goes to standard error.
- **Standard error** of the server is inherited by default. `--capture-stderr` copies it through the proxy instead.
- **End of input.** When the client closes its end, the proxy closes the server's standard input.
- **Exit.** The proxy exits with the server's exit code, after everything the server wrote has been delivered to
  the client. On Unix, when a signal ended the server, the proxy ends by the same signal.
- **Signals (Unix).** `SIGTERM`, `SIGINT` and `SIGHUP` are forwarded to the server's process group. On Linux the
  server also receives `SIGTERM` if the proxy itself is killed.
- **Windows.** The server starts suspended, joins a Job Object with kill-on-close and then resumes, so the whole
  process tree ends with the proxy. Commands are resolved through `PATH` and `PATHEXT`, so `npx` finds `npx.cmd`.
- **A command that does not exist** is refused with a message that names what was searched and how to fix it, and
  nothing is written to standard output.

The process tests listed under [how it is verified](#how-it-is-verified) pin the signal, exit and Windows
behaviour.

The server process inherits the proxy's environment, `CRUCIBLE_SECRET_KEY` included. Use `--secret-key-file` if
the server must not see the key.

## Streamable HTTP servers

```sh
crucible proxy http --listen 127.0.0.1:8090 --upstream http://127.0.0.1:3000
```

Clients then use `http://127.0.0.1:8090/mcp` where they used `http://127.0.0.1:3000/mcp`. Both protocol eras work:
legacy sessions with `Mcp-Session-Id`, and the stateless requests of the 2026-07-28 revision. `--listen` and
`--upstream` are also read from `CRUCIBLE_PROXY_LISTEN` and `CRUCIBLE_PROXY_UPSTREAM`; a path in the upstream URL is
put in front of every request path.

- **Requests** are forwarded with their method, path, query and headers as they came. `Authorization` is forwarded
  untouched.
- **Responses** come back with the upstream's status and headers. A 401 challenge and the well-known OAuth metadata
  pass through unchanged.
- **Event streams** are forwarded event by event, keep-alive comments included. The proxy adds no timeout except
  the connect timeout (10 s).
- **Bodies** are byte-identical. HTTP/1.1 framing (where chunk boundaries fall) belongs to the transport and may
  differ.
- **`Host`.** By default the upstream sees the `Host` header the client sent, so a token audience bound to the public
  name stays valid. `--host-header upstream` sends the upstream's own host instead, for upstreams that route by host
  name.
- **A client that disconnects** closes the upstream connection of that client, which aborts the request in flight.
  The operation is recorded as cancelled with `cancel_source = stream_close`.
- **An upstream that cannot be reached** is answered with `502` and a one-line reason, the only response the proxy
  writes itself:

  ```text
  crucible proxy: the upstream MCP server at 127.0.0.1:3999 could not be reached: Connection refused (os error 111)
  ```

The listener speaks plain HTTP/1.1. To serve clients outside the host or the internal network, put the proxy behind
the TLS terminator that already fronts the MCP server. `https` upstreams work.

### Identity (opt-in)

By default the proxy records no caller identity. Two modes are available, and both leave the token itself untouched
and never store it:

| `--identity` | What is recorded | Label on the row |
|---|---|---|
| `off` (default) | Nothing | none |
| `claims` | Issuer, subject, client id and tenant from the token's payload, without checking the signature | `token_claims` (unverified) |
| `verified` | The same claims, only from tokens whose signature verifies against `--jwks-url` and that have not expired; `--jwt-audience` and `--jwt-issuer` add those checks | `token_verified` |

A token that is opaque, malformed or fails verification yields no identity. The request is forwarded the same in
every case: the proxy observes, and the MCP server decides. The subject is sent to the Crucible server raw and hashed
there with the project's salt (contract 9.3). Each of these flags is also read from an environment variable:
`CRUCIBLE_PROXY_IDENTITY`, `CRUCIBLE_PROXY_JWKS_URL`, `CRUCIBLE_PROXY_JWT_AUDIENCE` and `CRUCIBLE_PROXY_JWT_ISSUER`.

## Telemetry settings

| Flag | Environment variable | Meaning |
|---|---|---|
| `--endpoint <url>` | `CRUCIBLE_ENDPOINT` | The Crucible server, such as `http://127.0.0.1:8080`. Without it the proxy only forwards, and says so on standard error |
| `--secret-key-file <path>` | `CRUCIBLE_SECRET_KEY_FILE`, or the key itself in `CRUCIBLE_SECRET_KEY` | A project secret key with the `ingest` scope |
| `--server-name <name>` | `CRUCIBLE_SERVER_NAME` | The `service.name` of the proxy's spans. Default: the command's file name (stdio) or the upstream host (http). Operations carry the name the server gives in its `initialize` answer, and this name until it has given one |
| `--deploy-id <id>` | `CRUCIBLE_DEPLOY_ID` | The deploy id of the server, recorded on every operation |

An endpoint without a key is refused at startup, with a message that names both ways to give the key and how to
run without telemetry.

Spans are sent as OTLP/HTTP protobuf to `/v1/traces`. They carry the OTel MCP semantic-convention keys and the
`crucible.mcp.*` keys of contract 9.1: method, tool or prompt name, JSON-RPC id, direction, era and protocol
version, client and server name, outcome class, JSON-RPC code, HTTP status, progress count, cancellation, and the
exact sizes of the request and response messages. When a request carries a `traceparent` in `params._meta`, the
span joins that trace; the proxy reads trace context and never writes it.

Crucible records each span as an MCP operation on the server side. A session through `crucible proxy stdio` with
the server above gave one row per message in the SQL workspace's `crucible_sql.mcp_operations`, with
`capture_source = proxy_stdio`:

| `method` | `target_name` | `status` | `error_class` | `jsonrpc_code` | `request_bytes` | `response_bytes` |
|---|---|---|---|---|---|---|
| `initialize` | | `ok` | `none` | | 160 | 4968 |
| `notifications/initialized` | | `ok` | `none` | | 54 | 0 |
| `tools/list` | | `ok` | `none` | | 58 | 25755 |
| `tools/call` | `echo` | `ok` | `none` | | 118 | 99 |
| `tools/call` | `no_such_tool` | `protocol_error` | `validation` | -32602 | 94 | 88 |

Through `crucible proxy http` the rows carry `capture_source = proxy_http`, the era (`legacy` for a session), the
hash of the session id, the client's `/24` or `/48` address prefix and its user agent.

Never sent: argument and result content, `Authorization` values, cookies, `Mcp-Param-*` values, raw session ids
(only their hash), raw IP addresses (only the /24 or /48 prefix) and `requestState`.

Not recorded by the proxy: argument and result content, error messages, multi-round state hashes, and the contents
of compressed bodies (such a request is reported from its `Mcp-Method` and `Mcp-Name` headers and the HTTP status
only). An SDK wrapper records these ([MCP](mcp.md)).

## When Crucible is down

The proxy fails open. Telemetry runs on its own task, on a copy of the traffic:

- The copy goes through a bounded queue with a send that never waits. When the queue is full, the copy is dropped
  and counted; the bytes are already on their way.
- Finished operations wait in a buffer of at most 8 MiB or 10,000 operations. When it is full, the oldest are
  dropped. The number dropped is logged on standard error at the next export that succeeds.
- Exports are retried with a backoff of 1 s doubling to 30 s. A refusal (for example a wrong key) drops the batch
  and says what to check.
- At exit the proxy waits at most 500 ms for a last export.
- Requests still waiting for a response when the server exits are recorded as transport errors.

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
the same machine. Times are in milliseconds. The largest added p99 was 0.3956 ms (stdio: 0.1289 ms; HTTP: 0.3956 ms).

| Path | Collector | Direct p50 ms | Direct p99 ms | Proxy p50 ms | Proxy p99 ms | Added p50 ms | Added p99 ms | Gate (added p99 at most 1 ms) |
|---|---|---|---|---|---|---|---|---|
| stdio, sequential | healthy | 0.0277 | 0.0535 | 0.0944 | 0.1824 | 0.0667 | 0.1289 | pass |
| stdio, pipelined (per request) | healthy | 0.0010 | 0.0023 | 0.0020 | 0.0037 | 0.0011 | 0.0015 | pass |
| stdio, sequential | refused | 0.0274 | 0.0555 | 0.0941 | 0.1835 | 0.0667 | 0.1280 | pass |
| stdio, pipelined (per request) | refused | 0.0009 | 0.0013 | 0.0018 | 0.0048 | 0.0010 | 0.0034 | pass |
| stdio, sequential | stalled | 0.0276 | 0.0623 | 0.0981 | 0.1885 | 0.0705 | 0.1262 | pass |
| stdio, pipelined (per request) | stalled | 0.0009 | 0.0015 | 0.0020 | 0.0038 | 0.0011 | 0.0023 | pass |
| http, JSON answer | healthy | 0.0359 | 0.0962 | 0.1113 | 0.2246 | 0.0754 | 0.1284 | pass |
| http, SSE first byte | healthy | 0.0390 | 0.0905 | 0.1169 | 0.2427 | 0.0779 | 0.1522 | pass |
| http, SSE whole stream | healthy | 0.0407 | 0.1016 | 0.1248 | 0.2744 | 0.0841 | 0.1728 | pass |
| http, JSON answer | refused | 0.0360 | 0.0960 | 0.1098 | 0.2294 | 0.0738 | 0.1333 | pass |
| http, SSE first byte | refused | 0.0380 | 0.0893 | 0.1122 | 0.2329 | 0.0742 | 0.1437 | pass |
| http, SSE whole stream | refused | 0.0396 | 0.0968 | 0.1213 | 0.2616 | 0.0818 | 0.1648 | pass |
| http, JSON answer | stalled | 0.0488 | 0.1195 | 0.1313 | 0.2773 | 0.0825 | 0.1578 | pass |
| http, SSE first byte | stalled | 0.0508 | 0.1358 | 0.1424 | 0.4691 | 0.0916 | 0.3333 | pass |
| http, SSE whole stream | stalled | 0.0528 | 0.1510 | 0.1564 | 0.5466 | 0.1036 | 0.3956 | pass |

"Added" is the difference of the percentiles, so it can be slightly negative where the two runs are within
noise. Absolute times will differ on other hosts; the gate is on the added p99.
