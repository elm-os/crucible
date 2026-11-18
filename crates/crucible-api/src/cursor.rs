//! A temporary cursor encoding over the cursor values of `crucible-query`.
//!
//! Milestone 2 replaces it with the signed `v1.<kid>.<payload>.<mac>` of `crucible-cursor` (contract 12.4);
//! clients treat a cursor as opaque either way, so the change is invisible to them. Until then a cursor is
//! `tmp1.<kind>.<payload>`: the kind (`p` page, `h` head, `c` children) and the cursor's parts in big-endian
//! order, base64url without padding. It carries no MAC, but the service checks the project, window, trace and
//! parent inside it against the request, so a cursor cannot cross projects; text that does not decode is
//! `cursor_invalid`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crucible_query::{ChildCursor, HeadCursor, PageCursor};
use crucible_records::{Lsn, ProjectId, SpanId, TimeWindow, Timestamp, TraceId};

const VERSION: &str = "tmp1";

/// Writes cursor parts.
#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn bytes(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }

    fn time(self, t: Timestamp) -> Self {
        self.bytes(&t.micros().to_be_bytes())
    }

    fn window(self, window: TimeWindow) -> Self {
        self.time(window.from()).time(window.to())
    }

    fn finish(self, kind: char) -> String {
        format!("{VERSION}.{kind}.{}", URL_SAFE_NO_PAD.encode(self.0))
    }
}

/// Reads cursor parts; every read fails once the payload is exhausted.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn time(&mut self) -> Option<Timestamp> {
        Some(Timestamp::from_micros(i64::from_be_bytes(self.array()?)))
    }

    fn window(&mut self) -> Option<TimeWindow> {
        TimeWindow::new(self.time()?, self.time()?).ok()
    }

    fn project(&mut self) -> Option<ProjectId> {
        Some(ProjectId::from_bytes(self.array()?))
    }

    fn trace(&mut self) -> Option<TraceId> {
        Some(TraceId::from_bytes(self.array()?))
    }

    fn span(&mut self) -> Option<SpanId> {
        Some(SpanId::from_bytes(self.array()?))
    }

    /// The value, when the payload held nothing after it.
    fn done<T>(&self, value: T) -> Option<T> {
        self.0.is_empty().then_some(value)
    }
}

fn payload(text: &str, kind: char) -> Option<Vec<u8>> {
    let rest = text.strip_prefix(VERSION)?.strip_prefix('.')?;
    let rest = rest.strip_prefix(kind)?.strip_prefix('.')?;
    URL_SAFE_NO_PAD.decode(rest).ok()
}

/// A trace-list page cursor.
pub(crate) fn encode_page(cursor: &PageCursor) -> String {
    let (project, window, as_of, (start, hash, trace)) = cursor.parts();
    Writer::default()
        .bytes(&project.to_bytes())
        .window(window)
        .time(as_of)
        .time(start)
        .bytes(&hash.to_be_bytes())
        .bytes(trace.as_bytes())
        .finish('p')
}

pub(crate) fn decode_page(text: &str) -> Option<PageCursor> {
    let bytes = payload(text, 'p')?;
    let mut r = Reader(&bytes);
    let (project, window, as_of) = (r.project()?, r.window()?, r.time()?);
    let (start, hash, trace) = (r.time()?, u32::from_be_bytes(r.array()?), r.trace()?);
    // The hash is a function of the trace id; a cursor whose parts disagree was not issued here.
    if hash != trace.trace_hash() {
        return None;
    }
    r.done(PageCursor::from_parts(
        project,
        window,
        as_of,
        (start, hash, trace),
    ))
}

/// A live-tail head cursor.
pub(crate) fn encode_head(cursor: &HeadCursor) -> String {
    let (project, window, as_of, lsn) = cursor.parts();
    Writer::default()
        .bytes(&project.to_bytes())
        .window(window)
        .time(as_of)
        .bytes(&lsn.get().to_be_bytes())
        .finish('h')
}

pub(crate) fn decode_head(text: &str) -> Option<HeadCursor> {
    let bytes = payload(text, 'h')?;
    let mut r = Reader(&bytes);
    let (project, window, as_of) = (r.project()?, r.window()?, r.time()?);
    let lsn = Lsn::new(u64::from_be_bytes(r.array()?));
    r.done(HeadCursor::from_parts(project, window, as_of, lsn))
}

/// A span-children cursor.
pub(crate) fn encode_child(cursor: &ChildCursor) -> String {
    let (project, trace, parent, (start, span)) = cursor.parts();
    Writer::default()
        .bytes(&project.to_bytes())
        .bytes(trace.as_bytes())
        .bytes(parent.as_bytes())
        .time(start)
        .bytes(span.as_bytes())
        .finish('c')
}

pub(crate) fn decode_child(text: &str) -> Option<ChildCursor> {
    let bytes = payload(text, 'c')?;
    let mut r = Reader(&bytes);
    let (project, trace, parent) = (r.project()?, r.trace()?, r.span()?);
    let (start, span) = (r.time()?, r.span()?);
    r.done(ChildCursor::from_parts(
        project,
        trace,
        parent,
        (start, span),
    ))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn window(from: i64, len: i64) -> TimeWindow {
        TimeWindow::new(
            Timestamp::from_micros(from),
            Timestamp::from_micros(from + len),
        )
        .unwrap()
    }

    proptest! {
        #[test]
        fn cursors_round_trip_and_never_pass_for_another_kind(
            project: [u8; 16],
            trace: [u8; 16],
            span: [u8; 8],
            parent: [u8; 8],
            from in 0i64..4_000_000_000_000_000,
            len in 1i64..100_000_000_000,
            as_of: i64,
            start: i64,
            lsn: u64,
        ) {
            let (project, trace) = (ProjectId::from_bytes(project), TraceId::from_bytes(trace));
            let (span, parent) = (SpanId::from_bytes(span), SpanId::from_bytes(parent));
            let (window, as_of, start) =
                (window(from, len), Timestamp::from_micros(as_of), Timestamp::from_micros(start));
            let page = PageCursor::from_parts(project, window, as_of, (start, trace.trace_hash(), trace));
            let head = HeadCursor::from_parts(project, window, as_of, Lsn::new(lsn));
            let child = ChildCursor::from_parts(project, trace, parent, (start, span));
            let (page_text, head_text, child_text) =
                (encode_page(&page), encode_head(&head), encode_child(&child));
            prop_assert_eq!(decode_page(&page_text), Some(page));
            prop_assert_eq!(decode_head(&head_text), Some(head));
            prop_assert_eq!(decode_child(&child_text), Some(child));
            prop_assert!(page_text.starts_with("tmp1.p."));
            for text in [&page_text, &head_text, &child_text] {
                let kinds = [
                    decode_page(text).is_some(),
                    decode_head(text).is_some(),
                    decode_child(text).is_some(),
                ];
                prop_assert_eq!(kinds.iter().filter(|ok| **ok).count(), 1);
            }
            // A cut payload never decodes.
            prop_assert_eq!(decode_page(&page_text[..page_text.len() - 2]), None);
        }

        #[test]
        fn arbitrary_text_never_panics(text in ".{0,120}") {
            let _ = (decode_page(&text), decode_head(&text), decode_child(&text));
        }
    }

    #[test]
    fn a_page_cursor_whose_hash_disagrees_with_its_trace_is_refused() {
        let trace = TraceId::from_bytes([3; 16]);
        let page = PageCursor::from_parts(
            ProjectId::from_bytes([1; 16]),
            window(0, 10),
            Timestamp::from_micros(5),
            (
                Timestamp::from_micros(1),
                trace.trace_hash().wrapping_add(1),
                trace,
            ),
        );
        assert_eq!(decode_page(&encode_page(&page)), None);
        assert_eq!(decode_page("tmp1.p.!!"), None);
        assert_eq!(decode_page("v1.k.AAAA.BBBB"), None);
    }
}
