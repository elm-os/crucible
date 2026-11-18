//! Repository automation. Ambient APIs are allowed here; product crates use `crucible-env`.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  help                 show this message
  ui compress [<dir>]  write .br and .gz variants beside the compressible files of a UI export
                       (default ui/out); run it after every UI build that will be embedded or served

Planned: sim (run, replay), perf (gate, fsync-probe), conformance, authz-matrix, openapi.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("ui") if args.next().as_deref() == Some("compress") => {
            let dir = args
                .next()
                .map_or_else(|| PathBuf::from("ui/out"), PathBuf::from);
            match ui::compress_dir(&dir) {
                Ok(summary) => {
                    println!(
                        "{}: {} files, {} bytes; brotli {} files, {} bytes; gzip {} files, {} bytes",
                        dir.display(),
                        summary.files,
                        summary.bytes,
                        summary.br.0,
                        summary.br.1,
                        summary.gz.0,
                        summary.gz.1
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("ui compress {}: {e}", dir.display());
                    ExitCode::FAILURE
                }
            }
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
