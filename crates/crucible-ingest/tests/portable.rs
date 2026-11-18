//! Handler tests over the real WAL on the production `PortableDisk` in a temporary directory: requests in every
//! encoding are acknowledged and their frames are durable across a reopen.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use crucible_env::Root;
use crucible_env_prod::{Prod, ProdConfig, WalBackend, build};
use crucible_ingest::IngestConfig;
use crucible_otlp::transcode_json;
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::ContentOrigin;
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
