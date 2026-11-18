//! The hashes senders put on the wire instead of raw values (contracts 9.4 and 9.6).

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Digest as _, Sha256};

/// The label that prefixes the hashed `requestState` (contract 9.6).
const MRTR_LABEL: &[u8] = b"crucible.mrtr.v1";

/// Contract 9.4: the first 16 bytes of SHA-256 of the raw session id, as 32 lowercase hex characters.
pub fn session_hash(raw: &str) -> String {
    hex16(&Sha256::digest(raw.as_bytes()))
}

/// Contract 9.6: the first 16 bytes of HMAC-SHA256 keyed with the sender's secret key string over
/// `crucible.mrtr.v1`, one zero byte and the `requestState` bytes, as 32 lowercase hex characters.
pub fn mrtr_hash(secret_key: &str, request_state: &str) -> String {
    // HMAC takes keys of any length, so this never fails.
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret_key.as_bytes()) else {
        return String::new();
    };
    mac.update(MRTR_LABEL);
    mac.update(&[0]);
    mac.update(request_state.as_bytes());
    hex16(&mac.finalize().into_bytes())
}

fn hex16(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect()
}
