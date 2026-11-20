//! Runs the object store conformance rows against one S3-compatible server.
//!
//! ```text
//! crucible-s3-conformance --endpoint http://host:7070 --bucket media --access-key KEY --secret-key SECRET
//!     [--region us-east-1] [--virtual-host] [--label versitygw]
//! ```
//!
//! The bucket must exist; the rows write and delete keys under `conformance/`. The exit code is 0 when every
//! row passes.

// The entry point reads its arguments and prints its report; product code does neither.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use crucible_conformance::s3::run;
use crucible_env_prod::{ProdConfig, build};
use crucible_media::S3Config;

fn main() -> Result<ExitCode> {
    let mut config = S3Config {
        endpoint: String::new(),
        public_endpoint: None,
        region: "us-east-1".to_owned(),
        bucket: String::new(),
        access_key: String::new(),
        secret_key: String::new(),
        virtual_host: false,
    };
    let mut label = "s3".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        if flag == "--virtual-host" {
            config.virtual_host = true;
            continue;
        }
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--endpoint" => config.endpoint = value,
            "--bucket" => config.bucket = value,
            "--access-key" => config.access_key = value,
            "--secret-key" => config.secret_key = value,
            "--region" => config.region = value,
            "--label" => label = value,
            other => bail!("unknown argument {other}"),
        }
    }
    if config.endpoint.is_empty() || config.bucket.is_empty() {
        bail!("--endpoint and --bucket are required");
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let prod = build(runtime.handle().clone(), &ProdConfig::default())?;
    let outcomes = runtime.block_on(run(&prod.env, &config));
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
