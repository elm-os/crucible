//! Lowercase hex helpers shared by the identifier types.

use core::fmt;

/// Value of one hex digit; uppercase input is accepted.
const fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decodes exactly `2 * out.len()` hex digits into `out`. Returns `false` on any other input.
pub(crate) fn decode_into(s: &[u8], out: &mut [u8]) -> bool {
    if s.len() != out.len() * 2 {
        return false;
    }
    for (pair, slot) in s.as_chunks::<2>().0.iter().zip(out.iter_mut()) {
        match (digit(pair[0]), digit(pair[1])) {
            (Some(hi), Some(lo)) => *slot = (hi << 4) | lo,
            _ => return false,
        }
    }
    true
}

/// Writes `bytes` as lowercase hex.
pub(crate) fn write_lower(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}
