//! The frames read ahead of stage B (contract 3.2). Stage A is pure per frame, so while the shipper is behind the
//! log it runs on each frame that carries rows as a blocking job under the stage A share of the CPU budget, while
//! the shipper task does stage B of the frames before it. Stage B takes the frames strictly in LSN order, each
//! with the records its stage A made, whatever order the jobs end in. A frame is charged to
//! `normalize.cache_bytes` from its read until stage B takes it (contract 4.8).

use std::collections::VecDeque;
use std::future;
use std::sync::Arc;

use crucible_env::Gauge;
use crucible_normalize::NormCtx;
use crucible_records::Lsn;
use crucible_wal::WalFrame;
use crucible_wal_codec::{MAX_PAYLOAD, RecordType};

use crate::cpu::{Pending, STAGE_A};
use crate::error::ShipError;
use crate::frames::{Content, Unshippable, read_frame};
use crate::shipper::Ctx;

/// What stage A made of a frame that carries rows: their records, or why this binary cannot read the frame.
pub(crate) type Read = Result<Content, Unshippable>;

/// A frame stage B can take: the frame, and for a frame that carries rows what stage A made of it.
pub(crate) type Ready = (WalFrame, Option<Read>);

/// A frame whose payload decompresses to at most this many bytes runs stage A on the shipper task, because the
/// hop to a blocking thread costs more than the work (default; revisit). Ingest prepares a small body on its
/// async worker for the same reason.
const INLINE_MAX_BYTES: u64 = 16 << 10;

#[derive(Debug)]
enum StageA {
    /// The frame carries no rows: stage B takes it as it is.
    Passes,
    /// The job runs, or waits for a permit.
    Running(Pending<Read>),
    /// Stage A is done with the frame.
    Done(Read),
    /// The job ended without a result.
    Lost,
}

#[derive(Debug)]
struct Slot {
    frame: WalFrame,
    stage_a: StageA,
    bytes: u64,
}

/// What a frame is charged to `normalize.cache_bytes`: its decompressed payload, whose length a compressed
/// payload records in front of its block, else its stored payload.
fn charge(frame: &WalFrame) -> u64 {
    let stored = frame.stored_payload();
    match stored.first_chunk::<4>() {
        Some(prefix) if frame.header().compressed => {
            u64::from(u32::from_le_bytes(*prefix).min(MAX_PAYLOAD))
        }
        _ => stored.len() as u64,
    }
}

/// The frames read from the WAL that stage B has not taken yet, in LSN order.
#[derive(Debug)]
pub(crate) struct Window {
    slots: VecDeque<Slot>,
    /// The bytes charged to `normalize.cache_bytes`, with its gauge.
    bytes: u64,
    gauge: Gauge,
    cap: u64,
    /// The most frames held: twice the stage A share, so that a job waits for each permit that ends.
    depth: usize,
}

impl Window {
    /// An empty window under the budget `cap`, for `permits` stage A jobs at once.
    pub(crate) fn new(cap: u64, permits: u32, gauge: Gauge) -> Self {
        gauge.set(0);
        Self {
            slots: VecDeque::new(),
            bytes: 0,
            gauge,
            cap,
            depth: usize::try_from(permits.max(1))
                .unwrap_or(usize::MAX)
                .saturating_mul(2),
        }
    }

    /// The frames held.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// The LSN of the frame stage B takes next, when the window holds one.
    pub(crate) fn head_lsn(&self) -> Option<Lsn> {
        self.slots.front().map(|slot| slot.frame.lsn())
    }

    /// Whether to read another frame: always into an empty window, so a frame larger than the budget is read by
    /// itself, and otherwise while the window holds fewer frames than its depth and fewer bytes than the budget.
    pub(crate) fn wants_frame(&self) -> bool {
        self.slots.is_empty() || (self.slots.len() < self.depth && self.bytes < self.cap)
    }

    /// Whether stage A of the frame stage B takes next is still running.
    pub(crate) fn head_runs(&self) -> bool {
        matches!(
            self.slots.front(),
            Some(Slot {
                stage_a: StageA::Running(_),
                ..
            })
        )
    }

    fn set_bytes(&mut self, bytes: u64) {
        self.bytes = bytes;
        self.gauge.set(i64::try_from(bytes).unwrap_or(i64::MAX));
    }

    /// Adds the next frame of the log and starts its stage A when it carries rows: as a job when the frame is
    /// large and the log holds durable frames after it, otherwise on the task. A shipper that keeps up with the
    /// log reads each frame as the last durable one, so it runs stage A on its task, as one thread, and leaves
    /// the CPUs to ingest.
    pub(crate) fn push(&mut self, ctx: &Arc<Ctx>, frame: WalFrame, norm: NormCtx<'static>) {
        let bytes = charge(&frame);
        let behind = ctx.wal.durable_lsn() > frame.lsn();
        let stage_a = match frame.record_type() {
            RecordType::IngestRequest | RecordType::FactBatch
                if bytes <= INLINE_MAX_BYTES || !behind =>
            {
                StageA::Done(read_frame(&ctx.wal, &frame, &norm, &ctx.config))
            }
            RecordType::IngestRequest | RecordType::FactBatch => {
                let (job_ctx, job_frame) = (Arc::clone(ctx), frame.clone());
                StageA::Running(ctx.stage_a.run(&ctx.env, move || {
                    read_frame(&job_ctx.wal, &job_frame, &norm, &job_ctx.config)
                }))
            }
            _ => StageA::Passes,
        };
        self.set_bytes(self.bytes.saturating_add(bytes));
        self.slots.push_back(Slot {
            frame,
            stage_a,
            bytes,
        });
    }

    /// The frame stage B takes next, once its stage A is done.
    pub(crate) fn take_ready(&mut self) -> Result<Option<Ready>, ShipError> {
        let Some(head) = self.slots.front_mut() else {
            return Ok(None);
        };
        if let StageA::Running(pending) = &mut head.stage_a {
            match pending.try_take() {
                None => return Ok(None),
                Some(Ok(read)) => head.stage_a = StageA::Done(read),
                Some(Err(_)) => head.stage_a = StageA::Lost,
            }
        }
        let Some(slot) = self.slots.pop_front() else {
            return Ok(None);
        };
        self.set_bytes(self.bytes.saturating_sub(slot.bytes));
        let lsn = slot.frame.lsn();
        let read = match slot.stage_a {
            StageA::Passes => None,
            StageA::Done(read) => Some(read),
            StageA::Running(_) | StageA::Lost => {
                return Err(ShipError::JobLost {
                    stage: STAGE_A,
                    lsn,
                });
            }
        };
        Ok(Some((slot.frame, read)))
    }

    /// Resolves once stage A of the frame stage B takes next has ended; never while none runs. Cancel-safe.
    pub(crate) async fn settled(&mut self) {
        let Some(Slot {
            stage_a: stage_a @ StageA::Running(_),
            ..
        }) = self.slots.front_mut()
        else {
            return future::pending().await;
        };
        if let StageA::Running(pending) = stage_a {
            *stage_a = match pending.take().await {
                Ok(read) => StageA::Done(read),
                Err(_) => StageA::Lost,
            };
        }
    }

    /// Drops every frame held, for a read that starts again at the first of them. Their jobs that wait for a
    /// permit are withdrawn; those that run end with nobody taking their result.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.set_bytes(0);
    }
}
