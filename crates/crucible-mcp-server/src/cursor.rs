//! The tools' cursors (contract 12.4): signed with the node's key ring as `v1.<kid>.<payload>.<mac>`, bound to
//! the key's project, the tool's list, its arguments and its window, and expiring 24 hours after issue.
//!
//! They reuse the positions and canonical filter forms of the REST lists, so the codec needs no route of its own:
//!
//! - `list_traces` and `list_spans`: the page positions of the trace list and the span list, bound to the hash
//!   of the list filter (the empty form without one).
//! - `list_sessions`, `list_users` and `list_user_sessions`: the id-page position of their list, bound to the
//!   empty form, or for the sessions of a user to the bytes of the user id.
//! - `get_span_children`: the span-children position, bound to the trace and the parent span (24 bytes).
//! - `get_trace`: a page inside one trace is the span-children layout with the zero span id as the parent, bound
//!   to its own canonical form, `get_trace` followed by the trace id, so it opens for `get_trace` of that trace
//!   only, and never as a children cursor.

use crucible_cursor::{
    Binding, Cursor, CursorError, CursorId, FilterHash, IdList, KeyRing, Kind, Position, Route,
    Sort,
};
use crucible_plan::Filter;
use crucible_query::{ChildCursor, HeadKey, IdCursor, PageCursor, SpanListCursor};
use crucible_records::{ProjectId, SpanId, TimeWindow, Timestamp, TraceId};

/// The order of the trace list, the span list and the lists keyed by a text id.
const TRACE_SORT: Sort = Sort::NewestFirst;
/// The order of spans: start time, then span id.
const SPAN_SORT: Sort = Sort::OldestFirst;

fn list_filters(filter: &Filter) -> FilterHash {
    FilterHash::from_bytes(filter.hash())
}

/// The filter form of a list keyed by a text id: the user of the sessions of a user, nothing for the others.
fn id_filters(list: IdList, user_id: Option<&str>) -> FilterHash {
    match (list, user_id) {
        (IdList::UserSessions, Some(user_id)) => FilterHash::of(user_id.as_bytes()),
        _ => FilterHash::of(&[]),
    }
}

fn children_filters(trace_id: TraceId, parent: SpanId) -> FilterHash {
    let mut canonical = [0; 24];
    canonical[..16].copy_from_slice(trace_id.as_bytes());
    canonical[16..].copy_from_slice(parent.as_bytes());
    FilterHash::of(&canonical)
}

fn trace_spans_filters(trace_id: TraceId) -> FilterHash {
    const TOOL: &[u8] = b"get_trace";
    let mut canonical = [0; TOOL.len() + 16];
    canonical[..TOOL.len()].copy_from_slice(TOOL);
    canonical[TOOL.len()..].copy_from_slice(trace_id.as_bytes());
    FilterHash::of(&canonical)
}

/// The cursors of one tool call: the node's key ring, the key's project and the instant of the call.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Cursors<'a> {
    pub(crate) ring: &'a KeyRing,
    pub(crate) project_id: ProjectId,
    pub(crate) now: Timestamp,
}

impl Cursors<'_> {
    /// A page cursor at `position`, bound to the call's project and to `filter` and `sort`. A tool that pages
    /// a list of its own seals with this and [`Cursors::open`], reusing the REST list's route, position and
    /// canonical filter form.
    pub(crate) fn seal(&self, filter: FilterHash, sort: Sort, position: Position) -> String {
        self.ring.seal(&Cursor::new(
            self.project_id,
            filter,
            sort,
            position,
            self.now,
        ))
    }

    /// The position of a page cursor that opens for this call's project, `route`, `filter`, `sort` and
    /// `window`.
    pub(crate) fn open(
        &self,
        text: &str,
        route: Route,
        filter: FilterHash,
        sort: Sort,
        window: Option<TimeWindow>,
    ) -> Result<Position, CursorError> {
        let binding = Binding {
            project_id: self.project_id,
            route,
            kind: Kind::Page,
            filter,
            sort,
            window,
        };
        Ok(self.ring.open(text, &binding, self.now)?.position)
    }

    /// The cursor of the trace-list page after `last`, for the list under `filter`.
    pub(crate) fn seal_traces(
        &self,
        window: TimeWindow,
        as_of: Timestamp,
        last: HeadKey,
        filter: &Filter,
    ) -> String {
        let position = Position::TracePage {
            window,
            as_of,
            last,
        };
        self.seal(list_filters(filter), TRACE_SORT, position)
    }

    /// A trace-list page cursor for a request over `window` under `filter`.
    pub(crate) fn open_traces(
        &self,
        text: &str,
        window: TimeWindow,
        filter: &Filter,
    ) -> Result<PageCursor, CursorError> {
        let filters = list_filters(filter);
        match self.open(text, Route::TraceList, filters, TRACE_SORT, Some(window))? {
            Position::TracePage {
                window,
                as_of,
                last,
            } => Ok(PageCursor::from_parts(self.project_id, window, as_of, last)),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The cursor of the span-list page after `last`, for the list under `filter`.
    pub(crate) fn seal_spans(
        &self,
        window: TimeWindow,
        as_of: Timestamp,
        last: (Timestamp, SpanId),
        filter: &Filter,
    ) -> String {
        let position = Position::SpanPage {
            window,
            as_of,
            last,
        };
        self.seal(list_filters(filter), TRACE_SORT, position)
    }

    /// A span-list page cursor for a request over `window` under `filter`.
    pub(crate) fn open_spans(
        &self,
        text: &str,
        window: TimeWindow,
        filter: &Filter,
    ) -> Result<SpanListCursor, CursorError> {
        let filters = list_filters(filter);
        match self.open(text, Route::SpanList, filters, TRACE_SORT, Some(window))? {
            Position::SpanPage {
                window,
                as_of,
                last,
            } => Ok(SpanListCursor::from_parts(
                self.project_id,
                window,
                as_of,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The cursor of the page of `list` after the row `(last_seen, id)`; `user_id` is the user of the sessions
    /// of a user. `None` when the id is longer than a cursor holds, which no stored id is (contract 3.1).
    pub(crate) fn seal_ids(
        &self,
        list: IdList,
        user_id: Option<&str>,
        window: TimeWindow,
        (last_seen, id): (Timestamp, &str),
    ) -> Option<String> {
        let position = Position::IdPage {
            list,
            window,
            last: (last_seen, CursorId::new(id.as_bytes())?),
        };
        Some(self.seal(id_filters(list, user_id), TRACE_SORT, position))
    }

    /// A page cursor of `list` for a request over `window`; `user_id` is the user of the sessions of a user.
    /// An id that is not UTF-8 never came from these lists, so it is malformed.
    pub(crate) fn open_ids(
        &self,
        text: &str,
        list: IdList,
        user_id: Option<&str>,
        window: TimeWindow,
    ) -> Result<IdCursor, CursorError> {
        let filters = id_filters(list, user_id);
        match self.open(text, list.route(), filters, TRACE_SORT, Some(window))? {
            Position::IdPage {
                list: of,
                window,
                last: (last_seen, id),
            } if of == list => {
                let id = String::from_utf8(id.as_bytes().to_vec())
                    .map_err(|_| CursorError::Malformed)?;
                Ok(IdCursor::from_parts(
                    self.project_id,
                    window,
                    (last_seen, id),
                ))
            }
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The cursor of the children of `parent` after `last`.
    pub(crate) fn seal_children(
        &self,
        trace_id: TraceId,
        parent: SpanId,
        last: (Timestamp, SpanId),
    ) -> String {
        let position = Position::ChildPage {
            trace_id,
            parent,
            last,
        };
        self.seal(children_filters(trace_id, parent), SPAN_SORT, position)
    }

    /// A children cursor for a request for the children of `parent` in `trace_id`.
    pub(crate) fn open_children(
        &self,
        text: &str,
        trace_id: TraceId,
        parent: SpanId,
    ) -> Result<ChildCursor, CursorError> {
        let filters = children_filters(trace_id, parent);
        match self.open(text, Route::SpanChildren, filters, SPAN_SORT, None)? {
            Position::ChildPage {
                trace_id,
                parent,
                last,
            } => Ok(ChildCursor::from_parts(
                self.project_id,
                trace_id,
                parent,
                last,
            )),
            _ => Err(CursorError::WrongKind),
        }
    }

    /// The cursor of the spans of `trace_id` after `last`.
    pub(crate) fn seal_trace(&self, trace_id: TraceId, last: (Timestamp, SpanId)) -> String {
        let position = Position::ChildPage {
            trace_id,
            parent: SpanId::ZERO,
            last,
        };
        self.seal(trace_spans_filters(trace_id), SPAN_SORT, position)
    }

    /// The `(start_time, span_id)` after which a `get_trace` page of `trace_id` resumes.
    pub(crate) fn open_trace(
        &self,
        text: &str,
        trace_id: TraceId,
    ) -> Result<(Timestamp, SpanId), CursorError> {
        let filters = trace_spans_filters(trace_id);
        match self.open(text, Route::SpanChildren, filters, SPAN_SORT, None)? {
            Position::ChildPage {
                trace_id: of,
                parent,
                last,
            } if of == trace_id && parent == SpanId::ZERO => Ok(last),
            _ => Err(CursorError::WrongKind),
        }
    }
}

#[cfg(test)]
mod tests {
    use crucible_cursor::{Kid, SigningKey};
    use proptest::prelude::*;

    use super::*;

    proptest! {
        /// Each kind of cursor opens for its own request only: the same project, list, arguments and window.
        #[test]
        fn cursors_open_only_for_their_own_request(
            secret: [u8; 32],
            project: [u8; 16],
            trace: [u8; 16],
            other_trace: [u8; 16],
            parent: [u8; 8],
            span: [u8; 8],
            from in 0i64..4_000_000_000_000_000,
            len in 1i64..100_000_000_000,
            start: i64,
        ) {
            prop_assume!(trace != other_trace && parent != [0; 8]);
            let ring = KeyRing::new(SigningKey::new(Kid::new("k").unwrap(), secret));
            let now = Timestamp::from_micros(from);
            let cursors = Cursors { ring: &ring, project_id: ProjectId::from_bytes(project), now };
            let (trace, other_trace) = (TraceId::from_bytes(trace), TraceId::from_bytes(other_trace));
            let (parent, last) = (SpanId::from_bytes(parent), (Timestamp::from_micros(start), SpanId::from_bytes(span)));
            let window = TimeWindow::new(now, Timestamp::from_micros(from + len)).unwrap();
            let head = (Timestamp::from_micros(start), trace.trace_hash(), trace);

            let all = Filter::all();
            let traces = cursors.seal_traces(window, now, head, &all);
            let children = cursors.seal_children(trace, parent, last);
            let spans = cursors.seal_trace(trace, last);
            prop_assert_eq!(
                cursors.open_traces(&traces, window, &all).map(|c| c.parts().3),
                Ok(head)
            );
            prop_assert_eq!(
                cursors.open_children(&children, trace, parent).map(|c| c.parts().3),
                Ok(last)
            );
            prop_assert_eq!(cursors.open_trace(&spans, trace), Ok(last));

            // A get_trace cursor is neither a children cursor of the zero span nor one of another trace.
            prop_assert!(cursors.open_children(&spans, trace, SpanId::ZERO).is_err());
            prop_assert!(cursors.open_trace(&spans, other_trace).is_err());
            prop_assert!(cursors.open_trace(&children, trace).is_err());
            prop_assert!(cursors.open_children(&children, trace, SpanId::ZERO).is_err());
            prop_assert!(cursors.open_traces(&children, window, &all).is_err());
            prop_assert!(cursors.open_trace(&traces, trace).is_err());
            // Another project's call opens none of them.
            let mut foreign = project;
            foreign[0] ^= 1;
            let other = Cursors { project_id: ProjectId::from_bytes(foreign), ..cursors };
            prop_assert_eq!(other.open_trace(&spans, trace), Err(CursorError::WrongProject));
            prop_assert_eq!(other.open_traces(&traces, window, &all).map(|_| ()), Err(CursorError::WrongProject));
        }

        /// The list cursors open for their own list, filter, user and window only, and for no other project.
        #[test]
        fn list_cursors_open_only_for_their_own_list(
            secret: [u8; 32],
            project: [u8; 16],
            span: [u8; 8],
            from in 0i64..4_000_000_000_000_000,
            len in 1i64..100_000_000_000,
            start: i64,
            id in "[a-z0-9-]{1,64}",
            user in "[a-z0-9@.]{1,64}",
            other_user in "[A-Z]{1,8}",
        ) {
            let ring = KeyRing::new(SigningKey::new(Kid::new("k").unwrap(), secret));
            let now = Timestamp::from_micros(from);
            let cursors = Cursors { ring: &ring, project_id: ProjectId::from_bytes(project), now };
            let window = TimeWindow::new(now, Timestamp::from_micros(from + len)).unwrap();
            let longer = TimeWindow::new(now, Timestamp::from_micros(from + len + 1)).unwrap();
            let last = (Timestamp::from_micros(start), SpanId::from_bytes(span));
            let errors = Filter::Level(vec![crucible_records::Level::Error]);

            let spans = cursors.seal_spans(window, now, last, &errors);
            prop_assert_eq!(cursors.open_spans(&spans, window, &errors).map(|c| c.parts().3), Ok(last));
            prop_assert!(cursors.open_spans(&spans, window, &Filter::all()).is_err());
            prop_assert!(cursors.open_spans(&spans, longer, &errors).is_err());
            prop_assert!(cursors.open_traces(&spans, window, &errors).is_err());

            let row = (Timestamp::from_micros(start), id.as_str());
            for list in [IdList::Sessions, IdList::Users] {
                let page = cursors.seal_ids(list, None, window, row).unwrap();
                let opened = cursors.open_ids(&page, list, None, window).unwrap();
                prop_assert_eq!(opened.parts().2, (row.0, id.as_str()));
                prop_assert!(cursors.open_ids(&page, list, None, longer).is_err());
                prop_assert!(cursors.open_spans(&page, window, &Filter::all()).is_err());
            }
            let sessions = cursors.seal_ids(IdList::Sessions, None, window, row).unwrap();
            prop_assert!(cursors.open_ids(&sessions, IdList::Users, None, window).is_err());
            let of_user = cursors.seal_ids(IdList::UserSessions, Some(&user), window, row).unwrap();
            prop_assert!(cursors.open_ids(&of_user, IdList::UserSessions, Some(&user), window).is_ok());
            prop_assert!(cursors.open_ids(&of_user, IdList::UserSessions, Some(&other_user), window).is_err());

            let mut foreign = project;
            foreign[0] ^= 1;
            let other = Cursors { project_id: ProjectId::from_bytes(foreign), ..cursors };
            prop_assert_eq!(
                other.open_spans(&spans, window, &errors).map(|_| ()),
                Err(CursorError::WrongProject)
            );
            prop_assert_eq!(
                other.open_ids(&sessions, IdList::Sessions, None, window).map(|_| ()),
                Err(CursorError::WrongProject)
            );
        }
    }
}
