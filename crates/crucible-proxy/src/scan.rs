//! A streaming scanner for JSON-RPC messages.
//!
//! It takes the bytes of one direction of a connection in chunks of any size and yields one [`Message`] per
//! JSON-RPC message, holding only the handful of fields the proxy reports: the id, the method and a few named
//! members of `params`, `result` and `error`. Every other value is skipped by depth and never stored, so memory
//! stays bounded whatever the size of a message.
//!
//! The scanner is lenient: it never rejects, because the proxy forwards the bytes whatever they are. It only
//! needs string and nesting structure to be right to find its fields. In line mode (stdio framing: one message
//! per line) a newline inside an unfinished message, or text that is not JSON, drops that line and resumes at
//! the next one.
//!
//! A `requestState` is never kept: with a [`StateKey`], the scanner feeds its decoded bytes to a keyed hash as
//! they stream past and keeps only the hash (contract 9.6).
//!
//! Content capture (contract 9.10) records two kinds of value as canonical JSON text, each at most its rule's
//! window: `params.arguments` of a `tools/call` request that a rule captures, and `content`, `structuredContent`
//! and `isError` of a result while some captured call waits for one. A request's capture is decided when its tool
//! name is read, so arguments that follow the name are recorded only for a sampled call. Arguments that come before
//! the name are recorded provisionally, at most the largest window of any rule, and dropped at the end of the
//! message unless the call turns out to be captured.

use std::collections::BTreeMap;
use std::fmt;

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

use crate::canonical::{Canonical, Fed, Recorded};
use crate::capture::ContentCapture;
use crate::core::MAX_ERROR_MESSAGE;
use crate::scrub;

/// The longest key the scanner compares; a longer key is no key it knows.
const KEY_CAP: usize = 48;
/// The most bytes kept of a captured string value.
const VALUE_CAP: usize = 512;
/// The most bytes kept of an error message or of the text an error message is taken from: what scrubbing reads
/// for the error message cap.
const MESSAGE_CAP: usize = scrub::window(MAX_ERROR_MESSAGE);
/// The label in front of every `requestState` in its keyed hash (contract 9.6).
const STATE_LABEL: &[u8] = b"crucible.mrtr.v1\0";
/// The most bytes kept of a number or literal.
const LITERAL_CAP: usize = 40;
/// Nesting levels inside a message whose keys are tracked. Deeper values are skipped.
const LEVELS: usize = 4;
/// Distinct content block types counted per message (contract 3.5: 16 keys).
const CONTENT_TYPES: usize = 16;

/// A JSON-RPC id. A number and a string with the same text are different ids.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum JsonId {
    /// A number, as written.
    Number(String),
    /// A string.
    String(String),
}

impl JsonId {
    /// The string form stored in `jsonrpc_id`.
    pub fn text(&self) -> &str {
        match self {
            Self::Number(text) | Self::String(text) => text,
        }
    }
}

/// The key of the multi-round state hashes (contract 9.6): HMAC-SHA256 keyed with the sender's secret key and
/// already fed the label, so each `requestState` is hashed while it streams past and never kept.
#[derive(Clone)]
pub struct StateKey(Hmac<Sha256>);

impl StateKey {
    /// The key for a project secret key. HMAC takes keys of any length, so this is `None` only in theory.
    pub fn new(secret_key: &str) -> Option<Self> {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes()).ok()?;
        mac.update(STATE_LABEL);
        Some(Self(mac))
    }
}

impl fmt::Debug for StateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key never reaches a log.
        f.write_str("StateKey(..)")
    }
}

/// What the scanner kept of one JSON-RPC message.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Message {
    /// `id`, when present and not null.
    pub id: Option<JsonId>,
    /// `method`.
    pub method: Option<String>,
    /// The message has a `result` member.
    pub has_result: bool,
    /// The message has an `error` member.
    pub has_error: bool,
    /// `params.name`: the tool or prompt.
    pub name: Option<String>,
    /// `params.requestId` of a cancellation notification.
    pub request_id: Option<JsonId>,
    /// `params.progressToken` of a progress notification.
    pub progress_token: Option<JsonId>,
    /// `params._meta.progressToken` of a request.
    pub meta_progress_token: Option<JsonId>,
    /// `params._meta.traceparent`.
    pub traceparent: Option<String>,
    /// The protocol version: `params.protocolVersion` or `result.protocolVersion` of the initialize exchange,
    /// or the per-request `_meta` key of the modern era.
    pub protocol_version: Option<String>,
    /// The protocol version came from `_meta`, which only the modern era sends.
    pub version_in_meta: bool,
    /// `clientInfo.name`, from `params` or from `params._meta`.
    pub client_name: Option<String>,
    /// `clientInfo.version`.
    pub client_version: Option<String>,
    /// `serverInfo.name`, from `result` or from `result._meta`.
    pub server_name: Option<String>,
    /// `serverInfo.version`.
    pub server_version: Option<String>,
    /// `result.isError` is true.
    pub is_error: bool,
    /// `result.resultType`.
    pub result_type: Option<String>,
    /// `error.code`.
    pub error_code: Option<i32>,
    /// `result.content[*].type`, counted per type.
    pub content_blocks: BTreeMap<String, u32>,
    /// `error.message`, its first bytes: as much as scrubbing reads for the error message cap.
    pub error_message: Option<String>,
    /// The `text` of the first `result.content` block of type `text`, kept like `error_message`: the error
    /// message of a result with `isError: true`.
    pub tool_text: Option<String>,
    /// The keyed hash of `params.requestState`, with a [`StateKey`].
    pub state_in: Option<[u8; 16]>,
    /// The keyed hash of `result.requestState`, with a [`StateKey`].
    pub state_out: Option<[u8; 16]>,
    /// The message was an element of a batch array.
    pub is_batch: bool,
    /// The size of the message in bytes, from its opening brace to its closing brace.
    pub bytes: u64,
    /// The index of the capture rule that captures this `tools/call` request, when the call is sampled.
    pub capture_rule: Option<usize>,
    /// `params.arguments` as canonical JSON text, when the call is captured and its rule takes arguments.
    pub arguments: Option<Recorded>,
    /// The result's members that capture takes, recorded while a captured call waits for its result.
    pub result_members: ResultMembers,
}

/// The members of a tool result that content capture takes, each as canonical JSON text.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ResultMembers {
    /// `result.content`.
    pub content: Option<Recorded>,
    /// `result.structuredContent`.
    pub structured_content: Option<Recorded>,
    /// `result.isError`.
    pub is_error: Option<Recorded>,
}

/// Where a recorded value goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Arguments,
    Content,
    StructuredContent,
    IsError,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Key {
    Other,
    Elem,
    Id,
    Method,
    Params,
    Result,
    Error,
    Name,
    Meta,
    RequestId,
    ProgressToken,
    ProtocolVersion,
    ClientInfo,
    ServerInfo,
    Version,
    Traceparent,
    MetaProtocolVersion,
    MetaClientInfo,
    MetaServerInfo,
    IsError,
    ResultType,
    Code,
    Content,
    Type,
    Text,
    ErrorMessage,
    RequestState,
    Arguments,
    StructuredContent,
}

fn key_of(text: &[u8]) -> Key {
    match text {
        b"id" => Key::Id,
        b"method" => Key::Method,
        b"params" => Key::Params,
        b"result" => Key::Result,
        b"error" => Key::Error,
        b"name" => Key::Name,
        b"_meta" => Key::Meta,
        b"requestId" => Key::RequestId,
        b"progressToken" => Key::ProgressToken,
        b"protocolVersion" => Key::ProtocolVersion,
        b"clientInfo" => Key::ClientInfo,
        b"serverInfo" => Key::ServerInfo,
        b"version" => Key::Version,
        b"traceparent" => Key::Traceparent,
        b"io.modelcontextprotocol/protocolVersion" => Key::MetaProtocolVersion,
        b"io.modelcontextprotocol/clientInfo" => Key::MetaClientInfo,
        b"io.modelcontextprotocol/serverInfo" => Key::MetaServerInfo,
        b"isError" => Key::IsError,
        b"resultType" => Key::ResultType,
        b"code" => Key::Code,
        b"content" => Key::Content,
        b"type" => Key::Type,
        b"text" => Key::Text,
        b"message" => Key::ErrorMessage,
        b"requestState" => Key::RequestState,
        b"arguments" => Key::Arguments,
        b"structuredContent" => Key::StructuredContent,
        _ => Key::Other,
    }
}

/// The message field a scalar at a key path fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Id,
    Method,
    Name,
    RequestId,
    ProgressToken,
    MetaProgressToken,
    Traceparent,
    ProtocolVersion,
    MetaProtocolVersion,
    ClientName,
    ClientVersion,
    ServerName,
    ServerVersion,
    IsError,
    ResultType,
    ErrorCode,
    ContentType,
    ContentText,
    ErrorMessage,
    StateIn,
    StateOut,
}

fn field_of(path: &[Key]) -> Option<Field> {
    use Key as K;
    Some(match path {
        [K::Id] => Field::Id,
        [K::Method] => Field::Method,
        [K::Params, K::Name] => Field::Name,
        [K::Params, K::RequestId] => Field::RequestId,
        [K::Params, K::ProgressToken] => Field::ProgressToken,
        [K::Params | K::Result, K::ProtocolVersion] => Field::ProtocolVersion,
        [K::Params, K::ClientInfo, K::Name] | [K::Params, K::Meta, K::MetaClientInfo, K::Name] => {
            Field::ClientName
        }
        [K::Params, K::ClientInfo, K::Version]
        | [K::Params, K::Meta, K::MetaClientInfo, K::Version] => Field::ClientVersion,
        [K::Params, K::Meta, K::ProgressToken] => Field::MetaProgressToken,
        [K::Params, K::Meta, K::Traceparent] => Field::Traceparent,
        [K::Params, K::Meta, K::MetaProtocolVersion] => Field::MetaProtocolVersion,
        [K::Result, K::ServerInfo, K::Name] | [K::Result, K::Meta, K::MetaServerInfo, K::Name] => {
            Field::ServerName
        }
        [K::Result, K::ServerInfo, K::Version]
        | [K::Result, K::Meta, K::MetaServerInfo, K::Version] => Field::ServerVersion,
        [K::Result, K::IsError] => Field::IsError,
        [K::Result, K::ResultType] => Field::ResultType,
        [K::Error, K::Code] => Field::ErrorCode,
        [K::Result, K::Content, K::Elem, K::Type] => Field::ContentType,
        [K::Result, K::Content, K::Elem, K::Text] => Field::ContentText,
        [K::Error, K::ErrorMessage] => Field::ErrorMessage,
        [K::Params, K::RequestState] => Field::StateIn,
        [K::Result, K::RequestState] => Field::StateOut,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug)]
struct Level {
    object: bool,
    expect_key: bool,
    key: Key,
}

const NO_LEVEL: Level = Level {
    object: false,
    expect_key: false,
    key: Key::Other,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Capture {
    /// The string is skipped.
    None,
    /// An object key.
    Key,
    /// A value for this field.
    Value(Field),
    /// A value whose keyed hash fills this field.
    Hash(Field),
}

/// What the scanner learned of the `result.content` block it is in.
#[derive(Clone, Debug, Default)]
struct Block {
    /// Its `type` is `text`, once the type was read.
    is_text: Option<bool>,
    /// Its `text`.
    text: Option<String>,
}

/// The escape state inside a captured string.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Escape {
    None,
    /// After a backslash.
    Start,
    /// Inside `\uXXXX`: digits still expected, and the code unit so far.
    Unicode(u8, u16),
}

/// See the module documentation.
#[derive(Debug)]
pub struct Scanner {
    line_mode: bool,
    /// Line mode: drop bytes up to the next newline. Otherwise: drop everything (the body is not JSON).
    skipping: bool,
    /// Open containers, the batch array included.
    depth: u32,
    in_batch: bool,
    /// The depth inside the message object, or 0 when no message is open.
    message_depth: u32,
    levels: [Level; LEVELS],
    in_string: bool,
    capture: Capture,
    escape: Escape,
    /// A high surrogate waiting for its low half.
    high_surrogate: Option<u16>,
    buffer: Vec<u8>,
    literal: Vec<u8>,
    in_literal: bool,
    message: Message,
    malformed: u64,
    out: Vec<Message>,
    state_key: Option<StateKey>,
    /// The keyed hash of the `requestState` being read.
    state_mac: Option<Hmac<Sha256>>,
    /// The `result.content` block being read.
    block: Block,
    /// The capture rules, on a scanner of requests.
    content: Option<ContentCapture>,
    /// The window of a recorded result member while results are recorded, else 0.
    result_window: usize,
    /// The capture decision of the message being read, once its tool name was read.
    decision: Option<Option<usize>>,
    /// The value being recorded, and where it goes.
    recording: Option<(Slot, Canonical)>,
}

impl Scanner {
    /// A scanner for stdio framing: one message, or one batch array, per line.
    pub fn lines() -> Self {
        Self::new(true)
    }

    /// A scanner for one JSON document that may span lines: an HTTP body, or the data of one SSE event.
    pub fn document() -> Self {
        Self::new(false)
    }

    fn new(line_mode: bool) -> Self {
        Self {
            line_mode,
            skipping: false,
            depth: 0,
            in_batch: false,
            message_depth: 0,
            levels: [NO_LEVEL; LEVELS],
            in_string: false,
            capture: Capture::None,
            escape: Escape::None,
            high_surrogate: None,
            buffer: Vec::new(),
            literal: Vec::new(),
            in_literal: false,
            message: Message::default(),
            malformed: 0,
            out: Vec::new(),
            state_key: None,
            state_mac: None,
            block: Block::default(),
            content: None,
            result_window: 0,
            decision: None,
            recording: None,
        }
    }

    /// The scanner, hashing every `requestState` with `key` (contract 9.6); without a key none is read.
    pub fn keyed(mut self, key: Option<StateKey>) -> Self {
        self.state_key = key;
        self
    }

    /// The scanner of requests, recording the arguments of the calls that `capture` captures (contract 9.10);
    /// without rules no argument is read.
    pub fn capturing(mut self, capture: Option<ContentCapture>) -> Self {
        self.content = capture;
        self
    }

    /// Records the result members of the responses that start from now on, each at most `window` bytes; 0 stops
    /// recording. The core sets it while some captured call waits for its result.
    pub fn record_results(&mut self, window: usize) {
        self.result_window = window;
    }

    /// Lines or documents dropped because they were not JSON-RPC, or were cut short.
    pub fn malformed(&self) -> u64 {
        self.malformed
    }

    /// Takes the messages completed so far, in order.
    pub fn take(&mut self) -> Vec<Message> {
        std::mem::take(&mut self.out)
    }

    /// Bytes were lost before the next push (the analysis queue was full). In line mode the scanner resumes at
    /// the next newline; a document scanner stops, since the rest of the document cannot be placed.
    pub fn gap(&mut self) {
        if self.depth > 0 || self.in_string {
            self.malformed += 1;
        }
        self.reset();
        self.skipping = true;
    }

    /// The document ended (end of an HTTP body or of an SSE event). An unfinished message is dropped, and a
    /// document scanner is ready for the next document.
    pub fn end_document(&mut self) {
        if self.depth > 0 || self.in_string {
            self.malformed += 1;
        }
        self.reset();
        self.skipping = false;
    }

    fn reset(&mut self) {
        self.depth = 0;
        self.in_batch = false;
        self.message_depth = 0;
        self.in_string = false;
        self.capture = Capture::None;
        self.escape = Escape::None;
        self.high_surrogate = None;
        self.in_literal = false;
        self.message = Message::default();
        self.state_mac = None;
        self.block = Block::default();
        self.decision = None;
        self.recording = None;
    }

    fn fail(&mut self) {
        self.malformed += 1;
        self.reset();
        self.skipping = true;
    }

    /// Feeds the next bytes of the stream.
    pub fn push(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            if self.skipping {
                if !self.line_mode {
                    return;
                }
                match memchr::memchr(b'\n', bytes) {
                    Some(at) => {
                        self.skipping = false;
                        bytes = &bytes[at + 1..];
                    }
                    None => return,
                }
                continue;
            }
            if self.in_string && self.capture == Capture::None && self.escape == Escape::None {
                // Skip to the next byte that can end the string, or the line.
                let Some(at) = memchr::memchr3(b'"', b'\\', b'\n', bytes) else {
                    self.count(bytes.len());
                    self.record_run(bytes);
                    return;
                };
                self.count(at);
                self.record_run(&bytes[..at]);
                bytes = &bytes[at..];
            }
            let byte = bytes[0];
            bytes = &bytes[1..];
            self.count(1);
            // A value that starts at this byte starts its recording here, so the byte is recorded after.
            self.byte(byte);
            self.record_byte(byte);
        }
    }

    /// Feeds a run of string bytes, which holds no quote or backslash, to the recording.
    fn record_run(&mut self, run: &[u8]) {
        if let Some((_, writer)) = &mut self.recording {
            writer.feed_run(run);
        }
    }

    /// Feeds one byte to the recording, and stores the recorded value when it ends.
    fn record_byte(&mut self, byte: u8) {
        let Some((slot, writer)) = &mut self.recording else {
            return;
        };
        if writer.feed(byte) == Fed::More {
            return;
        }
        let slot = *slot;
        let Some((_, writer)) = self.recording.take() else {
            return;
        };
        let recorded = writer.finish();
        let members = &mut self.message.result_members;
        match slot {
            Slot::Arguments => self.message.arguments = Some(recorded),
            Slot::Content => members.content = Some(recorded),
            Slot::StructuredContent => members.structured_content = Some(recorded),
            Slot::IsError => members.is_error = Some(recorded),
        }
    }

    /// Starts recording the value that starts here when capture takes it: `params.arguments` of a call that may
    /// be captured, or a result member while results are recorded.
    fn start_recording(&mut self) {
        if self.recording.is_some() || self.level() != Some(1) {
            return;
        }
        let (slot, limit) = match (self.levels[0].key, self.levels[1].key) {
            (Key::Params, Key::Arguments) => (Slot::Arguments, self.arguments_window()),
            (Key::Result, Key::Content) => (Slot::Content, self.result_window),
            (Key::Result, Key::StructuredContent) => (Slot::StructuredContent, self.result_window),
            (Key::Result, Key::IsError) => (Slot::IsError, self.result_window),
            _ => return,
        };
        if limit > 0 {
            self.recording = Some((slot, Canonical::new(limit)));
        }
    }

    /// How much of `params.arguments` to record: the window of the rule that captures the call, or provisionally
    /// the largest window of a rule that takes arguments while the tool is not known yet; 0 for none.
    fn arguments_window(&self) -> usize {
        let Some(capture) = &self.content else {
            return 0;
        };
        if self
            .message
            .method
            .as_deref()
            .is_some_and(|method| method != "tools/call")
        {
            return 0;
        }
        match self.decision {
            Some(Some(rule)) => capture
                .rule(rule)
                .filter(|rule| rule.args)
                .map_or(0, |rule| rule.window()),
            Some(None) => 0,
            None => capture.args_window(),
        }
    }

    /// Decides the capture of the message once its tool name is read: at most one draw per message.
    fn decide(&mut self) {
        let Some(capture) = &self.content else {
            return;
        };
        if self.decision.is_some() {
            return;
        }
        let call = self
            .message
            .method
            .as_deref()
            .is_none_or(|method| method == "tools/call");
        self.decision = Some(match &self.message.name {
            Some(name) if call => capture.decide(name),
            _ => None,
        });
    }

    /// The message ended: the arguments stay only for a captured call, cut to its rule's window.
    fn finish_capture(&mut self) {
        let Some(capture) = &self.content else {
            return;
        };
        let call = self.message.method.as_deref() == Some("tools/call");
        let rule = self
            .decision
            .flatten()
            .filter(|_| call)
            .and_then(|index| Some((index, capture.rule(index)?)));
        let arguments = self.message.arguments.take();
        if let Some((index, rule)) = rule {
            self.message.capture_rule = Some(index);
            if rule.args {
                self.message.arguments = arguments.map(|recorded| recorded.within(rule.window()));
            }
        }
    }

    fn count(&mut self, bytes: usize) {
        if self.message_depth > 0 {
            self.message.bytes += bytes as u64;
        }
    }

    /// The index in `levels` of the innermost open container of the message, when it is tracked.
    fn level(&self) -> Option<usize> {
        let index = self.depth.checked_sub(self.message_depth)? as usize;
        (self.message_depth > 0 && index < LEVELS).then_some(index)
    }

    /// The field that a scalar value at the current position fills.
    fn field(&self) -> Option<Field> {
        let index = self.level()?;
        let mut path = [Key::Other; LEVELS];
        for (slot, level) in path.iter_mut().zip(&self.levels[..=index]) {
            *slot = level.key;
        }
        field_of(&path[..=index])
    }

    /// A value starts at the current position: note a top-level `result` or `error`, and start recording a value
    /// that capture takes.
    fn value_starts(&mut self) {
        if self.level() == Some(0) {
            match self.levels[0].key {
                Key::Result => self.message.has_result = true,
                Key::Error => self.message.has_error = true,
                _ => {}
            }
        }
        self.start_recording();
    }

    fn byte(&mut self, byte: u8) {
        if self.line_mode && byte == b'\n' {
            if self.depth > 0 || self.in_string {
                // A message must fit on its line.
                self.malformed += 1;
                self.reset();
            }
            return;
        }
        if self.in_string {
            self.string_byte(byte);
            return;
        }
        match byte {
            b'"' => {
                self.end_literal();
                self.in_string = true;
                self.escape = Escape::None;
                self.high_surrogate = None;
                self.buffer.clear();
                self.capture = match self.level() {
                    Some(index) if self.levels[index].object && self.levels[index].expect_key => {
                        Capture::Key
                    }
                    _ if self.message_depth == 0 => {
                        self.fail();
                        return;
                    }
                    _ => {
                        self.value_starts();
                        self.capture_for(self.field())
                    }
                };
            }
            b'{' | b'[' => self.open(byte == b'{'),
            b'}' | b']' => self.close(),
            b',' => {
                self.end_literal();
                if let Some(index) = self.level()
                    && self.levels[index].object
                {
                    self.levels[index].expect_key = true;
                }
            }
            b':' | b' ' | b'\t' | b'\r' | b'\n' => self.end_literal(),
            _ => {
                if self.message_depth == 0 {
                    // Text outside a message is not JSON-RPC.
                    self.fail();
                    return;
                }
                if !self.in_literal {
                    self.in_literal = true;
                    self.literal.clear();
                    self.value_starts();
                }
                if self.literal.len() < LITERAL_CAP {
                    self.literal.push(byte);
                }
            }
        }
    }

    /// How a string value for `field` is read: a `requestState` is hashed, with a key, and never kept.
    fn capture_for(&mut self, field: Option<Field>) -> Capture {
        match field {
            Some(field @ (Field::StateIn | Field::StateOut)) => {
                self.state_mac = self.state_key.as_ref().map(|key| key.0.clone());
                if self.state_mac.is_some() {
                    Capture::Hash(field)
                } else {
                    Capture::None
                }
            }
            Some(field) => Capture::Value(field),
            None => Capture::None,
        }
    }

    /// Whether the innermost open container is a block of `result.content`.
    fn in_block(&self) -> bool {
        self.level() == Some(3)
            && self.levels[3].object
            && matches!(
                [self.levels[0].key, self.levels[1].key, self.levels[2].key],
                [Key::Result, Key::Content, Key::Elem]
            )
    }

    fn open(&mut self, object: bool) {
        self.end_literal();
        if self.message_depth == 0 {
            if object {
                // A message starts: at the top level, or as an element of a batch array.
                self.depth += 1;
                self.message_depth = self.depth;
                self.message = Message {
                    is_batch: self.in_batch,
                    bytes: 1,
                    ..Message::default()
                };
                self.decision = None;
                self.recording = None;
                self.levels[0] = Level {
                    object: true,
                    expect_key: true,
                    key: Key::Other,
                };
            } else if self.depth == 0 {
                self.depth = 1;
                self.in_batch = true;
            } else {
                self.fail();
            }
            return;
        }
        self.value_starts();
        self.depth += 1;
        if let Some(index) = self.level() {
            self.levels[index] = Level {
                object,
                expect_key: object,
                key: if object { Key::Other } else { Key::Elem },
            };
        }
        if self.in_block() {
            self.block = Block::default();
        }
    }

    fn close(&mut self) {
        self.end_literal();
        if self.depth == 0 {
            self.fail();
            return;
        }
        if self.in_block() {
            // The first block of type `text` with a text gives the text of an error result.
            let block = std::mem::take(&mut self.block);
            if self.message.tool_text.is_none() && block.is_text == Some(true) {
                self.message.tool_text = block.text;
            }
        }
        self.depth -= 1;
        if self.message_depth > 0 && self.depth < self.message_depth {
            self.message_depth = 0;
            self.finish_capture();
            self.out.push(std::mem::take(&mut self.message));
        } else if self.depth == 0 {
            self.in_batch = false;
        }
    }

    fn string_byte(&mut self, byte: u8) {
        if self.capture == Capture::None {
            match self.escape {
                Escape::None => match byte {
                    b'\\' => self.escape = Escape::Start,
                    b'"' => self.in_string = false,
                    _ => {}
                },
                // Whatever follows a backslash is part of the string; `\uXXXX` needs no care when skipping.
                _ => self.escape = Escape::None,
            }
            return;
        }
        match self.escape {
            Escape::None => match byte {
                b'\\' => self.escape = Escape::Start,
                b'"' => self.end_string(),
                _ => {
                    self.flush_surrogate();
                    self.keep(&[byte]);
                }
            },
            Escape::Start => {
                self.escape = Escape::None;
                let plain = match byte {
                    b'u' => {
                        self.escape = Escape::Unicode(4, 0);
                        return;
                    }
                    b'n' => b'\n',
                    b't' => b'\t',
                    b'r' => b'\r',
                    b'b' => 0x08,
                    b'f' => 0x0c,
                    other => other,
                };
                self.flush_surrogate();
                self.keep(&[plain]);
            }
            Escape::Unicode(left, unit) => {
                let digit = match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => {
                        // Not an escape after all; keep going as plain text.
                        self.escape = Escape::None;
                        self.keep_char(char::REPLACEMENT_CHARACTER);
                        self.string_byte(byte);
                        return;
                    }
                };
                let unit = (unit << 4) | u16::from(digit);
                if left > 1 {
                    self.escape = Escape::Unicode(left - 1, unit);
                    return;
                }
                self.escape = Escape::None;
                self.code_unit(unit);
            }
        }
    }

    fn code_unit(&mut self, unit: u16) {
        match (self.high_surrogate.take(), unit) {
            (Some(high), 0xDC00..=0xDFFF) => {
                let scalar =
                    0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                self.keep_char(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            (pending, unit) => {
                if pending.is_some() {
                    self.keep_char(char::REPLACEMENT_CHARACTER);
                }
                match unit {
                    0xD800..=0xDBFF => self.high_surrogate = Some(unit),
                    _ => self.keep_char(
                        char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER),
                    ),
                }
            }
        }
    }

    /// A high surrogate that no low half followed is not a character.
    fn flush_surrogate(&mut self) {
        if self.high_surrogate.take().is_some() {
            self.keep_char(char::REPLACEMENT_CHARACTER);
        }
    }

    fn keep_char(&mut self, c: char) {
        let mut utf8 = [0u8; 4];
        self.keep(c.encode_utf8(&mut utf8).as_bytes());
    }

    fn keep(&mut self, bytes: &[u8]) {
        let cap = match self.capture {
            // One past the cap marks a key as too long to be a known one.
            Capture::Key => KEY_CAP + 1,
            Capture::Value(Field::ErrorMessage | Field::ContentText) => MESSAGE_CAP,
            Capture::Hash(_) => {
                if let Some(mac) = &mut self.state_mac {
                    mac.update(bytes);
                }
                return;
            }
            Capture::Value(_) | Capture::None => VALUE_CAP,
        };
        let room = cap.saturating_sub(self.buffer.len());
        self.buffer
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        if cap == MESSAGE_CAP
            && self.buffer.len() >= MESSAGE_CAP
            && let Capture::Value(field) = self.capture
        {
            // The rest of a long message is never read: keep what is held and skip to the end of the string.
            self.capture = Capture::None;
            let text = String::from_utf8_lossy(&self.buffer).into_owned();
            self.set_text(field, text);
        }
    }

    fn end_string(&mut self) {
        self.flush_surrogate();
        self.in_string = false;
        let capture = std::mem::replace(&mut self.capture, Capture::None);
        match capture {
            Capture::None => {}
            Capture::Key => {
                if let Some(index) = self.level() {
                    self.levels[index].key = key_of(&self.buffer);
                    self.levels[index].expect_key = false;
                }
            }
            Capture::Value(field) => {
                let text = String::from_utf8_lossy(&self.buffer).into_owned();
                self.set_text(field, text);
            }
            Capture::Hash(field) => {
                let Some(mac) = self.state_mac.take() else {
                    return;
                };
                let mut hash = [0u8; 16];
                hash.copy_from_slice(&mac.finalize().into_bytes()[..16]);
                match field {
                    Field::StateIn => self.message.state_in = Some(hash),
                    _ => self.message.state_out = Some(hash),
                }
            }
        }
    }

    /// A string value for `field`. A string where a number or boolean belongs is ignored.
    fn set_text(&mut self, field: Field, text: String) {
        let message = &mut self.message;
        match field {
            Field::Id => message.id = Some(JsonId::String(text)),
            Field::Method => message.method = Some(text),
            Field::Name => {
                message.name = Some(text);
                self.decide();
            }
            Field::RequestId => message.request_id = Some(JsonId::String(text)),
            Field::ProgressToken => message.progress_token = Some(JsonId::String(text)),
            Field::MetaProgressToken => message.meta_progress_token = Some(JsonId::String(text)),
            Field::Traceparent => message.traceparent = Some(text),
            Field::ProtocolVersion => {
                // The per-request `_meta` key wins over the initialize member, whatever the order.
                if !message.version_in_meta {
                    message.protocol_version = Some(text);
                }
            }
            Field::MetaProtocolVersion => {
                message.protocol_version = Some(text);
                message.version_in_meta = true;
            }
            Field::ClientName => message.client_name = Some(text),
            Field::ClientVersion => message.client_version = Some(text),
            Field::ServerName => message.server_name = Some(text),
            Field::ServerVersion => message.server_version = Some(text),
            Field::ResultType => message.result_type = Some(text),
            Field::ContentType => {
                self.block.is_text = Some(text == "text");
                let known = message.content_blocks.len();
                if let Some(count) = message.content_blocks.get_mut(&text) {
                    *count = count.saturating_add(1);
                } else if known < CONTENT_TYPES {
                    message.content_blocks.insert(text, 1);
                }
            }
            Field::ContentText => self.block.text = Some(text),
            Field::ErrorMessage => message.error_message = Some(text),
            Field::IsError | Field::ErrorCode | Field::StateIn | Field::StateOut => {}
        }
    }

    /// A number, boolean or null for the field at the current position. A literal where a string belongs is
    /// ignored.
    fn end_literal(&mut self) {
        if !self.in_literal {
            return;
        }
        self.in_literal = false;
        let Some(field) = self.field() else {
            return;
        };
        let text = String::from_utf8_lossy(&self.literal).into_owned();
        let number = || {
            let first = text.bytes().next()?;
            (first == b'-' || first.is_ascii_digit()).then(|| JsonId::Number(text.clone()))
        };
        let message = &mut self.message;
        match field {
            Field::Id => message.id = number(),
            Field::RequestId => message.request_id = number(),
            Field::ProgressToken => message.progress_token = number(),
            Field::MetaProgressToken => message.meta_progress_token = number(),
            Field::IsError => message.is_error = text == "true",
            Field::ErrorCode => message.error_code = text.parse().ok(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(mut scanner: Scanner, chunks: &[&[u8]]) -> (Vec<Message>, u64) {
        for chunk in chunks {
            scanner.push(chunk);
        }
        (scanner.take(), scanner.malformed())
    }

    #[test]
    fn a_request_a_response_and_a_notification() {
        let wire = concat!(
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"echo","arguments":{"name":"inner","x":[1,{"id":9}]},"_meta":{"progressToken":"p1","traceparent":"00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"result":{"content":[{"type":"text","text":"hi"},{"type":"text","text":"}"},{"type":"image"}],"isError":true}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"p1","progress":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":"abc","error":{"code":-32601,"message":"no"}}"#,
            "\n",
        );
        let (messages, malformed) = scan(Scanner::lines(), &[wire.as_bytes()]);
        assert_eq!(malformed, 0);
        assert_eq!(messages.len(), 4);
        let request = &messages[0];
        assert_eq!(request.id, Some(JsonId::Number("7".to_owned())));
        assert_eq!(request.method.as_deref(), Some("tools/call"));
        assert_eq!(request.name.as_deref(), Some("echo"));
        assert_eq!(
            request.meta_progress_token,
            Some(JsonId::String("p1".to_owned()))
        );
        assert!(
            request
                .traceparent
                .as_deref()
                .unwrap()
                .starts_with("00-0af7")
        );
        assert!(!request.has_result && !request.has_error);
        assert_eq!(request.bytes as usize, wire.lines().next().unwrap().len());
        let response = &messages[1];
        assert!(response.has_result && response.is_error && response.method.is_none());
        assert_eq!(
            response.content_blocks,
            BTreeMap::from([("text".to_owned(), 2), ("image".to_owned(), 1)])
        );
        assert_eq!(
            messages[2].progress_token,
            Some(JsonId::String("p1".to_owned()))
        );
        assert_eq!(messages[2].id, None);
        assert_eq!(messages[3].id, Some(JsonId::String("abc".to_owned())));
        assert_eq!(messages[3].error_code, Some(-32601));
        assert!(messages[3].has_error);
    }

    #[test]
    fn every_split_point_gives_the_same_messages() {
        let wire = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"cé😀\"x","version":"1.2"}}}"#,
            "\n",
            r#"[{"id":2,"method":"a"},{"id":3,"result":{"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"s","version":"9"}}}}]"#,
            "\n"
        )
        .as_bytes();
        let (whole, _) = scan(Scanner::lines(), &[wire]);
        assert_eq!(whole.len(), 3);
        assert_eq!(whole[0].client_name.as_deref(), Some("c\u{e9}\u{1f600}\"x"));
        assert_eq!(whole[0].protocol_version.as_deref(), Some("2025-06-18"));
        assert!(whole[1].is_batch && whole[2].is_batch && !whole[0].is_batch);
        assert_eq!(whole[2].server_name.as_deref(), Some("s"));
        assert_eq!(whole[2].server_version.as_deref(), Some("9"));
        for at in 0..wire.len() {
            let (split, malformed) = scan(Scanner::lines(), &[&wire[..at], &wire[at..]]);
            assert_eq!(split, whole, "split at {at}");
            assert_eq!(malformed, 0);
        }
    }

    #[test]
    fn line_mode_drops_text_and_cut_lines_and_resumes() {
        let wire = b"starting server...\n{\"id\":1,\"method\":\"a\n{\"id\":2,\"method\":\"b\"}\n} stray\n{\"id\":3,\"method\":\"c\"}\n";
        let (messages, malformed) = scan(Scanner::lines(), &[wire]);
        let ids: Vec<_> = messages.iter().map(|m| m.id.clone().unwrap()).collect();
        assert_eq!(
            ids,
            [
                JsonId::Number("2".to_owned()),
                JsonId::Number("3".to_owned())
            ]
        );
        assert_eq!(malformed, 3);
    }

    #[test]
    fn a_gap_resumes_at_the_next_line_and_a_document_ends() {
        let mut scanner = Scanner::lines();
        scanner.push(b"{\"id\":1,\"method\":\"a\",\"params\":{");
        scanner.gap();
        scanner.push(b"\"x\":1}}\n{\"id\":2,\"method\":\"b\"}\n");
        let messages = scanner.take();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].method.as_deref(), Some("b"));

        let mut scanner = Scanner::document();
        scanner.push(b"{\n  \"id\": 1,\n  \"result\": {}\n}\n");
        scanner.end_document();
        scanner.push(b"{\"id\": 2, \"resu");
        scanner.end_document();
        scanner.push(b"<html>");
        scanner.push(b"{\"id\":3}");
        scanner.end_document();
        scanner.push(b"{\"id\":4,\"result\":null}");
        let messages = scanner.take();
        let ids: Vec<_> = messages.iter().map(|m| m.id.clone().unwrap()).collect();
        assert_eq!(
            ids,
            [
                JsonId::Number("1".to_owned()),
                JsonId::Number("4".to_owned())
            ]
        );
        assert!(messages[0].has_result && messages[1].has_result);
        assert_eq!(scanner.malformed(), 2);
    }

    #[test]
    fn values_are_bounded_and_null_ids_are_absent() {
        let long = "x".repeat(10_000);
        let wire = format!(
            "{{\"id\":null,\"method\":\"{long}\",\"params\":{{\"{long}\":1,\"name\":\"n\"}}}}\n"
        );
        let (messages, _) = scan(Scanner::lines(), &[wire.as_bytes()]);
        assert_eq!(messages[0].id, None);
        assert_eq!(messages[0].method.as_ref().unwrap().len(), VALUE_CAP);
        assert_eq!(messages[0].name.as_deref(), Some("n"));
        assert_eq!(messages[0].bytes as usize, wire.len() - 1);
    }
}
