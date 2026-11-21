//! Canonical JSON text of one value, written while the value's bytes stream past (contract 9.10 captures arguments
//! and results as JSON text).
//!
//! Whitespace between tokens is dropped, and every string is decoded and written again with only the escapes JSON
//! requires (a quote, a backslash and control characters, as the SDKs' serializers write them), so text hidden
//! behind escapes such as `@` reaches the scrubbing rules as plain text. Numbers and literals are kept as
//! written. At most `limit` bytes are kept, ending on a character boundary; the rest of the value is read to its
//! end only to find that end. Like the scanner, the writer never rejects: it needs only string and nesting
//! structure to be right.

/// What the writer kept of one value.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Recorded {
    /// The canonical text, at most the limit.
    pub text: String,
    /// The text was longer than what is kept.
    pub more: bool,
}

impl Recorded {
    /// At most `limit` bytes of the text, cut on a character boundary; whatever is cut counts as more.
    pub fn within(mut self, limit: usize) -> Self {
        if self.text.len() > limit {
            let mut end = limit;
            while !self.text.is_char_boundary(end) {
                end -= 1;
            }
            self.text.truncate(end);
            self.more = true;
        }
        self
    }
}

/// The escape state inside a string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Escape {
    None,
    /// After a backslash.
    Start,
    /// Inside `\uXXXX`: digits still expected, and the code unit so far.
    Unicode(u8, u16),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Outside strings and literals.
    Between,
    /// Inside a number, `true`, `false` or `null`.
    Literal,
    /// Inside a string.
    Text(Escape),
}

/// Whether the value ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fed {
    /// The value goes on.
    More,
    /// The byte was the value's last.
    Done,
    /// The value ended just before the byte, which belongs to what follows (a literal ends this way).
    DoneBefore,
}

/// See the module documentation.
#[derive(Debug)]
pub(crate) struct Canonical {
    out: Vec<u8>,
    limit: usize,
    more: bool,
    depth: u32,
    state: State,
    /// A high surrogate waiting for its low half.
    high_surrogate: Option<u16>,
}

const HEX: &[u8; 16] = b"0123456789abcdef";

impl Canonical {
    /// A writer that keeps at most `limit` bytes.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            out: Vec::new(),
            limit,
            more: false,
            depth: 0,
            state: State::Between,
            high_surrogate: None,
        }
    }

    /// What was kept. Bytes that are not UTF-8 in the input become replacement characters.
    pub(crate) fn finish(self) -> Recorded {
        let text = match String::from_utf8(self.out) {
            Ok(text) => text,
            Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
        };
        Recorded {
            text,
            more: self.more,
        }
        .within(self.limit)
    }

    /// Writes whole units: a unit that does not fit is dropped, with everything after it.
    fn write(&mut self, bytes: &[u8]) {
        if self.more {
            return;
        }
        if bytes.len() <= self.limit - self.out.len() {
            self.out.extend_from_slice(bytes);
        } else {
            self.seal();
        }
    }

    /// Writes raw string bytes, cut before a character that does not fit.
    fn write_run(&mut self, run: &[u8]) {
        if self.more {
            return;
        }
        let room = self.limit - self.out.len();
        if run.len() <= room {
            self.out.extend_from_slice(run);
            return;
        }
        let mut end = room;
        while end > 0 && run[end] & 0xc0 == 0x80 {
            end -= 1;
        }
        self.out.extend_from_slice(&run[..end]);
        self.seal();
    }

    /// The limit is reached: nothing more is kept, and a character that the limit cut is dropped whole.
    fn seal(&mut self) {
        self.more = true;
        let len = self.out.len();
        let lead = (len.saturating_sub(4)..len)
            .rev()
            .find(|&at| self.out[at] & 0xc0 != 0x80);
        if let Some(at) = lead {
            let need = match self.out[at] {
                0xf0.. => 4,
                0xe0.. => 3,
                0xc0.. => 2,
                _ => 1,
            };
            if len - at < need {
                self.out.truncate(at);
            }
        }
    }

    /// Writes one decoded character of a string, escaped as JSON requires.
    fn write_char(&mut self, c: char) {
        match c {
            '"' => self.write(b"\\\""),
            '\\' => self.write(b"\\\\"),
            '\u{08}' => self.write(b"\\b"),
            '\u{0c}' => self.write(b"\\f"),
            '\n' => self.write(b"\\n"),
            '\r' => self.write(b"\\r"),
            '\t' => self.write(b"\\t"),
            c if u32::from(c) < 0x20 => {
                let code = u32::from(c) as usize;
                self.write(&[b'\\', b'u', b'0', b'0', HEX[code >> 4], HEX[code & 0x0f]]);
            }
            c => {
                let mut utf8 = [0u8; 4];
                self.write(c.encode_utf8(&mut utf8).as_bytes());
            }
        }
    }

    /// A high surrogate that no low half followed is not a character.
    fn flush_surrogate(&mut self) {
        if self.high_surrogate.take().is_some() {
            self.write_char(char::REPLACEMENT_CHARACTER);
        }
    }

    fn code_unit(&mut self, unit: u16) {
        match (self.high_surrogate.take(), unit) {
            (Some(high), 0xdc00..=0xdfff) => {
                let scalar =
                    0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
                self.write_char(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            (pending, unit) => {
                if pending.is_some() {
                    self.write_char(char::REPLACEMENT_CHARACTER);
                }
                match unit {
                    0xd800..=0xdbff => self.high_surrogate = Some(unit),
                    _ => self.write_char(
                        char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER),
                    ),
                }
            }
        }
    }

    /// Feeds a run of string bytes that holds no quote and no backslash: the value cannot end in it.
    pub(crate) fn feed_run(&mut self, run: &[u8]) {
        let mut rest = run;
        while !rest.is_empty() {
            if self.state == State::Text(Escape::None) && self.high_surrogate.is_none() {
                if self.more {
                    return;
                }
                let plain = rest.iter().position(|&b| b < 0x20).unwrap_or(rest.len());
                self.write_run(&rest[..plain]);
                rest = &rest[plain..];
                if rest.is_empty() {
                    return;
                }
            }
            self.feed(rest[0]);
            rest = &rest[1..];
        }
    }

    /// Feeds the value's next byte.
    pub(crate) fn feed(&mut self, byte: u8) -> Fed {
        match self.state {
            State::Between => self.between(byte),
            State::Literal => {
                if matches!(
                    byte,
                    b' ' | b'\t' | b'\r' | b'\n' | b',' | b':' | b'}' | b']' | b'{' | b'[' | b'"'
                ) {
                    self.state = State::Between;
                    if self.depth == 0 {
                        return Fed::DoneBefore;
                    }
                    self.between(byte)
                } else {
                    self.write(&[byte]);
                    Fed::More
                }
            }
            State::Text(escape) => self.text(escape, byte),
        }
    }

    fn between(&mut self, byte: u8) -> Fed {
        match byte {
            b' ' | b'\t' | b'\r' | b'\n' => Fed::More,
            b'{' | b'[' => {
                self.write(&[byte]);
                self.depth += 1;
                Fed::More
            }
            b'}' | b']' => {
                if self.depth == 0 {
                    return Fed::DoneBefore;
                }
                self.write(&[byte]);
                self.depth -= 1;
                if self.depth == 0 {
                    Fed::Done
                } else {
                    Fed::More
                }
            }
            b',' | b':' if self.depth == 0 => Fed::DoneBefore,
            b',' | b':' => {
                self.write(&[byte]);
                Fed::More
            }
            b'"' => {
                self.write(b"\"");
                self.state = State::Text(Escape::None);
                Fed::More
            }
            _ => {
                self.write(&[byte]);
                self.state = State::Literal;
                Fed::More
            }
        }
    }

    fn text(&mut self, escape: Escape, byte: u8) -> Fed {
        match escape {
            Escape::None => match byte {
                b'\\' => self.state = State::Text(Escape::Start),
                b'"' => {
                    self.flush_surrogate();
                    self.write(b"\"");
                    self.state = State::Between;
                    if self.depth == 0 {
                        return Fed::Done;
                    }
                }
                _ => {
                    self.flush_surrogate();
                    if byte < 0x20 {
                        self.write_char(char::from(byte));
                    } else {
                        self.write_run(&[byte]);
                    }
                }
            },
            Escape::Start => {
                self.state = State::Text(Escape::None);
                let plain = match byte {
                    b'u' => {
                        self.state = State::Text(Escape::Unicode(4, 0));
                        return Fed::More;
                    }
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'b' => '\u{08}',
                    b'f' => '\u{0c}',
                    other => char::from(other),
                };
                self.flush_surrogate();
                self.write_char(plain);
            }
            Escape::Unicode(left, unit) => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => {
                        // Not an escape after all; keep going as plain text.
                        self.state = State::Text(Escape::None);
                        self.flush_surrogate();
                        self.write_char(char::REPLACEMENT_CHARACTER);
                        return self.text(Escape::None, byte);
                    }
                };
                let unit = (unit << 4) | u16::from(digit);
                if left > 1 {
                    self.state = State::Text(Escape::Unicode(left - 1, unit));
                } else {
                    self.state = State::Text(Escape::None);
                    self.code_unit(unit);
                }
            }
        }
        Fed::More
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical text of a whole value given in one piece, and how its end was found.
    fn canonical(input: &str, limit: usize) -> (Recorded, Fed, usize) {
        let mut writer = Canonical::new(limit);
        for (at, byte) in input.bytes().enumerate() {
            match writer.feed(byte) {
                Fed::More => {}
                fed => return (writer.finish(), fed, at),
            }
        }
        (writer.finish(), Fed::More, input.len())
    }

    #[test]
    fn whitespace_goes_and_strings_take_minimal_escapes() {
        let input = "{ \"note\" : \"ada\\u0040example.com \\/ \\u00e9\\ud83d\\ude00 \\n\\u0001\\\"\" , \"n\": [1, -2.5e3 , true,null] }tail";
        let (recorded, fed, at) = canonical(input, 1 << 20);
        assert_eq!(
            recorded.text,
            "{\"note\":\"ada@example.com / \u{e9}\u{1f600} \\n\\u0001\\\"\",\"n\":[1,-2.5e3,true,null]}"
        );
        assert!(!recorded.more);
        assert_eq!(fed, Fed::Done);
        assert_eq!(&input[at..], "}tail");
    }

    #[test]
    fn a_scalar_ends_where_its_token_does() {
        assert_eq!(canonical("\"x\\\"y\",", 64).0.text, "\"x\\\"y\"");
        let (recorded, fed, _) = canonical("12.5}", 64);
        assert_eq!((recorded.text.as_str(), fed), ("12.5", Fed::DoneBefore));
        let (recorded, fed, _) = canonical("true ", 64);
        assert_eq!((recorded.text.as_str(), fed), ("true", Fed::DoneBefore));
    }

    #[test]
    fn the_limit_keeps_whole_characters_and_escapes() {
        // Fed one byte at a time, the first bytes of a character that does not fit are taken back.
        let (recorded, _, _) = canonical("\"aé\\n\"", 3);
        assert_eq!(recorded.text, "\"a");
        assert!(recorded.more);
        let (recorded, _, _) = canonical("\"a😀\"", 5);
        assert_eq!(recorded.text, "\"a");
        assert!(recorded.more);
        let (recorded, _, _) = canonical("\"ab\\n\"", 4);
        assert_eq!(recorded.text, "\"ab");
        assert!(recorded.more);
        let cut = Recorded {
            text: "aé".to_owned(),
            more: false,
        }
        .within(2);
        assert_eq!(
            cut,
            Recorded {
                text: "a".to_owned(),
                more: true
            }
        );
    }

    #[test]
    fn a_run_of_plain_string_bytes_is_written_in_one_piece() {
        let mut writer = Canonical::new(1 << 20);
        for &byte in b"[\"ab" {
            writer.feed(byte);
        }
        writer.feed_run("cdé\u{1}".as_bytes());
        writer.feed(b'\\');
        writer.feed(b'u');
        writer.feed_run(b"0041rest");
        assert_eq!(writer.feed(b'"'), Fed::More);
        assert_eq!(writer.feed(b']'), Fed::Done);
        assert_eq!(writer.finish().text, "[\"abcd\u{e9}\\u0001Arest\"]");
    }
}
