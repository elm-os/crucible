//! The stdio pumps against a scripted MCP server task behind the `ChildProcess` trait (contract 2.8): seeded
//! chunk splitting on both sides, stalls, pipes of every size, crashes in the middle of a line, and exit codes.
//!
//! Whatever the seed, the client receives exactly the bytes the server wrote, the server reads a prefix of the
//! bytes the client wrote (all of them when it reads to the end), and `run` reports how the server ended.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::prod_env;
use crucible_env::{
    ChaChaRng, Child, ChildControl, ChildError, ChildProcess, ChildSignal, ChildSpec, Env,
    ExitStatus, JoinHandle, Rng,
};
use crucible_proxy::live::LiveTap;
use crucible_proxy::stdio::{ClientIo, run};
use futures::StreamExt as _;
use futures::channel::mpsc;
use tokio::io::{
    AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream,
};

fn below(rng: &dyn Rng, n: u64) -> u64 {
    rng.next_u64() % n
}

/// How the scripted server ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum End {
    /// Reads to the end of its input, then exits with this code.
    AtEndOfInput(i32),
    /// Exits with this code after answering this many requests, with input still open.
    After(usize, i32),
    /// Writes half an answer to this request, then exits with this code.
    CrashIn(usize, i32),
}

#[derive(Clone, Debug)]
struct Plan {
    seed: u64,
    end: End,
    /// The size of each of the server's pipes.
    pipe: usize,
    banner: bool,
    /// The least size of an answer's payload.
    min_answer: usize,
}

#[derive(Debug, Default)]
struct Observed {
    /// Every byte the server read from its standard input.
    read: Vec<u8>,
    /// Every byte the server wrote to its standard output.
    wrote: Vec<u8>,
}

/// Writes `bytes` in seeded chunks with seeded stalls, and records them.
async fn write_split(
    env: &Env,
    rng: &dyn Rng,
    out: &mut DuplexStream,
    observed: &Mutex<Observed>,
    bytes: &[u8],
) -> bool {
    let mut rest = bytes;
    while !rest.is_empty() {
        let size = match below(rng, 6) {
            0 => 1,
            1 | 2 => 1 + below(rng, 64) as usize,
            3 | 4 => 1 + below(rng, 8_192) as usize,
            _ => rest.len(),
        }
        .min(rest.len());
        let (chunk, tail) = rest.split_at(size);
        rest = tail;
        // Recorded before the write: when a kill cuts the write off, a part of the chunk may still arrive.
        observed.lock().unwrap().wrote.extend_from_slice(chunk);
        if out.write_all(chunk).await.is_err() {
            return false;
        }
        if below(rng, 8) == 0 {
            env.clock
                .sleep(Duration::from_micros(below(rng, 3_000)))
                .await;
        }
    }
    true
}

async fn server(
    env: Env,
    plan: Plan,
    stdin: DuplexStream,
    mut stdout: DuplexStream,
    observed: Arc<Mutex<Observed>>,
) -> ExitStatus {
    let rng = ChaChaRng::from_u64(plan.seed).fork("server");
    let mut stdin = BufReader::new(stdin);
    if plan.banner
        && !write_split(
            &env,
            rng.as_ref(),
            &mut stdout,
            &observed,
            b"scripted server ready\n",
        )
        .await
    {
        return ExitStatus::Code(141);
    }
    let mut answered = 0;
    loop {
        match plan.end {
            End::After(count, code) if answered == count => return ExitStatus::Code(code),
            _ => {}
        }
        let mut line = Vec::new();
        if stdin.read_until(b'\n', &mut line).await.unwrap_or(0) == 0 {
            return match plan.end {
                End::AtEndOfInput(code) => ExitStatus::Code(code),
                // The input ended before the scripted end.
                End::After(_, code) | End::CrashIn(_, code) => ExitStatus::Code(code),
            };
        }
        observed.lock().unwrap().read.extend_from_slice(&line);
        let size = match below(rng.as_ref(), 12) {
            0 => 100_000 + below(rng.as_ref(), 300_000),
            1 | 2 => 2_000 + below(rng.as_ref(), 20_000),
            _ => below(rng.as_ref(), 200),
        } as usize;
        let size = size.max(plan.min_answer);
        let answer = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{answered},\"result\":{{\"data\":\"{}\"}}}}\n",
            "d".repeat(size)
        );
        if let End::CrashIn(at, code) = plan.end
            && at == answered
        {
            let half = &answer.as_bytes()[..answer.len() / 2];
            write_split(&env, rng.as_ref(), &mut stdout, &observed, half).await;
            return ExitStatus::Code(code);
        }
        if below(rng.as_ref(), 10) == 0 {
            // A stall before answering.
            env.clock
                .sleep(Duration::from_millis(below(rng.as_ref(), 20)))
                .await;
        }
        if !write_split(
            &env,
            rng.as_ref(),
            &mut stdout,
            &observed,
            answer.as_bytes(),
        )
        .await
        {
            return ExitStatus::Code(141);
        }
        answered += 1;
    }
}

#[derive(Debug)]
struct ScriptedControl {
    task: JoinHandle<ExitStatus>,
    ended: Option<ExitStatus>,
    signalled: Option<i32>,
}

#[async_trait]
impl ChildControl for ScriptedControl {
    async fn wait(&mut self) -> std::io::Result<ExitStatus> {
        if let Some(status) = self.ended {
            return Ok(status);
        }
        let status = match self.task.join().await {
            Ok(status) => status,
            Err(_cancelled) => ExitStatus::Signal(self.signalled.unwrap_or(9)),
        };
        self.ended = Some(status);
        Ok(status)
    }

    fn kill(&mut self) -> std::io::Result<()> {
        self.signalled.get_or_insert(9);
        self.task.abort();
        Ok(())
    }

    fn signal(&mut self, signal: ChildSignal) -> std::io::Result<()> {
        self.signalled.get_or_insert(signal.number());
        self.task.abort();
        Ok(())
    }
}

/// A `ChildProcess` whose child is the scripted server task.
#[derive(Debug)]
struct Scripted {
    env: Env,
    plan: Plan,
    observed: Arc<Mutex<Observed>>,
}

#[async_trait]
impl ChildProcess for Scripted {
    async fn spawn(&self, _spec: ChildSpec) -> Result<Child, ChildError> {
        let (stdin_writer, stdin_reader) = tokio::io::duplex(self.plan.pipe);
        let (stdout_writer, stdout_reader) = tokio::io::duplex(self.plan.pipe);
        let task = self.env.spawn(
            "scripted-server",
            server(
                self.env.clone(),
                self.plan.clone(),
                stdin_reader,
                stdout_writer,
                Arc::clone(&self.observed),
            ),
        );
        Ok(Child::new(
            Some(Box::new(stdin_writer)),
            Some(Box::new(stdout_reader)),
            None,
            Box::new(ScriptedControl {
                task,
                ended: None,
                signalled: None,
            }),
        ))
    }
}

struct Outcome {
    status: ExitStatus,
    sent: Vec<u8>,
    received: Vec<u8>,
    observed: Observed,
}

/// Runs one session: the client writes `requests` lines in seeded chunks and reads everything, starting after
/// `read_delay`; `signal_after` sends a signal once that many answers have arrived.
async fn session(
    env: &Env,
    plan: Plan,
    requests: usize,
    read_delay: Duration,
    signal_after: Option<usize>,
) -> Outcome {
    let rng = ChaChaRng::from_u64(plan.seed).fork("client");
    let observed = Arc::new(Mutex::new(Observed::default()));
    let children = Scripted {
        env: env.clone(),
        plan: plan.clone(),
        observed: Arc::clone(&observed),
    };
    let client_pipe = 64 + below(rng.as_ref(), 64 * 1024) as usize;
    let (mut to_proxy, proxy_in) = tokio::io::duplex(client_pipe);
    let (proxy_out, mut from_proxy) = tokio::io::duplex(client_pipe);
    let (proxy_err, _stderr) = tokio::io::duplex(1024);
    let (signals, signal_stream) = mpsc::unbounded();

    let mut sent = Vec::new();
    for id in 0..requests {
        let padding = "p".repeat(below(rng.as_ref(), 3_000) as usize);
        sent.extend_from_slice(
            format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"tools/call\",\"params\":{{\"name\":\"echo\",\"x\":\"{padding}\"}}}}\n")
                .as_bytes(),
        );
    }
    let writer = env.spawn("scripted-client", {
        let (env, sent, rng) = (env.clone(), sent.clone(), Arc::clone(&rng));
        async move {
            let mut rest = sent.as_slice();
            while !rest.is_empty() {
                let size = (1 + below(rng.as_ref(), 4_096) as usize).min(rest.len());
                let (chunk, tail) = rest.split_at(size);
                rest = tail;
                if to_proxy.write_all(chunk).await.is_err() {
                    return;
                }
                if below(rng.as_ref(), 16) == 0 {
                    env.clock
                        .sleep(Duration::from_micros(below(rng.as_ref(), 2_000)))
                        .await;
                }
            }
            // The end of the client's input.
            drop(to_proxy);
        }
    });
    let reader = env.spawn("scripted-reader", {
        let env = env.clone();
        async move {
            env.clock.sleep(read_delay).await;
            let mut received = Vec::new();
            let mut buffer = vec![0u8; 16 * 1024];
            let mut signalled = false;
            loop {
                let read = from_proxy.read(&mut buffer).await.unwrap();
                if read == 0 {
                    return received;
                }
                received.extend_from_slice(&buffer[..read]);
                let answers = received.iter().filter(|byte| **byte == b'\n').count();
                if let Some(after) = signal_after
                    && answers >= after
                    && !signalled
                {
                    signalled = true;
                    signals.unbounded_send(ChildSignal::Terminate).unwrap();
                }
            }
        }
    });
    let client = ClientIo {
        input: Box::new(proxy_in),
        output: Box::new(proxy_out),
        error: Box::new(proxy_err),
    };
    let status = run(
        env,
        &children,
        ChildSpec::default(),
        client,
        signal_stream.boxed(),
        LiveTap::disabled(env),
    )
    .await
    .unwrap();
    let mut reader = reader;
    let received = reader.join().await.unwrap();
    writer.abort();
    let observed = std::mem::take(&mut *observed.lock().unwrap());
    Outcome {
        status,
        sent,
        received,
        observed,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_seed_passes_the_servers_bytes_and_exit_code_through() {
    let env = prod_env();
    let mut ends = [0usize; 3];
    for seed in 0..150 {
        let rng = ChaChaRng::from_u64(seed);
        let requests = 1 + below(&rng, 12) as usize;
        let code = [0, 0, 1, 2, 7, 137][below(&rng, 6) as usize];
        let at = below(&rng, requests as u64) as usize;
        let end = match below(&rng, 3) {
            0 => End::AtEndOfInput(code),
            1 => End::After(at, code),
            _ => End::CrashIn(at, code),
        };
        ends[match end {
            End::AtEndOfInput(_) => 0,
            End::After(..) => 1,
            End::CrashIn(..) => 2,
        }] += 1;
        let plan = Plan {
            seed,
            end,
            pipe: 16 + below(&rng, 128 * 1024) as usize,
            banner: below(&rng, 2) == 0,
            min_answer: 0,
        };
        let outcome = session(&env, plan.clone(), requests, Duration::ZERO, None).await;
        assert_eq!(
            outcome.status,
            ExitStatus::Code(code),
            "seed {seed}: {plan:?}"
        );
        assert!(
            outcome.received == outcome.observed.wrote,
            "seed {seed}: the client received {} bytes, the server wrote {} ({plan:?})",
            outcome.received.len(),
            outcome.observed.wrote.len()
        );
        assert!(
            outcome.sent.starts_with(&outcome.observed.read),
            "seed {seed}: the server read bytes the client did not send ({plan:?})"
        );
        if matches!(end, End::AtEndOfInput(_)) {
            assert_eq!(
                outcome.observed.read.len(),
                outcome.sent.len(),
                "seed {seed}"
            );
        }
        if let End::CrashIn(..) = end {
            assert!(
                !outcome.received.ends_with(b"\n"),
                "seed {seed}: the cut line stays cut"
            );
        }
    }
    assert!(ends.iter().all(|count| *count > 20), "{ends:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn output_written_before_the_exit_reaches_a_client_that_reads_late() {
    // The server answers and exits at once; the client starts reading three seconds later, through a pipe far
    // smaller than the answers. Nothing the server wrote may be lost while the proxy waits for the client.
    let env = prod_env();
    let plan = Plan {
        seed: 3,
        end: End::After(6, 5),
        // The server's pipe holds everything, so the server exits while the proxy still holds its output.
        pipe: 4 << 20,
        banner: false,
        min_answer: 60_000,
    };
    let started = env.clock.now();
    let outcome = session(&env, plan, 6, Duration::from_secs(3), None).await;
    assert!(env.clock.now().saturating_duration_since(started) >= Duration::from_secs(3));
    assert_eq!(outcome.status, ExitStatus::Code(5));
    assert!(
        outcome.observed.wrote.len() > 200_000,
        "the answers ({} bytes) must exceed the client's pipe",
        outcome.observed.wrote.len()
    );
    assert!(outcome.received == outcome.observed.wrote);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forwarded_signal_ends_the_scripted_server_and_is_reported() {
    let env = prod_env();
    let plan = Plan {
        seed: 11,
        end: End::AtEndOfInput(0),
        pipe: 4_096,
        banner: false,
        min_answer: 0,
    };
    // The client never closes its input in time: the signal is what ends the server.
    let outcome = session(&env, plan, 400, Duration::ZERO, Some(3)).await;
    assert_eq!(outcome.status, ExitStatus::Signal(15));
    // What arrived is a prefix of what the server wrote; the write the signal cut off may be partly missing.
    assert!(outcome.observed.wrote.starts_with(&outcome.received));
    assert!(outcome.received.len() >= 3);
}
