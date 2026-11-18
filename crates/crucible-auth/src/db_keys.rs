//! API keys from Postgres (contracts 11.3 and 11.8): an in-memory snapshot of every key, verified without a
//! database call or an allocation, kept current by the key feed, with the outage modes of `auth.pg_outage_mode`.
//!
//! - **Snapshot.** Immutable, swapped whole under a lock that readers hold only to clone an `Arc`. Never written
//!   to disk.
//! - **Verification.** Parse, map lookup (by lookup tag for a Bearer secret, by public key for a pair),
//!   `SHA-256(salt || secret)`, constant-time compare, then local expiry and revocation against the `Env` clock.
//! - **Feed.** [`run_feed`] polls every second (jitter from `Rng`) with a 2 s timeout and one retry, re-reading
//!   an overlap window that covers the transaction cap, applies rows by key version (a lower version never
//!   overwrites a higher one), and reloads everything every 60 s.
//! - **Outage.** Under `fail_open` (the default) keys in the snapshot keep working, local expiry still applies,
//!   and a key the snapshot lacks gets 503 with `Retry-After` while polls fail, because a new key cannot be told
//!   from an invalid one; `fail_open_max` optionally ends that after a while. Under `fail_closed`, 5 s without a
//!   successful poll makes every secret-key request 503. Before the first snapshot nothing is accepted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use crucible_control::{
    ApiKey, ControlStore, KeyChange, KeyFeed, KeyScope, KeySnapshot, TRANSACTION_CAP, TxError,
};
use crucible_env::{Clock, Env, Instant};
use crucible_records::{ApiKeyId, ProjectId, Timestamp};
use subtle::ConstantTimeEq;

use crate::key::{KeyPrincipal, KeyVerifier};
use crate::mint::{LookupKey, secret_hash};
use crate::scope;

/// The poll interval of the key feed.
pub const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long one poll may take.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the whole snapshot is reloaded.
pub const RECONCILE_EVERY: Duration = Duration::from_secs(60);
/// How often `last_used_at` is written.
pub const LAST_USED_EVERY: Duration = Duration::from_secs(60);
/// Without a successful poll for this long, the node is in an outage.
pub const OUTAGE_AFTER: Duration = Duration::from_secs(5);
/// The `Retry-After` of a request refused during an outage.
pub const RETRY_AFTER: Duration = Duration::from_secs(5);
/// The overlap window of a poll: the transaction cap plus room for the poll interval and timeout.
const OVERLAP: Duration = TRANSACTION_CAP.saturating_add(Duration::from_secs(5));
/// The jitter added to each poll interval, at most.
const JITTER_MICROS: u64 = 250_000;

/// `auth.pg_outage_mode` (contract 11.8).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutageMode {
    /// Keys in the snapshot keep working during an outage; `max` optionally bounds how long.
    FailOpen {
        /// After an outage this long, the node behaves as under `FailClosed`. `None` is unlimited.
        max: Option<Duration>,
    },
    /// After [`OUTAGE_AFTER`] without a successful poll, every secret-key request is refused with 503.
    FailClosed,
}

impl Default for OutageMode {
    fn default() -> Self {
        Self::FailOpen { max: None }
    }
}

/// A set of key scopes, as bits.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Scopes(u8);

impl Scopes {
    /// The scopes of a static key from the keys file: those of a new key.
    pub const DEFAULT: Self = Self(1 | 2 | 4);

    const fn bit(scope: KeyScope) -> u8 {
        match scope {
            KeyScope::Ingest => 1,
            KeyScope::Read => 2,
            KeyScope::Write => 4,
            KeyScope::Delete => 8,
            KeyScope::Sql => 16,
        }
    }

    /// The set of `scopes`.
    pub fn of(scopes: &[KeyScope]) -> Self {
        Self(
            scopes
                .iter()
                .fold(0, |bits, scope| bits | Self::bit(*scope)),
        )
    }

    /// Whether `scope` is in the set.
    pub const fn has(self, scope: KeyScope) -> bool {
        self.0 & Self::bit(scope) != 0
    }
}

/// A verified key: its principal, scopes and import flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct KeyGrant {
    /// The project principal.
    pub principal: KeyPrincipal,
    /// What the key may do.
    pub scopes: Scopes,
    /// Whether the key may bypass the event-age limit.
    pub import: bool,
}

/// The answer to a presented key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyVerdict {
    /// The key is valid.
    Valid(KeyGrant),
    /// The key is unknown, wrong, expired or revoked: 401.
    Invalid,
    /// The node cannot tell: 503 with `Retry-After`.
    Unavailable {
        /// When to try again.
        retry_after: Duration,
    },
}

/// One key as the snapshot holds it.
#[derive(Debug)]
struct Entry {
    id: ApiKeyId,
    project_id: ProjectId,
    public_key: String,
    salt: [u8; 16],
    hash: [u8; 32],
    lookup_tag: [u8; 8],
    scopes: Scopes,
    import: bool,
    expires_at: Option<Timestamp>,
    revoked: bool,
    version: u64,
    /// When the key was last used on this node, in microseconds; 0 when it was not.
    last_used: AtomicI64,
    /// The last use already written to Postgres.
    flushed: AtomicI64,
}

impl Entry {
    fn new(key: &ApiKey, carried: Option<&Entry>) -> Self {
        let (used, flushed) = carried.map_or((0, 0), |e| {
            (
                e.last_used.load(Ordering::Relaxed),
                e.flushed.load(Ordering::Relaxed),
            )
        });
        Self {
            id: key.id,
            project_id: key.project_id,
            public_key: key.public_key.clone(),
            salt: key.secret_salt,
            hash: key.secret_hash,
            lookup_tag: key.lookup_tag,
            scopes: Scopes::of(&key.scopes),
            import: key.import,
            expires_at: key.expires_at,
            revoked: key.revoked_at.is_some(),
            version: key.version,
            last_used: AtomicI64::new(used),
            flushed: AtomicI64::new(flushed),
        }
    }
}

/// Every key, immutable.
#[derive(Debug, Default)]
struct Snapshot {
    by_id: BTreeMap<ApiKeyId, Arc<Entry>>,
    by_tag: BTreeMap<[u8; 8], Vec<Arc<Entry>>>,
    by_public: BTreeMap<String, Arc<Entry>>,
    /// The highest feed sequence number seen.
    max_seq: u64,
}

impl Snapshot {
    fn build(by_id: BTreeMap<ApiKeyId, Arc<Entry>>, max_seq: u64) -> Self {
        let mut by_tag: BTreeMap<[u8; 8], Vec<Arc<Entry>>> = BTreeMap::new();
        let mut by_public = BTreeMap::new();
        for entry in by_id.values() {
            by_tag
                .entry(entry.lookup_tag)
                .or_default()
                .push(Arc::clone(entry));
            by_public.insert(entry.public_key.clone(), Arc::clone(entry));
        }
        Self {
            by_id,
            by_tag,
            by_public,
            max_seq,
        }
    }

    /// `self` with `keys` applied by version, or `None` when nothing changes.
    fn with(&self, keys: &[&ApiKey], max_seq: u64) -> Option<Self> {
        let newer: Vec<&&ApiKey> = keys
            .iter()
            .filter(|key| {
                self.by_id
                    .get(&key.id)
                    .is_none_or(|entry| key.version > entry.version)
            })
            .collect();
        if newer.is_empty() {
            return (max_seq > self.max_seq).then(|| Self::build(self.by_id.clone(), max_seq));
        }
        let mut by_id = self.by_id.clone();
        for key in newer {
            let carried = by_id.get(&key.id).cloned();
            if carried
                .as_ref()
                .is_none_or(|entry| key.version > entry.version)
            {
                by_id.insert(key.id, Arc::new(Entry::new(key, carried.as_deref())));
            }
        }
        Some(Self::build(by_id, max_seq.max(self.max_seq)))
    }
}

#[derive(Debug, Default)]
struct Health {
    /// When the last poll or reload succeeded.
    last_success: Option<Instant>,
}

/// The database keys of one node.
pub struct DbKeys {
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    health: Mutex<Health>,
    mode: OutageMode,
    lookup: LookupKey,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for DbKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbKeys")
            .field("mode", &self.mode)
            .field("loaded", &self.is_loaded())
            .finish_non_exhaustive()
    }
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

fn health(lock: &Mutex<Health>) -> std::sync::MutexGuard<'_, Health> {
    lock.lock().unwrap_or_else(PoisonError::into_inner)
}

impl DbKeys {
    /// No snapshot yet: nothing is accepted until [`DbKeys::load`] or the feed succeeds.
    pub fn new(env: &Env, lookup: LookupKey, mode: OutageMode) -> Self {
        Self {
            snapshot: RwLock::new(None),
            health: Mutex::new(Health::default()),
            mode,
            lookup,
            clock: Arc::clone(&env.clock),
        }
    }

    /// Whether a snapshot is loaded; readiness requires it (contract 11.8).
    pub fn is_loaded(&self) -> bool {
        read(&self.snapshot).is_some()
    }

    /// How long ago a poll or reload last succeeded; `None` before the first.
    pub fn age(&self) -> Option<Duration> {
        let last = health(&self.health).last_success?;
        Some(self.clock.now().saturating_duration_since(last))
    }

    fn current(&self) -> Option<Arc<Snapshot>> {
        read(&self.snapshot).clone()
    }

    fn succeeded(&self) {
        health(&self.health).last_success = Some(self.clock.now());
    }

    /// Applies a full reload. Like feed rows, its keys apply by version: a reload read before a change that this
    /// node applied locally never undoes the change. Keys are never deleted, so none is dropped.
    pub fn load(&self, snapshot: &KeySnapshot) {
        let keys: Vec<&ApiKey> = snapshot.keys.iter().collect();
        let mut current = write(&self.snapshot);
        let base = current.clone().unwrap_or_default();
        let next = base.with(&keys, snapshot.max_seq).map_or(base, Arc::new);
        *current = Some(next);
        drop(current);
        self.succeeded();
    }

    /// Applies feed rows by key version and records a successful poll.
    pub fn apply(&self, changes: &[KeyChange]) {
        let keys: Vec<&ApiKey> = changes.iter().map(|change| &change.key).collect();
        let max_seq = changes.iter().map(|change| change.seq).max().unwrap_or(0);
        self.apply_keys(&keys, max_seq);
        self.succeeded();
    }

    /// Applies a key that this node just changed, before the response returns (contract 11.8: a revocation made
    /// through a node applies on that node at once).
    pub fn apply_local(&self, key: &ApiKey) {
        self.apply_keys(&[key], 0);
    }

    fn apply_keys(&self, keys: &[&ApiKey], max_seq: u64) {
        let mut current = write(&self.snapshot);
        if let Some(snapshot) = current.as_deref()
            && let Some(next) = snapshot.with(keys, max_seq)
        {
            *current = Some(Arc::new(next));
        }
    }

    /// The highest feed sequence number seen; polling resumes after it.
    pub fn max_seq(&self) -> u64 {
        self.current().map_or(0, |snapshot| snapshot.max_seq)
    }

    /// Whether the node is in an outage now, and since how long.
    fn outage(&self) -> Option<Duration> {
        let since = self.age()?;
        (since >= OUTAGE_AFTER).then_some(since)
    }

    fn unavailable() -> KeyVerdict {
        KeyVerdict::Unavailable {
            retry_after: RETRY_AFTER,
        }
    }

    /// Refuses everything when the mode says the outage is too long to answer from the snapshot.
    fn closed(&self) -> bool {
        match (self.mode, self.outage()) {
            (_, None) => false,
            (OutageMode::FailClosed, Some(_)) => true,
            (OutageMode::FailOpen { max }, Some(since)) => max.is_some_and(|max| since >= max),
        }
    }

    /// The verdict on `entry` once its secret matched.
    fn grant(&self, entry: &Entry) -> KeyVerdict {
        let now = self.clock.wall_now();
        if entry.revoked || entry.expires_at.is_some_and(|at| at <= now) {
            return KeyVerdict::Invalid;
        }
        let Ok(scope) = scope::mint(entry.project_id) else {
            return KeyVerdict::Invalid;
        };
        entry.last_used.fetch_max(now.micros(), Ordering::Relaxed);
        KeyVerdict::Valid(KeyGrant {
            principal: KeyPrincipal {
                scope,
                api_key_id: entry.id,
            },
            scopes: entry.scopes,
            import: entry.import,
        })
    }

    fn matches(entry: &Entry, secret: &str) -> bool {
        bool::from(
            secret_hash(&entry.salt, secret)
                .as_slice()
                .ct_eq(entry.hash.as_slice()),
        )
    }

    /// The verdict when no key matched: unknown during an outage is 503, otherwise 401.
    fn unknown(&self) -> KeyVerdict {
        if self.outage().is_some() {
            Self::unavailable()
        } else {
            KeyVerdict::Invalid
        }
    }

    /// Verifies a secret key presented as `Bearer`. Allocates nothing.
    pub fn check_secret(&self, secret: &str) -> KeyVerdict {
        let Some(snapshot) = self.current() else {
            return Self::unavailable();
        };
        if self.closed() {
            return Self::unavailable();
        }
        let tag = self.lookup.tag(secret);
        let found = snapshot
            .by_tag
            .get(&tag)
            .and_then(|entries| entries.iter().find(|entry| Self::matches(entry, secret)));
        match found {
            Some(entry) => self.grant(entry),
            None => self.unknown(),
        }
    }

    /// Verifies a public and secret key pair presented as `Basic`. Allocates nothing.
    pub fn check_pair(&self, public: &str, secret: &str) -> KeyVerdict {
        let Some(snapshot) = self.current() else {
            return Self::unavailable();
        };
        if self.closed() {
            return Self::unavailable();
        }
        match snapshot.by_public.get(public) {
            Some(entry) if Self::matches(entry, secret) => self.grant(entry),
            Some(_) => KeyVerdict::Invalid,
            None => self.unknown(),
        }
    }

    /// The keys used since their last write, with the time of their last use; marks them written.
    fn take_uses(&self) -> Vec<(ApiKeyId, Timestamp)> {
        let Some(snapshot) = self.current() else {
            return Vec::new();
        };
        snapshot
            .by_id
            .values()
            .filter_map(|entry| {
                let used = entry.last_used.load(Ordering::Relaxed);
                let flushed = entry.flushed.swap(used, Ordering::Relaxed);
                (used > flushed).then(|| (entry.id, Timestamp::from_micros(used)))
            })
            .collect()
    }
}

impl KeyVerifier for DbKeys {
    fn verify(&self, secret: &str) -> Option<KeyPrincipal> {
        match self.check_secret(secret) {
            KeyVerdict::Valid(grant) => Some(grant.principal),
            KeyVerdict::Invalid | KeyVerdict::Unavailable { .. } => None,
        }
    }
}

/// A [`KeyFeed`] over a whole control store.
#[derive(Debug)]
pub struct StoreFeed(pub Arc<dyn ControlStore>);

#[async_trait]
impl KeyFeed for StoreFeed {
    async fn poll(&self, after_seq: u64, overlap: Duration) -> Result<Vec<KeyChange>, TxError> {
        self.0.key_feed().poll(after_seq, overlap).await
    }

    async fn snapshot(&self) -> Result<KeySnapshot, TxError> {
        self.0.key_feed().snapshot().await
    }
}

/// One poll with its timeout.
async fn poll_once(
    env: &Env,
    keys: &DbKeys,
    feed: &dyn KeyFeed,
    overlap: Duration,
) -> Result<(), String> {
    match env
        .timeout(POLL_TIMEOUT, feed.poll(keys.max_seq(), overlap))
        .await
    {
        Ok(Ok(changes)) => {
            keys.apply(&changes);
            Ok(())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("the poll timed out".to_owned()),
    }
}

/// A full reload with the poll timeout.
async fn reload(env: &Env, keys: &DbKeys, feed: &dyn KeyFeed) -> Result<(), String> {
    match env.timeout(POLL_TIMEOUT, feed.snapshot()).await {
        Ok(Ok(snapshot)) => {
            keys.load(&snapshot);
            Ok(())
        }
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err("the reload timed out".to_owned()),
    }
}

/// Keeps `keys` current until shutdown: the first reload (retried every poll interval until it succeeds), then
/// a poll every second with one retry, and a reload every 60 s.
pub async fn run_feed(env: Env, keys: Arc<DbKeys>, feed: Arc<dyn KeyFeed>) {
    feed_loop(env, keys, feed, OVERLAP).await;
}

/// [`run_feed`] with the overlap window as a parameter, so that a test can show what a missing window costs.
async fn feed_loop(env: Env, keys: Arc<DbKeys>, feed: Arc<dyn KeyFeed>, overlap: Duration) {
    let rng = env.rng.fork("crucible-auth/key-feed");
    let mut since_reload = Duration::ZERO;
    let mut loaded = keys.is_loaded();
    loop {
        if !loaded {
            match reload(&env, &keys, feed.as_ref()).await {
                Ok(()) => {
                    loaded = true;
                    since_reload = Duration::ZERO;
                    tracing::info!("the key snapshot is loaded");
                }
                Err(error) => tracing::warn!(%error, "loading the key snapshot failed; retrying"),
            }
        } else if since_reload >= RECONCILE_EVERY {
            if let Err(error) = reload(&env, &keys, feed.as_ref()).await {
                tracing::warn!(%error, "reloading the key snapshot failed");
            } else {
                since_reload = Duration::ZERO;
            }
        } else if let Err(first) = poll_once(&env, &keys, feed.as_ref(), overlap).await
            && let Err(error) = poll_once(&env, &keys, feed.as_ref(), overlap).await
        {
            tracing::warn!(first = %first, %error, "polling the key feed failed twice");
        }
        let jitter = Duration::from_micros(rng.next_u64() % JITTER_MICROS);
        let pause = POLL_EVERY + jitter;
        env.clock.sleep(pause).await;
        since_reload += pause;
    }
}

/// Writes `last_used_at` every 60 s for the keys used since, skipping the write during an outage.
pub async fn run_last_used(env: Env, keys: Arc<DbKeys>, store: Arc<dyn ControlStore>) {
    loop {
        env.clock.sleep(LAST_USED_EVERY).await;
        if keys.outage().is_some() {
            continue;
        }
        for (id, at) in keys.take_uses() {
            if let Err(error) = store.api_keys().set_last_used(id, at).await {
                tracing::debug!(%error, "recording a key's last use");
            }
        }
    }
}

#[cfg(test)]
#[path = "db_keys_tests.rs"]
mod tests;
