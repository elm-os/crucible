//! The cursor-signing keys of `crucible serve` (contracts 11.2 and 12.4).
//!
//! Signing keys will be `signing_keys` rows of the control plane. Until then, with a telemetry store the one key
//! is derived from the master key, as the ClickHouse identity passwords are: the HKDF-SHA256 subkey with the info
//! string `crucible/cursor-signing/v1/<generation>`, under the key id `master-<generation>`. Every node that
//! shares the master key opens the cursors of every other node, across restarts, and no further secret is
//! configured. In the ingest-only mode no list can issue a cursor; a random key from the `Env`'s `Rng` lets the
//! API refuse the cursors it is sent.

use crucible_cursor::{KeyRing, Kid, SigningKey};
use crucible_env::Env;

use crate::store::MasterKey;

/// The generation of the derived key.
const GENERATION: u32 = 0;

/// The ring of the key derived from `master`.
pub(crate) fn derived(master: &MasterKey) -> anyhow::Result<KeyRing> {
    let kid = Kid::new(&format!("master-{GENERATION}"))?;
    Ok(KeyRing::new(SigningKey::new(
        kid,
        master.cursor_key(GENERATION),
    )))
}

/// A ring of one random key.
pub(crate) fn random(env: &Env) -> anyhow::Result<KeyRing> {
    let rng = env.rng.fork("crucible/cursor-keys");
    let mut secret = [0; 32];
    rng.fill_bytes(&mut secret);
    let kid = Kid::new(&format!("random-{:016x}", rng.next_u64()))?;
    Ok(KeyRing::new(SigningKey::new(kid, secret)))
}
