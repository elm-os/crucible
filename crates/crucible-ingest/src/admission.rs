//! Admission before the ack (contract 4.8): the `ingest.inflight_bytes` budget and the WAL's signals, combined
//! by [`crucible_admission::decide`].

use std::sync::{Arc, Mutex, PoisonError};

use crucible_admission::{
    BucketConfig, ByteBudget, Decision, ProjectBuckets, Signals, UNAVAILABLE_MIN_S, decide,
};
use crucible_env::{Clock, Gauge, Instant, Rng};
use crucible_records::ProjectId;
use crucible_wal::{Wal, WalConfig};

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
    /// The per-project byte rate, when the operator set one; `None` leaves ingest to backpressure alone
    /// (contract 11.9).
    buckets: Option<Mutex<ProjectBuckets>>,
    clock: Arc<dyn Clock>,
    origin: Instant,
}

/// What a refused request is told, by the condition that refused it.
const INFLIGHT: &str =
    "the server holds too many request bytes at once (ingest.inflight_bytes); retry shortly";
const PROJECT_RATE: &str =
    "the project sends more bytes per second than its configured rate (ingest.project_bytes_per_s)";
const WAL_FAILED: &str = "the write-ahead log failed and stays closed until the server restarts";
const WAL_FULL: &str = "the write-ahead log is above 90% of wal.max_bytes; it reopens as shipping to \
                        the telemetry store drains it";
const DISK_FLOOR: &str = "free disk space is below wal.min_free_bytes";

impl Admission {
    pub(crate) fn new(
        wal: Wal,
        wal_config: &WalConfig,
        jitter: Arc<dyn Rng>,
        inflight_bytes: u64,
        gauge: Gauge,
        project_rate: Option<BucketConfig>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        gauge.set(0);
        Self {
            wal,
            wal_max_bytes: wal_config.max_bytes,
            wal_min_free_bytes: wal_config.min_free_bytes,
            jitter,
            budget: ByteBudget::new(inflight_bytes),
            gauge,
            lock: Mutex::new(()),
            buckets: project_rate.map(|config| Mutex::new(ProjectBuckets::new(config))),
            origin: clock.now(),
            clock,
        }
    }

    /// Takes `bytes` from the project's bucket. `true` when no rate is set or the bucket covered them.
    pub(crate) fn take(&self, project: ProjectId, bytes: u64) -> bool {
        let Some(buckets) = &self.buckets else {
            return true;
        };
        let elapsed = self.clock.now().saturating_duration_since(self.origin);
        let now_ns = u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_take(project, bytes, now_ns)
            .is_ok()
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

    /// The decision table of contract 4.8 over the WAL's current signals, with the reason for a refusal. The
    /// drain rate is what the shipper last reported to the WAL, so a 503 for WAL space waits only as long as
    /// the backlog takes to drain.
    pub(crate) fn refusal(&self, inflight_exceeded: bool, bucket_exceeded: bool) -> Option<Reject> {
        let wal = self.wal.admission();
        let decision = decide(&Signals {
            key_unknown_control_plane_down: false,
            wal_failed: wal.failed,
            wal_bytes: wal.segment_bytes,
            wal_max_bytes: self.wal_max_bytes,
            disk_free_bytes: wal.free_bytes,
            disk_min_free_bytes: self.wal_min_free_bytes,
            drain_bytes_per_s: wal.drain_bytes_per_s,
            inflight_exceeded,
            bucket_exceeded,
            jitter: self.jitter.next_u64(),
        });
        match decision {
            Decision::Admit => None,
            Decision::TooManyRequests { retry_after_s } => Some(Reject::TooManyRequests {
                retry_after_s,
                detail: if inflight_exceeded {
                    INFLIGHT
                } else {
                    PROJECT_RATE
                },
            }),
            Decision::Unavailable { retry_after_s } => Some(Reject::Unavailable {
                retry_after_s,
                detail: if wal.failed {
                    WAL_FAILED
                } else if wal.free_bytes < self.wal_min_free_bytes {
                    DISK_FLOOR
                } else {
                    WAL_FULL
                },
            }),
        }
    }

    /// Admits a request of `project` that declares `bytes`: reserves them in the in-flight budget, takes them
    /// from the project's bucket when a rate is set, and consults the decision table. The returned reservation
    /// grows as the request is read and decoded.
    pub(crate) fn admit(self: &Arc<Self>, project: ProjectId, bytes: u64) -> Result<Held, Reject> {
        let mut held = Held {
            admission: Arc::clone(self),
            bytes: 0,
        };
        let exceeded = !self.change(true, bytes);
        if !exceeded {
            held.bytes = bytes;
        }
        let throttled = !exceeded && !self.take(project, bytes);
        match self.refusal(exceeded, throttled) {
            None => Ok(held),
            Some(reject) => Err(reject),
        }
    }

    /// The answer to an append that failed (contract 4.5): 503 with the `Retry-After` that the WAL's state calls
    /// for, and at least the shortest one.
    pub(crate) fn append_failed(&self) -> Reject {
        match self.refusal(false, false) {
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
    /// Grows the reservation to at least `bytes` when the budget can hold them, and says whether it did. When it
    /// cannot, nothing changes and no refusal is decided: for a reservation made ahead of the bytes, on a hint.
    pub(crate) fn try_grow_to(&mut self, bytes: u64) -> bool {
        if bytes <= self.bytes {
            return true;
        }
        let grown = self.admission.change(true, bytes - self.bytes);
        if grown {
            self.bytes = bytes;
        }
        grown
    }

    /// Grows the reservation to at least `bytes`. When the budget cannot hold them, the request is refused
    /// with the decision for an exceeded budget: 429, or 503 when the WAL refuses as well.
    pub(crate) fn grow_to(&mut self, bytes: u64) -> Result<(), Reject> {
        if self.try_grow_to(bytes) {
            return Ok(());
        }
        Err(self
            .admission
            .refusal(true, false)
            .unwrap_or(Reject::TooManyRequests {
                retry_after_s: crucible_admission::THROTTLED_MIN_S,
                detail: INFLIGHT,
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
