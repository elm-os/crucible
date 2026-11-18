//! `crucible proxy` as the real binary on real standard streams (invariant I11, process tests): exit codes,
//! the signal a child died of, the parent-death signal, a clean standard output, and the HTTP subcommand.
//!
//! The servers are shell one-liners, so these tests need a Unix host. The Windows behaviour of the same code
//! (Job Objects, `PATHEXT`) is covered by the tests of `crucible-proxy`, which the `proxy` workflow runs on
//! Windows.
#![cfg(unix)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CRUCIBLE: &str = env!("CARGO_BIN_EXE_crucible");

fn proxy_stdio(script: &str, dir: &std::path::Path) -> Child {
    Command::new(CRUCIBLE)
        .args(["proxy", "stdio", "--", "sh", "-c", script])
        .current_dir(dir)
        .env_remove("CRUCIBLE_ENDPOINT")
        .env_remove("CRUCIBLE_SECRET_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn kill(signal: &str, pid: u32) {
    let status = Command::new("kill")
        .args([signal, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}

fn until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting until {what}");
}

#[test]
fn the_exit_code_passes_through_and_standard_output_carries_only_the_servers_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let mut proxy = proxy_stdio("cat; echo done >&2; exit 7", dir.path());
    let mut stdin = proxy.stdin.take().unwrap();
    let message = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    stdin.write_all(message).unwrap();
    drop(stdin);
    let output = proxy.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, message);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The proxy's own words (telemetry is off here) and the server's standard error share standard error.
    assert!(stderr.contains("telemetry is off"), "{stderr}");
    assert!(stderr.contains("done"), "{stderr}");
    // No server subsystem ran: nothing was created in the working directory (no data directory, no WAL).
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn a_signal_is_forwarded_and_the_proxy_dies_of_the_same_signal() {
    let dir = tempfile::tempdir().unwrap();
    let mut proxy = proxy_stdio("echo ready; exec sleep 300", dir.path());
    let mut stdout = BufReader::new(proxy.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    kill("-TERM", proxy.id());
    let status = proxy.wait().unwrap();
    // The child died of SIGTERM, so the proxy does too: a supervisor sees the same death.
    assert_eq!(status.signal(), Some(15), "{status:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn the_server_ends_when_the_proxy_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let mut proxy = proxy_stdio("echo $$; exec sleep 300", dir.path());
    let mut stdout = BufReader::new(proxy.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let server = format!("/proc/{}", line.trim());
    let alive = || {
        // A zombie still has its directory; its state tells.
        std::fs::read_to_string(format!("{server}/stat")).is_ok_and(|stat| !stat.contains(") Z "))
    };
    assert!(alive(), "the server runs as {server}");
    // SIGKILL cannot be handled: only the parent-death signal can end the server now.
    kill("-KILL", proxy.id());
    assert_eq!(proxy.wait().unwrap().signal(), Some(9));
    until("the server has ended", || !alive());
}

#[test]
fn an_unknown_command_is_refused_with_a_hint_and_nothing_on_standard_output() {
    let output = Command::new(CRUCIBLE)
        .args(["proxy", "stdio", "--", "crucible-no-such-server"])
        .env_remove("CRUCIBLE_ENDPOINT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("command not found: crucible-no-such-server"),
        "{stderr}"
    );
    assert!(stderr.contains("add its directory to PATH"), "{stderr}");
    // An endpoint without a key is a configuration mistake, and the refusal says how to fix it.
    let output = Command::new(CRUCIBLE)
        .args([
            "proxy",
            "stdio",
            "--endpoint",
            "http://127.0.0.1:1",
            "--",
            "true",
        ])
        .env_remove("CRUCIBLE_SECRET_KEY")
        .env_remove("CRUCIBLE_SECRET_KEY_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("CRUCIBLE_SECRET_KEY"));
}

#[test]
fn the_http_subcommand_forwards_and_stops_on_a_signal() {
    // An upstream that answers every connection with one fixed response.
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in upstream.incoming() {
            let mut stream = stream.unwrap();
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                request.push(byte[0]);
            }
            let body = format!(
                "{{\"saw_authorization\":{}}}",
                String::from_utf8_lossy(&request).contains("authorization: Bearer opaque-token")
            );
            let _sent = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let mut proxy = Command::new(CRUCIBLE)
        .args([
            "proxy",
            "http",
            "--listen",
            "127.0.0.1:0",
            "--upstream",
            &format!("http://{upstream_addr}"),
        ])
        .env_remove("CRUCIBLE_ENDPOINT")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The proxy says where it listens on standard error.
    let mut stderr = BufReader::new(proxy.stderr.take().unwrap());
    let address = loop {
        let mut line = String::new();
        assert_ne!(
            stderr.read_line(&mut line).unwrap(),
            0,
            "the proxy exited early"
        );
        if let Some(rest) = line.split("listen=").nth(1) {
            break rest.split_whitespace().next().unwrap().to_owned();
        }
    };
    let mut client = TcpStream::connect(&address).unwrap();
    write!(
        client,
        "GET /mcp HTTP/1.1\r\nhost: mcp.example.test\r\nauthorization: Bearer opaque-token\r\nconnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert!(
        response.ends_with("{\"saw_authorization\":true}"),
        "{response}"
    );

    kill("-TERM", proxy.id());
    assert_eq!(proxy.wait().unwrap().code(), Some(0));
    let mut stdout = Vec::new();
    proxy
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    assert!(stdout.is_empty());
}
