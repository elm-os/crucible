//! Randomness (contract 2.2).
//!
//! One ChaCha20 implementation serves production and simulation; they differ only in the root seed
//! (OS entropy against the run seed). Streams form a tree: [`Rng::fork`] derives a child from the parent's
//! seed and a label with keyed BLAKE3, so a child depends on the root seed and the label path only, never on
//! how many values any stream has produced.

use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use rand_chacha::ChaCha20Rng;
use rand_core::{Rng as _, SeedableRng, TryCryptoRng, TryRng};

/// A cryptographically strong random stream. Secrets (API keys, session tokens, CSRF tokens, signing keys,
/// nonces) must come from here.
pub trait Rng: fmt::Debug + Send + Sync {
    /// The next 64 bits of the stream.
    fn next_u64(&self) -> u64;

    /// Fills `dest` from the stream.
    fn fill_bytes(&self, dest: &mut [u8]);

    /// A child stream that is a function of the root seed and the label path only. Forking the same label
    /// twice yields two handles that produce the same sequence, so each consumer must use its own label.
    fn fork(&self, label: &str) -> Arc<dyn Rng>;
}

const STREAM_CONTEXT: &str = "crucible-env 2026-11-17 rng stream key";

/// The ChaCha20 stream behind every [`Rng`], in production and in simulation.
pub struct ChaChaRng {
    /// Identifies this node of the fork tree; children are derived from it. Never used as a cipher key.
    seed: [u8; 32],
    stream: Mutex<ChaCha20Rng>,
}

impl ChaChaRng {
    /// Builds the root stream from a 32-byte seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        // The stream key is derived from the seed under a separate domain, so stream output reveals nothing
        // about the keys of forked children.
        let key = blake3::derive_key(STREAM_CONTEXT, &seed);
        Self {
            seed,
            stream: Mutex::new(ChaCha20Rng::from_seed(key)),
        }
    }

    /// Builds the root stream from a 64-bit run seed (simulation and tests).
    pub fn from_u64(seed: u64) -> Self {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&seed.to_le_bytes());
        Self::from_seed(bytes)
    }
}

impl fmt::Debug for ChaChaRng {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The seed and the stream state are secrets.
        f.debug_struct("ChaChaRng").finish_non_exhaustive()
    }
}

impl Rng for ChaChaRng {
    fn next_u64(&self) -> u64 {
        self.stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .next_u64()
    }

    fn fill_bytes(&self, dest: &mut [u8]) {
        self.stream
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fill_bytes(dest);
    }

    fn fork(&self, label: &str) -> Arc<dyn Rng> {
        let child = blake3::keyed_hash(&self.seed, label.as_bytes());
        Arc::new(Self::from_seed(*child.as_bytes()))
    }
}

/// Adapts an [`Rng`] to the `rand_core` 0.10 traits (`TryRng` and `TryCryptoRng`, and through their blanket
/// implementations `Rng`, `RngCore` and `CryptoRng`) for libraries that take a generator.
#[derive(Clone, Debug)]
pub struct RngAdapter(pub Arc<dyn Rng>);

impl TryRng for RngAdapter {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Infallible> {
        // The high half of one 64-bit draw.
        Ok((self.0.next_u64() >> 32) as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Infallible> {
        Ok(self.0.next_u64())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Infallible> {
        self.0.fill_bytes(dst);
        Ok(())
    }
}

impl TryCryptoRng for RngAdapter {}

#[cfg(test)]
mod tests {
    use proptest::prelude::{prop_assert_eq, prop_assert_ne, proptest};

    use super::*;

    fn take(rng: &dyn Rng, n: usize) -> Vec<u64> {
        (0..n).map(|_| rng.next_u64()).collect()
    }

    #[test]
    fn stream_is_chacha20_keyed_by_the_derived_key() {
        // Pins the construction: any change to the derivation or the cipher changes every simulation run.
        let rng = ChaChaRng::from_u64(1);
        let mut seed = [0; 32];
        seed[0] = 1;
        let mut reference = ChaCha20Rng::from_seed(blake3::derive_key(STREAM_CONTEXT, &seed));
        assert_eq!(
            take(&rng, 4),
            (0..4).map(|_| reference.next_u64()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fill_bytes_and_next_u64_read_the_same_stream() {
        let a = ChaChaRng::from_u64(9);
        let b = ChaChaRng::from_u64(9);
        let mut bytes = [0; 16];
        a.fill_bytes(&mut bytes);
        let words = [b.next_u64().to_le_bytes(), b.next_u64().to_le_bytes()].concat();
        assert_eq!(bytes.as_slice(), words.as_slice());
    }

    #[test]
    fn adapter_satisfies_the_rand_core_traits() {
        // `RngCore` is the deprecated 0.10 name that older call sites still bound on.
        #[allow(deprecated)]
        fn needs_crypto_rng<R: rand_core::CryptoRng + rand_core::RngCore>(rng: &mut R) -> u64 {
            rng.next_u64()
        }
        let mut adapter = RngAdapter(Arc::new(ChaChaRng::from_u64(3)));
        assert_eq!(
            needs_crypto_rng(&mut adapter),
            ChaChaRng::from_u64(3).next_u64()
        );
        let mut buf = [0; 5];
        adapter.fill_bytes(&mut buf);
        assert_ne!(buf, [0; 5]);
    }

    proptest! {
        #[test]
        fn same_seed_gives_the_same_streams(seed: u64, label in "[a-z/]{0,12}") {
            let a = ChaChaRng::from_u64(seed);
            let b = ChaChaRng::from_u64(seed);
            prop_assert_eq!(take(&a, 8), take(&b, 8));
            prop_assert_eq!(take(a.fork(&label).as_ref(), 8), take(b.fork(&label).as_ref(), 8));
        }

        /// Adding a consumer (draws on the parent, a new sibling fork and draws on it, at any point) never
        /// changes another label's stream.
        #[test]
        fn a_fork_depends_only_on_the_seed_and_the_label_path(
            seed: u64,
            parent_draws in 0usize..64,
            sibling_draws in 0usize..64,
        ) {
            let quiet = ChaChaRng::from_u64(seed);
            let expected = take(quiet.fork("wal").fork("fsync-fault").as_ref(), 16);

            let busy = ChaChaRng::from_u64(seed);
            take(&busy, parent_draws);
            let sibling = busy.fork("net");
            take(sibling.as_ref(), sibling_draws);
            let wal = busy.fork("wal");
            take(wal.as_ref(), sibling_draws);
            take(&busy, parent_draws);
            prop_assert_eq!(take(wal.fork("fsync-fault").as_ref(), 16), expected);
        }

        #[test]
        fn distinct_seeds_labels_and_paths_give_distinct_streams(seed: u64) {
            let root = ChaChaRng::from_u64(seed);
            let streams = [
                take(&ChaChaRng::from_u64(seed), 4),
                take(&ChaChaRng::from_u64(seed.wrapping_add(1)), 4),
                take(root.fork("a").as_ref(), 4),
                take(root.fork("b").as_ref(), 4),
                take(root.fork("").as_ref(), 4),
                take(root.fork("a").fork("b").as_ref(), 4),
                take(root.fork("b").fork("a").as_ref(), 4),
                take(root.fork("a/b").as_ref(), 4),
                take(root.fork("ab").as_ref(), 4),
            ];
            for (i, x) in streams.iter().enumerate() {
                for y in &streams[i + 1..] {
                    prop_assert_ne!(x, y);
                }
            }
        }
    }
}
