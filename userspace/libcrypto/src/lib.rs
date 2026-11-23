//! `libcrypto` — hand-rolled cryptographic primitives for Nitrox userspace.
//!
//! `#![no_std]`, **no `alloc`**, `core`-only, **no dependencies** — the same
//! discipline as `kernel/src/libkern/chacha.rs` (the project forbids external
//! crates; see `kernel/CLAUDE.md`). Every routine works on caller-provided slices
//! or fixed stack buffers, so it links unchanged into a bare-target service and,
//! because it is pure `core`, into host tooling (`tools/xtask` seeds password
//! hashes into the image with the *same* code path the on-target verifier runs).
//!
//! Consumers: the **auth-service** (password verification) and, later, the
//! **audit subsystem** (hash-chained tamper-evident records). See
//! `docs/architecture/session-and-auth.md`.

#![cfg_attr(not(test), no_std)]

pub mod hmac;
pub mod password;
pub mod pbkdf2;
pub mod sha256;

pub use hmac::{HmacSha256, hmac_sha256};
pub use pbkdf2::pbkdf2_hmac_sha256;
pub use sha256::{Sha256, sha256};

/// Compare two byte slices for equality in **constant time** (no early return on
/// the first differing byte), so a caller comparing secrets does not leak how much
/// of a candidate matched via timing. Unequal lengths compare `false` (the length
/// is not itself secret here — the stored digest length is fixed).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abcd", b"abcd"));
        assert!(!ct_eq(b"abcd", b"abce"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    /// **Every bit of every byte counts**: 32-byte values differing by one bit anywhere compare
    /// unequal, where comparing the last byte alone would accept most of them.
    #[test]
    fn values_differing_by_any_one_bit_compare_unequal() {
        let a: [u8; 32] = core::array::from_fn(|i| i as u8 * 7);
        for i in 0..32 {
            for bit in 0..8 {
                let mut b = a;
                b[i] ^= 1 << bit;
                assert!(!ct_eq(&a, &b), "byte {i}, bit {bit}");
            }
        }
        let same = a;
        assert!(ct_eq(&a, &same));
    }
}
