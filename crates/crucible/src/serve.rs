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
use axum::extract::{Request, State};
use axum::http::header::CONTENT_TYPE;
use axum::middleware::{Next, from_fn_with_state};
use axum::response::Response;
use axum::routing::get;
use crucible_env::metrics::registry;
use crucible_env::{
    Counter, Env, Histogram, JoinHandle, Listener, MemoryRecorder, NetError, Root, Stream,
};
use crucible_env_prod::ProdConfig;

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

/// The main listener's routes. Later waves add the API, the UI, ingest and the readiness conditions (WAL
/// writability and the key snapshot).
fn api_router(env: &Env) -> Router {
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ready" }));
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
    listen: SocketAddr,
    metrics_listen: SocketAddr,
) -> anyhow::Result<Servers> {
    let api = EnvListener::bind(env, "api", listen).await?;
    let metrics = EnvListener::bind(env, "metrics", metrics_listen).await?;
    let (api_addr, metrics_addr) = (api.inner.local_addr()?, metrics.inner.local_addr()?);

    let (api_router, signal) = (api_router(env), env.shutdown());
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

    let mut servers = start(
        env,
        Arc::clone(&prod.recorder),
        args.listen,
        args.metrics_listen,
    )
    .await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %servers.api_addr,
        metrics = %servers.metrics_addr,
        wal_backend = prod.wal_backend.backend.as_str(),
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

    /// One HTTP/1.1 request over the `Env` network; returns the status line, the headers and the body.
    async fn http_get(env: &Env, addr: SocketAddr, path: &str) -> (String, String, String) {
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        let mut stream = env.net.connect(addr, deadline).await.unwrap();
        let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let (status, headers) = head.split_once("\r\n").unwrap();
        (
            status.to_owned(),
            headers.to_ascii_lowercase(),
            body.to_owned(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_and_metrics_are_served_and_shutdown_is_clean() {
        let prod =
            crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
                .unwrap();
        let env = &prod.env;
        let mut servers = start(env, Arc::clone(&prod.recorder), LOOPBACK, LOOPBACK)
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
}
