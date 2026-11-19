//! `crucible serve`: builds the production `Env`, opens the WAL, and runs the listeners until asked to stop.
//!
//! The servers accept through `Env`'s `Net`, so the same code will run over the simulated network. Each accept
//! loop is a named task in the shutdown scope; axum runs one further tokio task per connection, and graceful
//! shutdown waits for those before the accept-loop task returns. The gRPC listener is an axum server too: it
//! speaks HTTP/2 without TLS, as OTLP exporters do on `:4317`.
//!
//! Both HTTP ingest listeners serve `/v1/traces` and the compat ingest routes of `crucible_compat::routes`;
//! paths that name no route there answer problem 404 and never fall back to HTML (contract 12.1).
//!
//! Two modes:
//!
//! - **Ingest only** (no `--clickhouse-url`): requests are made durable in the WAL and nothing ships them; the
//!   API and the MCP server authenticate each request and answer `telemetry_store_unavailable`.
//! - **With ClickHouse**: the `telemetry-store` task brings the store up ([`store::bring_up`]: migrations when
//!   asked for, startup detection and the self-tests of contract 5.8), retrying until it succeeds. Until then
//!   ingest answers 503 and `/readyz` says why. Then it builds the recent-span index, mounts the API and the MCP
//!   server over the store, opens ingest, and runs the shipper, whose replay of the retained WAL prefix
//!   rebuilds the index.
//!
//! Shutdown is one signal to every task. The listeners stop accepting and wait for their open requests, whose
//! appends resolve because the WAL committer outlives every handle to the log. The shipper stops at its next
//! step and its in-flight inserts are abandoned: they are in doubt, and the next start probes every pair of
//! every sealed batch before it sends anything (contract 5.5).

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use crucible_api::StaticProjectSettings;
use crucible_api_types::ProblemCode;
use crucible_auth::service::Identity;
use crucible_auth::{DbKeys, KeyVerifier, Sealer, StaticKeys};
use crucible_config::Config;
use crucible_control::KeyScope;
use crucible_cursor::KeyRing;
use crucible_env::metrics::registry;
use crucible_env::{
    AssetSource, Counter, DiskPath, Env, Histogram, JoinHandle, Listener, MemoryRecorder, NetError,
    OpenMode, Root, Stores, Stream,
};
use crucible_env_prod::ProdConfig;
use crucible_export::{ExportRuntime, Exports};
use crucible_ingest::{CompatRoute, Ingest, IngestConfig, RoutingHeader};
use crucible_mcp_server::McpConfig;
use crucible_normalize::{NormCtx, PriceTable};
use crucible_query::QueryService;
use crucible_recent::{RecentConfig, RecentIndex};
use crucible_records::{Lsn, PartitionId, ProjectId, TableId};
use crucible_schema::SqlProfile;
use crucible_shipper::{
    FrameManifest, Hooks, MANIFEST_FILE, NoBarriers, NoSalts, ProjectSalts, RecentHook, Shipper,
    ShipperConfig,
};
use crucible_sqlws::SqlWorkspace;
use crucible_ui_assets::{Catalog, Outcome};
use crucible_wal::{RecoveryReport, Wal, WalConfig};
use tower::ServiceExt as _;

use crate::control::Control;
use crate::mcp::McpGate;
use crate::recent::WalSpans;
use crate::salts::StoreSalts;
use crate::store::{self, ClickHouse};

/// How long tasks get to finish after shutdown is requested before they are aborted.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(30);
/// Pause after an accept failure that is not about one connection (for example, out of descriptors).
const ACCEPT_BACKOFF: Duration = Duration::from_secs(1);
/// The first and the longest pause between attempts to bring the telemetry store up.
const STORE_RETRY: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(15));
/// `Retry-After` of an ingest request refused while the store is not up, in seconds.
const CLOSED_RETRY_AFTER: u32 = 5;
/// `recent.max_bytes` when the memory of the process cannot be read (contract 8.6 puts the default at 10% of it).
const RECENT_FALLBACK_BYTES: u64 = 256 << 20;
/// Startup warns when the memory budgets add up to more than this share of the memory the process may use, in
/// percent (default; revisit). The rest is for the memory no budget tracks.
const BUDGET_SHARE_PERCENT: u64 = 90;

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

/// A problem 404 for a path that no route serves.
fn not_found(env: &Env, request: &Request) -> Response {
    let request_id = crucible_api::request_id(request.extensions(), env);
    crucible_api::problem(
        request_id,
        ProblemCode::NotFound,
        "no resource exists at this path",
    )
}

/// Answers every request the routes do not match with the UI's try-files handler. Paths reserved for the
/// API, ingest, the MCP server and the health checks never fall back to HTML, and neither do paths under a
/// compat ingest route's first segment: they get a problem 404 (contract 12.1).
async fn serve_ui(State(ui): State<Ui>, request: Request) -> Response {
    if crucible_compat::routes::under_route_prefix(request.uri().path()) {
        return not_found(&ui.env, &request);
    }
    let outcome = crucible_ui_assets::serve(
        ui.source.as_ref(),
        request.method(),
        request.uri(),
        request.headers(),
    );
    match outcome {
        Outcome::Response(response) => response.map(Body::from),
        Outcome::Reserved => not_found(&ui.env, &request),
    }
}

/// Whether the node takes ingest, and which API it serves. Without a telemetry store ingest is open from the
/// start and the API never leaves its store-less form.
struct Gate {
    /// Why ingest is closed; `None` when it is open.
    closed: RwLock<Option<String>>,
    /// The API over the store, set once the store is up.
    api: OnceLock<Router>,
    /// The API until then.
    api_without_store: Router,
    /// The Postgres key snapshot, with a control plane: until it loads the node is not ready and ingest stays
    /// closed (contract 11.8: a cold start without Postgres accepts nothing).
    snapshot: Option<Arc<DbKeys>>,
    /// The MCP server at `/mcp`, mounted over the store with the API.
    mcp: Arc<McpGate>,
}

/// The keys of the MCP server. Its tools read, so with a control plane a key needs the `read` scope (contract
/// 11.3), as ingest needs `ingest`; keys from the keys file carry no scopes to check.
fn mcp_keys(identity: Option<&Arc<Identity>>, keys: &Arc<dyn KeyVerifier>) -> Arc<dyn KeyVerifier> {
    match identity {
        Some(identity) => Arc::new(identity.keys().with_scope(KeyScope::Read)),
        None => Arc::clone(keys),
    }
}

impl Gate {
    fn closed(&self) -> Option<String> {
        let closed = self
            .closed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        closed.or_else(|| {
            self.snapshot
                .as_ref()
                .filter(|keys| !keys.is_loaded())
                .map(|_| {
                    "the API key snapshot is not loaded yet: Postgres has not answered, or its migrations have \
                     not run (`--migrate` and `crucible migrate` apply them); the log has the error"
                        .to_owned()
                })
        })
    }

    fn close(&self, reason: String) {
        *self.closed.write().unwrap_or_else(PoisonError::into_inner) = Some(reason);
    }

    /// Mounts the API over the store and opens ingest.
    fn open(&self, api: Router) {
        // The store task is the only caller, once.
        let _ = self.api.set(api);
        *self.closed.write().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Refuses ingest with 503 while the gate is closed (contract 5.8).
async fn ingest_gate(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    match gate.closed() {
        None => next.run(request).await,
        Some(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(RETRY_AFTER, HeaderValue::from(CLOSED_RETRY_AFTER))],
            format!("ingest is closed: {reason}\n"),
        )
            .into_response(),
    }
}

/// Refuses gRPC ingest with `UNAVAILABLE` and a `RetryInfo` while the gate is closed (contract 5.8).
async fn grpc_gate(State(gate): State<Arc<Gate>>, request: Request, next: Next) -> Response {
    match gate.closed() {
        None => next.run(request).await,
        Some(reason) => crucible_ingest::grpc_unavailable(
            request.headers(),
            CLOSED_RETRY_AFTER,
            &format!("ingest is closed: {reason}"),
        ),
    }
}

/// Hands an API request to the API over the store once there is one.
async fn serve_api(State(gate): State<Arc<Gate>>, request: Request) -> Response {
    let api = gate.api.get().unwrap_or(&gate.api_without_store).clone();
    match api.oneshot(request).await {
        Ok(response) => response,
        Err(never) => match never {},
    }
}

/// What the routers serve besides the UI: OTLP ingest, the WAL behind it, the keys that ingest and the API
/// accept, and the gate in front of both.
struct App {
    ingest: Ingest,
    wal: Wal,
    keys: Arc<dyn KeyVerifier>,
    gate: Arc<Gate>,
}

/// The ingest settings: the defaults, with the compat ingest routes and routing headers of the compat registry
/// (contract 12.1).
fn ingest_config() -> IngestConfig {
    let compat_routes = crucible_compat::routes::INGEST_ROUTES
        .iter()
        .map(|route| CompatRoute {
            path: route.path,
            dialect: route.dialect,
            sdk_headers: route.sdk_headers.map(|sdk| (sdk.name, sdk.version)),
        })
        .collect();
    let routing_headers = crucible_compat::routes::ROUTING_HEADERS
        .iter()
        .map(|header| RoutingHeader {
            name: header.name,
            dialect: header.dialect,
        })
        .collect();
    IngestConfig {
        compat_routes,
        routing_headers,
        ..IngestConfig::default()
    }
}

/// Opens the WAL under the `wal` root, recovering what is there (contract 4.6), and builds ingest over it.
/// With `closed`, ingest starts closed for that reason. `cursors` signs and opens the API's cursors.
async fn open_app(
    env: &Env,
    keys: StaticKeys,
    cursors: Arc<KeyRing>,
    config: WalConfig,
    identity: Option<Arc<Identity>>,
    closed: Option<&str>,
    exports: Option<Exports>,
) -> anyhow::Result<(App, RecoveryReport)> {
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
    // With a control plane the keys are the Postgres snapshot; the keys file is the mode without one.
    let keys: Arc<dyn KeyVerifier> = match &identity {
        Some(identity) => Arc::clone(identity.keys()) as Arc<dyn KeyVerifier>,
        None => Arc::new(keys),
    };
    let ingest = Ingest::new(
        env,
        wal.clone(),
        &config,
        Arc::clone(&keys),
        ingest_config(),
    );
    let settings = Arc::new(StaticProjectSettings::default());
    let gate = Arc::new(Gate {
        closed: RwLock::new(closed.map(str::to_owned)),
        api: OnceLock::new(),
        snapshot: identity
            .as_ref()
            .map(|identity| Arc::clone(identity.keys())),
        mcp: Arc::new(McpGate::new(env, mcp_keys(identity.as_ref(), &keys))),
        api_without_store: match identity {
            Some(identity) => {
                crucible_api::platform_router(env, None, identity, settings, cursors, None, exports)
            }
            None => crucible_api::router_without_store(env, Arc::clone(&keys), settings, cursors),
        },
    });
    let app = App {
        ingest,
        wal,
        keys,
        gate,
    };
    Ok((app, report))
}

/// Readiness (contract 11.8): the WAL accepts writes and, with a telemetry store, the startup checks of
/// contract 5.8 have passed. The keys of milestone 1 come from configuration, so they are always loaded.
fn readiness(wal: &Wal, gate: &Gate) -> (StatusCode, String) {
    if wal.admission().failed {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "not ready: the wal has failed".to_owned(),
        );
    }
    match gate.closed() {
        Some(reason) => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("not ready: {reason}"),
        ),
        None => (StatusCode::OK, "ready".to_owned()),
    }
}

/// OTLP ingest behind the gate.
fn gated_ingest(app: &App) -> Router {
    app.ingest
        .router()
        .layer(from_fn_with_state(Arc::clone(&app.gate), ingest_gate))
}

/// The main listener's routes: health, readiness, OTLP ingest, the REST API and the MCP server; the UI answers
/// whatever they do not match. Every response carries a request id and the security headers.
///
/// Until a telemetry store is up the API authenticates and validates each request and then answers
/// `telemetry_store_unavailable`, and so does the MCP server; the gate then switches both to their forms over
/// the store.
fn api_router(env: &Env, app: &App, ui: Arc<dyn AssetSource>) -> Router {
    let (wal, gate) = (app.wal.clone(), Arc::clone(&app.gate));
    let api_path = format!("{}/{{*rest}}", crucible_api::BASE_PATH);
    let api = Router::new()
        .route(crucible_api::BASE_PATH, any(serve_api))
        .route(&api_path, any(serve_api))
        .with_state(Arc::clone(&app.gate));
    let mcp = Router::new()
        .route(crucible_mcp_server::PATH, any(crate::mcp::serve))
        .with_state(Arc::clone(&app.gate.mcp));
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/readyz",
            get(move || {
                let ready = readiness(&wal, &gate);
                async move { ready }
            }),
        )
        .fallback(serve_ui)
        .with_state(Ui {
            env: env.clone(),
            source: ui,
        })
        .merge(gated_ingest(app))
        .merge(api)
        .merge(mcp);
    crucible_api::common_layers(observed(router, env, "api"), env)
}

/// The OTLP/HTTP listener's routes: OTLP and compat ingest. Every other path answers problem 404, and every
/// response carries a request id.
fn ingest_router(env: &Env, app: &App) -> Router {
    let fallback_env = env.clone();
    let router = gated_ingest(app).fallback(move |request: Request| {
        let env = fallback_env.clone();
        async move { not_found(&env, &request) }
    });
    observed(crucible_api::common_layers(router, env), env, "ingest")
}

/// The OTLP/gRPC listener's routes, behind the gate.
fn grpc_router(env: &Env, app: &App) -> Router {
    let router = app
        .ingest
        .grpc_router()
        .layer(from_fn_with_state(Arc::clone(&app.gate), grpc_gate));
    observed(router, env, "grpc")
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
    grpc: SocketAddr,
    metrics: SocketAddr,
}

/// The running listeners.
struct Servers {
    addrs: Addrs,
    api: JoinHandle<io::Result<()>>,
    ingest: JoinHandle<io::Result<()>>,
    grpc: JoinHandle<io::Result<()>>,
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

/// Binds the four listeners and spawns their accept loops. The routers take over `app`.
async fn start(
    env: &Env,
    recorder: Arc<MemoryRecorder>,
    ui: Arc<dyn AssetSource>,
    listen: Addrs,
    app: App,
) -> anyhow::Result<Servers> {
    let api = EnvListener::bind(env, "api", listen.api).await?;
    let ingest = EnvListener::bind(env, "ingest", listen.ingest).await?;
    let grpc = EnvListener::bind(env, "grpc", listen.grpc).await?;
    let metrics = EnvListener::bind(env, "metrics", listen.metrics).await?;
    let addrs = Addrs {
        api: api.inner.local_addr()?,
        ingest: ingest.inner.local_addr()?,
        grpc: grpc.inner.local_addr()?,
        metrics: metrics.inner.local_addr()?,
    };
    let (api_router, ingest_router) = (api_router(env, &app, ui), ingest_router(env, &app));
    let grpc_router = grpc_router(env, &app);
    drop(app);
    Ok(Servers {
        addrs,
        api: serve(env, "http-api", api, api_router),
        ingest: serve(env, "http-ingest", ingest, ingest_router),
        grpc: serve(env, "grpc-ingest", grpc, grpc_router),
        metrics: serve(env, "http-metrics", metrics, metrics_router(env, recorder)),
    })
}

/// What the `telemetry-store` task takes over.
struct StoreTask {
    config: ClickHouse,
    /// `--migrate`: apply the migrations before the startup checks.
    migrate: bool,
    /// The control plane, when Postgres is configured.
    control: Option<Control>,
    wal: Wal,
    report: RecoveryReport,
    keys: Arc<dyn KeyVerifier>,
    cursors: Arc<KeyRing>,
    gate: Arc<Gate>,
    recent_max_bytes: u64,
    /// Identity, with a control plane.
    identity: Option<Arc<Identity>>,
    /// Where the API's fact writes go: this node's ingest.
    facts: Arc<dyn crucible_api::FactWriter>,
    /// The MCP server's rate limit.
    mcp: McpConfig,
    /// What the export job reads with, given the query service once the store is up.
    exports: ExportRuntime,
    /// The API's exports, with a control plane.
    export_service: Option<Exports>,
}

/// The pairs that were quarantined at or below `watermark`, from the manifests under the quarantine root. The
/// `Quarantine` frames of such pairs may be gone with their segments, and a read that could touch one must
/// still answer `partial` (invariant I3). A directory without a whole manifest is a copy that never finished.
///
/// The second list holds the frames that were quarantined as frames (contract 5.6): `(lsn, project, facts)`.
/// Stage B marks the ones the retained log still holds again when it replays them; the manifests cover the
/// rest.
async fn quarantined_pairs(
    env: &Env,
    watermark: Lsn,
) -> anyhow::Result<(Vec<(Lsn, TableId, PartitionId)>, Vec<UnshippableFrame>)> {
    let root = DiskPath::root_dir(Root::Quarantine);
    let mut pairs = Vec::new();
    let mut frames = Vec::new();
    for dir in env.disk.list(&root).await? {
        let dir = root.join(&dir.name)?;
        let Some(manifest) = env
            .disk
            .list(&dir)
            .await
            .ok()
            .and_then(|entries| entries.into_iter().find(|e| e.name == MANIFEST_FILE))
        else {
            continue;
        };
        let file = env
            .disk
            .open(&dir.join(MANIFEST_FILE)?, OpenMode::Read)
            .await?;
        let len = usize::try_from(manifest.len).unwrap_or(usize::MAX);
        let bytes = file.read_at(0, len).await?;
        let text = String::from_utf8_lossy(&bytes);
        if let Some(frame) = FrameManifest::parse(&text) {
            if frame.lsn <= watermark {
                frames.push((frame.lsn, frame.project_id, frame.facts));
            }
            continue;
        }
        let field = |name: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
        };
        let lsn = |name| field(name)?.parse().ok().map(Lsn::new);
        let table = field("table_id")
            .and_then(|id| id.parse().ok())
            .and_then(TableId::from_id);
        let partition = field("partition").and_then(|p| PartitionId::parse(p).ok());
        // `frames` is the last line a whole manifest has but one.
        if let (Some(first), Some(last), Some(table), Some(partition), Some(_)) = (
            lsn("first_lsn"),
            lsn("last_lsn"),
            table,
            partition,
            field("frames"),
        ) && last <= watermark
        {
            pairs.push((first, table, partition));
        }
    }
    Ok((pairs, frames))
}

/// A frame quarantined as a frame: its LSN, its project when known, and whether it is a `FactBatch`.
type UnshippableFrame = (Lsn, Option<ProjectId>, bool);

/// Brings the telemetry store up, retrying until it holds, then opens ingest and the API and runs the shipper.
/// Returns on shutdown, or with the error that stopped the shipper.
async fn run_store(env: Env, task: StoreTask) -> anyhow::Result<()> {
    let StoreTask {
        config,
        migrate,
        control,
        wal,
        report,
        keys,
        cursors,
        gate,
        recent_max_bytes,
        identity,
        facts: fact_writer,
        mcp,
        exports,
        export_service,
    } = task;
    let migration = migrate.then_some(store::Migrate(control.as_ref()));
    let store = config.store(&env);
    let mut shutdown = env.shutdown();
    let mut delay = STORE_RETRY.0;
    let facts = loop {
        let attempt = tokio::select! {
            biased;
            () = &mut shutdown => return Ok(()),
            attempt = store::bring_up(&env, &config, &store, migration.as_ref()) => attempt,
        };
        match attempt {
            Ok(facts) => break facts,
            Err(error) => {
                let reason = format!("{error:#}");
                tracing::warn!(reason, retry_in = ?delay, "the telemetry store is not ready; ingest stays closed");
                gate.close(format!("the telemetry store is not ready: {reason}"));
            }
        }
        tokio::select! {
            biased;
            () = &mut shutdown => return Ok(()),
            () = env.clock.sleep(delay) => {}
        }
        delay = (delay * 2).min(STORE_RETRY.1);
    };
    tracing::info!(
        version = format!("{}.{}", facts.version.major, facts.version.minor),
        flavour = ?facts.flavour,
        idle_timeout = ?store.idle_timeout(),
        "the telemetry store passed startup detection and the self-tests"
    );

    let shipper_config = ShipperConfig {
        flavour: facts.flavour,
        ..ShipperConfig::default()
    };
    let view = Arc::new(WalSpans {
        wal: wal.clone(),
        ctx: NormCtx::new(
            PriceTable::bundled().context("loading the bundled price table")?,
            shipper_config.caps,
        ),
    });
    // The index starts empty at the recovered watermark; the shipper's replay of the retained prefix feeds
    // it through the hook (contract 4.6, steps 6 and 7).
    let watermark = report.watermark_lsn();
    let (index, writer) = RecentIndex::rebuild(
        RecentConfig::new(recent_max_bytes, facts.flavour),
        Arc::clone(&env.clock),
        view,
        watermark,
        std::iter::empty(),
    );
    let hook = Arc::new(RecentHook::new(writer, Arc::clone(&env.clock), watermark));
    let (pairs, frames) = quarantined_pairs(&env, watermark)
        .await
        .context("reading the quarantine manifests")?;
    hook.restore_quarantined(pairs);
    hook.restore_unshippable(frames);
    // The SQL workspace needs the control plane: its identities, saved queries and audit rows live there.
    let sql = match &control {
        Some(control) => {
            let sql_store = Arc::new(config.sql_store(&store)?);
            // A server upgrade can add settings the workspace's profile should pin, and the operator can change
            // `sql.statements_per_hour`; the ledger would not apply access step A2 again, so the profile and the
            // quota are brought up to date at every start.
            let profile = async {
                let profile = sql_store.server_profile(SqlProfile::default()).await?;
                sql_store.apply_profile(&profile).await?;
                sql_store.apply_quota().await
            };
            if let Err(error) = profile.await {
                tracing::warn!(%error, "the SQL workspace's settings profile and quota were not updated; run with --migrate");
            }
            let workspace = SqlWorkspace::new(
                env.clone(),
                sql_store.clone(),
                Arc::new(control.store.clone()),
                true,
            );
            // Dropping a handle detaches its task; both loops end with the host's shutdown scope.
            drop(env.spawn("sql-probe", workspace.clone().run_probe_loop()));
            drop(env.spawn(
                "sql-reconciler",
                workspace.clone().run_reconciler(report.epoch.to_string()),
            ));
            Some((workspace, sql_store))
        }
        None => None,
    };
    let stores = Stores::new(Arc::new(store));
    // With a control plane the project salts are its `project_salts` rows; without one stage B drops the user
    // identity of MCP operations (contract 9.3).
    let (stores, salts): (_, Arc<dyn ProjectSalts>) = match control {
        Some(control) => {
            let control = Arc::new(control.store);
            let sealer = Sealer::new("master-0", config.master_key.seal_key());
            let salts = StoreSalts::new(&env, control.clone(), sealer);
            (stores.with_control(control), Arc::new(salts))
        }
        None => (stores, Arc::new(NoSalts)),
    };
    let (sql, stores) = match sql {
        Some((workspace, sql_store)) => (Some(workspace), stores.with_sql(sql_store)),
        None => (None, stores),
    };
    let hooks = Hooks {
        barriers: Arc::new(NoBarriers),
        recent: hook,
        salts,
    };
    let shipper = Shipper::new(&env, &stores, wal, &report, shipper_config, hooks)
        .context("starting the shipper")?;
    let service = QueryService::new(stores, Arc::clone(&env.clock), index, report.epoch);
    exports.set_service(service.clone());
    let (mcp_keys, mcp_cursors) = (mcp_keys(identity.as_ref(), &keys), Arc::clone(&cursors));
    gate.mcp.open(
        &env,
        service.clone(),
        mcp_keys,
        mcp_cursors,
        mcp,
        export_service.clone(),
    );
    let settings = Arc::new(StaticProjectSettings::default());
    gate.open(match identity {
        Some(identity) => crucible_api::platform_router(
            &env,
            Some((service, fact_writer)),
            identity,
            settings,
            cursors,
            sql,
            export_service,
        ),
        None => crucible_api::router(&env, service, keys, settings, cursors, fact_writer),
    });
    tracing::info!(
        watermark = %watermark,
        last_lsn = %report.last_lsn,
        resealed = report.unchecked_seals.len(),
        "ingest is open; the shipper is running"
    );
    shipper.run().await.context("the shipper stopped")
}

/// The memory this process may use, from which the memory-derived defaults are sized.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Memory {
    /// The host's total memory: `MemTotal` in `/proc/meminfo`.
    Host(u64),
    /// The container's memory limit, `/sys/fs/cgroup/memory.max`, where it is below the host's total.
    Container(u64),
}

impl Memory {
    /// From the texts of `/proc/meminfo` and `/sys/fs/cgroup/memory.max`, each `None` when it cannot be read. A
    /// limit of `max`, or no limit file (Windows, macOS, cgroup v1), leaves the host's total.
    fn from_files(meminfo: Option<&str>, memory_max: Option<&str>) -> Option<Self> {
        let host = meminfo
            .and_then(|text| {
                let line = text.lines().find(|line| line.starts_with("MemTotal:"))?;
                line.split_whitespace().nth(1)?.parse::<u64>().ok()
            })
            .map(|kib| kib.saturating_mul(1024));
        let limit = memory_max.and_then(|text| text.trim().parse::<u64>().ok());
        match (host, limit) {
            (Some(host), Some(limit)) if limit >= host => Some(Self::Host(host)),
            (_, Some(limit)) => Some(Self::Container(limit)),
            (host, None) => host.map(Self::Host),
        }
    }

    const fn bytes(self) -> u64 {
        match self {
            Self::Host(bytes) | Self::Container(bytes) => bytes,
        }
    }
}

/// The memory this process may use: the host's total, or the container's limit when that is smaller.
#[allow(clippy::disallowed_methods)] // The binary reads the host's memory size and the container's limit.
fn read_memory() -> Option<Memory> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    Memory::from_files(
        read("/proc/meminfo").as_deref(),
        read("/sys/fs/cgroup/memory.max").as_deref(),
    )
}

/// `recent.max_bytes` (contract 8.6): min(1 GiB, 10% of the memory this process may use).
fn default_recent_max_bytes(memory: Option<Memory>) -> u64 {
    memory.map_or(RECENT_FALLBACK_BYTES, |memory| {
        RecentConfig::default_max_bytes(memory.bytes())
    })
}

/// One memory budget of the node: its setting, its size, and the flag and variable that change it (`None` for a
/// budget that has no setting yet).
#[derive(Clone, Copy, Debug)]
struct Budget {
    setting: &'static str,
    bytes: u64,
    changed_by: Option<&'static str>,
}

/// Whole MiB, rounded up.
fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes.div_ceil(1 << 20))
}

/// The sum of the budgets' sizes.
fn sum<'a>(budgets: impl IntoIterator<Item = &'a Budget>) -> u64 {
    budgets
        .into_iter()
        .fold(0u64, |total, budget| total.saturating_add(budget.bytes))
}

/// The warning for budgets that add up to more than [`BUDGET_SHARE_PERCENT`] of `memory`, naming each budget and
/// what changes it (contract 1.7); `None` when they fit.
fn budget_warning(memory: Memory, budgets: &[Budget]) -> Option<String> {
    let limit = memory.bytes();
    let total = sum(budgets);
    if total.saturating_mul(100) <= limit.saturating_mul(BUDGET_SHARE_PERCENT) {
        return None;
    }
    let needed = total.saturating_mul(100).div_ceil(BUDGET_SHARE_PERCENT);
    let source = match memory {
        Memory::Host(_) => "the host's total memory",
        Memory::Container(_) => "the container's memory limit",
    };
    let sizes: Vec<String> = budgets
        .iter()
        .map(|budget| format!("{} {}", budget.setting, mib(budget.bytes)))
        .collect();
    let settable: Vec<String> = budgets
        .iter()
        .filter_map(|budget| Some(format!("{} ({})", budget.setting, budget.changed_by?)))
        .collect();
    let fixed: Vec<&Budget> = budgets
        .iter()
        .filter(|budget| budget.changed_by.is_none())
        .collect();
    let mut advice = format!("Give the process at least {} of memory", mib(needed));
    // Lowering the budgets that have a setting helps only when the others fit the share by themselves.
    let share = limit.saturating_mul(BUDGET_SHARE_PERCENT) / 100;
    if let Some(room) = share.checked_sub(sum(fixed.iter().copied()))
        && !settable.is_empty()
    {
        let together = if settable.len() > 1 { " together" } else { "" };
        advice.push_str(&format!(
            ", or lower {} to at most {} MiB{together}",
            settable.join(" and "),
            room >> 20
        ));
    }
    if !fixed.is_empty() {
        let names: Vec<&str> = fixed.iter().map(|budget| budget.setting).collect();
        let verb = if fixed.len() > 1 { "have" } else { "has" };
        advice.push_str(&format!("; {} {verb} no setting yet", names.join(" and ")));
    }
    Some(format!(
        "the memory budgets add up to {}, more than {BUDGET_SHARE_PERCENT}% of the {} this process may use \
         ({source}): {}. {advice}",
        mib(total),
        mib(limit),
        sizes.join(", "),
    ))
}

/// The ClickHouse settings of the configuration, or `None` for the ingest-only mode.
fn clickhouse(config: &Config, migrate: bool) -> anyhow::Result<Option<ClickHouse>> {
    let Some(clickhouse) = &config.clickhouse else {
        anyhow::ensure!(
            !migrate,
            "--migrate needs a telemetry store (--clickhouse-url); `crucible migrate` migrates Postgres alone"
        );
        return Ok(None);
    };
    ClickHouse::new(clickhouse, config.sql).map(Some)
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

pub(crate) async fn run(config: Config, migrate: bool) -> anyhow::Result<()> {
    let roots = [Root::Wal, Root::Quarantine, Root::Exports];
    let mut roots: BTreeMap<_, _> = roots
        .into_iter()
        .map(|root| (root, config.data.dir.join(root.name())))
        .collect();
    let ui_dir = config.server.ui_dir.as_ref();
    if let Some(dir) = ui_dir {
        // The disk backend creates a missing root; a mistyped `--ui-dir` must fail instead.
        anyhow::ensure!(
            dir.is_dir(),
            "--ui-dir {} is not a directory",
            dir.display()
        );
        roots.insert(Root::Ui, dir.clone());
    }
    let prod_config = ProdConfig {
        roots,
        wal_backend: crate::wal_backend(config.data.wal_backend),
    };
    let prod = crucible_env_prod::build(tokio::runtime::Handle::current(), &prod_config)
        .context("building the production environment")?;
    let env = &prod.env;
    env.metrics
        .gauge(&registry::BUILD_INFO, &[env!("CARGO_PKG_VERSION")])
        .set(1);

    let (ui, ui_source) = match ui_dir {
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

    let keys = match &config.server.keys_file {
        Some(path) => crate::keys::load(path, env)?,
        None => StaticKeys::new(env.rng.as_ref(), Vec::new())?,
    };
    if keys.is_empty() && config.postgres.is_none() {
        tracing::warn!(
            "no project keys are configured (--keys-file); ingest refuses every request"
        );
    }
    let key_count = keys.len();
    let clickhouse = clickhouse(&config, migrate)?;
    let control = match &config.postgres {
        Some(postgres) => Some(Control::new(env, postgres)?),
        None => None,
    };
    // With a control plane, identity serves sessions and the Postgres keys. Until its tables exist and the key
    // snapshot has loaded, the node is not ready and ingest stays closed; listeners still bind.
    let identity = match &control {
        Some(control) => {
            let identity = crate::identity::build(env, &config, control)?;
            crate::identity::start(env, &identity, control, migrate);
            Some(identity)
        }
        None => None,
    };
    let control_mode = if control.is_some() {
        "postgres"
    } else {
        "none"
    };
    // The background jobs (contract 11.7). They need Postgres, not ClickHouse, and readiness does not wait
    // for them.
    // The export job's reads come from the telemetry store task once it runs. Only that task and the job tasks
    // hold the runtime: its reads hold the WAL through the recent index, so a holder that outlived them would
    // keep the WAL committer from stopping.
    let exports = ExportRuntime::new();
    let jobs = crate::jobs::start(env, control.as_ref(), &exports);
    let cursors = Arc::new(match &clickhouse {
        Some(config) => crate::cursor_keys::derived(&config.master_key)?,
        None => crate::cursor_keys::random(env)?,
    });
    let cursor_kid = cursors.active_kid().to_string();
    let closed = clickhouse
        .as_ref()
        .map(|_| "the telemetry store is starting");
    // Exports live in the control plane; their files on this node's exports root.
    let export_service = control
        .as_ref()
        .map(|control| Exports::new(env.clone(), Arc::new(control.store.clone())));
    let (app, report) = open_app(
        env,
        keys,
        Arc::clone(&cursors),
        WalConfig::default(),
        identity.clone(),
        closed,
        export_service.clone(),
    )
    .await?;
    let mode = if clickhouse.is_some() {
        "clickhouse"
    } else {
        "ingest-only"
    };
    let memory = read_memory();
    let recent_max_bytes = config
        .recent_max_bytes
        .unwrap_or_else(|| default_recent_max_bytes(memory));
    // The budgets in use: the recent-span index and the shipper run only with a telemetry store.
    let ingest_budget = Budget {
        setting: "ingest.inflight_bytes",
        bytes: ingest_config().inflight_bytes,
        changed_by: None,
    };
    let budgets = if clickhouse.is_some() {
        vec![
            Budget {
                setting: "recent.max_bytes",
                bytes: recent_max_bytes,
                changed_by: Some("--recent-max-bytes or CRUCIBLE_RECENT_MAX_BYTES"),
            },
            ingest_budget,
            Budget {
                setting: "shipper.encoded_bytes",
                bytes: ShipperConfig::default().encoded_bytes,
                changed_by: None,
            },
        ]
    } else {
        vec![ingest_budget]
    };
    if let Some(warning) = memory.and_then(|memory| budget_warning(memory, &budgets)) {
        tracing::warn!("{warning}");
    }
    let mcp = McpConfig {
        calls_per_second: config.mcp.calls_per_second,
        burst: config.mcp.burst,
    };
    // Without a store the task is one that never ends, so the wait below has one shape.
    let mut store = match clickhouse {
        Some(config) => {
            let task = StoreTask {
                config,
                migrate,
                control,
                wal: app.wal.clone(),
                report,
                keys: Arc::clone(&app.keys),
                cursors,
                gate: Arc::clone(&app.gate),
                recent_max_bytes,
                identity,
                facts: Arc::new(crate::facts::IngestFacts(app.ingest.clone())),
                mcp,
                exports,
                export_service,
            };
            Some(env.spawn("telemetry-store", run_store(env.clone(), task)))
        }
        None => None,
    };
    let listen = Addrs {
        api: config.server.listen,
        ingest: config.server.ingest_listen,
        grpc: config.server.grpc_listen,
        metrics: config.server.metrics_listen,
    };
    let mut servers = start(env, Arc::clone(&prod.recorder), Arc::new(ui), listen, app).await?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %servers.addrs.api,
        ingest = %servers.addrs.ingest,
        grpc = %servers.addrs.grpc,
        metrics = %servers.addrs.metrics,
        wal_backend = prod.wal_backend.backend.as_str(),
        ui = ui_source,
        ui_routes,
        keys = key_count,
        cursor_kid,
        mode,
        control = control_mode,
        job_tasks = jobs.tasks(),
        memory = ?memory,
        recent_max_bytes,
        "crucible is serving"
    );

    // Run until a termination signal arrives, a server stops on its own, or the shipper fails.
    let store_stopped = async {
        match store.as_mut() {
            Some(task) => task.join().await,
            None => std::future::pending().await,
        }
    };
    let early = tokio::select! {
        biased;
        signal = termination_signal() => {
            tracing::info!(signal = signal.context("installing signal handlers")?, "shutdown requested");
            None
        }
        result = servers.api.join() => Some(("api", result)),
        result = servers.ingest.join() => Some(("ingest", result)),
        result = servers.grpc.join() => Some(("grpc", result)),
        result = servers.metrics.join() => Some(("metrics", result)),
        result = store_stopped => Some(("telemetry-store", result.map(|r| r.map_err(io::Error::other)))),
    };

    let aborted = prod.spawner.close(SHUTDOWN_GRACE).await;
    if let Some((name, result)) = early {
        server_result(name, result)?;
        anyhow::bail!("the {name} server stopped without a shutdown request");
    }
    if let Some(mut task) = store {
        server_result(
            "telemetry-store",
            task.join().await.map(|r| r.map_err(io::Error::other)),
        )?;
    }
    server_result("api", servers.api.join().await)?;
    server_result("ingest", servers.ingest.join().await)?;
    server_result("grpc", servers.grpc.join().await)?;
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

    use crucible_cursor::{Cursor, FilterHash, Position, Sort};
    use crucible_env_prod::WalBackend;
    use crucible_recent::{WalLocator, WalReadError, WalView as _};
    use crucible_records::{ProjectId, RequestId, TimeWindow, Timestamp, TraceId};
    use crucible_wal_codec::{
        Compression, ContentOrigin, IngestTransport, PreparedFrame, Record, RecordType,
    };
    use opentelemetry_proto::tonic::collector::trace::v1::trace_service_client::TraceServiceClient;
    use opentelemetry_proto::tonic::collector::trace::v1::{
        ExportTraceServiceRequest, ExportTraceServiceResponse,
    };
    use prost::Message as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tonic::codec::CompressionEncoding;

    use super::*;
    use crate::store::MasterKey;

    const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    const LISTEN: Addrs = Addrs {
        api: LOOPBACK,
        ingest: LOOPBACK,
        grpc: LOOPBACK,
        metrics: LOOPBACK,
    };
    const PROJECT: &str = "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b";
    /// The challenges of a 401 on the ingest paths, as the lowercased header block shows them.
    const CHALLENGES: &str = "bearer, basic realm=\"crucible\"";

    fn secret() -> String {
        format!("csk_{}", "7".repeat(43))
    }

    fn public() -> String {
        format!("cpk_{}", "7".repeat(43))
    }

    fn keys_toml() -> String {
        format!(
            "[[project_key]]\nproject_id = \"{PROJECT}\"\npublic_key = \"{}\"\nsecret = \"{}\"\n",
            public(),
            secret()
        )
    }

    /// The disk roots under `dir`, as `--data-dir` lays them out.
    fn data_roots(dir: &Path) -> BTreeMap<Root, PathBuf> {
        [Root::Wal, Root::Quarantine, Root::Exports]
            .into_iter()
            .map(|root| (root, dir.join(root.name())))
            .collect()
    }

    fn prod_with(roots: BTreeMap<Root, PathBuf>) -> crucible_env_prod::Prod {
        let config = ProdConfig {
            roots,
            wal_backend: WalBackend::Auto,
        };
        crucible_env_prod::build(tokio::runtime::Handle::current(), &config).unwrap()
    }

    /// A test server whose cursor key is random.
    async fn test_app(env: &Env, keys: StaticKeys) -> App {
        let cursors = Arc::new(crate::cursor_keys::random(env).unwrap());
        test_app_with(env, keys, cursors).await
    }

    /// The WAL and ingest of a test server, with small segments, whose API signs and opens cursors with
    /// `cursors`.
    async fn test_app_with(env: &Env, keys: StaticKeys, cursors: Arc<KeyRing>) -> App {
        let config = WalConfig {
            segment_bytes: 1 << 20,
            ..WalConfig::default()
        };
        open_app(env, keys, cursors, config, None, None, None)
            .await
            .unwrap()
            .0
    }

    fn no_keys(env: &Env) -> StaticKeys {
        StaticKeys::new(env.rng.as_ref(), Vec::new()).unwrap()
    }

    /// One HTTP/1.1 exchange over the `Env` network; returns the status line, the lowercased headers and the
    /// body.
    async fn exchange(
        env: &Env,
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> (String, String, Vec<u8>) {
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        let mut stream = env.net.connect(addr, deadline).await.unwrap();
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        if !body.is_empty() {
            request.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
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

    async fn http(
        env: &Env,
        addr: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
    ) -> (String, String, Vec<u8>) {
        exchange(env, addr, method, path, headers, b"").await
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

    /// Closes the scope and checks that every server, and with them the WAL committer, stopped in time.
    async fn shut_down(prod: &crucible_env_prod::Prod, mut servers: Servers) {
        assert_eq!(
            prod.spawner.close(Duration::from_secs(10)).await,
            Vec::<&str>::new()
        );
        assert!(matches!(servers.api.join().await, Ok(Ok(()))));
        assert!(matches!(servers.ingest.join().await, Ok(Ok(()))));
        assert!(matches!(servers.grpc.join().await, Ok(Ok(()))));
        assert!(matches!(servers.metrics.join().await, Ok(Ok(()))));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn health_and_metrics_are_served_and_shutdown_is_clean() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let app = test_app(env, no_keys(env)).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        assert_eq!(
            prod.spawner.task_names(),
            [
                "wal-committer",
                "http-api",
                "http-ingest",
                "grpc-ingest",
                "http-metrics"
            ]
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
        shut_down(&prod, servers).await;
        let deadline = env.clock.now().saturating_add(Duration::from_secs(10));
        assert!(env.net.connect(addrs.api, deadline).await.is_err());
    }

    /// The default bound of the recent-span index, in bytes, is at most 10% of the memory this process may use:
    /// of the container's limit when `/sys/fs/cgroup/memory.max` holds one, as it does in a container started
    /// with a memory limit.
    #[test]
    #[allow(clippy::disallowed_methods)] // The test reads the file the server reads.
    fn the_default_recent_bound_fits_the_container() {
        let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .and_then(|text| text.trim().parse::<u64>().ok());
        let bound = default_recent_max_bytes(read_memory());
        match limit {
            Some(limit) => assert!(
                bound <= limit / 10,
                "the default recent bound is {bound} bytes, more than 10% of the container's limit of {limit} bytes"
            ),
            None => assert!(bound <= 1 << 30, "{bound}"),
        }
    }

    /// The texts of `/proc/meminfo` and `/sys/fs/cgroup/memory.max` as a host and a container show them.
    #[test]
    fn the_memory_is_the_smaller_of_the_host_and_the_container_limit() {
        // 16,094,164 kB: the VM of the development rig.
        const MEMINFO: &str = "MemTotal:       16094164 kB\nMemFree:         8000000 kB\n";
        let host = 16_094_164 * 1024;
        let two_gib = 2 << 30;
        let cases = [
            // No container limit: `max`, or no file (Windows, macOS, cgroup v1).
            (Some(MEMINFO), Some("max\n"), Some(Memory::Host(host))),
            (Some(MEMINFO), None, Some(Memory::Host(host))),
            // A container limited to 2 GiB.
            (
                Some(MEMINFO),
                Some("2147483648\n"),
                Some(Memory::Container(two_gib)),
            ),
            // A limit above the host's memory limits nothing.
            (Some(MEMINFO), Some("68719476736"), Some(Memory::Host(host))),
            (None, Some("2147483648"), Some(Memory::Container(two_gib))),
            (None, None, None),
            (Some("no total here"), Some("garbage"), None),
        ];
        for (meminfo, memory_max, expected) in cases {
            assert_eq!(
                Memory::from_files(meminfo, memory_max),
                expected,
                "{meminfo:?} {memory_max:?}"
            );
        }
        // The default recent bound: 10% of the memory, at most 1 GiB; 256 MiB when it cannot be read.
        assert_eq!(
            default_recent_max_bytes(Some(Memory::Container(two_gib))),
            214_748_364
        );
        assert_eq!(default_recent_max_bytes(Some(Memory::Host(host))), 1 << 30);
        assert_eq!(default_recent_max_bytes(None), RECENT_FALLBACK_BYTES);
    }

    #[test]
    fn budgets_beyond_their_share_of_the_memory_are_named_with_what_changes_them() {
        let budgets = |recent: u64| {
            [
                Budget {
                    setting: "recent.max_bytes",
                    bytes: recent,
                    changed_by: Some("--recent-max-bytes or CRUCIBLE_RECENT_MAX_BYTES"),
                },
                Budget {
                    setting: "ingest.inflight_bytes",
                    bytes: 512 << 20,
                    changed_by: None,
                },
                Budget {
                    setting: "shipper.encoded_bytes",
                    bytes: 1 << 30,
                    changed_by: None,
                },
            ]
        };
        // A container limited to 2 GiB: the defaults take 85% of it.
        let two_gib = Memory::Container(2 << 30);
        assert_eq!(
            budget_warning(two_gib, &budgets(default_recent_max_bytes(Some(two_gib)))),
            None
        );
        // Up to the share exactly, nothing is said.
        let fits = (2 << 30) * BUDGET_SHARE_PERCENT / 100 - (1536 << 20);
        assert_eq!(budget_warning(two_gib, &budgets(fits)), None);
        assert!(budget_warning(two_gib, &budgets(fits + 1)).is_some());
        // A container limited to 1.75 GiB: the budgets without a setting fit, so lowering the recent bound to
        // what the warning names makes them all fit.
        let roomy = Memory::Container(1792 << 20);
        let warning =
            budget_warning(roomy, &budgets(default_recent_max_bytes(Some(roomy)))).unwrap();
        assert_eq!(
            warning,
            "the memory budgets add up to 1716 MiB, more than 90% of the 1792 MiB this process may use (the \
             container's memory limit): recent.max_bytes 180 MiB, ingest.inflight_bytes 512 MiB, \
             shipper.encoded_bytes 1024 MiB. Give the process at least 1906 MiB of memory, or lower \
             recent.max_bytes (--recent-max-bytes or CRUCIBLE_RECENT_MAX_BYTES) to at most 76 MiB; \
             ingest.inflight_bytes and shipper.encoded_bytes have no setting yet"
        );
        assert_eq!(budget_warning(roomy, &budgets(76 << 20)), None);
        assert!(budget_warning(Memory::Container(1906 << 20), &budgets(187_904_819)).is_none());
        // A container limited to 1.5 GiB: the budgets without a setting take more than the share by themselves,
        // so only more memory helps.
        let limited = Memory::Container(1536 << 20);
        let warning =
            budget_warning(limited, &budgets(default_recent_max_bytes(Some(limited)))).unwrap();
        assert_eq!(
            warning,
            "the memory budgets add up to 1690 MiB, more than 90% of the 1536 MiB this process may use (the \
             container's memory limit): recent.max_bytes 154 MiB, ingest.inflight_bytes 512 MiB, \
             shipper.encoded_bytes 1024 MiB. Give the process at least 1878 MiB of memory; \
             ingest.inflight_bytes and shipper.encoded_bytes have no setting yet"
        );
        assert!(budget_warning(limited, &budgets(0)).is_some());
        // Without a telemetry store only ingest holds a budget.
        let ingest_only = [budgets(0)[1]];
        assert!(
            budget_warning(Memory::Host(512 << 20), &ingest_only)
                .unwrap()
                .ends_with(
                    "Give the process at least 569 MiB of memory; ingest.inflight_bytes has no setting yet"
                )
        );
    }

    #[test]
    fn readiness_follows_the_wal() {
        crucible_sim::run(1, |sim| async move {
            let disk = sim.disk("disk");
            let host = sim.host("node/boot-1", &disk, 0);
            let config = WalConfig {
                segment_bytes: 1 << 20,
                ..WalConfig::default()
            };
            let (wal, _) = Wal::open(&host.env, config).await.unwrap();
            let gate = Gate {
                closed: RwLock::new(None),
                api: OnceLock::new(),
                api_without_store: Router::new(),
                snapshot: None,
                mcp: Arc::new(McpGate::new(&host.env, Arc::new(no_keys(&host.env)))),
            };
            let readiness = |wal: &Wal| {
                let (status, text) = readiness(wal, &gate);
                (status, text.leak() as &str)
            };
            assert_eq!(readiness(&wal), (StatusCode::OK, "ready"));
            // The group's sync fails and so does the rotation after it: the WAL fails until restart.
            disk.fail_next_syncs(2);
            let frame =
                PreparedFrame::new(RecordType::Noop, vec![0; 8], Compression::None).unwrap();
            assert!(wal.append(frame).await.is_err());
            assert_eq!(
                readiness(&wal),
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "not ready: the wal has failed"
                )
            );
        });
    }

    /// Asserts a problem with this status line and code whose request id matches `X-Request-Id`.
    fn assert_problem(status: &str, headers: &str, body: &[u8], expected: &str, code: &str) {
        assert_eq!(status, format!("HTTP/1.1 {expected}"));
        assert_eq!(
            header(headers, "content-type"),
            Some("application/problem+json")
        );
        let problem: serde_json::Value = serde_json::from_slice(body).unwrap();
        assert_eq!(problem["type"], format!("urn:crucible:problem:{code}"));
        assert_eq!(problem["status"].to_string(), expected[..3]);
        assert_eq!(problem["code"], code);
        let request_id = problem["request_id"].as_str().unwrap();
        assert!(RequestId::parse(request_id).is_ok(), "{request_id}");
        assert_eq!(header(headers, "x-request-id"), Some(request_id));
    }

    /// Asserts a problem 404 whose request id matches `X-Request-Id`.
    fn assert_not_found_problem(status: &str, headers: &str, body: &[u8]) {
        assert_problem(status, headers, body, "404 Not Found", "not_found");
    }

    /// The API is mounted before a telemetry store is: requests are authenticated and checked against the
    /// key's project, then answered with `telemetry_store_unavailable`. UI responses carry the request id and
    /// the security headers too.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_api_answers_unavailable_until_a_store_is_configured() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let app = test_app(env, keys).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addr = servers.addrs.api;
        let bearer = format!("Bearer {}", secret());
        let key = [("authorization", bearer.as_str())];
        let traces = format!(
            "/api/v1/projects/{PROJECT}/traces?from=2026-11-16T00:00:00Z&to=2026-11-17T00:00:00Z"
        );

        let (status, headers, body) = http(env, addr, "GET", &traces, &[]).await;
        assert_problem(
            &status,
            &headers,
            &body,
            "401 Unauthorized",
            "unauthenticated",
        );
        let other = traces.replace(PROJECT, "0193a1b2-7c3d-7e4f-8a5b-000000000000");
        let (status, headers, body) = http(env, addr, "GET", &other, &key).await;
        assert_problem(&status, &headers, &body, "404 Not Found", "not_found");
        for path in [
            traces.clone(),
            format!("/api/v1/projects/{PROJECT}/traces/5b8efff798038103d269b633813fc60c"),
            format!(
                "/api/v1/projects/{PROJECT}/traces/5b8efff798038103d269b633813fc60c/spans\
                 ?parent_span_id=eee19b7ec3c1b174"
            ),
            format!("/api/v1/projects/{PROJECT}/data-bounds"),
        ] {
            let (status, headers, body) = http(env, addr, "GET", &path, &key).await;
            assert_problem(
                &status,
                &headers,
                &body,
                "503 Service Unavailable",
                "telemetry_store_unavailable",
            );
            assert_eq!(
                header(&headers, "content-security-policy"),
                Some("default-src 'none'; frame-ancestors 'none'")
            );
        }

        let (status, headers, _) = http_get(env, addr, "/").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        assert!(header(&headers, "x-request-id").is_some());
        assert_eq!(header(&headers, "x-content-type-options"), Some("nosniff"));
        assert_eq!(header(&headers, "referrer-policy"), Some("same-origin"));
        let (_, headers, _) = http_get(env, addr, "/healthz").await;
        assert!(header(&headers, "x-request-id").is_some());

        shut_down(&prod, servers).await;
    }

    /// An MCP `initialize` of the session era.
    const MCP_INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;

    /// A query service over an empty store model and a recent index that nothing feeds.
    fn empty_store(env: &Env, app: &App) -> QueryService {
        use crucible_normalize::{NormCtx, PriceTable};
        use crucible_schema::Granularity;
        use crucible_sim::ch_model::ChModel;

        let flavour = crucible_env::StoreFlavour::SingleNode;
        let model = ChModel::new(
            env.rng.as_ref(),
            flavour,
            Granularity::Day,
            Duration::from_secs(60),
        );
        let view = Arc::new(WalSpans {
            wal: app.wal.clone(),
            ctx: NormCtx::new(
                PriceTable::bundled().unwrap(),
                ShipperConfig::default().caps,
            ),
        });
        let config = RecentConfig::new(1 << 20, flavour);
        let (index, _writer) = RecentIndex::new(config, Arc::clone(&env.clock), view);
        let stores = Stores::new(Arc::new(model));
        QueryService::new(stores, Arc::clone(&env.clock), index, app.wal.epoch())
    }

    /// The MCP server at `/mcp` of the main router. Before a store it authenticates and then says which setting
    /// provides one. Over a store, rmcp's own client of each protocol era (a session opened with `initialize`,
    /// and self-contained requests after `server/discover`) lists the tools and calls them. Paths below `/mcp/`
    /// stay the UI's.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_mcp_server_answers_both_protocol_eras_at_mcp() {
        use crucible_mcp_server::PATH;
        use http_body_util::BodyExt as _;
        use rmcp::model::{CallToolRequestParams, ProtocolVersion};
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        use rmcp::{ClientLifecycleMode, ClientServiceExt as _};
        use serde_json::json;

        use crate::mcp::client::RouterClient;

        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let cursors = Arc::new(crate::cursor_keys::random(env).unwrap());
        let app = test_app_with(env, keys, Arc::clone(&cursors)).await;
        let router = api_router(env, &app, Arc::new(Catalog::default()));
        let post = |authorization: Option<String>| {
            let mut request = axum::http::Request::post(PATH)
                .header("host", "crucible.internal:8080")
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json");
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            request.body(Body::from(MCP_INITIALIZE)).unwrap()
        };
        let problem = |response: Response| async move {
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            (status, body)
        };

        let (status, body) = problem(router.clone().oneshot(post(None)).await.unwrap()).await;
        assert_eq!(
            (status, &body["code"]),
            (StatusCode::UNAUTHORIZED, &json!("unauthenticated"))
        );
        let bearer = format!("Bearer {}", secret());
        let response = router.clone().oneshot(post(Some(bearer))).await.unwrap();
        let (status, body) = problem(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "telemetry_store_unavailable");
        assert!(
            body["detail"]
                .as_str()
                .unwrap()
                .contains("--clickhouse-url")
        );
        let ui_page = axum::http::Request::get("/mcp/tools")
            .body(Body::empty())
            .unwrap();
        let (status, body) = problem(router.clone().oneshot(ui_page).await.unwrap()).await;
        assert_eq!(
            (status, &body["code"]),
            (StatusCode::NOT_FOUND, &json!("not_found"))
        );

        let service = empty_store(env, &app);
        app.gate.mcp.open(
            env,
            service,
            Arc::clone(&app.keys),
            cursors,
            McpConfig::default(),
            None,
        );

        let lifecycles = [
            ClientLifecycleMode::Initialize,
            ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        ];
        for lifecycle in lifecycles {
            let config =
                StreamableHttpClientTransportConfig::with_uri("http://crucible.internal:8080/mcp")
                    .auth_header(secret());
            let transport =
                StreamableHttpClientTransport::with_client(RouterClient(router.clone()), config);
            let client = ().serve_with_lifecycle(transport, lifecycle).await.unwrap();
            let tools: Vec<String> = client
                .list_all_tools()
                .await
                .unwrap()
                .into_iter()
                .map(|tool| tool.name.into_owned())
                .collect();
            for name in [
                "get_data_bounds",
                "get_span_children",
                "get_trace",
                "list_traces",
            ] {
                assert!(tools.iter().any(|tool| tool == name), "{name}: {tools:?}");
            }
            let bounds = client
                .call_tool(CallToolRequestParams::new("get_data_bounds"))
                .await
                .unwrap();
            assert_eq!(bounds.is_error, Some(false));
            let output = bounds.structured_content.unwrap();
            assert_eq!(output["max_window_days"], 30);
            assert_eq!(output["spans"]["first"], serde_json::Value::Null);
            let arguments = json!({ "from": "yesterday", "to": "2026-11-17T00:00:00Z" });
            let refused = client
                .call_tool(
                    CallToolRequestParams::new("list_traces")
                        .with_arguments(arguments.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            assert_eq!(refused.is_error, Some(true));
            let text = &refused.content[0].as_text().unwrap().text;
            let problem: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(problem["code"], "validation_failed");
            assert_eq!(problem["errors"][0]["pointer"], "/from");
            client.cancel().await.unwrap();
        }
    }

    /// A graceful shutdown ends the MCP server's sessions and their open event streams, so a connected agent does
    /// not hold the listeners past the grace period: `shut_down` fails when a task is still running after it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_graceful_shutdown_closes_open_mcp_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let cursors = Arc::new(crate::cursor_keys::random(env).unwrap());
        let app = test_app_with(env, keys, Arc::clone(&cursors)).await;
        let (keys, service) = (Arc::clone(&app.keys), empty_store(env, &app));
        app.gate
            .mcp
            .open(env, service, keys, cursors, McpConfig::default(), None);
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addr = servers.addrs.api;
        let bearer = format!("Bearer {}", secret());
        let posted = [
            ("authorization", bearer.as_str()),
            ("accept", "application/json, text/event-stream"),
            ("content-type", "application/json"),
        ];
        let initialize = MCP_INITIALIZE.as_bytes();
        let (status, headers, _) = exchange(env, addr, "POST", "/mcp", &posted, initialize).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        let session = header(&headers, "mcp-session-id").unwrap().to_owned();

        // The session's event stream stays open until the server ends it.
        let stream_env = env.clone();
        let mut stream = env.spawn("mcp-stream", async move {
            let headers = [
                ("authorization", bearer.as_str()),
                ("accept", "text/event-stream"),
                ("mcp-session-id", session.as_str()),
            ];
            exchange(&stream_env, addr, "GET", "/mcp", &headers, b"").await
        });
        env.clock.sleep(Duration::from_millis(200)).await;
        shut_down(&prod, servers).await;
        let (status, headers, _) = stream.join().await.unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
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
        let dir = tempfile::tempdir().unwrap();
        let mut roots = data_roots(dir.path());
        roots.insert(Root::Ui, FIXTURE.into());
        let prod = prod_with(roots);
        let env = &prod.env;
        let ui = Catalog::from_dir(env.disk.as_ref()).await.unwrap();
        assert_eq!(
            ui.manifest().routes,
            ["/", "/home/", "/mcp/servers/", "/traces/"]
        );
        let traces = ui.get("traces/index.html").unwrap();
        assert_eq!(traces.bytes, TRACES);
        assert_eq!(traces.encodings.br.as_deref(), Some(TRACES_BR));
        let app = test_app(env, no_keys(env)).await;
        let servers = start(env, Arc::clone(&prod.recorder), Arc::new(ui), LISTEN, app)
            .await
            .unwrap();
        let addr = servers.addrs.api;

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
            ("POST", "/v1/logs"),
            ("GET", "/mcp/nope"),
        ] {
            let (status, headers, body) = http(env, addr, method, path, &[]).await;
            assert_not_found_problem(&status, &headers, &body);
        }
        // `/mcp` is the MCP server, never the UI: without a key it is a 401 problem.
        let (status, headers, body) = http(env, addr, "GET", "/mcp", &[]).await;
        assert_problem(
            &status,
            &headers,
            &body,
            "401 Unauthorized",
            "unauthenticated",
        );
        // `/v1/traces` is ingest, never the UI.
        let (status, headers, _) = http(env, addr, "POST", "/v1/traces", &[]).await;
        assert_eq!(status, "HTTP/1.1 401 Unauthorized");
        assert_eq!(header(&headers, "www-authenticate"), Some(CHALLENGES));
        let (status, _, body) = http_get(env, addr, "/healthz").await;
        assert_eq!((status.as_str(), body.as_str()), ("HTTP/1.1 200 OK", "ok"));

        shut_down(&prod, servers).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_ui_the_root_says_how_to_build_one() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let app = test_app(env, no_keys(env)).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addr = servers.addrs.api;

        let (status, headers, body) = http_get(env, addr, "/").await;
        assert_eq!(status, "HTTP/1.1 404 Not Found");
        assert_eq!(
            header(&headers, "content-type"),
            Some("text/plain; charset=utf-8")
        );
        assert!(
            body.contains("--ui-dir") && body.contains("embed-ui"),
            "{body}"
        );
        let (status, headers, body) = http(env, addr, "GET", "/api/v1/x", &[]).await;
        assert_not_found_problem(&status, &headers, &body);

        shut_down(&prod, servers).await;
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
    async fn the_api_fact_writer_appends_durable_fact_batches_through_ingest() {
        use crucible_api::{FactAuthor, FactRefusal, FactWriter as _, NewFact};
        use crucible_records::{AuthorKind, FactBody, FactTargetKind, SpanId, TagBody, TagOp};

        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let principal = keys.verify(&secret()).unwrap();
        let app = test_app(env, keys).await;
        let writer = crate::facts::IngestFacts(app.ingest.clone());
        let author = FactAuthor {
            kind: AuthorKind::ApiKey,
            id: principal.api_key_id.uuid(),
        };
        let fact = NewFact {
            target_kind: FactTargetKind::Trace,
            trace_id: TraceId::from_bytes([5; 16]),
            span_id: SpanId::ZERO,
            session_id: "".into(),
            target_time: env.clock.wall_now(),
            key: "urgent".into(),
            retracted: false,
            body: FactBody::Tag(TagBody { op: TagOp::Add }),
        };
        let accepted = writer
            .append(principal.scope, author, vec![fact.clone()])
            .await
            .unwrap();
        assert_eq!(accepted.len(), 1);

        // The answer came after the fsync: the log already holds the fact's frame.
        let mut replay = app.wal.replay(Lsn::new(1));
        let frame = replay.next().await.unwrap().unwrap();
        let payload = frame.payload().unwrap();
        let Ok(Record::FactBatch(batch)) = Record::decode(RecordType::FactBatch, &payload) else {
            panic!("frame {} is not a fact batch", frame.lsn());
        };
        assert_eq!(batch.project_id.to_string(), PROJECT);
        assert_eq!(batch.author_id, principal.api_key_id.uuid());
        let written = &batch.facts[0];
        assert_eq!(written.fact_id, accepted[0].fact_id);
        assert_eq!(
            (written.key, written.version),
            ("urgent", accepted[0].version)
        );

        // A refusal of ingest reaches the route as a refusal, and nothing is written.
        let empty = NewFact {
            key: "".into(),
            ..fact
        };
        let refused = writer.append(principal.scope, author, vec![empty]).await;
        assert!(
            matches!(refused, Err(FactRefusal::Invalid(_))),
            "{refused:?}"
        );
        assert!(replay.next().await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ingest_is_served_on_both_listeners_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let app = test_app(env, keys).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addrs = servers.addrs;

        let start_nanos = u64::try_from(env.clock.wall_now().micros()).unwrap() * 1_000;
        let json = span_json(start_nanos);
        let protobuf = crucible_otlp::transcode_json(json.as_bytes()).unwrap();
        let bearer = format!("Bearer {}", secret());
        let post = |addr, authorization: &'static str, content_type, body: Vec<u8>| {
            let bearer = bearer.clone();
            async move {
                let authorization = if authorization.is_empty() {
                    Vec::new()
                } else {
                    vec![("authorization", bearer.as_str())]
                };
                let mut headers = vec![
                    ("content-type", content_type),
                    ("user-agent", "e2e-exporter/1.0"),
                ];
                headers.extend(authorization);
                exchange(env, addr, "POST", "/v1/traces", &headers, &body).await
            }
        };
        let protobuf_type = "application/x-protobuf";
        let (status, headers, body) =
            post(addrs.ingest, "key", protobuf_type, protobuf.clone()).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(header(&headers, "content-type"), Some(protobuf_type));
        assert!(body.is_empty(), "a full success has an empty response");
        let (status, headers, body) = post(
            addrs.api,
            "key",
            "application/json",
            json.clone().into_bytes(),
        )
        .await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(header(&headers, "content-type"), Some("application/json"));
        assert_eq!(body, b"{}");
        let (status, headers, _) = post(addrs.ingest, "", protobuf_type, protobuf.clone()).await;
        assert_eq!(status, "HTTP/1.1 401 Unauthorized");
        assert_eq!(header(&headers, "www-authenticate"), Some(CHALLENGES));
        let (status, _, body) = http_get(env, addrs.api, "/readyz").await;
        assert_eq!(
            (status.as_str(), body.as_str()),
            ("HTTP/1.1 200 OK", "ready")
        );
        let recorder = &prod.recorder;
        let requests = |outcome| recorder.counter_value(&registry::INGEST_REQUESTS, &[outcome]);
        assert_eq!((requests("accepted"), requests("unauthenticated")), (2, 1));
        assert_eq!(
            recorder.counter_value(&registry::HTTP_REQUESTS, &["ingest", "200"]),
            1
        );
        assert_eq!(
            recorder.counter_value(&registry::HTTP_REQUESTS, &["api", "200"]),
            2
        );

        shut_down(&prod, servers).await;
        drop(prod);

        // A restart on the same directory finds both requests durable.
        let prod = prod_with(data_roots(dir.path()));
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

        // The point read behind a recent-span locator: the frame at its segment and offset, and nothing
        // anywhere else.
        let mut replay = wal.replay(Lsn::new(2));
        let second = replay.next().await.unwrap().unwrap();
        let (segment, offset) = (second.segment_no(), second.offset());
        assert_eq!(wal.read_at(segment, offset).await.unwrap(), Some(second));
        assert_eq!(wal.read_at(segment, offset + 1).await.unwrap(), None);
        assert_eq!(wal.read_at(segment, 1 << 40).await.unwrap(), None);
        assert_eq!(wal.read_at(segment + 1, offset).await.unwrap(), None);

        // The recent index re-reads a span through it and the normalizer, inside its project only.
        let view = WalSpans {
            wal: wal.clone(),
            ctx: NormCtx::new(
                PriceTable::bundled().unwrap(),
                ShipperConfig::default().caps,
            ),
        };
        let project = crucible_records::ProjectId::parse(PROJECT).unwrap();
        let locator = |ordinal| WalLocator {
            segment,
            offset,
            ordinal,
        };
        let span = view.read_span(project, locator(0)).await.unwrap();
        assert_eq!(&*span.core.name, "tool call");
        assert_eq!(span.core.ingestion.lsn, Lsn::new(2));
        assert_eq!(view.durable_lsn(), Lsn::new(2));
        let gone = Err(WalReadError::Gone);
        assert_eq!(view.read_span(project, locator(1)).await, gone);
        let other = crucible_records::ProjectId::from_bytes([9; 16]);
        assert_eq!(view.read_span(other, locator(0)).await, gone);
        let missing = WalLocator {
            offset: offset + 1,
            ..locator(0)
        };
        assert_eq!(view.read_span(project, missing).await, gone);
    }

    /// `Authorization: Basic` with `public:secret`.
    fn basic(public: &str, secret: &str) -> String {
        use base64::Engine as _;
        let pair = format!("{public}:{secret}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }

    /// An HTTP/2 connection to the gRPC listener at `addr`, as an OTLP exporter whose user agent is
    /// `e2e-grpc/2.0` keeps one open.
    async fn grpc_channel(addr: SocketAddr) -> tonic::transport::Channel {
        tonic::transport::Endpoint::from_shared(format!("http://{addr}"))
            .unwrap()
            .user_agent("e2e-grpc/2.0")
            .unwrap()
            .connect()
            .await
            .unwrap()
    }

    /// A gRPC export of `body` with `authorization` on `channel`, gzip-compressed.
    async fn grpc_export(
        channel: &tonic::transport::Channel,
        authorization: &str,
        body: &[u8],
    ) -> Result<ExportTraceServiceResponse, tonic::Status> {
        let mut client =
            TraceServiceClient::new(channel.clone()).send_compressed(CompressionEncoding::Gzip);
        let mut request = tonic::Request::new(ExportTraceServiceRequest::decode(body).unwrap());
        request
            .metadata_mut()
            .insert("authorization", authorization.parse().unwrap());
        client
            .export(request)
            .await
            .map(tonic::Response::into_inner)
    }

    /// The compat ingest routes of the registry, its routing headers, gRPC and the problem 404s, through the
    /// real listeners. Every route takes the key pair as Basic on both HTTP listeners; the frames carry each
    /// route's hint and SDK and survive a restart.
    #[tokio::test(flavor = "multi_thread")]
    async fn compat_routes_and_grpc_are_served_on_their_listeners() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let app = test_app(env, keys).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addrs = servers.addrs;
        let start_nanos = u64::try_from(env.clock.wall_now().micros()).unwrap() * 1_000;
        let protobuf = crucible_otlp::transcode_json(span_json(start_nanos).as_bytes()).unwrap();
        let (pair, bearer) = (basic(&public(), &secret()), format!("Bearer {}", secret()));
        let protobuf_type = "application/x-protobuf";
        let json_type = "application/json";
        let json = span_json(start_nanos).into_bytes();
        let gzip = |bytes: &[u8]| {
            use std::io::Write as _;
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(bytes).unwrap();
            encoder.finish().unwrap()
        };
        let (pb, js) = (ContentOrigin::Protobuf, ContentOrigin::Json);
        // Each encoding on one of the two HTTP ingest listeners: (listener, media type, gzip, body, origin).
        let sends = [
            (addrs.api, protobuf_type, false, protobuf.clone(), pb),
            (addrs.ingest, protobuf_type, false, protobuf.clone(), pb),
            (addrs.ingest, json_type, false, json.clone(), js),
            (addrs.api, protobuf_type, true, gzip(&protobuf), pb),
            (addrs.ingest, json_type, true, gzip(&json), js),
        ];
        let agent = ("e2e-exporter", "1.0");
        // (compat_dialect, transport, content origin, (sdk_name, sdk_version)) of each acknowledged request, in
        // order.
        let mut expected = Vec::new();

        for route in crucible_compat::routes::INGEST_ROUTES {
            for (addr, content_type, compressed, body, origin) in &sends {
                let mut headers = vec![
                    ("authorization", pair.as_str()),
                    ("content-type", *content_type),
                    ("user-agent", "e2e-exporter/1.0"),
                ];
                if *compressed {
                    headers.push(("content-encoding", "gzip"));
                }
                if let Some(sdk) = route.sdk_headers {
                    headers.extend([(sdk.name, "sdk-a"), (sdk.version, "2.1.0")]);
                }
                let (status, _, reply) =
                    exchange(env, *addr, "POST", route.path, &headers, body).await;
                assert_eq!(status, "HTTP/1.1 200 OK", "{}: {reply:?}", route.path);
                let empty: &[u8] = if *origin == js { b"{}" } else { b"" };
                assert_eq!(reply, empty);
                let sdk = route.sdk_headers.map_or(agent, |_| ("sdk-a", "2.1.0"));
                expected.push((route.dialect, IngestTransport::Http, *origin, sdk));
            }
            // A pair whose halves belong to different keys is refused with what to fix.
            let mixed = basic(&format!("cpk_{}", "8".repeat(43)), &secret());
            let headers = [
                ("authorization", mixed.as_str()),
                ("content-type", protobuf_type),
            ];
            let (status, headers, body) =
                exchange(env, addrs.ingest, "POST", route.path, &headers, &protobuf).await;
            assert_eq!(status, "HTTP/1.1 401 Unauthorized");
            assert_eq!(header(&headers, "www-authenticate"), Some(CHALLENGES));
            assert_eq!(header(&headers, "content-type"), Some(protobuf_type));
            let body = String::from_utf8_lossy(&body);
            assert!(body.contains("the public key does not belong to the same project"));
            assert!(!body.contains(&secret()[4..]) && !body.contains(&"8".repeat(43)));
        }
        for routing in crucible_compat::routes::ROUTING_HEADERS {
            let headers = [
                ("authorization", bearer.as_str()),
                ("content-type", protobuf_type),
                ("user-agent", "e2e-exporter/1.0"),
                (routing.name, "x"),
            ];
            let (status, ..) =
                exchange(env, addrs.ingest, "POST", "/v1/traces", &headers, &protobuf).await;
            assert_eq!(status, "HTTP/1.1 200 OK");
            expected.push((routing.dialect, IngestTransport::Http, pb, agent));
        }
        // One connection for every export, held open across the shutdown as an exporter holds it.
        let channel = grpc_channel(addrs.grpc).await;
        for authorization in [&bearer, &pair] {
            let response = grpc_export(&channel, authorization, &protobuf)
                .await
                .unwrap();
            assert_eq!(response.partial_success, None);
            expected.push((0, IngestTransport::Grpc, pb, ("e2e-grpc", "2.0")));
        }
        let refused = grpc_export(&channel, "Bearer csk_short", &protobuf)
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        // OTLP over HTTP/1.1 sent to the gRPC port.
        let headers = [
            ("authorization", bearer.as_str()),
            ("content-type", protobuf_type),
        ];
        let (status, ..) =
            exchange(env, addrs.grpc, "POST", "/v1/traces", &headers, &protobuf).await;
        assert_eq!(status, "HTTP/1.1 415 Unsupported Media Type");

        // Paths under a route's first segment that name no route, and the signals v0 does not ingest, answer
        // problem 404 on both HTTP listeners; the ingest listener answers it for every other path too.
        let mut unknown: Vec<String> = crucible_compat::routes::NOT_INGESTED
            .iter()
            .map(|path| (*path).to_owned())
            .collect();
        for route in crucible_compat::routes::INGEST_ROUTES {
            let segment = route.path[1..]
                .find('/')
                .map_or(route.path, |at| &route.path[..=at]);
            unknown.push(format!("{segment}/v1/metrics"));
            unknown.push(segment.to_owned());
        }
        for addr in [addrs.api, addrs.ingest] {
            for path in &unknown {
                for method in ["GET", "POST"] {
                    let (status, headers, body) = http(env, addr, method, path, &[]).await;
                    assert_not_found_problem(&status, &headers, &body);
                }
            }
        }
        for path in ["/v1/logs", "/", "/traces/"] {
            let (status, headers, body) = http(env, addrs.ingest, "POST", path, &[]).await;
            assert_not_found_problem(&status, &headers, &body);
        }
        let recorder = &prod.recorder;
        assert_eq!(
            recorder.counter_value(&registry::HTTP_REQUESTS, &["grpc", "200"]),
            3,
            "the refused export is a gRPC status on HTTP 200"
        );
        // The idle connection does not hold up the shutdown: `shut_down` checks that nothing was aborted.
        shut_down(&prod, servers).await;
        drop(channel);
        drop(prod);

        let prod = prod_with(data_roots(dir.path()));
        let (wal, _) = Wal::open(&prod.env, WalConfig::default()).await.unwrap();
        let mut replay = wal.replay(Lsn::new(1));
        let mut written = Vec::new();
        while let Some(frame) = replay.next().await.unwrap() {
            let payload = frame.payload().unwrap();
            let Ok(Record::IngestRequest(request)) =
                Record::decode(RecordType::IngestRequest, &payload)
            else {
                panic!("frame {} is not an ingest request", frame.lsn());
            };
            assert_eq!(request.project_id.to_string(), PROJECT);
            assert_eq!(request.otlp, protobuf.as_slice());
            let sdk = (request.sdk_name.to_owned(), request.sdk_version.to_owned());
            written.push((
                request.compat_dialect,
                request.transport,
                request.content_origin,
                sdk,
            ));
        }
        let expected: Vec<_> = expected
            .into_iter()
            .map(|(dialect, transport, origin, (name, version))| {
                let sdk = (name.to_owned(), version.to_owned());
                (dialect, transport, origin, sdk)
            })
            .collect();
        assert_eq!(written, expected);
    }

    /// While the telemetry store is not up, gRPC ingest answers `UNAVAILABLE` with the reason and a
    /// `RetryInfo`, as HTTP ingest answers 503 with `Retry-After`.
    #[tokio::test(flavor = "multi_thread")]
    async fn grpc_ingest_is_unavailable_while_the_gate_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let cursors = Arc::new(crate::cursor_keys::random(env).unwrap());
        let config = WalConfig {
            segment_bytes: 1 << 20,
            ..WalConfig::default()
        };
        let closed = Some("the telemetry store is starting");
        let (app, _) = open_app(env, keys, cursors, config, None, closed, None)
            .await
            .unwrap();
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let start_nanos = u64::try_from(env.clock.wall_now().micros()).unwrap() * 1_000;
        let protobuf = crucible_otlp::transcode_json(span_json(start_nanos).as_bytes()).unwrap();
        let bearer = format!("Bearer {}", secret());
        let channel = grpc_channel(servers.addrs.grpc).await;
        let status = grpc_export(&channel, &bearer, &protobuf).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(
            status.message(),
            "ingest is closed: the telemetry store is starting"
        );
        assert!(
            !status.details().is_empty(),
            "the status carries a RetryInfo"
        );
        let headers = [("authorization", bearer.as_str())];
        let (status, headers, _) =
            http(env, servers.addrs.ingest, "POST", "/v1/traces", &headers).await;
        assert_eq!(status, "HTTP/1.1 503 Service Unavailable");
        assert_eq!(header(&headers, "retry-after"), Some("5"));
        // OTLP over HTTP sent to the gRPC port is refused as such, closed or not, never with HTTP 200.
        let headers = [
            ("authorization", bearer.as_str()),
            ("content-type", "application/x-protobuf"),
        ];
        let (status, headers, _) = exchange(
            env,
            servers.addrs.grpc,
            "POST",
            "/v1/traces",
            &headers,
            &protobuf,
        )
        .await;
        assert_eq!(status, "HTTP/1.1 415 Unsupported Media Type");
        assert_eq!(header(&headers, "grpc-status"), None);
        shut_down(&prod, servers).await;
    }

    /// The API opens cursors with the key derived from the master key, before it finds that no store is up: a
    /// cursor signed with the key that another node derives from the same master key passes and gets
    /// `telemetry_store_unavailable`; one signed with any other key is `cursor_invalid`.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_api_opens_cursors_signed_with_the_master_keys_cursor_key() {
        const MASTER: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let master = MasterKey::parse(MASTER).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prod = prod_with(data_roots(dir.path()));
        let env = &prod.env;
        let keys = crate::keys::parse(&keys_toml(), env).unwrap();
        let cursors = crate::cursor_keys::derived(&master).unwrap();
        assert_eq!(cursors.active_kid().as_str(), "master-0");
        let app = test_app_with(env, keys, Arc::new(cursors)).await;
        let ui = Arc::new(Catalog::default());
        let servers = start(env, Arc::clone(&prod.recorder), ui, LISTEN, app)
            .await
            .unwrap();
        let addr = servers.addrs.api;
        let bearer = format!("Bearer {}", secret());
        let key = [("authorization", bearer.as_str())];
        let (from, to) = ("2026-11-16T00:00:00Z", "2026-11-17T00:00:00Z");
        let window = TimeWindow::new(
            Timestamp::parse_rfc3339(from).unwrap(),
            Timestamp::parse_rfc3339(to).unwrap(),
        )
        .unwrap();
        // A page cursor of the trace list as the API issues it; the filter hash of the unfiltered list covers
        // the empty canonical form.
        let now = env.clock.wall_now();
        let cursor = Cursor::new(
            ProjectId::parse(PROJECT).unwrap(),
            FilterHash::of(&[]),
            Sort::NewestFirst,
            Position::TracePage {
                window,
                as_of: now,
                last: (window.from(), 0, TraceId::from_bytes([1; 16])),
            },
            now,
        );
        let page = |text: String| {
            format!("/api/v1/projects/{PROJECT}/traces?from={from}&to={to}&cursor={text}")
        };
        let same = crate::cursor_keys::derived(&master).unwrap();
        let (status, headers, body) = http(env, addr, "GET", &page(same.seal(&cursor)), &key).await;
        assert_problem(
            &status,
            &headers,
            &body,
            "503 Service Unavailable",
            "telemetry_store_unavailable",
        );
        let other = MasterKey::parse(&MASTER.replace("00", "ff")).unwrap();
        for stranger in [
            crate::cursor_keys::derived(&other).unwrap(),
            crate::cursor_keys::random(env).unwrap(),
        ] {
            let uri = page(stranger.seal(&cursor));
            let (status, headers, body) = http(env, addr, "GET", &uri, &key).await;
            assert_problem(
                &status,
                &headers,
                &body,
                "400 Bad Request",
                "cursor_invalid",
            );
        }

        shut_down(&prod, servers).await;
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
        assert_eq!(keys.verify_pair(&public(), &secret()), Ok(principal));
        assert!(crate::keys::parse("", env).unwrap().is_empty());

        let entry = "[[project_key]]\nproject_id = \"{project}\"\npublic_key = \"{public}\"\n\
                     secret = \"{secret}\"\n";
        let other_secret = format!("csk_{}", "8".repeat(43));
        let other_public = format!("cpk_{}", "8".repeat(43));
        let without_public = "[[project_key]]\nproject_id = \"{project}\"\nsecret = \"{secret}\"\n";
        let refused = [
            (
                entry.replace("{project}", "nope"),
                "project_key 1: project_id",
            ),
            (
                entry.replace("{secret}", "csk_short"),
                "project_key 1: secret",
            ),
            (
                entry.replace("{public}", "cpk_short"),
                "project_key 1: public_key",
            ),
            (
                entry.replace("{public}", &other_secret),
                "project_key 1: public_key",
            ),
            (
                without_public.to_owned(),
                "missing field `public_key`; every [[project_key]] needs the public key of its pair",
            ),
            (format!("{entry}note = \"x\"\n"), "unknown field"),
            (format!("{entry}{entry}"), "same secret"),
            (
                format!("{entry}{}", entry.replace("{secret}", &other_secret)),
                "same public key",
            ),
            (
                format!("{entry}{}", entry.replace("{public}", &other_public)),
                "same secret",
            ),
            (entry.replace("{secret}\"", "{secret}"), "not valid"),
            ("[[project_key]\n".to_owned(), "not valid"),
        ];
        for (text, expected) in refused {
            let text = text
                .replace("{project}", PROJECT)
                .replace("{public}", &public())
                .replace("{secret}", &secret());
            let error = format!("{:#}", crate::keys::parse(&text, env).unwrap_err());
            assert!(error.contains(expected), "{error}");
            for key in [&secret()[4..], &public()[4..], &other_secret[4..]] {
                assert!(!error.contains(key), "the error echoes a key: {error}");
            }
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
        let prod = prod_with(BTreeMap::from([(Root::Wal, PathBuf::from(dir))]));
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
