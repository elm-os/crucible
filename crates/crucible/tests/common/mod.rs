//! What the tests that run the `crucible serve` binary share. The server binds every listener on a port the
//! system picks, and a test reads the addresses from the line the server logs once it listens. A port that a
//! test picked and released before the server bound it could be taken by then, and four such picks could even
//! repeat one port: the server then exited at startup with "address already in use".

use std::net::SocketAddr;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

/// The address to give every listener: a port the system picks.
pub const ANY_PORT: &str = "127.0.0.1:0";

/// The line `crucible serve` logs once its listeners are bound, without its colours.
#[derive(Debug)]
pub struct Serving(String);

impl Serving {
    /// Where a listener is: `listen` (the API), `ingest`, `grpc` or `metrics`.
    pub fn addr(&self, listener: &str) -> SocketAddr {
        self.0
            .split_whitespace()
            .find_map(|field| field.strip_prefix(listener)?.strip_prefix('='))
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no {listener} address in {:?}", self.0))
    }
}

/// Waits until the `crucible serve` that writes `log` says where it listens. Panics with the server's error when
/// it exits first, and after `within`.
pub fn serving(child: &mut Child, log: &Path, within: Duration) -> Serving {
    let deadline = Instant::now() + within;
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        // Only a whole line: the server may be writing this one.
        if let Some(line) = text
            .split_inclusive('\n')
            .find(|line| line.ends_with('\n') && line.contains("crucible is serving"))
        {
            return Serving(plain(line));
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!(
                "crucible serve exited ({status}) before it listened: {}",
                exit_error(&text)
            );
        }
        assert!(
            Instant::now() < deadline,
            "crucible serve did not listen within {within:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What `crucible serve` printed when it stopped on an error: the `Error:` line and its causes, without the
/// backtrace that follows them.
pub fn exit_error(log: &str) -> String {
    let lines: Vec<&str> = log
        .lines()
        .skip_while(|line| !line.starts_with("Error: "))
        .take_while(|line| !line.starts_with("Stack backtrace:"))
        .collect();
    if lines.is_empty() {
        "the log has no error line".to_owned()
    } else {
        lines.join("\n").trim_end().to_owned()
    }
}

/// The log without the terminal colour codes.
pub fn plain(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            chars.by_ref().find(|c| *c == 'm');
        } else {
            out.push(c);
        }
    }
    out
}
