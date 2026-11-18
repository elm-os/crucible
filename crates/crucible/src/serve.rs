//! `crucible serve`: builds the production `Env` and runs the HTTP listeners until asked to stop.
//!
//! Both servers accept through `Env`'s `Net`, so the same code will run over the simulated network. Each
//! accept loop is a named task in the shutdown scope; axum runs one further tokio task per connection, and
//! graceful shutdown waits for those before the accept-loop task returns.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use crucible_env::metrics::registry;
use crucible_env::{
    AssetSource, Counter, Env, Histogram, JoinHandle, Listener, MemoryRecorder, NetError, Root,
    Stream,
};
use crucible_env_prod::ProdConfig;
use crucible_records::RequestId;
use crucible_ui_assets::{Catalog, Outcome};

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

/// What the UI fallback needs.
#[derive(Clone)]
struct Ui {
    env: Env,
    source: Arc<dyn AssetSource>,
}

/// Answers every request the routes do not match with the UI's try-files handler. Paths reserved for the
/// API, ingest, the MCP server and the health checks never fall back to HTML: they get a problem 404
/// (contract 12.1).
async fn serve_ui(State(ui): State<Ui>, request: Request) -> Response {
    let outcome = crucible_ui_assets::serve(
        ui.source.as_ref(),
        request.method(),
        request.uri(),
        request.headers(),
    );
    match outcome {
        Outcome::Response(response) => response.map(Body::from),
        Outcome::Reserved => not_found_problem(&ui.env),
    }
}

/// The `not_found` problem document (contract 12.3). Until the request-id middleware lands, the request id is
/// minted here, so only problem responses carry one.
fn not_found_problem(env: &Env) -> Response {
    let request_id = RequestId::from_uuid(env.new_id()).to_string();
    let body = format!(
        "{{\"type\":\"urn:crucible:problem:not_found\",\"title\":\"Not Found\",\"status\":404,\
         \"detail\":\"No resource exists at this path.\",\"code\":\"not_found\",\
         \"request_id\":\"{request_id}\"}}"
    );
    let mut response = (
        StatusCode::NOT_FOUND,
        [(CONTENT_TYPE, "application/problem+json")],
        body,
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// The main listener's routes; the UI answers whatever they do not match. Later waves add the API, ingest
/// and the readiness conditions (WAL writability and the key snapshot).
fn api_router(env: &Env, ui: Arc<dyn AssetSource>) -> Router {
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ready" }))
        .fallback(serve_ui)
        .with_state(Ui {
            env: env.clone(),
            source: ui,
        });
    observed(router, env, "api")
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

/// The running listeners.
struct Servers {
    api_addr: SocketAddr,
    metrics_addr: SocketAddr,
    api: JoinHandle<io::Result<()>>,
    metrics: JoinHandle<io::Result<()>>,
}

/// Binds both listeners and spawns their accept loops in the shutdown scope. Each loop stops accepting when
/// the scope's shutdown signal resolves and returns once its open connections have finished.
async fn start(
    env: &Env,
    recorder: Arc<MemoryRecorder>,
    ui: Arc<dyn AssetSource>,
    listen: SocketAddr,
    metrics_listen: SocketAddr,
) -> anyhow::Result<Servers> {
    let api = EnvListener::bind(env, "api", listen).await?;
    let metrics = EnvListener::bind(env, "metrics", metrics_listen).await?;
    let (api_addr, metrics_addr) = (api.inner.local_addr()?, metrics.inner.local_addr()?);

    let (api_router, signal) = (api_router(env, ui), env.shutdown());
    let api = env.spawn("http-api", async move {
        axum::serve(api, api_router)
            .with_graceful_shutdown(signal)
            .await
    });
    let (metrics_router, signal) = (metrics_router(env, recorder), env.shutdown());
    let metrics = env.spawn("http-metrics", async move {
        axum::serve(metrics, metrics_router)
            .with_graceful_shutdown(signal)
            .await
    });
    Ok(Servers {
        api_addr,
        metrics_addr,
        api,
        metrics,
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
    let mut roots: BTreeMap<_, _> = roots
        .into_iter()
        .map(|root| (root, args.data_dir.join(root.name())))
        .collect();
    if let Some(dir) = &args.ui_dir {
        // The disk backend creates a missing root; a mistyped `--ui-dir` must fail instead.
        anyhow::ensure!(
            dir.is_dir(),
            "--ui-dir {} is not a directory",
            dir.display()
        );
        roots.insert(Root::Ui, dir.clone());
    }
    let config = ProdConfig {
        roots,
        wal_backend: args.wal_backend,
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &config)
        .context("building the production environment")?;
    let env = &prod.env;
    env.metrics
        .gauge(&registry::BUILD_INFO, &[env!("CARGO_PKG_VERSION")])
        .set(1);

    let (ui, ui_source) = match &args.ui_dir {
        Some(dir) => {
            let catalog = Catalog::from_dir(env.disk.as_ref())
                .await
                .with_context(|| {
                    format!(
                        "loading the UI from --ui-dir {} (it must hold index.html)",
                        dir.display()
                    )
                })?;
            (catalog, "directory")
        }
        None if cfg!(feature = "embed-ui") => (Catalog::built_in(), "embedded"),
        None => (Catalog::built_in(), "none"),
    };
    let ui_routes = ui.manifest().routes.len();
    let mut servers = start(
        env,
        Arc::clone(&prod.recorder),
        Arc::new(ui),
        args.listen,
        args.metrics_listen,
    )
    .await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %servers.api_addr,
        metrics = %servers.metrics_addr,
        wal_backend = prod.wal_backend.backend.as_str(),
        ui = ui_source,
        ui_routes,
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
        result = servers.metrics.join() => Some(("metrics", result)),
    };

    let aborted = prod.spawner.close(SHUTDOWN_GRACE).await;
    if let Some((name, result)) = early {
        server_result(name, result)?;
        anyhow::bail!("the {name} server stopped without a shutdown request");
    }
    server_result("api", servers.api.join().await)?;
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

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    /// One HTTP/1.1 request over the `Env` network; returns the status line, the lowercased headers and the
    /// body.
    async fn http(
        env: &Env,
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (String, String, Vec<u8>) {
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        let mut stream = env.net.connect(addr, deadline).await.unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8(response[..end].to_vec()).unwrap();
        let (status, headers) = head.split_once("\r\n").unwrap();
        (
            status.to_owned(),
            headers.to_ascii_lowercase(),
            response[end + 4..].to_vec(),
        )
    }

    async fn http_get(env: &Env, addr: SocketAddr, path: &str) -> (String, String, String) {
        let (status, headers, body) = http(env, addr, "GET", path, &[]).await;
        (status, headers, String::from_utf8(body).unwrap())
    }

    /// The value of header `name` in the lowercased header block `headers`.
    fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
        headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then(|| value.trim())
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_and_metrics_are_served_and_shutdown_is_clean() {
        let prod =
            crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
                .unwrap();
        let env = &prod.env;
        let ui = Arc::new(Catalog::default());
        let mut servers = start(env, Arc::clone(&prod.recorder), ui, LOOPBACK, LOOPBACK)
            .await
            .unwrap();
        assert_eq!(prod.spawner.task_names(), ["http-api", "http-metrics"]);

        let (status, _, body) = http_get(env, servers.api_addr, "/healthz").await;
        assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "ok"));
        let (status, _, body) = http_get(env, servers.api_addr, "/readyz").await;
        assert_eq!(
            (status.as_str(), body.as_str()),
            ("HTTP/1.1 200 OK", "ready")
        );
        let (status, _, _) = http_get(env, servers.api_addr, "/metrics").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        let (status, _, _) = http_get(env, servers.metrics_addr, "/healthz").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");

        let (status, headers, body) = http_get(env, servers.metrics_addr, "/metrics").await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert!(
            headers.contains("content-type: text/plain; version=0.0.4"),
            "{headers}"
        );
        for line in [
            "# TYPE crucible_http_requests_total counter",
            "crucible_http_requests_total{listener=\"api\",status=\"200\"} 2",
            "crucible_http_requests_total{listener=\"api\",status=\"404\"} 1",
            "crucible_http_requests_total{listener=\"metrics\",status=\"404\"} 1",
            "crucible_http_request_duration_microseconds_count{listener=\"api\"} 3",
            "crucible_wal_backend_info{requested=\"auto\",backend=\"portable\",fallback_reason=\"",
        ] {
            assert!(body.contains(line), "missing {line:?} in:\n{body}");
        }
        assert_eq!(
            prod.recorder
                .counter_value(&registry::HTTP_REQUESTS, &["metrics", "200"]),
            1
        );

        // Shutdown: both accept loops return on their own, well inside the grace period.
        let aborted = prod.spawner.close(Duration::from_secs(10)).await;
        assert_eq!(aborted, Vec::<&str>::new());
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        assert!(env.net.connect(servers.api_addr, deadline).await.is_err());
    }

    /// Asserts a problem 404 whose request id matches `X-Request-Id`.
    fn assert_not_found_problem(status: &str, headers: &str, body: &[u8]) {
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        assert_eq!(
            header(headers, "content-type"),
            Some("application/problem+json")
        );
        let problem: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(problem["type"], "urn:crucible:problem:not_found");
        assert_eq!(problem["status"], 404);
        assert_eq!(problem["code"], "not_found");
        let request_id = problem["request_id"].as_str().unwrap();
        assert!(RequestId::parse(request_id).is_ok(), "{request_id}");
        assert_eq!(header(headers, "x-request-id"), Some(request_id));
    }

    /// `crucible serve --ui-dir` over the fixture export: the directory source, read through the production
    /// disk backend, behind the real HTTP stack.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_ui_directory_is_served_with_try_files() {
        const FIXTURE: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../crucible-ui-assets/tests/fixtures/out"
        );
        const TRACES: &[u8] =
            include_bytes!("../../crucible-ui-assets/tests/fixtures/out/traces/index.html");
        const TRACES_BR: &[u8] =
            include_bytes!("../../crucible-ui-assets/tests/fixtures/out/traces/index.html.br");
        let config = ProdConfig {
            roots: BTreeMap::from([(Root::Ui, FIXTURE.into())]),
            ..ProdConfig::default()
        };
        let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &config).unwrap();
        let env = &prod.env;
        let ui = Catalog::from_dir(env.disk.as_ref()).await.unwrap();
        assert_eq!(
            ui.manifest().routes,
            ["/", "/home/", "/mcp/servers/", "/traces/"]
        );
        let traces = ui.get("traces/index.html").unwrap();
        assert_eq!(traces.bytes, TRACES);
        assert_eq!(traces.encodings.br.as_deref(), Some(TRACES_BR));
        let mut servers = start(
            env,
            Arc::clone(&prod.recorder),
            Arc::new(ui),
            LOOPBACK,
            LOOPBACK,
        )
        .await
        .unwrap();
        let addr = servers.api_addr;

        let (status, headers, body) = http(env, addr, "GET", "/traces/", &[]).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(body, TRACES);
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/html; charset=utf-8")
        );
        assert_eq!(header(&headers, "cache-control"), Some("no-cache"));
        let policy = header(&headers, "content-security-policy").unwrap();
        assert_eq!(policy.matches("'sha256-").count(), 3, "{policy}");
        let etag = header(&headers, "etag").unwrap().to_owned();

        // HEAD through hyper: the length of the GET, no body.
        let (status, headers, body) = http(env, addr, "HEAD", "/traces/?project=p1", &[]).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(
            header(&headers, "content-length"),
            Some(TRACES.len().to_string().as_str())
        );
        assert!(body.is_empty());

        let (status, headers, body) = http(
            env,
            addr,
            "GET",
            "/traces/",
            &[("accept-encoding", "gzip, deflate, br, zstd")],
        )
        .await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(header(&headers, "content-encoding"), Some("br"));
        assert_eq!(header(&headers, "vary"), Some("accept-encoding"));
        assert_eq!(body, TRACES_BR);

        let (status, headers, body) =
            http(env, addr, "GET", "/traces/", &[("if-none-match", &etag)]).await;
        assert_eq!(status, "HTTP/1.1 304 Not Modified");
        assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
        assert!(body.is_empty());

        for (path, content_type) in [
            (
                "/traces/index.txt?project=p1&range=24h&_rsc=1ld0r",
                "text/plain; charset=utf-8",
            ),
            (
                "/traces/__next.!KHNoZWxsKQ.traces.__PAGE__.txt",
                "text/plain; charset=utf-8",
            ),
            (
                "/home/__next.!KHNoZWxsKQ.home.__PAGE__.txt",
                "text/plain; charset=utf-8",
            ),
            ("/mcp/servers/", "text/html; charset=utf-8"),
            (
                "/_next/static/chunks/0pnnrkjq50nr7.js",
                "text/javascript; charset=utf-8",
            ),
        ] {
            let (status, headers, _) = http(env, addr, "GET", path, &[]).await;
            assert_eq!(status, "HTTP/1.1 200 OK", "{path}");
            assert_eq!(
                header(&headers, "content-type"),
                Some(content_type),
                "{path}"
            );
        }
        let (_, headers, _) = http(
            env,
            addr,
            "GET",
            "/_next/static/chunks/0pnnrkjq50nr7.js",
            &[],
        )
        .await;
        assert_eq!(
            header(&headers, "cache-control"),
            Some("public, max-age=31536000, immutable")
        );

        let (status, headers, _) = http(env, addr, "GET", "/missing/", &[]).await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/html; charset=utf-8")
        );

        for (method, path) in [
            ("GET", "/api/v1/nope"),
            ("POST", "/v1/traces"),
            ("GET", "/mcp"),
        ] {
            let (status, headers, body) = http(env, addr, method, path, &[]).await;
            assert_not_found_problem(&status, &headers, &body);
        }
        let (status, _, body) = http_get(env, addr, "/healthz").await;
        assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "ok"));

        assert_eq!(
            prod.spawner.close(Duration::from_secs(10)).await,
            Vec::<&str>::new()
        );
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_ui_the_root_says_how_to_build_one() {
        let prod =
            crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
                .unwrap();
        let env = &prod.env;
        let ui = Arc::new(Catalog::default());
        let mut servers = start(env, Arc::clone(&prod.recorder), ui, LOOPBACK, LOOPBACK)
            .await
            .unwrap();

        let (status, headers, body) = http_get(env, servers.api_addr, "/").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert!(
            body.contains("--ui-dir") && body.contains("embed-ui"),
            "{body}"
        );
        let (status, headers, body) = http(env, servers.api_addr, "GET", "/api/v1/x", &[]).await;
        assert_not_found_problem(&status, &headers, &body);

        assert_eq!(
            prod.spawner.close(Duration::from_secs(10)).await,
            Vec::<&str>::new()
        );
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
    }
}
