//! `crucible proxy`: the MCP proxy (contract 9.10). It starts none of the server's subsystems: no
//! configuration file, WAL, ClickHouse, Postgres or UI. Settings come from flags and `CRUCIBLE_*` variables.
//!
//! - `crucible proxy stdio -- <command> [args]` wraps a stdio MCP server.
//! - `crucible proxy http --listen <addr> --upstream <url>` stands in front of a Streamable HTTP MCP server.
//!
//! Standard output carries only the server's bytes; everything the proxy says goes to standard error.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Args, Subcommand};
use crucible_env::{ChildSignal, ChildSpec, Env, ExitStatus, MemoryRecorder, StderrMode};
use crucible_env_prod::{PortableDisk, SystemClock, TokioChildProcess, TokioNet, TokioSpawner};
use crucible_proxy::encode::ResourceInfo;
use crucible_proxy::http::{HostHeader, HttpProxyConfig};
use crucible_proxy::identity::{IdentityResolver, VerifyConfig};
use crucible_proxy::live::{ExportConfig, Telemetry};
use crucible_proxy::net::Target;
use crucible_proxy::stdio::ClientIo;
use crucible_proxy::{CoreConfig, ProxyCore};
use crucible_records::CaptureSource;
use futures::StreamExt as _;
use futures::stream::BoxStream;

/// The hidden subcommand that the stdio proxy starts its child through on Linux.
const EXEC_CHILD: &str = "exec-child";

#[derive(Debug, Args)]
pub struct ProxyArgs {
    #[command(subcommand)]
    command: ProxyCommand,
}

#[derive(Debug, Subcommand)]
enum ProxyCommand {
    /// Wrap a stdio MCP server: `crucible proxy stdio -- <command> [args]`. Bytes pass unchanged in both
    /// directions; the proxy exits the way the server does.
    Stdio(StdioArgs),
    /// Stand in front of a Streamable HTTP MCP server: requests, responses and event streams pass unchanged,
    /// `Authorization` included.
    Http(Box<HttpArgs>),
    /// Start a program so that it ends when this proxy does (internal).
    #[command(name = EXEC_CHILD, hide = true)]
    ExecChild {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// Where the telemetry goes. Without an endpoint the proxy only forwards.
#[derive(Debug, Args)]
struct TelemetryArgs {
    /// The Crucible server that receives the telemetry, such as http://127.0.0.1:8080. Also read from
    /// CRUCIBLE_ENDPOINT. Without it the proxy forwards and records nothing.
    #[arg(long, env = "CRUCIBLE_ENDPOINT")]
    endpoint: Option<String>,
    /// File that holds the project's secret key (csk_...) with the ingest scope. Also read from
    /// CRUCIBLE_SECRET_KEY_FILE; the key itself is read from CRUCIBLE_SECRET_KEY.
    #[arg(long, env = "CRUCIBLE_SECRET_KEY_FILE")]
    secret_key_file: Option<PathBuf>,
    #[arg(long, env = "CRUCIBLE_SECRET_KEY", hide = true, hide_env_values = true)]
    secret_key: Option<String>,
    /// The name the server is reported under. Defaults to the command's file name (stdio) or the upstream
    /// host (http). Also read from CRUCIBLE_SERVER_NAME.
    #[arg(long, env = "CRUCIBLE_SERVER_NAME")]
    server_name: Option<String>,
    /// The deploy id of the server, for deploy comparison. Also read from CRUCIBLE_DEPLOY_ID.
    #[arg(long, env = "CRUCIBLE_DEPLOY_ID")]
    deploy_id: Option<String>,
}

#[derive(Debug, Args)]
struct StdioArgs {
    #[command(flatten)]
    telemetry: TelemetryArgs,
    /// Copy the server's standard error through the proxy instead of letting the server inherit it.
    #[arg(long)]
    capture_stderr: bool,
    /// The server command and its arguments, after `--`.
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

/// Which `Host` header the upstream sees.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum HostHeaderArg {
    /// The client's, so a token audience bound to the public name stays valid.
    Preserve,
    /// The upstream's own, for upstreams that route by host name.
    Upstream,
}

/// Whether identity is derived from the bearer token.
#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
enum IdentityArg {
    /// No identity is recorded.
    Off,
    /// The token's claims are read without verification and labelled unverified.
    Claims,
    /// Only tokens whose signature verifies against --jwks-url yield an identity.
    Verified,
}

#[derive(Debug, Args)]
struct HttpArgs {
    #[command(flatten)]
    telemetry: TelemetryArgs,
    /// The address to listen on, such as 127.0.0.1:8090. Plain HTTP; put a TLS terminator in front for
    /// clients outside the host or the internal network.
    #[arg(long, env = "CRUCIBLE_PROXY_LISTEN")]
    listen: SocketAddr,
    /// The MCP server, such as http://127.0.0.1:3000 or https://mcp.internal.example. A path in the URL is
    /// put in front of every request path.
    #[arg(long, env = "CRUCIBLE_PROXY_UPSTREAM")]
    upstream: String,
    /// Which `Host` header the upstream sees.
    #[arg(long, value_enum, default_value = "preserve")]
    host_header: HostHeaderArg,
    /// Derive the caller's identity from the bearer token. Off by default; the token is forwarded untouched
    /// in every mode. Also read from CRUCIBLE_PROXY_IDENTITY.
    #[arg(
        long,
        value_enum,
        default_value = "off",
        env = "CRUCIBLE_PROXY_IDENTITY"
    )]
    identity: IdentityArg,
    /// The JWKS URL of the token issuer, for --identity verified.
    #[arg(long, env = "CRUCIBLE_PROXY_JWKS_URL")]
    jwks_url: Option<String>,
    /// With --identity verified: the audience a token must carry.
    #[arg(long, env = "CRUCIBLE_PROXY_JWT_AUDIENCE")]
    jwt_audience: Option<String>,
    /// With --identity verified: the issuer a token must carry.
    #[arg(long, env = "CRUCIBLE_PROXY_JWT_ISSUER")]
    jwt_issuer: Option<String>,
}

/// The environment of a proxy process: clocks, entropy, tasks and the network. No disk root is opened.
fn proxy_env(runtime: &tokio::runtime::Handle) -> anyhow::Result<(Env, Arc<TokioSpawner>)> {
    let clock = Arc::new(SystemClock::new());
    let rng = Arc::new(
        crucible_env_prod::os_seeded_rng().map_err(|e| anyhow::anyhow!("os entropy: {e}"))?,
    );
    let spawner = Arc::new(TokioSpawner::new(runtime.clone()));
    let disk = Arc::new(PortableDisk::new(runtime.clone(), &BTreeMap::new())?);
    let net = Arc::new(TokioNet::new(clock.clone()));
    let env = Env::new(
        clock,
        rng,
        spawner.clone(),
        disk,
        net,
        Arc::new(MemoryRecorder::new()),
    );
    Ok((env, spawner))
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("crucible-proxy")
        .build()
        .context("starting the runtime")
}

fn init_logging(level: tracing::Level) {
    // Standard output belongs to the MCP server; the proxy speaks on standard error only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(level)
        .init();
}

/// Starts the telemetry task, or none when no endpoint is set.
#[allow(clippy::disallowed_methods)] // The binary reads the secret key file it was given.
fn telemetry(
    env: &Env,
    args: &TelemetryArgs,
    capture_source: CaptureSource,
    default_name: &str,
) -> anyhow::Result<Telemetry> {
    let key = match (&args.secret_key, &args.secret_key_file) {
        (Some(key), _) => Some(key.trim().to_owned()),
        (None, Some(path)) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("reading the secret key file {}", path.display()))?
                .trim()
                .to_owned(),
        ),
        (None, None) => None,
    };
    let (endpoint, key) = match (&args.endpoint, key) {
        (Some(endpoint), Some(key)) if !key.is_empty() => (endpoint, key),
        (Some(_), _) => anyhow::bail!(
            "an endpoint is set but no secret key: set CRUCIBLE_SECRET_KEY or --secret-key-file to a project \
             secret key with the ingest scope, or remove --endpoint (CRUCIBLE_ENDPOINT) to proxy without \
             telemetry"
        ),
        (None, _) => {
            tracing::warn!(
                "telemetry is off: set --endpoint (CRUCIBLE_ENDPOINT) and CRUCIBLE_SECRET_KEY to record \
                 operations"
            );
            return Ok(Telemetry::disabled(env));
        }
    };
    let endpoint = Target::parse(endpoint).map_err(|e| anyhow::anyhow!("--endpoint: {e}"))?;
    let core = ProxyCore::new(
        CoreConfig::new(capture_source),
        env.rng.fork("crucible-proxy"),
    );
    Ok(Telemetry::start(
        env,
        core,
        ExportConfig {
            endpoint,
            secret_key: key,
            timeout: Duration::from_secs(5),
            resource: ResourceInfo {
                service_name: args
                    .server_name
                    .clone()
                    .unwrap_or_else(|| default_name.to_owned()),
                instance_id: env.new_id().to_string(),
                deploy_id: args.deploy_id.clone(),
                capture_source,
            },
        },
    ))
}

/// The signals that are forwarded to the child.
#[cfg(unix)]
fn signals() -> anyhow::Result<BoxStream<'static, ChildSignal>> {
    use tokio::signal::unix::{SignalKind, signal};

    let stream = |kind, forward| -> anyhow::Result<BoxStream<'static, ChildSignal>> {
        let signal = signal(kind).context("installing a signal handler")?;
        Ok(
            futures::stream::unfold(signal, move |mut signal| async move {
                signal.recv().await.map(|()| (forward, signal))
            })
            .boxed(),
        )
    };
    Ok(futures::stream::select_all([
        stream(SignalKind::terminate(), ChildSignal::Terminate)?,
        stream(SignalKind::interrupt(), ChildSignal::Interrupt)?,
        stream(SignalKind::hangup(), ChildSignal::Hangup)?,
    ])
    .boxed())
}

/// Windows has no signals to forward: Ctrl-C ends the server's process tree.
#[cfg(not(unix))]
fn signals() -> anyhow::Result<BoxStream<'static, ChildSignal>> {
    Ok(futures::stream::unfold((), |()| async {
        tokio::signal::ctrl_c()
            .await
            .ok()
            .map(|()| (ChildSignal::Interrupt, ()))
    })
    .boxed())
}

/// Ends this process the way the child ended: with its exit code, or by its signal.
fn exit_like(status: ExitStatus) -> ! {
    match status {
        ExitStatus::Code(code) => std::process::exit(code),
        ExitStatus::Signal(signal) => {
            // Restores the default disposition and raises the signal, so a parent sees the same death.
            #[cfg(unix)]
            let _still_alive = signal_hook::low_level::emulate_default_handler(signal);
            // The signal's default did not end the process: use the shell's convention.
            std::process::exit(128 + signal)
        }
    }
}

fn stdio(args: StdioArgs) -> anyhow::Result<()> {
    init_logging(tracing::Level::WARN);
    let (program, rest) = args
        .command
        .split_first()
        .context("give the server command after `--`")?;
    let default_name = std::path::Path::new(program).file_stem().map_or_else(
        || program.clone(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let spec = ChildSpec {
        program: program.clone(),
        args: rest.to_vec(),
        stderr: if args.capture_stderr {
            StderrMode::Pipe
        } else {
            StderrMode::Inherit
        },
        ..ChildSpec::default()
    };
    let exe = std::env::current_exe().context("finding this executable")?;
    let children = TokioChildProcess::new()
        .with_parent_death_trampoline(exe, vec!["proxy".to_owned(), EXEC_CHILD.to_owned()]);
    let runtime = runtime()?;
    // The child is started from this thread, which lives as long as the process: the parent-death signal is
    // tied to the thread that starts the child.
    let status = runtime.block_on(async {
        let (env, _scope) = proxy_env(&tokio::runtime::Handle::current())?;
        let telemetry = telemetry(
            &env,
            &args.telemetry,
            CaptureSource::ProxyStdio,
            &default_name,
        )?;
        let client = ClientIo {
            input: Box::new(tokio::io::stdin()),
            output: Box::new(tokio::io::stdout()),
            error: Box::new(tokio::io::stderr()),
        };
        let status =
            crucible_proxy::stdio::run(&env, &children, spec, client, signals()?, telemetry.tap())
                .await?;
        // At most the 500 ms flush deadline.
        telemetry.finish().await;
        anyhow::Ok(status)
    })?;
    // Not a normal return: a read of standard input that never completes would keep the runtime from stopping.
    exit_like(status)
}

fn http(args: HttpArgs) -> anyhow::Result<()> {
    init_logging(tracing::Level::INFO);
    let upstream = Target::parse(&args.upstream).map_err(|e| anyhow::anyhow!("--upstream: {e}"))?;
    let verify = match (args.identity, &args.jwks_url) {
        (IdentityArg::Verified, Some(url)) => Some(VerifyConfig {
            jwks: Target::parse(url).map_err(|e| anyhow::anyhow!("--jwks-url: {e}"))?,
            audience: args.jwt_audience.clone(),
            issuer: args.jwt_issuer.clone(),
            refresh: Duration::from_secs(600),
        }),
        (IdentityArg::Verified, None) => anyhow::bail!(
            "--identity verified needs the issuer's key set: set --jwks-url (CRUCIBLE_PROXY_JWKS_URL), or use \
             --identity claims to record unverified claims, or --identity off"
        ),
        _ => None,
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("crucible-proxy")
        .build()
        .context("starting the runtime")?;
    runtime.block_on(async {
        let (env, scope) = proxy_env(&tokio::runtime::Handle::current())?;
        let telemetry = telemetry(
            &env,
            &args.telemetry,
            CaptureSource::ProxyHttp,
            &upstream.host,
        )?;
        let identity = match (args.identity, verify) {
            (IdentityArg::Off, _) => IdentityResolver::off(&env),
            (IdentityArg::Claims, _) => IdentityResolver::claims(&env),
            (IdentityArg::Verified, Some(verify)) => IdentityResolver::verified(&env, verify),
            (IdentityArg::Verified, None) => IdentityResolver::off(&env),
        };
        let listener = env.net.listen(args.listen).await.with_context(|| {
            format!(
                "listening on {}; choose another address with --listen",
                args.listen
            )
        })?;
        let config = HttpProxyConfig {
            upstream,
            host_header: match args.host_header {
                HostHeaderArg::Preserve => HostHeader::Preserve,
                HostHeaderArg::Upstream => HostHeader::Upstream,
            },
            connect_timeout: Duration::from_secs(10),
            identity,
        };
        tracing::info!(
            listen = %listener.local_addr().unwrap_or(args.listen),
            upstream = %config.upstream.authority(),
            "the proxy is listening"
        );
        let mut serving = env.spawn(
            "proxy-http",
            crucible_proxy::http::serve(env.clone(), listener, config, telemetry.tap()),
        );
        let mut signals = signals()?;
        tokio::select! {
            biased;
            _signal = signals.next() => {}
            _stopped = serving.join() => {}
        }
        scope.request_shutdown();
        telemetry.finish().await;
        anyhow::Ok(())
    })?;
    // Open connections are not waited for: an event stream may never end.
    std::process::exit(0)
}

/// Runs a `crucible proxy` subcommand.
pub fn run(args: ProxyArgs) -> anyhow::Result<()> {
    match args.command {
        ProxyCommand::Stdio(args) => stdio(args),
        ProxyCommand::Http(args) => http(*args),
        ProxyCommand::ExecChild { args } => {
            #[cfg(unix)]
            {
                let error = crucible_env_prod::parent_death_trampoline(&args);
                // Like a shell: 127 when the command cannot be started.
                let _written = writeln!(
                    std::io::stderr(),
                    "crucible proxy: cannot start {:?}: {error}",
                    args.get(1)
                );
                std::process::exit(127)
            }
            #[cfg(not(unix))]
            {
                let _unused = args;
                anyhow::bail!("this internal subcommand exists only on Unix")
            }
        }
    }
}
