//! The open-loop runner against the stub server on loopback, through the production `Env`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use crucible_env::{Env, JoinHandle, NetError};
use crucible_env_prod::ProdConfig;
use crucible_loadgen::stub::{StubConfig, serve};
use crucible_loadgen::{
    ErrorCounts, Pool, PoolConfig, ResolveError, RunConfig, RunError, SpanSize, resolve, run,
};
use http::StatusCode;

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

fn env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

async fn start_stub(
    env: &Env,
    config: StubConfig,
) -> (SocketAddr, JoinHandle<Result<(), NetError>>) {
    let listener = env.net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    (
        addr,
        env.spawn("stub", serve(env.clone(), listener, config)),
    )
}

fn pool(env: &Env, spans_per_request: u32) -> Arc<Pool> {
    let config = PoolConfig {
        span_size: SpanSize::Kib1,
        spans_per_request,
        traces_per_request: 1,
        rootless: false,
        templates: 3,
        seed: 5,
    };
    Arc::new(Pool::build(config, env.new_id()).unwrap())
}

fn config(target: SocketAddr, rate: u64, duration: Duration, concurrency: usize) -> RunConfig {
    RunConfig {
        target,
        authority: target.to_string(),
        rate,
        duration,
        concurrency,
        gzip: false,
        gzip_threads: 2,
        timeout: Duration::from_secs(5),
        headers: Vec::new(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_scheduled_request_is_sent_and_acked_in_both_lanes() {
    let env = env();
    let (addr, stub) = start_stub(&env, StubConfig::default()).await;
    for gzip in [false, true] {
        let pool = pool(&env, 4);
        let config = RunConfig {
            gzip,
            ..config(addr, 400, Duration::from_millis(500), 8)
        };
        let outcome = run(&env, Arc::clone(&pool), &config).await.unwrap();
        let report = &outcome.report;
        let counts = &report.outcome;
        assert_eq!(report.schedule.requests, 200);
        // The stub answers 400 to a body it cannot decode, so 200 everywhere also proves the bodies valid.
        assert_eq!(counts.status_counts.get(&200), Some(&200), "{counts:?}");
        assert_eq!(
            (counts.sent, counts.responses, counts.acked),
            (200, 200, 200)
        );
        assert_eq!(counts.acked_spans, 800);
        assert_eq!(counts.errors, ErrorCounts::default());
        assert_eq!(report.latency_us.count, 200);
        assert_eq!(report.send_lag_us.count, 200);
        assert!(report.not_acked_requests.is_empty());
        assert_eq!(outcome.ledger.acked_requests(), 200);
        assert_eq!(outcome.ledger.run_id(), pool.run_id());
        assert_eq!(report.run_id, pool.run_id().to_string());
        // Nothing is sent before it is due, so the generator can never beat the target rate.
        assert!(
            counts.achieved_rps <= 400.0 + 1e-9,
            "{}",
            counts.achieved_rps
        );
        if gzip {
            assert!(report.lane.wire_bytes_mean < report.lane.request_bytes_mean / 2.0);
            assert_eq!(report.lane.gzip_threads, 2);
        } else {
            assert_eq!(report.lane.wire_bytes_mean, report.lane.request_bytes_mean);
            assert_eq!(report.lane.gzip_threads, 0);
        }
        // RFC 3339 with six fractional digits, in UTC.
        assert_eq!(report.started_at.len(), 27, "{}", report.started_at);
        assert!(report.started_at.ends_with('Z'), "{}", report.started_at);
    }
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_request_shape_decodes_at_the_stub() {
    // Several traces per request, with and without roots, gzipped by one compression thread for eight
    // connections: the stub answers 400 to any body that does not decode.
    let env = env();
    let (addr, stub) = start_stub(&env, StubConfig::default()).await;
    for rootless in [false, true] {
        let shape = PoolConfig {
            span_size: SpanSize::Kib1,
            spans_per_request: 10,
            traces_per_request: 3,
            rootless,
            templates: 2,
            seed: 9,
        };
        let pool = Arc::new(Pool::build(shape, env.new_id()).unwrap());
        let gzipped = RunConfig {
            gzip: true,
            gzip_threads: 1,
            ..config(addr, 200, Duration::from_millis(500), 8)
        };
        let report = run(&env, pool, &gzipped).await.unwrap().report;
        assert_eq!(
            report.outcome.status_counts.get(&200),
            Some(&100),
            "{:?}",
            report.outcome
        );
        assert_eq!(
            (
                report.lane.traces_per_request,
                report.lane.rootless,
                report.lane.gzip_threads
            ),
            (3, rootless, 1)
        );
    }
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn latency_counts_from_the_intended_send_time() {
    // The stub takes 50 ms per request and only 2 requests may be in flight, so at most 40 requests per
    // second complete against a schedule of 200 per second. Every request is still sent, in order, and the
    // queueing shows in the latency, not in the service time.
    let env = env();
    let delay = Duration::from_millis(50);
    let (addr, stub) = start_stub(
        &env,
        StubConfig {
            delay,
            ..StubConfig::default()
        },
    )
    .await;
    let outcome = run(
        &env,
        pool(&env, 1),
        &config(addr, 200, Duration::from_millis(500), 2),
    )
    .await
    .unwrap();
    let report = outcome.report;
    assert_eq!((report.outcome.sent, report.outcome.acked), (100, 100));
    assert!(
        report.service_time_us.p50 >= 50_000,
        "{:?}",
        report.service_time_us
    );
    // Request k is due at 5k ms but cannot complete before (k / 2 + 1) * 50 ms: request 50 waits a second
    // and request 99 two.
    assert!(report.latency_us.p50 >= 500_000, "{:?}", report.latency_us);
    assert!(
        report.latency_us.max >= 1_500_000,
        "{:?}",
        report.latency_us
    );
    assert!(report.outcome.achieved_rps < 100.0);
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn error_statuses_are_counted_and_never_acked() {
    let env = env();
    let (addr, stub) = start_stub(
        &env,
        StubConfig {
            status: StatusCode::SERVICE_UNAVAILABLE,
            ..StubConfig::default()
        },
    )
    .await;
    let outcome = run(
        &env,
        pool(&env, 2),
        &config(addr, 200, Duration::from_millis(250), 4),
    )
    .await
    .unwrap();
    let report = &outcome.report;
    assert_eq!(report.outcome.status_counts.get(&503), Some(&50));
    assert_eq!((report.outcome.responses, report.outcome.acked), (50, 0));
    assert_eq!(report.latency_us.count, 0);
    assert_eq!(report.not_acked_requests, [[0, 50]]);
    assert_eq!(outcome.ledger.acked_requests(), 0);
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn timed_out_requests_are_errors_and_are_not_resent() {
    let env = env();
    let (addr, stub) = start_stub(
        &env,
        StubConfig {
            delay: Duration::from_millis(500),
            ..StubConfig::default()
        },
    )
    .await;
    let config = RunConfig {
        timeout: Duration::from_millis(50),
        ..config(addr, 100, Duration::from_millis(100), 2)
    };
    let report = run(&env, pool(&env, 1), &config).await.unwrap().report;
    assert_eq!(
        report.outcome.errors,
        ErrorCounts {
            timeout: 10,
            ..ErrorCounts::default()
        }
    );
    assert_eq!(
        (
            report.outcome.sent,
            report.outcome.responses,
            report.outcome.acked
        ),
        (10, 0, 0)
    );
    assert_eq!(report.send_lag_us.count, 10);
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_the_server_closes_are_replaced_before_sending() {
    // The stub closes every connection after one response, so each request after the first finds its
    // connection closed and must open a new one instead of failing.
    let env = env();
    let (addr, stub) = start_stub(
        &env,
        StubConfig {
            keep_alive: false,
            ..StubConfig::default()
        },
    )
    .await;
    let report = run(
        &env,
        pool(&env, 1),
        &config(addr, 100, Duration::from_millis(300), 2),
    )
    .await
    .unwrap()
    .report;
    assert_eq!(report.outcome.errors, ErrorCounts::default());
    assert_eq!((report.outcome.sent, report.outcome.acked), (30, 30));
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_target_fails_before_the_schedule_starts() {
    let env = env();
    let listener = env.net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let result = run(
        &env,
        pool(&env, 1),
        &config(addr, 10, Duration::from_secs(1), 1),
    )
    .await;
    assert!(matches!(result, Err(RunError::Connect(_))), "{result:?}");
}

#[tokio::test]
async fn unrunnable_schedules_are_refused() {
    let env = env();
    let empty = config(LOOPBACK, 1, Duration::from_millis(500), 1);
    let no_connections = config(LOOPBACK, 10, Duration::from_secs(1), 0);
    let no_compression = RunConfig {
        gzip: true,
        gzip_threads: 0,
        ..config(LOOPBACK, 10, Duration::from_secs(1), 1)
    };
    for config in [empty, no_connections, no_compression] {
        let result = run(&env, pool(&env, 1), &config).await;
        assert!(matches!(result, Err(RunError::Config(_))), "{result:?}");
    }
}

#[tokio::test]
async fn targets_resolve_from_literals_and_names() {
    let env = env();
    let literal: SocketAddr = "127.0.0.1:4318".parse().unwrap();
    assert_eq!(resolve(&env, "127.0.0.1:4318").await.unwrap(), literal);
    let named = resolve(&env, "localhost:4318").await.unwrap();
    assert!(named.ip().is_loopback() && named.port() == 4318, "{named}");
    assert!(matches!(
        resolve(&env, "localhost").await,
        Err(ResolveError::Syntax)
    ));
}
