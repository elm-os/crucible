//! Cutting values to byte caps (contract 3.1): an over-long value is cut to the longest prefix at or below the cap
//! that ends on a UTF-8 character boundary. The span payload rule built on these helpers is
//! [`crate::SpanRecord::enforce_payload_caps`].

/// Whether `b` continues a multi-byte UTF-8 character.
const fn is_continuation(b: u8) -> bool {
    b & 0b1100_0000 == 0b1000_0000
}

/// The length of the longest prefix of `value` of at most `cap` bytes that ends on a UTF-8 character boundary.
///
/// For valid UTF-8 this is exact. For other bytes it backs off over at most three continuation bytes (a UTF-8
/// character is at most four bytes long), so the cost never depends on the length of `value`.
pub const fn utf8_floor(value: &[u8], cap: usize) -> usize {
    if value.len() <= cap {
        return value.len();
    }
    let mut end = cap;
    while end > 0 && cap - end < 3 && is_continuation(value[end]) {
        end -= 1;
    }
    end
}

/// `s` cut to the longest prefix of at most `cap` bytes that ends on a character boundary.
pub fn truncate_str(s: &str, cap: usize) -> &str {
    s.get(..utf8_floor(s.as_bytes(), cap)).unwrap_or_default()
}

/// The longest valid UTF-8 prefix of the first `cap` bytes of `value`: a preview of a body that is UTF-8 text or
/// JSON. It stops early only at bytes that are not UTF-8.
pub fn utf8_prefix(value: &[u8], cap: usize) -> &str {
    let head = value.get(..cap).unwrap_or(value);
    match core::str::from_utf8(head) {
        Ok(s) => s,
        Err(e) => head
            .get(..e.valid_up_to())
            .and_then(|valid| core::str::from_utf8(valid).ok())
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn cuts_on_character_boundaries() {
        assert_eq!(truncate_str("héllo", 2), "h");
        assert_eq!(truncate_str("héllo", 3), "hé");
        assert_eq!(truncate_str("héllo", 99), "héllo");
        assert_eq!(truncate_str("😀x", 3), "");
        assert_eq!(truncate_str("😀x", 4), "😀");
        assert_eq!(truncate_str("abc", 0), "");
        assert_eq!(utf8_prefix("aé".as_bytes(), 2), "a");
        assert_eq!(utf8_prefix(b"ab\xffcd", 5), "ab");
        assert_eq!(utf8_prefix(b"", 5), "");
    }

    #[test]
    fn arbitrary_bytes_back_off_at_most_three() {
        let value = [0x80u8; 16];
        assert_eq!(utf8_floor(&value, 10), 7);
        assert_eq!(utf8_floor(&value, 2), 0);
        assert_eq!(utf8_floor(&value, 16), 16);
    }

    proptest! {
        #[test]
        fn truncate_str_is_the_longest_fitting_prefix(s in "\\PC{0,40}", cap in 0usize..80) {
            let t = truncate_str(&s, cap);
            prop_assert!(s.starts_with(t));
            prop_assert!(t.len() <= cap);
            // Longest: the next character, if any, would not fit.
            if let Some(next) = s[t.len()..].chars().next() {
                prop_assert!(t.len() + next.len_utf8() > cap);
            }
        }

        #[test]
        fn utf8_floor_agrees_with_str_boundaries(s in "\\PC{0,40}", cap in 0usize..80) {
            let n = utf8_floor(s.as_bytes(), cap);
            prop_assert!(s.is_char_boundary(n));
            prop_assert_eq!(n, truncate_str(&s, cap).len());
            prop_assert_eq!(utf8_prefix(s.as_bytes(), cap), truncate_str(&s, cap));
        }
    }
}
