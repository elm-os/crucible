//! The production root seed.

use crucible_env::ChaChaRng;

/// The shared ChaCha20 stream, seeded with 32 bytes from the operating system's entropy source. This seed is
/// the only difference between production and simulated randomness.
pub fn os_seeded_rng() -> Result<ChaChaRng, getrandom::Error> {
    let mut seed = [0; 32];
    getrandom::fill(&mut seed)?;
    Ok(ChaChaRng::from_seed(seed))
}
