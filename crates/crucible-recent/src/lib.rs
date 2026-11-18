//! The recent-span index (contract 8.6): an in-memory index that makes a span readable within about 100 ms of
//! its ack and keeps it readable until ClickHouse serves it.
//!
//! - **Entries** hold what lists and trees need (ids, times, type, level, model, cost, tokens, interned name,
//!   user and session, the LSN and a [`WalLocator`]) and no payload: [`RecentIndex::span_record`] re-reads the
//!   frame through a [`WalView`], which the host implements over the WAL and the normalizer.
//! - **One writer**, the [`RecentWriter`] that stage B owns, applies [`Frame`]s in LSN order. Readers share 16
//!   shards behind read-write locks and always take a `ProjectScope`.
//! - **Eviction** removes an entry only when the checkpoint watermark has passed its LSN and that checkpoint is
//!   older than the visibility grace, so the store serves the span before the index forgets it.
//! - **The gap rule.** When `recent.max_bytes` is hit first (a store outage), the oldest entries are dropped and
//!   their projects get an open gap, which closes once the grace has covered the dropped LSNs. A quarantined
//!   pair marks its projects until it is requeued. Every read returns a [`ReadState`] taken after its entries,
//!   and the writer records a gap before it drops an entry, so a reader that misses an entry always sees why:
//!   the query layer answers `partial: true` instead of a silent miss (invariant I3).
//! - **The read barrier.** [`RecentIndex::read_barrier`] waits, at most 250 ms, until stage B has applied
//!   through the durable LSN; if it has not, the response is `consistency: lagging`.
//! - **Rebuild.** [`RecentIndex::rebuild`] replays the same [`Frame`]s after a restart.
//!
//! Time comes from the `Clock` the index is given; nothing here reads ambient state.

mod control;
mod entry;
mod index;
mod shard;
mod sync;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
mod tests;
mod wal;

pub use control::{QuarantineReach, QuarantinedPair, ReadState};
pub use entry::{RecentSpan, SpanEntry, SpanSummary, WalLocator};
pub use index::{Consistency, Frame, RecentConfig, RecentIndex, RecentStats, RecentWriter, SHARDS};
pub use shard::{DeletionSelector, ENTRY_BYTES};
pub use wal::{SpanRead, WalReadError, WalView};
