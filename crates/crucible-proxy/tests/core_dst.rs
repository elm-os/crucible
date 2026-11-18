//! Deterministic simulation of `ProxyCore` (invariant I11).
//!
//! Each seed scripts an MCP client and server exchanging JSON-RPC lines, with the server stalling, crashing or
//! exiting, splits both byte streams into random chunks, runs the analyser on a random schedule (including not
//! at all) against random queue and buffer sizes, and puts the collector in one of five states. Whatever the
//! seed:
//!
//! - every chunk comes out of [`Tap::forward`] byte for byte as it went in, at the moment it goes in;
//! - telemetry that does not fit is dropped and counted, and every finished operation is accounted for;
//! - when nothing was dropped, the operations are exactly the ones the script implies.
//!
//! The two seeded defects (`test-mutants`) must be caught by the same checks.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{ChaChaRng, Instant, Rng};
use crucible_proxy::core::Mutant;
use crucible_proxy::{Batch, CoreConfig, ExportResult, Now, Op, ProxyCore, StreamId, Tap};
use crucible_records::{
    CancelSource, CaptureSource, Direction, ErrorClass, McpStatus, ResultType, Timestamp,
};

const SEEDS: u64 = 10_000;

fn below(rng: &dyn Rng, n: u64) -> u64 {
    rng.next_u64() % n
}

fn chance(rng: &dyn Rng, percent: u64) -> bool {
    below(rng, 100) < percent
}

/// What the script expects of one operation.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Expected {
    method: String,
    id: Option<String>,
    direction: u8,
    target: String,
    status: u8,
    class: u8,
    cancel: u8,
    result_type: u8,
    progress: u32,
    is_batch: bool,
}

fn observed(op: &Op) -> Expected {
    Expected {
        method: op.method.clone(),
        id: op.jsonrpc_id.clone(),
        direction: op.direction.value(),
        target: op.target.clone(),
        status: op.classification.status.value(),
        class: op.classification.error_class.value(),
        cancel: op.cancel_source.value(),
        result_type: op.result_type.value(),
        progress: op.progress_count,
        is_batch: op.is_batch,
    }
}

/// How the scripted server answers a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Answer {
    Ok,
    ToolError,
    Code(i32),
    /// The server never answers; the request is pending when the server exits.
    Never,
    /// The client cancels and the server stays silent.
    Cancelled,
}

struct Outstanding {
    quoted_id: String,
    answer: Answer,
    progress_token: Option<String>,
    expected: Expected,
}

/// One line of the script, on the wire of one direction.
struct Line {
    direction: Direction,
    bytes: Vec<u8>,
}

struct Script {
    lines: Vec<Line>,
    expected: Vec<Expected>,
    /// Lines that are not JSON-RPC.
    malformed: u64,
    exit_code: i32,
}

const C2S: Direction = Direction::ClientToServer;
const S2C: Direction = Direction::ServerToClient;

fn expected(method: &str, id: Option<&str>, direction: Direction, target: &str) -> Expected {
    Expected {
        method: method.to_owned(),
        id: id.map(str::to_owned),
        direction: direction.value(),
        target: target.to_owned(),
        status: McpStatus::Ok.value(),
        class: ErrorClass::None.value(),
        cancel: CancelSource::None.value(),
        result_type: if id.is_some() {
            ResultType::Complete.value()
        } else {
            ResultType::None.value()
        },
        progress: 0,
        is_batch: false,
    }
}

fn settle(expected: &mut Expected, status: McpStatus, class: ErrorClass) {
    expected.status = status.value();
    expected.class = class.value();
    if status != McpStatus::Ok && status != McpStatus::ToolError {
        expected.result_type = ResultType::None.value();
    }
}

fn padding(rng: &dyn Rng) -> String {
    // Mostly small arguments, sometimes a large one, with characters that look like structure.
    let len = match below(rng, 20) {
        0 => 20_000 + below(rng, 60_000),
        1..=4 => 200 + below(rng, 2_000),
        _ => below(rng, 40),
    };
    let alphabet = [
        "a", "b", "{", "}", "[", "]", ",", ":", "\\\"", "\\\\", "\\n", "\\u00e9", " ", "é", "1",
    ];
    let mut out = String::new();
    while (out.len() as u64) < len {
        out.push_str(alphabet[below(rng, alphabet.len() as u64) as usize]);
    }
    out
}

fn script(rng: &dyn Rng) -> Script {
    let mut lines = Vec::new();
    let mut done: Vec<Expected> = Vec::new();
    let mut outstanding: Vec<Outstanding> = Vec::new();
    let mut malformed = 0;
    let mut next_id = 1u64;
    let mut line = |direction, text: String| {
        lines.push(Line {
            direction,
            bytes: format!("{text}\n").into_bytes(),
        });
    };
    let tools = ["echo", "search", "lookup_order", "a tool \\\"quoted\\\""];
    let tool_names = ["echo", "search", "lookup_order", "a tool \"quoted\""];

    if chance(rng, 70) {
        let version = if chance(rng, 50) {
            "2025-06-18"
        } else {
            "2026-07-28"
        };
        line(
            C2S,
            format!(
                r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{version}","capabilities":{{}},"clientInfo":{{"name":"dst-client","version":"1.0"}}}}}}"#
            ),
        );
        line(
            S2C,
            format!(
                r#"{{"jsonrpc":"2.0","id":0,"result":{{"protocolVersion":"{version}","capabilities":{{"tools":{{}}}},"serverInfo":{{"name":"dst-server","version":"2.0"}}}}}}"#
            ),
        );
        done.push(expected("initialize", Some("0"), C2S, ""));
        line(
            C2S,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
        );
        done.push(expected("notifications/initialized", None, C2S, ""));
    }

    let steps = 1 + below(rng, 40);
    for _ in 0..steps {
        match below(rng, 100) {
            // A client request.
            0..=44 => {
                let id = next_id;
                next_id += 1;
                let (id, quoted_id) = if chance(rng, 30) {
                    (format!("req-{id}"), format!("\"req-{id}\""))
                } else {
                    (id.to_string(), id.to_string())
                };
                let tool = below(rng, tools.len() as u64) as usize;
                let progress_token = chance(rng, 30).then(|| format!("tok-{id}"));
                let meta = progress_token.as_ref().map_or_else(String::new, |token| {
                    format!(r#","_meta":{{"progressToken":"{token}"}}"#)
                });
                let (method, target, params) = match below(rng, 4) {
                    0 => ("tools/list", "", "{}".to_owned()),
                    1 => (
                        "prompts/get",
                        tool_names[tool],
                        format!(r#"{{"name":"{}"{meta}}}"#, tools[tool]),
                    ),
                    _ => (
                        "tools/call",
                        tool_names[tool],
                        format!(
                            r#"{{"arguments":{{"text":"{}","name":"shadow","nested":{{"id":5,"method":"x"}}}},"name":"{}"{meta}}}"#,
                            padding(rng),
                            tools[tool]
                        ),
                    ),
                };
                let token_used = method != "tools/list";
                line(
                    C2S,
                    format!(
                        r#"{{"jsonrpc":"2.0","id":{quoted_id},"method":"{method}","params":{params}}}"#
                    ),
                );
                let answer = match below(rng, 100) {
                    0..=54 => Answer::Ok,
                    55..=64 => Answer::ToolError,
                    65..=84 => Answer::Code(
                        [-32601, -32602, -32002, -32603, -32700, -1][below(rng, 6) as usize],
                    ),
                    85..=92 => Answer::Never,
                    _ => Answer::Cancelled,
                };
                outstanding.push(Outstanding {
                    expected: expected(method, Some(&id), C2S, target),
                    quoted_id,
                    answer,
                    progress_token: progress_token.filter(|_| token_used),
                });
            }
            // The server answers, or reports progress on, something outstanding.
            45..=79 if !outstanding.is_empty() => {
                let index = below(rng, outstanding.len() as u64) as usize;
                if let Some(token) = outstanding[index].progress_token.clone()
                    && chance(rng, 50)
                {
                    line(
                        S2C,
                        format!(
                            r#"{{"jsonrpc":"2.0","method":"notifications/progress","params":{{"progressToken":"{token}","progress":1}}}}"#
                        ),
                    );
                    outstanding[index].expected.progress += 1;
                    done.push(expected("notifications/progress", None, S2C, ""));
                    continue;
                }
                let mut request = outstanding.remove(index);
                let id = &request.quoted_id;
                match request.answer {
                    Answer::Ok => line(
                        S2C,
                        format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"{}"}}],"isError":false}}}}"#,
                            padding(rng)
                        ),
                    ),
                    Answer::ToolError => {
                        line(
                            S2C,
                            format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"isError":true,"content":[]}}}}"#
                            ),
                        );
                        settle(
                            &mut request.expected,
                            McpStatus::ToolError,
                            ErrorClass::ToolExecution,
                        );
                    }
                    Answer::Code(code) => {
                        line(
                            S2C,
                            format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{code},"message":"{}"}}}}"#,
                                padding(rng)
                            ),
                        );
                        let class = match code {
                            -32601 | -32700 => ErrorClass::Protocol,
                            -32602 => ErrorClass::Validation,
                            -32002 => ErrorClass::NotFound,
                            _ => ErrorClass::Internal,
                        };
                        settle(&mut request.expected, McpStatus::ProtocolError, class);
                    }
                    Answer::Cancelled => {
                        line(
                            C2S,
                            format!(
                                r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{"requestId":{id},"reason":"dst"}}}}"#
                            ),
                        );
                        settle(
                            &mut request.expected,
                            McpStatus::Cancelled,
                            ErrorClass::Cancelled,
                        );
                        request.expected.cancel = CancelSource::Notification.value();
                        done.push(expected("notifications/cancelled", None, C2S, ""));
                    }
                    Answer::Never => {
                        // Put it back: it stays pending until the server exits.
                        outstanding.push(request);
                        continue;
                    }
                }
                done.push(request.expected);
            }
            // A request from the server, answered by the client: its own id space.
            80..=86 => {
                let id = next_id;
                next_id += 1;
                line(
                    S2C,
                    format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"method":"sampling/createMessage","params":{{"messages":[]}}}}"#
                    ),
                );
                line(
                    C2S,
                    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"role":"assistant"}}}}"#),
                );
                done.push(expected(
                    "sampling/createMessage",
                    Some(&id.to_string()),
                    S2C,
                    "",
                ));
            }
            // Text on the server's standard output that is no protocol message.
            87..=91 => {
                line(S2C, "listening on stdio {not json".to_owned());
                malformed += 1;
            }
            // A batch of two requests, answered one by one.
            92..=95 => {
                let (first, second) = (next_id, next_id + 1);
                next_id += 2;
                line(
                    C2S,
                    format!(
                        r#"[{{"jsonrpc":"2.0","id":{first},"method":"ping"}},{{"jsonrpc":"2.0","id":{second},"method":"tools/list"}}]"#
                    ),
                );
                line(
                    S2C,
                    format!(
                        r#"[{{"jsonrpc":"2.0","id":{first},"result":{{}}}},{{"jsonrpc":"2.0","id":{second},"result":{{"tools":[]}}}}]"#
                    ),
                );
                for (id, method) in [(first, "ping"), (second, "tools/list")] {
                    let mut op = expected(method, Some(&id.to_string()), C2S, "");
                    op.is_batch = true;
                    done.push(op);
                }
            }
            _ => {
                line(
                    C2S,
                    r#"{"jsonrpc":"2.0","method":"notifications/roots/list_changed"}"#.to_owned(),
                );
                done.push(expected("notifications/roots/list_changed", None, C2S, ""));
            }
        }
    }

    // The server exits: cleanly, or in the middle of writing an answer.
    if let Some(request) = outstanding
        .iter()
        .find(|request| request.answer == Answer::Ok)
        && chance(rng, 40)
    {
        let cut = format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"content":[{{"type":"te"#,
            request.quoted_id
        );
        lines.push(Line {
            direction: S2C,
            bytes: cut.into_bytes(),
        });
    }
    for mut request in outstanding {
        settle(
            &mut request.expected,
            McpStatus::TransportError,
            ErrorClass::Transport,
        );
        done.push(request.expected);
    }
    Script {
        lines,
        expected: done,
        malformed,
        exit_code: [0, 0, 1, 2, 137][below(rng, 5) as usize],
    }
}

/// The collector's behaviour for a run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Collector {
    Healthy,
    /// Connections are refused: every export fails at once.
    Refused,
    /// The collector accepts the connection and never answers.
    Stalled,
    Flaky,
    /// The collector refuses the key.
    Rejecting,
}

struct Run {
    core: ProxyCore,
    tap: Tap,
    rng: Arc<dyn Rng>,
    collector: Collector,
    mono: u64,
    in_flight: Option<Batch>,
    delivered: Vec<Op>,
    lost: u64,
    buffer_ops: usize,
}

impl Run {
    fn now(&mut self) -> Now {
        self.mono += below(self.rng.as_ref(), 3_000_000);
        Now {
            mono: Instant::from_nanos(self.mono),
            wall: Timestamp::from_micros(1_800_000_000_000_000 + (self.mono / 1_000) as i64),
        }
    }

    /// One turn of the exporter: maybe finish the export in flight, maybe start one.
    fn exporter(&mut self, flush: bool) {
        let now = self.now().mono;
        if let Some(batch) = self.in_flight.take() {
            let result = match self.collector {
                Collector::Healthy => ExportResult::Delivered,
                Collector::Refused => ExportResult::Retry,
                Collector::Rejecting => ExportResult::Rejected,
                Collector::Flaky if chance(self.rng.as_ref(), 50) => ExportResult::Retry,
                Collector::Flaky => ExportResult::Delivered,
                Collector::Stalled => {
                    // Still waiting: the batch stays in flight.
                    self.in_flight = Some(batch);
                    return;
                }
            };
            match result {
                ExportResult::Delivered => self.delivered.extend(batch.ops.iter().cloned()),
                ExportResult::Rejected => self.lost += batch.ops.len() as u64,
                ExportResult::Retry => {}
            }
            self.core.export_done(batch, result, now);
        }
        if let Some(batch) = self.core.next_export(now, flush) {
            assert!(!batch.ops.is_empty());
            self.in_flight = Some(batch);
        }
    }

    fn check_bounds(&self) -> Result<(), String> {
        if self.core.buffered() > self.buffer_ops {
            return Err(format!(
                "the export buffer holds {} operations, over its bound of {}",
                self.core.buffered(),
                self.buffer_ops
            ));
        }
        Ok(())
    }
}

/// What a run that kept every invariant looked like.
struct Summary {
    /// Nothing was dropped, so the operations were compared with the script one for one.
    complete: bool,
    /// Operations delivered.
    ops: usize,
    /// Chunk copies dropped.
    dropped_chunks: u64,
    /// Finished operations dropped.
    dropped_ops: u64,
}

/// Runs one seed. `Err` describes the first violated invariant.
fn run(seed: u64, mutant: Option<Mutant>) -> Result<Summary, String> {
    let rng: Arc<dyn Rng> = Arc::new(ChaChaRng::from_u64(seed));
    let script = script(rng.as_ref());

    // Swarm: each run draws its own queue sizes, analyser schedule and collector state.
    let mut config = CoreConfig::new(CaptureSource::ProxyStdio);
    config.mutant = mutant;
    if chance(rng.as_ref(), 40) {
        config.queue_bytes = 64 + below(rng.as_ref(), 4_096) as usize;
    }
    if chance(rng.as_ref(), 30) {
        config.queue_items = 1 + below(rng.as_ref(), 8) as usize;
    }
    if chance(rng.as_ref(), 40) {
        config.buffer_ops = 1 + below(rng.as_ref(), 12) as usize;
    }
    if chance(rng.as_ref(), 20) {
        config.buffer_bytes = 400 + below(rng.as_ref(), 4_000) as usize;
    }
    if chance(rng.as_ref(), 30) {
        config.batch_ops = 1 + below(rng.as_ref(), 4) as usize;
    }
    if chance(rng.as_ref(), 20) {
        config.max_pending = 1 + below(rng.as_ref(), 4) as usize;
    }
    // How often the analyser gets a turn after a chunk, in percent. Zero is a stalled analyser.
    let analyser = [100, 100, 60, 20, 0][below(rng.as_ref(), 5) as usize];
    let collector = [
        Collector::Healthy,
        Collector::Refused,
        Collector::Stalled,
        Collector::Flaky,
        Collector::Rejecting,
    ][below(rng.as_ref(), 5) as usize];
    let max_pending = config.max_pending;
    let buffer_ops = config.buffer_ops;
    let core = ProxyCore::new(config, rng.fork("ids"));
    let mut run = Run {
        tap: core.tap(),
        core,
        rng: Arc::clone(&rng),
        collector,
        mono: 0,
        in_flight: None,
        delivered: Vec::new(),
        lost: 0,
        buffer_ops,
    };

    let mut sent: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    let mut forwarded: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
    let mut carry: Vec<u8> = Vec::new();
    for (index, line) in script.lines.iter().enumerate() {
        let mut bytes = std::mem::take(&mut carry);
        bytes.extend_from_slice(&line.bytes);
        let mut rest = bytes.as_slice();
        // The tail of a line may travel with the head of the next one, when that goes the same way.
        let may_carry = script
            .lines
            .get(index + 1)
            .is_some_and(|next| next.direction == line.direction);
        while !rest.is_empty() {
            let size = match below(rng.as_ref(), 10) {
                0 => 1,
                1..=3 => 1 + below(rng.as_ref(), 16) as usize,
                4..=7 => 1 + below(rng.as_ref(), 512) as usize,
                _ => rest.len(),
            }
            .min(rest.len());
            if may_carry && size == rest.len() && chance(rng.as_ref(), 30) {
                carry = rest.to_vec();
                break;
            }
            let (chunk, tail) = rest.split_at(size);
            rest = tail;
            let now = run.now();
            let out = run.tap.forward(
                StreamId::STDIO,
                line.direction,
                Bytes::copy_from_slice(chunk),
                now,
            );
            // I11: the bytes to write are the bytes read, now, whatever the analyser and collector do.
            if out.as_ref() != chunk {
                return Err(format!(
                    "seed {seed}: forward returned {} bytes that differ from the {} bytes read",
                    out.len(),
                    chunk.len()
                ));
            }
            sent.entry(line.direction.value())
                .or_default()
                .extend_from_slice(chunk);
            forwarded
                .entry(line.direction.value())
                .or_default()
                .extend_from_slice(&out);
            if chance(rng.as_ref(), analyser) {
                run.core.drain();
            } else if analyser > 0 && chance(rng.as_ref(), 30) {
                run.core.step();
            }
            if chance(rng.as_ref(), 25) {
                run.exporter(false);
            }
            run.check_bounds()?;
        }
    }
    if sent != forwarded {
        return Err(format!(
            "seed {seed}: the forwarded streams differ from the sent ones"
        ));
    }

    // The server exits.
    let _exit_code = script.exit_code;
    let now = run.now();
    run.tap.session_end(now);
    run.core.drain();
    run.check_bounds()?;
    if run.core.pending() != 0 {
        return Err(format!(
            "seed {seed}: {} operations still pending after the server exited",
            run.core.pending()
        ));
    }
    let stats = run.core.stats();
    let total: u64 = sent.values().map(|bytes| bytes.len() as u64).sum();
    if stats.forwarded_bytes != total {
        return Err(format!(
            "seed {seed}: forwarded {} of {total} bytes",
            stats.forwarded_bytes
        ));
    }

    // Accounting while the collector is still in its state: nothing is lost without being counted.
    let in_flight = run.in_flight.as_ref().map_or(0, |batch| batch.ops.len()) as u64;
    let accounted = stats.exported_ops + stats.dropped_ops + run.core.buffered() as u64 + in_flight;
    if stats.ops != accounted {
        return Err(format!(
            "seed {seed}: {} operations finished but {accounted} are accounted for ({stats:?})",
            stats.ops
        ));
    }

    // The collector comes back: everything still buffered is delivered.
    run.collector = Collector::Healthy;
    run.mono += 120_000_000_000;
    for _ in 0..100_000 {
        run.exporter(true);
        if run.in_flight.is_none() && run.core.buffered() == 0 {
            break;
        }
    }
    let stats = run.core.stats();
    if run.core.buffered() != 0 || stats.ops != stats.exported_ops + stats.dropped_ops {
        return Err(format!("seed {seed}: the buffer did not drain: {stats:?}"));
    }
    if stats.exported_ops != run.delivered.len() as u64 {
        return Err(format!(
            "seed {seed}: exported count differs from the delivered operations"
        ));
    }

    let mut got: Vec<Expected> = run.delivered.iter().map(observed).collect();
    got.sort();
    let mut want = script.expected.clone();
    want.sort();
    let complete = stats.dropped_chunks == 0
        && stats.dropped_ops == 0
        && stats.evicted_pending == 0
        && script.expected.len() <= max_pending;
    if complete {
        if got != want {
            let missing: Vec<_> = want.iter().filter(|op| !got.contains(op)).collect();
            let extra: Vec<_> = got.iter().filter(|op| !want.contains(op)).collect();
            return Err(format!(
                "seed {seed}: operations differ from the script; missing {missing:?}; unexpected {extra:?}"
            ));
        }
        if stats.malformed < script.malformed {
            return Err(format!(
                "seed {seed}: {} malformed lines counted, the script has {}",
                stats.malformed, script.malformed
            ));
        }
    } else if got.len() > want.len() {
        return Err(format!(
            "seed {seed}: {} operations from a script of {}",
            got.len(),
            want.len()
        ));
    }
    for op in &run.delivered {
        if op.trace_id.is_zero() || op.span_id.is_zero() {
            return Err(format!("seed {seed}: an operation has a zero id"));
        }
    }
    Ok(Summary {
        complete,
        ops: got.len(),
        dropped_chunks: stats.dropped_chunks,
        dropped_ops: stats.dropped_ops,
    })
}

#[test]
fn passthrough_is_byte_identical_and_drops_are_counted_for_every_seed() {
    let (mut complete, mut compared, mut with_chunk_drops, mut with_op_drops) =
        (0u64, 0usize, 0u64, 0u64);
    for seed in 0..SEEDS {
        match run(seed, None) {
            Ok(summary) => {
                if summary.complete {
                    complete += 1;
                    compared += summary.ops;
                }
                with_chunk_drops += u64::from(summary.dropped_chunks > 0);
                with_op_drops += u64::from(summary.dropped_ops > 0);
            }
            Err(violation) => panic!("{violation}"),
        }
    }
    // The run must have exercised both halves: exact comparison with the script, and loss under pressure.
    assert!(
        complete > SEEDS / 5,
        "only {complete} runs were compared with their script"
    );
    assert!(
        compared > 20_000,
        "only {compared} operations were compared"
    );
    assert!(
        with_chunk_drops > SEEDS / 10,
        "only {with_chunk_drops} runs dropped chunk copies"
    );
    assert!(
        with_op_drops > SEEDS / 10,
        "only {with_op_drops} runs dropped operations"
    );
    println!(
        "{SEEDS} seeds: {complete} compared one for one ({compared} operations), {with_chunk_drops} with \
         dropped chunk copies, {with_op_drops} with dropped operations"
    );
}

#[test]
fn the_same_seed_gives_the_same_run() {
    // The run is a function of the seed: ids come from the seeded stream, time from the script.
    let ops = |seed| {
        let rng: Arc<dyn Rng> = Arc::new(ChaChaRng::from_u64(seed));
        let script = script(rng.as_ref());
        let mut core = ProxyCore::new(CoreConfig::new(CaptureSource::ProxyStdio), rng.fork("ids"));
        let tap = core.tap();
        for (index, line) in script.lines.iter().enumerate() {
            let now = Now {
                mono: Instant::from_nanos(index as u64 * 1_000),
                wall: Timestamp::from_micros(index as i64),
            };
            tap.forward(
                StreamId::STDIO,
                line.direction,
                Bytes::from(line.bytes.clone()),
                now,
            );
        }
        core.drain();
        let far = Instant::from_nanos(u64::MAX / 2);
        let mut all = Vec::new();
        while let Some(batch) = core.next_export(far, true) {
            all.extend(batch.ops.iter().cloned());
            core.export_done(batch, ExportResult::Delivered, far);
        }
        all
    };
    for seed in [1, 7, 4242] {
        let first = ops(seed);
        assert!(!first.is_empty());
        assert_eq!(first, ops(seed));
    }
}

#[test]
fn the_byte_changing_mutant_is_caught() {
    let caught = (0..200)
        .filter(|seed| run(*seed, Some(Mutant::FlipByte)).is_err())
        .count();
    assert!(
        caught > 150,
        "only {caught} of 200 seeds caught the changed byte"
    );
}

#[test]
fn the_blocking_mutant_is_caught() {
    // Only runs whose analysis queue fills up can tell; the swarm makes that common.
    let caught = (0..200)
        .filter(|seed| run(*seed, Some(Mutant::BlockOnFullQueue)).is_err())
        .count();
    assert!(
        caught > 20,
        "only {caught} of 200 seeds caught the blocking send"
    );
    // And a seed that the mutants fail passes without them.
    let failing = (0..200)
        .find(|seed| run(*seed, Some(Mutant::BlockOnFullQueue)).is_err())
        .unwrap();
    run(failing, None).unwrap();
}

#[test]
fn a_stalled_analyser_and_a_dead_collector_cost_only_telemetry() {
    // A directed case of the same invariant with a line over 1 MiB and nothing ever analysed or exported.
    let rng: Arc<dyn Rng> = Arc::new(ChaChaRng::from_u64(9));
    let mut config = CoreConfig::new(CaptureSource::ProxyStdio);
    config.queue_bytes = 64 * 1024;
    let mut core = ProxyCore::new(config, rng);
    let tap = core.tap();
    let big = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{{\"name\":\"echo\",\"arguments\":{{\"text\":\"{}\"}}}}}}\n",
        "x".repeat(3 << 20)
    );
    let mut out = Vec::new();
    for chunk in big.as_bytes().chunks(8 * 1024) {
        out.extend_from_slice(&tap.forward(
            StreamId::STDIO,
            C2S,
            Bytes::copy_from_slice(chunk),
            Now::default(),
        ));
    }
    assert_eq!(out, big.as_bytes());
    let stats = core.stats();
    assert!(stats.dropped_chunks > 0, "{stats:?}");
    assert_eq!(stats.forwarded_bytes, big.len() as u64);
    // Every byte is either queued or counted as dropped.
    assert_eq!(stats.dropped_chunk_bytes + 64 * 1024, big.len() as u64);
    // The analyser wakes up. It cannot know where the damaged line ended, so the first line after the gap is
    // spent on finding its place; the one after that is reported. The other direction lost nothing.
    core.drain();
    let send = |direction, line: &'static str| {
        tap.forward(
            StreamId::STDIO,
            direction,
            Bytes::from_static(line.as_bytes()),
            Now::default(),
        );
    };
    send(C2S, "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"lost\"}\n");
    send(C2S, "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n");
    send(S2C, "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n");
    core.drain();
    let batch = core
        .next_export(Instant::from_nanos(u64::MAX / 2), true)
        .unwrap();
    assert_eq!(batch.ops.len(), 1);
    assert_eq!(batch.ops[0].method, "ping");
    assert_eq!(batch.ops[0].duration, Duration::ZERO);
}
