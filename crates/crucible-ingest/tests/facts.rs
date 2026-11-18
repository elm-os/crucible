//! The fact write path, the per-project byte rate and the drain-scaled `Retry-After` (contracts 3.8, 4.4 and
//! 4.8), on the simulated disk.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::pin::pin;
use std::time::Duration;

use axum::http::StatusCode;
use bytes::Bytes;
use common::*;
use crucible_admission::{UNAVAILABLE_MAX_S, UNAVAILABLE_MIN_S};
use crucible_auth::test_support::scope;
use crucible_env::metrics::registry;
use crucible_ingest::{BucketConfig, FactAuthor, FactWriteError, Ingest, IngestConfig, NewFact};
use crucible_records::{
    AuthorKind, CorrectionBody, FactBody, FactTargetKind, FlagBody, Lsn, ScoreBody, ScoreDataType,
    ScoreSource, SpanId, TagBody, TagOp, Timestamp, TraceId, Uuid, fact_order,
};
use crucible_sim::{DiskFaults, Host, SIM_EPOCH_MICROS, Sim, run};
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::{self as codec, Record, RecordType};
use prost::Message as _;

const NOW_NANOS: u64 = SIM_EPOCH_MICROS as u64 * 1_000;

struct Node {
    host: Host,
    wal: Wal,
    ingest: Ingest,
}

async fn start(sim: &Sim, config: &WalConfig, ingest: IngestConfig) -> Node {
    let disk = sim.disk("disk");
    let host = sim.host("node/boot-1", &disk, 0);
    let (wal, _report) = Wal::open(&host.env, config.clone()).await.unwrap();
    let ingest = Ingest::new(&host.env, wal.clone(), config, keys(), ingest);
    Node { host, wal, ingest }
}

fn author() -> FactAuthor {
    FactAuthor {
        kind: AuthorKind::User,
        id: Uuid::from_bytes([9; 16]),
    }
}

fn tag(key: &str, op: TagOp) -> NewFact {
    NewFact {
        target_kind: FactTargetKind::Trace,
        trace_id: TraceId::from_bytes([2; 16]),
        span_id: SpanId::ZERO,
        session_id: "".into(),
        target_time: Timestamp::from_micros(SIM_EPOCH_MICROS - 60_000_000),
        key: key.into(),
        retracted: false,
        body: FactBody::Tag(TagBody { op }),
    }
}

/// The fact frames of the log, decoded: (LSN, project, author kind, number of facts, first key).
async fn fact_frames(wal: &Wal) -> Vec<(u64, [u8; 16], AuthorKind, usize, String)> {
    let mut replay = wal.replay(Lsn::new(1));
    let mut out = Vec::new();
    while let Some(frame) = replay.next().await.unwrap() {
        assert_eq!(frame.record_type(), RecordType::FactBatch);
        let payload = frame.payload().unwrap();
        let Record::FactBatch(batch) = Record::decode(RecordType::FactBatch, &payload).unwrap()
        else {
            panic!("not a fact batch")
        };
        out.push((
            frame.lsn().get(),
            batch.project_id.to_bytes(),
            batch.author_kind,
            batch.facts.len(),
            batch.facts[0].key.to_owned(),
        ));
    }
    out
}

#[test]
fn facts_become_durable_fact_batch_frames_before_the_ack() {
    run(21, |sim| async move {
        let node = start(&sim, &wal_config(), IngestConfig::default()).await;
        let scope = scope(PROJECT).unwrap();
        let facts = vec![
            tag("a", TagOp::Add),
            NewFact {
                target_kind: FactTargetKind::Span,
                span_id: SpanId::from_bytes([3; 8]),
                key: "quality#1".into(),
                body: FactBody::Score(ScoreBody {
                    data_type: ScoreDataType::Numeric,
                    value_num: 0.5,
                    value_str: "".into(),
                    comment: "fine".into(),
                    source: ScoreSource::Feedback,
                    metadata: Bytes::from_static(b"{\"k\":1}"),
                    config_id: None,
                }),
                ..tag("", TagOp::Add)
            },
            NewFact {
                target_kind: FactTargetKind::Session,
                trace_id: TraceId::ZERO,
                session_id: "s1".into(),
                key: "bookmarked".into(),
                retracted: true,
                body: FactBody::Flag(FlagBody { value: true }),
                ..tag("", TagOp::Add)
            },
            NewFact {
                key: "output".into(),
                body: FactBody::Correction(CorrectionBody {
                    corrected: Bytes::from_static(b"\"better\""),
                    note: "note".into(),
                }),
                ..tag("", TagOp::Add)
            },
        ];
        let accepted = node
            .ingest
            .append_facts(&scope, author(), &facts)
            .await
            .unwrap();
        assert_eq!(accepted.len(), 4);
        assert!(accepted.iter().all(|a| a.lsn == Lsn::new(1)));
        assert!(
            accepted
                .iter()
                .all(|a| a.version == SIM_EPOCH_MICROS as u64)
        );
        // Later facts of one write win ties in the version: their ids are greater in the fact order.
        assert!(accepted.windows(2).all(|w| {
            fact_order((w[0].version, w[0].fact_id), (w[1].version, w[1].fact_id)).is_lt()
        }));
        assert_eq!(node.wal.durable_lsn(), Lsn::new(1));

        let mut replay = node.wal.replay(Lsn::new(1));
        let frame = replay.next().await.unwrap().unwrap();
        let payload = frame.payload().unwrap();
        let Record::FactBatch(batch) = Record::decode(RecordType::FactBatch, &payload).unwrap()
        else {
            panic!("not a fact batch")
        };
        assert_eq!(batch.project_id, PROJECT);
        assert_eq!(
            (batch.author_kind, batch.author_id),
            (author().kind, author().id)
        );
        for ((wire, sent), assigned) in batch.facts.iter().zip(&facts).zip(&accepted) {
            assert_eq!(wire.fact_id, assigned.fact_id);
            assert_eq!(wire.version, assigned.version);
            assert_eq!(wire.created_at, assigned.created_at);
            assert_eq!(wire.target_kind, sent.target_kind);
            assert_eq!((wire.trace_id, wire.span_id), (sent.trace_id, sent.span_id));
            assert_eq!((wire.session_id, wire.key), (&*sent.session_id, &*sent.key));
            assert_eq!(
                (wire.target_time, wire.retracted),
                (sent.target_time, sent.retracted)
            );
        }
        assert!(matches!(
            batch.facts[0].body,
            codec::FactBody::Tag { op: TagOp::Add }
        ));
        assert!(matches!(
            batch.facts[2].body,
            codec::FactBody::Flag { value: true }
        ));
        let codec::FactBody::Score(score) = batch.facts[1].body else {
            panic!("not a score")
        };
        assert_eq!((score.comment, score.metadata), ("fine", "{\"k\":1}"));
        let codec::FactBody::Correction { corrected, note } = batch.facts[3].body else {
            panic!("not a correction")
        };
        assert_eq!((corrected, note), ("\"better\"", "note"));
        let recorder = &node.host.recorder;
        assert_eq!(recorder.counter_value(&registry::INGEST_FACTS, &[]), 4);
        assert_eq!(
            recorder.counter_value(&registry::INGEST_FACT_WRITES, &["accepted"]),
            1
        );
        assert_eq!(
            recorder.gauge_value(&registry::INGEST_INFLIGHT_BYTES, &[]),
            Some(0)
        );
    });
}

#[test]
fn a_fact_write_is_acknowledged_only_after_the_sync() {
    run(22, |sim| async move {
        let disk = sim.disk("disk");
        let host = sim.host("node/boot-1", &disk, 0);
        let (wal, _report) = Wal::open(&host.env, wal_config()).await.unwrap();
        let ingest = Ingest::new(
            &host.env,
            wal.clone(),
            &wal_config(),
            keys(),
            IngestConfig::default(),
        );
        disk.set_faults(DiskFaults {
            sync_latency_micros: (200_000, 200_000),
            ..DiskFaults::default()
        });
        let scope = scope(PROJECT).unwrap();
        let facts = [tag("a", TagOp::Add)];
        let mut write = pin!(ingest.append_facts(&scope, author(), &facts));
        let early = tokio::select! {
            biased;
            done = &mut write => Some(done),
            () = host.env.clock.sleep(Duration::from_millis(100)) => None,
        };
        assert!(early.is_none(), "acknowledged before the sync returned");
        assert_eq!(wal.durable_lsn(), Lsn::ZERO);
        let accepted = write.await.unwrap();
        assert_eq!(wal.durable_lsn(), accepted[0].lsn);
    });
}

#[test]
fn a_large_write_is_split_into_frames_of_at_most_1000_facts() {
    run(23, |sim| async move {
        let node = start(&sim, &wal_config(), IngestConfig::default()).await;
        let scope = scope(PROJECT).unwrap();
        let facts: Vec<NewFact> = (0..2_300)
            .map(|i| tag(&format!("t{i}"), TagOp::Add))
            .collect();
        let accepted = node
            .ingest
            .append_facts(&scope, author(), &facts)
            .await
            .unwrap();
        let lsns: Vec<u64> = accepted.iter().map(|a| a.lsn.get()).collect();
        assert_eq!(
            (lsns[0], lsns[999], lsns[1_000], lsns[1_999], lsns[2_000]),
            (1, 1, 2, 2, 3)
        );
        let frames = fact_frames(&node.wal).await;
        let shape: Vec<(u64, usize, &str)> =
            frames.iter().map(|f| (f.0, f.3, f.4.as_str())).collect();
        assert_eq!(
            shape,
            [(1, 1_000, "t0"), (2, 1_000, "t1000"), (3, 300, "t2000")]
        );
        let mut ids: Vec<_> = accepted.iter().map(|a| a.fact_id).collect();
        ids.dedup();
        assert_eq!(ids.len(), 2_300);
    });
}

#[test]
fn facts_that_are_not_well_formed_are_refused_before_the_wal() {
    run(24, |sim| async move {
        let node = start(&sim, &wal_config(), IngestConfig::default()).await;
        let scope = scope(PROJECT).unwrap();
        let good = || tag("ok", TagOp::Add);
        let invalid = [
            NewFact {
                key: "".into(),
                ..good()
            },
            NewFact {
                span_id: SpanId::from_bytes([1; 8]),
                ..good()
            },
            NewFact {
                trace_id: TraceId::ZERO,
                ..good()
            },
            NewFact {
                target_kind: FactTargetKind::Session,
                ..good()
            },
            NewFact {
                session_id: "s".into(),
                ..good()
            },
            // Year 2100 and later route to no partition.
            NewFact {
                target_time: Timestamp::from_micros(4_102_444_800_000_000),
                ..good()
            },
            NewFact {
                target_time: Timestamp::from_micros(-1),
                ..good()
            },
            NewFact {
                body: FactBody::Correction(CorrectionBody {
                    corrected: Bytes::from_static(&[0xff, 0xfe]),
                    note: "".into(),
                }),
                ..good()
            },
        ];
        for bad in invalid {
            let write = [good(), bad.clone()];
            let refused = node.ingest.append_facts(&scope, author(), &write).await;
            assert!(
                matches!(refused, Err(FactWriteError::Invalid { index: 1, .. })),
                "{bad:?}: {refused:?}"
            );
        }
        let large = [
            NewFact {
                key: "k".repeat(257).into(),
                ..good()
            },
            NewFact {
                body: FactBody::Correction(CorrectionBody {
                    corrected: Bytes::from_static(b"1"),
                    note: "n".repeat(4_097).into(),
                }),
                ..good()
            },
        ];
        for big in large {
            let refused = node.ingest.append_facts(&scope, author(), &[big]).await;
            assert!(
                matches!(refused, Err(FactWriteError::TooLarge { index: 0, .. })),
                "{refused:?}"
            );
        }
        assert_eq!(node.wal.durable_lsn(), Lsn::ZERO);
        let recorder = &node.host.recorder;
        assert_eq!(
            recorder.counter_value(&registry::INGEST_FACT_WRITES, &["validation_failed"]),
            8
        );
        assert_eq!(
            recorder.counter_value(&registry::INGEST_FACT_WRITES, &["payload_too_large"]),
            2
        );
    });
}

#[test]
fn fact_writes_share_admission_with_ingest() {
    let full = WalConfig {
        max_bytes: wal_config().segment_bytes,
        ..wal_config()
    };
    run(25, |sim| async move {
        let node = start(&sim, &full, IngestConfig::default()).await;
        let scope = scope(PROJECT).unwrap();
        let refused = node
            .ingest
            .append_facts(&scope, author(), &[tag("a", TagOp::Add)])
            .await;
        let Err(FactWriteError::Unavailable {
            retry_after_s,
            detail,
        }) = refused
        else {
            panic!("{refused:?}")
        };
        assert_eq!(retry_after_s, UNAVAILABLE_MAX_S);
        assert!(detail.contains("wal.max_bytes"), "{detail}");
    });
    let tiny = IngestConfig {
        inflight_bytes: 16,
        ..IngestConfig::default()
    };
    run(26, |sim| async move {
        let node = start(&sim, &wal_config(), tiny).await;
        let scope = scope(PROJECT).unwrap();
        let refused = node
            .ingest
            .append_facts(&scope, author(), &[tag("a", TagOp::Add)])
            .await;
        let Err(FactWriteError::Throttled { detail, .. }) = refused else {
            panic!("{refused:?}")
        };
        assert!(detail.contains("ingest.inflight_bytes"), "{detail}");
    });
}

#[test]
fn the_project_byte_rate_is_off_by_default_and_names_its_setting_when_it_refuses() {
    let body = export(vec![good_span(1, NOW_NANOS)]).encode_to_vec();
    let len = body.len() as u64;
    // The default: any number of requests in one instant pass.
    let unlimited = body.clone();
    run(27, |sim| async move {
        let node = start(&sim, &wal_config(), IngestConfig::default()).await;
        let router = node.ingest.router();
        for _ in 0..50 {
            let reply = send(&router, Req::new(unlimited.clone(), PROTOBUF)).await;
            assert_eq!(reply.status, StatusCode::OK);
        }
    });
    // A rate of one body per second with a burst of two bodies.
    let limited = IngestConfig {
        project_rate: Some(BucketConfig {
            rate_per_s: len,
            burst: 2 * len,
        }),
        ..IngestConfig::default()
    };
    run(28, |sim| async move {
        let node = start(&sim, &wal_config(), limited).await;
        let router = node.ingest.router();
        let post = || send(&router, Req::new(body.clone(), PROTOBUF));
        assert_eq!(post().await.status, StatusCode::OK);
        assert_eq!(post().await.status, StatusCode::OK);
        let reply = post().await;
        assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
        assert!((1..=5).contains(&reply.retry_after().unwrap()));
        let message = reply.rpc_status().message;
        assert!(message.contains("ingest.project_bytes_per_s"), "{message}");
        // A body without a declared length is charged when its size is known.
        let streamed =
            send_request(&router, Req::new(body.clone(), PROTOBUF).build_streamed()).await;
        assert_eq!(streamed.status, StatusCode::TOO_MANY_REQUESTS);
        // One second refills one body.
        node.host.env.clock.sleep(Duration::from_secs(1)).await;
        assert_eq!(post().await.status, StatusCode::OK);
        assert_eq!(post().await.status, StatusCode::TOO_MANY_REQUESTS);
        // Fact writes draw on the same bucket.
        let scope = scope(PROJECT).unwrap();
        let facts: Vec<NewFact> = (0..200)
            .map(|i| tag(&format!("t{i}"), TagOp::Add))
            .collect();
        let refused = node.ingest.append_facts(&scope, author(), &facts).await;
        assert!(
            matches!(refused, Err(FactWriteError::Throttled { .. })),
            "{refused:?}"
        );
    });
}

#[test]
fn a_503_for_wal_space_waits_as_long_as_the_drain_takes() {
    let full = WalConfig {
        max_bytes: wal_config().segment_bytes,
        ..wal_config()
    };
    run(29, |sim| async move {
        let node = start(&sim, &full, IngestConfig::default()).await;
        let router = node.ingest.router();
        let req = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
        let wait = |reply: Reply| {
            assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
            let message = reply.rpc_status().message;
            assert!(message.contains("wal.max_bytes"), "{message}");
            reply.retry_after().unwrap()
        };
        // Nothing drains: the longest wait.
        assert_eq!(wait(send(&router, req.clone()).await), UNAVAILABLE_MAX_S);
        // The backlog is the bytes above the high-water mark: one tenth of the one segment.
        let backlog =
            node.wal.admission().segment_bytes - (full.max_bytes - full.max_bytes.div_ceil(10));
        node.wal.set_drain_rate(backlog / 12);
        assert_eq!(wait(send(&router, req.clone()).await), 13);
        // A fast drain: the shortest wait.
        node.wal.set_drain_rate(backlog);
        assert_eq!(wait(send(&router, req.clone()).await), UNAVAILABLE_MIN_S);
        node.wal.set_drain_rate(0);
        assert_eq!(wait(send(&router, req).await), UNAVAILABLE_MAX_S);
    });
    let low_disk = WalConfig {
        min_free_bytes: u64::MAX,
        ..wal_config()
    };
    run(30, |sim| async move {
        let node = start(&sim, &low_disk, IngestConfig::default()).await;
        let req = Req::protobuf(&export(vec![good_span(1, NOW_NANOS)]));
        let reply = send(&node.ingest.router(), req).await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
        let message = reply.rpc_status().message;
        assert!(message.contains("wal.min_free_bytes"), "{message}");
    });
}
