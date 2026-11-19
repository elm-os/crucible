//! The text form `v1.<kid>.<payload>.<mac>` and the key ring that signs and verifies it (contracts 11.2 and
//! 12.4).
//!
//! `payload` is the base64url (no padding) of [`Cursor::encode`]. `mac` is the base64url (no padding) of the
//! first [`TAG_LEN`] bytes of HMAC-SHA256 under the key named by `kid`, over the ASCII text
//! `v1.<kid>.<payload>`. Decoding is strict base64url: padding, other alphabets and non-zero trailing bits are
//! malformed, so every cursor has exactly one text form and any change to the text fails to open.

use std::collections::BTreeSet;
use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use crucible_records::{ProjectId, TimeWindow, Timestamp};
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

use crate::payload::{Cursor, FilterHash, Kind, Route, Sort};

/// The version prefix of the text form.
const TEXT_VERSION: &str = "v1";

/// The bytes of HMAC-SHA256 a cursor keeps (contract 12.4).
pub const TAG_LEN: usize = 16;

/// The longest text [`KeyRing::open`] reads; the longest cursor, a page of a list keyed by a text id of
/// [`crate::MAX_ID_LEN`] bytes, is under 850 characters.
pub const MAX_TEXT_LEN: usize = 1024;

/// The longest key id.
pub const MAX_KID_LEN: usize = 32;

/// A key id: it names a `signing_keys` row of purpose `cursor` (contract 11.1). 1 to [`MAX_KID_LEN`]
/// characters of `A-Z`, `a-z`, `0-9`, `-` and `_`, so it never contains the separator.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Kid(String);

/// Text that is not a key id.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("a key id is 1 to 32 characters of A-Z, a-z, 0-9, '-' and '_'")]
pub struct InvalidKid;

impl Kid {
    /// A key id, when `text` is one.
    pub fn new(text: &str) -> Result<Self, InvalidKid> {
        let allowed = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
        if (1..=MAX_KID_LEN).contains(&text.len()) && text.bytes().all(allowed) {
            Ok(Self(text.to_owned()))
        } else {
            Err(InvalidKid)
        }
    }

    /// The id.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Kid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A cursor-signing key: an id and a 32-byte secret. Only the HMAC state derived from the secret is kept, and
/// `Debug` shows the id alone.
#[derive(Clone)]
pub struct SigningKey {
    kid: Kid,
    mac: Hmac<Sha256>,
}

impl SigningKey {
    /// The key `kid` with this secret.
    pub fn new(kid: Kid, secret: [u8; 32]) -> Self {
        // HMAC pads a key shorter than the hash's 64-byte block with zeros, so keying it with the padded block
        // is keying it with `secret`; this form of the constructor cannot fail.
        let mut block = [0; 64];
        block[..32].copy_from_slice(&secret);
        Self {
            kid,
            mac: Hmac::new(&block.into()),
        }
    }

    /// The key's id.
    pub const fn kid(&self) -> &Kid {
        &self.kid
    }

    /// The first [`TAG_LEN`] bytes of HMAC-SHA256 over `signed`.
    fn tag(&self, signed: &[u8]) -> [u8; TAG_LEN] {
        let mut mac = self.mac.clone();
        mac.update(signed);
        let full = mac.finalize().into_bytes();
        let mut tag = [0; TAG_LEN];
        tag.copy_from_slice(&full[..TAG_LEN]);
        tag
    }

    /// Whether `tag` is the MAC of `signed`, compared in constant time.
    fn verifies(&self, signed: &[u8], tag: &[u8; TAG_LEN]) -> bool {
        let mut mac = self.mac.clone();
        mac.update(signed);
        mac.verify_truncated_left(tag).is_ok()
    }
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SigningKey")
            .field("kid", &self.kid)
            .finish_non_exhaustive()
    }
}

/// The state of a key in the ring (contract 11.1; retired keys are not in a ring).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KeyState {
    /// Signs new cursors and verifies.
    Active,
    /// Verifies only: a key kept after a rotation until every cursor it signed has expired.
    VerifyOnly,
}

/// Why keys do not form a ring.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum KeyRingError {
    /// Two keys share an id.
    #[error("the key id {0} appears more than once")]
    DuplicateKid(Kid),
    /// No key is active.
    #[error("no key is active")]
    NoActiveKey,
    /// More than one key is active.
    #[error("more than one key is active")]
    SeveralActiveKeys,
}

/// What a request accepts (contract 12.4): a cursor opens only for the project, list, kind, filters, sort and
/// window it was issued for.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Binding {
    /// The request's project, from its credential.
    pub project_id: ProjectId,
    /// The list the request reads.
    pub route: Route,
    /// `page` for a `cursor` parameter, `head` for live tail.
    pub kind: Kind,
    /// The request's filters.
    pub filter: FilterHash,
    /// The request's sort.
    pub sort: Sort,
    /// The request's window; `None` for a list without one.
    pub window: Option<TimeWindow>,
}

/// Why a cursor did not open. Every reason is the problem code `cursor_invalid` (contract 12.3), which
/// clients treat as "restart from the first page".
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, thiserror::Error)]
pub enum CursorError {
    /// Not the text form of a v1 cursor, or a payload that does not decode.
    #[error("the cursor is malformed")]
    Malformed,
    /// The key it names is not in the ring: never issued here, or retired.
    #[error("the cursor names an unknown signing key")]
    UnknownKid,
    /// The MAC does not verify: the text was changed.
    #[error("the cursor's MAC does not verify")]
    Tampered,
    /// Its expiry has passed.
    #[error("the cursor has expired")]
    Expired,
    /// Issued for another project.
    #[error("the cursor belongs to another project")]
    WrongProject,
    /// Issued for another list.
    #[error("the cursor belongs to another list")]
    WrongRoute,
    /// A page cursor where a head cursor is expected, or the reverse.
    #[error("the cursor is of the other kind")]
    WrongKind,
    /// The request's filters are not the ones it was issued for.
    #[error("the filters changed")]
    FiltersChanged,
    /// The request's sort is not the one it was issued for.
    #[error("the sort changed")]
    SortChanged,
    /// The request's window is not the one it was issued for.
    #[error("the window changed")]
    WindowChanged,
}

impl CursorError {
    /// The problem code: `cursor_invalid` for every reason.
    pub const fn problem_code(self) -> &'static str {
        "cursor_invalid"
    }
}

impl Cursor {
    /// The first difference between the cursor's binding and the request's.
    fn check(&self, expected: &Binding) -> Result<(), CursorError> {
        let actual = self.binding();
        if actual.project_id != expected.project_id {
            Err(CursorError::WrongProject)
        } else if actual.route != expected.route {
            Err(CursorError::WrongRoute)
        } else if actual.kind != expected.kind {
            Err(CursorError::WrongKind)
        } else if actual.filter != expected.filter {
            Err(CursorError::FiltersChanged)
        } else if actual.sort != expected.sort {
            Err(CursorError::SortChanged)
        } else if actual.window != expected.window {
            Err(CursorError::WindowChanged)
        } else {
            Ok(())
        }
    }
}

/// The cursor-signing keys of a node: one active key that signs, and the verify-only keys that a rotation
/// keeps (contract 11.2). The keys come from the caller: configuration now, the `signing_keys` rows later.
#[derive(Clone, Debug)]
pub struct KeyRing {
    active: SigningKey,
    verify_only: Vec<SigningKey>,
}

impl KeyRing {
    /// A ring of one key, active.
    pub fn new(active: SigningKey) -> Self {
        Self {
            active,
            verify_only: Vec::new(),
        }
    }

    /// A ring of keys and their states: exactly one active, ids unique.
    pub fn from_keys(
        keys: impl IntoIterator<Item = (SigningKey, KeyState)>,
    ) -> Result<Self, KeyRingError> {
        let mut active = None;
        let mut verify_only = Vec::new();
        let mut kids = BTreeSet::new();
        for (key, state) in keys {
            if !kids.insert(key.kid.clone()) {
                return Err(KeyRingError::DuplicateKid(key.kid));
            }
            match state {
                KeyState::Active if active.is_some() => {
                    return Err(KeyRingError::SeveralActiveKeys);
                }
                KeyState::Active => active = Some(key),
                KeyState::VerifyOnly => verify_only.push(key),
            }
        }
        let active = active.ok_or(KeyRingError::NoActiveKey)?;
        Ok(Self {
            active,
            verify_only,
        })
    }

    /// The id of the key that signs.
    pub const fn active_kid(&self) -> &Kid {
        &self.active.kid
    }

    /// The state of the key `kid`, when the ring holds it.
    pub fn state(&self, kid: &Kid) -> Option<KeyState> {
        if self.active.kid == *kid {
            Some(KeyState::Active)
        } else if self.verify_only.iter().any(|key| key.kid == *kid) {
            Some(KeyState::VerifyOnly)
        } else {
            None
        }
    }

    /// Rotation (contract 11.2): `key` becomes the active key and the previous active key is kept to verify the
    /// cursors it signed. Retire that key once they have expired, [`crate::CURSOR_TTL_MICROS`] after the
    /// rotation.
    pub fn rotate(&mut self, key: SigningKey) -> Result<(), KeyRingError> {
        if self.state(&key.kid).is_some() {
            return Err(KeyRingError::DuplicateKid(key.kid));
        }
        let previous = std::mem::replace(&mut self.active, key);
        self.verify_only.push(previous);
        Ok(())
    }

    /// Drops the verify-only key `kid`; its cursors no longer open. Returns whether the ring held it. The active
    /// key is never dropped.
    pub fn retire(&mut self, kid: &Kid) -> bool {
        let before = self.verify_only.len();
        self.verify_only.retain(|key| key.kid != *kid);
        self.verify_only.len() != before
    }

    /// The text form of `cursor`, signed with the active key.
    pub fn seal(&self, cursor: &Cursor) -> String {
        let payload = URL_SAFE_NO_PAD.encode(cursor.encode().as_bytes());
        let mut text = format!("{TEXT_VERSION}.{}.{payload}", self.active.kid);
        let tag = self.active.tag(text.as_bytes());
        text.push('.');
        text.push_str(&URL_SAFE_NO_PAD.encode(tag));
        text
    }

    /// Opens `text` for a request bound to `expected`, at `now`. The MAC is verified, with the key the cursor
    /// names, before anything in the payload is read; then the expiry, then the binding.
    pub fn open(
        &self,
        text: &str,
        expected: &Binding,
        now: Timestamp,
    ) -> Result<Cursor, CursorError> {
        if text.len() > MAX_TEXT_LEN {
            return Err(CursorError::Malformed);
        }
        let (signed, tag) = text.rsplit_once('.').ok_or(CursorError::Malformed)?;
        let mut parts = signed.split('.');
        let (Some(version), Some(kid), Some(payload), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(CursorError::Malformed);
        };
        if version != TEXT_VERSION {
            return Err(CursorError::Malformed);
        }
        let key = std::iter::once(&self.active)
            .chain(&self.verify_only)
            .find(|key| key.kid.as_str() == kid)
            .ok_or(CursorError::UnknownKid)?;
        let tag: [u8; TAG_LEN] = URL_SAFE_NO_PAD
            .decode(tag)
            .ok()
            .and_then(|tag| tag.try_into().ok())
            .ok_or(CursorError::Malformed)?;
        if !key.verifies(signed.as_bytes(), &tag) {
            return Err(CursorError::Tampered);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| CursorError::Malformed)?;
        let cursor = Cursor::decode(&bytes).map_err(|_| CursorError::Malformed)?;
        if now >= cursor.expires_at {
            return Err(CursorError::Expired);
        }
        cursor.check(expected)?;
        Ok(cursor)
    }
}
