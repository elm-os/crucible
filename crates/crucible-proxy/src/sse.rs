//! A streaming reader of `text/event-stream` bodies that hands the `data` of each event to a [`Scanner`].
//!
//! Only `data` fields matter: each event of a Streamable HTTP response carries one JSON-RPC message. Comments
//! (keep-alives), `event`, `id` and `retry` fields are skipped. The bytes themselves are forwarded elsewhere,
//! untouched; this reader only looks at a copy.

use crate::scan::{Message, Scanner, StateKey};

/// The longest field name compared (`data`).
const NAME_CAP: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum At {
    /// At the start of a line, reading the field name.
    Name,
    /// After `data:`, before the optional space.
    Space,
    /// Inside the value of a `data` field.
    Data,
    /// Inside a line that is skipped.
    Skip,
}

/// See the module documentation.
#[derive(Debug)]
pub struct SseReader {
    scanner: Scanner,
    at: At,
    name: Vec<u8>,
    /// The last byte was a carriage return, so a line feed that follows belongs to the same line end.
    after_cr: bool,
    /// The current event has had a `data` line.
    has_data: bool,
}

impl Default for SseReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SseReader {
    /// A reader at the start of a stream.
    pub fn new() -> Self {
        Self {
            scanner: Scanner::document(),
            at: At::Name,
            name: Vec::new(),
            after_cr: false,
            has_data: false,
        }
    }

    /// The reader, hashing every `requestState` with `key` (see [`Scanner::keyed`]).
    pub fn keyed(mut self, key: Option<StateKey>) -> Self {
        self.scanner = self.scanner.keyed(key);
        self
    }

    /// See [`Scanner::record_results`].
    pub fn record_results(&mut self, window: usize) {
        self.scanner.record_results(window);
    }

    /// Takes the messages completed so far.
    pub fn take(&mut self) -> Vec<Message> {
        self.scanner.take()
    }

    /// Events whose data was not a complete JSON-RPC message.
    pub fn malformed(&self) -> u64 {
        self.scanner.malformed()
    }

    /// Bytes were lost before the next push. The reader resumes at the next event boundary it can recognise.
    pub fn gap(&mut self) {
        self.scanner.gap();
        self.at = At::Skip;
        self.after_cr = false;
        // The next blank line then resets the scanner, whatever was lost.
        self.has_data = true;
    }

    /// Feeds the next bytes of the body.
    pub fn push(&mut self, mut bytes: &[u8]) {
        while let Some(&byte) = bytes.first() {
            if self.after_cr {
                self.after_cr = false;
                if byte == b'\n' {
                    bytes = &bytes[1..];
                    continue;
                }
            }
            if byte == b'\n' || byte == b'\r' {
                self.after_cr = byte == b'\r';
                bytes = &bytes[1..];
                self.end_line();
                continue;
            }
            match self.at {
                At::Name => {
                    bytes = &bytes[1..];
                    if byte == b':' {
                        self.at = if self.name == b"data" {
                            self.has_data = true;
                            At::Space
                        } else {
                            At::Skip
                        };
                    } else if self.name.len() < NAME_CAP {
                        self.name.push(byte);
                    } else {
                        self.at = At::Skip;
                    }
                }
                At::Space => {
                    self.at = At::Data;
                    if byte == b' ' {
                        bytes = &bytes[1..];
                    }
                }
                At::Data | At::Skip => {
                    let end = memchr::memchr2(b'\n', b'\r', bytes).unwrap_or(bytes.len());
                    if self.at == At::Data {
                        self.scanner.push(&bytes[..end]);
                    }
                    bytes = &bytes[end..];
                }
            }
        }
    }

    fn end_line(&mut self) {
        let blank = self.at == At::Name && self.name.is_empty();
        if blank {
            // A blank line dispatches the event.
            if self.has_data {
                self.scanner.end_document();
            }
            self.has_data = false;
        } else if matches!(self.at, At::Data | At::Space) {
            // Several data lines of one event are joined with a line feed.
            self.scanner.push(b"\n");
        }
        self.at = At::Name;
        self.name.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::JsonId;

    const STREAM: &[u8] = b": keep-alive\n\nevent: message\nid: 1\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progressToken\":5}}\n\n:\r\ndata:{\"jsonrpc\":\"2.0\",\r\ndata: \"id\":3,\"result\":{}}\r\n\r\ndata: not json\n\ndatax: {\"id\":9,\"result\":{}}\n\ndata: {\"id\":4,\"result\":{}}\r\rdata: {\"id\":5,\"res";

    fn ids(messages: &[Message]) -> Vec<Option<JsonId>> {
        messages.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn events_become_messages_at_every_split_point() {
        let number = |n: &str| Some(JsonId::Number(n.to_owned()));
        let expected = vec![None, number("3"), number("4")];
        let mut whole = SseReader::new();
        whole.push(STREAM);
        let messages = whole.take();
        assert_eq!(ids(&messages), expected);
        assert_eq!(messages[0].progress_token, number("5"));
        assert_eq!(whole.malformed(), 1);
        for at in 0..STREAM.len() {
            let mut reader = SseReader::new();
            reader.push(&STREAM[..at]);
            reader.push(&STREAM[at..]);
            assert_eq!(reader.take(), messages, "split at {at}");
            assert_eq!(reader.malformed(), 1);
        }
    }

    #[test]
    fn a_gap_resumes_at_a_later_event() {
        let mut reader = SseReader::new();
        reader.push(b"data: {\"id\":1,\"resu");
        reader.gap();
        reader.push(b"lt\":{}}\n\ndata: {\"id\":2,\"result\":{}}\n\n");
        assert_eq!(ids(&reader.take()), [Some(JsonId::Number("2".to_owned()))]);
    }
}
