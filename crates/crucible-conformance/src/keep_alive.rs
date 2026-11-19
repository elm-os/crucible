//! Idle connections of the store's pool against a real server: how many connections a sequence of reads opens,
//! counted at `Net::connect`, and what a read does when its connection cannot be opened.
//!
//! The server keeps an idle connection for its `keep_alive_timeout` (10 s in the configuration file of 26.3, 30 s
//! in that of 26.8). A pool that drops idle connections sooner makes every read after a pause open a new one.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use async_trait::async_trait;
use crucible_env::metrics::registry;
use crucible_env::{
    Env, Instant, Listener, Net, NetError, StoreHealth, Stream, TelemetryStore, TlsRoots,
};
use crucible_schema::ServiceIdentity;
use crucible_store_clickhouse::{ClickHouseStore, StoreConfig};

use crate::{Ctx, expect, statement};

/// The pause between two reads: longer than the 2 s the pool used to keep an idle connection, shorter than the
/// server's keep-alive timeout on every version of the matrix.
const GAP: Duration = Duration::from_secs(3);
/// Reads in the sequence.
const READS: u64 = 3;
/// The pool keeps an idle connection for the server's timeout less this.
const MARGIN: Duration = Duration::from_secs(1);

/// A `Net` that counts the connections opened and the names resolved through it, and can fail the next
/// resolutions, so that a statement fails before any byte of it is sent.
#[derive(Debug)]
struct CountingNet {
    inner: Arc<dyn Net>,
    connects: AtomicU64,
    resolves: AtomicU64,
    failing_resolves: AtomicU64,
}

impl CountingNet {
    fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
    }

    fn resolves(&self) -> u64 {
        self.resolves.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Net for CountingNet {
    async fn listen(&self, addr: SocketAddr) -> Result<Box<dyn Listener>, NetError> {
        self.inner.listen(addr).await
    }

    async fn connect(&self, addr: SocketAddr, deadline: Instant) -> Result<Stream, NetError> {
        self.connects.fetch_add(1, Ordering::Relaxed);
        self.inner.connect(addr, deadline).await
    }

    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, NetError> {
        self.resolves.fetch_add(1, Ordering::Relaxed);
        let fail = self
            .failing_resolves
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if fail {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{host} did not resolve (a failure the conformance row injected)"),
            )));
        }
        self.inner.resolve(host).await
    }

    async fn tls_connect(
        &self,
        stream: Stream,
        server_name: &str,
        roots: &TlsRoots,
    ) -> Result<Stream, NetError> {
        self.inner.tls_connect(stream, server_name, roots).await
    }
}

/// A store over `ctx`'s environment whose network is counted. Its ids come from a stream of their own.
fn counted_store(ctx: &Ctx, config: StoreConfig) -> (ClickHouseStore, Arc<CountingNet>) {
    let env = &ctx.env;
    let net = Arc::new(CountingNet {
        inner: Arc::clone(&env.net),
        connects: AtomicU64::new(0),
        resolves: AtomicU64::new(0),
        failing_resolves: AtomicU64::new(0),
    });
    let counted = Env::new(
        Arc::clone(&env.clock),
        env.rng.fork("crucible-conformance/keep-alive"),
        Arc::clone(&env.spawner),
        Arc::clone(&env.disk),
        Arc::clone(&net) as Arc<dyn Net>,
        Arc::clone(&env.metrics),
    );
    (ClickHouseStore::new(counted, config), net)
}

async fn read(store: &ClickHouseStore) -> Result<()> {
    match store.health().await {
        StoreHealth::Healthy => Ok(()),
        StoreHealth::Unavailable { reason } => bail!("a read failed: {reason}"),
    }
}

/// The server's `keep_alive_timeout`.
async fn server_keep_alive(ctx: &Ctx) -> Result<Duration> {
    let secs = ctx
        .admin_count("SELECT value FROM system.server_settings WHERE name = 'keep_alive_timeout'")
        .await?;
    Ok(Duration::from_secs(secs))
}

/// Connections the store metric has counted, over every store of the environment.
fn metric(ctx: &Ctx) -> u64 {
    ctx.env
        .metrics
        .counter(&registry::CLICKHOUSE_CONNECTIONS_OPENED, &[])
        .get()
}

/// After startup detection, reads [`GAP`] apart share one connection: the first may open it, and every later
/// read finds it in the pool. Measured as the connections opened at `Net::connect` by the reads after a pause.
pub(crate) async fn reuse(ctx: &Ctx) -> Result<String> {
    let keep_alive = server_keep_alive(ctx).await?;
    let (store, net) = counted_store(ctx, ctx.config.clone());
    store.detect().await.context("detect")?;
    let idle = store.idle_timeout();
    expect(idle == keep_alive.saturating_sub(MARGIN), || {
        format!(
            "after detection the pool keeps an idle connection for {idle:?}; the server keeps one for \
             {keep_alive:?}"
        )
    })?;
    let start = net.connects();
    read(&store).await?;
    let first = net.connects() - start;
    for _ in 1..READS {
        ctx.env.clock.sleep(GAP).await;
        read(&store).await?;
    }
    let later = net.connects() - start - first;
    let (gap, paused) = (GAP.as_secs(), READS - 1);
    expect(later == 0, || {
        format!(
            "the {paused} reads after a {gap} s pause opened {later} connections: the pool drops an idle \
             connection sooner than the server does"
        )
    })?;
    Ok(format!(
        "the server keeps idle connections for {keep_alive:?} and the pool for {idle:?}; of {READS} reads {gap} s \
         apart the first opened {first} connections and the {paused} after a pause opened none"
    ))
}

/// With the pool's idle timeout above the server's, as a `clickhouse.idle_timeout` set too high would make it,
/// a read after the server has closed the pooled connection still succeeds: it goes out on a new connection.
pub(crate) async fn above_server(ctx: &Ctx) -> Result<String> {
    let keep_alive = server_keep_alive(ctx).await?;
    let idle = keep_alive + Duration::from_secs(60);
    let (store, net) = counted_store(
        ctx,
        StoreConfig {
            idle_timeout: Some(idle),
            ..ctx.config.clone()
        },
    );
    store.detect().await.context("detect")?;
    expect(store.idle_timeout() == idle, || {
        format!(
            "detection replaced the configured idle timeout {idle:?} with {:?}",
            store.idle_timeout()
        )
    })?;
    read(&store).await?;
    let pause = keep_alive + Duration::from_secs(2);
    ctx.env.clock.sleep(pause).await;
    let (start, counted) = (net.connects(), metric(ctx));
    read(&store)
        .await
        .context("the read after the server closed the pooled connection")?;
    let opened = net.connects() - start;
    expect(opened == 1, || {
        format!(
            "the read after a {pause:?} pause opened {opened} connections; the server should have closed the \
             pooled one after {keep_alive:?}"
        )
    })?;
    let metered = metric(ctx) - counted;
    expect(metered == opened, || {
        format!(
            "crucible_clickhouse_connections_opened_total rose by {metered} while Net::connect opened {opened}"
        )
    })?;
    Ok(format!(
        "with the pool keeping idle connections for {idle:?} and the server for {keep_alive:?}, the read after a \
         {pause:?} pause succeeded on a new connection, which the metric counted"
    ))
}

/// Whether the host of an `http(s)://host:port` URL is an address rather than a name.
fn host_is_address(url: &str) -> bool {
    let authority = url.split_once("://").map_or(url, |(_, rest)| rest);
    let host = authority.split(['/', '?']).next().unwrap_or_default();
    host.starts_with('[')
        || host
            .rsplit_once(':')
            .map_or(host, |(host, _)| host)
            .parse::<IpAddr>()
            .is_ok()
}

/// A read whose connection could not be opened changed nothing, so it is sent once more, and only once, on the
/// short-read path and on the streaming path of typed plans. The failure is a name that does not resolve, so no
/// byte reaches the server; counted at `Net::resolve`, one resolution per attempt.
pub(crate) async fn resend(ctx: &Ctx) -> Result<String> {
    if host_is_address(&ctx.config.url) {
        return Ok(format!(
            "not run: {} names an address, so no name resolution can fail",
            ctx.config.url
        ));
    }
    let sql = statement("SELECT number FROM numbers(10)");
    for (failures, succeeds) in [(1, true), (2, false)] {
        let (store, net) = counted_store(ctx, ctx.config.clone());
        net.failing_resolves.store(failures, Ordering::Relaxed);
        let short = store.health().await;
        let (store, streaming_net) = counted_store(ctx, ctx.config.clone());
        streaming_net
            .failing_resolves
            .store(failures, Ordering::Relaxed);
        let streaming = store
            .raw_read(ServiceIdentity::Maint, &sql, Duration::from_secs(30), false)
            .map_err(|error| anyhow!("{error}"))?
            .drain()
            .await;
        for (path, attempts, ok, outcome) in [
            (
                "short read",
                net.resolves(),
                short == StoreHealth::Healthy,
                format!("{short:?}"),
            ),
            (
                "streaming read",
                streaming_net.resolves(),
                streaming.is_ok(),
                format!("{streaming:?}"),
            ),
        ] {
            expect(attempts == 2 && ok == succeeds, || {
                format!(
                    "a {path} whose first {failures} name resolutions fail made {attempts} attempts and ended \
                     {outcome}; expected 2 attempts and {}",
                    if succeeds { "success" } else { "failure" }
                )
            })?;
        }
    }
    Ok(
        "a short read and a streaming read whose first connection attempt fails are sent once more and succeed; \
         when the second attempt fails too they fail without a third"
            .to_owned(),
    )
}
