//! `libcrypto` — hand-rolled cryptographic primitives for Nitrox userspace.
//!
//! `#![no_std]`, **no `alloc`**, `core`-only, **no dependencies** — the same
//! discipline as `kernel/src/libkern/chacha.rs` (the project forbids external
//! crates; see `kernel/CLAUDE.md`). Every routine works on caller-provided slices
//! or fixed stack buffers, so it links unchanged into a bare-target service and,
//! because it is pure `core`, into host tooling (`tools/xtask` seeds password
//! hashes into the image with the *same* code path the on-target verifier runs).
//!
//! Contents:
//! - [`sha256`] — SHA-256 (FIPS 180-4), streaming + one-shot.
//! - [`hmac`] — HMAC-SHA256 (RFC 2104).
//! - [`pbkdf2`] — PBKDF2-HMAC-SHA256 (RFC 8018), the password KDF.
//! - [`password`] — the credential helper: derive + constant-time verify.
//!
//! Consumers: the **auth-service** (password verification) and, later, the
//! **audit subsystem** (hash-chained tamper-evident records) — the "build the
//! hash once, share it" intent recorded in the implementation plan. Under
//! `cargo test` the crate compiles as host `std` so the published vectors run on
//! the host. See `docs/architecture/session-and-auth.md`.

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
}
