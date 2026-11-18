//! `crucible proxy stdio` as the real binary on Windows: the server's process tree ends when the proxy process
//! is killed, because the Job Object that holds the tree closes with the proxy (kill-on-close). The Unix
//! counterpart, the parent-death signal, is in `proxy.rs`.
#![cfg(windows)]
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::io::{BufRead as _, BufReader};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CRUCIBLE: &str = env!("CARGO_BIN_EXE_crucible");

/// A server that opens a listener, says its port on standard output and then sleeps.
const LISTENER: &str = "$l = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0); \
     $l.Start(); [Console]::Out.WriteLine($l.LocalEndpoint.Port); [Console]::Out.Flush(); \
     Start-Sleep -Seconds 600";

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2)).is_ok()
}

#[test]
fn the_servers_process_tree_ends_when_the_proxy_is_killed() {
    // `powershell` is found through PATH and PATHEXT; it runs as the proxy's child inside the Job Object.
    let mut proxy = Command::new(CRUCIBLE)
        .args([
            "proxy",
            "stdio",
            "--",
            "powershell",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            LISTENER,
        ])
        .env_remove("CRUCIBLE_ENDPOINT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(proxy.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let port: u16 = line
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("no port in {line:?}"));
    assert!(listening(port), "the server listens on {port}");

    // TerminateProcess: the proxy runs no handler and no destructor. Only the closing of its handles is left.
    proxy.kill().unwrap();
    proxy.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while listening(port) {
        assert!(Instant::now() < deadline, "the server outlived the proxy");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_exit_code_of_a_command_passes_through() {
    // `cmd` resolves to `cmd.exe` through PATHEXT; its exit code is the proxy's.
    let output = Command::new(CRUCIBLE)
        .args(["proxy", "stdio", "--", "cmd", "/d", "/c", "exit 7"])
        .env_remove("CRUCIBLE_ENDPOINT")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("telemetry is off"), "{stderr}");
}
