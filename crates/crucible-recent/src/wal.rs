//! What the index needs from the WAL. `crucible-wal` does not depend on this crate, and re-reading a span needs
//! the normalizer, so the host implements this trait over both.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crucible_records::{Lsn, ProjectId, SpanRecord};

use crate::entry::WalLocator;

/// Why a span could not be re-read from the WAL.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum WalReadError {
    /// The segment is gone, or the frame at the locator does not hold the span. The store serves it by now.
    #[error("the span is no longer in the WAL")]
    Gone,
    /// The read or the normalization failed.
    #[error("the WAL read failed: {0}")]
    Failed(String),
}

/// The future of [`WalView::read_span`].
pub type SpanRead = Pin<Box<dyn Future<Output = Result<SpanRecord, WalReadError>> + Send>>;

/// The node's WAL as the index sees it.
pub trait WalView: fmt::Debug + Send + Sync {
    /// The highest LSN that is durable, which is the highest LSN acknowledged to a client.
    fn durable_lsn(&self) -> Lsn;

    /// Re-reads the frame at `locator` and normalizes the span at its ordinal (the normalizer is pure, so this
    /// is the record stage B built). The frame must belong to `project_id`.
    fn read_span(&self, project_id: ProjectId, locator: WalLocator) -> SpanRead;
}
