//! Directed tests of `crucible-lifecycle` on one node with the real WAL and the real shipper (contract 13.3):
//! the tombstone of a deletion request is a durable frame of the node's WAL, the shipper's checkpoint passes it
//! on a node that receives nothing else, and from there the executor deletes, masks and verifies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crucible_auth::test_support::scope;
use crucible_chfmt::EncodedBlock;
use crucible_control::{
    ActionKind, ControlStore, DeletionRequest, DeletionSelector, ExportCommit, ExportFormat,
    JsonDoc, MediaObject, MediaRef, MediaState, NewExport, Project, Queue, Repos as _,
};
use crucible_env::{
    InsertError, InsertSettings, MutationId, MutationStatus, QueryError, RowStream, StoreError,
    StoreFacts, StoreFlavour, StoreHealth, Stores, TelemetryStore,
};
use crucible_jobs::Registry;
use crucible_lifecycle::{
    EARLIER_DELETION_BY, ExportObjects, LeaderSource, LeaderStores, LifecycleConfig, NewDeletion,
    NodeLog as _, NodeState, Service, Subjects, WalLog, leader_jobs, node_job,
};
use crucible_plan::{BatchProbe, DeletePlan, QueryPlan, Value};
use crucible_records::{
    BatchToken, DeletionState, ExportJobId, Lsn, MediaObjectId, PartitionId, ProjectId, TableId,
    Timestamp, TraceId, Uuid, WalEpoch,
};
use crucible_schema::{Granularity, MigrationStep, route, table};
use crucible_sim::blocks::Rows;
use crucible_shipper::{Hooks, Shipper, ShipperConfig};
use crucible_sim::ch_model::ChModel;
use crucible_sim::object_model::{ObjectFaults, SimObjectStore};
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

/// What a test runs just before a drop or a delete reaches the store model, with its table and partition.
type Hook = Arc<dyn Fn(TableId, PartitionId) -> BoxFuture<'static, ()> + Send + Sync>;

/// The store model as the leader jobs reach it, with what a test steers: a hook that runs just before a drop or a
/// delete reaches the model, and a number of reads that fail, as a server that times out fails them.
#[derive(Clone)]
struct Hooked {
    ch: ChModel,
    hook: Arc<Mutex<Option<Hook>>>,
    /// The reads over every instant of a table, and the reads that find where a project's rows are, that are
    /// still to fail.
    failing: Arc<AtomicU32>,
    /// Whether a read over every instant of a table is refused as too broad, as a server refuses a read of more
    /// rows than its class allows.
    refuse_wide: Arc<AtomicBool>,
}

impl std::fmt::Debug for Hooked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooked")
    }
}

impl Hooked {
    fn set_hook(&self, hook: Hook) {
        *self.hook.lock().unwrap() = Some(hook);
    }

    async fn before(&self, table_id: TableId, partition: PartitionId) {
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(table_id, partition).await;
        }
    }

    /// Whether this read fails, while reads are still to fail.
    fn fails(&self) -> bool {
        self.failing
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
            .is_ok()
    }
}

#[async_trait]
impl TelemetryStore for Hooked {
    async fn insert_block(
        &self,
        block: EncodedBlock,
        settings: InsertSettings,
    ) -> Result<(), InsertError> {
        self.ch.insert_block(block, settings).await
    }

    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        let every_instant = plan.window().from() == Timestamp::EPOCH
            && plan.window().to() == Timestamp::MAX_EXCLUSIVE;
        if every_instant && self.refuse_wide.load(Ordering::SeqCst) {
            return Err(QueryError::TooBroad);
        }
        if every_instant && self.fails() {
            return Err(QueryError::Timeout);
        }
        self.ch.execute(plan).await
    }

    async fn probe(&self, probe: BatchProbe) -> Result<bool, StoreError> {
        self.ch.probe(probe).await
    }

    async fn delete(&self, plan: DeletePlan) -> Result<MutationId, StoreError> {
        self.before(plan.table(), plan.partition()).await;
        self.ch.delete(plan).await
    }

    async fn drop_partition(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<(), StoreError> {
        self.before(table_id, partition).await;
        self.ch.drop_partition(table_id, partition).await
    }

    async fn apply_deleted_mask(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<MutationId, StoreError> {
        self.ch.apply_deleted_mask(table_id, partition).await
    }

    async fn mutation_status(&self, id: &MutationId) -> Result<MutationStatus, StoreError> {
        self.ch.mutation_status(id).await
    }

    async fn partition_projects(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        self.ch.partition_projects(table_id, partition).await
    }

    async fn masked_parts(
        &self,
        table_id: TableId,
        partition: PartitionId,
    ) -> Result<u64, StoreError> {
        self.ch.masked_parts(table_id, partition).await
    }

    async fn partitions(&self, table_id: TableId) -> Result<Vec<PartitionId>, StoreError> {
        if self.fails() {
            return Err(StoreError::Unavailable("the read timed out".to_owned()));
        }
        self.ch.partitions(table_id).await
    }

    async fn holds_project(
        &self,
        table_id: TableId,
        partition: Option<PartitionId>,
        project: ProjectId,
    ) -> Result<bool, StoreError> {
        if self.fails() {
            return Err(StoreError::Unavailable("the read timed out".to_owned()));
        }
        self.ch.holds_project(table_id, partition, project).await
    }

    async fn apply_migration(&self, step: &MigrationStep) -> Result<(), StoreError> {
        self.ch.apply_migration(step).await
    }

    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        self.ch.detect().await
    }

    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        self.ch.dedup_selftest().await
    }

    async fn kill(&self, query_id: Uuid) -> Result<(), StoreError> {
        self.ch.kill(query_id).await
    }

    async fn health(&self) -> StoreHealth {
        self.ch.health().await
    }
}

/// One `span_core` row of `project` that starts at `start`, as a shipper writes it: a span of the trace `tag`
/// from a WAL that only such rows come from, at log position `lsn`.
async fn land_span(ch: &ChModel, project: ProjectId, tag: u8, start: Timestamp, lsn: u64) {
    let epoch = WalEpoch::from_uuid(Uuid::from_v7_parts(1_700_000_000_000, 7));
    let trace = TraceId::from_bytes([tag; 16]);
    let rows = Rows::new(TableId::SpanCore, 1)
        .all("project_id", Value::Uuid(project.uuid()))
        .all("start_time", Value::Time(start))
        .all("trace_id", Value::Bytes(trace.to_bytes().to_vec()))
        .all("trace_hash", Value::UInt(u64::from(trace.trace_hash())))
        .all("span_id", Value::Bytes(vec![tag, 0, 0, 0, 0, 0, 0, 1]))
        .all("received_at", Value::Time(start))
        .all("wal_epoch", Value::Uuid(epoch.uuid()))
        .all("batch_seq", Value::UInt(lsn))
        .all("lsn", Value::UInt(lsn))
        .sorted();
    let partition = route(TableId::SpanCore, Granularity::Day, start).unwrap();
    let token = BatchToken::new(
        epoch,
        Lsn::new(lsn),
        Lsn::new(lsn),
        TableId::SpanCore,
        partition,
    )
    .unwrap();
    let settings = InsertSettings::new(token, StoreFlavour::SingleNode);
    ch.insert_block(rows.encode().unwrap(), settings)
        .await
        .unwrap();
}

/// A live project made at `made_at` (microseconds), with the UUIDv7 id that `Env::new_id` mints at that moment,
/// keeping its data forever.
fn made(n: u8, name: &str, made_at: i64) -> Project {
    Project {
        id: ProjectId::from_uuid(Uuid::from_v7_parts(
            u64::try_from(made_at / 1_000).unwrap(),
            u128::from(n),
        )),
        ..project(n, name)
    }
}

struct Node {
    host: Host,
    wal: Wal,
    ch: ChModel,
    /// The store as the leader jobs reach it.
    hooked: Hooked,
    pg: PgModel,
    service: Service,
    log: WalLog,
    /// The object store, which holds export files under the prefix `media`.
    objects: Arc<SimObjectStore>,
}

impl Node {
    async fn sleep(&self, d: Duration) {
        self.host.env.clock.sleep(d).await;
    }

    /// Appends one request with the spans `spans` of the trace `tag` and returns its LSN once durable.
    async fn ingest(&self, tag: u8, spans: std::ops::Range<u8>) -> Lsn {
        self.ingest_at(tag, spans, SIM_EPOCH_MICROS).await
    }

    /// [`Node::ingest`] with spans that start at `start` (microseconds) and after.
    async fn ingest_at(&self, tag: u8, spans: std::ops::Range<u8>, start: i64) -> Lsn {
        let specs: Vec<SpanSpec> = spans
            .map(|i| {
                (
                    [tag; 16],
                    [tag, i, 1, 1, 1, 1, 1, 1],
                    start + i64::from(i),
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

    /// Marks the project deleted as `DELETE projects/{id}` does, with the project deletion request that this version
    /// makes in the same transaction when `request` is given, and with none, as an older version did, otherwise.
    async fn mark_deleted(&self, project: ProjectId, request: Option<DeletionRequest>) {
        let at = request.as_ref().map_or_else(
            || self.host.env.clock.wall_now(),
            |request| request.requested_at,
        );
        crucible_control::transaction(&self.pg, move |tx| {
            Box::pin(async move {
                let stored = tx.projects().get(project).await?.unwrap();
                let deleted = Project {
                    deleted_at: Some(at),
                    version: stored.version + 1,
                    ..stored.clone()
                };
                assert!(
                    tx.projects()
                        .update_if_version(&deleted, stored.version)
                        .await?
                );
                if let Some(request) = &request {
                    tx.lifecycle().insert_request(request).await?;
                }
                Ok(())
            })
        })
        .await
        .unwrap();
    }

    /// No table that a project deletion reaches holds a row of the project, masked or not.
    fn assert_purged(&self, project: ProjectId) {
        for table_id in crucible_lifecycle::project_reach() {
            let at = table(table_id)
                .columns
                .iter()
                .position(|column| column.name == "project_id")
                .unwrap();
            let left = self
                .ch
                .held_rows(table_id)
                .iter()
                .filter(|(_, _, cells)| cells[at] == Value::Uuid(project.uuid()))
                .count();
            assert_eq!(
                left,
                0,
                "{} still holds rows of the project",
                table_id.name()
            );
        }
    }
}

/// A project of the store, live and keeping its data forever.
fn project(n: u8, name: &str) -> Project {
    Project {
        id: ProjectId::from_bytes([n; 16]),
        name: name.to_owned(),
        slug: name.to_lowercase(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

/// The leader jobs of a second server over the same stores, which takes the `lifecycle` lease when the first does
/// not hold it.
fn second_server(node: &Node) -> crucible_jobs::Running {
    let mut config = LifecycleConfig::default();
    config.mask_cycle = Duration::from_secs(1_800);
    config.media = true;
    let stores = LeaderStores {
        telemetry: Arc::new(node.ch.clone()),
        subjects: Arc::new(NoSalt),
        exports: Some(ExportObjects::new(node.objects.clone(), "media")),
    };
    let source: LeaderSource = Arc::new(move || Some(stores.clone()));
    let mut registry = Registry::new();
    for job in leader_jobs(&source, config) {
        registry.register(job);
    }
    let control: Arc<dyn ControlStore> = Arc::new(node.pg.clone());
    registry.start(&node.host.env, Some(control), "node#2")
}

/// One node: the real WAL on a simulated disk, the real shipper over the store model, and the lifecycle jobs
/// through the real registry over the control-store model.
fn directed<F, Fut>(body: F)
where
    F: FnOnce(Node) -> Fut,
    Fut: Future<Output = ()>,
{
    directed_with(|_| {}, body);
}

/// [`directed`] with the lifecycle settings that `configure` changes.
fn directed_with<F, Fut>(configure: impl FnOnce(&mut LifecycleConfig), body: F)
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
        configure(&mut config);
        let objects = SimObjectStore::new(
            host.clock.clone(),
            sim.rng("objects"),
            ObjectFaults::default(),
        );
        let hooked = Hooked {
            ch: ch.clone(),
            hook: Arc::default(),
            failing: Arc::default(),
            refuse_wide: Arc::default(),
        };
        let stores = LeaderStores {
            telemetry: Arc::new(hooked.clone()),
            subjects: Arc::new(NoSalt),
            exports: Some(ExportObjects::new(objects.clone(), "media")),
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
            hooked,
            pg,
            service,
            log,
            objects,
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

/// Contract 13.4: deleting a project makes its project deletion request in the transaction that marks it deleted.
/// The node logs a project tombstone for it, and the executor removes every row of the project from every table
/// that holds them, the rollups the shipper wrote included and a frame taken after the deletion too, then its
/// objects and its control-plane rows, and the request completes.
#[test]
fn a_project_deletion_empties_every_table_and_completes_over_the_real_wal_and_shipper() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..3).await;
        node.ingest(2, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        assert_eq!((node.held(1), node.held(2)), (3, 2));
        // An object under the project's key prefix, as an export file or a media object leaves one.
        let key = format!("media/{project}/exports/x/1/crucible-export-x.jsonl");
        let now = node.host.env.clock.wall_now();
        node.objects
            .plant(&key, bytes::Bytes::from_static(b"{}\n"), now);
        node.pg
            .project_salts()
            .insert_if_absent(&crucible_control::SealedProjectSalt {
                project_id: project,
                key_id: "master-0".to_owned(),
                nonce: vec![1; 24],
                ciphertext: vec![1; 48],
            })
            .await
            .unwrap();

        // What `DELETE projects/{id}` runs: the project is marked deleted and its request made, together.
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([0x0d; 16]),
            project,
            node.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        node.mark_deleted(project, Some(request.clone())).await;
        // A node whose key snapshot has not caught up still takes a frame of the project.
        node.ingest(3, 0..1).await;

        // The node timer logs the project's tombstone.
        node.sleep(Duration::from_secs(11)).await;
        let acks = node.pg.lifecycle().acks(request.id).await.unwrap();
        assert_eq!(acks.len(), 1, "{acks:?}");
        let mut replay = node.wal.replay(acks[0].tombstone_lsn);
        let frame = replay.next().await.unwrap().unwrap();
        let payload = frame.payload().unwrap();
        let Record::Tombstone(read) = Record::decode(RecordType::Tombstone, &payload).unwrap()
        else {
            panic!("a tombstone frame");
        };
        assert_eq!(read.selector, TombstoneSelector::Project);

        // After the mask cycle every place is empty and the request is complete.
        node.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        let done = node.request(request.id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        node.assert_purged(project);
        // The shipper had written the trace and minute rollups too, and they went with the spans.
        for written in [
            TableId::SpanCore,
            TableId::TraceHeads,
            TableId::TraceStats,
            TableId::SpanMetrics,
        ] {
            assert!(
                !node.ch.parts(written).is_empty(),
                "the shipper wrote no {}",
                written.name()
            );
        }
        assert!(
            !node
                .objects
                .objects()
                .keys()
                .any(|key| key.starts_with(&format!("media/{project}/"))),
            "objects of the project are left"
        );
        assert!(
            node.pg
                .project_salts()
                .get(project)
                .await
                .unwrap()
                .is_none(),
            "the salt is left"
        );
        assert!(
            done.places.contains("\"control_plane\":\"done\""),
            "{}",
            done.places
        );
        let open = node.pg.lifecycle().open_actions().await.unwrap();
        assert!(open.is_empty(), "{open:?}");
    });
}

const DAY: i64 = 86_400_000_000;

/// The `span_core` partition of the day `back` days before the simulation's epoch.
fn day(back: i64) -> PartitionId {
    route(
        TableId::SpanCore,
        Granularity::Day,
        Timestamp::from_micros(SIM_EPOCH_MICROS - back * DAY),
    )
    .unwrap()
}

/// `ingest.max_event_age` in the tests of the drop rule: 5 days.
fn five_days(config: &mut LifecycleConfig) {
    config.max_event_age = Duration::from_secs(5 * 86_400);
}

impl Node {
    /// How each `span_core` partition of a request was purged: dropped or deleted from.
    async fn span_core_kinds(
        &self,
        request: crucible_records::DeletionRequestId,
    ) -> BTreeMap<PartitionId, ActionKind> {
        self.pg
            .lifecycle()
            .request_actions(request)
            .await
            .unwrap()
            .into_iter()
            .filter(|action| action.table == TableId::SpanCore)
            .map(|action| (action.partition, action.kind))
            .collect()
    }

    /// The `span_core` rows of a project that reads still see.
    fn visible(&self, project: ProjectId) -> usize {
        let at = table(TableId::SpanCore)
            .columns
            .iter()
            .position(|column| column.name == "project_id")
            .unwrap();
        self.ch
            .held_rows(TableId::SpanCore)
            .iter()
            .filter(|(_, masked, cells)| !masked && cells[at] == Value::Uuid(project.uuid()))
            .count()
    }

    /// Deletes the project as `DELETE projects/{id}` does and waits two mask cycles, by which the purge is complete.
    async fn delete_and_purge(&self, project: ProjectId, id: u8) -> DeletionRequest {
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([id; 16]),
            project,
            self.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        self.mark_deleted(project, Some(request.clone())).await;
        self.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        let done = self.request(request.id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        self.assert_purged(project);
        done
    }
}

/// Contract 13.4: a project deletion drops a partition only when the partition ends before the oldest instant at which
/// any project, live or made later, can still write, and every project with rows there is deleted. With
/// `ingest.max_event_age` at 5 days and the one live project made now, keeping everything, the deleted project's
/// partition of ten days ago is dropped, while those of two days ago and of today, which the live project can still
/// write to, get deletes of the deleted project's rows. (The rule before kept every partition from being dropped while
/// a project that keeps everything lived.)
#[test]
fn a_project_deletion_drops_a_partition_only_where_no_project_can_write_any_more() {
    directed_with(five_days, |node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.pg
            .projects()
            .insert(&made(2, "Two", SIM_EPOCH_MICROS))
            .await
            .unwrap();
        for (tag, back) in [(1, 10), (2, 2), (3, 0)] {
            node.ingest_at(tag, 0..2, SIM_EPOCH_MICROS - back * DAY)
                .await;
        }
        node.sleep(Duration::from_secs(2)).await;
        let done = node.delete_and_purge(project, 0x0e).await;
        assert_eq!(
            node.span_core_kinds(done.id).await,
            BTreeMap::from([
                (day(10), ActionKind::Drop),
                (day(2), ActionKind::Delete),
                (day(0), ActionKind::Delete),
            ])
        );
    });
}

/// Contract 13.4: a live project made forty days ago can still write a row that starts up to `max_event_age` before it
/// was made, whenever it ships: a span it took back then and ships now, or a fact on one of its old spans, which takes
/// that span's start. So the deleted project's partition of ten days ago, though it ends more than `max_event_age`
/// before now, is not dropped. Such a row of the old project lands there just as the purge reaches the store, and it
/// stays.
#[test]
fn a_project_made_long_ago_keeps_every_partition_it_can_still_write_to() {
    directed_with(five_days, |node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        let old = made(2, "Old", SIM_EPOCH_MICROS - 40 * DAY).id;
        node.pg
            .projects()
            .insert(&made(2, "Old", SIM_EPOCH_MICROS - 40 * DAY))
            .await
            .unwrap();
        node.ingest_at(1, 0..2, SIM_EPOCH_MICROS - 10 * DAY).await;
        node.ingest_at(3, 0..2, SIM_EPOCH_MICROS).await;
        node.sleep(Duration::from_secs(2)).await;
        let (ch, landed) = (node.ch.clone(), Arc::new(AtomicU32::new(0)));
        node.hooked.set_hook(Arc::new(move |table_id, partition| {
            let (ch, landed) = (ch.clone(), Arc::clone(&landed));
            Box::pin(async move {
                if table_id == TableId::SpanCore
                    && partition == day(10)
                    && landed.fetch_add(1, Ordering::SeqCst) == 0
                {
                    let start = SIM_EPOCH_MICROS - 10 * DAY + 3_600_000_000;
                    land_span(&ch, old, 9, Timestamp::from_micros(start), 1).await;
                }
            })
        }));
        let done = node.delete_and_purge(project, 0x0c).await;
        assert_eq!(
            node.span_core_kinds(done.id).await,
            BTreeMap::from([(day(10), ActionKind::Delete), (day(0), ActionKind::Delete)])
        );
        assert_eq!(node.visible(old), 1, "the old project's row is gone");
    });
}

/// Contract 13.4: a project made while a deletion runs keeps its rows. With no live project, the rule before dropped
/// every partition of the deleted project, today's included, so a project made between that decision and the drop
/// lost what it wrote today. Now today's partition, which a project made at any later moment can write to, gets a
/// delete of the deleted project's rows, and the new project's row, which lands just before that purge reaches the
/// store, stays; the partition of ten days ago, before the reach of every project, is dropped.
#[test]
fn a_project_made_while_a_deletion_runs_keeps_its_rows() {
    directed_with(five_days, |node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest_at(1, 0..2, SIM_EPOCH_MICROS - 10 * DAY).await;
        node.ingest_at(3, 0..2, SIM_EPOCH_MICROS).await;
        node.sleep(Duration::from_secs(2)).await;
        let newer = made(5, "Five", SIM_EPOCH_MICROS);
        let (ch, pg, clock) = (
            node.ch.clone(),
            node.pg.clone(),
            Arc::clone(&node.host.env.clock),
        );
        let landed = Arc::new(AtomicU32::new(0));
        let id = newer.id;
        node.hooked.set_hook(Arc::new(move |table_id, partition| {
            let (ch, pg, clock, landed) = (ch.clone(), pg.clone(), clock.clone(), Arc::clone(&landed));
            let newer = newer.clone();
            Box::pin(async move {
                if table_id == TableId::SpanCore
                    && partition == day(0)
                    && landed.fetch_add(1, Ordering::SeqCst) == 0
                {
                    pg.projects().insert(&newer).await.unwrap();
                    land_span(&ch, newer.id, 9, clock.wall_now(), 1).await;
                }
            })
        }));
        let done = node.delete_and_purge(project, 0x0b).await;
        assert_eq!(
            node.span_core_kinds(done.id).await,
            BTreeMap::from([(day(10), ActionKind::Drop), (day(0), ActionKind::Delete)])
        );
        assert_eq!(node.visible(id), 1, "the new project's row is gone");
    });
}

/// Contract 13.4: a project with more rows than one read may scan is still purged. The executor finds the partitions
/// that hold its rows from the partitions of each table and a probe of at most one row in each, never by a read over
/// all of the project's rows, which a server refuses past the limits of the read's class.
#[test]
fn a_project_too_large_for_one_read_is_purged() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..3).await;
        node.ingest_at(2, 0..2, SIM_EPOCH_MICROS - 3 * DAY).await;
        node.sleep(Duration::from_secs(2)).await;
        node.hooked.refuse_wide.store(true, Ordering::SeqCst);
        let done = node.delete_and_purge(project, 0x0a).await;
        assert_eq!(done.error, None);
        assert_eq!(node.held(1) + node.held(2), 0);
    });
}

/// A read that finds where a deleted project's rows are and keeps failing is recorded on the request, with how many
/// times it failed, so the Settings list shows it rather than a line in a debug log. Once the reads answer, the
/// request goes on, completes, and the error goes.
#[test]
fn a_failing_read_of_a_project_deletion_is_recorded_on_the_request() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..3).await;
        node.sleep(Duration::from_secs(2)).await;
        node.hooked.failing.store(3, Ordering::SeqCst);
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([0x09; 16]),
            project,
            node.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        node.mark_deleted(project, Some(request.clone())).await;
        let mut waiting = node.request(request.id).await;
        for _ in 0..40 {
            if waiting.attempts > 0 {
                break;
            }
            node.sleep(Duration::from_secs(5)).await;
            waiting = node.request(request.id).await;
        }
        assert_eq!(waiting.state, DeletionState::Logged, "{waiting:?}");
        assert!(waiting.attempts > 0, "{waiting:?}");
        assert!(
            waiting
                .error
                .as_deref()
                .is_some_and(|error| error.contains("tries again")),
            "{waiting:?}"
        );
        node.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        let done = node.request(request.id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        assert_eq!((done.attempts, done.error.as_deref()), (3, None), "{done:?}");
        node.assert_purged(project);
    });
}

/// Contract 13.4: a project that an older version deleted (its row marked deleted, without a project deletion
/// request) gets that request from the deletion executor, made by `system:upgrade`, and is purged like a project
/// deleted now. The executor runs every 30 s, and a second server's leader jobs take the lease in turn, yet the
/// project gets one request; a project deleted with its request gets no second one, and a live project none.
#[test]
fn a_project_deleted_by_an_older_version_gets_one_project_deletion_and_is_purged() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        let (with, live) = (self::project(3, "Three"), self::project(4, "Four"));
        node.pg.projects().insert(&with).await.unwrap();
        node.pg.projects().insert(&live).await.unwrap();
        node.ingest(1, 0..3).await;
        node.sleep(Duration::from_secs(2)).await;
        let _second = second_server(&node);
        node.mark_deleted(project, None).await;
        let made = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([0x0f; 16]),
            with.id,
            node.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        node.mark_deleted(with.id, Some(made)).await;

        node.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        let requests = |id| {
            let repo = node.pg.lifecycle();
            async move { repo.list_requests(id, None, 10).await.unwrap() }
        };
        let earlier = requests(project).await;
        assert_eq!(earlier.len(), 1, "{earlier:?}");
        assert_eq!(earlier[0].selector, DeletionSelector::Project);
        assert_eq!(earlier[0].requested_by, EARLIER_DELETION_BY);
        assert_eq!(earlier[0].state, DeletionState::Completed, "{earlier:?}");
        node.assert_purged(project);
        assert_eq!(node.held(1), 0);
        let others: Vec<usize> = [requests(with.id).await, requests(live.id).await]
            .iter()
            .map(Vec::len)
            .collect();
        assert_eq!(others, [1, 0]);
        let unrequested = node.pg.lifecycle().deleted_without_request().await.unwrap();
        assert!(unrequested.is_empty(), "{unrequested:?}");
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
        // Each export's file is an object on the store, recorded under the export's claim.
        let file = |n: u8| {
            let id = ExportJobId::from_bytes([n; 16]);
            let key = format!(
                "media/{}/exports/{id}/1/crucible-export-{id}.jsonl",
                ProjectId::from_bytes(PROJECT)
            );
            (key.clone(), format!("object:{key}"))
        };
        let commit = |n: u8| {
            let (pg, objects) = (node.pg.clone(), Arc::clone(&node.objects));
            let now = node.host.env.clock.wall_now();
            async move {
                let (key, reference) = file(n);
                objects.plant(&key, bytes::Bytes::from_static(b"{}\n"), now);
                let claim = pg
                    .claims()
                    .claim(Queue::ExportJobs, 1, Duration::from_secs(60))
                    .await
                    .unwrap();
                let commit = ExportCommit {
                    file: reference,
                    checksum: "ab".repeat(32),
                    spans: 1,
                    bytes: 3,
                    skipped: 0,
                    expires_at: now.saturating_add_micros(7 * 86_400_000_000),
                };
                assert!(pg.export_jobs().commit(&claim[0], &commit).await.unwrap());
            }
        };
        let before = node.host.env.clock.wall_now();
        exports.insert(&export(1, before)).await.unwrap();
        commit(1).await;
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
        commit(2).await;
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
            .map(|job| {
                job.expires_at
                    .is_some_and(|at| at <= node.host.env.clock.wall_now())
            })
            .collect();
        assert_eq!(expired, [true, false]);
        // The file of the expired export is removed from the object store once the expiry is recorded; the later
        // export's file stays.
        let stored = node.objects.objects();
        assert!(!stored.contains_key(&file(1).0), "{:?}", stored.keys());
        assert!(stored.contains_key(&file(2).0), "{:?}", stored.keys());
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
                    .uses(
                        ProjectId::from_bytes(PROJECT),
                        TraceId::from_bytes([trace; 16]),
                        None,
                    )
                    .await
                    .unwrap()
                    .iter()
                    .map(|usage| usage.reference.target_time)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(left(1).await, [after]);
        assert_eq!(left(2).await, [before]);
        assert!(
            done.places.contains("\"media\":\"done\""),
            "{}",
            done.places
        );
    });
}
