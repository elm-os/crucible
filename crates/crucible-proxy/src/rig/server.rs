//! `crucible-proxy-rig server`: a scripted MCP-shaped server for the proxy's process tests and benchmark. It uses
//! the standard library only, so it builds and behaves the same on every platform.
//!
//! It reads JSON-RPC lines from standard input and answers on standard output. The method of a request picks
//! the behaviour:
//!
//! | Method | Behaviour |
//! |---|---|
//! | `exit` | Exits with the code in `"code":N` |
//! | `crash` | Writes half of an answer, then exits with code 3 |
//! | `big` | Answers with a result of `"bytes":N` bytes of text |
//! | `sleep` | Waits `"ms":N` milliseconds, then answers |
//! | `port` | Answers with the port of a listener this process keeps open (`--listen`) |
//! | `fail`, `unknown` | Answers with `isError: true`, or with the JSON-RPC error -32601 |
//! | `leak` | Answers with the JSON-RPC error -32603, whose message holds an email address and a secret key |
//! | a call of the tool `needs_input` | Without `requestState`: answers `input_required` with the request state of `REQUEST_STATE`. With it: answers as below |
//! | anything else | Answers with the length and the FNV-1a hash of the request line; a message without an id gets no answer |
//!
//! Flags: `--eof-exit N` (exit code at end of input, default 0), `--stderr` (write `ready` to standard error
//! first), `--banner` (write a line that is not JSON to standard output first), `--listen` (keep a TCP listener
//! open), `--spawn-listener` (start a copy of this program with `--listen --hold`, answer its port to `port`),
//! `--hold` (ignore the end of input and stay alive).

use std::io::{BufRead as _, Read as _, Write as _};
use std::net::TcpListener;
use std::process::{Command, Stdio, exit};
use std::time::Duration;

/// The `requestState` that `needs_input` hands out.
const REQUEST_STATE: &str = "rig-request-state-7f3a";

fn number_after(line: &str, key: &str) -> Option<i64> {
    let rest = &line[line.find(key)? + key.len()..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// FNV-1a over the request line: the answer proves which bytes arrived.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The raw text of the top-level `id` member, which the tests put first.
fn id_of(line: &str) -> Option<&str> {
    let rest = &line[line.find("\"id\":")? + 5..];
    let end = match rest.strip_prefix('"') {
        Some(text) => text.find('"')? + 2,
        None => rest.find([',', '}'])?,
    };
    Some(&rest[..end])
}

/// Runs the scripted server with these flags. Never returns.
pub fn run(args: &[String]) -> ! {
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let eof_exit = args
        .iter()
        .position(|arg| arg == "--eof-exit")
        .and_then(|at| args.get(at + 1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    let listener = has("--listen").then(|| TcpListener::bind("127.0.0.1:0").expect("bind"));
    let mut port = listener
        .as_ref()
        .map(|listener| listener.local_addr().expect("address").port());
    let mut stdout = std::io::stdout().lock();
    if has("--hold") {
        // Announce the port to the parent, then outlive the end of input.
        writeln!(stdout, "{}", port.unwrap_or(0)).expect("write");
        stdout.flush().expect("flush");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    let mut _held_child = None;
    if has("--spawn-listener") {
        let mut child = Command::new(std::env::current_exe().expect("exe"))
            .args(["server", "--listen", "--hold"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let mut line = String::new();
        let mut reader = std::io::BufReader::new(child.stdout.take().expect("pipe"));
        reader.read_line(&mut line).expect("read");
        port = line.trim().parse().ok();
        _held_child = Some((child, reader));
    }
    if has("--stderr") {
        eprintln!("ready");
    }
    if has("--banner") {
        writeln!(stdout, "scripted server listening on stdio").expect("write");
        stdout.flush().expect("flush");
    }

    let mut stdin = std::io::stdin().lock();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let text = line.trim_end();
        let method = |name: &str| text.contains(&format!("\"method\":\"{name}\""));
        if method("exit") {
            exit(number_after(text, "\"code\":").unwrap_or(0) as i32);
        }
        let Some(id) = id_of(text) else {
            continue;
        };
        if method("crash") {
            write!(
                stdout,
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"content\":[{{\"ty"
            )
            .expect("write");
            stdout.flush().expect("flush");
            exit(3);
        }
        if method("sleep") {
            let ms = number_after(text, "\"ms\":").unwrap_or(0);
            std::thread::sleep(Duration::from_millis(ms as u64));
        }
        let result = if method("big") {
            let bytes = number_after(text, "\"bytes\":").unwrap_or(0) as usize;
            format!(
                "{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}",
                "y".repeat(bytes)
            )
        } else if method("port") {
            format!("{{\"port\":{}}}", port.unwrap_or(0))
        } else if method("fail") {
            "{\"content\":[],\"isError\":true}".to_owned()
        } else if text.contains("\"name\":\"needs_input\"") && !text.contains("\"requestState\"") {
            format!(
                "{{\"resultType\":\"input_required\",\"requestState\":\"{REQUEST_STATE}\",\"inputRequests\":{{}}}}"
            )
        } else if method("leak") {
            writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32603,\"message\":\"lookup failed for ada.lovelace@example.com with key csk_7Hq2mXr9TzP4vLw8NcB1dKf6YsA3eJu5RgQ0hVt2iWo\"}}}}").expect("write");
            stdout.flush().expect("flush");
            continue;
        } else if method("unknown") {
            writeln!(stdout, "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":-32601,\"message\":\"no such method\"}}}}").expect("write");
            stdout.flush().expect("flush");
            continue;
        } else {
            format!(
                "{{\"len\":{},\"fnv\":\"{:016x}\"}}",
                text.len(),
                fnv(text.as_bytes())
            )
        };
        writeln!(
            stdout,
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}"
        )
        .expect("write");
        stdout.flush().expect("flush");
    }
    // Keep reading nothing: the input is closed.
    let _ = stdin.read(&mut [0u8; 1]);
    if has("--stderr") {
        eprintln!("bye");
    }
    exit(eof_exit);
}
