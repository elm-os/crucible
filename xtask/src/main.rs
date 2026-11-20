//! Repository automation. Ambient APIs are allowed here; product crates use `crucible-env`.
#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod gen_ts;
mod perf;
mod sim;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "usage: cargo xtask <command>

commands:
  help                 show this message
  gen-ts               regenerate the TypeScript types of the REST API in ui/src/api/generated/ from the
                       DTOs of crucible-api-types; commit the result (CI fails on any difference)
  authz-matrix         regenerate docs/architecture/authz-matrix.md from the route permissions of
                       crucible-api; commit the result (a test fails on any difference)
  ui compress [<dir>]  write .br and .gz variants beside the compressible files of a UI export
                       (default ui/out); run it after every UI build that will be embedded or served";

const CONFORMANCE: &str = "The conformance suite against real ClickHouse servers runs with \
                           crates/crucible-conformance/scripts/conformance.sh.";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        None | Some("help" | "--help" | "-h") => {
            println!("{USAGE}\n{}\n{}\n\n{CONFORMANCE}", sim::USAGE, perf::USAGE);
            ExitCode::SUCCESS
        }
        Some("perf") => match perf::main(args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("perf: {e}");
                ExitCode::from(2)
            }
        },
        Some("sim") => match sim::main(args) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("sim: {e}");
                ExitCode::from(2)
            }
        },
        Some("authz-matrix") => {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../docs/architecture/authz-matrix.md");
            match std::fs::write(&path, crucible_api::authz_matrix()) {
                Ok(()) => {
                    println!("{}: {} routes", path.display(), crucible_api::ROUTES.len());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("authz-matrix: writing {}: {e}", path.display());
                    ExitCode::FAILURE
                }
            }
        }
        Some("gen-ts") => match gen_ts::generate() {
            Ok((dir, files)) => {
                println!("{}: {files} files", dir.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("gen-ts: {e}");
                ExitCode::FAILURE
            }
        },
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
            eprintln!(
                "unknown command: {other}\n\n{USAGE}\n{}\n{}",
                sim::USAGE,
                perf::USAGE
            );
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    /// The help says how to run the conformance suite, in the present tense.
    #[test]
    fn the_help_says_how_to_run_conformance() {
        assert!(super::CONFORMANCE.contains("scripts/conformance.sh"));
        assert!(!super::CONFORMANCE.to_lowercase().contains("planned"));
    }
}
