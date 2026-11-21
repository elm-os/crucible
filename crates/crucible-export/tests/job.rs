//! The export job on the virtual clock (contracts 11.7 and 12.8): a crash at any disk operation of an export
//! leaves no partial file under a final name, and the export resumes on the restarted node, which commits one
//! complete file whose checksum the row records; the sweep then leaves only that file. Also: a deletion's purge
//! expires the export and removes its file, the API service's rate limit and `Idempotency-Key`, and, with an
//! object store, a file that is an object any node hands out a link to, which expiry and purge remove.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use crucible_chfmt::EncodedBlock;
use crucible_control::{
    ControlStore, ExportFormat, ExportJob, JsonDoc, NewExport, Project, Repos as _,
};
use crucible_env::{
    ByteStream, Clock, DiskPath, Disposition, Env, InsertError, InsertSettings, MutationId,
    MutationStatus, ObjectError, ObjectMeta, ObjectPage, ObjectStore, OpenMode, Presigned,
    QueryError, Root, RowStream, StoreError, StoreFacts, StoreHealth, Stores, TelemetryStore,
};
use crucible_export::{
    CreateError, CreateExport, Created, DOWNLOAD_TTL, Download, DownloadError, EXPORT_LEASE,
    EXPORT_TTL, ExportObjects, ExportRuntime, ExportState, Exports, Options, PART_BYTES,
    SWEEP_EVERY, Selector, job, object_sweep_job, purge, sweep, sweep_job, sweep_objects,
};
use crucible_jobs::{Registry, Running};
use crucible_plan::{BatchProbe, DeletePlan, Node, QueryPlan, Row, RowBlock, Value};
use crucible_query::QueryService;
use crucible_recent::{RecentConfig, RecentIndex, SpanRead, WalLocator, WalReadError, WalView};
use crucible_records::{
    ExportJobId, Lsn, PartitionId, ProjectId, TableId, TimeWindow, Timestamp, TraceId, Uuid,
    WalEpoch,
};
use crucible_schema::{MigrationStep, Ty, table};
use crucible_sim::object_model::{ObjectFaults, SimObjectStore};
use crucible_sim::pg_model::{PgFaults, PgModel};
use crucible_sim::{Host, SimDisk};
use futures::FutureExt as _;
use sha2::{Digest, Sha256};

const PROJECT: ProjectId = ProjectId::from_bytes([0x5a; 16]);
const TRACE: TraceId = TraceId::from_bytes([0xc3; 16]);
const SPANS: u64 = 2_500;

/// The zero value of a column type.
fn zero(ty: &Ty) -> Value {
    match ty {
        Ty::Uuid => Value::Uuid(Uuid::NIL),
        Ty::UInt8 | Ty::UInt16 | Ty::UInt32 | Ty::UInt64 => Value::UInt(0),
        Ty::Int32 | Ty::Int64 => Value::Int(0),
        Ty::Float32 | Ty::Float64 => Value::Float(0.0),
        Ty::String | Ty::LowCardinalityString => Value::String(String::new()),
        Ty::FixedString(n) => Value::Bytes(vec![0; usize::from(*n)]),
        Ty::Date => Value::Date(Timestamp::EPOCH.date()),
        Ty::DateTime | Ty::DateTime64 => Value::Time(Timestamp::EPOCH),
        Ty::Enum8(kind) => Value::Enum(kind.variants()[0].1),
        Ty::Nullable(_) => Value::Null,
        Ty::Array(_) => Value::Array(Vec::new()),
        Ty::Tuple2(a, b) => Value::Tuple2(Box::new(zero(a)), Box::new(zero(b))),
        Ty::Saf(_, inner) => zero(inner),
    }
}

/// A store that holds [`SPANS`] spans of one trace of [`PROJECT`], starting at `base`, one every 10 µs, each
/// received 1 µs after the previous one. It answers the page scans of `span_core` and has no payload, MCP or
/// fact row.
#[derive(Debug)]
struct Spans {
    base: Timestamp,
    /// How long each span's name is: 400 characters, or more for an export of several multipart parts.
    name_len: usize,
}

impl Spans {
    fn start(&self, i: u64) -> Timestamp {
        self.base
            .saturating_add_micros(i64::try_from(i * 10).unwrap())
    }

    fn value(&self, i: u64, name: &str, ty: &Ty) -> Value {
        match name {
            "start_minute" => Value::Time(self.start(i).floor_minute()),
            "trace_hash" => Value::UInt(u64::from(TRACE.trace_hash())),
            "start_time" => Value::Time(self.start(i)),
            "end_time" => Value::Time(self.start(i).saturating_add_micros(5)),
            "span_id" => Value::Bytes((i + 1).to_be_bytes().to_vec()),
            "trace_id" => Value::Bytes(TRACE.to_bytes().to_vec()),
            "name" => Value::String(format!("span {i} {}", "x".repeat(self.name_len))),
            "received_at" => {
                Value::Time(self.base.saturating_add_micros(i64::try_from(i).unwrap()))
            }
            "lsn" => Value::UInt(i + 1),
            "event_bytes" => Value::UInt(500),
            _ => zero(ty),
        }
    }

    fn page(&self, plan: &QueryPlan) -> Vec<Row> {
        let Node::Scan(scan) = plan.root() else {
            return Vec::new();
        };
        if scan.table() != TableId::SpanCore || plan.scope().project_id() != PROJECT {
            return Vec::new();
        }
        // Keys differ in `start_time` only, so the position's start time places it.
        let after = scan.cursor().and_then(|key| match key.get(2) {
            Some(crucible_plan::Literal::Time(t)) => Some(*t),
            _ => None,
        });
        let as_of = scan.arrival().as_of.unwrap_or(Timestamp::MAX_EXCLUSIVE);
        let def = table(TableId::SpanCore);
        (0..SPANS)
            .filter(|i| plan.window().contains(self.start(*i)))
            .filter(|i| after.is_none_or(|after| self.start(*i) > after))
            .filter(|i| self.base.saturating_add_micros(i64::try_from(*i).unwrap()) <= as_of)
            .take(scan.limit().map_or(usize::MAX, |l| l as usize))
            .map(|i| {
                let values = scan.columns().iter().map(|c| {
                    let ty = def.column(c.name()).map_or(Ty::String, |col| col.ty);
                    self.value(i, c.name(), &ty)
                });
                Row::new(values.collect())
            })
            .collect()
    }
}

fn unsupported<T>() -> Result<T, StoreError> {
    Err(StoreError::Internal("the test store only reads".to_owned()))
}

#[async_trait]
impl TelemetryStore for Spans {
    async fn insert_block(&self, _: EncodedBlock, _: InsertSettings) -> Result<(), InsertError> {
        Err(InsertError::FatalConfig {
            code: 0,
            message: "the test store only reads".to_owned(),
        })
    }
    async fn execute(&self, plan: QueryPlan) -> Result<RowStream, QueryError> {
        let blocks = RowBlock::split(self.page(&plan)).into_iter().map(Ok);
        Ok(RowStream::new(
            Uuid::from_bytes([1; 16]),
            futures::stream::iter(blocks),
        ))
    }
    async fn probe(&self, _: BatchProbe) -> Result<bool, StoreError> {
        unsupported()
    }
    async fn delete(&self, _: DeletePlan) -> Result<MutationId, StoreError> {
        unsupported()
    }
    async fn drop_partition(&self, _: TableId, _: PartitionId) -> Result<(), StoreError> {
        unsupported()
    }
    async fn apply_deleted_mask(
        &self,
        _: TableId,
        _: PartitionId,
    ) -> Result<MutationId, StoreError> {
        unsupported()
    }
    async fn mutation_status(&self, _: &MutationId) -> Result<MutationStatus, StoreError> {
        unsupported()
    }
    async fn partition_projects(
        &self,
        _: TableId,
        _: PartitionId,
    ) -> Result<Vec<ProjectId>, StoreError> {
        unsupported()
    }
    async fn masked_parts(&self, _: TableId, _: PartitionId) -> Result<u64, StoreError> {
        unsupported()
    }
    async fn apply_migration(&self, _: &MigrationStep) -> Result<(), StoreError> {
        unsupported()
    }
    async fn detect(&self) -> Result<StoreFacts, StoreError> {
        unsupported()
    }
    async fn dedup_selftest(&self) -> Result<(), StoreError> {
        unsupported()
    }
    async fn kill(&self, _: Uuid) -> Result<(), StoreError> {
        Ok(())
    }
    async fn health(&self) -> StoreHealth {
        StoreHealth::Healthy
    }
}

#[derive(Debug)]
struct NoWal;

impl WalView for NoWal {
    fn durable_lsn(&self) -> Lsn {
        Lsn::ZERO
    }
    fn read_span(&self, _: ProjectId, _: WalLocator) -> SpanRead {
        Box::pin(async { Err(WalReadError::Failed("no wal".to_owned())) })
    }
}

fn project() -> Project {
    Project {
        id: PROJECT,
        name: "Exports".to_owned(),
        slug: "exports".to_owned(),
        settings: JsonDoc::empty(),
        retention_days: None,
        payload_retention_days: None,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

/// A node: the registry with the export job and the sweep, its query service over [`Spans`], writing its files
/// to `objects` when the deployment has an object store.
fn start(
    host: &Host,
    model: &PgModel,
    base: Timestamp,
    objects: Option<&ExportObjects>,
) -> Running {
    start_over(
        host,
        model,
        Spans {
            base,
            name_len: 400,
        },
        objects,
    )
}

/// [`start`] over these spans.
fn start_over(
    host: &Host,
    model: &PgModel,
    spans: Spans,
    objects: Option<&ExportObjects>,
) -> Running {
    let (index, _writer) = RecentIndex::new(
        RecentConfig::new(1 << 20, crucible_env::StoreFlavour::SingleNode),
        Arc::clone(&host.env.clock),
        Arc::new(NoWal),
    );
    let service = QueryService::new(
        Stores::new(Arc::new(spans)),
        Arc::clone(&host.env.clock),
        index,
        WalEpoch::from_bytes([1; 16]),
    );
    let mut runtime = ExportRuntime::new();
    if let Some(objects) = objects {
        runtime = runtime.with_objects(objects.clone());
    }
    runtime.set_service(service);
    let mut registry = Registry::new();
    registry.register(job(runtime));
    registry.register(sweep_job());
    if let Some(objects) = objects {
        registry.register(object_sweep_job(objects.clone()));
    }
    let store: Arc<dyn ControlStore> = Arc::new(model.clone());
    registry.start(&host.env, Some(store), "node")
}

/// Every file under the project's export directory: name and contents.
async fn files(env: &Env) -> Vec<(String, Vec<u8>)> {
    let dir = DiskPath::new(Root::Exports, &PROJECT.to_string()).unwrap();
    let Ok(entries) = env.disk.list(&dir).await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        let file = env
            .disk
            .open(&dir.join(&entry.name).unwrap(), OpenMode::Read)
            .await
            .unwrap();
        let bytes = file
            .read_at(0, usize::try_from(entry.len).unwrap())
            .await
            .unwrap();
        out.push((entry.name, bytes.to_vec()));
    }
    out
}

/// Whether a committed file holds the whole export: one valid line per span.
fn complete(bytes: &[u8]) -> bool {
    let text = std::str::from_utf8(bytes).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    text.ends_with('\n')
        && lines.len() as u64 == SPANS
        && lines
            .iter()
            .all(|l| serde_json::from_str::<serde_json::Value>(l).is_ok())
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn row(model: &PgModel, id: ExportJobId) -> ExportJob {
    model.export_jobs().get(PROJECT, id).await.unwrap().unwrap()
}

/// The checks after a crash, and after the restarted node finished: no file under a final name is partial, and
/// a recorded file is there with the recorded checksum.
async fn check(env: &Env, job: &ExportJob, at: &str) {
    for (name, bytes) in files(env).await {
        if !name.ends_with(".part") {
            assert!(
                complete(&bytes),
                "{at}: the committed file {name} is partial"
            );
        }
    }
    if let Some(file) = &job.file {
        let name = file.rsplit('/').next().unwrap();
        let held = files(env).await.into_iter().find(|(n, _)| n == name);
        let (_, bytes) =
            held.unwrap_or_else(|| panic!("{at}: the recorded file {file} is not there"));
        assert_eq!(
            job.checksum.as_deref(),
            Some(sha256(&bytes).as_str()),
            "{at}: checksum"
        );
        assert_eq!(job.bytes, bytes.len() as u64, "{at}: length");
        assert_eq!(job.spans, SPANS, "{at}: spans");
    }
}

/// What a crash left: whether the node crashed at all, a temporary file, a file under a final name, and whether
/// the row recorded a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Left {
    crashed: bool,
    temporary: bool,
    committed: bool,
    recorded: bool,
}

fn crash_and_resume(seed: u64, crash_after: u64) -> Left {
    crucible_sim::run(seed, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let now = database.clock.wall_now();
        let base = now.saturating_add_micros(-600_000_000);
        let selector = Selector {
            window: TimeWindow::new(base, now).unwrap(),
            trace_ids: Vec::new(),
            user_id: None,
            session_id: None,
        };
        let id = ExportJobId::from_bytes([0x0e; 16]);
        let new = NewExport {
            id,
            project_id: PROJECT,
            format: ExportFormat::CrucibleJsonl,
            selector: selector.to_text(),
            options: Options::default().to_text(),
            dedup_key: id.to_string(),
            created_by: "user:test".to_owned(),
            created_at: now,
            as_of: now,
        };
        model.export_jobs().insert(&new).await.unwrap();

        let disk: SimDisk = sim.disk("node");
        let host = sim.host("node/boot-1", &disk, 0);
        disk.arm_crash_after_ops(crash_after);
        let running = start(&host, &model, base, None);
        let crashed = {
            let crash = disk.crash_point_reached().fuse();
            // Past the sweep's cadence, so the last crash points fall on the sweep after the commit.
            let give_up = database.clock.sleep(Duration::from_secs(1_500)).fuse();
            futures::pin_mut!(crash, give_up);
            futures::select_biased! {
                () = crash => true,
                () = give_up => false,
            }
        };
        let mut left = Left {
            crashed,
            temporary: false,
            committed: false,
            recorded: false,
        };
        if crashed {
            drop(running);
            host.crash().await;
            let after = sim.host("node/inspect", &disk, 0);
            let job = row(&model, id).await;
            check(&after.env, &job, &format!("crash after {crash_after} ops")).await;
            let names: Vec<String> = files(&after.env)
                .await
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            left.temporary = names.iter().any(|n| n.ends_with(".part"));
            left.committed = names.iter().any(|n| !n.ends_with(".part"));
            left.recorded = job.file.is_some();
            after.stop().await;
        } else {
            drop(running);
            host.stop().await;
        }
        disk.disarm_crash();

        // The restarted node takes the export once the crashed attempt's claim has run out.
        let host = sim.host("node/boot-2", &disk, 0);
        let _running = start(&host, &model, base, None);
        let mut done = None;
        for _ in 0..(EXPORT_LEASE.as_secs() / 30 + 20) {
            let job = row(&model, id).await;
            if job.file.is_some() || job.error.is_some() {
                done = Some(job);
                break;
            }
            database.clock.sleep(Duration::from_secs(30)).await;
        }
        let job = done
            .unwrap_or_else(|| panic!("crash after {crash_after} ops: the export never finished"));
        assert_eq!(job.error, None);
        assert_eq!(
            ExportState::of(&job, host.env.clock.wall_now()),
            ExportState::Ready
        );
        check(
            &host.env,
            &job,
            &format!("resumed after a crash after {crash_after} ops"),
        )
        .await;

        // The sweep leaves only the recorded file: it removes a file of an attempt the row does not name (what a
        // crash between the rename and the row leaves) and a temporary file whose attempt holds no claim.
        let dir = DiskPath::new(Root::Exports, &PROJECT.to_string()).unwrap();
        for stray in [format!("{id}.99.jsonl"), format!("{id}.98.part")] {
            let file = host
                .env
                .disk
                .open(&dir.join(&stray).unwrap(), OpenMode::CreateNew)
                .await
                .unwrap();
            file.append(0, vec![bytes::Bytes::from_static(b"{}\n")])
                .await
                .unwrap();
            file.sync_data().await.unwrap();
        }
        host.env.disk.sync_dir(&dir).await.unwrap();
        let swept = sweep(&model, host.env.disk.as_ref(), host.env.clock.wall_now())
            .await
            .unwrap();
        // The two strays, and the crashed attempt's temporary file when no sweep since its claim ran out took it.
        assert!(swept.files >= 2, "crash after {crash_after} ops: {swept:?}");
        let names: Vec<String> = files(&host.env).await.into_iter().map(|(n, _)| n).collect();
        let recorded = job
            .file
            .as_deref()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_owned();
        assert_eq!(names, vec![recorded], "crash after {crash_after} ops");
        left
    })
}

#[test]
fn a_crash_at_any_disk_operation_leaves_no_partial_file_and_the_export_resumes() {
    // An export of this size takes about a dozen disk operations: a crash point at each of them, and beyond.
    let left: Vec<Left> = (1..=16).map(|n| crash_and_resume(n, n)).collect();
    // The crash points fell before the file existed, while a temporary file was being written, and after the
    // file was committed and recorded (on the sweep that followed).
    assert!(
        left.iter()
            .any(|l| l.crashed && !l.temporary && !l.committed),
        "{left:?}"
    );
    assert!(
        left.iter()
            .any(|l| l.crashed && l.temporary && !l.committed),
        "{left:?}"
    );
    assert!(
        left.iter().any(|l| l.crashed && l.committed && l.recorded),
        "{left:?}"
    );
}

#[test]
fn a_deletion_expires_the_exports_it_predates_and_removes_their_files() {
    crash_and_resume_then(|model, env, id, now| async move {
        // Five exports were created before the request, each with its file.
        let purged = purge(&model, env.disk.as_ref(), PROJECT, now, now)
            .await
            .unwrap();
        assert_eq!(purged.expired.len(), 5);
        assert_eq!(purged.files_removed, 5);
        let job = row(&model, id).await;
        assert_eq!(ExportState::of(&job, now), ExportState::Expired);
        assert!(files(&env).await.is_empty());
        // Again: nothing left to expire.
        let again = purge(&model, env.disk.as_ref(), PROJECT, now, now)
            .await
            .unwrap();
        assert!(again.expired.is_empty());
    });
}

/// Contract 13.4: from the moment a project is deleted its exports stop. One waiting to run ends with
/// `project_deleted` and writes no file.
#[test]
fn an_export_of_a_deleted_project_ends_without_a_file() {
    crucible_sim::run(61, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let now = database.clock.wall_now();
        let base = now.saturating_add_micros(-600_000_000);
        let id = ExportJobId::from_bytes([0x0d; 16]);
        let selector = Selector {
            window: TimeWindow::new(base, now).unwrap(),
            trace_ids: Vec::new(),
            user_id: None,
            session_id: None,
        };
        model
            .export_jobs()
            .insert(&NewExport {
                id,
                project_id: PROJECT,
                format: ExportFormat::CrucibleJsonl,
                selector: selector.to_text(),
                options: Options::default().to_text(),
                dedup_key: id.to_string(),
                created_by: "user:test".to_owned(),
                created_at: now,
                as_of: now,
            })
            .await
            .unwrap();
        let deleted = Project {
            deleted_at: Some(now),
            version: 2,
            ..project()
        };
        assert!(
            model
                .projects()
                .update_if_version(&deleted, 1)
                .await
                .unwrap()
        );
        let host = sim.host("node", &sim.disk("node"), 0);
        let _running = start(&host, &model, base, None);
        database.clock.sleep(Duration::from_secs(120)).await;
        let job = row(&model, id).await;
        assert_eq!(job.error.as_deref(), Some("project_deleted"));
        assert_eq!(job.file, None);
        assert!(files(&host.env).await.is_empty());
    });
}

/// Runs one export to completion on a node, then `then` with the model, the node's env, the export id and now.
fn crash_and_resume_then<F, Fut>(then: F)
where
    F: FnOnce(PgModel, Env, ExportJobId, Timestamp) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    crucible_sim::run(7, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let disk = sim.disk("node");
        let host = sim.host("node/boot-1", &disk, 0);
        let exports = Exports::new(host.env.clone(), Arc::new(model.clone()));
        let scope = crucible_auth::test_support::scope(PROJECT).unwrap();
        let now = host.env.clock.wall_now();
        let base = now.saturating_add_micros(-600_000_000);
        let request = |key: &str| CreateExport {
            format: ExportFormat::OtlpJson,
            selector: Selector {
                window: TimeWindow::new(base, now).unwrap(),
                trace_ids: Vec::new(),
                user_id: None,
                session_id: None,
            },
            options: Options::default(),
            created_by: "user:test".to_owned(),
            idempotency_key: Some(key.to_owned()),
        };
        let Created::New(job) = exports.create(scope, request("k1")).await.unwrap() else {
            panic!("a new export")
        };
        // The same request under the same key is a replay; another request under it is refused.
        assert!(
            matches!(exports.create(scope, request("k1")).await.unwrap(), Created::Replay(j) if j.new.id == job.new.id)
        );
        let mut other = request("k1");
        other.options.gzip = true;
        assert!(matches!(
            exports.create(scope, other).await,
            Err(CreateError::KeyReused)
        ));
        // Five per hour per project; replays did not count.
        for n in 2..=5 {
            assert!(matches!(
                exports.create(scope, request(&format!("k{n}"))).await,
                Ok(Created::New(_))
            ));
        }
        let limited = exports.create(scope, request("k6")).await;
        assert!(
            matches!(limited, Err(CreateError::RateLimited { retry_after_s }) if retry_after_s > 3_000),
            "{limited:?}"
        );

        let _running = start(&host, &model, base, None);
        let id = job.new.id;
        for _ in 0..400 {
            let all = model.export_jobs().list(PROJECT, 10).await.unwrap();
            if all.iter().all(|j| j.file.is_some()) {
                break;
            }
            database.clock.sleep(Duration::from_secs(5)).await;
        }
        let job = row(&model, id).await;
        check(&host.env, &job, "a finished export").await;
        let Download::File(download) = exports.open(scope, id).await.unwrap() else {
            panic!("a file on this node's disk")
        };
        assert_eq!(download.len, job.bytes);
        let bytes = download
            .read(0, usize::try_from(download.len).unwrap())
            .await
            .unwrap();
        assert_eq!(sha256(&bytes), download.checksum);
        then(model, host.env.clone(), id, host.env.clock.wall_now()).await;
    });
}

/// A file written to the disk before the deployment had an object store still downloads from the disk.
#[test]
fn a_file_on_the_disk_downloads_from_the_disk_once_an_object_store_is_configured() {
    crash_and_resume_then(|model, env, id, _| async move {
        let store = SimObjectStore::new(
            Arc::clone(&env.clock),
            env.rng.fork("objects"),
            ObjectFaults::default(),
        );
        let exports = Exports::new(env.clone(), Arc::new(model))
            .with_objects(ExportObjects::new(store.clone(), "media"));
        let scope = crucible_auth::test_support::scope(PROJECT).unwrap();
        assert!(matches!(
            exports.open(scope, id).await.unwrap(),
            Download::File(_)
        ));
        assert!(store.objects().is_empty());
    });
}

/// The objects under the project's export keys: key and bytes.
fn export_objects(
    store: &SimObjectStore,
) -> Vec<(String, crucible_sim::object_model::StoredObject)> {
    let prefix = format!("media/{PROJECT}/exports/");
    store
        .objects()
        .into_iter()
        .filter(|(key, _)| key.starts_with(&prefix))
        .collect()
}

/// Runs one export to completion on a node of a deployment with an object store (prefix `media`), then `then`
/// with the model, the store, the node's env, another node's env, the export and now.
fn with_objects_then<F, Fut>(seed: u64, gzip: bool, then: F)
where
    F: FnOnce(PgModel, Arc<SimObjectStore>, Env, Env, ExportJob, Timestamp) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    crucible_sim::run(seed, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let store = SimObjectStore::new(
            database.clock.clone(),
            sim.rng("objects"),
            ObjectFaults::default(),
        );
        let objects = ExportObjects::new(store.clone(), "media");
        let host = sim.host("node", &sim.disk("node"), 0);
        let now = host.env.clock.wall_now();
        let base = now.saturating_add_micros(-600_000_000);
        let id = ExportJobId::from_bytes([0x0b; 16]);
        let new = NewExport {
            id,
            project_id: PROJECT,
            format: ExportFormat::CrucibleJsonl,
            selector: Selector {
                window: TimeWindow::new(base, now).unwrap(),
                trace_ids: Vec::new(),
                user_id: None,
                session_id: None,
            }
            .to_text(),
            options: Options {
                gzip,
                keep_truncated: false,
            }
            .to_text(),
            dedup_key: id.to_string(),
            created_by: "user:test".to_owned(),
            created_at: now,
            as_of: now,
        };
        model.export_jobs().insert(&new).await.unwrap();
        let _running = start(&host, &model, base, Some(&objects));
        let mut done = None;
        for _ in 0..200 {
            let job = row(&model, id).await;
            if job.file.is_some() || job.error.is_some() {
                done = Some(job);
                break;
            }
            database.clock.sleep(Duration::from_secs(5)).await;
        }
        let job = done.expect("the export never finished");
        assert_eq!(job.error, None);
        let other = sim.host("node-2", &sim.disk("node-2"), 0);
        let now = host.env.clock.wall_now();
        then(model, store, host.env.clone(), other.env.clone(), job, now).await;
    });
}

/// With an object store the file goes to the store under the project's key prefix and nothing to the node's
/// disk. Any node, not only the one that wrote it, answers the download with a presigned link that lives at most
/// 5 minutes and saves the file as an attachment; a node without the store says which setting it needs.
#[test]
fn with_an_object_store_the_file_is_an_object_any_node_links_to() {
    with_objects_then(11, false, |model, store, env, other, job, now| async move {
        assert!(files(&env).await.is_empty(), "a file on the node's disk");
        let id = job.new.id;
        let objects = export_objects(&store);
        let [(key, object)] = objects.as_slice() else {
            panic!("one object, not {objects:?}")
        };
        assert_eq!(
            key,
            &format!("media/{PROJECT}/exports/{id}/1/crucible-export-{id}.jsonl")
        );
        assert!(complete(&object.bytes), "the object holds the whole export");
        assert_eq!(
            job.checksum.as_deref(),
            Some(sha256(&object.bytes).as_str())
        );
        assert_eq!(job.bytes, object.bytes.len() as u64);
        assert_eq!(object.content_type, "application/x-ndjson");
        assert!(store.open_uploads().is_empty());

        // Another node: its own disk, the same control plane and store.
        let exports = Exports::new(other.clone(), Arc::new(model.clone()))
            .with_objects(ExportObjects::new(store.clone(), "media"));
        let scope = crucible_auth::test_support::scope(PROJECT).unwrap();
        let Download::Link(link) = exports.open(scope, id).await.unwrap() else {
            panic!("a link to the object")
        };
        assert!(link.expires_at <= now.saturating_add_micros(300_000_000));
        assert!(DOWNLOAD_TTL <= Duration::from_secs(300));
        let (served, disposition) = store.download(&link.url).unwrap();
        assert_eq!(disposition, Disposition::Attachment);
        assert_eq!(served.bytes, object.bytes);

        // Without the store the link cannot be made, and the refusal names the settings.
        let bare = Exports::new(other, Arc::new(model));
        let refused = bare.open(scope, id).await.unwrap_err();
        assert!(
            matches!(refused, DownloadError::NoObjectStore),
            "{refused:?}"
        );
        assert!(refused.to_string().contains("object_store.endpoint"));
    });
}

/// The sweep removes an export's object once its file expired (7 days), and objects of attempts the row does not
/// name, and keeps the file of an export that is still downloadable.
#[test]
fn the_sweep_removes_expired_export_objects() {
    with_objects_then(12, true, |model, store, env, _, job, _| async move {
        let id = job.new.id;
        let [(key, _)] = export_objects(&store).try_into().unwrap();
        assert_eq!(
            key,
            format!("media/{PROJECT}/exports/{id}/1/crucible-export-{id}.jsonl.gz")
        );
        // What an attempt whose claim ran out after its upload leaves.
        let stray = format!("media/{PROJECT}/exports/{id}/9/crucible-export-{id}.jsonl.gz");
        store.plant(
            &stray,
            bytes::Bytes::from_static(b"stray"),
            env.clock.wall_now(),
        );
        env.clock.sleep(SWEEP_EVERY + Duration::from_secs(60)).await;
        let left: Vec<String> = export_objects(&store).into_iter().map(|(k, _)| k).collect();
        assert_eq!(left, vec![key], "the stray goes and the file stays");

        // A sweep before the file expires keeps it; one after removes it.
        let objects = ExportObjects::new(store.clone(), "media");
        let expires_at = row(&model, id).await.expires_at.unwrap();
        let before = expires_at.saturating_add_micros(-1);
        assert_eq!(sweep_objects(&model, &objects, before).await.unwrap(), 0);
        assert_eq!(export_objects(&store).len(), 1);
        assert_eq!(ExportState::of(&job, expires_at), ExportState::Expired);
        assert_eq!(
            sweep_objects(&model, &objects, expires_at).await.unwrap(),
            1
        );
        assert!(
            export_objects(&store).is_empty(),
            "the expired file is gone"
        );
        assert!(
            expires_at
                <= job.new.created_at.saturating_add_micros(
                    i64::try_from((EXPORT_TTL + EXPORT_LEASE).as_micros()).unwrap()
                )
        );
    });
}

/// A deletion request expires the exports it predates and removes their objects once the expiry is recorded.
#[test]
fn a_deletion_removes_the_objects_of_the_exports_it_predates() {
    with_objects_then(13, false, |model, store, env, _, job, now| async move {
        let objects = ExportObjects::new(store.clone(), "media");
        let purged = purge(&model, env.disk.as_ref(), PROJECT, now, now)
            .await
            .unwrap();
        assert_eq!(purged.expired.len(), 1);
        assert_eq!(purged.files_removed, 0, "nothing on the node's disk");
        assert_eq!(
            export_objects(&store).len(),
            1,
            "the purge never calls the store"
        );
        assert_eq!(objects.remove(&purged.expired).await.unwrap(), 1);
        assert!(export_objects(&store).is_empty());
        let job = row(&model, job.new.id).await;
        assert_eq!(ExportState::of(&job, now), ExportState::Expired);
    });
}

/// Under a store that throttles a fifth of its calls, an export of several parts is retried until it records one
/// whole object; no partial object ever appears, and once the store recovers the sweep leaves only the recorded one.
/// An upload whose abandoning was throttled stays open, invisible, for the bucket's lifecycle rule.
#[test]
fn a_throttled_store_ends_with_one_whole_object() {
    crucible_sim::run(17, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let store = SimObjectStore::new(
            database.clock.clone(),
            sim.rng("objects"),
            ObjectFaults {
                throttle_ppm: 200_000,
                ..ObjectFaults::default()
            },
        );
        let objects = ExportObjects::new(store.clone(), "media");
        let host = sim.host("node", &sim.disk("node"), 0);
        let now = host.env.clock.wall_now();
        let base = now.saturating_add_micros(-600_000_000);
        let id = ExportJobId::from_bytes([0x0c; 16]);
        let new = NewExport {
            id,
            project_id: PROJECT,
            format: ExportFormat::CrucibleJsonl,
            selector: Selector {
                window: TimeWindow::new(base, now).unwrap(),
                trace_ids: Vec::new(),
                user_id: None,
                session_id: None,
            }
            .to_text(),
            options: Options::default().to_text(),
            dedup_key: id.to_string(),
            created_by: "user:test".to_owned(),
            created_at: now,
            as_of: now,
        };
        model.export_jobs().insert(&new).await.unwrap();
        let spans = Spans {
            base,
            name_len: 4_000,
        };
        let _running = start_over(&host, &model, spans, Some(&objects));
        let mut done = None;
        for _ in 0..2_000 {
            let job = row(&model, id).await;
            if job.file.is_some() || job.error.is_some() {
                done = Some(job);
                break;
            }
            for (key, object) in export_objects(&store) {
                assert!(complete(&object.bytes), "{key} is a partial object");
            }
            database.clock.sleep(Duration::from_secs(5)).await;
        }
        let job = done.expect("the export never finished");
        assert_eq!(job.error, None);
        assert!(
            job.attempts > 1,
            "the store's faults never made an attempt fail"
        );
        assert!(
            job.bytes > PART_BYTES as u64,
            "{} bytes is not several parts",
            job.bytes
        );
        let recorded = job
            .file
            .as_deref()
            .unwrap()
            .strip_prefix("object:")
            .unwrap()
            .to_owned();

        store.set_faults(ObjectFaults::default());
        host.env
            .clock
            .sleep(SWEEP_EVERY + Duration::from_secs(60))
            .await;
        let left = export_objects(&store);
        let keys: Vec<&str> = left.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, [recorded.as_str()]);
        assert!(complete(&left[0].1.bytes));
        assert_eq!(
            job.checksum.as_deref(),
            Some(sha256(&left[0].1.bytes).as_str())
        );
        assert!(store.open_uploads().iter().all(|key| *key != recorded));
    });
}

/// An object store that counts the listings asked of it, over the simulated one.
#[derive(Debug)]
struct CountedListings {
    inner: Arc<SimObjectStore>,
    lists: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl ObjectStore for CountedListings {
    fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &[u8; 32],
        len: u64,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        self.inner.presign_put(key, content_type, sha256, len, ttl)
    }

    fn presign_get(
        &self,
        key: &str,
        disposition: Disposition,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        self.inner.presign_get(key, disposition, ttl)
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, ObjectError> {
        self.inner.head(key).await
    }

    async fn put_small(
        &self,
        key: &str,
        content_type: &str,
        bytes: bytes::Bytes,
    ) -> Result<(), ObjectError> {
        self.inner.put_small(key, content_type, bytes).await
    }

    async fn start_upload(&self, key: &str, content_type: &str) -> Result<String, ObjectError> {
        self.inner.start_upload(key, content_type).await
    }

    async fn upload_part(
        &self,
        key: &str,
        upload_id: &str,
        number: u16,
        bytes: bytes::Bytes,
    ) -> Result<String, ObjectError> {
        self.inner.upload_part(key, upload_id, number, bytes).await
    }

    async fn finish_upload(
        &self,
        key: &str,
        upload_id: &str,
        etags: &[String],
    ) -> Result<(), ObjectError> {
        self.inner.finish_upload(key, upload_id, etags).await
    }

    async fn abort_upload(&self, key: &str, upload_id: &str) -> Result<(), ObjectError> {
        self.inner.abort_upload(key, upload_id).await
    }

    async fn get_stream(&self, key: &str) -> Result<ByteStream, ObjectError> {
        self.inner.get_stream(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        self.inner.delete(key).await
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), ObjectError> {
        self.inner.delete_batch(keys).await
    }

    async fn list(&self, prefix: &str, after: Option<&str>) -> Result<ObjectPage, ObjectError> {
        self.lists
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.list(prefix, after).await
    }
}

/// With an object store, one node at a time sweeps the export objects: two nodes list the project's export keys
/// as often as one node would, not once each.
#[test]
fn one_node_at_a_time_sweeps_the_export_objects() {
    crucible_sim::run(19, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        model.projects().insert(&project()).await.unwrap();
        let counted = Arc::new(CountedListings {
            inner: SimObjectStore::new(
                database.clock.clone(),
                sim.rng("objects"),
                ObjectFaults::default(),
            ),
            lists: std::sync::atomic::AtomicU64::new(0),
        });
        let objects = ExportObjects::new(counted.clone(), "media");
        let mut running = Vec::new();
        for name in ["node-1", "node-2"] {
            let host = sim.host(name, &sim.disk(name), 0);
            let mut registry = Registry::new();
            registry.register(sweep_job());
            registry.register(object_sweep_job(objects.clone()));
            let store: Arc<dyn ControlStore> = Arc::new(model.clone());
            running.push(registry.start(&host.env, Some(store), name));
        }
        // Three sweep periods with their largest jitter, and some room: one node sweeps four times in that span.
        database
            .clock
            .sleep(3 * (SWEEP_EVERY + SWEEP_EVERY / 10) + Duration::from_secs(30))
            .await;
        let lists = counted.lists.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            (1..=4).contains(&lists),
            "two nodes listed the export keys {lists} times"
        );
        drop(running);
    });
}
