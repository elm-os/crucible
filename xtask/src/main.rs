//! Repository automation. Ambient APIs are allowed here; product crates use `crucible-env`.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  help    show this message

Planned: sim (run, replay), perf (gate, fsync-probe), conformance, authz-matrix, openapi.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
