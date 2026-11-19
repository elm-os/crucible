//! The media simulation (contract 14): the upload flow and the `media-gc` job of `crucible-media` over
//! [`PgModel`] and [`SimObjectStore`], on the virtual clock.
//!
//! - **Clients** ask for uploads of bytes from a small pool, so the same bytes come back and are deduplicated.
//!   Of the uploads they are handed, some arrive and are confirmed, some arrive and are never confirmed, some
//!   never arrive, and with a lax store some arrive with other bytes. They also read media back.
//! - **Deletion requests and retention** remove uses: a request deletes the uses of a trace, as the deletion
//!   executor does with each page, and one project keeps payloads for a day.
//! - **Collectors**: two nodes run the job registry with the collector under the `media_gc` lease, with the
//!   collector's horizons shortened; during the faulty phase they crash and come back.
//! - **Faults** during the faulty phase: Postgres unavailability, commits of unknown outcome, spurious
//!   serialization failures, latency; a throttling object store with an eventually consistent listing.
//!
//! The oracle (`docs/testing/media-simulation.md`):
//!
//! - **M1 no lost bytes.** After every client step, every uploaded object the clients know of is stored, with
//!   its length (and its SHA-256 when the store keeps checksums); and a download of an uploaded object, read
//!   from the presigned URL at once, finds the bytes.
//! - **M2 no garbage.** Once the run has been quiet for longer than the collector's horizons, no pending object
//!   is older than the pending horizon, every uploaded object has a use, and every stored object has a row.
//! - **M3 retention.** By then no use of the one-day project is older than its payload cut-off.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use crucible_control::{ControlStore, JsonDoc, MediaState, Project, Repos as _};
use crucible_env::{Clock, ClockExt as _, Env, Instant, ObjectStore, Rng};
use crucible_jobs::Registry;
use crucible_media::gc::{GcConfig, Mutant};
use crucible_media::rules::{media_id_of, object_key};
use crucible_media::{Created, Media, MediaConfig, NewUpload};
use crucible_records::{MediaObjectId, ProjectId, SpanId, Timestamp, TraceId, Uuid};
use crucible_schema::retention_cutoff;
use sha2::{Digest, Sha256};

use crate::object_model::{ObjectFaults, SimObjectStore};
use crate::pg_model::{PgFaults, PgModel};
use crate::{Sim, run};

/// The key prefix of the simulated deployment.
const PREFIX: &str = "media";
/// How many distinct byte strings the clients upload.
const BLOBS: usize = 6;
/// How many traces the clients use.
const TRACES: u8 = 8;

/// What a run varies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// Store faults during the faulty phase.
    pub faults: bool,
    /// Collector nodes crash during the faulty phase.
    pub crashes: bool,
    /// The store does not check uploads against their signed length and checksum.
    pub lax: bool,
    /// How long clients act.
    pub faulty_for: Duration,
    /// A seeded defect of the collector.
    pub mutant: Option<Mutant>,
}

impl Options {
    /// Faults and crashes on, a strict store, an hour of clients.
    pub const fn realistic() -> Self {
        Self {
            faults: true,
            crashes: true,
            lax: false,
            faulty_for: Duration::from_secs(3_600),
            mutant: None,
        }
    }
}

/// What a run did.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Stats {
    /// Upload requests that answered.
    pub creates: u64,
    /// Requests whose bytes the project already held.
    pub deduplicated: u64,
    /// Uploads that arrived.
    pub uploads: u64,
    /// Uploads that never arrived.
    pub never_uploaded: u64,
    /// Confirms that marked an object uploaded.
    pub confirmed: u64,
    /// Confirms refused because the upload differed from its declaration.
    pub mismatches: u64,
    /// Downloads checked.
    pub downloads: u64,
    /// Deletion requests that removed uses.
    pub deletions: u64,
    /// Collector crashes.
    pub crashes: u64,
}

/// The outcome of one run.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// The seed.
    pub seed: u64,
    /// What happened.
    pub stats: Stats,
    /// Every invariant violation, in the order found.
    pub violations: Vec<String>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn below(rng: &dyn Rng, n: u64) -> u64 {
    rng.next_u64() % n.max(1)
}

fn millis(rng: &dyn Rng, low: u64, high: u64) -> Duration {
    Duration::from_millis(low + below(rng, high - low + 1))
}

/// The collector's horizons in the simulation: minutes instead of days.
fn gc_config(mutant: Option<Mutant>) -> GcConfig {
    let mut config = GcConfig::default();
    config.every = Duration::from_secs(60);
    config.pending_ttl = Duration::from_secs(600);
    config.scan_every = Duration::from_secs(3_600);
    config.orphan_age = Duration::from_secs(1_200);
    match mutant {
        Some(mutant) => config.with_mutant(mutant),
        None => config,
    }
}

fn pg_faults(on: bool) -> PgFaults {
    if on {
        PgFaults {
            unavailable_ppm: 10_000,
            commit_unknown_ppm: 10_000,
            spurious_serialization_ppm: 10_000,
            latency_micros: (100, 20_000),
            ..PgFaults::default()
        }
    } else {
        PgFaults {
            latency_micros: (100, 2_000),
            ..PgFaults::default()
        }
    }
}

fn object_faults(on: bool, lax: bool) -> ObjectFaults {
    ObjectFaults {
        throttle_ppm: if on { 30_000 } else { 0 },
        list_lag: if on {
            Duration::from_secs(30)
        } else {
            Duration::ZERO
        },
        lax_uploads: lax,
    }
}

fn blob(n: usize) -> Bytes {
    Bytes::from(format!("blob {n}: {}", "x".repeat(n * 7)))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn project(n: u64, payload_days: Option<u32>) -> Project {
    Project {
        id: ProjectId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n))),
        name: format!("Media {n}"),
        slug: format!("media-{n}"),
        settings: JsonDoc::empty(),
        retention_days: payload_days,
        payload_retention_days: payload_days,
        mcp_content_retention_days: None,
        config_version: 1,
        version: 1,
        deleted_at: None,
    }
}

/// What the run shares.
#[derive(Clone)]
struct World {
    pg: PgModel,
    store: Arc<SimObjectStore>,
    projects: [Project; 2],
    options: Options,
    /// Every media object the clients were answered with, by id.
    known: Arc<Mutex<BTreeMap<MediaObjectId, ProjectId>>>,
    stats: Arc<Mutex<Stats>>,
    violations: Arc<Mutex<Vec<String>>>,
}

impl World {
    fn violation(&self, message: String) {
        lock(&self.violations).push(message);
    }

    /// M1 at this instant: every uploaded object the clients know of is stored.
    async fn no_lost_bytes(&self, when: &str) {
        let stored = self.store.objects();
        let known: Vec<(MediaObjectId, ProjectId)> =
            lock(&self.known).iter().map(|(id, p)| (*id, *p)).collect();
        for (id, project_id) in known {
            // A read that a fault refuses is checked at the next step.
            let Ok(Some(row)) = self.pg.media().object(project_id, id).await else {
                continue;
            };
            if row.state != MediaState::Uploaded {
                continue;
            }
            let key = object_key(PREFIX, project_id, id);
            match stored.get(&key) {
                None => self.violation(format!(
                    "M1 {when}: media {id} is uploaded but the store has no object under {key}"
                )),
                Some(object) if object.bytes.len() as u64 != row.len => self.violation(format!(
                    "M1 {when}: media {id} declares {} bytes and the store holds {}",
                    row.len,
                    object.bytes.len()
                )),
                Some(object) if !self.options.lax && sha256(&object.bytes) != row.sha256 => {
                    self.violation(format!(
                        "M1 {when}: media {id} holds other bytes than its SHA-256"
                    ));
                }
                Some(_) => {}
            }
        }
    }
}

/// One client step.
async fn step(world: &World, media: &Media, env: &Env, rng: &dyn Rng) {
    let project = &world.projects[usize::try_from(below(rng, 2)).unwrap_or(0)];
    let trace =
        TraceId::from_bytes([u8::try_from(below(rng, u64::from(TRACES))).unwrap_or(0) + 1; 16]);
    let Ok(scope) = crucible_auth::test_support::scope(project.id) else {
        return;
    };
    match below(rng, 20) {
        // An upload.
        0..=11 => {
            let n = usize::try_from(below(rng, BLOBS as u64)).unwrap_or(0);
            let bytes = blob(n);
            let upload = NewUpload {
                content_type: "image/png".to_owned(),
                len: bytes.len() as u64,
                sha256: sha256(&bytes),
                trace_id: trace,
                span_id: Some(SpanId::from_bytes([u8::try_from(n).unwrap_or(0) + 1; 8])),
                field: "input".to_owned(),
            };
            let Ok(created) = media.create(scope, upload).await else {
                return;
            };
            lock(&world.known).insert(created.object().id, project.id);
            lock(&world.stats).creates += 1;
            let (object, link) = match created {
                Created::Existing(_) => {
                    lock(&world.stats).deduplicated += 1;
                    return;
                }
                Created::Upload(object, link) => (object, link),
            };
            let body = match below(rng, 10) {
                0..=1 => {
                    lock(&world.stats).never_uploaded += 1;
                    return;
                }
                // Other bytes of the same length, which only a lax store takes.
                2 => {
                    let mut other = bytes.to_vec();
                    if let Some(first) = other.first_mut() {
                        *first ^= 0xff;
                    }
                    Bytes::from(other)
                }
                // A shorter body, which only a lax store takes.
                3 => bytes.slice(..bytes.len() / 2),
                _ => bytes,
            };
            if world.store.upload(&link.url, "image/png", body).is_ok() {
                lock(&world.stats).uploads += 1;
            }
            if below(rng, 10) < 8 {
                match media.confirm(scope, object.id).await {
                    Ok(_) => lock(&world.stats).confirmed += 1,
                    Err(crucible_media::MediaError::Mismatch) => lock(&world.stats).mismatches += 1,
                    Err(_) => {}
                }
            }
        }
        // A confirm of an object asked for earlier.
        12..=13 => {
            let known: Vec<(MediaObjectId, ProjectId)> =
                lock(&world.known).iter().map(|(id, p)| (*id, *p)).collect();
            let pick = usize::try_from(below(rng, known.len() as u64)).unwrap_or(0);
            if let Some((id, project_id)) = known.get(pick).copied()
                && let Ok(scope) = crucible_auth::test_support::scope(project_id)
                && media.confirm(scope, id).await.is_ok()
            {
                lock(&world.stats).confirmed += 1;
            }
        }
        // A deletion request removes the uses of its traces, as the executor does with each page: a trace, or
        // every trace of the project, as a deletion by user can resolve to.
        14..=15 => {
            let now = env.clock.wall_now();
            let traces: Vec<TraceId> = if below(rng, 3) == 0 {
                (1..=TRACES).map(|n| TraceId::from_bytes([n; 16])).collect()
            } else {
                vec![trace]
            };
            if let Ok(deleted) = world
                .pg
                .media()
                .delete_trace_refs(project.id, &traces, now)
                .await
                && deleted > 0
            {
                lock(&world.stats).deletions += 1;
            }
        }
        // A read: an uploaded object's download finds its bytes.
        _ => {
            let known: Vec<(MediaObjectId, ProjectId)> =
                lock(&world.known).iter().map(|(id, p)| (*id, *p)).collect();
            let pick = usize::try_from(below(rng, known.len() as u64)).unwrap_or(0);
            let Some((id, project_id)) = known.get(pick).copied() else {
                return;
            };
            let Ok(scope) = crucible_auth::test_support::scope(project_id) else {
                return;
            };
            if let Ok((object, Some(link))) = media.get(scope, id).await {
                lock(&world.stats).downloads += 1;
                match world.store.download(&link.url) {
                    Ok((stored, _)) if stored.bytes.len() as u64 == object.len => {}
                    Ok(_) => world
                        .violation(format!("M1: the download of media {id} has another length")),
                    Err(refused) => world.violation(format!(
                        "M1: media {id} is uploaded and its download answered {}",
                        refused.0
                    )),
                }
            }
        }
    }
}

/// The clients, until the end of the faulty phase.
async fn clients(sim: &Sim, world: World, until: Instant) {
    let host = sim.host("media-client", &sim.disk("media-client"), 0);
    let rng = sim.rng("media-sim/clients");
    let control: Arc<dyn ControlStore> = Arc::new(world.pg.clone());
    let media = Media::new(
        &host.env,
        control,
        Arc::clone(&world.store) as Arc<dyn ObjectStore>,
        MediaConfig {
            prefix: PREFIX.to_owned(),
            max_bytes: 1 << 20,
        },
    );
    while host.env.clock.now() < until {
        host.env.clock.sleep(millis(rng.as_ref(), 50, 2_000)).await;
        step(&world, &media, &host.env, rng.as_ref()).await;
        world.no_lost_bytes("during the run").await;
    }
    host.stop().await;
}

/// One collector node across its boots.
async fn collector(sim: &Sim, world: World, n: usize, faulty_until: Instant, end: Instant) {
    let rng = sim.rng(&format!("media-sim/node-{n}"));
    let disk = sim.disk(&format!("media-node-{n}"));
    let mut boot = 0;
    loop {
        boot += 1;
        let name = format!("media-node-{n}#{boot}");
        let host = sim.host(&name, &disk, 0);
        let clock = Arc::clone(&host.env.clock);
        let mut registry = Registry::new();
        registry.register(crucible_media::gc::job(
            Arc::clone(&world.store) as Arc<dyn ObjectStore>,
            PREFIX.to_owned(),
            gc_config(world.options.mutant),
        ));
        let control: Arc<dyn ControlStore> = Arc::new(world.pg.clone());
        let mut running = registry.start(&host.env, Some(control), &name);
        let pause = millis(rng.as_ref(), 20_000, 300_000);
        let wake = clock.now().saturating_add(pause);
        if !world.options.crashes || wake >= faulty_until {
            clock.sleep_until(end).await;
            host.spawner.request_shutdown();
            let _ = clock.timeout(Duration::from_secs(60), running.join()).await;
            host.stop().await;
            return;
        }
        clock.sleep(pause).await;
        host.crash().await;
        lock(&world.stats).crashes += 1;
        clock.sleep(millis(rng.as_ref(), 0, 120_000)).await;
    }
}

/// M2 and M3 once the run has been quiet for longer than the collector's horizons.
async fn audit(world: &World, now: Timestamp) {
    world.no_lost_bytes("at the end").await;
    let media = world.pg.media();
    let config = gc_config(None);
    let stale_before =
        now.saturating_add_micros(-i64::try_from(config.pending_ttl.as_micros()).unwrap_or(0));
    match media.stale_pending(stale_before, 1_000).await {
        Ok(stale) if !stale.is_empty() => world.violation(format!(
            "M2: {} pending objects are older than the pending horizon, the first {}",
            stale.len(),
            stale[0].id
        )),
        Ok(_) => {}
        Err(error) => world.violation(format!("the audit could not read pending objects: {error}")),
    }
    match media.unreferenced(1_000).await {
        Ok(unused) if !unused.is_empty() => world.violation(format!(
            "M2: {} uploaded objects have no use, the first {}",
            unused.len(),
            unused[0].id
        )),
        Ok(_) => {}
        Err(error) => world.violation(format!("the audit could not read unused objects: {error}")),
    }
    let stored = world.store.objects();
    let ids: Vec<MediaObjectId> = stored
        .keys()
        .filter_map(|key| media_id_of(PREFIX, key).map(|(_, id)| id))
        .collect();
    match media.existing(&ids).await {
        Ok(existing) => {
            for id in ids.iter().filter(|id| existing.binary_search(id).is_err()) {
                world.violation(format!("M2: the stored object of media {id} has no row"));
            }
        }
        Err(error) => world.violation(format!("the audit could not read the rows: {error}")),
    }
    // M3: the one-day project keeps no use from before its payload cut-off.
    let short = &world.projects[0];
    if let Some(days) = short.payload_retention_days
        && let Ok(cutoff) = retention_cutoff(now, days)
    {
        for trace in 1..=TRACES {
            if let Ok(uses) = media
                .uses(short.id, TraceId::from_bytes([trace; 16]), None)
                .await
            {
                for usage in uses
                    .iter()
                    .filter(|u| u.reference.target_time < cutoff.start())
                {
                    world.violation(format!(
                        "M3: a use of media {} from {} outlived the payload cut-off {}",
                        usage.object.id,
                        usage.reference.target_time,
                        cutoff.start()
                    ));
                }
            }
        }
    }
}

async fn simulate(sim: Sim, options: Options) -> Outcome {
    let database = sim.host("media-postgres", &sim.disk("media-postgres"), 0);
    let clock = Arc::clone(&database.env.clock);
    let pg = PgModel::new(
        database.clock.clone(),
        sim.rng("media-sim/pg").as_ref(),
        pg_faults(false),
    );
    let store = SimObjectStore::new(
        Arc::clone(&clock) as Arc<dyn Clock>,
        sim.rng("media-sim/objects"),
        object_faults(false, options.lax),
    );
    let projects = [project(1, Some(1)), project(2, None)];
    let world = World {
        pg: pg.clone(),
        store: Arc::clone(&store),
        projects: projects.clone(),
        options,
        known: Arc::default(),
        stats: Arc::default(),
        violations: Arc::default(),
    };
    for project in &projects {
        if let Err(error) = pg.projects().insert(project).await {
            world.violation(format!("the fixture project could not be stored: {error}"));
        }
    }
    // An object that no row names, as a crash between a row's delete and its object's leaves one.
    store.plant(
        &object_key(
            PREFIX,
            projects[1].id,
            MediaObjectId::from_uuid(Uuid::from_v7_parts(1_800_000_000_500, 5)),
        ),
        Bytes::from_static(b"orphan"),
        clock.wall_now(),
    );
    pg.set_faults(pg_faults(options.faults));
    store.set_faults(object_faults(options.faults, options.lax));
    let faulty_until = clock.now().saturating_add(options.faulty_for);
    // Quiet for two days and an hour: past every horizon, and past the one-day project's cut-off of the uses
    // made at the start.
    let end = faulty_until.saturating_add(Duration::from_secs(49 * 3_600));
    let calm = async {
        clock.sleep_until(faulty_until).await;
        pg.set_faults(pg_faults(false));
        store.set_faults(object_faults(false, options.lax));
    };
    futures::join!(
        clients(&sim, world.clone(), faulty_until),
        collector(&sim, world.clone(), 0, faulty_until, end),
        collector(&sim, world.clone(), 1, faulty_until, end),
        calm,
    );
    audit(&world, clock.wall_now()).await;
    database.stop().await;
    let stats = *lock(&world.stats);
    let violations = std::mem::take(&mut *lock(&world.violations));
    Outcome {
        seed: sim.seed(),
        stats,
        violations,
    }
}

/// Runs the media simulation for one seed.
pub fn run_media_sim(seed: u64, options: Options) -> Outcome {
    run(seed, |sim| simulate(sim, options))
}
