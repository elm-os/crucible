//! The read generator and the ack-to-API probe against small servers on loopback, through the production `Env`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use crucible_env::{Env, JoinHandle, NetError};
use crucible_env_prod::ProdConfig;
use crucible_loadgen::stub::{StubConfig, serve};
use crucible_loadgen::{
    Dataset, FreshConfig, ReadClass, ReadConfig, ReadError, Think, fresh, read,
};
use http::{Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use prost::Message;

const LOOPBACK: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

fn env() -> Env {
    crucible_env_prod::build(tokio::runtime::Handle::current(), &ProdConfig::default())
        .unwrap()
        .env
}

/// A run report of 20 requests of 10 spans in 2 traces, of which requests 5 to 9 were not acknowledged.
fn dataset() -> Dataset {
    let report = r#"{"schema":"crucible-loadgen.run.v1","run_id":"019a6b2e-7c3d-7e4f-8a5b-6c7d8e9f0a1b",
        "started_at":"2026-11-18T21:30:00.123456Z","lane":{"spans_per_request":10,"traces_per_request":2,
        "spread_s":86400},"schedule":{"target_rps":100,"requests":20},"not_acked_requests":[[5,10]]}"#;
    Dataset::from_reports([report]).unwrap()
}

fn class(name: &str, nonempty: Option<&str>) -> ReadClass {
    ReadClass {
        name: name.to_owned(),
        method: "GET".to_owned(),
        path: "/api/v1/projects/{project}/traces/{trace_id}?hint_time={trace_start}&from={from}&to={to}"
            .to_owned(),
        body: None,
        nonempty: nonempty.map(str::to_owned),
    }
}

fn config(target: SocketAddr, classes: Vec<ReadClass>) -> ReadConfig {
    ReadConfig {
        target,
        authority: target.to_string(),
        headers: Vec::new(),
        project: "p".to_owned(),
        classes,
        rate: 200.0,
        duration: Duration::from_millis(500),
        think: Think::Exponential,
        concurrency: 4,
        timeout: Duration::from_secs(5),
        window: Duration::from_secs(3_600),
        seed: 3,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_read_is_counted_per_class() {
    // The stub answers every request 200 with an empty protobuf body: fine for a class that checks nothing,
    // empty for a class that wants a non-empty JSON array.
    let env = env();
    let listener = env.net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stub = env.spawn("stub", serve(env.clone(), listener, StubConfig::default()));
    let classes = vec![class("plain", None), class("listed", Some("/data"))];
    let report = read(&env, dataset(), &config(addr, classes)).await.unwrap();
    assert_eq!(report.schedule.requests, 100);
    assert_eq!((report.dataset_runs, report.dataset_requests), (1, 15));
    let (plain, listed) = (&report.classes[0], &report.classes[1]);
    assert_eq!((plain.sent, plain.ok, plain.empty), (50, 50, 0));
    assert_eq!((listed.sent, listed.ok, listed.empty), (50, 0, 50));
    assert_eq!(plain.status_counts.get(&200), Some(&50));
    assert_eq!(plain.latency_us.count, 50);
    assert_eq!(listed.latency_us.count, 0);
    assert!(plain.latency_us.p50 >= plain.service_time_us.p50);
    // The halves split the answers by due time: both halves of the schedule had reads, and none is counted twice.
    let [first, second] = plain.latency_halves_us;
    assert!(first.count > 0 && second.count > 0);
    assert_eq!(first.count + second.count, plain.latency_us.count);
    assert_eq!(listed.latency_halves_us[0].count + listed.latency_halves_us[1].count, 0);
    stub.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrunnable_reads_are_refused() {
    let env = env();
    let bad_method = ReadClass {
        method: "DELETE".to_owned(),
        ..class("x", None)
    };
    for config in [
        config(LOOPBACK, Vec::new()),
        ReadConfig {
            rate: 0.0,
            ..config(LOOPBACK, vec![class("x", None)])
        },
        config(LOOPBACK, vec![bad_method]),
    ] {
        let result = read(&env, dataset(), &config).await;
        assert!(
            matches!(result, Err(ReadError::Run(_) | ReadError::Class(..))),
            "{result:?}"
        );
    }
}

/// Acknowledges every OTLP request and remembers its trace ids; answers any other request with a JSON body
/// that names every trace id seen so far, as the trace list would.
async fn mini_crucible(env: &Env) -> (SocketAddr, JoinHandle<Result<(), NetError>>) {
    let mut listener = env.net.listen(LOOPBACK).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let task_env = env.clone();
    let task = env.spawn("mini-crucible", async move {
        loop {
            let (stream, _) = listener.accept().await?;
            let seen = Arc::clone(&seen);
            let service = service_fn(move |request: Request<Incoming>| {
                let seen = Arc::clone(&seen);
                async move {
                    let post = request.method() == http::Method::POST;
                    let body = request.into_body().collect().await.unwrap().to_bytes();
                    let answer = if post {
                        let decoded = ExportTraceServiceRequest::decode(body).unwrap();
                        for span in decoded
                            .resource_spans
                            .iter()
                            .flat_map(|r| &r.scope_spans)
                            .flat_map(|s| &s.spans)
                        {
                            let hex: String =
                                span.trace_id.iter().map(|b| format!("{b:02x}")).collect();
                            seen.lock().unwrap().push(hex);
                        }
                        Bytes::new()
                    } else {
                        let ids = seen.lock().unwrap().join("\",\"");
                        Bytes::from(format!("{{\"data\":[\"{ids}\"]}}"))
                    };
                    let mut response = Response::new(Full::new(answer));
                    *response.status_mut() = StatusCode::OK;
                    Ok::<_, Infallible>(response)
                }
            });
            drop(task_env.spawn("mini-crucible-connection", async move {
                let _closed = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            }));
        }
    });
    (addr, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_probe_measures_ack_to_listed() {
    let env = env();
    let (addr, server) = mini_crucible(&env).await;
    let config = FreshConfig {
        ingest: addr,
        ingest_authority: addr.to_string(),
        api: addr,
        api_authority: addr.to_string(),
        headers: Vec::new(),
        project: "p".to_owned(),
        probes: 5,
        interval: Duration::from_millis(10),
        poll: Duration::from_millis(2),
        timeout: Duration::from_secs(5),
        seed: 1,
    };
    let report = fresh(&env, &config).await.unwrap();
    assert_eq!((report.probes, report.listed, report.missing), (5, 5, 0));
    assert_eq!((report.ingest_errors, report.list_errors), (0, 0));
    // The trace is listed by the first list after the acknowledgement, and that list's round trip is timed.
    assert_eq!(report.lists_per_probe.max, 1);
    assert_eq!(report.ack_to_list_us.count, 5);
    assert_eq!(report.list_us.count, 5);
    assert!(report.list_us.max <= report.ack_to_list_us.max);
    server.abort();
}
