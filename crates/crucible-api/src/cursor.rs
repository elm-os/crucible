//! Cursors on the wire (contract 12.4): the cursor values of `crucible-query`, signed by `crucible-cursor` as
//! `v1.<kid>.<payload>.<mac>` with the node's key ring, bound to the request's project, list, filters, sort and
//! window, and expiring 24 h after issue. A cursor that does not open for the request is `cursor_invalid`.
//!
//! Canonical filter forms, which the filter hash covers: the trace list and the span list hash the canonical
//! bytes of their list filter (`Filter::canonical_bytes`, empty without a filter); the children of a span are
//! selected by the trace and the parent span, so their 24 raw bytes are the form; a search is selected by its
//! mode, fields and tokens (`SearchQuery::canonical`); the session and user lists take no filter, so their
//! form is empty; the sessions of a user are selected by the user, so the bytes of its id are the form; the MCP
//! operation list hashes its side, caller-fault rule, failed-only flag, error class, capture source, server and
//! text filters (`mcp_operation_filters`).

use crucible_cursor::{
    Binding, Cursor, CursorError, CursorId, FilterHash, IdList, KeyRing, Kind, Position, Route,
    Sort,
};
use crucible_plan::Filter;
use crucible_query::{
    ChildCursor, HeadCursor, IdCursor, McpOperationCursor, McpOperationsQuery, PageCursor, SearchCursor,
    SearchQuery, SpanListCursor,
};
use crucible_records::{ProjectId, SpanId, TimeWindow, Timestamp, TraceId};

/// The order of the trace list, the span list and the lists keyed by a text id.
const TRACE_SORT: Sort = Sort::NewestFirst;
/// The order of the children of a span.
const CHILD_SORT: Sort = Sort::OldestFirst;
/// The order of search hits.
const SEARCH_SORT: Sort = Sort::NewestFirst;

fn list_filters(filter: &Filter) -> FilterHash {
    FilterHash::from_bytes(filter.hash())
}

/// The filter form of a list keyed by a text id: the user of the sessions of a user, nothing for the others.
fn id_filters(list: IdList, scope_id: Option<&str>) -> FilterHash {
    match (list, scope_id) {
        (IdList::UserSessions, Some(user_id)) => FilterHash::of(user_id.as_bytes()),
        _ => FilterHash::of(&[]),
    }
}

fn child_filters(trace_id: TraceId, parent: SpanId) -> FilterHash {
    let mut canonical = [0; 24];
    canonical[..16].copy_from_slice(trace_id.as_bytes());
    canonical[16..].copy_from_slice(parent.as_bytes());
    FilterHash::of(&canonical)
}

/// The order of the MCP operation list.
const MCP_SORT: Sort = Sort::NewestFirst;

/// The canonical form of an MCP operation-list request: the side, the capture-source choice, the caller-fault
/// rule, then every optional text as a presence byte and a length-prefixed value, in a fixed order.
fn mcp_operation_filters(query: &McpOperationsQuery) -> FilterHash {
    let (q, f) = (&query.query, &query.filter);
    let mut canonical = vec![
        q.side.value(),
        u8::from(q.include_caller_faults),
        u8::from(f.errors_only),
        f.error_class.map_or(u8::MAX, |c| c.value()),
    ];
    let texts = [
        Some(q.source.as_str()),
        q.server.as_deref(),
        f.target.as_deref(),
        f.method.as_deref(),
        f.client.as_deref(),
        f.session.as_deref(),
        f.user.as_deref(),
    ];
    for text in texts {
        match text {
            Some(text) => {
                canonical.push(1);
                canonical.extend_from_slice(&(text.len() as u64).to_be_bytes());
                canonical.extend_from_slice(text.as_bytes());
            }
            None => canonical.push(0),
        }
    }
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

    /// A trace-list page cursor, bound to the list's filter.
    pub(crate) fn seal_page(&self, cursor: &PageCursor, filter: &Filter) -> String {
        let (_, window, as_of, last) = cursor.parts();
        let position = Position::TracePage {
            window,
            as_of,
            last,
        };
        self.seal(list_filters(filter), TRACE_SORT, position)
    }

    /// A live-tail head cursor, bound to the list's filter.
    pub(crate) fn seal_head(&self, cursor: &HeadCursor, filter: &Filter) -> String {
        let (_, window, as_of, wal_epoch, lsn) = cursor.parts();
        let position = Position::TraceHead {
            window,
            as_of,
            wal_epoch,
            lsn,
        };
        self.seal(list_filters(filter), TRACE_SORT, position)
    }

    /// A span-list page cursor, bound to the list's filter.
    pub(crate) fn seal_span_page(&self, cursor: &SpanListCursor, filter: &Filter) -> String {
        let (_, window, as_of, last) = cursor.parts();
        let position = Position::SpanPage {
            window,
            as_of,
            last,
        };
        self.seal(list_filters(filter), TRACE_SORT, position)
    }

    /// A page cursor of a list keyed by a text id; `scope_id` is the user of the sessions of a user. `None`
    /// when the last row's id is longer than a cursor holds, which no stored id is (contract 3.1).
    pub(crate) fn seal_id_page(
        &self,
        list: IdList,
        scope_id: Option<&str>,
        cursor: &IdCursor,
    ) -> Option<String> {
        let (_, window, (last_seen, id)) = cursor.parts();
        let position = Position::IdPage {
            list,
            window,
            last: (last_seen, CursorId::new(id.as_bytes())?),
        };
        Some(self.seal(id_filters(list, scope_id), TRACE_SORT, position))
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

    /// The page cursor of a trace-list request over `window` with `filter`.
    pub(crate) fn open_page(
        &self,
        text: &str,
        window: TimeWindow,
        filter: &Filter,
    ) -> Result<PageCursor, CursorError> {
        let list = (Route::TraceList, Kind::Page);
        let cursor = self.open(text, list, list_filters(filter), TRACE_SORT, Some(window))?;
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

    /// The head cursor of a live-tail request over `window` with `filter`.
    pub(crate) fn open_head(
        &self,
        text: &str,
        window: TimeWindow,
        filter: &Filter,
    ) -> Result<HeadCursor, CursorError> {
        let list = (Route::TraceList, Kind::Head);
        let cursor = self.open(text, list, list_filters(filter), TRACE_SORT, Some(window))?;
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

    /// An MCP operation-list cursor, bound to the list's side, capture source, caller-fault rule, server and
    /// filters.
    pub(crate) fn seal_mcp_operations(
        &self,
        query: &McpOperationsQuery,
        next: McpOperationCursor,
    ) -> String {
        let position = Position::McpOperationPage {
            window: query.query.window,
            as_of: next.as_of,
            last: next.last,
        };
        self.seal(mcp_operation_filters(query), MCP_SORT, position)
    }

    /// The cursor of an MCP operation-list request, whose window and filters it must share.
    pub(crate) fn open_mcp_operations(
        &self,
        text: &str,
        query: &McpOperationsQuery,
    ) -> Result<McpOperationCursor, CursorError> {
        let list = (Route::McpOperations, Kind::Page);
        let filters = mcp_operation_filters(query);
        let cursor = self.open(text, list, filters, MCP_SORT, Some(query.query.window))?;
        match cursor.position {
            Position::McpOperationPage { as_of, last, .. } => {
                Ok(McpOperationCursor { as_of, last })
            }
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The page cursor of a span-list request over `window` with `filter`.
    pub(crate) fn open_span_page(
        &self,
        text: &str,
        window: TimeWindow,
        filter: &Filter,
    ) -> Result<SpanListCursor, CursorError> {
        let list = (Route::SpanList, Kind::Page);
        let cursor = self.open(text, list, list_filters(filter), TRACE_SORT, Some(window))?;
        match cursor.position {
            Position::SpanPage {
                window,
                as_of,
                last,
            } => Ok(SpanListCursor::from_parts(
                cursor.project_id,
                window,
                as_of,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The page cursor of a request for a list keyed by a text id over `window`; `scope_id` is the user of
    /// the sessions of a user. An id that is not UTF-8 never came from these lists, so it is malformed.
    pub(crate) fn open_id_page(
        &self,
        text: &str,
        list: IdList,
        scope_id: Option<&str>,
        window: TimeWindow,
    ) -> Result<IdCursor, CursorError> {
        let route = (list.route(), Kind::Page);
        let filters = id_filters(list, scope_id);
        let cursor = self.open(text, route, filters, TRACE_SORT, Some(window))?;
        match cursor.position {
            Position::IdPage {
                window,
                last: (last_seen, id),
                ..
            } => {
                let id = String::from_utf8(id.as_bytes().to_vec())
                    .map_err(|_| CursorError::Malformed)?;
                Ok(IdCursor::from_parts(
                    cursor.project_id,
                    window,
                    (last_seen, id),
                ))
            }
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
    use crucible_plan::TextField;
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
            user in ".{1,40}",
        ) {
            let ring = ring(secret);
            let project = ProjectId::from_bytes(project);
            let cursors = Cursors { ring: &ring, project_id: project, now: Timestamp::from_micros(from) };
            let (trace, span, parent) =
                (TraceId::from_bytes(trace), SpanId::from_bytes(span), SpanId::from_bytes(parent));
            let (window, as_of, start) =
                (window(from, len), Timestamp::from_micros(as_of), Timestamp::from_micros(start));
            let all = Filter::all();
            let page = PageCursor::from_parts(project, window, as_of, (start, trace.trace_hash(), trace));
            let head = HeadCursor::from_parts(project, window, as_of, WalEpoch::from_bytes(epoch), Lsn::new(lsn));
            let child = ChildCursor::from_parts(project, trace, parent, (start, span));
            let (page_text, head_text, child_text) =
                (cursors.seal_page(&page, &all), cursors.seal_head(&head, &all), cursors.seal_child(&child));
            prop_assert_eq!(cursors.open_page(&page_text, window, &all), Ok(page));
            prop_assert_eq!(cursors.open_head(&head_text, window, &all), Ok(head));
            prop_assert_eq!(cursors.open_child(&child_text, trace, parent), Ok(child));
            prop_assert!(page_text.starts_with("v1.k."));
            // A cursor never opens as another kind.
            prop_assert_eq!(cursors.open_head(&page_text, window, &all), Err(CursorError::WrongKind));
            prop_assert_eq!(cursors.open_page(&head_text, window, &all), Err(CursorError::WrongKind));
            prop_assert_eq!(cursors.open_page(&child_text, window, &all), Err(CursorError::WrongRoute));
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

            // List cursors are bound to the list filter's canonical form: an equivalent filter opens them, any
            // other does not.
            let by_user = Filter::Text(TextField::UserId, vec![user.clone()]);
            let shuffled = Filter::And(vec![by_user.clone(), Filter::all()]);
            let filtered_page = cursors.seal_page(&page, &by_user);
            prop_assert_eq!(cursors.open_page(&filtered_page, window, &shuffled), Ok(page));
            prop_assert_eq!(cursors.open_page(&filtered_page, window, &all), Err(CursorError::FiltersChanged));
            prop_assert_eq!(cursors.open_page(&page_text, window, &by_user), Err(CursorError::FiltersChanged));
            let filtered_head = cursors.seal_head(&head, &by_user);
            prop_assert_eq!(cursors.open_head(&filtered_head, window, &all), Err(CursorError::FiltersChanged));

            let spans = SpanListCursor::from_parts(project, window, as_of, (start, span));
            let spans_text = cursors.seal_span_page(&spans, &by_user);
            prop_assert_eq!(cursors.open_span_page(&spans_text, window, &by_user), Ok(spans));
            prop_assert_eq!(cursors.open_span_page(&spans_text, window, &all), Err(CursorError::FiltersChanged));
            prop_assert_eq!(cursors.open_page(&spans_text, window, &by_user), Err(CursorError::WrongRoute));

            // The lists keyed by a text id: each opens only as its own list, and the sessions of a user only for
            // that user.
            let id = IdCursor::from_parts(project, window, (start, user.clone()));
            for list in [IdList::Sessions, IdList::Users, IdList::UserSessions] {
                let scope_id = (list == IdList::UserSessions).then_some(user.as_str());
                let text = cursors.seal_id_page(list, scope_id, &id).unwrap();
                prop_assert_eq!(cursors.open_id_page(&text, list, scope_id, window), Ok(id.clone()));
                for other in [IdList::Sessions, IdList::Users, IdList::UserSessions] {
                    if other != list {
                        let scope = (other == IdList::UserSessions).then_some(user.as_str());
                        prop_assert_eq!(
                            cursors.open_id_page(&text, other, scope, window),
                            Err(CursorError::WrongRoute)
                        );
                    }
                }
            }
            let mine = cursors.seal_id_page(IdList::UserSessions, Some(&user), &id).unwrap();
            let someone = format!("{user}-else");
            prop_assert_eq!(
                cursors.open_id_page(&mine, IdList::UserSessions, Some(&someone), window),
                Err(CursorError::FiltersChanged)
            );
            // An id longer than a cursor holds cannot be sealed.
            let long = IdCursor::from_parts(project, window, (start, "x".repeat(513)));
            prop_assert_eq!(cursors.seal_id_page(IdList::Sessions, None, &long), None);
        }
    }
}
