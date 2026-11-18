//! The `ExportTraceServiceRequest` schema: one parser per message.
//!
//! A parser walks the fields of one message once. It checks the wire type of every known field, UTF-8
//! in every string field and the length of every id; keeps the last occurrence of each singular scalar
//! field; refuses a second occurrence of a singular message field; and refuses unknown fields below the
//! nesting limit. Each nested message that may occur several times (a repeated field or a message in
//! the `AnyValue` oneof) is passed to `child`, so the validator can descend into it while the views
//! ignore it. Singular nested messages are returned for the caller to parse.

use crate::wire::{Fields, Value, WireError, unknown};

/// Length of an OTel trace id.
pub(crate) const TRACE_ID_LEN: usize = 16;
/// Length of an OTel span id.
pub(crate) const SPAN_ID_LEN: usize = 8;

/// The type of a nested message handed to a parser's `child` callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Msg {
    ResourceSpans,
    Resource,
    EntityRef,
    ScopeSpans,
    Scope,
    Span,
    Event,
    Link,
    Status,
    KeyValue,
    AnyValue,
    ArrayValue,
    KeyValueList,
}

fn wire_type(message: &'static str, field: u32) -> WireError {
    WireError::WireType { message, field }
}

fn utf8<'a>(message: &'static str, field: u32, bytes: &'a [u8]) -> Result<&'a str, WireError> {
    core::str::from_utf8(bytes).map_err(|_| WireError::Utf8 { message, field })
}

fn once<'a>(
    message: &'static str,
    field: u32,
    slot: &mut Option<&'a [u8]>,
    bytes: &'a [u8],
) -> Result<(), WireError> {
    match slot.replace(bytes) {
        Some(_) => Err(WireError::Duplicate { message, field }),
        None => Ok(()),
    }
}

/// Checks the final value of an id field: empty (absent) or exactly `len` bytes.
fn id(message: &'static str, field: u32, bytes: &[u8], len: usize) -> Result<(), WireError> {
    if bytes.is_empty() || bytes.len() == len {
        Ok(())
    } else {
        Err(WireError::IdLength {
            message,
            field,
            len: bytes.len(),
        })
    }
}

/// `ExportTraceServiceRequest`: `resource_spans` (1, repeated).
pub(crate) fn request<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<(), E> {
    const M: &str = "ExportTraceServiceRequest";
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => child(Msg::ResourceSpans, v)?,
            (1, _) => return Err(wire_type(M, 1).into()),
            _ => unknown(level)?,
        }
    }
    Ok(())
}

/// The singular fields of a `ResourceSpans`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ResourceSpans<'a> {
    pub resource: Option<&'a [u8]>,
}

/// `ResourceSpans`: `resource` (1), `scope_spans` (2, repeated), `schema_url` (3).
pub(crate) fn resource_spans<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<ResourceSpans<'a>, E> {
    const M: &str = "ResourceSpans";
    let mut out = ResourceSpans::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => once(M, 1, &mut out.resource, v)?,
            (2, Value::Len(v)) => child(Msg::ScopeSpans, v)?,
            (3, Value::Len(v)) => {
                utf8(M, 3, v)?;
            }
            (n @ 1..=3, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// `Resource`: `attributes` (1, repeated), `dropped_attributes_count` (2), `entity_refs` (3, repeated).
pub(crate) fn resource<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<(), E> {
    const M: &str = "Resource";
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => child(Msg::KeyValue, v)?,
            (2, Value::Varint(_)) => {}
            (3, Value::Len(v)) => child(Msg::EntityRef, v)?,
            (n @ 1..=3, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    Ok(())
}

/// `EntityRef`: `schema_url` (1), `type` (2), `id_keys` (3, repeated), `description_keys` (4, repeated),
/// all strings.
pub(crate) fn entity_ref(bytes: &[u8], level: u32) -> Result<(), WireError> {
    const M: &str = "EntityRef";
    for field in Fields::new(bytes, level) {
        match field? {
            (n @ 1..=4, Value::Len(v)) => {
                utf8(M, n, v)?;
            }
            (n @ 1..=4, _) => return Err(wire_type(M, n)),
            _ => unknown(level)?,
        }
    }
    Ok(())
}

/// The singular fields of a `ScopeSpans`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ScopeSpans<'a> {
    pub scope: Option<&'a [u8]>,
}

/// `ScopeSpans`: `scope` (1), `spans` (2, repeated), `schema_url` (3).
pub(crate) fn scope_spans<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<ScopeSpans<'a>, E> {
    const M: &str = "ScopeSpans";
    let mut out = ScopeSpans::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => once(M, 1, &mut out.scope, v)?,
            (2, Value::Len(v)) => child(Msg::Span, v)?,
            (3, Value::Len(v)) => {
                utf8(M, 3, v)?;
            }
            (n @ 1..=3, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// The singular fields of an `InstrumentationScope`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Scope<'a> {
    pub name: &'a str,
    pub version: &'a str,
}

/// `InstrumentationScope`: `name` (1), `version` (2), `attributes` (3, repeated),
/// `dropped_attributes_count` (4).
pub(crate) fn scope<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<Scope<'a>, E> {
    const M: &str = "InstrumentationScope";
    let mut out = Scope::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => out.name = utf8(M, 1, v)?,
            (2, Value::Len(v)) => out.version = utf8(M, 2, v)?,
            (3, Value::Len(v)) => child(Msg::KeyValue, v)?,
            (4, Value::Varint(_)) => {}
            (n @ 1..=4, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// The singular fields of a `Span`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Span<'a> {
    pub trace_id: &'a [u8],
    pub span_id: &'a [u8],
    pub trace_state: &'a str,
    pub parent_span_id: &'a [u8],
    pub name: &'a str,
    pub kind: i32,
    pub start_time_unix_nano: u64,
    pub end_time_unix_nano: u64,
    pub status: Option<&'a [u8]>,
}

/// `Span`: ids (1, 2, 4), `trace_state` (3), `name` (5), `kind` (6), times (7, 8), `attributes` (9,
/// repeated), dropped counts (10, 12, 14), `events` (11, repeated), `links` (13, repeated), `status`
/// (15) and `flags` (16).
pub(crate) fn span<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<Span<'a>, E> {
    const M: &str = "Span";
    let mut out = Span::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => out.trace_id = v,
            (2, Value::Len(v)) => out.span_id = v,
            (3, Value::Len(v)) => out.trace_state = utf8(M, 3, v)?,
            (4, Value::Len(v)) => out.parent_span_id = v,
            (5, Value::Len(v)) => out.name = utf8(M, 5, v)?,
            (6, Value::Varint(v)) => out.kind = v as i32,
            (7, Value::I64(v)) => out.start_time_unix_nano = v,
            (8, Value::I64(v)) => out.end_time_unix_nano = v,
            (9, Value::Len(v)) => child(Msg::KeyValue, v)?,
            (11, Value::Len(v)) => child(Msg::Event, v)?,
            (13, Value::Len(v)) => child(Msg::Link, v)?,
            (15, Value::Len(v)) => once(M, 15, &mut out.status, v)?,
            (10 | 12 | 14, Value::Varint(_)) | (16, Value::I32) => {}
            (n @ 1..=16, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    id(M, 1, out.trace_id, TRACE_ID_LEN)?;
    id(M, 2, out.span_id, SPAN_ID_LEN)?;
    id(M, 4, out.parent_span_id, SPAN_ID_LEN)?;
    Ok(out)
}

/// The singular fields of a `Span.Event`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Event<'a> {
    pub time_unix_nano: u64,
    pub name: &'a str,
}

/// `Span.Event`: `time_unix_nano` (1), `name` (2), `attributes` (3, repeated),
/// `dropped_attributes_count` (4).
pub(crate) fn event<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<Event<'a>, E> {
    const M: &str = "Span.Event";
    let mut out = Event::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::I64(v)) => out.time_unix_nano = v,
            (2, Value::Len(v)) => out.name = utf8(M, 2, v)?,
            (3, Value::Len(v)) => child(Msg::KeyValue, v)?,
            (4, Value::Varint(_)) => {}
            (n @ 1..=4, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// The singular fields of a `Span.Link`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Link<'a> {
    pub trace_id: &'a [u8],
    pub span_id: &'a [u8],
    pub trace_state: &'a str,
}

/// `Span.Link`: `trace_id` (1), `span_id` (2), `trace_state` (3), `attributes` (4, repeated),
/// `dropped_attributes_count` (5), `flags` (6).
pub(crate) fn link<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<Link<'a>, E> {
    const M: &str = "Span.Link";
    let mut out = Link::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => out.trace_id = v,
            (2, Value::Len(v)) => out.span_id = v,
            (3, Value::Len(v)) => out.trace_state = utf8(M, 3, v)?,
            (4, Value::Len(v)) => child(Msg::KeyValue, v)?,
            (5, Value::Varint(_)) | (6, Value::I32) => {}
            (n @ 1..=6, _) => return Err(wire_type(M, n).into()),
            _ => unknown(level)?,
        }
    }
    id(M, 1, out.trace_id, TRACE_ID_LEN)?;
    id(M, 2, out.span_id, SPAN_ID_LEN)?;
    Ok(out)
}

/// The fields of a `Status`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Status<'a> {
    pub message: &'a str,
    pub code: i32,
}

/// `Status`: `message` (2), `code` (3). Field 1 is reserved and skipped like any unknown field.
pub(crate) fn status(bytes: &[u8], level: u32) -> Result<Status<'_>, WireError> {
    const M: &str = "Status";
    let mut out = Status::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (2, Value::Len(v)) => out.message = utf8(M, 2, v)?,
            (3, Value::Varint(v)) => out.code = v as i32,
            (n @ 2..=3, _) => return Err(wire_type(M, n)),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// The fields of a `KeyValue`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct KeyValue<'a> {
    pub key: &'a str,
    pub value: Option<&'a [u8]>,
}

/// `KeyValue`: `key` (1), `value` (2), `key_strindex` (3, profiles only, ignored).
pub(crate) fn key_value(bytes: &[u8], level: u32) -> Result<KeyValue<'_>, WireError> {
    const M: &str = "KeyValue";
    let mut out = KeyValue::default();
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => out.key = utf8(M, 1, v)?,
            (2, Value::Len(v)) => once(M, 2, &mut out.value, v)?,
            (3, Value::Varint(_)) => {}
            (n @ 1..=3, _) => return Err(wire_type(M, n)),
            _ => unknown(level)?,
        }
    }
    Ok(out)
}

/// The value an `AnyValue` holds: its last oneof field.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) enum AnyValue<'a> {
    #[default]
    Empty,
    String(&'a str),
    Bool(bool),
    Int(i64),
    Double(f64),
    Array(&'a [u8]),
    KvList(&'a [u8]),
    Bytes(&'a [u8]),
}

/// `AnyValue`: the oneof `string_value` (1), `bool_value` (2), `int_value` (3), `double_value` (4),
/// `array_value` (5), `kvlist_value` (6), `bytes_value` (7) and `string_value_strindex` (8). The last
/// oneof field wins. `string_value_strindex` belongs to the profiles signal and reads as empty. An
/// `array_value` or `kvlist_value` directly following one of its own kind would be merged by protobuf,
/// so it is refused as a duplicate.
pub(crate) fn any_value<'a, E: From<WireError>>(
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<AnyValue<'a>, E> {
    const M: &str = "AnyValue";
    let mut out = AnyValue::Empty;
    let mut set_by = 0;
    for field in Fields::new(bytes, level) {
        let (number, value) = field?;
        out = match (number, value) {
            (1, Value::Len(v)) => AnyValue::String(utf8(M, 1, v)?),
            (2, Value::Varint(v)) => AnyValue::Bool(v != 0),
            (3, Value::Varint(v)) => AnyValue::Int(v as i64),
            (4, Value::I64(v)) => AnyValue::Double(f64::from_bits(v)),
            (n @ (5 | 6), Value::Len(v)) => {
                if set_by == n {
                    return Err(WireError::Duplicate {
                        message: M,
                        field: n,
                    }
                    .into());
                }
                if n == 5 {
                    child(Msg::ArrayValue, v)?;
                    AnyValue::Array(v)
                } else {
                    child(Msg::KeyValueList, v)?;
                    AnyValue::KvList(v)
                }
            }
            (7, Value::Len(v)) => AnyValue::Bytes(v),
            (8, Value::Varint(_)) => AnyValue::Empty,
            (n @ 1..=8, _) => return Err(wire_type(M, n).into()),
            _ => {
                unknown(level)?;
                continue;
            }
        };
        set_by = number;
    }
    Ok(out)
}

/// `ArrayValue` (elements `AnyValue`) or `KeyValueList` (elements `KeyValue`): `values` (1, repeated).
pub(crate) fn list<'a, E: From<WireError>>(
    element: Msg,
    bytes: &'a [u8],
    level: u32,
    mut child: impl FnMut(Msg, &'a [u8]) -> Result<(), E>,
) -> Result<(), E> {
    let message = if element == Msg::AnyValue {
        "ArrayValue"
    } else {
        "KeyValueList"
    };
    for field in Fields::new(bytes, level) {
        match field? {
            (1, Value::Len(v)) => child(element, v)?,
            (1, _) => return Err(wire_type(message, 1).into()),
            _ => unknown(level)?,
        }
    }
    Ok(())
}
