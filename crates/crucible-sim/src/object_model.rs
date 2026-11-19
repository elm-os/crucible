//! The simulated object store (contract 2.10, `SimObjectStore`): presigned URLs as grants, a seeded fault model
//! and a simulated client that uses the grants.
//!
//! - **Grants.** `presign_put` and `presign_get` record what the URL allows: the method, the key, the signed
//!   headers and the expiry on the store's clock. [`SimObjectStore::upload`] and [`SimObjectStore::download`]
//!   play the client and are refused as a real store refuses them.
//! - **Uploads that never arrive** are the client's choice: a grant can simply go unused.
//! - **Lax uploads.** With [`ObjectFaults::lax_uploads`] the store does not check the signed length and checksum
//!   against the body, and a HEAD returns no checksum: a stored object can differ from what was signed, and
//!   only the length catches it.
//! - **Eventually consistent listing.** Every write and delete reaches the listing after a seeded lag of up to
//!   [`ObjectFaults::list_lag`], in order per key.
//! - **Throttling.** Every call other than a presign fails as unavailable at the seeded rate.
//!
//! The model may be stricter than a real store and is never more permissive.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use crucible_env::{
    ByteStream, Clock, Disposition, ObjectError, ObjectMeta, ObjectPage, ObjectStore, Presigned,
    Rng,
};
use crucible_records::Timestamp;
use futures::StreamExt as _;
use sha2::{Digest, Sha256};

/// The most keys one listing page returns, as S3's default.
const LIST_PAGE: usize = 1_000;

/// The seeded faults of the store.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ObjectFaults {
    /// Calls that fail as throttled, in parts per million.
    pub throttle_ppm: u32,
    /// The longest a write or a delete takes to reach the listing.
    pub list_lag: Duration,
    /// The store does not check uploads against their signed length and checksum, and keeps no checksum.
    pub lax_uploads: bool,
}

/// One stored object.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StoredObject {
    /// The bytes.
    pub bytes: Bytes,
    /// The content type it was stored with.
    pub content_type: String,
    /// When it was written, on the store's clock.
    pub created_at: Timestamp,
    /// The checksum the store recorded: the signed one, unless the store is lax.
    pub sha256: Option<[u8; 32]>,
}

#[derive(Clone, Debug)]
enum Grant {
    Put {
        key: String,
        content_type: String,
        sha256: [u8; 32],
        len: u64,
        expires_at: Timestamp,
    },
    Get {
        key: String,
        disposition: Disposition,
        expires_at: Timestamp,
    },
}

/// A change on its way to the listing.
#[derive(Clone, Debug)]
struct Change {
    visible_at: Timestamp,
    key: String,
    object: Option<ObjectMeta>,
}

#[derive(Debug, Default)]
struct State {
    objects: BTreeMap<String, StoredObject>,
    grants: BTreeMap<String, Grant>,
    next_grant: u64,
    /// Changes not yet in `listed`, in the order they were made.
    changes: Vec<Change>,
    /// When the last change of each key reaches the listing, so that one key's changes arrive in order.
    last_visible: BTreeMap<String, Timestamp>,
    listed: BTreeMap<String, ObjectMeta>,
    calls: u64,
}

/// Why the simulated client's request was refused, as an HTTP status.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Refused(pub u16);

/// The simulated object store.
#[derive(Debug)]
pub struct SimObjectStore {
    clock: Arc<dyn Clock>,
    rng: Arc<dyn Rng>,
    faults: ObjectFaults,
    state: Mutex<State>,
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn micros(d: Duration) -> i64 {
    i64::try_from(d.as_micros()).unwrap_or(i64::MAX)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

impl SimObjectStore {
    /// A store on `clock` (the store's own host) whose faults draw from `rng`.
    pub fn new(clock: Arc<dyn Clock>, rng: Arc<dyn Rng>, faults: ObjectFaults) -> Arc<Self> {
        Arc::new(Self {
            clock,
            rng,
            faults,
            state: Mutex::default(),
        })
    }

    fn now(&self) -> Timestamp {
        self.clock.wall_now()
    }

    /// Fails the call as throttled at the seeded rate.
    fn gate(&self, state: &mut State) -> Result<(), ObjectError> {
        state.calls += 1;
        let rate = u64::from(self.faults.throttle_ppm);
        if rate > 0 && self.rng.next_u64() % 1_000_000 < rate {
            return Err(ObjectError::Unavailable("SlowDown".to_owned()));
        }
        Ok(())
    }

    /// Records a write or a delete for the listing.
    fn change(&self, state: &mut State, key: &str, object: Option<ObjectMeta>) {
        let lag = micros(self.faults.list_lag);
        let drawn = if lag > 0 {
            i64::try_from(self.rng.next_u64() % (lag.unsigned_abs() + 1)).unwrap_or(lag)
        } else {
            0
        };
        let earliest = state
            .last_visible
            .get(key)
            .copied()
            .unwrap_or(Timestamp::EPOCH);
        let visible_at = self.now().saturating_add_micros(drawn).max(earliest);
        state.last_visible.insert(key.to_owned(), visible_at);
        state.changes.push(Change {
            visible_at,
            key: key.to_owned(),
            object,
        });
    }

    fn store(&self, state: &mut State, key: &str, object: StoredObject) {
        let meta = ObjectMeta {
            key: key.to_owned(),
            len: object.bytes.len() as u64,
            sha256: object.sha256,
            created_at: object.created_at,
        };
        state.objects.insert(key.to_owned(), object);
        self.change(state, key, Some(meta));
    }

    fn remove(&self, state: &mut State, key: &str) {
        if state.objects.remove(key).is_some() {
            self.change(state, key, None);
        }
    }

    fn grant(&self, state: &mut State, key: &str, grant: Grant) -> String {
        state.next_grant += 1;
        let url = format!("sim://objects/{key}?grant={}", state.next_grant);
        state.grants.insert(url.clone(), grant);
        url
    }

    /// The simulated client's PUT to a presigned URL: refused (403) when the grant is unknown, expired or for
    /// another content type or length; with the store not lax, refused (400) when the body's checksum is not
    /// the signed one. On success the object is stored with the signed content type.
    pub fn upload(&self, url: &str, content_type: &str, body: Bytes) -> Result<(), Refused> {
        let mut state = lock(&self.state);
        let Some(Grant::Put {
            key,
            content_type: signed_type,
            sha256: signed,
            len,
            expires_at,
        }) = state.grants.get(url).cloned()
        else {
            return Err(Refused(403));
        };
        if self.now() >= expires_at || content_type != signed_type {
            return Err(Refused(403));
        }
        if !self.faults.lax_uploads {
            if body.len() as u64 != len {
                return Err(Refused(403));
            }
            if sha256(&body) != signed {
                return Err(Refused(400));
            }
        }
        let object = StoredObject {
            bytes: body,
            content_type: signed_type,
            created_at: self.now(),
            sha256: (!self.faults.lax_uploads).then_some(signed),
        };
        self.store(&mut state, &key, object);
        Ok(())
    }

    /// The simulated client's GET of a presigned URL: the object and the disposition the answer carries, or
    /// 403 for an unknown or expired grant and 404 for a missing object.
    pub fn download(&self, url: &str) -> Result<(StoredObject, Disposition), Refused> {
        let state = lock(&self.state);
        let Some(Grant::Get {
            key,
            disposition,
            expires_at,
        }) = state.grants.get(url).cloned()
        else {
            return Err(Refused(403));
        };
        if self.now() >= expires_at {
            return Err(Refused(403));
        }
        let object = state.objects.get(&key).cloned().ok_or(Refused(404))?;
        Ok((object, disposition))
    }

    /// The stored objects, by key: what the oracles compare the control store with.
    pub fn objects(&self) -> BTreeMap<String, StoredObject> {
        lock(&self.state).objects.clone()
    }

    /// Stores an object directly, bypassing grants: the state an upload outside Crucible leaves.
    pub fn plant(&self, key: &str, bytes: Bytes, created_at: Timestamp) {
        let mut state = lock(&self.state);
        let object = StoredObject {
            sha256: Some(sha256(&bytes)),
            bytes,
            content_type: "application/octet-stream".to_owned(),
            created_at,
        };
        self.store(&mut state, key, object);
    }
}

#[async_trait]
impl ObjectStore for SimObjectStore {
    fn presign_put(
        &self,
        key: &str,
        content_type: &str,
        sha256: &[u8; 32],
        len: u64,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        let expires_at = self.now().saturating_add_micros(micros(ttl));
        let mut state = lock(&self.state);
        let grant = Grant::Put {
            key: key.to_owned(),
            content_type: content_type.to_owned(),
            sha256: *sha256,
            len,
            expires_at,
        };
        let url = self.grant(&mut state, key, grant);
        Ok(Presigned {
            url,
            headers: vec![
                ("content-type".to_owned(), content_type.to_owned()),
                ("content-length".to_owned(), len.to_string()),
            ],
            expires_at,
        })
    }

    fn presign_get(
        &self,
        key: &str,
        disposition: Disposition,
        ttl: Duration,
    ) -> Result<Presigned, ObjectError> {
        let expires_at = self.now().saturating_add_micros(micros(ttl));
        let mut state = lock(&self.state);
        let grant = Grant::Get {
            key: key.to_owned(),
            disposition,
            expires_at,
        };
        let url = self.grant(&mut state, key, grant);
        Ok(Presigned {
            url,
            headers: Vec::new(),
            expires_at,
        })
    }

    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>, ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        Ok(state.objects.get(key).map(|object| ObjectMeta {
            key: key.to_owned(),
            len: object.bytes.len() as u64,
            sha256: object.sha256,
            created_at: object.created_at,
        }))
    }

    async fn put_small(&self, key: &str, bytes: Bytes) -> Result<(), ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        let object = StoredObject {
            sha256: Some(sha256(&bytes)),
            bytes,
            content_type: "application/octet-stream".to_owned(),
            created_at: self.now(),
        };
        self.store(&mut state, key, object);
        Ok(())
    }

    async fn get_stream(&self, key: &str) -> Result<ByteStream, ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        let bytes = state
            .objects
            .get(key)
            .map(|object| object.bytes.clone())
            .ok_or(ObjectError::NotFound)?;
        Ok(futures::stream::iter([Ok(bytes)]).boxed())
    }

    async fn delete(&self, key: &str) -> Result<(), ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        self.remove(&mut state, key);
        Ok(())
    }

    async fn delete_batch(&self, keys: &[String]) -> Result<(), ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        if keys.len() > 1_000 {
            return Err(ObjectError::Rejected {
                status: 400,
                code: "MalformedXML".to_owned(),
            });
        }
        for key in keys {
            self.remove(&mut state, key);
        }
        Ok(())
    }

    async fn list(&self, prefix: &str, after: Option<&str>) -> Result<ObjectPage, ObjectError> {
        let mut state = lock(&self.state);
        self.gate(&mut state)?;
        // The changes that have reached the listing, in the order they were made.
        let now = self.now();
        let (arrived, waiting): (Vec<Change>, Vec<Change>) = std::mem::take(&mut state.changes)
            .into_iter()
            .partition(|change| change.visible_at <= now);
        state.changes = waiting;
        for change in arrived {
            match change.object {
                Some(meta) => state.listed.insert(change.key, meta),
                None => state.listed.remove(&change.key),
            };
        }
        let mut objects: Vec<ObjectMeta> = state
            .listed
            .range(after.map(str::to_owned).unwrap_or_default()..)
            .filter(|(key, _)| key.starts_with(prefix) && Some(key.as_str()) != after)
            .map(|(_, meta)| meta.clone())
            .take(LIST_PAGE + 1)
            .collect();
        let next = if objects.len() > LIST_PAGE {
            objects.truncate(LIST_PAGE);
            objects.last().map(|object| object.key.clone())
        } else {
            None
        };
        Ok(ObjectPage { objects, next })
    }
}
