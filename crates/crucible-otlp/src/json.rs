//! OTLP/JSON to canonical protobuf (contract 4.4: JSON bodies are transcoded before the WAL append).
//!
//! The transcoder streams: a serde visitor walks the JSON once and writes each field straight into the
//! protobuf output, so no JSON tree is built. Keys are the lowerCamelCase field names; unknown keys are
//! ignored, as OTLP/JSON requires, and `null` means the field is absent. Trace and span ids are hex
//! (either case), 64-bit integers are decimal strings or numbers, enums are integers, bytes are base64
//! (standard or URL-safe alphabet, padding optional) and doubles are numbers or the strings `NaN`,
//! `Infinity` and `-Infinity`. Fields are written in JSON order with minimal length prefixes.

use core::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT};
use serde::de::{
    self, DeserializeSeed, Deserializer, IgnoredAny, MapAccess, SeqAccess, Unexpected, Visitor,
};

use crate::MAX_BODY_BYTES;
use crate::wire::{I32, I64, LEN, VARINT, begin_len, end_len, put_key, put_varint};
use Kind as K;
use Ty::Message as M;

/// Why an OTLP/JSON body cannot be transcoded.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    /// The canonical protobuf would exceed [`MAX_BODY_BYTES`] (HTTP 413).
    #[error("transcoded body of {len} bytes is above the cap of {max} bytes", max = MAX_BODY_BYTES)]
    BodyTooLarge {
        /// The transcoded length.
        len: usize,
    },
    /// The body is not valid OTLP/JSON (HTTP 400).
    #[error("invalid OTLP/JSON: {0}")]
    Invalid(#[from] serde_json::Error),
}

/// Transcodes one OTLP/JSON `ExportTraceServiceRequest` into canonical protobuf. The output still
/// needs [`TraceRequest::scan`](crate::TraceRequest::scan), which checks what the JSON grammar does
/// not (id lengths, the span cap, duplicate message fields).
pub fn transcode_json(json: &[u8]) -> Result<Vec<u8>, JsonError> {
    let mut out = Vec::with_capacity(json.len() / 2);
    let mut deserializer = serde_json::Deserializer::from_slice(json);
    Message {
        out: &mut out,
        fields: REQUEST,
    }
    .deserialize(&mut deserializer)?;
    deserializer.end()?;
    if out.len() > MAX_BODY_BYTES {
        return Err(JsonError::BodyTooLarge { len: out.len() });
    }
    Ok(out)
}

/// The message types the schema tables refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
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

/// How a JSON value maps to a protobuf field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Message(Kind),
    String,
    HexId,
    Base64,
    Bool,
    Double,
    Int64,
    Int32,
    Uint32,
    Fixed64,
    Fixed32,
}

#[derive(Debug)]
struct Field {
    name: &'static str,
    number: u32,
    ty: Ty,
    repeated: bool,
}

const fn one(name: &'static str, number: u32, ty: Ty) -> Field {
    Field {
        name,
        number,
        ty,
        repeated: false,
    }
}

const fn many(name: &'static str, number: u32, ty: Ty) -> Field {
    Field {
        name,
        number,
        ty,
        repeated: true,
    }
}

const REQUEST: &[Field] = &[many("resourceSpans", 1, M(K::ResourceSpans))];
const RESOURCE_SPANS: &[Field] = &[
    one("resource", 1, M(K::Resource)),
    many("scopeSpans", 2, M(K::ScopeSpans)),
    one("schemaUrl", 3, Ty::String),
];
const RESOURCE: &[Field] = &[
    many("attributes", 1, M(K::KeyValue)),
    one("droppedAttributesCount", 2, Ty::Uint32),
    many("entityRefs", 3, M(K::EntityRef)),
];
const ENTITY_REF: &[Field] = &[
    one("schemaUrl", 1, Ty::String),
    one("type", 2, Ty::String),
    many("idKeys", 3, Ty::String),
    many("descriptionKeys", 4, Ty::String),
];
const SCOPE_SPANS: &[Field] = &[
    one("scope", 1, M(K::Scope)),
    many("spans", 2, M(K::Span)),
    one("schemaUrl", 3, Ty::String),
];
const SCOPE: &[Field] = &[
    one("name", 1, Ty::String),
    one("version", 2, Ty::String),
    many("attributes", 3, M(K::KeyValue)),
    one("droppedAttributesCount", 4, Ty::Uint32),
];
const SPAN: &[Field] = &[
    one("traceId", 1, Ty::HexId),
    one("spanId", 2, Ty::HexId),
    one("traceState", 3, Ty::String),
    one("parentSpanId", 4, Ty::HexId),
    one("flags", 16, Ty::Fixed32),
    one("name", 5, Ty::String),
    one("kind", 6, Ty::Int32),
    one("startTimeUnixNano", 7, Ty::Fixed64),
    one("endTimeUnixNano", 8, Ty::Fixed64),
    many("attributes", 9, M(K::KeyValue)),
    one("droppedAttributesCount", 10, Ty::Uint32),
    many("events", 11, M(K::Event)),
    one("droppedEventsCount", 12, Ty::Uint32),
    many("links", 13, M(K::Link)),
    one("droppedLinksCount", 14, Ty::Uint32),
    one("status", 15, M(K::Status)),
];
const EVENT: &[Field] = &[
    one("timeUnixNano", 1, Ty::Fixed64),
    one("name", 2, Ty::String),
    many("attributes", 3, M(K::KeyValue)),
    one("droppedAttributesCount", 4, Ty::Uint32),
];
const LINK: &[Field] = &[
    one("traceId", 1, Ty::HexId),
    one("spanId", 2, Ty::HexId),
    one("traceState", 3, Ty::String),
    many("attributes", 4, M(K::KeyValue)),
    one("droppedAttributesCount", 5, Ty::Uint32),
    one("flags", 6, Ty::Fixed32),
];
const STATUS: &[Field] = &[one("message", 2, Ty::String), one("code", 3, Ty::Int32)];
const KEY_VALUE: &[Field] = &[
    one("key", 1, Ty::String),
    one("value", 2, M(K::AnyValue)),
    one("keyStrindex", 3, Ty::Int32),
];
const ANY_VALUE: &[Field] = &[
    one("stringValue", 1, Ty::String),
    one("boolValue", 2, Ty::Bool),
    one("intValue", 3, Ty::Int64),
    one("doubleValue", 4, Ty::Double),
    one("arrayValue", 5, M(K::ArrayValue)),
    one("kvlistValue", 6, M(K::KeyValueList)),
    one("bytesValue", 7, Ty::Base64),
    one("stringValueStrindex", 8, Ty::Int32),
];
const ARRAY_VALUE: &[Field] = &[many("values", 1, M(K::AnyValue))];
const KEY_VALUE_LIST: &[Field] = &[many("values", 1, M(K::KeyValue))];

const fn fields(kind: Kind) -> &'static [Field] {
    match kind {
        Kind::ResourceSpans => RESOURCE_SPANS,
        Kind::Resource => RESOURCE,
        Kind::EntityRef => ENTITY_REF,
        Kind::ScopeSpans => SCOPE_SPANS,
        Kind::Scope => SCOPE,
        Kind::Span => SPAN,
        Kind::Event => EVENT,
        Kind::Link => LINK,
        Kind::Status => STATUS,
        Kind::KeyValue => KEY_VALUE,
        Kind::AnyValue => ANY_VALUE,
        Kind::ArrayValue => ARRAY_VALUE,
        Kind::KeyValueList => KEY_VALUE_LIST,
    }
}

/// One JSON object, written as the fields of a protobuf message.
struct Message<'o> {
    out: &'o mut Vec<u8>,
    fields: &'static [Field],
}

impl<'de> DeserializeSeed<'de> for Message<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Message<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an OTLP/JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while let Some(field) = map.next_key_seed(Key(self.fields))? {
            match field {
                Some(field) => map.next_value_seed(Occurrences {
                    out: &mut *self.out,
                    field,
                })?,
                None => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

/// A JSON key, looked up in the schema table of the enclosing message.
struct Key(&'static [Field]);

impl<'de> DeserializeSeed<'de> for Key {
    type Value = Option<&'static Field>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for Key {
    type Value = Option<&'static Field>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a field name")
    }

    fn visit_str<E: de::Error>(self, name: &str) -> Result<Self::Value, E> {
        Ok(self.0.iter().find(|field| field.name == name))
    }
}

/// The JSON value of a known field: an array of occurrences for a repeated field, else one occurrence.
struct Occurrences<'o> {
    out: &'o mut Vec<u8>,
    field: &'static Field,
}

impl<'de> DeserializeSeed<'de> for Occurrences<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        if self.field.repeated {
            deserializer.deserialize_any(self)
        } else {
            Occurrence {
                out: self.out,
                field: self.field,
            }
            .deserialize(deserializer)
        }
    }
}

impl<'de> Visitor<'de> for Occurrences<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an array for `{}`", self.field.name)
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq
            .next_element_seed(Occurrence {
                out: &mut *self.out,
                field: self.field,
            })?
            .is_some()
        {}
        Ok(())
    }
}

/// One occurrence of a field.
struct Occurrence<'o> {
    out: &'o mut Vec<u8>,
    field: &'static Field,
}

impl Occurrence<'_> {
    fn out_of_range<E: de::Error>(&self) -> E {
        E::custom(format_args!("`{}` is out of range", self.field.name))
    }

    fn integer<E: de::Error>(self, value: i128) -> Result<(), E> {
        let number = self.field.number;
        match self.field.ty {
            Ty::Int64 => {
                let v = i64::try_from(value).map_err(|_| self.out_of_range())?;
                put_key(self.out, number, VARINT);
                put_varint(self.out, v as u64);
            }
            Ty::Int32 => {
                let v = i32::try_from(value).map_err(|_| self.out_of_range())?;
                put_key(self.out, number, VARINT);
                put_varint(self.out, i64::from(v) as u64);
            }
            Ty::Uint32 => {
                let v = u32::try_from(value).map_err(|_| self.out_of_range())?;
                put_key(self.out, number, VARINT);
                put_varint(self.out, u64::from(v));
            }
            Ty::Fixed64 => {
                let v = u64::try_from(value).map_err(|_| self.out_of_range())?;
                put_key(self.out, number, I64);
                self.out.extend_from_slice(&v.to_le_bytes());
            }
            Ty::Fixed32 => {
                let v = u32::try_from(value).map_err(|_| self.out_of_range())?;
                put_key(self.out, number, I32);
                self.out.extend_from_slice(&v.to_le_bytes());
            }
            Ty::Double => self.double(value as f64),
            Ty::Message(_) | Ty::String | Ty::HexId | Ty::Base64 | Ty::Bool => {
                return Err(E::invalid_type(Unexpected::Other("a number"), &self));
            }
        }
        Ok(())
    }

    fn double(self, value: f64) {
        put_key(self.out, self.field.number, I64);
        self.out.extend_from_slice(&value.to_bits().to_le_bytes());
    }

    fn hex<E: de::Error>(self, text: &str) -> Result<(), E> {
        let (pairs, rest) = text.as_bytes().as_chunks::<2>();
        if !rest.is_empty() {
            return Err(E::invalid_value(Unexpected::Str(text), &self));
        }
        put_key(self.out, self.field.number, LEN);
        put_varint(self.out, pairs.len() as u64);
        for [hi, lo] in pairs {
            match (hex_digit(*hi), hex_digit(*lo)) {
                (Some(hi), Some(lo)) => self.out.push((hi << 4) | lo),
                _ => return Err(E::invalid_value(Unexpected::Str(text), &self)),
            }
        }
        Ok(())
    }

    fn base64<E: de::Error>(self, text: &str) -> Result<(), E> {
        let start = begin_len(self.out, self.field.number);
        if STANDARD_PAD_INDIFFERENT.decode_vec(text, self.out).is_err() {
            self.out.truncate(start);
            if URL_SAFE_PAD_INDIFFERENT.decode_vec(text, self.out).is_err() {
                return Err(E::invalid_value(Unexpected::Str(text), &self));
            }
        }
        end_len(self.out, start);
        Ok(())
    }
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

impl<'de> DeserializeSeed<'de> for Occurrence<'_> {
    type Value = ();

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Occurrence<'_> {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a valid value for `{}`", self.field.name)
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<(), E> {
        if self.field.ty != Ty::Bool {
            return Err(E::invalid_type(Unexpected::Bool(value), &self));
        }
        put_key(self.out, self.field.number, VARINT);
        self.out.push(u8::from(value));
        Ok(())
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<(), E> {
        self.integer(i128::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<(), E> {
        self.integer(i128::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        if self.field.ty != Ty::Double {
            return Err(E::invalid_type(Unexpected::Float(value), &self));
        }
        self.double(value);
        Ok(())
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<(), E> {
        match self.field.ty {
            Ty::String => {
                put_key(self.out, self.field.number, LEN);
                put_varint(self.out, text.len() as u64);
                self.out.extend_from_slice(text.as_bytes());
                Ok(())
            }
            Ty::HexId => self.hex(text),
            Ty::Base64 => self.base64(text),
            Ty::Double => match text.parse::<f64>() {
                Ok(value) => {
                    self.double(value);
                    Ok(())
                }
                Err(_) => Err(E::invalid_value(Unexpected::Str(text), &self)),
            },
            Ty::Int64 | Ty::Int32 | Ty::Uint32 | Ty::Fixed64 | Ty::Fixed32 => {
                match text.parse::<i128>() {
                    Ok(value) => self.integer(value),
                    Err(_) => Err(E::invalid_value(Unexpected::Str(text), &self)),
                }
            }
            Ty::Message(_) | Ty::Bool => Err(E::invalid_type(Unexpected::Str(text), &self)),
        }
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<(), A::Error> {
        let Ty::Message(kind) = self.field.ty else {
            return Err(de::Error::invalid_type(Unexpected::Map, &self));
        };
        let start = begin_len(self.out, self.field.number);
        Message {
            out: &mut *self.out,
            fields: fields(kind),
        }
        .visit_map(map)?;
        end_len(self.out, start);
        Ok(())
    }
}
