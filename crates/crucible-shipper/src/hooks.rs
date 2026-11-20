//! What the shipper takes from its surroundings besides `Env` and `Stores`: crash barriers (contract 5.9), the
//! recent-span index (contract 8.6), the project salts (contract 9.3) and the deletion requests (contract 13.3).

use std::fmt;

use crucible_normalize::{Admission, NormalizedFrame, ProjectSalt, Tombstone};
use crucible_records::{DeletionRequestId, DeletionState, Lsn, PartitionId, ProjectId, TableId};
use crucible_wal::WalFrame;
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

/// The recent-span index as the shipper sees it (contract 8.6). [`crate::RecentHook`] implements it over the
/// writer of `crucible-recent`.
///
/// Stage B reports every frame it reads, in LSN order, starting with the retained prefix that recovery replays
/// (contract 4.6, steps 6 and 7): an `IngestRequest` frame through [`RecentIndex::applied`] and every other
/// frame through [`RecentIndex::passed`].
pub trait RecentIndex: fmt::Debug + Send + Sync {
    /// Stage B applied the `IngestRequest` frame `source`, which normalized to `frame`.
    fn applied(&self, source: &WalFrame, frame: &NormalizedFrame, admission: &Admission);

    /// Stage B passed a frame that carries no spans. The index needs the `Checkpoint` and `Quarantine` frames
    /// (eviction, gap closure and the quarantine marks), and the LSN of every frame for its read barrier.
    fn passed(&self, source: &WalFrame);

    /// Stage B passed a frame that this binary cannot decode or normalize and that is quarantined as a frame
    /// (contract 5.6): its rows reach neither the index nor the store, so reads that could touch them must say
    /// so. `project` is the frame's project when its envelope decodes; `facts` tells a `FactBatch` frame from
    /// an `IngestRequest` frame. Without an override the frame passes like a control frame.
    fn unshippable(&self, source: &WalFrame, project: Option<ProjectId>, facts: bool) {
        let _ = (project, facts);
        self.passed(source);
    }

    /// The lowest LSN the index still references, which pins WAL segments against garbage collection
    /// (contract 4.7).
    fn floor(&self) -> Option<Lsn>;
}

/// Why a project's salt cannot be had now. The shipper holds the frame that needs it and asks again.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("the salt of the project is not available: {0}")]
pub struct SaltUnavailable(pub String);

/// Where the project salts come from (contracts 9.3 and 11.1): the control store's `project_salts`.
///
/// Stage B hashes a user subject with the salt of its project. A salt is random, immutable for the life of its
/// project and sealed at rest; it is created in the transaction that creates its project. The control store's
/// hook loads every salt at startup and again on a timer, so that it answers a project whose salt it has loaded
/// without a call to the store. The hook is a trait the shipper takes, as the barriers are, so that the shipper
/// does not depend on the control plane.
pub trait ProjectSalts: fmt::Debug + Send + Sync {
    /// Whether a control store holds salts. Without one (ingest-only mode) the shipper never asks for a salt:
    /// stage B drops the identity keys of every MCP operation, as `identity: none` in the sender would.
    fn configured(&self) -> bool;

    /// The salt of `project`. A project older than salts made with their project gets its salt here (insert if
    /// absent, then read again, so that two nodes agree on one salt). `None` for a deleted project without a
    /// salt: it never gets one, so stage B drops the user subjects of its frames.
    ///
    /// An error means the store did not answer, or the stored salt does not open: the frame that needs the salt
    /// waits and the shipper asks again, because a row hashed without its salt could not be told from one of
    /// another attempt. A hook that has loaded the project's salt never answers with an error.
    fn salt(
        &self,
        project: ProjectId,
    ) -> BoxFuture<'_, Result<Option<ProjectSalt>, SaltUnavailable>>;
}

/// A deletion request of the control store, as the shipper applies it (contract 13.3).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KnownDeletion {
    /// What the request deletes, with the issuer of a deletion by user.
    pub tombstone: Tombstone,
    /// Where the request stands.
    pub state: DeletionState,
}

/// Why the deletion requests cannot be read now. The shipper waits and asks again.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("the deletion requests are not available: {0}")]
pub struct DeletionsUnavailable(pub String);

/// Where the deletion requests come from (contract 13.3): the control store's `deletion_requests`.
///
/// Stage B drops what a deletion selects from the frames after the deletion's `Tombstone` frame. A request can
/// also complete while this node is down, and then this node appends no tombstone for it, so before it ships the
/// shipper reads every request in every state, after the node has registered in the node registry: a request made
/// after the read waits for this node's tombstone, because the node is live. The issuer of a deletion by user is
/// not in its tombstone (contract 13.6), so the shipper reads it here too. The hook is a trait the shipper takes,
/// as the salts are, so that the shipper does not depend on the control plane.
pub trait DeletionRequests: fmt::Debug + Send + Sync {
    /// Whether a control store holds deletion requests. Without one there are none, and the shipper reads none.
    fn configured(&self) -> bool;

    /// Registers the node, then reads every deletion request of the deployment, in every state.
    fn all(&self) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>>;

    /// The request with this id in `project`; `None` when there is none.
    fn request(
        &self,
        project: ProjectId,
        id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>>;
}

/// No control store: no deletion requests.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDeletions;

impl DeletionRequests for NoDeletions {
    fn configured(&self) -> bool {
        false
    }

    fn all(&self) -> BoxFuture<'_, Result<Vec<KnownDeletion>, DeletionsUnavailable>> {
        Box::pin(future::ready(Ok(Vec::new())))
    }

    fn request(
        &self,
        _project: ProjectId,
        _id: DeletionRequestId,
    ) -> BoxFuture<'_, Result<Option<KnownDeletion>, DeletionsUnavailable>> {
        Box::pin(future::ready(Ok(None)))
    }
}

/// No control store: ingest-only mode.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSalts;

impl ProjectSalts for NoSalts {
    fn configured(&self) -> bool {
        false
    }

    fn salt(
        &self,
        _project: ProjectId,
    ) -> BoxFuture<'_, Result<Option<ProjectSalt>, SaltUnavailable>> {
        Box::pin(future::ready(Err(SaltUnavailable(
            "no control store is configured".to_owned(),
        ))))
    }
}

/// No recent-span index: nothing is fed and nothing is pinned.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoRecent;

impl RecentIndex for NoRecent {
    fn applied(&self, _source: &WalFrame, _frame: &NormalizedFrame, _admission: &Admission) {}

    fn passed(&self, _source: &WalFrame) {}

    fn floor(&self) -> Option<Lsn> {
        None
    }
}
