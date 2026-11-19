//! JSON text for the payload tier: attribute values, residual attribute objects, events and links.
//!
//! One streaming writer renders every value, so equal inputs always give equal bytes (the row digest depends on
//! them). The rules:
//!
//! - An attribute list is an object whose members keep wire order. A key that occurs twice is written twice; no
//!   member is dropped or reordered.
//! - Strings are escaped as RFC 8259 requires: `"`, `\` and the control characters below U+0020 (as `\n`, `\r`,
//!   `\t`, `\b`, `\f` or `\u00XX`). Every other character is written as UTF-8.
//! - Integers are JSON numbers in decimal. A finite double is the shortest text that reads back as the same
//!   double (`1.0`, `0.1`, `1e+300`, `-0.0`); NaN and the infinities are the strings `"NaN"`, `"Infinity"` and
//!   `"-Infinity"`, the OTLP/JSON spellings.
//! - Bytes are a lowercase hex string. An unset value is `null`.
//! - The attribute lists of events and links leave out the keys that must never be stored
//!   ([`mcp_keys::never_stored`], contract 3.5), as the residual attributes of a span do.

use core::fmt::{self, Write as _};

use bytes::Bytes;
use crucible_otlp::{AnyValueView, Attributes, EventView, LinkView};
use crucible_records::{caps, mcp_keys};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Appends the lowercase hex form of `bytes`.
pub(crate) fn push_hex(out: &mut Vec<u8>, bytes: &[u8]) {
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)]);
        out.push(HEX[usize::from(b & 0x0f)]);
    }
}

/// Appends `s` as a JSON string.
pub(crate) fn push_str(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    let bytes = s.as_bytes();
    let mut start = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let short: &[u8] = match b {
            b'"' => b"\\\"",
            b'\\' => b"\\\\",
            b'\n' => b"\\n",
            b'\r' => b"\\r",
            b'\t' => b"\\t",
            0x08 => b"\\b",
            0x0c => b"\\f",
            0x00..=0x1f => b"",
            _ => continue,
        };
        out.extend_from_slice(&bytes[start..i]);
        if short.is_empty() {
            out.extend_from_slice(b"\\u00");
            push_hex(out, &[b]);
        } else {
            out.extend_from_slice(short);
        }
        start = i + 1;
    }
    out.extend_from_slice(&bytes[start..]);
    out.push(b'"');
}

/// Appends a signed integer in decimal.
pub(crate) fn push_i64(out: &mut Vec<u8>, value: i64) {
    if value < 0 {
        out.push(b'-');
    }
    push_u64(out, value.unsigned_abs());
}

/// Appends an unsigned integer in decimal.
pub(crate) fn push_u64(out: &mut Vec<u8>, mut value: u64) {
    // u64::MAX has 20 digits.
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

pub(crate) fn push_f64(out: &mut Vec<u8>, value: f64) {
    if value.is_nan() {
        push_str(out, "NaN");
    } else if value.is_infinite() {
        push_str(out, if value < 0.0 { "-Infinity" } else { "Infinity" });
    } else if serde_json::to_writer(&mut *out, &value).is_err() {
        // Writing a finite double to a vector cannot fail.
        out.extend_from_slice(b"null");
    }
}

/// Appends one attribute value.
pub(crate) fn push_value(out: &mut Vec<u8>, value: AnyValueView<'_>) {
    match value {
        AnyValueView::Empty => out.extend_from_slice(b"null"),
        AnyValueView::String(s) => push_str(out, s),
        AnyValueView::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        AnyValueView::Int(i) => push_i64(out, i),
        AnyValueView::Double(d) => push_f64(out, d),
        AnyValueView::Bytes(b) => {
            out.push(b'"');
            push_hex(out, b);
            out.push(b'"');
        }
        AnyValueView::Array(values) => {
            out.push(b'[');
            for (i, v) in values.enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                push_value(out, v);
            }
            out.push(b']');
        }
        AnyValueView::KvList(pairs) => push_object(out, pairs),
    }
}

/// Appends an attribute list as an object.
fn push_object<'a>(out: &mut Vec<u8>, pairs: impl Iterator<Item = (&'a str, AnyValueView<'a>)>) {
    out.push(b'{');
    for (i, (key, value)) in pairs.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        push_str(out, key);
        out.push(b':');
        push_value(out, value);
    }
    out.push(b'}');
}

/// A value rendered as JSON text.
pub(crate) fn value(value: AnyValueView<'_>) -> Bytes {
    let mut out = Vec::new();
    push_value(&mut out, value);
    Bytes::from(out)
}

/// The pairs as a JSON object, or empty bytes (absent) when there are none.
pub(crate) fn object_or_empty<'a>(
    pairs: impl Iterator<Item = (&'a str, AnyValueView<'a>)>,
) -> Bytes {
    let mut out = Vec::new();
    for (key, value) in pairs {
        out.push(if out.is_empty() { b'{' } else { b',' });
        push_str(&mut out, key);
        out.push(b':');
        push_value(&mut out, value);
    }
    if out.is_empty() {
        return Bytes::new();
    }
    out.push(b'}');
    Bytes::from(out)
}

/// The attribute list of an event or a link without the keys that must never be stored.
fn stored<'a>(pairs: Attributes<'a>) -> impl Iterator<Item = (&'a str, AnyValueView<'a>)> {
    pairs.filter(|(key, _)| !mcp_keys::never_stored(key))
}

/// Writes `Display` output into the vector.
struct Text<'a>(&'a mut Vec<u8>);

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

/// `events`: a list of `{"time", "name", "attributes"}` with the time in RFC 3339 form, the first
/// [`caps::EVENTS_MAX_ITEMS`] events in wire order; empty bytes (absent) when the span has none.
pub(crate) fn events<'a>(events: impl Iterator<Item = EventView<'a>>) -> Bytes {
    let mut out = Vec::new();
    for event in events.take(caps::EVENTS_MAX_ITEMS) {
        out.push(if out.is_empty() { b'[' } else { b',' });
        out.extend_from_slice(b"{\"time\":\"");
        // Writing into a vector cannot fail.
        let _ = write!(Text(&mut out), "{}", event.time());
        out.extend_from_slice(b"\",\"name\":");
        push_str(&mut out, event.name());
        out.extend_from_slice(b",\"attributes\":");
        push_object(&mut out, stored(event.attributes()));
        out.push(b'}');
    }
    if out.is_empty() {
        return Bytes::new();
    }
    out.push(b']');
    Bytes::from(out)
}

/// `links`: a list of `{"trace_id", "span_id", "attributes"}` with hex ids, the first [`caps::LINKS_MAX_ITEMS`]
/// links in wire order; empty bytes (absent) when the span has none.
pub(crate) fn links<'a>(links: impl Iterator<Item = LinkView<'a>>) -> Bytes {
    let mut out = Vec::new();
    for link in links.take(caps::LINKS_MAX_ITEMS) {
        out.push(if out.is_empty() { b'[' } else { b',' });
        out.extend_from_slice(b"{\"trace_id\":\"");
        push_hex(&mut out, link.trace_id().as_bytes());
        out.extend_from_slice(b"\",\"span_id\":\"");
        push_hex(&mut out, link.span_id().as_bytes());
        out.extend_from_slice(b"\",\"attributes\":");
        push_object(&mut out, stored(link.attributes()));
        out.push(b'}');
    }
    if out.is_empty() {
        return Bytes::new();
    }
    out.push(b']');
    Bytes::from(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn rendered(f: impl FnOnce(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn strings_escape_quotes_backslashes_and_controls() {
        let s = rendered(|o| push_str(o, "a\"b\\c\nd\re\tf\u{8}g\u{c}h\u{1}i\u{1f}j\u{7f}é😀"));
        assert_eq!(
            s,
            "\"a\\\"b\\\\c\\nd\\re\\tf\\bg\\fh\\u0001i\\u001fj\u{7f}é😀\""
        );
    }

    #[test]
    fn numbers_and_scalars() {
        let doubles = [
            (1.0, "1.0"),
            (0.1, "0.1"),
            (-0.0, "-0.0"),
            (1e300, "1e+300"),
            (f64::NAN, "\"NaN\""),
            (f64::INFINITY, "\"Infinity\""),
            (f64::NEG_INFINITY, "\"-Infinity\""),
        ];
        for (d, text) in doubles {
            assert_eq!(rendered(|o| push_value(o, AnyValueView::Double(d))), text);
        }
        assert_eq!(rendered(|o| push_i64(o, i64::MIN)), "-9223372036854775808");
        assert_eq!(rendered(|o| push_i64(o, 0)), "0");
        assert_eq!(rendered(|o| push_u64(o, u64::MAX)), "18446744073709551615");
        assert_eq!(
            rendered(|o| push_value(o, AnyValueView::Bool(true))),
            "true"
        );
        assert_eq!(rendered(|o| push_value(o, AnyValueView::Empty)), "null");
        let bytes = AnyValueView::Bytes(&[0x00, 0xab, 0x7f]);
        assert_eq!(rendered(|o| push_value(o, bytes)), "\"00ab7f\"");
    }

    #[test]
    fn empty_lists_are_absent() {
        assert!(object_or_empty(core::iter::empty()).is_empty());
        assert!(events(core::iter::empty()).is_empty());
        assert!(links(core::iter::empty()).is_empty());
    }

    proptest! {
        /// Every string renders to a JSON string that reads back as itself.
        #[test]
        fn strings_round_trip(s in "\\PC*|[\\x00-\\x1f\"\\\\]*") {
            let text = rendered(|o| push_str(o, &s));
            prop_assert_eq!(serde_json::from_str::<String>(&text).unwrap(), s);
        }

        /// Every finite double renders to JSON that reads back as the same double.
        #[test]
        fn finite_doubles_round_trip(bits in any::<u64>()) {
            let d = f64::from_bits(bits);
            prop_assume!(d.is_finite());
            let text = rendered(|o| push_value(o, AnyValueView::Double(d)));
            let back: f64 = serde_json::from_str(&text).unwrap();
            prop_assert_eq!(back.to_bits(), d.to_bits());
        }

        #[test]
        fn integers_match_display(i in any::<i64>(), u in any::<u64>()) {
            prop_assert_eq!(rendered(|o| push_i64(o, i)), i.to_string());
            prop_assert_eq!(rendered(|o| push_u64(o, u)), u.to_string());
        }
    }
}
