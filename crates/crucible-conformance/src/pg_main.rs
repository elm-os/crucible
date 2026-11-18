//! Runs the control-store conformance suite against one Postgres server.
//!
//! ```text
//! crucible-pg-conformance --url postgres://user:password@host:5432/database [--label 18]
//! ```
//!
//! The database must be empty or hold only Crucible's own tables; the suite empties them. The exit code is 0
//! when every row passes.

// The entry point reads its arguments and prints its report; product code does neither.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use crucible_conformance::control_pg::run;
use crucible_env_prod::{ProdConfig, build};

fn main() -> Result<ExitCode> {
    let (mut url, mut label) = (None, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--url" => url = Some(value),
            "--label" => label = Some(value),
            other => bail!("unknown argument {other}"),
        }
    }
    let url = url.context("--url is required")?;
    let label = label.unwrap_or_else(|| "postgres".to_owned());
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let prod = build(runtime.handle().clone(), &ProdConfig::default())?;
    let outcomes = runtime.block_on(run(&prod.env, &url));
    let mut failed = 0;
    for outcome in &outcomes {
        match &outcome.result {
            Ok(detail) => println!("PASS\t{label}\t{}\t{detail}", outcome.row),
            Err(reason) => {
                failed += 1;
                println!("FAIL\t{label}\t{}\t{reason}", outcome.row);
            }
        }
    }
    println!(
        "{label}: {} rows, {} passed, {failed} failed",
        outcomes.len(),
        outcomes.len() - failed
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
