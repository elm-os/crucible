//! Idle connections of the store's pool against a real server: how many connections a sequence of reads opens,
//! counted at `Net::connect`.
//!
//! The server keeps an idle connection for its `keep_alive_timeout` (10 s in the configuration file of 26.3, 30 s
//! in that of 26.8). A pool that drops idle connections sooner makes every read after a pause open a new one.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use crucible_env::{
    Env, Instant, Listener, Net, NetError, StoreHealth, Stream, TelemetryStore, TlsRoots,
};
use crucible_store_clickhouse::{ClickHouseStore, StoreConfig};

use crate::{Ctx, expect};

/// The pause between two reads: longer than the 2 s the pool used to keep an idle connection, shorter than the
/// server's keep-alive timeout on every version of the matrix.
const GAP: Duration = Duration::from_secs(3);
/// Reads in the sequence.
const READS: u64 = 3;

/// A `Net` that counts the connections opened through it.
#[derive(Debug)]
struct CountingNet {
    inner: Arc<dyn Net>,
    connects: AtomicU64,
}

impl CountingNet {
    fn connects(&self) -> u64 {
        self.connects.load(Ordering::Relaxed)
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

/// A store over `ctx`'s environment whose connections are counted. Its ids come from a stream of their own.
fn counted_store(ctx: &Ctx, config: StoreConfig) -> (ClickHouseStore, Arc<CountingNet>) {
    let env = &ctx.env;
    let net = Arc::new(CountingNet {
        inner: Arc::clone(&env.net),
        connects: AtomicU64::new(0),
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

/// After startup detection, reads [`GAP`] apart share one connection: the first may open it, and every later
/// read finds it in the pool. Measured as the connections opened at `Net::connect` by the reads after a pause.
pub(crate) async fn reuse(ctx: &Ctx) -> Result<String> {
    let (store, net) = counted_store(ctx, ctx.config.clone());
    store.detect().await.context("detect")?;
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
        "{READS} reads {gap} s apart after detection opened {} connection, none after a pause",
        first + later
    ))
}
