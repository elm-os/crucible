//! Where the fact routes write (contract 3.8, 4.4 and 4.5): a [`FactWriter`] appends `FactBatch` frames to the
//! WAL and answers once they are durable. The binary implements it over `crucible_ingest::Ingest::append_facts`;
//! this crate has no dependency edge to `crucible-ingest`, so the seam carries its own copies of the write's
//! types.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crucible_records::{
    AuthorKind, FactBody, FactId, FactTargetKind, ProjectScope, SpanId, Timestamp, TraceId, Uuid,
};

/// One fact as a route hands it in; the writer adds the id, the version and the creation time.
#[derive(Clone, PartialEq, Debug)]
pub struct NewFact {
    /// What the fact is attached to.
    pub target_kind: FactTargetKind,
    /// The target trace, or zero for a session.
    pub trace_id: TraceId,
    /// The target span, or zero.
    pub span_id: SpanId,
    /// The target session, or empty.
    pub session_id: Box<str>,
    /// The target's earliest known start, which files the fact.
    pub target_time: Timestamp,
    /// What latest-wins resolves over.
    pub key: Box<str>,
    /// Whether the fact retracts its key.
    pub retracted: bool,
    /// The body, which names the fact table.
    pub body: FactBody,
}

/// Who writes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FactAuthor {
    /// The kind of author.
    pub kind: AuthorKind,
    /// The user or API key.
    pub id: Uuid,
}

/// What the writer assigned to a durable fact.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AcceptedFact {
    /// The fact's id.
    pub fact_id: FactId,
    /// The fact's version: the server's microsecond clock at acceptance.
    pub version: u64,
    /// When the fact was accepted.
    pub created_at: Timestamp,
}

/// Why a write was refused. Nothing of a refused write is acknowledged.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FactRefusal {
    /// A fact is not well formed (`validation_failed`).
    Invalid(&'static str),
    /// A fact is above a size cap (`payload_too_large`).
    TooLarge(&'static str),
    /// The in-flight byte budget or the project's byte rate is exhausted (`rate_limited`).
    Throttled {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
    },
    /// The WAL cannot take the write or make it durable (`ingest_unavailable`).
    Unavailable {
        /// Seconds for `Retry-After`.
        retry_after_s: u32,
    },
    /// A failure the write did not cause (`internal_error`).
    Internal(&'static str),
}

/// The future of one append.
pub type FactAppend<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<AcceptedFact>, FactRefusal>> + Send + 'a>>;

/// Appends facts of one author to one project and resolves once every frame is durable.
pub trait FactWriter: fmt::Debug + Send + Sync {
    /// Appends `facts`, in order; the result lists what was assigned, in the same order.
    fn append(
        &self,
        scope: ProjectScope,
        author: FactAuthor,
        facts: Vec<NewFact>,
    ) -> FactAppend<'_>;
}
