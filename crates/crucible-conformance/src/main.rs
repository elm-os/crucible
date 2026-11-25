//! Runs the conformance suite against one ClickHouse server.
//!
//! ```text
//! crucible-conformance --url http://host:8123 [--user crucible_admin] [--password ...] [--cluster name]
//!                      [--replica-url http://other-replica:8123 ...] [--label 26.3]
//! ```
//!
//! The admin user must hold access management. The exit code is 0 when every row passes.

// The entry point reads its arguments and prints its report; product code does neither.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use crucible_conformance::{Target, run};
use crucible_env_prod::{ProdConfig, build};
use crucible_store_clickhouse::Credentials;

struct Args {
    target: Target,
    label: String,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let (mut url, mut cluster, mut label) = (None, None, None);
    let mut replicas = Vec::new();
    let mut admin = Credentials {
        user: "crucible_admin".to_owned(),
        password: String::new(),
    };
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--url" => url = Some(value),
            "--user" => admin.user = value,
            "--password" => admin.password = value,
            "--cluster" => cluster = Some(value),
            "--replica-url" => replicas.push(value),
            "--label" => label = Some(value),
            other => bail!("unknown argument {other}"),
        }
    }
    let url = url.context("--url is required")?;
    Ok(Args {
        label: label.unwrap_or_else(|| url.clone()),
        target: Target {
            url,
            admin,
            cluster,
            replicas,
        },
    })
}

fn main() -> Result<ExitCode> {
    let args = parse(std::env::args().skip(1))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let prod = build(runtime.handle().clone(), &ProdConfig::default())?;
    let outcomes = runtime.block_on(run(&prod.env, &args.target));
    let mut failed = 0;
    for outcome in &outcomes {
        match &outcome.result {
            Ok(detail) => println!("PASS\t{}\t{}\t{detail}", args.label, outcome.row),
            Err(reason) => {
                failed += 1;
                println!("FAIL\t{}\t{}\t{reason}", args.label, outcome.row);
            }
        }
    }
    println!(
        "{}: {} rows, {} passed, {failed} failed",
        args.label,
        outcomes.len(),
        outcomes.len() - failed
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
