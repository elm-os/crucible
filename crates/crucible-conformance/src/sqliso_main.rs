//! Runs `sqliso`, the isolation suite of the SQL workspace (contract 7.5), against one ClickHouse server.
//!
//! ```text
//! crucible-sqliso --url http://host:8123 [--user crucible_admin] [--password ...] [--label 26.3]
//!                 [--switch strict|permissive] [--layers a,b,d] [--fuzz 5000] [--seed 1] [--identities 20]
//! ```
//!
//! `--switch` declares the server's `users_without_row_policies_can_read_rows` value; the suite checks it.
//! The admin user must hold access management. The exit code is 0 when every row passes.

// The entry point reads its arguments and prints its report; product code does neither.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use crucible_conformance::Target;
use crucible_conformance::sqliso::{Options, Switch, run};
use crucible_env_prod::{ProdConfig, build};
use crucible_store_clickhouse::Credentials;

struct Args {
    target: Target,
    label: String,
    options: Options,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Args> {
    let (mut url, mut label) = (None, None);
    let mut admin = Credentials {
        user: "crucible_admin".to_owned(),
        password: String::new(),
    };
    let mut options = Options {
        switch: Switch::Strict,
        layer_a: true,
        fuzz: 5000,
        seed: 1,
        layer_d: true,
        identities: 20,
    };
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--url" => url = Some(value),
            "--user" => admin.user = value,
            "--password" => admin.password = value,
            "--label" => label = Some(value),
            "--switch" => {
                options.switch = match value.as_str() {
                    "strict" => Switch::Strict,
                    "permissive" => Switch::Permissive,
                    other => bail!("--switch is strict or permissive, not {other}"),
                }
            }
            "--layers" => {
                let layers: Vec<&str> = value.split(',').collect();
                options.layer_a = layers.contains(&"a");
                options.layer_d = layers.contains(&"d");
                if !layers.contains(&"b") {
                    options.fuzz = 0;
                }
            }
            "--fuzz" => options.fuzz = value.parse().context("--fuzz is a number")?,
            "--seed" => options.seed = value.parse().context("--seed is a number")?,
            "--identities" => {
                options.identities = value.parse().context("--identities is a number")?;
            }
            other => bail!("unknown argument {other}"),
        }
    }
    let url = url.context("--url is required")?;
    Ok(Args {
        label: label.unwrap_or_else(|| url.clone()),
        target: Target {
            url,
            admin,
            cluster: None,
            replicas: Vec::new(),
        },
        options,
    })
}

fn main() -> Result<ExitCode> {
    let args = parse(std::env::args().skip(1))?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let prod = build(runtime.handle().clone(), &ProdConfig::default())?;
    let outcomes = runtime.block_on(run(&prod.env, &args.target, &args.options));
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
        "{} (switch {:?}): {} rows, {} passed, {failed} failed",
        args.label,
        args.options.switch,
        outcomes.len(),
        outcomes.len() - failed
    );
    Ok(if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
