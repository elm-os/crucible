//! Identifiers (contract section 1.1).
//!
//! OTel ids are raw bytes internally and lowercase hex on the wire. Crucible ids are UUIDv7 values in RFC 9562
//! byte order, written as canonical hyphenated lowercase strings. Minting a new id needs `Clock` and `Rng`, so
//! it lives in `crucible_env::new_id`; this module only represents, parses and formats ids.

use core::fmt;
use core::str::FromStr;

use crate::hex;

/// Any malformed identifier on input. Maps to `validation_failed` on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid {kind} identifier")]
pub struct IdParseError {
    kind: &'static str,
}

impl IdParseError {
    /// The kind of identifier that failed to parse, for example `trace` or `project`.
    pub const fn kind(&self) -> &'static str {
        self.kind
    }
}

/// OTel trace id: 16 bytes; 32 lowercase hex characters on the wire. All-zero is representable here and
/// rejected at ingest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// The all-zero id, which ingest rejects.
    pub const ZERO: Self = Self([0; 16]);

    /// Wraps raw bytes.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Borrows the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Whether every byte is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Parses 32 hex characters; uppercase is accepted and lowered.
    pub fn parse(s: &str) -> Result<Self, IdParseError> {
        let mut out = [0; 16];
        if hex::decode_into(s.as_bytes(), &mut out) {
            Ok(Self(out))
        } else {
            Err(IdParseError { kind: "trace" })
        }
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write_lower(f, &self.0)
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({self})")
    }
}

impl FromStr for TraceId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// OTel span id: 8 bytes; 16 lowercase hex characters on the wire. An all-zero or absent parent means
/// "no parent"; see [`SpanId::non_zero`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SpanId([u8; 8]);

impl SpanId {
    /// The all-zero id. Rejected as a `span_id`; as a parent it means "no parent".
    pub const ZERO: Self = Self([0; 8]);

    /// Wraps raw bytes.
    pub const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    /// The raw bytes.
    pub const fn to_bytes(self) -> [u8; 8] {
        self.0
    }

    /// Borrows the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 8] {
        &self.0
    }

    /// Whether every byte is zero.
    pub fn is_zero(&self) -> bool {
        self.0 == [0; 8]
    }

    /// `None` for the all-zero id, which is how a missing parent is represented.
    pub fn non_zero(self) -> Option<Self> {
        if self.is_zero() { None } else { Some(self) }
    }

    /// Parses 16 hex characters; uppercase is accepted and lowered.
    pub fn parse(s: &str) -> Result<Self, IdParseError> {
        let mut out = [0; 8];
        if hex::decode_into(s.as_bytes(), &mut out) {
            Ok(Self(out))
        } else {
            Err(IdParseError { kind: "span" })
        }
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        hex::write_lower(f, &self.0)
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({self})")
    }
}

impl FromStr for SpanId {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// A UUID in RFC 9562 byte order. Crucible ids are version 7.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Uuid([u8; 16]);

/// Mask of the 74 random bits of a UUIDv7 (12 bits `rand_a`, 62 bits `rand_b`).
const V7_RAND_MASK: u128 = (1 << 74) - 1;

impl Uuid {
    /// The nil UUID, reserved for marker rows. Never a project id.
    pub const NIL: Self = Self([0; 16]);

    /// Wraps raw bytes in RFC 9562 order.
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The raw bytes in RFC 9562 order.
    pub const fn to_bytes(self) -> [u8; 16] {
        self.0
    }

    /// Borrows the raw bytes.
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Whether this is the nil UUID.
    pub fn is_nil(&self) -> bool {
        self.0 == [0; 16]
    }

    /// Builds a UUIDv7 from 48 bits of Unix milliseconds and 74 random bits (the low bits of `rand`).
    /// Higher bits of either argument are ignored.
    pub const fn from_v7_parts(unix_ms: u64, rand: u128) -> Self {
        let rand = rand & V7_RAND_MASK;
        let rand_a = rand >> 62; // 12 bits
        let rand_b = rand & ((1 << 62) - 1); // 62 bits
        let ms = (unix_ms as u128) & ((1 << 48) - 1);
        let value = (ms << 80) | (0x7_u128 << 76) | (rand_a << 64) | (0b10_u128 << 62) | rand_b;
        Self(value.to_be_bytes())
    }

    /// The Unix milliseconds and the 74 random bits of a UUIDv7, or `None` when the version is not 7 or the
    /// variant is not RFC 9562.
    pub const fn v7_parts(&self) -> Option<(u64, u128)> {
        let value = u128::from_be_bytes(self.0);
        let version = (value >> 76) & 0xf;
        let variant = (value >> 62) & 0b11;
        if version != 7 || variant != 0b10 {
            return None;
        }
        let ms = (value >> 80) as u64;
        let rand_a = (value >> 64) & 0xfff;
        let rand_b = value & ((1 << 62) - 1);
        Some((ms, (rand_a << 62) | rand_b))
    }

    /// Parses the canonical hyphenated form (8-4-4-4-12). Uppercase is accepted and lowered; any other form
    /// is rejected.
    pub fn parse(s: &str) -> Result<Self, IdParseError> {
        Self::parse_kind(s, "uuid")
    }

    fn parse_kind(s: &str, kind: &'static str) -> Result<Self, IdParseError> {
        let err = IdParseError { kind };
        let b = s.as_bytes();
        if b.len() != 36 || b[8] != b'-' || b[13] != b'-' || b[18] != b'-' || b[23] != b'-' {
            return Err(err);
        }
        let mut out = [0; 16];
        let groups: [(usize, usize, usize); 5] = [
            (0, 8, 0),
            (9, 13, 4),
            (14, 18, 6),
            (19, 23, 8),
            (24, 36, 10),
        ];
        for (start, end, at) in groups {
            let len = (end - start) / 2;
            if !hex::decode_into(&b[start..end], &mut out[at..at + len]) {
                return Err(err);
            }
        }
        Ok(Self(out))
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        hex::write_lower(f, &b[0..4])?;
        f.write_str("-")?;
        hex::write_lower(f, &b[4..6])?;
        f.write_str("-")?;
        hex::write_lower(f, &b[6..8])?;
        f.write_str("-")?;
        hex::write_lower(f, &b[8..10])?;
        f.write_str("-")?;
        hex::write_lower(f, &b[10..16])
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({self})")
    }
}

impl FromStr for Uuid {
    type Err = IdParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

macro_rules! uuid_ids {
    ($($(#[$meta:meta])* $name:ident => $kind:literal;)+) => {$(
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(Uuid);

        impl $name {
            /// Wraps a UUID.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// The underlying UUID.
            pub const fn uuid(self) -> Uuid {
                self.0
            }

            /// Wraps raw bytes in RFC 9562 order.
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(Uuid::from_bytes(bytes))
            }

            /// The raw bytes in RFC 9562 order.
            pub const fn to_bytes(self) -> [u8; 16] {
                self.0.to_bytes()
            }

            /// Whether this is the nil UUID.
            pub fn is_nil(&self) -> bool {
                self.0.is_nil()
            }

            /// Parses the canonical hyphenated form; uppercase is accepted and lowered.
            pub fn parse(s: &str) -> Result<Self, IdParseError> {
                Uuid::parse_kind(s, $kind).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::parse(s)
            }
        }
    )+};
}

uuid_ids! {
    /// Organization id. One organization per deployment.
    OrgId => "org";
    /// Project id. The nil value marks marker rows and is refused by `ProjectScope`.
    ProjectId => "project";
    /// User id.
    UserId => "user";
    /// API key id.
    ApiKeyId => "api_key";
    /// Login session row id (not the client-provided telemetry `session_id`, which is a string).
    AuthSessionId => "auth_session";
    /// Dashboard id.
    DashboardId => "dashboard";
    /// Dashboard widget id.
    WidgetId => "widget";
    /// Alert rule id.
    AlertRuleId => "alert_rule";
    /// Notification channel id.
    ChannelId => "channel";
    /// Deletion request id.
    DeletionRequestId => "deletion_request";
    /// Export job id.
    ExportJobId => "export_job";
    /// Fact id.
    FactId => "fact";
    /// Comment id.
    CommentId => "comment";
    /// Media object id.
    MediaObjectId => "media_object";
    /// Saved SQL query id.
    SavedQueryId => "saved_query";
    /// SQL run id.
    SqlRunId => "sql_run";
    /// Request id, returned in problem documents.
    RequestId => "request";
    /// WAL epoch: one per WAL directory, part of every batch token.
    WalEpoch => "wal_epoch";
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn trace_and_span_ids_round_trip_and_lower_case() {
        let t = TraceId::parse("0AF7651916CD43DD8448EB211C80319C").unwrap();
        assert_eq!(t.to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert!(TraceId::parse("0af7651916cd43dd8448eb211c80319").is_err());
        assert!(TraceId::parse("0af7651916cd43dd8448eb211c80319g").is_err());
        assert!(TraceId::ZERO.is_zero());
        let s = SpanId::parse("b7ad6b7169203331").unwrap();
        assert_eq!(s.to_string(), "b7ad6b7169203331");
        assert_eq!(SpanId::ZERO.non_zero(), None);
        assert_eq!(s.non_zero(), Some(s));
    }

    #[test]
    fn uuid_parse_is_canonical_only() {
        let u = Uuid::parse("0193A1B2-7C3D-7E4F-8A5B-6C7D8E9F0A1B").unwrap();
        assert_eq!(u.to_string(), "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b");
        for bad in [
            "0193a1b27c3d7e4f8a5b6c7d8e9f0a1b",
            "{0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b}",
            "urn:uuid:0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1b",
            "0193a1b2-7c3d-7e4f-8a5b-6c7d8e9f0a1",
            "0193a1b2_7c3d-7e4f-8a5b-6c7d8e9f0a1b",
        ] {
            assert!(Uuid::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(ProjectId::parse("x").unwrap_err().kind(), "project");
    }

    #[test]
    fn v7_layout_matches_rfc_9562() {
        let u = Uuid::from_v7_parts(0x0193_a1b2_7c3d, 0);
        let s = u.to_string();
        assert!(s.starts_with("0193a1b2-7c3d-7"), "{s}");
        assert_eq!(&s[19..20], "8");
        assert_eq!(u.v7_parts(), Some((0x0193_a1b2_7c3d, 0)));
        assert_eq!(Uuid::NIL.v7_parts(), None);
    }

    proptest! {
        #[test]
        fn trace_id_round_trips(bytes in any::<[u8; 16]>()) {
            let id = TraceId::from_bytes(bytes);
            prop_assert_eq!(TraceId::parse(&id.to_string()), Ok(id));
            prop_assert_eq!(TraceId::parse(&id.to_string().to_uppercase()), Ok(id));
        }

        #[test]
        fn span_id_round_trips(bytes in any::<[u8; 8]>()) {
            let id = SpanId::from_bytes(bytes);
            prop_assert_eq!(SpanId::parse(&id.to_string()), Ok(id));
        }

        #[test]
        fn uuid_round_trips(bytes in any::<[u8; 16]>()) {
            let u = Uuid::from_bytes(bytes);
            prop_assert_eq!(Uuid::parse(&u.to_string()), Ok(u));
            prop_assert_eq!(WalEpoch::parse(&u.to_string()), Ok(WalEpoch::from_uuid(u)));
        }

        #[test]
        fn v7_parts_round_trip(ms in 0u64..(1 << 48), rand in 0u128..(1 << 74)) {
            let u = Uuid::from_v7_parts(ms, rand);
            prop_assert_eq!(u.v7_parts(), Some((ms, rand)));
        }

        #[test]
        fn v7_order_follows_time_then_rand(
            a in (0u64..(1 << 48), 0u128..(1 << 74)),
            b in (0u64..(1 << 48), 0u128..(1 << 74)),
        ) {
            let ua = Uuid::from_v7_parts(a.0, a.1);
            let ub = Uuid::from_v7_parts(b.0, b.1);
            prop_assert_eq!(ua.cmp(&ub), a.cmp(&b));
        }
    }
}
