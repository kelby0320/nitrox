//! Password credential helper: derive a verifier from a password + salt, and
//! verify a candidate in constant time. A thin, opinionated layer over
//! [`crate::pbkdf2`] so the auth-service and the build-time seeder (`tools/xtask`)
//! agree on one KDF by construction.

use crate::pbkdf2::pbkdf2_hmac_sha256;

/// Stored verifier length in bytes (one SHA-256 block of derived key material).
pub const VERIFIER_LEN: usize = 32;

/// Default PBKDF2 iteration count. Modest on purpose — this is a hobby OS proving
/// the login path under emulation, not a production authenticator; the count is
/// stored per record (see the auth user-DB format) so it can be raised without a
/// format change. Chosen to be noticeable but not slow the headless test boot.
pub const DEFAULT_ITERATIONS: u32 = 4096;

/// Derive the `VERIFIER_LEN`-byte verifier for `password` under `salt` +
/// `iterations`. The stored credential is `(salt, iterations, derive(...))`.
pub fn derive(password: &[u8], salt: &[u8], iterations: u32) -> [u8; VERIFIER_LEN] {
    let mut out = [0u8; VERIFIER_LEN];
    pbkdf2_hmac_sha256(password, salt, iterations, &mut out);
    out
}

/// Verify a candidate `password` against a stored `expected` verifier, in constant
/// time. Returns `false` on any mismatch (including a wrong-length `expected`).
pub fn verify(password: &[u8], salt: &[u8], iterations: u32, expected: &[u8]) -> bool {
    let got = derive(password, salt, iterations);
    crate::ct_eq(&got, expected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_then_verify_roundtrip() {
        let salt = b"\x01\x02\x03\x04\x05\x06\x07\x08";
        let v = derive(b"correct horse", salt, DEFAULT_ITERATIONS);
        assert!(verify(b"correct horse", salt, DEFAULT_ITERATIONS, &v));
        assert!(!verify(b"wrong password", salt, DEFAULT_ITERATIONS, &v));
    }

    #[test]
    fn salt_changes_verifier() {
        let a = derive(b"same-password", b"salt-aaaa", DEFAULT_ITERATIONS);
        let b = derive(b"same-password", b"salt-bbbb", DEFAULT_ITERATIONS);
        assert_ne!(a, b);
    }

    #[test]
    fn wrong_length_expected_rejected() {
        let salt = b"saltsalt";
        let v = derive(b"pw", salt, DEFAULT_ITERATIONS);
        assert!(!verify(b"pw", salt, DEFAULT_ITERATIONS, &v[..16]));
    }
}
