//! Directed tests of `crucible-lifecycle` on one node with the real WAL and the real shipper (contract 13.3):
//! the tombstone of a deletion request is a durable frame of the node's WAL, the shipper's checkpoint passes it
//! on a node that receives nothing else, and from there the executor deletes, masks and verifies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use crucible_auth::test_support::scope;
use crucible_control::{
    ControlStore, DeletionRequest, DeletionSelector, ExportFormat, JsonDoc, MediaObject, MediaRef,
    MediaState, NewExport, Project, Repos as _,
};
use crucible_env::{StoreFlavour, Stores};
use crucible_jobs::Registry;
use crucible_lifecycle::{
    LeaderSource, LeaderStores, LifecycleConfig, NewDeletion, NodeLog as _, NodeState, Service,
    Subjects, WalLog, leader_jobs, node_job,
};
use crucible_plan::Value;
use crucible_records::{
    DeletionState, ExportJobId, Lsn, MediaObjectId, ProjectId, TableId, Timestamp, TraceId,
};
use crucible_schema::{Granularity, table};
use crucible_shipper::{Hooks, Shipper, ShipperConfig};
use crucible_sim::ch_model::ChModel;
use crucible_sim::pg_model::{PgFaults, PgModel};
use crucible_sim::ship_sim::{SpanSpec, request_payload};
use crucible_sim::{Host, SIM_EPOCH_MICROS, run};
use crucible_wal::{Wal, WalConfig};
use crucible_wal_codec::{Compression, PreparedFrame, Record, RecordType, TombstoneSelector};
use futures::future::BoxFuture;

const PROJECT: [u8; 16] = [1; 16];

/// The project salts of a node whose shipper has none: no row carries a subject hash.
#[derive(Debug)]
struct NoSalt;

impl Subjects for NoSalt {
    fn hashes<'a>(
        &'a self,
        _: ProjectId,
        _: &'a [String],
        _: &'a str,
    ) -> BoxFuture<'a, Result<Option<Vec<[u8; 16]>>, String>> {
        Box::pin(async { Ok(None) })
    }
}

struct Node {
    host: Host,
    wal: Wal,
    ch: ChModel,
    pg: PgModel,
    service: Service,
    log: WalLog,
}

impl Node {
    async fn sleep(&self, d: Duration) {
        self.host.env.clock.sleep(d).await;
    }

    /// Appends one request with the spans `spans` of the trace `tag` and returns its LSN once durable.
    async fn ingest(&self, tag: u8, spans: std::ops::Range<u8>) -> Lsn {
        let specs: Vec<SpanSpec> = spans
            .map(|i| {
                (
                    [tag; 16],
                    [tag, i, 1, 1, 1, 1, 1, 1],
                    SIM_EPOCH_MICROS + i64::from(i),
                    i == 0,
                )
            })
            .collect();
        let payload = request_payload(PROJECT, &specs, 0).unwrap();
        let frame = PreparedFrame::new(RecordType::IngestRequest, payload, Compression::Lz4);
        self.wal.append(frame.unwrap()).await.unwrap()
    }

    /// The spans of the trace `tag` that the store still holds in `span_core`, masked or not.
    fn held(&self, tag: u8) -> usize {
        let at = table(TableId::SpanCore)
            .columns
            .iter()
            .position(|column| column.name == "trace_id")
            .unwrap();
        self.ch
            .held_rows(TableId::SpanCore)
            .iter()
            .filter(|(_, _, cells)| cells[at] == Value::Bytes(vec![tag; 16]))
            .count()
    }

    async fn request(&self, id: crucible_records::DeletionRequestId) -> DeletionRequest {
        self.pg
            .lifecycle()
            .request(ProjectId::from_bytes(PROJECT), id)
            .await
            .unwrap()
            .unwrap()
    }
}

/// One node: the real WAL on a simulated disk, the real shipper over the store model, and the lifecycle jobs
/// through the real registry over the control-store model.
fn directed<F, Fut>(body: F)
where
    F: FnOnce(Node) -> Fut,
    Fut: Future<Output = ()>,
{
    run(11, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let ch = ChModel::new(
            sim.rng("store").as_ref(),
            StoreFlavour::SingleNode,
            Granularity::Day,
            Duration::from_secs(60),
        );
        let pg = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        pg.projects()
            .insert(&Project {
                id: ProjectId::from_bytes(PROJECT),
                name: "One".to_owned(),
                slug: "one".to_owned(),
                settings: JsonDoc::empty(),
                retention_days: None,
                payload_retention_days: None,
                mcp_content_retention_days: None,
                config_version: 1,
                version: 1,
                deleted_at: None,
            })
            .await
            .unwrap();
        let (wal, report) = Box::pin(Wal::open(&host.env, WalConfig::default()))
            .await
            .unwrap();
        let stores = Stores::new(Arc::new(ch.clone()));
        let shipper_config = ShipperConfig {
            linger: Duration::from_millis(10),
            tail_poll: Duration::from_millis(1),
            ..ShipperConfig::default()
        };
        let shipper = Shipper::new(
            &host.env,
            &stores,
            wal.clone(),
            &report,
            shipper_config,
            Hooks::default(),
        )
        .unwrap();
        drop(host.env.spawn("shipper", async move {
            Box::pin(shipper.run()).await.unwrap();
        }));

        let log = WalLog(wal.clone());
        let state = Arc::new(NodeState::new(Arc::new(log.clone()), "test", ""));
        let control: Arc<dyn ControlStore> = Arc::new(pg.clone());
        let service = Service::new(&host.env, Arc::clone(&control), Arc::clone(&state));
        let mut config = LifecycleConfig::default();
        config.mask_cycle = Duration::from_secs(1_800);
        config.media = true;
        let stores = LeaderStores {
            telemetry: Arc::new(ch.clone()),
            subjects: Arc::new(NoSalt),
        };
        let source: LeaderSource = Arc::new(move || Some(stores.clone()));
        let mut registry = Registry::new();
        registry.register(node_job(state, &config));
        for job in leader_jobs(&source, config) {
            registry.register(job);
        }
        let _running = registry.start(&host.env, Some(control), "node#1");
        let node = Node {
            host,
            wal,
            ch,
            pg,
            service,
            log,
        };
        body(node).await;
    });
}

#[test]
fn a_tombstone_is_a_durable_frame_that_the_checkpoint_passes_on_an_idle_node() {
    directed(|node| async move {
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(1)).await;
        let new = NewDeletion {
            selector: DeletionSelector::Traces(vec![TraceId::from_bytes([1; 16])]),
            requested_by: "user:test".to_owned(),
            idempotency: None,
        };
        let scope = scope(ProjectId::from_bytes(PROJECT)).unwrap();
        let accepted = node.service.request_deletion(scope, new).await.unwrap();
        let request = accepted.request;

        // The accepting node has recorded its tombstone before the call returns, and the frame reads back as
        // the request.
        let acks = node.pg.lifecycle().acks(request.id).await.unwrap();
        assert_eq!(acks.len(), 1);
        let tombstone_lsn = acks[0].tombstone_lsn;
        let mut replay = node.wal.replay(tombstone_lsn);
        let frame = replay.next().await.unwrap().unwrap();
        assert_eq!(frame.lsn(), tombstone_lsn);
        assert_eq!(frame.record_type(), RecordType::Tombstone);
        let payload = frame.payload().unwrap();
        let Record::Tombstone(read) = Record::decode(RecordType::Tombstone, &payload).unwrap()
        else {
            panic!("a tombstone frame");
        };
        assert_eq!(read.deletion_request_id, request.id);
        assert_eq!(read.project_id, request.project_id);
        assert_eq!(read.requested_at, request.requested_at);
        assert_eq!(read.deletion_seq, request.deletion_seq);
        assert_eq!(
            read.selector,
            TombstoneSelector::Traces(vec![TraceId::from_bytes([1; 16])])
        );

        // Nothing else arrives. The shipper seals the tombstone, so the watermark passes it, and the node
        // timer reports that within two of its periods.
        node.sleep(Duration::from_secs(11)).await;
        assert!(
            node.log.watermark() >= tombstone_lsn,
            "the watermark {} is below the tombstone {tombstone_lsn}",
            node.log.watermark()
        );
        let acks = node.pg.lifecycle().acks(request.id).await.unwrap();
        assert!(acks[0].passed_at.is_some(), "{acks:?}");
    });
}

#[test]
fn a_deletion_by_trace_completes_over_the_real_wal_and_shipper() {
    directed(|node| async move {
        node.ingest(1, 0..3).await;
        node.ingest(2, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        assert_eq!((node.held(1), node.held(2)), (3, 2));

        // An export made before the request may hold the spans it deletes.
        let export = |n: u8, created_at| NewExport {
            id: ExportJobId::from_bytes([n; 16]),
            project_id: ProjectId::from_bytes(PROJECT),
            format: ExportFormat::CrucibleJsonl,
            selector: "{}".to_owned(),
            options: "{}".to_owned(),
            dedup_key: format!("export-{n}"),
            created_by: "user:test".to_owned(),
            created_at,
            as_of: created_at,
        };
        let exports = node.pg.export_jobs();
        let before = node.host.env.clock.wall_now();
        exports.insert(&export(1, before)).await.unwrap();
        // Media used in both traces before the request.
        let media = node.pg.media();
        let object = MediaObject {
            id: MediaObjectId::from_bytes([3; 16]),
            project_id: ProjectId::from_bytes(PROJECT),
            sha256: [3; 32],
            content_type: "image/png".to_owned(),
            len: 3,
            state: MediaState::Uploaded,
            created_at: before,
            uploaded_at: Some(before),
        };
        media.insert_object(&object).await.unwrap();
        let used = |trace: u8, at: Timestamp| MediaRef {
            project_id: ProjectId::from_bytes(PROJECT),
            media_id: object.id,
            trace_id: TraceId::from_bytes([trace; 16]),
            span_id: None,
            field: format!("input-{}", at.micros()),
            target_time: at,
        };
        media.add_ref(&used(1, before)).await.unwrap();
        media.add_ref(&used(2, before)).await.unwrap();
        node.sleep(Duration::from_secs(1)).await;

        let new = NewDeletion {
            selector: DeletionSelector::Traces(vec![TraceId::from_bytes([1; 16])]),
            requested_by: "user:test".to_owned(),
            idempotency: Some(("k".to_owned(), "h".to_owned())),
        };
        let scope = scope(ProjectId::from_bytes(PROJECT)).unwrap();
        let accepted = node
            .service
            .request_deletion(scope, new.clone())
            .await
            .unwrap();
        let id = accepted.request.id;
        // The same key is the same request, and no second tombstone.
        let again = node.service.request_deletion(scope, new).await.unwrap();
        assert!(again.replayed);
        assert_eq!(again.request.id, id);
        assert_eq!(node.pg.lifecycle().acks(id).await.unwrap().len(), 1);

        // Spans of the trace that arrive after the request are new data, and so is an export made after it.
        node.sleep(Duration::from_secs(1)).await;
        node.ingest(1, 3..4).await;
        let after = node.host.env.clock.wall_now();
        exports.insert(&export(2, after)).await.unwrap();
        media.add_ref(&used(1, after)).await.unwrap();

        // One period of the executor after the checkpoint: the deletes have run and hide the rows.
        node.sleep(Duration::from_secs(90)).await;
        let applied = node.request(id).await;
        assert_eq!(applied.state, DeletionState::Applied, "{applied:?}");
        // After the mask cycle the rows are gone from disk and the request is complete.
        node.sleep(Duration::from_secs(2 * 1_800 + 120)).await;
        let done = node.request(id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        assert_eq!((node.held(1), node.held(2)), (1, 2));
        let open = node.pg.lifecycle().open_actions().await.unwrap();
        assert!(open.is_empty(), "{open:?}");
        // The export that predates the request is expired; the later one is not.
        let ids = [
            ExportJobId::from_bytes([1; 16]),
            ExportJobId::from_bytes([2; 16]),
        ];
        let expired: Vec<bool> = exports
            .get_many(&ids)
            .await
            .unwrap()
            .iter()
            .map(|job| job.expires_at.is_some())
            .collect();
        assert_eq!(expired, [true, false]);
        assert!(
            done.places.contains("\"export_files\":\"done\""),
            "{}",
            done.places
        );
        // The media uses of the trace recorded before the request are gone; the other trace's and the later
        // one stay, and the object with them.
        let left = |trace: u8| {
            let media = node.pg.media();
            async move {
                media
                    .uses(ProjectId::from_bytes(PROJECT), TraceId::from_bytes([trace; 16]), None)
                    .await
                    .unwrap()
                    .iter()
                    .map(|usage| usage.reference.target_time)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(left(1).await, [after]);
        assert_eq!(left(2).await, [before]);
        assert!(done.places.contains("\"media\":\"done\""), "{}", done.places);
    });
}
