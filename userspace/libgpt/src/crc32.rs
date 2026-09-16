//! CRC-32 as GPT uses it (the ordinary reflected polynomial, IEEE 802.3).
//!
//! **Hand-rolled because it is twenty lines**, and because a partition table's two checksums are
//! the only thing standing between "the firmware reads your table" and "the firmware ignores it
//! and boots nothing". Nothing else in the tree had one.
//!
//! Bitwise rather than table-driven: a table is 1 KiB of static data to save microseconds on
//! buffers that are at most 16 KiB, written once per install.

/// The reflected polynomial, which is what every CRC-32 in this family uses.
const POLY: u32 = 0xEDB8_8320;

/// CRC-32 of `bytes` — initial value all-ones, final complement, as GPT specifies.
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            // The low bit decides: shift out, and mix the polynomial back in when it was set.
            crc = (crc >> 1) ^ (POLY & (!(crc & 1)).wrapping_add(1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The published vectors**, which is the whole reason to trust an implementation of a
    /// checksum: a wrong one is self-consistent and fails only on somebody else's firmware.
    #[test]
    fn the_standard_vectors_match() {
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"abc"), 0x3524_41C2);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn a_changed_byte_changes_the_sum() {
        let a = crc32(b"nitrox-root");
        let b = crc32(b"nitrox-live");
        assert_ne!(a, b);
        // And length matters: a checksum that ignored trailing zeros would accept a truncated
        // entry array, which is exactly the corruption these two sums exist to catch.
        assert_ne!(crc32(&[0u8; 16]), crc32(&[0u8; 32]));
    }
}
