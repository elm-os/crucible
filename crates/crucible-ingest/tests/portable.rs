//! Handler tests over the real WAL on the production `PortableDisk` in a temporary directory: requests in every
//! encoding are acknowledged and their frames are durable across a reopen; the CPU budget bounds the blocking
//! jobs that prepare frames, and small bodies skip them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use crucible_env::{Env, Root, ShutdownSignal, Spawner, TaskAbort};
use crucible_env_prod::{Prod, ProdConfig, WalBackend, build};
use crucible_ingest::IngestConfig;
use crucible_otlp::transcode_json;
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::ContentOrigin;
use futures::future::BoxFuture;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message as _;

async fn open(dir: &Path, config: &WalConfig) -> (Prod, Wal) {
    let prod_config = ProdConfig {
        roots: BTreeMap::from([(Root::Wal, dir.to_path_buf())]),
        wal_backend: WalBackend::Portable,
    };
    let prod = build(tokio::runtime::Handle::current(), &prod_config).unwrap();
    let (wal, _report) = Wal::open(&prod.env, config.clone()).await.unwrap();
    (prod, wal)
}

/// Drops the last WAL handle and waits for the committer to stop, so that the directory has one writer when
/// it is opened again.
async fn close(prod: Prod, wal: Wal) {
    drop(wal);
    while !prod.spawner.task_names().is_empty() {
        prod.env.clock.sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_requests_are_durable_on_a_real_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let config = wal_config();
    let (prod, wal) = open(dir.path(), &config).await;
    let router = router(&prod.env, &wal, &config, IngestConfig::default());

    let now = u64::try_from(prod.env.clock.wall_now().micros()).unwrap() * 1_000;
    let request = export(vec![
        good_span(1, now),
        span(vec![0; 16], vec![1; 8], now, "zero trace id"),
        good_span(3, now - DAY_NANOS),
    ]);
    let protobuf = request.encode_to_vec();
    let json = serde_json::to_vec(&request).unwrap();
    let sent = [
        (
            Req::new(protobuf.clone(), PROTOBUF),
            ContentOrigin::Protobuf,
        ),
        (Req::json(json.clone()), ContentOrigin::Json),
        (
            Req::new(protobuf.clone(), PROTOBUF).gzip(),
            ContentOrigin::Protobuf,
        ),
        (Req::json(json.clone()).gzip(), ContentOrigin::Json),
    ];
    for (req, origin) in &sent {
        let reply = send(&router, req.clone()).await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
        let rejected = match origin {
            ContentOrigin::Protobuf => reply.rejected_spans(),
            ContentOrigin::Json => reply.json()["partialSuccess"]["rejectedSpans"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap(),
        };
        assert_eq!(rejected, 1);
    }
    drop(router);
    close(prod, wal).await;

    let (prod, wal) = open(dir.path(), &config).await;
    let written = payloads(&wal).await;
    assert_eq!(written.len(), sent.len());
    let transcoded = transcode_json(&json).unwrap();
    for (payload, (_, origin)) in written.iter().zip(&sent) {
        let frame = envelope(payload);
        assert_common_envelope(&frame);
        assert_eq!(frame.content_origin, *origin);
        let canonical = match origin {
            ContentOrigin::Protobuf => &protobuf,
            ContentOrigin::Json => &transcoded,
        };
        assert_eq!(frame.otlp, canonical.as_slice());
        assert_eq!(frame.span_count, 3);
        assert_eq!(rejected_ordinals(frame.rejected), [1]);
    }
    close(prod, wal).await;
}

/// Watches the `ingest-prepare` jobs that ingest hands to the inner spawner: how many it spawned, and the most
/// that ran at once. With `hold_until`, each job first holds its thread until that many jobs run at once or 100 ms
/// pass, so that jobs which are allowed to overlap do; a job is counted while it holds, before its work.
#[derive(Debug)]
struct Watch {
    inner: Arc<dyn Spawner>,
    jobs: Arc<Jobs>,
}

#[derive(Debug, Default)]
struct Jobs {
    hold_until: Option<usize>,
    counts: Mutex<Counts>,
    changed: Condvar,
}

#[derive(Clone, Copy, Debug, Default)]
struct Counts {
    spawned: usize,
    running: usize,
    most: usize,
}

impl Jobs {
    fn run(&self, job: Box<dyn FnOnce() + Send>) {
        let mut counts = self.counts.lock().unwrap();
        counts.running += 1;
        counts.most = counts.most.max(counts.running);
        self.changed.notify_all();
        if let Some(all) = self.hold_until {
            counts = self
                .changed
                .wait_timeout_while(counts, Duration::from_millis(100), |c| c.running < all)
                .unwrap()
                .0;
        }
        // The job leaves the count before its work, at whose end a job gives its permit back: a job let in by that
        // permit never finds this one still counted.
        counts.running -= 1;
        drop(counts);
        job();
    }

    fn counts(&self) -> Counts {
        *self.counts.lock().unwrap()
    }
}

impl Spawner for Watch {
    fn spawn_boxed(&self, name: &'static str, fut: BoxFuture<'static, ()>) -> Box<dyn TaskAbort> {
        self.inner.spawn_boxed(name, fut)
    }

    fn spawn_blocking_boxed(
        &self,
        name: &'static str,
        f: Box<dyn FnOnce() + Send>,
    ) -> Box<dyn TaskAbort> {
        if name != "ingest-prepare" {
            return self.inner.spawn_blocking_boxed(name, f);
        }
        self.jobs.counts.lock().unwrap().spawned += 1;
        let jobs = Arc::clone(&self.jobs);
        self.inner
            .spawn_blocking_boxed(name, Box::new(move || jobs.run(f)))
    }

    fn shutdown(&self) -> ShutdownSignal {
        self.inner.shutdown()
    }
}

/// `prod`'s environment with its `ingest-prepare` jobs watched.
fn watched(prod: &Prod, hold_until: Option<usize>) -> (Env, Arc<Jobs>) {
    let jobs = Arc::new(Jobs {
        hold_until,
        ..Jobs::default()
    });
    let spawner = Arc::new(Watch {
        inner: Arc::clone(&prod.env.spawner),
        jobs: Arc::clone(&jobs),
    });
    let env = Env::new(
        Arc::clone(&prod.env.clock),
        Arc::clone(&prod.env.rng),
        spawner,
        Arc::clone(&prod.env.disk),
        Arc::clone(&prod.env.net),
        Arc::clone(&prod.env.metrics),
    );
    (env, jobs)
}

/// `count` spans of about `bytes` bytes each, starting at `now`.
fn spans(count: u8, bytes: usize, now: u64) -> ExportTraceServiceRequest {
    export(
        (0..count)
            .map(|n| {
                let mut span = good_span(n, now);
                span.attributes
                    .push(string("gen_ai.input.messages", &"word ".repeat(bytes / 5)));
                span
            })
            .collect(),
    )
}

fn now_nanos(prod: &Prod) -> u64 {
    u64::try_from(prod.env.clock.wall_now().micros()).unwrap() * 1_000
}

/// The CPU budget bounds ingest's blocking work: with the default of four permits, 16 requests at once run at
/// most four `ingest-prepare` jobs at a time, and use all four.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ingest_prepare_jobs_never_exceed_their_permits() {
    let dir = tempfile::tempdir().unwrap();
    let config = wal_config();
    let (prod, wal) = open(dir.path(), &config).await;
    let (env, jobs) = watched(&prod, Some(16));
    let router = router(&env, &wal, &config, IngestConfig::default());

    // A gzip body always takes the blocking path.
    let request = Req::protobuf(&spans(20, 1_024, now_nanos(&prod))).gzip();
    let replies = futures::future::join_all((0..16).map(|_| send(&router, request.clone()))).await;
    for reply in &replies {
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    }
    let counts = jobs.counts();
    assert_eq!(counts.spawned, 16);
    assert!(
        counts.most <= 4,
        "{} ingest-prepare jobs ran at once, more than the 4 permits",
        counts.most
    );
    assert_eq!(counts.most, 4, "the jobs did not use every permit");
    drop(router);
    close(prod, wal).await;
}

/// A small uncompressed body is prepared on the async worker, which costs less than the hop to a blocking
/// thread; a gzip body, whose inflated size is unknown, and a large body each take one `ingest-prepare` job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_plain_bodies_skip_the_blocking_hop() {
    let dir = tempfile::tempdir().unwrap();
    let config = wal_config();
    let (prod, wal) = open(dir.path(), &config).await;
    let (env, jobs) = watched(&prod, None);
    let router = router(&env, &wal, &config, IngestConfig::default());
    let now = now_nanos(&prod);

    let small = export(vec![good_span(1, now), good_span(3, now)]);
    for _ in 0..3 {
        let reply = send(&router, Req::protobuf(&small)).await;
        assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    }
    assert_eq!(
        jobs.counts().spawned,
        0,
        "3 small protobuf requests took the blocking hop"
    );
    let reply = send(&router, Req::protobuf(&small).gzip()).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(jobs.counts().spawned, 1);
    let reply = send(&router, Req::protobuf(&spans(100, 4_096, now))).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    assert_eq!(jobs.counts().spawned, 2);
    drop(router);
    close(prod, wal).await;
}

/// A blocking preparation records how long it waited for its permit and how long it held it, under the stage
/// `ingest_prepare`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_permit_wait_and_hold_are_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let config = wal_config();
    let (prod, wal) = open(dir.path(), &config).await;
    let router = router(&prod.env, &wal, &config, IngestConfig::default());

    let request = Req::protobuf(&spans(2, 1_024, now_nanos(&prod))).gzip();
    let reply = send(&router, request).await;
    assert_eq!(reply.status, StatusCode::OK, "{reply:?}");
    let text = prod.recorder.render_prometheus();
    for metric in [
        "crucible_cpu_permit_wait_microseconds",
        "crucible_cpu_permit_hold_microseconds",
    ] {
        let line = format!("{metric}_count{{stage=\"ingest_prepare\"}} 1");
        assert!(text.contains(&line), "no `{line}` in the metrics:\n{text}");
    }
    drop(router);
    close(prod, wal).await;
}
