//! What the shipper takes from its surroundings besides `Env` and `Stores`: crash barriers (contract 5.9) and the
//! recent-span index (contract 8.6).

use std::fmt;

use crucible_normalize::{Admission, NormalizedFrame};
use crucible_records::{Lsn, PartitionId, TableId};
use futures::future::{self, BoxFuture};

/// A crash barrier of the insert protocol (contract 5.9): a labelled point between two steps whose order
/// matters after a crash.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Barrier {
    /// The batch boundary is chosen; the `BatchSeal` frame is not appended yet.
    BeforeSealAppend,
    /// `seal.appended`: the `BatchSeal` frame is durable; no insert of the batch was sent.
    SealAppended,
    /// The insert of one pair is about to be sent.
    BeforeInsertSend(TableId, PartitionId),
    /// `insert.sent`: the insert of one pair returned, with whatever outcome.
    InsertSent(TableId, PartitionId),
    /// `quarantine.copied`: the copy is durable; the `Quarantine` frame is not appended yet.
    QuarantineCopied,
    /// `insert.confirmed` for all pairs: the batch is resolved; the `Checkpoint` frame is not appended yet.
    BeforeCheckpointAppend,
    /// `checkpoint.appended`: the `Checkpoint` frame is durable; garbage collection has not run.
    CheckpointAppended,
}

/// The crash-barrier hook. The shipper awaits [`Barriers::reached`] at every barrier.
///
/// Production uses [`NoBarriers`], whose future is ready at once. Simulation returns a future that never
/// completes at the barrier it wants to crash at, and aborts the host while the shipper waits there: the crash
/// then falls exactly between the two steps. The hook is a trait the shipper takes, not part of `Env`, because
/// no other crate has barriers of this protocol and a pending future is all a forced crash point needs.
pub trait Barriers: fmt::Debug + Send + Sync {
    /// Called when the shipper reaches `barrier`.
    fn reached(&self, barrier: Barrier) -> BoxFuture<'static, ()>;
}

/// The production barriers: every one is a no-op.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoBarriers;

impl Barriers for NoBarriers {
    fn reached(&self, _barrier: Barrier) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }
}

/// The recent-span index as the shipper sees it (contract 8.6). `crucible-recent` implements it; the shipper has
/// no dependency on that crate (contract 1.5).
pub trait RecentIndex: fmt::Debug + Send + Sync {
    /// Stage B applied this frame. Calls arrive in LSN order, starting with the retained prefix that recovery
    /// replays (contract 4.6, steps 6 and 7).
    fn applied(&self, frame: &NormalizedFrame, admission: &Admission);

    /// The lowest LSN the index still references, which pins WAL segments against garbage collection
    /// (contract 4.7).
    fn floor(&self) -> Option<Lsn>;
}

/// No recent-span index: nothing is fed and nothing is pinned.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRecent;

impl RecentIndex for NoRecent {
    fn applied(&self, _frame: &NormalizedFrame, _admission: &Admission) {}

    fn floor(&self) -> Option<Lsn> {
        None
    }
}
