//! `crucible serve`: builds the production `Env`, opens the WAL, and runs the HTTP listeners until asked to stop.
//!
//! The servers accept through `Env`'s `Net`, so the same code will run over the simulated network. Each accept
//! loop is a named task in the shutdown scope; axum runs one further tokio task per connection, and graceful
//! shutdown waits for those before the accept-loop task returns. The routers hold the only handles to the WAL,
//! so its committer stops once the last server has returned.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::get;
use crucible_auth::StaticKeys;
use crucible_env::metrics::registry;
use crucible_env::{
    Counter, Env, Histogram, JoinHandle, Listener, MemoryRecorder, NetError, Root, Stream,
};
use crucible_env_prod::ProdConfig;
use crucible_ingest::{Ingest, IngestConfig};
use crucible_wal::{Wal, WalConfig};

use crate::ServeArgs;

/// How long tasks get to finish after shutdown is requested before they are aborted.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Pause after an accept failure that is not about one connection (for example, out of descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);

/// A `Net` listener as an axum listener.
struct EnvListener {
    inner: Box<dyn Listener>,
    env: Env,
    name: &'static str,
    errors: Counter,
}

impl EnvListener {
    async fn bind(env: &Env, name: &'static str, addr: SocketAddr) -> anyhow::Result<Self> {
        let inner = env
            .net
            .listen(addr)
            .await
            .with_context(|| format!("binding the {name} listener on {addr}"))?;
        let errors = env.metrics.counter(&registry::NET_ACCEPT_ERRORS, &[name]);
        Ok(Self {
            inner,
            env: env.clone(),
            name,
            errors,
        })
    }
}

/// Whether an accept error concerns only the connection that was being accepted.
fn is_connection_error(e: &NetError) -> bool {
    matches!(
        e,
        NetError::Io(e) if matches!(
            e.kind(),
            io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset
        )
    )
}

impl axum::serve::Listener for EnvListener {
    type Io = Stream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Stream, SocketAddr) {
        loop {
            match self.inner.accept().await {
                Ok(connection) => return connection,
                Err(e) => {
                    self.errors.add(1);
                    tracing::warn!(listener = self.name, error = %e, "accept failed");
                    if !is_connection_error(&e) {
                        self.env.clock.sleep(ACCEPT_BACKOFF).await;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr().map_err(io::Error::other)
    }
}

#[derive(Clone)]
struct HttpMetrics {
    env: Env,
    listener: &'static str,
    duration: Histogram,
}

/// Counts every response by status and records the time to the response head.
async fn observe(State(metrics): State<HttpMetrics>, request: Request, next: Next) -> Response {
    let start = metrics.env.clock.now();
    let response = next.run(request).await;
    let elapsed = metrics.env.clock.now().saturating_duration_since(start);
    metrics
        .duration
        .record(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
    let status = response.status();
    let labels = [metrics.listener, status.as_str()];
    metrics
        .env
        .metrics
        .counter(&registry::HTTP_REQUESTS, &labels)
        .add(1);
    response
}

fn observed(router: Router, env: &Env, listener: &'static str) -> Router {
    let duration = env
        .metrics
        .histogram(&registry::HTTP_REQUEST_DURATION, &[listener]);
    router.layer(from_fn_with_state(
        HttpMetrics {
            env: env.clone(),
            listener,
            duration,
        },
        observe,
    ))
}

/// What the routers serve: OTLP ingest and the WAL behind it.
struct App {
    ingest: Ingest,
    wal: Wal,
}

/// Opens the WAL under the `wal` root, recovering what is there (contract 4.6), and builds ingest over it.
async fn open_app(env: &Env, keys: StaticKeys) -> anyhow::Result<App> {
    let config = WalConfig::default();
    let (wal, report) = Wal::open(env, config.clone())
        .await
        .map_err(|e| anyhow::anyhow!("opening the WAL failed ({}): {e}", e.code()))?;
    tracing::info!(
        epoch = %report.epoch,
        created = report.created,
        last_lsn = %report.last_lsn,
        segments = report.segments.len(),
        "wal opened"
    );
    let ingest = Ingest::new(
        env,
        wal.clone(),
        &config,
        Arc::new(keys),
        IngestConfig::default(),
    );
    Ok(App { ingest, wal })
}

/// Readiness (contract 11.8): the WAL accepts writes. The keys of milestone 1 come from configuration, so they
/// are always loaded.
fn readiness(wal: &Wal) -> (StatusCode, &'static str) {
    if wal.admission().failed {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "not ready: the wal has failed",
        )
    } else {
        (StatusCode::OK, "ready")
    }
}

/// The main listener's routes: health, readiness and OTLP ingest. Later waves add the API and the UI.
fn api_router(env: &Env, app: &App) -> Router {
    let wal = app.wal.clone();
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let ready = readiness(&wal);
                async move { ready }
            }),
        )
        .merge(app.ingest.router());
    observed(router, env, "api")
}

/// The OTLP/HTTP listener's routes.
fn ingest_router(env: &Env, app: &App) -> Router {
    observed(app.ingest.router(), env, "ingest")
}

fn metrics_router(env: &Env, recorder: Arc<MemoryRecorder>) -> Router {
    let render = move || {
        let recorder = Arc::clone(&recorder);
        async move {
            (
                [(CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
                recorder.render_prometheus(),
            )
        }
    };
    observed(Router::new().route("/metrics", get(render)), env, "metrics")
}

/// The listener addresses.
#[derive(Clone, Copy, Debug)]
struct Addrs {
    api: SocketAddr,
    ingest: SocketAddr,
    metrics: SocketAddr,
}

/// The running listeners.
struct Servers {
    addrs: Addrs,
    api: JoinHandle<io::Result<()>>,
    ingest: JoinHandle<io::Result<()>>,
    metrics: JoinHandle<io::Result<()>>,
}

/// Serves `router` on `listener` in the shutdown scope until the scope's shutdown signal resolves and the open
/// connections have finished.
fn serve(
    env: &Env,
    name: &'static str,
    listener: EnvListener,
    router: Router,
) -> JoinHandle<io::Result<()>> {
    let signal = env.shutdown();
    env.spawn(name, async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(signal)
            .await
    })
}

/// Binds the three listeners and spawns their accept loops. The routers take over `app`.
async fn start(
    env: &Env,
    recorder: Arc<MemoryRecorder>,
    listen: Addrs,
    app: App,
) -> anyhow::Result<Servers> {
    let api = EnvListener::bind(env, "api", listen.api).await?;
    let ingest = EnvListener::bind(env, "ingest", listen.ingest).await?;
    let metrics = EnvListener::bind(env, "metrics", listen.metrics).await?;
    let addrs = Addrs {
        api: api.inner.local_addr()?,
        ingest: ingest.inner.local_addr()?,
        metrics: metrics.inner.local_addr()?,
    };
    let (api_router, ingest_router) = (api_router(env, &app), ingest_router(env, &app));
    drop(app);
    Ok(Servers {
        addrs,
        api: serve(env, "http-api", api, api_router),
        ingest: serve(env, "http-ingest", ingest, ingest_router),
        metrics: serve(env, "http-metrics", metrics, metrics_router(env, recorder)),
    })
}

/// Resolves with the name of the first termination signal the process receives.
async fn termination_signal() -> io::Result<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            biased;
            _ = terminate.recv() => Ok("SIGTERM"),
            result = tokio::signal::ctrl_c() => result.map(|()| "SIGINT"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.map(|()| "Ctrl-C")
    }
}

fn server_result(
    name: &str,
    result: Result<io::Result<()>, crucible_env::Cancelled>,
) -> anyhow::Result<()> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e).with_context(|| format!("the {name} server failed")),
        Err(crucible_env::Cancelled) => anyhow::bail!("the {name} server was aborted"),
    }
}

pub(crate) async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let roots = [Root::Wal, Root::Quarantine, Root::Exports];
    let config = ProdConfig {
        roots: roots
            .into_iter()
            .map(|root| (root, args.data_dir.join(root.name())))
            .collect::<BTreeMap<_, _>>(),
        wal_backend: args.wal_backend,
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &config)
        .context("building the production environment")?;
    let env = &prod.env;
    env.metrics
        .gauge(&registry::BUILD_INFO, &[env!("CARGO_PKG_VERSION")])
        .set(1);

    let keys = match &args.keys_file {
        Some(path) => crate::keys::load(path, env)?,
        None => StaticKeys::new(env.rng.as_ref(), Vec::new())?,
    };
    if keys.is_empty() {
        tracing::warn!(
            "no project keys are configured (--keys-file); ingest refuses every request"
        );
    }
    let key_count = keys.len();
    let app = open_app(env, keys).await?;
    let listen = Addrs {
        api: args.listen,
        ingest: args.ingest_listen,
        metrics: args.metrics_listen,
    };
    let mut servers = start(env, Arc::clone(&prod.recorder), listen, app).await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %servers.addrs.api,
        ingest = %servers.addrs.ingest,
        metrics = %servers.addrs.metrics,
        wal_backend = prod.wal_backend.backend.as_str(),
        keys = key_count,
        "crucible is serving"
    );

    // Run until a termination signal arrives or a server stops on its own.
    let early = tokio::select! {
        biased;
        signal = termination_signal() => {
            tracing::info!(signal = signal.context("installing signal handlers")?, "shutdown requested");
            None
        }
        result = servers.api.join() => Some(("api", result)),
        result = servers.ingest.join() => Some(("ingest", result)),
        result = servers.metrics.join() => Some(("metrics", result)),
    };

    let aborted = prod.spawner.close(SHUTDOWN_GRACE).await;
    if let Some((name, result)) = early {
        server_result(name, result)?;
        anyhow::bail!("the {name} server stopped without a shutdown request");
    }
    server_result("api", servers.api.join().await)?;
    server_result("ingest", servers.ingest.join().await)?;
    server_result("metrics", servers.metrics.join().await)?;
    anyhow::ensure!(
        aborted.is_empty(),
        "tasks did not stop within the grace period: {aborted:?}"
    );
    tracing::info!("shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::{Path, PathBuf};

    use crucible_auth::KeyVerifier as _;
    use crucible_env_prod::WalBackend;
    use crucible_records::Lsn;
    use crucible_wal_codec::{ContentOrigin, Record, RecordType};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    const PROJECT: &str = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";

    fn secret() -> String {
        format!("csk_{}", "7".repeat(43))
    }

    fn keys_toml() -> String {
        format!(
            "[[project_key]]\nproject_id = \"{PROJECT}\"\nsecret = \"{}\"\n",
            secret()
        )
    }

    fn loopback() -> Addrs {
        Addrs {
            api: LOOPBACK,
            ingest: LOOPBACK,
            metrics: LOOPBACK,
        }
    }

    /// The production environment with its disk roots under `dir`, as `--data-dir` lays them out.
    fn prod_in(dir: &Path) -> crucible_env_prod::Prod {
        let roots = [Root::Wal, Root::Quarantine, Root::Exports]
            .into_iter()
            .map(|root| (root, dir.join(root.name())))
            .collect();
        let config = ProdConfig {
            roots,
            wal_backend: WalBackend::Auto,
        };
        crucible_env_prod::build(tokio::runtime::Handle::current(), &config).unwrap()
    }

    /// One HTTP/1.1 exchange over the `Env` network; returns the status line, the lowercased headers and the
    /// body.
    async fn http(
        env: &Env,
        addr: SocketAddr,
        head: &str,
        body: &[u8],
    ) -> (String, String, Vec<u8>) {
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        let mut stream = env.net.connect(addr, deadline).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(response[..split].to_vec()).unwrap();
        let (status, headers) = head.split_once("\r\n").unwrap();
        (
            status.to_owned(),
            headers.to_ascii_lowercase(),
            response[split + 4..].to_vec(),
        )
    }

    async fn http_get(env: &Env, addr: SocketAddr, path: &str) -> (String, String, String) {
        let head = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        let (status, headers, body) = http(env, addr, &head, b"").await;
        (status, headers, String::from_utf8(body).unwrap())
    }

    /// `POST /v1/traces` with the test key, or without credentials when `authorization` is empty.
    async fn post_traces(
        env: &Env,
        addr: SocketAddr,
        authorization: &str,
        content_type: &str,
        body: &[u8],
    ) -> (String, String, Vec<u8>) {
        let len = body.len();
        let head = format!(
            "POST /v1/traces HTTP/1.1\r\nHost: test\r\nConnection: close\r\n{authorization}Content-Type: \
             {content_type}\r\nContent-Length: {len}\r\nUser-Agent: e2e-exporter/1.0\r\n\r\n"
        );
        http(env, addr, &head, body).await
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_and_metrics_are_served_and_shutdown_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_in(dir.path());
        let env = &prod.env;
        let app = open_app(env, StaticKeys::new(env.rng.as_ref(), Vec::new()).unwrap())
            .await
            .unwrap();
        let mut servers = start(env, Arc::clone(&prod.recorder), loopback(), app)
            .await
            .unwrap();
        assert_eq!(
            prod.spawner.task_names(),
            ["wal-committer", "http-api", "http-ingest", "http-metrics"]
        );
        let addrs = servers.addrs;

        let (status, _, body) = http_get(env, addrs.api, "/healthz").await;
        assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "ok"));
        let (status, _, body) = http_get(env, addrs.api, "/readyz").await;
        assert_eq!(
            (status.as_str(), body.as_str()),
            ("HTTP/1.1 200 OK", "ready")
        );
        let (status, _, _) = http_get(env, addrs.api, "/metrics").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        let (status, _, _) = http_get(env, addrs.metrics, "/healthz").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        let (status, _, _) = http_get(env, addrs.ingest, "/healthz").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");

        let (status, headers, body) = http_get(env, addrs.metrics, "/metrics").await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            headers.contains("content-type: text/plain; version=0.0.4"),
            "{headers}"
        );
        for line in [
            "# TYPE crucible_http_requests_total counter",
            "crucible_http_requests_total{listener=\"api\",status=\"200\"} 2",
            "crucible_http_requests_total{listener=\"api\",status=\"404\"} 1",
            "crucible_http_requests_total{listener=\"ingest\",status=\"404\"} 1",
            "crucible_http_requests_total{listener=\"metrics\",status=\"404\"} 1",
            "crucible_http_request_duration_microseconds_count{listener=\"api\"} 3",
            "crucible_wal_backend_info{requested=\"auto\",backend=\"portable\",fallback_reason=\"",
            "crucible_ingest_inflight_bytes 0",
            "crucible_wal_durable_lsn 0",
        ] {
            assert!(body.contains(line), "missing {line:?} in:\n{body}");
        }
        assert_eq!(
            prod.recorder
                .counter_value(&registry::HTTP_REQUESTS, &["metrics", "200"]),
            1
        );

        // Shutdown: the accept loops return on their own, well inside the grace period, and the WAL committer
        // follows once the routers have dropped their handles.
        let aborted = prod.spawner.close(Duration::from_secs(10)).await;
        assert_eq!(aborted, Vec::<&str>::new());
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.ingest.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        assert!(env.net.connect(addrs.api, deadline).await.is_err());
    }

    /// One OTLP/JSON span that starts at `start_nanos`.
    fn span_json(start_nanos: u64) -> String {
        format!(
            concat!(
                r#"{{"resourceSpans":[{{"resource":{{"attributes":[{{"key":"service.name","value":{{"stringValue":"e2e"}}}}]}},"#,
                r#""scopeSpans":[{{"scope":{{"name":"e2e"}},"spans":[{{"traceId":"5b8efff798038103d269b633813fc60c","#,
                r#""spanId":"eee19b7ec3c1b174","name":"tool call","kind":1,"startTimeUnixNano":"{start}","#,
                r#""endTimeUnixNano":"{end}"}}]}}]}}]}}"#
            ),
            start = start_nanos,
            end = start_nanos + 2_000_000,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ingest_is_served_on_both_listeners_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_in(dir.path());
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let app = open_app(env, keys).await.unwrap();
        let mut servers = start(env, Arc::clone(&prod.recorder), loopback(), app)
            .await
            .unwrap();
        let addrs = servers.addrs;

        let start_nanos = u64::try_from(env.clock.wall_now().micros()).unwrap() * 1_000;
        let json = span_json(start_nanos);
        let protobuf = crucible_otlp::transcode_json(json.as_bytes()).unwrap();
        let bearer = format!("Authorization: Bearer {}\r\n", secret());
        let protobuf_type = "application/x-protobuf";
        let (status, headers, body) =
            post_traces(env, addrs.ingest, &bearer, protobuf_type, &protobuf).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            headers.contains("content-type: application/x-protobuf"),
            "{headers}"
        );
        assert!(body.is_empty(), "a full success has an empty response");
        let (status, headers, body) =
            post_traces(env, addrs.api, &bearer, "application/json", json.as_bytes()).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            headers.contains("content-type: application/json"),
            "{headers}"
        );
        assert_eq!(body, b"{}");
        let (status, headers, _) =
            post_traces(env, addrs.ingest, "", protobuf_type, &protobuf).await;
        assert_eq!(status, "HTTP/1.1 401 Unauthorized");
        assert!(headers.contains("www-authenticate: bearer"), "{headers}");
        let (status, _, body) = http_get(env, addrs.api, "/readyz").await;
        assert_eq!(
            (status.as_str(), body.as_str()),
            ("HTTP/1.1 200 OK", "ready")
        );
        let recorder = &prod.recorder;
        assert_eq!(
            recorder.counter_value(&registry::INGEST_REQUESTS, &["accepted"]),
            2
        );
        assert_eq!(
            recorder.counter_value(&registry::INGEST_REQUESTS, &["unauthenticated"]),
            1
        );
        assert_eq!(
            recorder.counter_value(&registry::HTTP_REQUESTS, &["ingest", "200"]),
            1
        );
        assert_eq!(
            recorder.counter_value(&registry::HTTP_REQUESTS, &["api", "200"]),
            2
        );

        let aborted = prod.spawner.close(Duration::from_secs(10)).await;
        assert_eq!(aborted, Vec::<&str>::new());
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.ingest.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
        drop(prod);

        // A restart on the same directory finds both requests durable.
        let prod = prod_in(dir.path());
        let (wal, report) = Wal::open(&prod.env, WalConfig::default()).await.unwrap();
        assert_eq!(report.last_lsn, Lsn::new(2));
        let mut replay = wal.replay(Lsn::new(1));
        let mut origins = Vec::new();
        while let Some(frame) = replay.next().await.unwrap() {
            let payload = frame.payload().unwrap();
            let Ok(Record::IngestRequest(request)) =
                Record::decode(RecordType::IngestRequest, &payload)
            else {
                panic!("frame {} is not an ingest request", frame.lsn());
            };
            assert_eq!(request.project_id.to_string(), PROJECT);
            assert_eq!(
                (request.sdk_name, request.sdk_version),
                ("e2e-exporter", "1.0")
            );
            assert_eq!((request.span_count, request.rejected), (1, &[][..]));
            assert_eq!(request.otlp, protobuf.as_slice());
            origins.push(request.content_origin);
        }
        assert_eq!(origins, [ContentOrigin::Protobuf, ContentOrigin::Json]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn keys_files_are_parsed_strictly_and_never_echo_a_secret() {
        let prod =
            crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
                .unwrap();
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        assert_eq!(keys.len(), 1);
        let principal = keys.verify(&secret()).unwrap();
        assert_eq!(principal.scope.project_id().to_string(), PROJECT);
        assert!(crate::keys::parse("", env).unwrap().is_empty());

        let entry = "[[project_key]]\nproject_id = \"{project}\"\nsecret = \"{secret}\"\n";
        let refused = [
            (
                entry.replace("{project}", "nope"),
                "project_key 1: project_id",
            ),
            (
                entry.replace("{secret}", "csk_short"),
                "project_key 1: secret",
            ),
            (format!("{entry}note = \"x\"\n"), "unknown field"),
            (format!("{entry}{entry}"), "same secret"),
            (entry.replace("{secret}\"", "{secret}"), "not valid"),
            ("[[project_key]\n".to_owned(), "not valid"),
        ];
        for (text, expected) in refused {
            let text = text
                .replace("{project}", PROJECT)
                .replace("{secret}", &secret());
            let error = format!("{:#}", crate::keys::parse(&text, env).unwrap_err());
            assert!(error.contains(expected), "{error}");
            assert!(
                !error.contains(&secret()[4..]),
                "the error echoes the secret: {error}"
            );
        }
    }

    /// The manual end-to-end check: prints every frame of the WAL directory in `CRUCIBLE_WAL_DUMP_DIR` (the
    /// `wal` directory of a stopped server's `--data-dir`):
    ///
    /// `CRUCIBLE_WAL_DUMP_DIR=<data-dir>/wal cargo nextest run -p crucible dump_wal --run-ignored only --no-capture`
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "manual: reads the WAL directory named by CRUCIBLE_WAL_DUMP_DIR"]
    #[allow(clippy::disallowed_methods)] // The manual check takes its directory from the environment.
    async fn dump_wal() {
        let dir = std::env::var_os("CRUCIBLE_WAL_DUMP_DIR").expect("CRUCIBLE_WAL_DUMP_DIR is set");
        let config = ProdConfig {
            roots: BTreeMap::from([(Root::Wal, PathBuf::from(dir))]),
            wal_backend: WalBackend::Portable,
        };
        let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &config).unwrap();
        let (wal, report) = Wal::open(&prod.env, WalConfig::default()).await.unwrap();
        println!(
            "epoch {} last_lsn {} segments {:?}",
            report.epoch, report.last_lsn, report.segments
        );
        let mut replay = wal.replay(Lsn::new(1));
        while let Some(frame) = replay.next().await.unwrap() {
            let header = frame.header();
            println!(
                "lsn {} {:?} segment {} offset {} received_at {} compressed {} stored {} bytes",
                frame.lsn(),
                frame.record_type(),
                frame.segment_no(),
                frame.offset(),
                frame.received_at(),
                header.compressed,
                header.payload_len,
            );
            let payload = frame.payload().unwrap();
            if let Ok(Record::IngestRequest(r)) = Record::decode(frame.record_type(), &payload) {
                println!(
                    "  project {} key {} transport {:?} origin {:?} sdk {:?}/{:?} spans {} rejected {:?} otlp {} bytes",
                    r.project_id,
                    r.api_key_id,
                    r.transport,
                    r.content_origin,
                    r.sdk_name,
                    r.sdk_version,
                    r.span_count,
                    r.rejected,
                    r.otlp.len(),
                );
            }
        }
    }
}
