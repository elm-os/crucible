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
| `--capture <rule>` | `CRUCIBLE_PROXY_CAPTURE`, rules separated by `;` | Records the arguments and results of matching tool calls; see [content capture](#content-capture-opt-in). Repeat the flag for more rules |

An endpoint without a key is refused at startup, with a message that names both ways to give the key and how to
run without telemetry.

Spans are sent as OTLP/HTTP protobuf to `/v1/traces`. They carry the OTel MCP semantic-convention keys and the
`crucible.mcp.*` keys of contract 9.1: method, tool or prompt name, JSON-RPC id, direction, era and protocol
version, client and server name, outcome class, JSON-RPC code, HTTP status, progress count, cancellation, the
error message of a failed operation, the keyed hashes of a multi-round exchange, and the exact sizes of the request
and response messages, and the arguments and results of the calls a `--capture` rule records. When a request
carries a `traceparent` in `params._meta`, the span joins that trace; the proxy reads trace context and never
writes it.

- **Error messages.** A failed operation carries the server's own words: the `message` of a JSON-RPC error, or the
  text of the first text block of a result with `isError: true`. They are scrubbed with the rules the SDKs apply
  (`conformance/scrub-rules.json`: email addresses, phone numbers, IP addresses, card numbers, keys, tokens and
  private keys become `[redacted:<kind>]`) and cut at 2 KiB. When the server gave no words, the message says what
  the proxy saw: `cancelled`, a connection that ended before the answer, or the HTTP status.
- **Multi-round exchanges.** Each operation whose request carries a `requestState`, and each `input_required`
  result that returns one, carries the keyed hash of that state (contract 9.6): the first 16 bytes of HMAC-SHA256
  keyed with the proxy's secret key, as an SDK computes it. Crucible chains the rounds of an exchange by these
  hashes. The state itself is hashed as it streams past and is never kept or sent.

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

Never sent: `Authorization` values, cookies, `Mcp-Param-*` values, raw session ids (only their hash), raw IP
addresses (only the /24 or /48 prefix), raw `requestState` values (only their keyed hash), unscrubbed error messages
and unscrubbed content. Without a `--capture` rule no argument or result content is sent; the text of a failed tool
result's first text block becomes the operation's error message, scrubbed and cut at 2 KiB.

The contents of compressed bodies are not read: such a request is reported from its `Mcp-Method` and `Mcp-Name`
headers and the HTTP status only, and its content is not captured.

### Content capture (opt-in)

`--capture` records the arguments and results of the tool calls you name, with the rules the SDKs apply (contract
9.10). A rule is a tool name glob (`*` matches any run of characters, `?` one character), then options:

```sh
crucible proxy stdio --capture 'search_*' --capture 'lookup_order,result=false,sample_rate=0.1,max_bytes=4096' -- npx -y @modelcontextprotocol/server-everything
```

| Option | Default | Meaning |
|---|---|---|
| `args` | `true` | Record `params.arguments` as `gen_ai.tool.call.arguments` |
| `result` | `true` | Record the result's `content`, `structuredContent` and `isError` as `gen_ai.tool.call.result` |
| `sample_rate` | `1` | The share of matching calls recorded, from 0 to 1 |
| `max_bytes` | `16384` | The cap per field, in bytes, from 1 to 8,323,072 (8 MiB less 64 KiB, so that a field captured at the cap fits the 8 MiB export buffer with its operation) |

`tool=<glob>` is accepted for the glob too. A rule the proxy cannot read is refused at startup with what to write
instead, for example `max_bytes must be a whole number from 1 to 8323072 (8 MiB less 64 KiB, so that a field
captured at the cap fits the proxy's 8 MiB export buffer with its operation), not "lots"`.

- **Which calls.** Only `tools/call` requests from the client. The first rule whose glob matches the tool applies;
  a tool no rule matches is not recorded. In `CRUCIBLE_PROXY_CAPTURE`, rules are separated by `;`.
- **Sampling.** Each call is sampled against its rule's rate once its tool name is read. Arguments that follow the
  tool name in the request are read only for a sampled call. Arguments sent before the tool name are held, at most
  the largest cap plus 1 KiB, until the name arrives, and dropped unless the call is recorded.
- **What is recorded.** Each field is JSON text written again from the wire: whitespace between tokens is dropped
  and every string is decoded and escaped with only the escapes JSON requires, so an address written as
  `ada@example.com` is scrubbed like `ada@example.com`. The result keeps `content`, `structuredContent` and
  `isError` only, so a `requestState` or `_meta` member is never recorded.
- **Scrubbing and the cap.** The shared rules of `conformance/scrub-rules.json` run over the cap plus 1 KiB of each
  field, so a secret that starts before the cap is replaced whole, and the field is then cut at the cap. A field
  that was cut carries `crucible.mcp.content.truncation = cap_exceeded`. However large an argument or result, the
  proxy keeps and scrubs at most the cap plus 1 KiB of it.
- **The keys.** A recorded call carries `crucible.mcp.content.captured = true`, its rule's
  `crucible.mcp.content.sample_rate` and `crucible.mcp.content.truncation`, under the keys the SDKs send. Crucible
  keeps the content beside the operation, under `mcp_content_retention_days`.
- **Bounds.** Recording runs on the analysis task, never on the forwarding path. Arguments that wait for their
  response count toward a bound of 8 MiB; past it a call keeps its operation and loses its content, and the number
  of such calls is logged on standard error after the next export. Recorded content counts toward the 8 MiB
  telemetry buffer.

## When Crucible is down

The proxy fails open. Telemetry runs on its own task, on a copy of the traffic:

- The copy goes through a bounded queue with a send that never waits. When the queue is full, the copy is dropped
  and counted; the bytes are already on their way.
- Finished operations wait in a buffer of at most 8 MiB or 10,000 operations. When it is full, the oldest are
  dropped. The number dropped is logged on standard error at the next export that succeeds. A call whose arguments
  and result are both captured, under a `max_bytes` above 4 MiB, can make one operation larger than the buffer:
  every operation waiting before it is dropped to make room, and then the operation itself.
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
| Error messages scrubbed with the shared rules and cut at 2 KiB, the multi-round state hashes, and a raw `requestState` that never leaves the proxy: in the core, through a real child process, and against a full JSON parse of the scanner's input | `crates/crucible-proxy/tests/core_parity.rs`, `crates/crucible-proxy/tests/stdio_process.rs`, `crates/crucible-proxy/tests/scan_diff.rs` |
| Content capture: the SDKs' rules for which calls and what of them, sampling, escapes decoded before scrubbing, the cap with no part of a secret left at it, either order of tool name and arguments, the bound on waiting content, JSON and event-stream answers | `crates/crucible-proxy/tests/core_capture.rs` |
| The recorded text against a full JSON parse under arbitrary chunking, escaping and whitespace | `crates/crucible-proxy/tests/scan_diff.rs` |
| Capture in the simulation: a swarm flag over the 10,000 seeds, every property above held, and the same seed without capture giving the same operations apart from their content | `crates/crucible-proxy/tests/core_dst.rs` |
| The shared content-capture scenario (`mcp-secret`) in its four variants through the SDK conformance driver, judged by its golden, the registry and the real normalizer; the capture-overhead shapes read no more than the work bound | `crates/crucible-proxy/tests/conformance.rs` |
| Capture through a real child process: the bytes unchanged, content scrubbed and cut | `crates/crucible-proxy/tests/stdio_process.rs` |
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

Measured on 2026-11-20 on a laptop (Windows 11 host, Docker Desktop Linux container limited to 3 CPUs and 2 GiB,
release build), 100,000 calls per path and collector state in 20 alternating rounds, while other builds ran on
the same machine. Times are in milliseconds. The largest added p99 was 0.2245 ms (stdio: 0.2245 ms; HTTP: 0.1871 ms).

| Path | Collector | Direct p50 ms | Direct p99 ms | Proxy p50 ms | Proxy p99 ms | Added p50 ms | Added p99 ms | Gate (added p99 at most 1 ms) |
|---|---|---|---|---|---|---|---|---|
| stdio, sequential | healthy | 0.0233 | 0.0704 | 0.0780 | 0.2814 | 0.0547 | 0.2110 | pass |
| stdio, pipelined (per request) | healthy | 0.0009 | 0.0036 | 0.0017 | 0.0029 | 0.0008 | -0.0007 | pass |
| stdio, sequential | refused | 0.0217 | 0.0689 | 0.0806 | 0.2934 | 0.0588 | 0.2245 | pass |
| stdio, pipelined (per request) | refused | 0.0009 | 0.0024 | 0.0017 | 0.0045 | 0.0008 | 0.0021 | pass |
| stdio, sequential | stalled | 0.0231 | 0.0729 | 0.0781 | 0.2838 | 0.0551 | 0.2109 | pass |
| stdio, pipelined (per request) | stalled | 0.0009 | 0.0014 | 0.0017 | 0.0029 | 0.0009 | 0.0015 | pass |
| http, JSON answer | healthy | 0.0267 | 0.0918 | 0.0657 | 0.2301 | 0.0391 | 0.1383 | pass |
| http, SSE first byte | healthy | 0.0270 | 0.1046 | 0.0860 | 0.2595 | 0.0590 | 0.1550 | pass |
| http, SSE whole stream | healthy | 0.0277 | 0.1174 | 0.1002 | 0.3044 | 0.0725 | 0.1871 | pass |
| http, JSON answer | refused | 0.0272 | 0.0947 | 0.0798 | 0.2173 | 0.0526 | 0.1225 | pass |
| http, SSE first byte | refused | 0.0271 | 0.0776 | 0.0922 | 0.2277 | 0.0651 | 0.1502 | pass |
| http, SSE whole stream | refused | 0.0279 | 0.0816 | 0.1039 | 0.2506 | 0.0760 | 0.1690 | pass |
| http, JSON answer | stalled | 0.0271 | 0.0949 | 0.0813 | 0.2121 | 0.0542 | 0.1173 | pass |
| http, SSE first byte | stalled | 0.0271 | 0.0813 | 0.0912 | 0.2345 | 0.0641 | 0.1533 | pass |
| http, SSE whole stream | stalled | 0.0279 | 0.0847 | 0.1047 | 0.2588 | 0.0768 | 0.1741 | pass |

"Added" is the difference of the percentiles, so it can be slightly negative where the two runs are within
noise. Absolute times will differ on other hosts; the gate is on the added p99.
