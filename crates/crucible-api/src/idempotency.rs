//! `Idempotency-Key` on fact writes (contract 12.5): a key of 1 to 128 characters of `[A-Za-z0-9_-]` is kept
//! for 24 hours per (principal, route, key) with a hash of the request. A replay with the same request returns
//! the stored response; another request under the key is `idempotency_key_reused`.
//!
//! The control plane's `idempotency_keys` table is not there yet, so the keys live in this node's memory, at
//! most [`MAX_KEYS`] of them: a replay that reaches another node, or this one after a restart, writes the fact
//! again. A fact written twice resolves the same, because the later copy of an identical fact wins.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use axum::http::HeaderMap;
use crucible_records::{ApiKeyId, Timestamp};
use sha2::{Digest, Sha256};

/// The header.
pub(crate) const IDEMPOTENCY_KEY: &str = "idempotency-key";
/// How long a key is kept.
const TTL_MICROS: i64 = 24 * 3_600 * 1_000_000;
/// The most keys a node keeps; past it the oldest go first.
pub(crate) const MAX_KEYS: usize = 10_000;

/// The value of the header, when it is present and well formed; `Err` when it is malformed.
pub(crate) fn key(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let Some(value) = headers.get(IDEMPOTENCY_KEY) else {
        return Ok(None);
    };
    let text = value.to_str().map_err(|_| ())?;
    let valid = (1..=128).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(Some(text.to_owned()))
    } else {
        Err(())
    }
}

/// The hash of a request: its route and its body.
pub(crate) fn request_hash(route: &str, body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(route.as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hasher.finalize().into()
}

type Slot = (ApiKeyId, &'static str, String);

#[derive(Debug)]
struct Entry {
    hash: [u8; 32],
    at: Timestamp,
    /// The stored response: its status and body. `None` while the first request is in flight.
    response: Option<(u16, Vec<u8>)>,
}

/// A key reserved for one request. Completing it stores the response; dropping it uncompleted (a failure, or
/// a request that was cancelled) frees the key, so that a retry runs again.
#[derive(Debug)]
pub(crate) struct Reservation {
    keys: Arc<Keys>,
    slot: Slot,
    done: bool,
}

impl Reservation {
    /// Stores the response of the request for replays.
    pub(crate) fn complete(mut self, status: u16, body: Vec<u8>) {
        self.done = true;
        if let Some(entry) = self.keys.lock().get_mut(&self.slot) {
            entry.response = Some((status, body));
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.done {
            self.keys.lock().remove(&self.slot);
        }
    }
}

/// What a request under a key does.
#[derive(Debug)]
pub(crate) enum Begin {
    /// The key is new: run the request, then complete the reservation.
    Run(Reservation),
    /// The same request already succeeded: answer with its response.
    Replay(u16, Vec<u8>),
    /// The same request is still running.
    InFlight,
    /// Another request used the key.
    Reused,
}

/// The keys of one node.
#[derive(Debug, Default)]
pub(crate) struct Keys {
    entries: Mutex<BTreeMap<Slot, Entry>>,
}

impl Keys {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<Slot, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Looks up a key, and reserves it when it is new.
    pub(crate) fn begin(
        self: &Arc<Self>,
        slot: (ApiKeyId, &'static str, &str),
        hash: [u8; 32],
        now: Timestamp,
    ) -> Begin {
        let mut entries = self.lock();
        let horizon = now.saturating_add_micros(-TTL_MICROS);
        entries.retain(|_, entry| entry.at > horizon);
        let slot = (slot.0, slot.1, slot.2.to_owned());
        if let Some(entry) = entries.get(&slot) {
            return match (&entry.response, entry.hash == hash) {
                (_, false) => Begin::Reused,
                (Some((status, body)), true) => Begin::Replay(*status, body.clone()),
                (None, true) => Begin::InFlight,
            };
        }
        while entries.len() >= MAX_KEYS {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.at)
                .map(|(slot, _)| slot.clone());
            match oldest {
                Some(slot) => entries.remove(&slot),
                None => break,
            };
        }
        entries.insert(
            slot.clone(),
            Entry {
                hash,
                at: now,
                response: None,
            },
        );
        Begin::Run(Reservation {
            keys: Arc::clone(self),
            slot,
            done: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const KEY: ApiKeyId = ApiKeyId::from_bytes([1; 16]);
    const T0: Timestamp = Timestamp::from_micros(1_794_907_800_000_000);

    fn run(begin: Begin) -> Reservation {
        match begin {
            Begin::Run(reservation) => reservation,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_key_replays_its_success_and_refuses_another_request() {
        let keys = Arc::new(Keys::default());
        let (a, b) = (request_hash("tags", b"{}"), request_hash("tags", b"{ }"));
        let slot = (KEY, "tags", "k1");
        let first = run(keys.begin(slot, a, T0));
        assert!(matches!(keys.begin(slot, a, T0), Begin::InFlight));
        assert!(matches!(keys.begin(slot, b, T0), Begin::Reused));
        first.complete(201, b"ok".to_vec());
        assert!(matches!(keys.begin(slot, a, T0), Begin::Replay(201, body) if body == b"ok"));
        // Per principal and per route.
        drop(run(keys.begin((KEY, "flags", "k1"), a, T0)));
        let other = ApiKeyId::from_bytes([2; 16]);
        // A failure, or a cancelled request, frees the key; 24 hours later every key is new again.
        drop(run(keys.begin((other, "tags", "k1"), a, T0)));
        drop(run(keys.begin((other, "tags", "k1"), b, T0)));
        let later = T0.saturating_add_micros(TTL_MICROS);
        drop(run(keys.begin(slot, b, later)));
    }

    #[test]
    fn the_store_is_bounded_and_keys_are_checked() {
        let keys = Arc::new(Keys::default());
        let hash = request_hash("tags", b"");
        for n in 0..=MAX_KEYS {
            let name = n.to_string();
            let at = T0.saturating_add_micros(n as i64);
            run(keys.begin((KEY, "tags", &name), hash, at)).complete(201, Vec::new());
        }
        assert_eq!(keys.lock().len(), MAX_KEYS);
        let mut headers = HeaderMap::new();
        assert_eq!(key(&headers), Ok(None));
        for (value, expected) in [
            ("abc-DEF_09", Ok(Some("abc-DEF_09".to_owned()))),
            ("has space", Err(())),
            ("a.b", Err(())),
        ] {
            headers.insert(IDEMPOTENCY_KEY, HeaderValue::from_str(value).unwrap());
            assert_eq!(key(&headers), expected, "{value}");
        }
        headers.insert(
            IDEMPOTENCY_KEY,
            HeaderValue::from_str(&"a".repeat(129)).unwrap(),
        );
        assert_eq!(key(&headers), Err(()));
    }
}
