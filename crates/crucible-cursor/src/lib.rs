//! Signed pagination cursors (contract 12.4): the text `v1.<kid>.<payload>.<mac>`, its payload codec and the
//! key ring that signs and verifies it (contract 11.2).
//!
//! - **Payload.** [`Cursor`] is what a cursor holds: the project, filter hash, sort and [`Position`] (kind,
//!   route, window, `as_of` and the sort tuple of the last row, or for a head cursor the `wal_epoch` and LSN;
//!   the lists keyed by a text id carry the id inline as a [`CursorId`] of at most [`MAX_ID_LEN`] bytes),
//!   with its issue time and an expiry [`CURSOR_TTL_MICROS`] later. [`Cursor::encode`] and [`Cursor::decode`]
//!   are the versioned binary codec; Kani proves that decoding never panics and that the codec round-trips.
//! - **Text and MAC.** [`KeyRing::seal`] signs with the active key: HMAC-SHA256 truncated to [`TAG_LEN`]
//!   bytes, base64url without padding. [`KeyRing::open`] verifies the MAC in constant time with the key the
//!   cursor names, then the expiry, then the [`Binding`] of the request.
//! - **Keys.** A [`KeyRing`] holds one active key and the verify-only keys a rotation keeps. The keys come from
//!   the caller.
//! - **Errors.** Every [`CursorError`] is the problem code `cursor_invalid`.
//!
//! This crate is pure: no `Env`, no IO. Time is an argument.

mod payload;
#[cfg(kani)]
mod proofs;
mod ring;

pub use payload::{
    Cursor, CursorId, DecodeError, FilterHash, IdList, Kind, MAX_ID_LEN, MAX_PAYLOAD_LEN,
    PAYLOAD_VERSION, Payload, Position, Route, Sort,
};
pub use ring::{
    Binding, CursorError, InvalidKid, KeyRing, KeyRingError, KeyState, Kid, MAX_KID_LEN,
    MAX_TEXT_LEN, SigningKey, TAG_LEN,
};

/// How long a cursor opens after it is issued: 24 hours (contract 12.4). A key stays verify-only this long
/// after a rotation (contract 11.2).
pub const CURSOR_TTL_MICROS: i64 = 24 * 60 * 60 * 1_000_000;
