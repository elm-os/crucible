//! Directed tests of `crucible-lifecycle` on one node with the real WAL and the real shipper (contract 13.3):
//! the tombstone of a deletion request is a durable frame of the node's WAL, the shipper's checkpoint passes it
//! on a node that receives nothing else, and from there the executor deletes, masks and verifies.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crucible_auth::test_support::scope;
use crucible_chfmt::EncodedBlock;
use crucible_control::{
    ActionKind, ActionState, ControlStore, DeletionRequest, DeletionSelector, ExportCommit,
    ExportFormat, JsonDoc, MediaObject, MediaRef, MediaState, NewExport, Project, Queue,
    Repos as _,
};
use crucible_env::{
    InsertError, InsertSettings, MutationId, MutationStatus, QueryError, RetryReason, RowStream,
    StoreError, StoreFacts, StoreFlavour, StoreHealth, Stores, TelemetryStore,
};
use crucible_jobs::Registry;
use crucible_lifecycle::{
    EARLIER_DELETION_BY, ExportObjects, LeaderSource, LeaderStores, LifecycleConfig, LogError,
    NewDeletion, NodeLog, NodeState, Progress, Service, Subjects, WalLog, leader_jobs, node_job,
};
use crucible_plan::{BatchProbe, DeletePlan, QueryPlan, Value};
use crucible_records::{
    BatchToken, DeletionState, ExportJobId, Lsn, MediaObjectId, PartitionId, ProjectId, TableId,
    Timestamp, TraceId, Uuid, WalEpoch,
};
use crucible_schema::{Granularity, MigrationStep, route, table};
use crucible_shipper::{Hooks, Shipper, ShipperConfig};
use crucible_sim::blocks::Rows;
use crucible_sim::ch_model::{ChModel, Script};
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
    /// A project whose probes fail, as a probe that reads past many hidden rows runs into the read deadline.
    failing_project: Arc<Mutex<Option<ProjectId>>>,
    /// A table whose partition list and probes fail.
    failing_table: Arc<Mutex<Option<TableId>>>,
    /// A table and how many more of its deletes the server refuses outright.
    refused_deletes: Arc<Mutex<Option<(TableId, u32)>>>,
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
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }

    /// Whether a probe of `project` in `table` fails.
    fn probe_fails(&self, table_id: TableId, project: Option<ProjectId>) -> bool {
        *self.failing_table.lock().unwrap() == Some(table_id)
            || project.is_some_and(|project| *self.failing_project.lock().unwrap() == Some(project))
    }

    /// Whether the server refuses this delete outright.
    fn refuses(&self, table_id: TableId) -> bool {
        let mut refused = self.refused_deletes.lock().unwrap();
        match refused.as_mut() {
            Some((table, left)) if *table == table_id && *left > 0 => {
                *left -= 1;
                true
            }
            _ => false,
        }
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
        if self.refuses(plan.table()) {
            return Err(StoreError::Rejected {
                code: 241,
                message: "memory limit exceeded".to_owned(),
            });
        }
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
        if self.fails() || self.probe_fails(table_id, None) {
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
        if self.fails() || self.probe_fails(table_id, Some(project)) {
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
    directed_beside(configure, None, |node, _| body(node));
}

/// [`directed_with`] and, when `skew` is given, the host of a second server, whose wall clock is `skew` microseconds
/// ahead of the first's.
fn directed_beside<F, Fut>(configure: impl FnOnce(&mut LifecycleConfig), skew: Option<i64>, body: F)
where
    F: FnOnce(Node, Option<Host>) -> Fut,
    Fut: Future<Output = ()>,
{
    run(11, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let second = skew.map(|skew| sim.host("node-2", &sim.disk("node-2"), skew));
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
            failing_project: Arc::default(),
            failing_table: Arc::default(),
            refused_deletes: Arc::default(),
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
        body(node, second).await;
    });
}

/// The log of a second server: a tombstone is durable at once, and the checkpoint passes it at once.
#[derive(Debug)]
struct MemLog {
    epoch: WalEpoch,
    last: AtomicU64,
}

#[async_trait]
impl NodeLog for MemLog {
    fn epoch(&self) -> WalEpoch {
        self.epoch
    }

    async fn append_tombstone(&self, _: &DeletionRequest) -> Result<Lsn, LogError> {
        Ok(Lsn::new(self.last.fetch_add(1, Ordering::SeqCst) + 1))
    }

    fn watermark(&self) -> Lsn {
        Lsn::new(self.last.load(Ordering::SeqCst))
    }
}

/// Starts the node timer of a second server on `host`, which registers `version` in the node registry: it
/// heartbeats with its own clock, records every tombstone and passes it at once.
fn second_node(node: &Node, host: &Host, version: &str) -> crucible_jobs::Running {
    let log = MemLog {
        epoch: WalEpoch::from_uuid(Uuid::from_v7_parts(1_700_000_000_000, 2)),
        last: AtomicU64::new(0),
    };
    let state = Arc::new(NodeState::new(Arc::new(log), version, ""));
    let mut registry = Registry::new();
    registry.register(node_job(state, &LifecycleConfig::default()));
    let control: Arc<dyn ControlStore> = Arc::new(node.pg.clone());
    registry.start(&host.env, Some(control), "node#2")
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
        self.delete_at_and_purge(project, id, self.host.env.clock.wall_now())
            .await
    }

    /// [`Node::delete_and_purge`] for a project deleted at `at`.
    async fn delete_at_and_purge(
        &self,
        project: ProjectId,
        id: u8,
        at: Timestamp,
    ) -> DeletionRequest {
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([id; 16]),
            project,
            at,
            "user:test".to_owned(),
        );
        self.mark_deleted(project, Some(request.clone())).await;
        self.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        let done = self.request_of(project, request.id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        self.assert_purged(project);
        done
    }

    /// The request `id` of `project`.
    async fn request_of(
        &self,
        project: ProjectId,
        id: crucible_records::DeletionRequestId,
    ) -> DeletionRequest {
        self.pg
            .lifecycle()
            .request(project, id)
            .await
            .unwrap()
            .unwrap()
    }

    /// Plants an object under the project's key prefix, as an export file or a media object leaves one, and gives
    /// the project a salt.
    async fn plant_project_rows(&self, project: ProjectId) {
        let key = format!("media/{project}/exports/x/1/crucible-export-x.jsonl");
        let now = self.host.env.clock.wall_now();
        self.objects
            .plant(&key, bytes::Bytes::from_static(b"{}\n"), now);
        self.pg
            .project_salts()
            .insert_if_absent(&crucible_control::SealedProjectSalt {
                project_id: project,
                key_id: "master-0".to_owned(),
                nonce: vec![1; 24],
                ciphertext: vec![1; 48],
            })
            .await
            .unwrap();
    }

    /// Whether the project's salt and every object under its key prefix are gone.
    async fn project_rows_gone(&self, project: ProjectId) -> (bool, bool) {
        let salt = self
            .pg
            .project_salts()
            .get(project)
            .await
            .unwrap()
            .is_none();
        let objects = !self
            .objects
            .objects()
            .keys()
            .any(|key| key.starts_with(&format!("media/{project}/")));
        (salt, objects)
    }

    /// Waits up to `limit` for the request to reach `state`, looking every 30 s, and returns it as it then stands.
    async fn wait_for(
        &self,
        id: crucible_records::DeletionRequestId,
        state: DeletionState,
        limit: Duration,
    ) -> DeletionRequest {
        let mut waited = Duration::ZERO;
        let mut now = self.request(id).await;
        while now.state != state && waited < limit {
            self.sleep(Duration::from_secs(30)).await;
            waited += Duration::from_secs(30);
            now = self.request(id).await;
        }
        now
    }
}

const SWEEP_EVERY: Duration = Duration::from_secs(3_600);

/// A project deletion request of the project, made now.
fn purge_of(node: &Node, project: ProjectId, id: u8) -> DeletionRequest {
    DeletionRequest::project(
        crucible_records::DeletionRequestId::from_bytes([id; 16]),
        project,
        node.host.env.clock.wall_now(),
        "user:test".to_owned(),
    )
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
            let (ch, pg, clock, landed) =
                (ch.clone(), pg.clone(), clock.clone(), Arc::clone(&landed));
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
/// that hold its rows from the partitions of each table and a probe that stops at the first row in each, never by a
/// read over all of the project's rows, which a server refuses past the limits of the read's class.
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
        assert_eq!(
            (done.attempts, done.error.as_deref()),
            (3, None),
            "{done:?}"
        );
        node.assert_purged(project);
    });
}

/// Contract 13.4: a row of a deleted project that lands after its purge has completed is removed at the next pass of
/// the purge sweep. The node logs the project's tombstone, then takes one more frame of the project, as a node does
/// before its key snapshot has the revocation; stage B keeps it, since it arrived after the deletion, and its insert
/// is held until the request has completed. Within one sweep interval of landing its rows are deleted, and after the
/// next mask cycle they are gone; the request stays completed and counts its late purges.
#[test]
fn a_row_that_lands_after_the_purge_is_removed_at_the_next_sweep() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([0x0f; 16]),
            project,
            node.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        node.mark_deleted(project, Some(request.clone())).await;
        let mut tombstone = None;
        for _ in 0..60 {
            let acks = node.pg.lifecycle().acks(request.id).await.unwrap();
            if let Some(ack) = acks.first() {
                tombstone = Some(ack.tombstone_lsn);
                break;
            }
            node.sleep(Duration::from_secs(1)).await;
        }
        let tombstone = tombstone.expect("the node logs the tombstone");
        while node.log.watermark() < tombstone {
            node.sleep(Duration::from_millis(500)).await;
        }
        let hold = Arc::new(AtomicBool::new(true));
        let held = Arc::clone(&hold);
        node.ch.set_script(Script::new(move |token, _| {
            (held.load(Ordering::SeqCst) && token.first_lsn() > tombstone).then(|| {
                InsertError::Retry {
                    reason: RetryReason::TooManyQueries,
                    message: "held".to_owned(),
                }
            })
        }));
        assert!(node.ingest(3, 0..1).await > tombstone);
        let mut done = node.request(request.id).await;
        for _ in 0..1_000 {
            if done.state == DeletionState::Completed {
                break;
            }
            node.sleep(Duration::from_secs(5)).await;
            done = node.request(request.id).await;
        }
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        assert_eq!(node.held(3), 0);

        hold.store(false, Ordering::SeqCst);
        for _ in 0..60 {
            if node.held(3) > 0 {
                break;
            }
            node.sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(node.held(3), 1, "the late frame landed");
        // Within one sweep interval, and the next run of the executor, its rows are deleted from every read.
        node.sleep(LifecycleConfig::default().sweep_every + Duration::from_secs(90))
            .await;
        assert_eq!(node.visible(project), 0, "the late rows are still visible");
        node.sleep(Duration::from_secs(2 * 1_800 + 180)).await;
        node.assert_purged(project);
        let after = node.request(request.id).await;
        assert_eq!(after.state, DeletionState::Completed, "{after:?}");
        let progress = Progress::parse(&after.places);
        assert!(
            progress.late > 0 && progress.late_at.is_some(),
            "{}",
            after.places
        );
        let open = node.pg.lifecycle().open_actions().await.unwrap();
        assert!(open.is_empty(), "{open:?}");
    });
}

/// Contract 13.4: no purge touches a live project. Nothing makes a request of kind `project` for a project that is not
/// deleted, but should one exist, the executor fails it and the purge sweep passes it by, so every row of the project
/// stays.
#[test]
fn a_project_deletion_of_a_live_project_removes_nothing() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..3).await;
        node.sleep(Duration::from_secs(2)).await;
        let request = DeletionRequest::project(
            crucible_records::DeletionRequestId::from_bytes([0x08; 16]),
            project,
            node.host.env.clock.wall_now(),
            "user:test".to_owned(),
        );
        node.pg.lifecycle().insert_request(&request).await.unwrap();
        node.sleep(LifecycleConfig::default().sweep_every + Duration::from_secs(2 * 1_800 + 180))
            .await;
        let failed = node.request(request.id).await;
        assert_eq!(failed.state, DeletionState::Failed, "{failed:?}");
        assert_eq!(node.visible(project), 3);
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

/// Contract 13.4: a deleted project whose probes keep failing holds back no other deleted project. The probes of the
/// second of three deleted projects fail, as a probe that reads past many hidden rows runs into the read deadline.
/// Late rows of the first and of the third are still removed within one sweep interval, and the failure is recorded
/// on the second's request, with the table and that the sweep looks again, until a pass whose probes answer clears it.
#[test]
fn a_deleted_project_whose_probes_fail_holds_back_no_other() {
    directed(|node| async move {
        let first = ProjectId::from_bytes(PROJECT);
        let (second, third) = (project(2, "Two"), project(3, "Three"));
        node.pg.projects().insert(&second).await.unwrap();
        node.pg.projects().insert(&third).await.unwrap();
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        node.delete_and_purge(first, 0x21).await;
        let failing = node.delete_and_purge(second.id, 0x22).await;
        node.delete_and_purge(third.id, 0x23).await;
        *node.hooked.failing_project.lock().unwrap() = Some(second.id);
        let now = node.host.env.clock.wall_now();
        land_span(&node.ch, first, 31, now, 31).await;
        land_span(&node.ch, third.id, 33, now, 33).await;
        node.sleep(SWEEP_EVERY + Duration::from_secs(90)).await;
        assert_eq!(
            (node.visible(first), node.visible(third.id)),
            (0, 0),
            "late rows of the first and the third project are still visible"
        );
        let recorded = node.request_of(second.id, failing.id).await;
        assert_eq!(recorded.state, DeletionState::Completed, "{recorded:?}");
        assert!(recorded.attempts > 0, "{recorded:?}");
        assert!(
            recorded
                .error
                .as_deref()
                .is_some_and(|error| error.contains("span_core") && error.contains("again")),
            "{recorded:?}"
        );
        *node.hooked.failing_project.lock().unwrap() = None;
        node.sleep(SWEEP_EVERY + Duration::from_secs(90)).await;
        let cleared = node.request_of(second.id, failing.id).await;
        assert_eq!(cleared.error, None, "{cleared:?}");
    });
}

/// Contract 13.4: the daily pass over the projects deleted more than 30 days ago runs whatever the hourly pass meets.
/// The probes of a recent deletion fail, and a late row of a project deleted 31 days ago is still removed within a
/// day.
#[test]
fn a_deleted_project_whose_probes_fail_holds_back_no_daily_pass() {
    directed(|node| async move {
        let old = ProjectId::from_bytes(PROJECT);
        let recent = project(2, "Two");
        node.pg.projects().insert(&recent).await.unwrap();
        let month_ago = node
            .host
            .env
            .clock
            .wall_now()
            .saturating_add_micros(-31 * DAY);
        node.delete_at_and_purge(old, 0x24, month_ago).await;
        node.delete_and_purge(recent.id, 0x25).await;
        *node.hooked.failing_project.lock().unwrap() = Some(recent.id);
        land_span(&node.ch, old, 34, node.host.env.clock.wall_now(), 34).await;
        let mut hours = 0;
        while node.visible(old) > 0 && hours < 26 {
            node.sleep(Duration::from_secs(3_600)).await;
            hours += 1;
        }
        assert_eq!(
            node.visible(old),
            0,
            "the late row of the project deleted 31 days ago is still visible after {hours} h"
        );
    });
}

/// Contract 13.4: while the object store refuses calls, a project deletion whose telemetry is verified waits at
/// `verified`, with how many attempts failed and an error that says the object store refused and what to check, and
/// tries again. Once the store answers, the project's objects and control-plane rows go and the request completes.
#[test]
fn an_object_store_that_refuses_holds_a_project_deletion_at_verified_and_says_why() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        node.plant_project_rows(project).await;
        node.objects.set_faults(ObjectFaults {
            throttle_ppm: 1_000_000,
            ..ObjectFaults::default()
        });
        let request = purge_of(&node, project, 0x26);
        node.mark_deleted(project, Some(request.clone())).await;
        node.sleep(Duration::from_secs(4 * 1_800 + 180)).await;
        let held = node.request(request.id).await;
        assert_eq!(held.state, DeletionState::Verified, "{held:?}");
        assert!(held.attempts > 0, "{held:?}");
        assert!(
            held.error
                .as_deref()
                .is_some_and(|error| error.contains("object store") && error.contains("check")),
            "{held:?}"
        );
        assert_eq!(node.project_rows_gone(project).await, (false, false));
        node.objects.set_faults(ObjectFaults::default());
        node.sleep(Duration::from_secs(90)).await;
        let done = node.request(request.id).await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        assert_eq!(done.error, None, "{done:?}");
        assert_eq!(node.project_rows_gone(project).await, (true, true));
        node.assert_purged(project);
    });
}

/// Contract 13.4: rows of a deleted project that keep landing never fail its deletion. A frame of the project lands
/// every minute for six hours, as from a server cut off from Postgres under `fail_open`, across twelve mask cycles:
/// rows that a purge's check finds after its delete arrived after it, and the purge, which takes every row of the
/// project whatever its arrival, runs again without counting a failed attempt. The sweep sends a purge whose delete
/// has run again rather than add one each hour, so a partition never has more than two purges at once. Once the
/// frames stop, the purge completes with the project's objects and control-plane rows.
#[test]
fn rows_that_keep_landing_never_fail_a_project_deletion() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        node.plant_project_rows(project).await;
        let request = purge_of(&node, project, 0x27);
        node.mark_deleted(project, Some(request.clone())).await;
        for minute in 0..360_u32 {
            let tag = 10 + u8::try_from(minute / 250).unwrap();
            let span = u8::try_from(minute % 250).unwrap();
            node.ingest(tag, span..span + 1).await;
            node.sleep(Duration::from_secs(60)).await;
            let now = node.request(request.id).await;
            assert_ne!(
                now.state,
                DeletionState::Failed,
                "after {minute} minutes of late rows: {now:?}"
            );
        }
        let mut purges: BTreeMap<(TableId, PartitionId), usize> = BTreeMap::new();
        for action in node
            .pg
            .lifecycle()
            .request_actions(request.id)
            .await
            .unwrap()
        {
            *purges.entry((action.table, action.partition)).or_default() += 1;
        }
        let most = purges.values().copied().max().unwrap_or(0);
        assert!(most <= 2, "purges of one partition piled up: {purges:?}");
        let done = node
            .wait_for(
                request.id,
                DeletionState::Completed,
                Duration::from_secs(4 * 3_600),
            )
            .await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        assert_eq!(node.project_rows_gone(project).await, (true, true));
        node.sleep(Duration::from_secs(2 * 1_800)).await;
        node.assert_purged(project);
    });
}

/// Contract 13.4: a project deletion that failed for good still finishes. The server refuses every delete of
/// `span_core` until that purge is given up and the request fails; the purge sweep then deletes the rows the purge
/// left, and once it finds no row of the project in any table and every delete of the request is removed from
/// storage, the project's objects and control-plane rows go and the request completes. With the mask cycle at six
/// hours, the sweep's probes find nothing hours before the mask removes the deleted rows, and the request waits for
/// it.
#[test]
fn a_failed_project_deletion_finishes_once_the_sweep_finds_every_table_empty() {
    directed_with(
        |config| config.mask_cycle = Duration::from_secs(6 * 3_600),
        |node| async move {
            let project = ProjectId::from_bytes(PROJECT);
            node.ingest(1, 0..2).await;
            node.sleep(Duration::from_secs(2)).await;
            node.plant_project_rows(project).await;
            *node.hooked.refused_deletes.lock().unwrap() = Some((TableId::SpanCore, 8));
            let request = purge_of(&node, project, 0x28);
            node.mark_deleted(project, Some(request.clone())).await;
            let failed = node
                .wait_for(
                    request.id,
                    DeletionState::Failed,
                    Duration::from_secs(1_800),
                )
                .await;
            assert_eq!(failed.state, DeletionState::Failed, "{failed:?}");
            let done = node
                .wait_for(
                    request.id,
                    DeletionState::Completed,
                    Duration::from_secs(9 * 3_600),
                )
                .await;
            assert_eq!(done.state, DeletionState::Completed, "{done:?}");
            let open: Vec<(TableId, PartitionId, ActionState)> = node
                .pg
                .lifecycle()
                .request_actions(request.id)
                .await
                .unwrap()
                .into_iter()
                .filter(|action| {
                    !matches!(action.state, ActionState::Verified | ActionState::Failed)
                })
                .map(|action| (action.table, action.partition, action.state))
                .collect();
            assert!(
                open.is_empty(),
                "completed before its deletes were removed from storage: {open:?}"
            );
            assert_eq!(node.project_rows_gone(project).await, (true, true));
            assert!(
                done.places.contains("\"control_plane\":\"done\""),
                "{}",
                done.places
            );
            node.assert_purged(project);
        },
    );
}

/// Contract 13.4: a failed project deletion keeps saying why it failed while the purge sweep cannot look at its
/// project. The request fails as above; then the probes of its project fail for one sweep pass, which the request
/// records after its reason, with the table and that the sweep tries again. The next pass that goes through takes
/// that away and leaves the reason.
#[test]
fn a_failed_project_deletion_keeps_its_reason_while_the_sweep_cannot_look() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        *node.hooked.refused_deletes.lock().unwrap() = Some((TableId::SpanCore, 8));
        let request = purge_of(&node, project, 0x2b);
        node.mark_deleted(project, Some(request.clone())).await;
        let failed = node
            .wait_for(
                request.id,
                DeletionState::Failed,
                Duration::from_secs(1_800),
            )
            .await;
        assert_eq!(failed.state, DeletionState::Failed, "{failed:?}");
        let reason = failed.error.clone().unwrap();
        assert!(reason.contains("given up"), "{reason}");
        *node.hooked.failing_project.lock().unwrap() = Some(project);
        node.sleep(SWEEP_EVERY + Duration::from_secs(90)).await;
        let recorded = node.request(request.id).await;
        let error = recorded.error.clone().unwrap_or_default();
        assert!(
            error.starts_with(&reason) && error.contains("span_core") && error.contains("again"),
            "{recorded:?}"
        );
        assert!(recorded.attempts > failed.attempts, "{recorded:?}");
        *node.hooked.failing_project.lock().unwrap() = None;
        node.sleep(SWEEP_EVERY + Duration::from_secs(90)).await;
        let cleared = node.request(request.id).await;
        assert_eq!(
            (cleared.state, cleared.error.as_deref()),
            (DeletionState::Failed, Some(reason.as_str())),
            "{cleared:?}"
        );
    });
}

/// Contract 13.4: the purge sweep also looks at the tables that a project deletion's discovery has passed, while
/// discovery goes on with the rest. Discovery is held at `trace_stats`, and a row of the project that lands in a
/// `span_core` partition that discovery did not find is removed within one sweep interval.
#[test]
fn the_sweep_removes_late_rows_from_the_tables_discovery_has_passed() {
    directed(|node| async move {
        let project = ProjectId::from_bytes(PROJECT);
        node.ingest(1, 0..2).await;
        node.sleep(Duration::from_secs(2)).await;
        *node.hooked.failing_table.lock().unwrap() = Some(TableId::TraceStats);
        let request = purge_of(&node, project, 0x29);
        node.mark_deleted(project, Some(request.clone())).await;
        let passed = |request: &DeletionRequest| {
            Progress::parse(&request.places)
                .purged
                .iter()
                .any(|table| table == "trace_heads")
        };
        let mut waiting = node.request(request.id).await;
        for _ in 0..60 {
            if passed(&waiting) {
                break;
            }
            node.sleep(Duration::from_secs(5)).await;
            waiting = node.request(request.id).await;
        }
        assert!(passed(&waiting), "{waiting:?}");
        let start = SIM_EPOCH_MICROS - 3 * DAY + 3_600_000_000;
        land_span(&node.ch, project, 35, Timestamp::from_micros(start), 35).await;
        node.sleep(SWEEP_EVERY + Duration::from_secs(90)).await;
        assert_eq!(node.visible(project), 0, "the late row is still visible");
        let still = node.request(request.id).await;
        assert!(!Progress::parse(&still.places).planned, "{still:?}");
        *node.hooked.failing_table.lock().unwrap() = None;
        let done = node
            .wait_for(
                request.id,
                DeletionState::Completed,
                Duration::from_secs(3 * 1_800),
            )
            .await;
        assert_eq!(done.state, DeletionState::Completed, "{done:?}");
        node.sleep(Duration::from_secs(2 * 1_800)).await;
        node.assert_purged(project);
    });
}

/// Contract 13.4: a server of 0.1.3 or earlier cannot read a deletion request of kind `project`, and one such row fails
/// its every read of deletion requests. While a server that registered such a version is live, the server that holds
/// the `lifecycle` lease makes no project deletion request, and the deleted project's list of deletion requests says
/// what its purge waits for. Once that server is gone, the request is made under the id the list showed, and the
/// purge completes.
#[test]
fn a_server_of_an_earlier_version_holds_project_deletions_until_it_is_gone() {
    directed_beside(
        |_| {},
        Some(0),
        |node, second| async move {
            let project = ProjectId::from_bytes(PROJECT);
            let second = second.unwrap();
            let _old = second_node(&node, &second, "0.1.3");
            node.ingest(1, 0..2).await;
            node.sleep(Duration::from_secs(10)).await;
            node.mark_deleted(project, None).await;
            node.sleep(Duration::from_secs(120)).await;
            let repo = node.pg.lifecycle();
            let made = repo.list_requests(project, None, 10).await.unwrap();
            assert!(
                made.is_empty(),
                "a project deletion request was made while a server of 0.1.3 runs: {made:?}"
            );
            let scope = scope(project).unwrap();
            let listed = node.service.deletions(scope, None, 10).await.unwrap();
            assert_eq!(listed.len(), 1, "{listed:?}");
            assert_eq!(
                (listed[0].selector.clone(), listed[0].state),
                (DeletionSelector::Project, DeletionState::Pending)
            );
            assert!(
                listed[0]
                    .error
                    .as_deref()
                    .is_some_and(|why| why.contains("(0.1.3)") && why.contains("Upgrade or stop")),
                "{listed:?}"
            );
            let one = node.service.deletion(scope, listed[0].id).await.unwrap();
            assert_eq!(one.as_ref(), Some(&listed[0]));

            second.stop().await;
            node.sleep(Duration::from_secs(120)).await;
            let made = repo.list_requests(project, None, 10).await.unwrap();
            assert_eq!(made.len(), 1, "{made:?}");
            assert_eq!(made[0].id, listed[0].id);
            let done = node
                .wait_for(
                    made[0].id,
                    DeletionState::Completed,
                    Duration::from_secs(3 * 1_800),
                )
                .await;
            assert_eq!(done.state, DeletionState::Completed, "{done:?}");
            node.assert_purged(project);
        },
    );
}

/// Contract 13.4: the drop rule counts on every server's clock being within the clock margin of the database clock,
/// and the leader checks it at each heartbeat's record. A second server's clock is 45 minutes ahead, more than half of
/// the hour's margin, so the partition of ten days ago, which the deletion drops while the clocks agree
/// (`a_project_deletion_drops_a_partition_only_where_no_project_can_write_any_more`), gets a delete of the project's
/// rows instead.
#[test]
fn a_server_clock_far_off_the_database_clock_makes_a_purge_delete_instead_of_drop() {
    directed_beside(
        five_days,
        Some(45 * 60_000_000),
        |node, second| async move {
            let project = ProjectId::from_bytes(PROJECT);
            let second = second.unwrap();
            let _ahead = second_node(&node, &second, "test");
            node.pg
                .projects()
                .insert(&made(2, "Two", SIM_EPOCH_MICROS))
                .await
                .unwrap();
            for (tag, back) in [(1, 10), (3, 0)] {
                node.ingest_at(tag, 0..2, SIM_EPOCH_MICROS - back * DAY)
                    .await;
            }
            node.sleep(Duration::from_secs(10)).await;
            let done = node.delete_and_purge(project, 0x2a).await;
            assert_eq!(
                node.span_core_kinds(done.id).await,
                BTreeMap::from([(day(10), ActionKind::Delete), (day(0), ActionKind::Delete)])
            );
        },
    );
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
