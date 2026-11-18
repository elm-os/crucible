//! CRC32C (Castagnoli) checksum used by every header, entry and payload.

/// CRC32C of `bytes`.
///
/// Product builds use the `crc32c` crate. Under Kani the checksum is the bitwise model below, which the model
/// checker can reason about; a property test ties the model to the product routine.
pub(crate) fn crc32c(bytes: &[u8]) -> u32 {
    #[cfg(not(kani))]
    {
        ::crc32c::crc32c(bytes)
    }
    #[cfg(kani)]
    {
        crc32c_model(bytes)
    }
}

/// One byte step of the bitwise CRC32C model (reflected polynomial `0x82F63B78`).
#[cfg(any(kani, test))]
pub(crate) fn crc32c_update(mut state: u32, byte: u8) -> u32 {
    state ^= u32::from(byte);
    let mut bit = 0;
    while bit < 8 {
        let mask = (state & 1).wrapping_neg();
        state = (state >> 1) ^ (0x82F6_3B78 & mask);
        bit += 1;
    }
    state
}

/// Bitwise CRC32C: the byte step folded from an initial value of all ones, then a final XOR of all ones.
#[cfg(any(kani, test))]
pub(crate) fn crc32c_model(bytes: &[u8]) -> u32 {
    let mut state = !0u32;
    for &byte in bytes {
        state = crc32c_update(state, byte);
    }
    !state
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    #[test]
    fn known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_model(b"123456789"), 0xE306_9283);
    }

    proptest! {
        #[test]
        fn model_equals_product_routine(bytes in proptest::collection::vec(any::<u8>(), 0..300)) {
            prop_assert_eq!(crc32c_model(&bytes), ::crc32c::crc32c(&bytes));
        }
    }
}
