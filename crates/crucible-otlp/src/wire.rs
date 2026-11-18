//! Protobuf wire primitives: varints, field keys, field iteration with unknown-field skipping, and the
//! writer helpers the JSON transcoder uses.
//!
//! The decoding rules follow the protobuf encoding specification as strict decoders apply it: a varint
//! is at most ten bytes and at most 64 bits, a key is at most 32 bits with a non-zero field number and
//! a wire type from 0 to 5, a length never runs past its enclosing message, and nesting is limited to
//! [`RECURSION_LIMIT`] levels.

/// Nesting levels a decoder may descend. The fields of the top-level message sit at this level; the
/// fields of each nested message (or skipped group) sit one level below their parent field.
pub(crate) const RECURSION_LIMIT: u32 = 100;

pub(crate) const VARINT: u8 = 0;
pub(crate) const I64: u8 = 1;
pub(crate) const LEN: u8 = 2;
const START_GROUP: u8 = 3;
const END_GROUP: u8 = 4;
pub(crate) const I32: u8 = 5;

/// The wire-level problem in a malformed protobuf body. Field numbers name the field of the message
/// in which the problem sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    /// A field, length or varint runs past the end of its enclosing message.
    #[error("truncated field")]
    Truncated,
    /// A varint longer than ten bytes or above 64 bits.
    #[error("invalid varint")]
    Varint,
    /// A field key above 32 bits, with field number 0, or with wire type 6 or 7.
    #[error("invalid field key")]
    Key,
    /// An end-group tag with no matching start-group tag.
    #[error("unmatched end-group tag")]
    EndGroup,
    /// Messages or groups nested deeper than 100 levels.
    #[error("nesting deeper than 100 levels")]
    TooDeep,
    /// A known field with a wire type its schema does not allow.
    #[error("{message} field {field} has an unexpected wire type")]
    WireType {
        /// The message holding the field.
        message: &'static str,
        /// The field number.
        field: u32,
    },
    /// A string field that is not valid UTF-8.
    #[error("{message} field {field} is not valid UTF-8")]
    Utf8 {
        /// The message holding the field.
        message: &'static str,
        /// The field number.
        field: u32,
    },
    /// A trace id that is not 16 bytes or a span id that is not 8 bytes. An empty id is valid and
    /// means the id is absent.
    #[error("{message} field {field} holds an id of {len} bytes")]
    IdLength {
        /// The message holding the field.
        message: &'static str,
        /// The field number.
        field: u32,
        /// The length found.
        len: usize,
    },
    /// A singular message field that occurs more than once. Encoders never emit this; protobuf merges
    /// the occurrences, which a view over the raw bytes cannot represent, so the body is refused.
    #[error("{message} field {field} occurs more than once")]
    Duplicate {
        /// The message holding the field.
        message: &'static str,
        /// The field number.
        field: u32,
    },
}

/// The payload of one field.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Value<'a> {
    Varint(u64),
    I64(u64),
    Len(&'a [u8]),
    /// A group, already skipped. No field of the OTLP schema is a group.
    Group,
    /// A 32-bit fixed value. The only such fields (`flags`) are not read, so the value is dropped.
    I32,
}

/// Reads the varint at `pos`, returning its value and the position after it.
fn varint(buf: &[u8], pos: usize) -> Result<(u64, usize), WireError> {
    let mut value = 0u64;
    for i in 0..10 {
        let at = pos.checked_add(i).ok_or(WireError::Truncated)?;
        let byte = *buf.get(at).ok_or(WireError::Truncated)?;
        // The tenth byte may only carry bit 63.
        if i == 9 && byte > 1 {
            return Err(WireError::Varint);
        }
        value |= u64::from(byte & 0x7f) << (7 * i);
        if byte < 0x80 {
            return Ok((value, at + 1));
        }
    }
    Err(WireError::Varint)
}

/// Reads the field key at `pos`: field number, wire type and the position after the key.
fn key(buf: &[u8], pos: usize) -> Result<(u32, u8, usize), WireError> {
    let (key, next) = varint(buf, pos)?;
    let key = u32::try_from(key).map_err(|_| WireError::Key)?;
    let field = key >> 3;
    let wire_type = (key & 7) as u8;
    if field == 0 || wire_type > I32 {
        return Err(WireError::Key);
    }
    Ok((field, wire_type, next))
}

/// The `len` bytes at `pos`, and the position after them.
fn take(buf: &[u8], pos: usize, len: usize) -> Result<(&[u8], usize), WireError> {
    let end = pos.checked_add(len).ok_or(WireError::Truncated)?;
    let bytes = buf.get(pos..end).ok_or(WireError::Truncated)?;
    Ok((bytes, end))
}

/// Skips the value of an unknown field whose key ended at `pos`. `level` is the nesting level of the
/// field: skipping is itself refused at level 0, and a group's own fields sit one level down.
fn skip(buf: &[u8], pos: usize, field: u32, wire_type: u8, level: u32) -> Result<usize, WireError> {
    if level == 0 {
        return Err(WireError::TooDeep);
    }
    match wire_type {
        VARINT => varint(buf, pos).map(|(_, end)| end),
        I64 => take(buf, pos, 8).map(|(_, end)| end),
        LEN => {
            let (len, start) = varint(buf, pos)?;
            let len = usize::try_from(len).map_err(|_| WireError::Truncated)?;
            take(buf, start, len).map(|(_, end)| end)
        }
        START_GROUP => {
            let mut pos = pos;
            loop {
                let (inner, inner_type, next) = key(buf, pos)?;
                if inner_type == END_GROUP {
                    return if inner == field {
                        Ok(next)
                    } else {
                        Err(WireError::EndGroup)
                    };
                }
                pos = skip(buf, next, inner, inner_type, level - 1)?;
            }
        }
        I32 => take(buf, pos, 4).map(|(_, end)| end),
        _ => Err(WireError::EndGroup),
    }
}

/// The fields of one message, in wire order. Yields an error at most once and then stops.
#[derive(Debug, Clone)]
pub(crate) struct Fields<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Nesting level of this message's fields; used to skip groups.
    level: u32,
}

impl<'a> Fields<'a> {
    pub(crate) fn new(buf: &'a [u8], level: u32) -> Self {
        Self { buf, pos: 0, level }
    }

    fn read(&mut self) -> Result<(u32, Value<'a>), WireError> {
        let (field, wire_type, pos) = key(self.buf, self.pos)?;
        let (value, end) = match wire_type {
            VARINT => {
                let (v, end) = varint(self.buf, pos)?;
                (Value::Varint(v), end)
            }
            I64 => {
                let (bytes, end) = take(self.buf, pos, 8)?;
                let raw = bytes.first_chunk::<8>().ok_or(WireError::Truncated)?;
                (Value::I64(u64::from_le_bytes(*raw)), end)
            }
            LEN => {
                let (len, start) = varint(self.buf, pos)?;
                let len = usize::try_from(len).map_err(|_| WireError::Truncated)?;
                let (bytes, end) = take(self.buf, start, len)?;
                (Value::Len(bytes), end)
            }
            START_GROUP => {
                let end = skip(self.buf, pos, field, wire_type, self.level)?;
                (Value::Group, end)
            }
            I32 => (Value::I32, take(self.buf, pos, 4)?.1),
            _ => return Err(WireError::EndGroup),
        };
        self.pos = end;
        Ok((field, value))
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = Result<(u32, Value<'a>), WireError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.buf.len() {
            return None;
        }
        let item = self.read();
        if item.is_err() {
            self.pos = self.buf.len();
        }
        Some(item)
    }
}

/// Refuses an unknown field at level 0; skipping one counts as descending.
pub(crate) fn unknown(level: u32) -> Result<(), WireError> {
    if level == 0 {
        Err(WireError::TooDeep)
    } else {
        Ok(())
    }
}

/// The level of a nested message's fields, given the level of the field that holds it.
pub(crate) fn descend(level: u32) -> Result<u32, WireError> {
    level.checked_sub(1).ok_or(WireError::TooDeep)
}

/// Encodes `value` as a varint into `buf`, returning the number of bytes used.
fn encode_varint(mut value: u64, buf: &mut [u8; 10]) -> usize {
    let mut n = 0;
    for slot in buf.iter_mut() {
        n += 1;
        if value < 0x80 {
            *slot = value as u8;
            break;
        }
        *slot = (value as u8) | 0x80;
        value >>= 7;
    }
    n
}

/// Appends a varint.
pub(crate) fn put_varint(out: &mut Vec<u8>, value: u64) {
    let mut buf = [0; 10];
    let n = encode_varint(value, &mut buf);
    out.extend_from_slice(buf.get(..n).unwrap_or_default());
}

/// Appends a field key.
pub(crate) fn put_key(out: &mut Vec<u8>, field: u32, wire_type: u8) {
    put_varint(out, u64::from((field << 3) | u32::from(wire_type)));
}

/// Starts a length-delimited field: writes its key and a one-byte length placeholder, and returns the
/// position where the payload starts. [`end_len`] fills in the length once the payload is written.
pub(crate) fn begin_len(out: &mut Vec<u8>, field: u32) -> usize {
    put_key(out, field, LEN);
    out.push(0);
    out.len()
}

/// Writes the length of the payload that starts at `start` into its placeholder, moving the payload
/// when the length needs more than one byte, so the encoding stays minimal.
pub(crate) fn end_len(out: &mut Vec<u8>, start: usize) {
    let len = out.len().saturating_sub(start);
    let mut buf = [0; 10];
    let n = encode_varint(len as u64, &mut buf);
    let placeholder = start.saturating_sub(1)..start;
    out.splice(placeholder, buf.iter().take(n).copied());
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn fields(buf: &[u8]) -> Vec<Result<(u32, String), WireError>> {
        Fields::new(buf, RECURSION_LIMIT)
            .map(|f| f.map(|(n, v)| (n, format!("{v:?}"))))
            .collect()
    }

    #[test]
    fn varint_limits() {
        assert_eq!(varint(&[0x7f], 0), Ok((127, 1)));
        assert_eq!(varint(&[0x80, 0x01], 0), Ok((128, 2)));
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(varint(&max, 0), Ok((u64::MAX, 10)));
        let over = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(varint(&over, 0), Err(WireError::Varint));
        let eleven = [0x80; 11];
        assert_eq!(varint(&eleven, 0), Err(WireError::Varint));
        assert_eq!(varint(&[0x80], 0), Err(WireError::Truncated));
        // Overlong encodings are accepted, as protobuf decoders do.
        assert_eq!(varint(&[0x80, 0x00], 0), Ok((0, 2)));
    }

    #[test]
    fn keys_reject_field_zero_and_wire_types_six_and_seven() {
        assert_eq!(key(&[0x08], 0), Ok((1, VARINT, 1)));
        assert_eq!(key(&[0x00], 0), Err(WireError::Key));
        assert_eq!(key(&[0x0e], 0), Err(WireError::Key));
        assert_eq!(key(&[0x0f], 0), Err(WireError::Key));
        // A key above 32 bits.
        assert_eq!(key(&[0x80, 0x80, 0x80, 0x80, 0x10], 0), Err(WireError::Key));
    }

    #[test]
    fn groups_are_skipped_and_must_match() {
        // Field 1 group containing field 2 varint 5, then field 3 varint 7.
        let ok = [0x0b, 0x10, 0x05, 0x0c, 0x18, 0x07];
        let got = fields(&ok);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], Ok((1, "Group".to_owned())));
        assert_eq!(got[1], Ok((3, "Varint(7)".to_owned())));
        // End-group tag for field 2 closing a group opened as field 1.
        assert_eq!(fields(&[0x0b, 0x14]), vec![Err(WireError::EndGroup)]);
        // A bare end-group tag.
        assert_eq!(fields(&[0x0c]), vec![Err(WireError::EndGroup)]);
        // An unterminated group.
        assert_eq!(fields(&[0x0b, 0x10, 0x05]), vec![Err(WireError::Truncated)]);
    }

    #[test]
    fn group_nesting_is_limited() {
        // Groups nested 100 deep under a level-100 field: the innermost group sits at level 1 and
        // its end tag closes it.
        let mut nested = vec![0x0b; 100];
        nested.extend(std::iter::repeat_n(0x0c, 100));
        assert!(fields(&nested).iter().all(Result::is_ok));
        // A field inside the innermost group sits at level 0.
        let mut deeper = vec![0x0b; 100];
        deeper.extend_from_slice(&[0x08, 0x01]);
        deeper.extend(std::iter::repeat_n(0x0c, 100));
        assert_eq!(fields(&deeper), vec![Err(WireError::TooDeep)]);
    }

    #[test]
    fn lengths_never_run_past_the_message() {
        assert_eq!(fields(&[0x0a, 0x02, 0x00]), vec![Err(WireError::Truncated)]);
        assert_eq!(fields(&[0x09, 0x00]), vec![Err(WireError::Truncated)]);
        assert_eq!(fields(&[0x0d, 0x00]), vec![Err(WireError::Truncated)]);
        let huge = [
            0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        assert_eq!(fields(&huge), vec![Err(WireError::Truncated)]);
    }

    #[test]
    fn length_placeholders_grow_when_needed() {
        for len in [0usize, 1, 127, 128, 300, 16_384, 70_000] {
            let mut out = vec![0xaa];
            let start = begin_len(&mut out, 5);
            out.extend(std::iter::repeat_n(0x55, len));
            end_len(&mut out, start);
            let mut expected = vec![0xaa];
            put_key(&mut expected, 5, LEN);
            put_varint(&mut expected, len as u64);
            expected.extend(std::iter::repeat_n(0x55, len));
            assert_eq!(out, expected, "len {len}");
        }
    }

    proptest! {
        #[test]
        fn varints_round_trip(value in any::<u64>()) {
            let mut out = Vec::new();
            put_varint(&mut out, value);
            prop_assert_eq!(varint(&out, 0), Ok((value, out.len())));
        }

        #[test]
        fn field_iteration_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
            for item in Fields::new(&bytes, 3) {
                let _ = item;
            }
        }
    }
}
