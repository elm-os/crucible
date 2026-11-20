//! Tests of the key snapshot: verdicts and outage modes on one node, and invariant I10 across nodes in simulation
//! over `PgModel` (partition, restart, failover and out-of-order feed commits), with the overlap-window mutant.

use std::sync::atomic::AtomicBool;

use crucible_control::{JsonDoc, KeyChangeKind, Project, Repos as _};
use crucible_env::ChaChaRng;
use crucible_records::Uuid;
use crucible_sim::pg_model::{PgFaults, PgModel};

use super::*;
use crate::mint::{MintedKey, mint};

const LOOKUP: [u8; 32] = [7; 32];

fn project(n: u8) -> ProjectId {
    ProjectId::from_uuid(Uuid::from_v7_parts(1_800_000_000_000, u128::from(n)))
}

fn row(n: u8, minted: &MintedKey, scopes: &[KeyScope]) -> ApiKey {
    ApiKey {
        id: ApiKeyId::from_uuid(Uuid::from_v7_parts(1_800_000_000_001, u128::from(n))),
        project_id: project(1),
        public_key: minted.public_key.clone(),
        secret_salt: minted.salt,
        secret_hash: minted.hash,
        lookup_tag: minted.lookup_tag,
        scopes: scopes.to_vec(),
        import: false,
        note: format!("key {n}"),
        expires_at: None,
        last_used_at: None,
        revoked_at: None,
        version: 1,
    }
}

fn snapshot(keys: &[ApiKey]) -> KeySnapshot {
    KeySnapshot {
        max_seq: 10,
        keys: keys.to_vec(),
    }
}

fn valid(verdict: KeyVerdict) -> Option<KeyGrant> {
    match verdict {
        KeyVerdict::Valid(grant) => Some(grant),
        KeyVerdict::Invalid | KeyVerdict::Unavailable { .. } => None,
    }
}

/// A public key alone finds its pair's grant until the key is revoked (contract 11.3); ingest and the MCP server's
/// verifiers never take it.
#[test]
fn a_public_key_alone_finds_its_pair_until_the_key_is_revoked() {
    crate::testing::sim(1, async |env| {
        let rng = ChaChaRng::from_u64(2);
        let lookup = LookupKey::new(LOOKUP);
        let a = mint(&rng, &lookup);
        let keys = DbKeys::new(&env, lookup, OutageMode::default());
        assert!(matches!(
            keys.check_public(&a.public_key),
            KeyVerdict::Unavailable { .. }
        ));
        let mut key = row(1, &a, &[KeyScope::Write]);
        keys.load(&snapshot(core::slice::from_ref(&key)));
        let grant = valid(keys.check_public(&a.public_key)).unwrap();
        assert_eq!(grant.principal.api_key_id, key.id);
        assert!(grant.scopes.has(KeyScope::Write));
        assert_eq!(keys.check_public(&a.secret), KeyVerdict::Invalid);
        let keys = Arc::new(keys);
        assert_eq!(keys.verify_public(&a.public_key), None);
        assert_eq!(
            keys.with_scope(KeyScope::Read).verify_public(&a.public_key),
            None
        );
        key.revoked_at = Some(Timestamp::from_micros(1));
        key.version = 2;
        keys.apply_local(&key);
        assert_eq!(keys.check_public(&a.public_key), KeyVerdict::Invalid);
    });
}

#[test]
fn keys_verify_by_secret_and_by_pair_with_their_scopes() {
    crate::testing::sim(1, async |env| {
        let rng = ChaChaRng::from_u64(1);
        let lookup = LookupKey::new(LOOKUP);
        let (a, b) = (mint(&rng, &lookup), mint(&rng, &lookup));
        let keys = DbKeys::new(&env, lookup, OutageMode::default());
        // Cold: nothing is accepted.
        assert!(!keys.is_loaded());
        assert!(matches!(
            keys.check_secret(&a.secret),
            KeyVerdict::Unavailable { .. }
        ));
        let rows = [
            row(1, &a, &[KeyScope::Ingest]),
            row(2, &b, &[KeyScope::Read, KeyScope::Sql]),
        ];
        keys.load(&snapshot(&rows));
        let grant = valid(keys.check_secret(&a.secret)).unwrap();
        assert_eq!(grant.principal.api_key_id, rows[0].id);
        assert_eq!(grant.principal.scope.project_id(), project(1));
        assert!(grant.scopes.has(KeyScope::Ingest) && !grant.scopes.has(KeyScope::Read));
        let grant = valid(keys.check_pair(&b.public_key, &b.secret)).unwrap();
        assert!(grant.scopes.has(KeyScope::Sql) && !grant.scopes.has(KeyScope::Ingest));
        // A pair whose halves belong to different keys, an unknown secret and a wrong pair are refused.
        assert_eq!(
            keys.check_pair(&a.public_key, &b.secret),
            KeyVerdict::Invalid
        );
        let stranger = mint(&rng, &LookupKey::new(LOOKUP));
        assert_eq!(keys.check_secret(&stranger.secret), KeyVerdict::Invalid);
        assert_eq!(
            keys.check_pair(&stranger.public_key, &stranger.secret),
            KeyVerdict::Invalid
        );
        // Ingest and the compat routes take only keys with the ingest scope, as a secret or as a pair.
        assert_eq!(
            keys.verify_pair(&a.public_key, &a.secret)
                .map(|p| p.api_key_id),
            Ok(rows[0].id)
        );
        assert_eq!(
            keys.verify_pair(&b.public_key, &a.secret),
            Err(crate::PairError::Mismatch)
        );
        assert_eq!(
            keys.verify_pair(&b.public_key, &b.secret),
            Err(crate::PairError::Secret)
        );
        let basic = |public: &str, secret: &str| {
            use base64::Engine as _;
            let pair = format!("{public}:{secret}");
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            )
        };
        assert!(crate::authenticate(&keys, &basic(&a.public_key, &a.secret)).is_ok());
        assert_eq!(
            crate::authenticate(&keys, &format!("Bearer {}", stranger.secret)),
            Err(crate::Refusal::UnknownSecret)
        );
        assert_eq!(
            keys.verify(&a.secret).map(|p| p.api_key_id),
            Some(rows[0].id)
        );
        assert_eq!(keys.verify(&b.secret), None);
        // The MCP server's reads take keys with the read scope instead, by secret and by pair.
        let keys = Arc::new(keys);
        let reads = keys.with_scope(KeyScope::Read);
        assert_eq!(
            reads.verify(&b.secret).map(|p| p.api_key_id),
            Some(rows[1].id)
        );
        assert_eq!(reads.verify(&a.secret), None);
        assert_eq!(
            reads
                .verify_pair(&b.public_key, &b.secret)
                .map(|p| p.api_key_id),
            Ok(rows[1].id)
        );
        assert_eq!(
            crate::authenticate(&reads, &basic(&a.public_key, &a.secret)),
            Err(crate::Refusal::UnknownSecret)
        );
    });
}

#[test]
fn versions_decide_and_expiry_and_revocation_apply_locally() {
    crate::testing::sim(2, async |env| {
        let rng = ChaChaRng::from_u64(2);
        let lookup = LookupKey::new(LOOKUP);
        let a = mint(&rng, &lookup);
        let keys = DbKeys::new(&env, lookup, OutageMode::default());
        let key = row(1, &a, &[KeyScope::Ingest]);
        keys.load(&snapshot(std::slice::from_ref(&key)));

        let mut revoked = key.clone();
        revoked.revoked_at = Some(env.clock.wall_now());
        revoked.version = 3;
        keys.apply_local(&revoked);
        assert_eq!(keys.check_secret(&a.secret), KeyVerdict::Invalid);
        // A feed row of an older version never brings the key back.
        let mut stale = key.clone();
        stale.version = 2;
        keys.apply(&[KeyChange {
            seq: 11,
            change: KeyChangeKind::Changed,
            key_version: 2,
            at: env.clock.wall_now(),
            key: stale,
        }]);
        assert_eq!(keys.check_secret(&a.secret), KeyVerdict::Invalid);
        assert_eq!(keys.max_seq(), 11);

        // Expiry is checked against the clock, also when the snapshot is old.
        let b = mint(&rng, &LookupKey::new(LOOKUP));
        let mut expiring = row(2, &b, &[KeyScope::Ingest]);
        expiring.expires_at = Some(env.clock.wall_now().saturating_add_micros(2_000_000));
        keys.apply_local(&expiring);
        assert!(valid(keys.check_secret(&b.secret)).is_some());
        env.clock.sleep(Duration::from_secs(3)).await;
        assert_eq!(keys.check_secret(&b.secret), KeyVerdict::Invalid);
    });
}

#[test]
fn outage_modes() {
    crate::testing::sim(3, async |env| {
        let rng = ChaChaRng::from_u64(3);
        let lookup = LookupKey::new(LOOKUP);
        let (a, b) = (mint(&rng, &lookup), mint(&rng, &lookup));
        let rows = [row(1, &a, &[KeyScope::Ingest])];
        let open = DbKeys::new(&env, lookup.clone(), OutageMode::default());
        let capped = DbKeys::new(
            &env,
            lookup.clone(),
            OutageMode::FailOpen {
                max: Some(Duration::from_secs(60)),
            },
        );
        let closed = DbKeys::new(&env, lookup, OutageMode::FailClosed);
        for keys in [&open, &capped, &closed] {
            keys.load(&snapshot(&rows));
        }
        // Healthy: an unknown key is simply invalid.
        assert_eq!(open.check_secret(&b.secret), KeyVerdict::Invalid);
        env.clock.sleep(OUTAGE_AFTER).await;
        // Fail-open: known keys keep working, unknown ones are 503, also through `authenticate`.
        assert_eq!(
            crate::authenticate(&open, &format!("Bearer {}", b.secret)),
            Err(crate::Refusal::Unavailable)
        );
        assert_eq!(open.retry_after(&b.secret), Some(RETRY_AFTER));
        assert_eq!(open.retry_after(&a.secret), None);
        assert!(valid(open.check_secret(&a.secret)).is_some());
        assert_eq!(
            open.check_secret(&b.secret),
            KeyVerdict::Unavailable {
                retry_after: RETRY_AFTER
            }
        );
        // Fail-closed: every secret is 503.
        assert!(matches!(
            closed.check_secret(&a.secret),
            KeyVerdict::Unavailable { .. }
        ));
        // The cap ends fail-open.
        assert!(valid(capped.check_secret(&a.secret)).is_some());
        env.clock.sleep(Duration::from_secs(60)).await;
        assert!(matches!(
            capped.check_secret(&a.secret),
            KeyVerdict::Unavailable { .. }
        ));
        assert!(valid(open.check_secret(&a.secret)).is_some());
        // A successful poll ends the outage.
        closed.apply(&[]);
        assert!(valid(closed.check_secret(&a.secret)).is_some());
        assert_eq!(closed.age(), Some(Duration::ZERO));
    });
}

#[test]
fn uses_are_written_in_batches() {
    crate::testing::sim(4, async |env| {
        let rng = ChaChaRng::from_u64(4);
        let lookup = LookupKey::new(LOOKUP);
        let a = mint(&rng, &lookup);
        let keys = DbKeys::new(&env, lookup, OutageMode::default());
        let key = row(1, &a, &[KeyScope::Ingest]);
        keys.load(&snapshot(std::slice::from_ref(&key)));
        assert!(keys.take_uses().is_empty());
        let _ = keys.check_secret(&a.secret);
        let used = env.clock.wall_now();
        // A reload keeps the use time.
        keys.load(&snapshot(std::slice::from_ref(&key)));
        assert_eq!(keys.take_uses(), [(key.id, used)]);
        assert!(keys.take_uses().is_empty());
    });
}

/// A node's connection to Postgres, which the scenario can cut.
#[derive(Debug)]
struct Link {
    model: PgModel,
    cut: AtomicBool,
}

impl Link {
    fn set_cut(&self, cut: bool) {
        self.cut.store(cut, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), TxError> {
        if self.cut.load(Ordering::SeqCst) {
            Err(TxError::Unavailable("partitioned".to_owned()))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl KeyFeed for Link {
    async fn poll(&self, after_seq: u64, overlap: Duration) -> Result<Vec<KeyChange>, TxError> {
        self.check()?;
        self.model.key_feed().poll(after_seq, overlap).await
    }

    async fn snapshot(&self) -> Result<KeySnapshot, TxError> {
        self.check()?;
        self.model.key_feed().snapshot().await
    }

    async fn prune(&self, older_than: Duration) -> Result<u64, TxError> {
        self.check()?;
        self.model.key_feed().prune(older_than).await
    }
}

/// A node: its keys and its link.
struct Node {
    keys: Arc<DbKeys>,
    link: Arc<Link>,
    feed: crucible_env::JoinHandle<()>,
}

fn start(env: &Env, model: &PgModel, overlap: Duration, cut: bool) -> Node {
    let keys = Arc::new(DbKeys::new(
        env,
        LookupKey::new(LOOKUP),
        OutageMode::default(),
    ));
    let link = Arc::new(Link {
        model: model.clone(),
        cut: AtomicBool::new(cut),
    });
    let feed = env.spawn(
        "key-feed",
        feed_loop(
            env.clone(),
            Arc::clone(&keys),
            Arc::clone(&link) as Arc<dyn KeyFeed>,
            overlap,
        ),
    );
    Node { keys, link, feed }
}

/// Writes `key` with its version bumped, retrying until it is known to be applied. Answers the stored row.
async fn change(
    model: &PgModel,
    key: &ApiKey,
    kind: KeyChangeKind,
    edit: impl Fn(&mut ApiKey),
) -> ApiKey {
    loop {
        let current = match model.api_keys().get(key.id).await {
            Ok(Some(current)) => current,
            _ => continue,
        };
        let mut next = current.clone();
        edit(&mut next);
        next.version = current.version + 1;
        match model
            .api_keys()
            .update_if_version(&next, current.version, kind)
            .await
        {
            Ok(true) => return next,
            // Lost a race, or the outcome is unknown: read back and try again; an applied change is seen then.
            Ok(false) | Err(_) => {
                if let Ok(Some(stored)) = model.api_keys().get(key.id).await
                    && stored.version == next.version
                    && stored == next
                {
                    return stored;
                }
            }
        }
    }
}

/// Runs the I10 scenario under `seed`; answers the first violation.
fn scenario(seed: u64, overlap: Duration) -> Result<(), String> {
    crucible_sim::run(seed, |sim| async move {
        let pg = sim.host("pg", &sim.disk("pg"), 0);
        let faults = PgFaults {
            unavailable_ppm: 20_000,
            slow_commit_ppm: 300_000,
            slow_commit_micros: (0, 2_000_000),
            latency_micros: (0, 30_000),
            ..PgFaults::default()
        };
        let model = PgModel::new(
            pg.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        let rng = sim.rng("keys");
        let lookup = LookupKey::new(LOOKUP);
        let setup = async {
            model
                .projects()
                .insert(&Project {
                    id: project(1),
                    name: "P".to_owned(),
                    slug: "p".to_owned(),
                    settings: JsonDoc::empty(),
                    retention_days: None,
                    payload_retention_days: None,
                    mcp_content_retention_days: None,
                    config_version: 1,
                    version: 1,
                    deleted_at: None,
                })
                .await?;
            let mut rows = Vec::new();
            for n in 1..=6 {
                let minted = mint(rng.as_ref(), &lookup);
                let key = row(n, &minted, &[KeyScope::Ingest]);
                model.api_keys().insert(&key).await?;
                rows.push((key, minted));
            }
            Ok::<_, TxError>(rows)
        };
        let rows = setup.await.map_err(|error| format!("setup: {error}"))?;
        model.set_faults(faults);
        // a revokes; b restarts after the revocation; c is partitioned; d stays connected throughout.
        let hosts: Vec<_> = ["a", "b", "c", "d"]
            .iter()
            .enumerate()
            .map(|(i, name)| sim.host(name, &sim.disk(name), 250_000 * i as i64))
            .collect();
        let mut nodes: Vec<Node> = hosts
            .iter()
            .map(|host| start(&host.env, &model, overlap, false))
            .collect();
        // Churn: other keys change all the time, so feed rows commit around the revocation out of order.
        let churn_model = model.clone();
        let churn_clock = Arc::clone(&pg.env.clock);
        let churn_rows: Vec<ApiKey> = rows[1..].iter().map(|(key, _)| key.clone()).collect();
        let churn = pg.env.spawn("churn", async move {
            let mut n = 0usize;
            loop {
                let key = &churn_rows[n % churn_rows.len()];
                change(&churn_model, key, KeyChangeKind::Changed, |k| {
                    k.note = format!("n{n}")
                })
                .await;
                n += 1;
                churn_clock.sleep(Duration::from_millis(100)).await;
            }
        });
        let clock = pg.clock.clone();
        let sleep = |d| {
            let clock = clock.clone();
            async move { clock.sleep(d).await }
        };
        sleep(Duration::from_secs(3)).await;
        let (target, secret) = (&rows[0].0, rows[0].1.secret.clone());
        for (i, node) in nodes.iter().enumerate() {
            if valid(node.keys.check_secret(&secret)).is_none() {
                return Err(format!(
                    "node {i} does not accept the key before the revocation"
                ));
            }
        }
        // A failover: every node is cut off for a while; fail-open keeps the key working.
        model.fail_over(Duration::from_millis(1_500));
        sleep(Duration::from_secs(2)).await;
        // Node c is partitioned before the revocation.
        nodes[2].link.set_cut(true);
        sleep(Duration::from_secs(1)).await;

        let revoked = change(&model, target, KeyChangeKind::Revoked, |k| {
            k.revoked_at = Some(Timestamp::from_micros(1_800_000_000_000_000));
        })
        .await;
        let committed = clock.now();
        nodes[0].keys.apply_local(&revoked);
        if nodes[0].keys.check_secret(&secret) != KeyVerdict::Invalid {
            return Err("the revoking node still accepts the key after its response".to_owned());
        }
        // Node b restarts one second later.
        sleep(Duration::from_secs(1)).await;
        nodes[1].feed.abort();
        nodes[1] = start(&hosts[1].env, &model, overlap, false);

        let healed_at = committed.saturating_add(Duration::from_secs(8));
        let mut healed = false;
        loop {
            let now = clock.now();
            let since = now.saturating_duration_since(committed);
            if !healed && now >= healed_at {
                nodes[2].link.set_cut(false);
                healed = true;
            }
            for (i, node) in nodes.iter().enumerate() {
                let accepts = valid(node.keys.check_secret(&secret)).is_some();
                let partitioned = i == 2 && !healed;
                let bound = if i == 2 {
                    healed_at.saturating_add(OUTAGE_AFTER)
                } else {
                    committed.saturating_add(OUTAGE_AFTER)
                };
                if accepts && !partitioned && now >= bound {
                    return Err(format!(
                        "node {i} accepts the revoked key {} ms after the revocation",
                        since.as_millis()
                    ));
                }
                if accepts && i == 1 && now > committed.saturating_add(Duration::from_secs(1)) {
                    return Err("the restarted node accepted the revoked key".to_owned());
                }
            }
            if since >= Duration::from_secs(16) {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
        // A node that starts without reaching Postgres accepts nothing.
        let cold = start(&hosts[0].env, &model, overlap, true);
        sleep(Duration::from_secs(3)).await;
        if cold.keys.is_loaded() || valid(cold.keys.check_secret(&rows[1].1.secret)).is_some() {
            return Err("a cold node without Postgres accepted a key".to_owned());
        }
        churn.abort();
        for node in nodes.iter().chain([&cold]) {
            node.feed.abort();
        }
        Ok(())
    })
}

#[test]
fn i10_holds_under_partition_restart_failover_and_out_of_order_commits() {
    for seed in 0..24 {
        if let Err(violation) = scenario(seed, OVERLAP) {
            panic!("seed {seed}: {violation}");
        }
    }
}

#[test]
fn the_mutant_without_an_overlap_window_is_caught() {
    let caught = (0..24).find(|seed| scenario(*seed, Duration::ZERO).is_err());
    assert!(
        caught.is_some(),
        "no seed caught the missing overlap window"
    );
}

/// The feed and the `last_used_at` writer stop as soon as the node is asked to stop, whatever they were waiting for:
/// a stop must not wait out its grace period for them.
#[test]
fn the_feed_and_the_use_writer_stop_at_the_shutdown_signal() {
    crucible_sim::run(5, |sim| async move {
        let host = sim.host("node", &sim.disk("node"), 0);
        let env = host.env.clone();
        let model = PgModel::new(
            host.clock.clone(),
            sim.rng("pg").as_ref(),
            PgFaults::default(),
        );
        let store: Arc<dyn ControlStore> = Arc::new(model);
        let keys = Arc::new(DbKeys::new(
            &env,
            LookupKey::new(LOOKUP),
            OutageMode::default(),
        ));
        let mut feed = env.spawn(
            "key-feed",
            run_feed(
                env.clone(),
                Arc::clone(&keys),
                Arc::new(StoreFeed(Arc::clone(&store))),
            ),
        );
        let mut uses = env.spawn(
            "key-last-used",
            run_last_used(env.clone(), Arc::clone(&keys), store),
        );
        env.clock.sleep(Duration::from_millis(90_500)).await;
        assert!(keys.is_loaded());
        host.spawner.request_shutdown();
        let stopped = env
            .timeout(Duration::from_millis(10), async {
                (feed.join().await, uses.join().await)
            })
            .await;
        assert_eq!(
            stopped,
            Ok((Ok(()), Ok(()))),
            "the key feed and the use writer were still running after the shutdown signal"
        );
    });
}
