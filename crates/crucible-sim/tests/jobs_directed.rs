//! Directed scenarios for the job runners of `crucible-jobs` over `PgModel`, on the virtual clock: what the
//! leader runner does when its window ends and on shutdown, how claimed rows are retried and parked, the
//! housekeeping job, and a node without a control store.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crucible_control::{
    ApiKey, AuthMethod, ControlStore, Email, JsonDoc, KeyScope, LEASE_TTL, LeaseName, NewJob,
    Project, Queue, Repos as _, Session, User, UserStatus,
};
use crucible_env::{Clock, Env};
use crucible_jobs::housekeeping::{self, KEY_FEED_KEEP};
use crucible_jobs::{
    BACKOFF_FIRST, Cadence, ClaimCtx, ClaimPolicy, DRAIN_LIMIT, Job, JobError, LeaderCtx, NodeCtx,
    Registry,
};
use crucible_records::{ApiKeyId, ProjectId, Timestamp, UserId, Uuid};
use crucible_sim::pg_model::{JobStatus, PgFaults, PgModel};
use crucible_sim::{Host, Sim};

const QUEUE: Queue = Queue::NotificationOutbox;

struct World {
    sim: Sim,
    database: Host,
    model: PgModel,
}

impl World {
    fn node(&self, name: &str, skew_micros: i64) -> Host {
        self.sim.host(name, &self.sim.disk(name), skew_micros)
    }

    fn store(&self) -> Option<Arc<dyn ControlStore>> {
        Some(Arc::new(self.model.clone()))
    }

    async fn sleep(&self, d: Duration) {
        self.database.clock.sleep(d).await;
    }

    fn secs(&self) -> f64 {
        self.sim.elapsed().as_secs_f64()
    }

    fn db_now(&self) -> Timestamp {
        self.database.clock.wall_now()
    }
}

fn world<T>(seed: u64, body: impl AsyncFnOnce(World) -> T) -> T {
    crucible_sim::run(seed, |sim| async move {
        let database = sim.host("postgres", &sim.disk("postgres"), 0);
        let model = PgModel::new(
            database.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        body(World {
            sim,
            database,
            model,
        })
        .await
    })
}

fn id(n: u64) -> Uuid {
    Uuid::from_v7_parts(1_800_000_000_000 + n, u128::from(n))
}

/// A leader job that counts its runs; each run takes `work`.
fn counting(runs: &Arc<AtomicU64>, every: Duration, work: Duration) -> Registry {
    let runs = Arc::clone(runs);
    let mut registry = Registry::new();
    registry.register(Job::leader(
        "count",
        LeaseName::Lifecycle,
        Cadence::every(every).with_jitter(Duration::ZERO),
        move |ctx: LeaderCtx| {
            let runs = Arc::clone(&runs);
            async move {
                ctx.env.clock.sleep(work).await;
                ctx.set_cursor("x").await?;
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        },
    ));
    registry
}

#[test]
fn a_leader_releases_its_lease_on_shutdown_and_another_node_takes_over() {
    world(1, async |w| {
        let (a, b) = (w.node("a", 0), w.node("b", 0));
        let (runs_a, runs_b) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let every = Duration::from_secs(1);
        let mut running_a = counting(&runs_a, every, Duration::ZERO).start(&a.env, w.store(), "a#1");
        w.sleep(Duration::from_secs(1)).await;
        let _running_b = counting(&runs_b, every, Duration::ZERO).start(&b.env, w.store(), "b#1");

        // Well past the TTL: the lease is renewed, never taken over.
        w.sleep(LEASE_TTL * 4).await;
        let lease = w.model.leases().get(LeaseName::Lifecycle).await.unwrap().unwrap();
        assert_eq!((lease.holder.as_str(), lease.fencing_token), ("a#1", 1));
        assert!(lease.expires_at > w.db_now());
        assert!(runs_a.load(Ordering::SeqCst) >= 60);
        assert_eq!(runs_b.load(Ordering::SeqCst), 0);

        a.spawner.request_shutdown();
        running_a.join().await;
        let lease = w.model.leases().get(LeaseName::Lifecycle).await.unwrap().unwrap();
        assert_eq!(lease.holder, "a#1");
        assert_eq!(lease.expires_at, Timestamp::from_micros(0), "released");

        // The other node asks every 5 s plus up to 1 s of jitter: it leads without waiting out a TTL.
        w.sleep(Duration::from_secs(7)).await;
        let lease = w.model.leases().get(LeaseName::Lifecycle).await.unwrap().unwrap();
        assert_eq!((lease.holder.as_str(), lease.fencing_token), ("b#1", 2));
        assert!(runs_b.load(Ordering::SeqCst) > 0);
    });
}

#[test]
fn work_stops_when_the_local_window_ends_and_resumes_under_a_new_token() {
    world(2, async |w| {
        let node = w.node("a", 0);
        let ticks = Arc::new(Mutex::new(Vec::new()));
        let mut registry = Registry::new();
        let seen = Arc::clone(&ticks);
        // One run that never ends by itself: it ticks every 100 ms for as long as it is polled.
        registry.register(Job::leader(
            "tick",
            LeaseName::Lifecycle,
            Cadence::every(Duration::from_secs(1)),
            move |ctx: LeaderCtx| {
                let seen = Arc::clone(&seen);
                async move {
                    loop {
                        let token = ctx.lease()?.fencing_token;
                        seen.lock().unwrap().push((ctx.env.clock.now().nanos(), token));
                        ctx.env.clock.sleep(Duration::from_millis(100)).await;
                    }
                }
            },
        ));
        let _running = registry.start(&node.env, w.store(), "a#1");
        // The lease is taken at 0 s and renewed at 5, 10, 15 and 20 s. From 20.5 s every call fails.
        w.sleep(Duration::from_millis(20_500)).await;
        w.model.fail_over(Duration::from_secs(60));
        w.sleep(Duration::from_secs(40)).await;
        let last = ticks.lock().unwrap().last().copied().unwrap();
        let secs = Duration::from_nanos(last.0).as_secs_f64();
        // The last renewal was asked at 20 s, so the window ends at 20 + 15 - 2 = 33 s, and so does the work.
        assert!((32.85..33.0).contains(&secs), "the last tick was at {secs} s");
        assert_eq!(last.1, 1);

        // After the outage the node takes the expired lease again and works under the next token.
        w.sleep(Duration::from_secs(40)).await;
        let last = ticks.lock().unwrap().last().copied().unwrap();
        assert_eq!(last.1, 2);
        assert!(Duration::from_nanos(last.0).as_secs_f64() > 80.0);
    });
}

#[test]
fn a_shutdown_lets_the_run_in_flight_finish_and_cuts_one_that_takes_too_long() {
    world(3, async |w| {
        // A run of 3 s is in flight at the shutdown: it ends, and then the lease is released.
        let short = w.node("short", 0);
        let runs = Arc::new(AtomicU64::new(0));
        let every = Duration::from_secs(60);
        let mut running =
            counting(&runs, every, Duration::from_secs(3)).start(&short.env, w.store(), "short#1");
        w.sleep(Duration::from_secs(1)).await;
        short.spawner.request_shutdown();
        running.join().await;
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!((3.0..3.1).contains(&w.secs()), "stopped at {} s", w.secs());
        let lease = w.model.leases().get(LeaseName::Lifecycle).await.unwrap().unwrap();
        assert_eq!(lease.expires_at, Timestamp::from_micros(0), "released");

        // A run of 30 s is cut after the drain limit; its lease is left to expire.
        let long = w.node("long", 0);
        let runs = Arc::new(AtomicU64::new(0));
        let mut running =
            counting(&runs, every, Duration::from_secs(30)).start(&long.env, w.store(), "long#1");
        w.sleep(Duration::from_secs(1)).await;
        let before = w.secs();
        long.spawner.request_shutdown();
        running.join().await;
        let waited = w.secs() - before;
        assert!((waited - DRAIN_LIMIT.as_secs_f64()).abs() < 0.01, "waited {waited} s");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let lease = w.model.leases().get(LeaseName::Lifecycle).await.unwrap().unwrap();
        assert_eq!(lease.holder, "long#1");
        assert!(lease.expires_at > w.db_now(), "not released: {lease:?}");
    });
}

/// A consumer whose handler fails every row whose payload starts with `poison`, and records every call
/// with the time `secs` gives.
fn consumer(
    calls: &Arc<Mutex<Vec<(String, u32, f64)>>>,
    secs: impl Fn() -> f64 + Send + Sync + 'static,
    cap: u32,
) -> Job {
    let calls = Arc::clone(calls);
    let secs = Arc::new(secs);
    Job::claimed(
        "outbox",
        QUEUE,
        Cadence::every(Duration::from_secs(1)),
        ClaimPolicy {
            max_attempts: cap,
            ..ClaimPolicy::default()
        },
        move |ctx: ClaimCtx| {
            let calls = Arc::clone(&calls);
            let secs = Arc::clone(&secs);
            async move {
                let payload = ctx.claim.payload.clone();
                calls
                    .lock()
                    .unwrap()
                    .push((payload.clone(), ctx.claim.attempt, secs()));
                if payload.starts_with("poison") {
                    Err(JobError::Failed("poison".to_owned()))
                } else {
                    Ok(())
                }
            }
        },
    )
}

fn only(job: Job) -> Registry {
    let mut registry = Registry::new();
    registry.register(job);
    registry
}

fn row(n: u64, payload: &str, run_at: Timestamp) -> NewJob {
    NewJob {
        id: id(n),
        dedup_key: format!("row-{n}"),
        payload: payload.to_owned(),
        run_at,
    }
}

#[test]
fn a_failing_row_backs_off_on_the_database_clock_and_is_parked_at_the_cap() {
    // The node's wall clock is 5 min ahead of, or behind, the database's: the retry times do not move.
    for skew in [0, 300_000_000, -300_000_000] {
        world(4, async |w| {
            let node = w.node("a", skew);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let clock: Arc<dyn Clock> = w.database.clock.clone();
            let secs = move || Duration::from_nanos(clock.now().nanos()).as_secs_f64();
            w.model.claims().enqueue(QUEUE, &row(1, "poison", w.db_now())).await.unwrap();
            w.model.claims().enqueue(QUEUE, &row(2, "fine", w.db_now())).await.unwrap();
            let _running = only(consumer(&calls, secs, 4)).start(&node.env, w.store(), "a#1");
            w.sleep(Duration::from_secs(600)).await;

            let calls = calls.lock().unwrap().clone();
            let fine: Vec<_> = calls.iter().filter(|call| call.0 == "fine").collect();
            assert_eq!(fine.len(), 1, "{calls:?}");
            let poison: Vec<_> = calls.iter().filter(|call| call.0 == "poison").collect();
            assert_eq!(
                poison.iter().map(|call| call.1).collect::<Vec<_>>(),
                [1, 2, 3, 4],
                "skew {skew}: {calls:?}"
            );
            // Each gap is the backoff (the base less up to a tenth) plus at most one poll interval of 1.1 s.
            for (n, pair) in poison.windows(2).enumerate() {
                let gap = pair[1].2 - pair[0].2;
                let base = BACKOFF_FIRST.as_secs_f64() * f64::from(1_u32 << n);
                assert!(
                    (base * 0.9..=base + 1.2).contains(&gap),
                    "skew {skew}: the gap after attempt {} was {gap} s",
                    n + 1
                );
            }
            let rows = w.model.job_states();
            let status = |payload: &str| {
                let row = rows.iter().find(|row| row.payload == payload).unwrap();
                (row.status, row.attempts)
            };
            assert_eq!(status("fine"), (JobStatus::Done, 1));
            assert_eq!(status("poison"), (JobStatus::Parked, 4));
        });
    }
}

#[test]
fn a_row_whose_attempts_died_without_an_answer_is_parked_without_running() {
    world(5, async |w| {
        w.model.claims().enqueue(QUEUE, &row(1, "fine", w.db_now())).await.unwrap();
        // Three claimers take the row and die: no completion, no failure.
        for attempt in 1..=3 {
            let claims = w.model.claims().claim(QUEUE, 1, Duration::from_secs(1)).await.unwrap();
            assert_eq!(claims[0].attempt, attempt);
            w.sleep(Duration::from_secs(2)).await;
        }
        let node = w.node("a", 0);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let _running = only(consumer(&calls, || 0.0, 3)).start(&node.env, w.store(), "a#1");
        w.sleep(Duration::from_secs(120)).await;
        assert!(calls.lock().unwrap().is_empty());
        let rows = w.model.job_states();
        assert_eq!((rows[0].status, rows[0].attempts), (JobStatus::Parked, 4));
    });
}

fn api_key(n: u64, project: ProjectId) -> ApiKey {
    ApiKey {
        id: ApiKeyId::from_uuid(id(100 + n)),
        project_id: project,
        public_key: format!("cpk_{n:043}"),
        secret_salt: [1; 16],
        secret_hash: [2; 32],
        lookup_tag: [3; 8],
        scopes: vec![KeyScope::Ingest],
        import: false,
        note: String::new(),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        version: 1,
    }
}

#[test]
fn housekeeping_prunes_old_feed_rows_and_expired_sessions_and_nothing_younger() {
    world(6, async |w| {
        let s = &w.model;
        let project = Project {
            id: ProjectId::from_uuid(id(1)),
            name: "Web".to_owned(),
            slug: "web".to_owned(),
            settings: JsonDoc::empty(),
            retention_days: None,
            payload_retention_days: None,
            mcp_content_retention_days: None,
            config_version: 1,
            version: 1,
            deleted_at: None,
        };
        s.projects().insert(&project).await.unwrap();
        let user = User {
            id: UserId::from_uuid(id(2)),
            email: Email::new("ada@example.com").unwrap(),
            name: "Ada".to_owned(),
            status: UserStatus::Active,
            email_verified_at: None,
        };
        s.users().insert(&user).await.unwrap();
        // Expired an hour ago, expired 5 min ago (inside the grace for clock skew), and live for a day.
        let now = w.db_now();
        let hash = |n: u8| [n; 32];
        for (n, expires_in_secs) in [(1, -3_600), (2, -300), (3, 86_400)] {
            let expires = now.saturating_add_micros(expires_in_secs * 1_000_000);
            let session = Session {
                token_hash: hash(n),
                user_id: user.id,
                csrf_token: String::new(),
                idle_expires_at: expires,
                absolute_expires_at: expires,
                auth_method: AuthMethod::Password,
                revoked_at: None,
            };
            s.sessions().insert(&session).await.unwrap();
        }
        for n in 0..3 {
            s.api_keys().insert(&api_key(n, project.id)).await.unwrap();
        }

        let node = w.node("a", 0);
        let mut registry = Registry::new();
        registry.register(housekeeping::job());
        let _running = registry.start(&node.env, w.store(), "a#1");

        // The first pass runs at once, and the second after 60 to 66 s: no feed row is 75 s old yet.
        w.sleep(KEY_FEED_KEEP - Duration::from_secs(1)).await;
        assert_eq!(s.feed_len(), 3);
        assert!(s.sessions().get(&hash(1)).await.unwrap().is_none());
        assert!(s.sessions().get(&hash(2)).await.unwrap().is_some());
        assert!(s.sessions().get(&hash(3)).await.unwrap().is_some());

        // A row written at 100 s outlives the pass after 120 s, which removes the three old ones.
        w.sleep(Duration::from_secs(26)).await;
        s.api_keys().insert(&api_key(9, project.id)).await.unwrap();
        w.sleep(Duration::from_secs(40)).await;
        assert_eq!(s.feed_len(), 1);
        let left = s.key_feed().poll(0, Duration::ZERO).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].key.id, api_key(9, project.id).id);
    });
}

#[test]
fn without_a_control_store_only_the_per_node_jobs_run() {
    world(7, async |w| {
        let node = w.node("a", 0);
        let ticks = Arc::new(AtomicU64::new(0));
        let seen = Arc::clone(&ticks);
        let mut registry = Registry::new();
        registry
            .register(housekeeping::job())
            .register(consumer(&Arc::new(Mutex::new(Vec::new())), || 0.0, 3))
            .register(Job::per_node(
                "tick",
                Cadence::every(Duration::from_secs(5)).with_jitter(Duration::ZERO),
                move |ctx: NodeCtx| {
                    assert!(ctx.control.is_none());
                    seen.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                },
            ));
        assert_eq!(registry.names(), ["housekeeping", "outbox", "tick"]);
        let env: &Env = &node.env;
        let mut running = registry.start(env, None, "a#1");
        assert_eq!(running.tasks(), 1);
        // A run at the start, then one every 5 s.
        w.sleep(Duration::from_millis(21_000)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 5);
        node.spawner.request_shutdown();
        running.join().await;
        w.sleep(Duration::from_secs(20)).await;
        assert_eq!(ticks.load(Ordering::SeqCst), 5);
    });
}
