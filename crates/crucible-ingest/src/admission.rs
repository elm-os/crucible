//! Admission before the ack (contract 4.8): the `ingest.inflight_bytes` budget and the WAL's signals, combined
//! by [`crucible_admission::decide`].

use std::sync::{Arc, Mutex, PoisonError};

use crucible_admission::{ByteBudget, Decision, Signals, UNAVAILABLE_MIN_S, decide};
use crucible_env::{Gauge, Rng};
use crucible_wal::Wal;

use crate::reply::Reject;

/// The admission state shared by every request.
#[derive(Debug)]
pub(crate) struct Admission {
    pub(crate) wal: Wal,
    wal_max_bytes: u64,
    wal_min_free_bytes: u64,
    /// The source of the 429 jitter.
    jitter: Arc<dyn Rng>,
    budget: ByteBudget,
    /// The budget's metric. Updated under `lock` together with the budget, so that it never lags behind.
    gauge: Gauge,
    lock: Mutex<()>,
}

impl Admission {
    pub(crate) fn new(
        wal: Wal,
        wal_max_bytes: u64,
        wal_min_free_bytes: u64,
        jitter: Arc<dyn Rng>,
        inflight_bytes: u64,
        gauge: Gauge,
    ) -> Self {
        gauge.set(0);
        Self {
            wal,
            wal_max_bytes,
            wal_min_free_bytes,
            jitter,
            budget: ByteBudget::new(inflight_bytes),
            gauge,
            lock: Mutex::new(()),
        }
    }

    /// Reserves (`grow`) or releases bytes in the budget and publishes the total. Returns whether a reservation
    /// fitted; a release always succeeds.
    fn change(&self, grow: bool, bytes: u64) -> bool {
        let _serial = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        let ok = if grow {
            self.budget.try_reserve(bytes).is_ok()
        } else {
            let released = self.budget.release(bytes);
            debug_assert!(released.is_ok(), "released more than was reserved");
            true
        };
        self.gauge
            .set(i64::try_from(self.budget.used()).unwrap_or(i64::MAX));
        ok
    }

    /// The decision table of contract 4.8 over the WAL's current signals. No shipper drains the WAL in
    /// milestone 1, so the drain rate is zero and a 503 for WAL space waits the longest.
    fn decide(&self, inflight_exceeded: bool) -> Decision {
        let wal = self.wal.admission();
        decide(&Signals {
            key_unknown_control_plane_down: false,
            wal_failed: wal.failed,
            wal_bytes: wal.segment_bytes,
            wal_max_bytes: self.wal_max_bytes,
            disk_free_bytes: wal.free_bytes,
            disk_min_free_bytes: self.wal_min_free_bytes,
            drain_bytes_per_s: 0,
            inflight_exceeded,
            bucket_exceeded: false,
            jitter: self.jitter.next_u64(),
        })
    }

    fn refusal(&self, inflight_exceeded: bool) -> Option<Reject> {
        match self.decide(inflight_exceeded) {
            Decision::Admit => None,
            Decision::TooManyRequests { retry_after_s } => {
                Some(Reject::TooManyRequests { retry_after_s })
            }
            Decision::Unavailable { retry_after_s } => Some(Reject::Unavailable {
                retry_after_s,
                detail: "the write-ahead log cannot take more data now",
            }),
        }
    }

    /// Admits a request that declares `bytes`: reserves them and consults the decision table. The returned
    /// reservation grows as the request is read and decoded.
    pub(crate) fn admit(self: &Arc<Self>, bytes: u64) -> Result<Held, Reject> {
        let mut held = Held {
            admission: Arc::clone(self),
            bytes: 0,
        };
        let exceeded = !self.change(true, bytes);
        if !exceeded {
            held.bytes = bytes;
        }
        match self.refusal(exceeded) {
            None => Ok(held),
            Some(reject) => Err(reject),
        }
    }

    /// The answer to an append that failed (contract 4.5): 503 with the `Retry-After` that the WAL's state calls
    /// for, and at least the shortest one.
    pub(crate) fn append_failed(&self) -> Reject {
        match self.refusal(false) {
            Some(reject @ Reject::Unavailable { .. }) => reject,
            _ => Reject::Unavailable {
                retry_after_s: UNAVAILABLE_MIN_S,
                detail: "the write-ahead log failed to make the request durable",
            },
        }
    }
}

/// The bytes one request holds in the in-flight budget. Dropping it releases them.
#[derive(Debug)]
pub(crate) struct Held {
    admission: Arc<Admission>,
    bytes: u64,
}

impl Held {
    /// Grows the reservation to at least `bytes`. When the budget cannot hold them, the request is refused
    /// with the decision for an exceeded budget: 429, or 503 when the WAL refuses as well.
    pub(crate) fn grow_to(&mut self, bytes: u64) -> Result<(), Reject> {
        if bytes <= self.bytes {
            return Ok(());
        }
        if self.admission.change(true, bytes - self.bytes) {
            self.bytes = bytes;
            return Ok(());
        }
        Err(self
            .admission
            .refusal(true)
            .unwrap_or(Reject::TooManyRequests {
                retry_after_s: crucible_admission::THROTTLED_MIN_S,
            }))
    }

    /// Sets the reservation to exactly `bytes`, releasing what the request no longer holds.
    pub(crate) fn set(&mut self, bytes: u64) -> Result<(), Reject> {
        if bytes < self.bytes {
            self.admission.change(false, self.bytes - bytes);
            self.bytes = bytes;
        }
        self.grow_to(bytes)
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        if self.bytes > 0 {
            self.admission.change(false, self.bytes);
        }
    }
}
