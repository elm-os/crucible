//! Cursors on the wire (contract 12.4): the cursor values of `crucible-query`, signed by `crucible-cursor` as
//! `v1.<kid>.<payload>.<mac>` with the node's key ring, bound to the request's project, list, filters, sort and
//! window, and expiring 24 h after issue. A cursor that does not open for the request is `cursor_invalid`.
//!
//! Canonical filter forms, which the filter hash covers: the trace list takes no filter parameters yet, so its
//! form is empty; the children of a span are selected by the trace and the parent span, so their 24 raw bytes
//! are the form; a search is selected by its mode, fields and tokens (`SearchQuery::canonical`).

use crucible_cursor::{
    Binding, Cursor, CursorError, FilterHash, KeyRing, Kind, Position, Route, Sort,
};
use crucible_query::{ChildCursor, HeadCursor, PageCursor, SearchCursor, SearchQuery};
use crucible_records::{ProjectId, SpanId, TimeWindow, Timestamp, TraceId};

/// The order of the trace list.
const TRACE_SORT: Sort = Sort::NewestFirst;
/// The order of the children of a span.
const CHILD_SORT: Sort = Sort::OldestFirst;
/// The order of search hits.
const SEARCH_SORT: Sort = Sort::NewestFirst;

fn trace_filters() -> FilterHash {
    FilterHash::of(&[])
}

fn child_filters(trace_id: TraceId, parent: SpanId) -> FilterHash {
    let mut canonical = [0; 24];
    canonical[..16].copy_from_slice(trace_id.as_bytes());
    canonical[16..].copy_from_slice(parent.as_bytes());
    FilterHash::of(&canonical)
}

/// The cursors of one request: the node's key ring, the request's project and the instant it is served at.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursors<'a> {
    pub(crate) ring: &'a KeyRing,
    pub(crate) project_id: ProjectId,
    pub(crate) now: Timestamp,
}

impl Cursors<'_> {
    fn seal(&self, filter: FilterHash, sort: Sort, position: Position) -> String {
        let cursor = Cursor::new(self.project_id, filter, sort, position, self.now);
        self.ring.seal(&cursor)
    }

    fn open(
        &self,
        text: &str,
        (route, kind): (Route, Kind),
        filter: FilterHash,
        sort: Sort,
        window: Option<TimeWindow>,
    ) -> Result<Cursor, CursorError> {
        let binding = Binding {
            project_id: self.project_id,
            route,
            kind,
            filter,
            sort,
            window,
        };
        self.ring.open(text, &binding, self.now)
    }

    /// A trace-list page cursor.
    pub(crate) fn seal_page(&self, cursor: &PageCursor) -> String {
        let (_, window, as_of, last) = cursor.parts();
        let position = Position::TracePage {
            window,
            as_of,
            last,
        };
        self.seal(trace_filters(), TRACE_SORT, position)
    }

    /// A live-tail head cursor.
    pub(crate) fn seal_head(&self, cursor: &HeadCursor) -> String {
        let (_, window, as_of, wal_epoch, lsn) = cursor.parts();
        let position = Position::TraceHead {
            window,
            as_of,
            wal_epoch,
            lsn,
        };
        self.seal(trace_filters(), TRACE_SORT, position)
    }

    /// A span-children cursor.
    pub(crate) fn seal_child(&self, cursor: &ChildCursor) -> String {
        let (_, trace_id, parent, last) = cursor.parts();
        let position = Position::ChildPage {
            trace_id,
            parent,
            last,
        };
        self.seal(child_filters(trace_id, parent), CHILD_SORT, position)
    }

    /// A search cursor, bound to the search's mode, fields and tokens.
    pub(crate) fn seal_search(&self, cursor: &SearchCursor, query: &SearchQuery) -> String {
        let (_, window, as_of, last) = cursor.parts();
        let position = Position::SearchPage {
            window,
            as_of,
            last,
        };
        self.seal(FilterHash::of(&query.canonical()), SEARCH_SORT, position)
    }

    /// The cursor of `query`, whose window it must share.
    pub(crate) fn open_search(
        &self,
        text: &str,
        query: &SearchQuery,
    ) -> Result<SearchCursor, CursorError> {
        let list = (Route::Search, Kind::Page);
        let filters = FilterHash::of(&query.canonical());
        let cursor = self.open(text, list, filters, SEARCH_SORT, Some(query.window))?;
        match cursor.position {
            Position::SearchPage {
                window,
                as_of,
                last,
            } => Ok(SearchCursor::from_parts(
                cursor.project_id,
                window,
                as_of,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The page cursor of a trace-list request over `window`.
    pub(crate) fn open_page(
        &self,
        text: &str,
        window: TimeWindow,
    ) -> Result<PageCursor, CursorError> {
        let list = (Route::TraceList, Kind::Page);
        let cursor = self.open(text, list, trace_filters(), TRACE_SORT, Some(window))?;
        match cursor.position {
            Position::TracePage {
                window,
                as_of,
                last,
            } => Ok(PageCursor::from_parts(
                cursor.project_id,
                window,
                as_of,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The head cursor of a live-tail request over `window`.
    pub(crate) fn open_head(
        &self,
        text: &str,
        window: TimeWindow,
    ) -> Result<HeadCursor, CursorError> {
        let list = (Route::TraceList, Kind::Head);
        let cursor = self.open(text, list, trace_filters(), TRACE_SORT, Some(window))?;
        match cursor.position {
            Position::TraceHead {
                window,
                as_of,
                wal_epoch,
                lsn,
            } => Ok(HeadCursor::from_parts(
                cursor.project_id,
                window,
                as_of,
                wal_epoch,
                lsn,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The cursor of a request for the children of `parent` in `trace_id`.
    pub(crate) fn open_child(
        &self,
        text: &str,
        trace_id: TraceId,
        parent: SpanId,
    ) -> Result<ChildCursor, CursorError> {
        let list = (Route::SpanChildren, Kind::Page);
        let filters = child_filters(trace_id, parent);
        let cursor = self.open(text, list, filters, CHILD_SORT, None)?;
        match cursor.position {
            Position::ChildPage {
                trace_id,
                parent,
                last,
            } => Ok(ChildCursor::from_parts(
                cursor.project_id,
                trace_id,
                parent,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_cursor::{Kid, SigningKey};
    use crucible_records::{Lsn, WalEpoch};
    use proptest::prelude::*;

    use super::*;

    fn ring(secret: [u8; 32]) -> KeyRing {
        KeyRing::new(SigningKey::new(Kid::new("k").unwrap(), secret))
    }

    fn window(from: i64, len: i64) -> TimeWindow {
        TimeWindow::new(
            Timestamp::from_micros(from),
            Timestamp::from_micros(from + len),
        )
        .unwrap()
    }

    proptest! {
        #[test]
        fn cursors_round_trip_and_open_only_for_their_own_request(
            secret: [u8; 32],
            project: [u8; 16],
            trace: [u8; 16],
            span: [u8; 8],
            parent: [u8; 8],
            epoch: [u8; 16],
            from in 0i64..4_000_000_000_000_000,
            len in 1i64..100_000_000_000,
            as_of: i64,
            start: i64,
            lsn: u64,
        ) {
            let ring = ring(secret);
            let project = ProjectId::from_bytes(project);
            let cursors = Cursors { ring: &ring, project_id: project, now: Timestamp::from_micros(from) };
            let (trace, span, parent) =
                (TraceId::from_bytes(trace), SpanId::from_bytes(span), SpanId::from_bytes(parent));
            let (window, as_of, start) =
                (window(from, len), Timestamp::from_micros(as_of), Timestamp::from_micros(start));
            let page = PageCursor::from_parts(project, window, as_of, (start, trace.trace_hash(), trace));
            let head = HeadCursor::from_parts(project, window, as_of, WalEpoch::from_bytes(epoch), Lsn::new(lsn));
            let child = ChildCursor::from_parts(project, trace, parent, (start, span));
            let (page_text, head_text, child_text) =
                (cursors.seal_page(&page), cursors.seal_head(&head), cursors.seal_child(&child));
            prop_assert_eq!(cursors.open_page(&page_text, window), Ok(page));
            prop_assert_eq!(cursors.open_head(&head_text, window), Ok(head));
            prop_assert_eq!(cursors.open_child(&child_text, trace, parent), Ok(child));
            prop_assert!(page_text.starts_with("v1.k."));
            // A cursor never opens as another kind.
            prop_assert_eq!(cursors.open_head(&page_text, window), Err(CursorError::WrongKind));
            prop_assert_eq!(cursors.open_page(&head_text, window), Err(CursorError::WrongKind));
            prop_assert_eq!(cursors.open_page(&child_text, window), Err(CursorError::WrongRoute));
            prop_assert_eq!(
                cursors.open_child(&page_text, trace, parent),
                Err(CursorError::WrongRoute)
            );
            // A children cursor is bound to its trace and parent.
            let other = SpanId::from_bytes(parent.to_bytes().map(|byte| !byte));
            prop_assert_eq!(
                cursors.open_child(&child_text, trace, other),
                Err(CursorError::FiltersChanged)
            );
        }
    }
}
