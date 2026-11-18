//! `crucible-proxy-rig`: the test and benchmark rig of the proxy. It is not part of the product and the
//! `crucible` binary never runs it.
//!
//! - `crucible-proxy-rig server [flags]`: a scripted MCP-shaped server on standard input and output.
//!
//! The rig talks to real pipes, sockets and processes by design, so the determinism lints are off here.

#![allow(
    clippy::disallowed_methods,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

#[path = "rig/server.rs"]
mod server;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("server") => server::run(&args[1..]),
        _ => {
            eprintln!("usage: crucible-proxy-rig server [flags]");
            std::process::exit(2);
        }
    }
}
