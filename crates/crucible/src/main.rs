//! The Crucible server binary. Subcommands are wired as their crates land.

mod alerts;
mod control;
mod cursor_keys;
mod export;
mod facts;
mod identity;
mod jobs;
mod keys;
mod lifecycle;
mod mcp;
mod media;
mod proxy;
mod recent;
mod salts;
mod serve;
mod store;

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Context as _;
use clap::{Args, Parser, Subcommand};
use crucible_config::{Config, ConfigErrors, Loaded};

#[cfg(not(windows))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "crucible",
    version,
    about = "Observability and analytics for AI agents and MCP servers"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the server.
    Serve(Box<ServeArgs>),
    /// Apply the Postgres migrations and the ClickHouse schema and access steps, then exit. The steps are
    /// idempotent; with Postgres configured each is recorded in its ledger and applied once.
    Migrate(Box<ConfigArgs>),
    /// Check or print the configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Print the OpenAPI 3.1 document of the REST API (committed at specs/openapi/crucible-v1.json).
    Openapi,
    /// Administer sign-in on a server with a control plane (Postgres).
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Run the MCP proxy in front of an MCP server. It starts no server subsystem.
    Proxy(proxy::ProxyArgs),
    /// Export a project's spans in a window from the telemetry store, as crucible-jsonl or otlp-json, to a file
    /// or standard output.
    Export(Box<export::ExportArgs>),
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Create the organization and its first owner, instead of the one-time setup page that `serve` prints.
    /// The password is read from --password-file, or else from the first line of standard input.
    Init(Box<AdminInitArgs>),
    /// Print a one-time password-reset link for a user, valid for 30 minutes. This version sends no email, so
    /// pass the link on.
    ResetPassword(Box<AdminResetArgs>),
    /// Make an API key for a project and print `project_id`, `public_key` and `secret_key`. The secret key is
    /// shown once.
    CreateKey(Box<AdminCreateKeyArgs>),
}

#[derive(Debug, Args)]
struct AdminCreateKeyArgs {
    #[command(flatten)]
    config: ConfigArgs,
    /// The project: its slug (the first project's is `default`) or its id.
    #[arg(long)]
    project: String,
    /// The scopes, comma-separated, from ingest, read, write, delete and sql [default: ingest,read,write].
    #[arg(long, value_delimiter = ',')]
    scopes: Vec<String>,
    /// A note, shown with the key in Settings.
    #[arg(long)]
    note: Option<String>,
    /// Let the key send spans older than the event-age limit, to import history.
    #[arg(long)]
    import: bool,
}

#[derive(Debug, Args)]
struct AdminInitArgs {
    #[command(flatten)]
    config: ConfigArgs,
    /// The organization's name.
    #[arg(long)]
    org_name: String,
    /// The owner's email.
    #[arg(long)]
    email: String,
    /// The owner's display name.
    #[arg(long)]
    name: String,
    /// A file that holds the owner's password (12 to 128 characters).
    #[arg(long)]
    password_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct AdminResetArgs {
    #[command(flatten)]
    config: ConfigArgs,
    /// The user's email.
    email: String,
}

/// The owner's password for `admin init`: the file's first line, or standard input's.
#[allow(clippy::disallowed_methods)] // The binary reads the operator's input.
fn read_password(file: Option<&std::path::Path>) -> anyhow::Result<String> {
    let text = match file {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("reading the password file {}", path.display()))?,
        None => {
            let mut line = String::new();
            std::io::stdin()
                .read_line(&mut line)
                .context("reading the password from standard input")?;
            line
        }
    };
    Ok(text.lines().next().unwrap_or_default().to_owned())
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Load the configuration and report every problem at once. Exits with 1 when there is one.
    Check(Box<ConfigArgs>),
    /// Print the effective configuration as TOML, each value with where it came from. Secrets are redacted.
    Print(Box<ConfigArgs>),
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    config: ConfigArgs,
    /// Apply the migrations before the startup checks: the ClickHouse schema and the service identities and,
    /// with Postgres configured, the Postgres migrations. The steps are idempotent.
    #[arg(long)]
    migrate: bool,
}

/// The settings that have a flag. Each layer overrides the one before: the configuration file, then the
/// `CRUCIBLE_*` environment variables, then these flags. `crucible config print` shows the result.
#[derive(Debug, Default, Args)]
struct ConfigArgs {
    /// TOML configuration file. Also read from CRUCIBLE_CONFIG.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Address of the main listener (API, UI, ingest and health paths) [default: 0.0.0.0:8080].
    #[arg(long)]
    listen: Option<String>,
    /// Address of the OTLP/HTTP ingest listener [default: 0.0.0.0:4318].
    #[arg(long)]
    ingest_listen: Option<String>,
    /// Address of the OTLP/gRPC ingest listener [default: 0.0.0.0:4317].
    #[arg(long)]
    grpc_listen: Option<String>,
    /// TOML file of the project keys, for a server without Postgres: `[[project_key]]` tables with `project_id`,
    /// `public_key` and `secret`. With Postgres the keys live there; make them in Settings or with
    /// `crucible admin create-key`.
    #[arg(long)]
    keys_file: Option<String>,
    /// Address of the Prometheus metrics listener [default: 127.0.0.1:9464].
    #[arg(long)]
    metrics_listen: Option<String>,
    /// Directory that holds the `wal`, `quarantine` and `exports` roots [default: data].
    #[arg(long)]
    data_dir: Option<String>,
    /// Disk backend for the WAL: `auto`, `portable` or `uring` [default: auto].
    #[arg(long)]
    wal_backend: Option<String>,
    /// Serve the UI from this directory (a built export such as `ui/out`) instead of the one compiled in.
    /// The files are read once at startup.
    #[arg(long)]
    ui_dir: Option<String>,
    /// HTTP endpoint of the ClickHouse telemetry store, `http://host:8123` or `https://host:8443`. Without
    /// it the server only ingests: requests are made durable and wait in the WAL, and the API answers 503.
    /// Also read from CRUCIBLE_CLICKHOUSE_URL.
    #[arg(long)]
    clickhouse_url: Option<String>,
    /// The ClickHouse admin user that the operator created; it runs the migrations [default:
    /// crucible_admin]. Also read from CRUCIBLE_CLICKHOUSE_USER.
    #[arg(long)]
    clickhouse_user: Option<String>,
    /// File that holds the admin user's password. Also read from CRUCIBLE_CLICKHOUSE_PASSWORD_FILE; the
    /// password itself is read from CRUCIBLE_CLICKHOUSE_PASSWORD.
    #[arg(long)]
    clickhouse_password_file: Option<String>,
    /// File that holds the master key as 64 hexadecimal characters. The passwords of the four service
    /// identities are derived from it. Also read from CRUCIBLE_MASTER_KEY_FILE; the key itself is read from
    /// CRUCIBLE_MASTER_KEY.
    #[arg(long)]
    master_key_file: Option<String>,
    /// Bound on the memory of the recent-span index, in bytes. The default is 10% of the container's memory
    /// limit, or of RAM without one, at most 1 GiB.
    #[arg(long)]
    recent_max_bytes: Option<String>,
    /// The control plane, `postgres://user@host:5432/database`; add `?sslmode=require` for TLS. Without it
    /// the server runs without a control plane. Also read from CRUCIBLE_POSTGRES_URL.
    #[arg(long)]
    postgres_url: Option<String>,
    /// File that holds the Postgres password. Also read from CRUCIBLE_POSTGRES_PASSWORD_FILE; the password
    /// itself is read from CRUCIBLE_POSTGRES_PASSWORD.
    #[arg(long)]
    postgres_password_file: Option<String>,
}

impl ConfigArgs {
    /// The flags that were given, as `(dotted key, value)`.
    fn flags(&self) -> Vec<(&'static str, String)> {
        [
            ("server.listen", &self.listen),
            ("server.ingest_listen", &self.ingest_listen),
            ("server.grpc_listen", &self.grpc_listen),
            ("server.keys_file", &self.keys_file),
            ("server.metrics_listen", &self.metrics_listen),
            ("data.dir", &self.data_dir),
            ("data.wal_backend", &self.wal_backend),
            ("server.ui_dir", &self.ui_dir),
            ("clickhouse.url", &self.clickhouse_url),
            ("clickhouse.user", &self.clickhouse_user),
            ("clickhouse.password_file", &self.clickhouse_password_file),
            ("master_key_file", &self.master_key_file),
            ("recent.max_bytes", &self.recent_max_bytes),
            ("postgres.url", &self.postgres_url),
            ("postgres.password_file", &self.postgres_password_file),
        ]
        .into_iter()
        .filter_map(|(key, value)| Some((key, value.clone()?)))
        .collect()
    }

    /// The configuration of this process: file, environment, then these flags.
    fn load(&self) -> Result<Loaded, ConfigErrors> {
        crucible_config::load(self.config.as_deref(), &self.flags())
    }

    /// Loads the configuration for a command that runs, logging what looks like a mistake.
    fn load_for_run(&self) -> anyhow::Result<Config> {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .init();
        let loaded = self.load()?;
        for warning in &loaded.warnings {
            tracing::warn!("{warning}");
        }
        Ok(loaded.config)
    }
}

/// The disk backend the configuration names.
fn wal_backend(backend: crucible_config::WalBackend) -> crucible_env_prod::WalBackend {
    match backend {
        crucible_config::WalBackend::Auto => crucible_env_prod::WalBackend::Auto,
        crucible_config::WalBackend::Portable => crucible_env_prod::WalBackend::Portable,
        crucible_config::WalBackend::Uring => crucible_env_prod::WalBackend::Uring,
    }
}

/// Blocking threads the runtime keeps beyond the CPU budget, for the blocking work that takes no permit: disk reads,
/// name lookups and password hashing.
const BLOCKING_RESERVE: usize = 8;

/// Permits per CPU when `cpu.permits` is unset (default; revisit when the shipper's stages take their shares). The
/// blocking jobs share the CPUs with the runtime's workers, which run the rest of ingest and the shipper. With one
/// permit per CPU, a burst on the perf gate's small rig (2 CPUs) acknowledged 12 to 26% fewer spans per second than
/// with main's unbounded blocking pool (docs/development.md).
const PERMITS_PER_CPU: u32 = 8;

/// `cpu.permits`: the setting, or [`PERMITS_PER_CPU`] per CPU this process may use (its affinity and its cgroup
/// quota count). The binary is the one place that reads the core count.
fn cpu_permits(config: &Config) -> u32 {
    config.cpu_permits.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map_or(1, |cpus| u32::try_from(cpus.get()).unwrap_or(u32::MAX))
            .saturating_mul(PERMITS_PER_CPU)
    })
}

/// The runtime, whose blocking threads are the CPU budget's `cpu_permits` plus [`BLOCKING_RESERVE`].
fn runtime(cpu_permits: u32) -> anyhow::Result<tokio::runtime::Runtime> {
    let permits = usize::try_from(cpu_permits).unwrap_or(usize::MAX);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("crucible-worker")
        .max_blocking_threads(permits.saturating_add(BLOCKING_RESERVE))
        .build()
        .context("starting the runtime")
}

/// `crucible migrate`: the Postgres migrations, then the ClickHouse steps, whichever are configured.
async fn migrate(config: Config) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.postgres.is_some() || config.clickhouse.is_some(),
        "nothing to migrate: set postgres.url (--postgres-url, CRUCIBLE_POSTGRES_URL) or clickhouse.url \
         (--clickhouse-url, CRUCIBLE_CLICKHOUSE_URL)"
    );
    let prod = crucible_env_prod::build(
        tokio::runtime::Handle::current(),
        &crucible_env_prod::ProdConfig::default(),
    )
    .context("building the production environment")?;
    let env = &prod.env;
    let control = match &config.postgres {
        Some(postgres) => Some(control::Control::new(env, postgres)?),
        None => None,
    };
    match (&config.clickhouse, &control) {
        (Some(clickhouse), control) => {
            let clickhouse = store::ClickHouse::new(clickhouse, config.sql)?;
            let store = clickhouse.store(env);
            store::apply_migrations(env, &clickhouse, &store, control.as_ref()).await?;
        }
        (None, Some(control)) => {
            control.migrate_postgres(env).await?;
        }
        (None, None) => {}
    }
    tracing::info!(
        postgres = config.postgres.is_some(),
        clickhouse = config.clickhouse.is_some(),
        "the migrations are applied"
    );
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Proxy(args) => proxy::run(args),
        Command::Serve(args) => {
            let config = args.config.load_for_run()?;
            let permits = cpu_permits(&config);
            runtime(permits)?.block_on(serve::run(config, args.migrate, permits))
        }
        Command::Migrate(args) => {
            let config = args.load_for_run()?;
            runtime(cpu_permits(&config))?.block_on(migrate(config))
        }
        Command::Export(args) => {
            let config = args.config.load_for_run()?;
            runtime(cpu_permits(&config))?.block_on(export::run(&args, config))
        }
        Command::Config { command } => {
            let (args, print) = match &command {
                ConfigCommand::Check(args) => (args, false),
                ConfigCommand::Print(args) => (args, true),
            };
            let loaded = args.load()?;
            let mut err = std::io::stderr().lock();
            for warning in &loaded.warnings {
                writeln!(err, "warning: {warning}")?;
            }
            let mut out = std::io::stdout().lock();
            if print {
                out.write_all(loaded.print().as_bytes())?;
            } else {
                writeln!(out, "the configuration is valid")?;
            }
            Ok(())
        }
        Command::Openapi => {
            let json = crucible_api::openapi_json()?;
            std::io::stdout().lock().write_all(json.as_bytes())?;
            Ok(())
        }
        Command::Admin { command } => match command {
            AdminCommand::Init(args) => {
                let config = args.config.load_for_run()?;
                let password = read_password(args.password_file.as_deref())?;
                runtime(cpu_permits(&config))?.block_on(identity::admin_init(
                    &config,
                    &args.org_name,
                    &args.email,
                    &args.name,
                    &password,
                ))
            }
            AdminCommand::CreateKey(args) => {
                let config = args.config.load_for_run()?;
                let printed =
                    runtime(cpu_permits(&config))?.block_on(identity::admin_create_key(
                        &config,
                        &args.project,
                        &args.scopes,
                        args.note.clone(),
                        args.import,
                    ))?;
                std::io::stdout().lock().write_all(printed.as_bytes())?;
                writeln!(
                    std::io::stderr().lock(),
                    "the secret key is shown only now; running servers accept the key within 5 seconds"
                )?;
                Ok(())
            }
            AdminCommand::ResetPassword(args) => {
                let config = args.config.load_for_run()?;
                let link = runtime(cpu_permits(&config))?
                    .block_on(identity::admin_reset_password(&config, &args.email))?;
                writeln!(std::io::stdout().lock(), "{link}")?;
                if config.server.public_url.is_none() {
                    writeln!(
                        std::io::stderr().lock(),
                        "without server.public_url this is a path: open it at the address you reach the server \
                         at, for example http://127.0.0.1:8080/reset/?token=...; it works once and for 30 minutes"
                    )?;
                }
                Ok(())
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use clap::CommandFactory;
    use crucible_config::{Layers, resolve};

    use super::*;

    const KEY: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn cli_is_well_formed_and_prints_the_version() {
        Cli::command().debug_assert();
        let version = Cli::command().render_version();
        assert_eq!(
            version.trim(),
            format!("crucible {}", env!("CARGO_PKG_VERSION"))
        );
    }

    fn serve_args(argv: &[&str]) -> ServeArgs {
        match Cli::parse_from(argv).command {
            Command::Serve(args) => *args,
            other => panic!("{argv:?} parsed as {other:?}"),
        }
    }

    /// The configuration that `argv` and `env` give, with the two secret files of the tests.
    fn config(argv: &[&str], env: &[(&str, &str)]) -> Result<Config, ConfigErrors> {
        let flags = serve_args(argv).config.flags();
        let flags: Vec<(&str, String)> = flags.into_iter().collect();
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        let layers = Layers {
            file: None,
            env: &env,
            flags: &flags,
        };
        let files = |path: &Path| match path.to_str() {
            Some("/run/secrets/clickhouse") => Ok("password\n".to_owned()),
            Some("/run/secrets/master") => Ok(format!("{KEY}\n")),
            _ => Err(std::io::Error::other("no such file")),
        };
        resolve(&layers, &files).map(|loaded| loaded.config)
    }

    #[test]
    fn serve_defaults_match_the_contract_ports() {
        let config = config(&["crucible", "serve"], &[]).unwrap();
        assert_eq!(config.server.listen.to_string(), "0.0.0.0:8080");
        assert_eq!(config.server.ingest_listen.to_string(), "0.0.0.0:4318");
        assert_eq!(config.server.grpc_listen.to_string(), "0.0.0.0:4317");
        assert_eq!(config.server.metrics_listen.to_string(), "127.0.0.1:9464");
        assert_eq!(config.data.dir, Path::new("data"));
        assert_eq!(config.data.wal_backend, crucible_config::WalBackend::Auto);
        assert_eq!(config.server.keys_file, None);
        assert_eq!(config.server.ui_dir, None);
        assert_eq!(config.clickhouse, None);
        assert_eq!(config.postgres, None);
        assert!(!serve_args(&["crucible", "serve"]).migrate);
        let error =
            self::config(&["crucible", "serve", "--wal-backend", "floppy"], &[]).unwrap_err();
        assert!(
            error.to_string().contains("auto, portable, uring"),
            "{error}"
        );
        let config = self::config(&["crucible", "serve", "--ui-dir", "ui/out"], &[]).unwrap();
        assert_eq!(config.server.ui_dir.as_deref(), Some(Path::new("ui/out")));
    }

    #[test]
    fn serve_takes_every_first_flag() {
        let config = config(
            &[
                "crucible",
                "serve",
                "--listen",
                "127.0.0.1:1",
                "--ingest-listen",
                "127.0.0.1:2",
                "--grpc-listen",
                "127.0.0.1:4",
                "--metrics-listen",
                "127.0.0.1:3",
                "--keys-file",
                "keys.toml",
                "--data-dir",
                "/var/lib/crucible",
                "--wal-backend",
                "portable",
            ],
            &[],
        )
        .unwrap();
        let addr = |text: &str| text.parse::<SocketAddr>().unwrap();
        assert_eq!(config.server.listen, addr("127.0.0.1:1"));
        assert_eq!(config.server.ingest_listen, addr("127.0.0.1:2"));
        assert_eq!(config.server.grpc_listen, addr("127.0.0.1:4"));
        assert_eq!(config.server.metrics_listen, addr("127.0.0.1:3"));
        assert_eq!(
            config.server.keys_file.as_deref(),
            Some(Path::new("keys.toml"))
        );
        assert_eq!(config.data.dir, Path::new("/var/lib/crucible"));
        assert_eq!(
            config.data.wal_backend,
            crucible_config::WalBackend::Portable
        );
    }

    #[test]
    fn serve_takes_the_telemetry_store_from_flags() {
        let argv = [
            "crucible",
            "serve",
            "--clickhouse-url",
            "http://clickhouse:8123",
            "--clickhouse-password-file",
            "/run/secrets/clickhouse",
            "--master-key-file",
            "/run/secrets/master",
            "--migrate",
            "--recent-max-bytes",
            "1048576",
        ];
        assert!(serve_args(&argv).migrate);
        let config = config(&argv, &[]).unwrap();
        let clickhouse = config.clickhouse.unwrap();
        assert_eq!(clickhouse.url, "http://clickhouse:8123");
        assert_eq!(clickhouse.user, "crucible_admin");
        assert_eq!(clickhouse.password.expose(), "password");
        assert_eq!(clickhouse.master_key.expose(), KEY);
        assert_eq!(config.recent_max_bytes, Some(1 << 20));
    }

    #[test]
    fn serve_takes_the_telemetry_store_from_the_environment() {
        // The four variables the first `serve` read, and the two value variables of the secrets.
        let env = [
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            ("CRUCIBLE_CLICKHOUSE_USER", "admin"),
            ("CRUCIBLE_CLICKHOUSE_PASSWORD", "from-env"),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ];
        let clickhouse = config(&["crucible", "serve"], &env)
            .unwrap()
            .clickhouse
            .unwrap();
        assert_eq!(clickhouse.user, "admin");
        assert_eq!(clickhouse.password.expose(), "from-env");
        let env = [
            ("CRUCIBLE_CLICKHOUSE_URL", "http://clickhouse:8123"),
            (
                "CRUCIBLE_CLICKHOUSE_PASSWORD_FILE",
                "/run/secrets/clickhouse",
            ),
            ("CRUCIBLE_MASTER_KEY_FILE", "/run/secrets/master"),
        ];
        let clickhouse = config(&["crucible", "serve"], &env)
            .unwrap()
            .clickhouse
            .unwrap();
        assert_eq!(clickhouse.password.expose(), "password");
        assert_eq!(clickhouse.master_key.expose(), KEY);
        // The URL without its secrets says which settings are missing.
        let error = config(
            &["crucible", "serve", "--clickhouse-url", "http://c:8123"],
            &[],
        )
        .unwrap_err();
        let text = error.to_string();
        assert!(
            text.contains("--clickhouse-password-file") && text.contains("--master-key-file"),
            "{text}"
        );
    }

    #[test]
    fn postgres_is_optional_and_comes_from_a_flag_or_the_environment() {
        let argv = [
            "crucible",
            "serve",
            "--postgres-url",
            "postgres://crucible@postgres:5432/crucible",
            "--postgres-password-file",
            "/run/secrets/clickhouse",
            "--master-key-file",
            "/run/secrets/master",
        ];
        let postgres = config(&argv, &[]).unwrap().postgres.unwrap();
        assert_eq!(
            postgres.url.expose(),
            "postgres://crucible@postgres:5432/crucible"
        );
        assert_eq!(postgres.password.unwrap().expose(), "password");
        assert_eq!(postgres.pool_size, 8);
        let env = [
            ("CRUCIBLE_POSTGRES_URL", "postgres://u:p@h/d"),
            ("CRUCIBLE_MASTER_KEY", KEY),
        ];
        let postgres = config(&["crucible", "serve"], &env)
            .unwrap()
            .postgres
            .unwrap();
        assert_eq!(postgres.password, None);
    }

    #[test]
    fn migrate_and_config_take_the_same_settings() {
        let argv = ["crucible", "migrate", "--postgres-url", "postgres://u@h/d"];
        assert!(matches!(
            Cli::parse_from(argv).command,
            Command::Migrate(args) if args.postgres_url.is_some()
        ));
        for sub in ["check", "print"] {
            let argv = [
                "crucible",
                "config",
                sub,
                "--config",
                "crucible.toml",
                "--listen",
                "0.0.0.0:1",
            ];
            match Cli::parse_from(argv).command {
                Command::Config {
                    command: ConfigCommand::Check(args) | ConfigCommand::Print(args),
                } => {
                    assert_eq!(args.config.as_deref(), Some(Path::new("crucible.toml")));
                    assert_eq!(args.flags(), [("server.listen", "0.0.0.0:1".to_owned())]);
                }
                other => panic!("{other:?}"),
            }
        }
        assert!(Cli::try_parse_from(["crucible", "config"]).is_err());
        assert!(Cli::try_parse_from(["crucible", "serve", "--no-such-flag"]).is_err());
    }

    #[test]
    fn openapi_takes_no_arguments() {
        assert!(matches!(
            Cli::parse_from(["crucible", "openapi"]).command,
            Command::Openapi
        ));
        assert!(Cli::try_parse_from(["crucible", "openapi", "--yaml"]).is_err());
    }

    /// The most blocking jobs `runtime` runs at once, out of `jobs` jobs that each hold their thread until all of
    /// them run or 50 ms pass.
    fn blocking_at_once(runtime: &tokio::runtime::Runtime, jobs: usize) -> usize {
        let state = Arc::new((Mutex::new((0usize, 0usize)), Condvar::new()));
        runtime.block_on(async {
            let handles: Vec<_> = (0..jobs)
                .map(|_| {
                    let state = Arc::clone(&state);
                    runtime.spawn_blocking(move || {
                        let (lock, changed) = &*state;
                        let mut counts = lock.lock().unwrap();
                        counts.0 += 1;
                        counts.1 = counts.1.max(counts.0);
                        changed.notify_all();
                        let mut counts = changed
                            .wait_timeout_while(counts, Duration::from_millis(50), |c| c.0 < jobs)
                            .unwrap()
                            .0;
                        counts.0 -= 1;
                    })
                })
                .collect();
            for handle in handles {
                handle.await.unwrap();
            }
        });
        state.0.lock().unwrap().1
    }

    #[test]
    fn the_server_runtime_caps_its_blocking_threads() {
        // Two permits and a reserve of eight threads: at most ten blocking jobs at once, out of 64 that could run.
        let at_once = blocking_at_once(&runtime(2).unwrap(), 64);
        assert!(
            at_once <= 10,
            "{at_once} blocking jobs ran at once, above the ceiling of 10"
        );
        assert_eq!(at_once, 2 + BLOCKING_RESERVE);
    }

    #[test]
    fn the_cpu_budget_defaults_to_8_per_cpu_of_the_process() {
        let unset = config(&["crucible", "serve"], &[]).unwrap();
        let cpus = std::thread::available_parallelism().unwrap().get();
        assert_eq!(cpu_permits(&unset), u32::try_from(cpus).unwrap() * 8);
        let set = config(&["crucible", "serve"], &[("CRUCIBLE_CPU_PERMITS", "3")]).unwrap();
        assert_eq!(cpu_permits(&set), 3);
    }
}
