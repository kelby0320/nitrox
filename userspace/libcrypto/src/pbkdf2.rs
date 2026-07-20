//! PBKDF2-HMAC-SHA256 (RFC 8018 §5.2). Derives `out.len()` key bytes from a
//! password + salt over `iterations` rounds. No `alloc`: output is written into a
//! caller-provided slice; the per-block accumulator is a 32-byte stack array.

use crate::hmac::HmacSha256;
use crate::sha256::DIGEST_LEN;

/// Fill `out` with PBKDF2-HMAC-SHA256(`password`, `salt`, `iterations`).
///
/// `iterations` must be ≥ 1 (a 0 count leaves `out` as the first HMAC only would
/// be undefined; callers pass a real cost). `out` may be any length; it is filled
/// block-by-block (32 bytes per block).
pub fn pbkdf2_hmac_sha256(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    let iters = iterations.max(1);
    // Pre-key an HMAC once; each block/iteration re-clones this keyed state rather
    // than re-absorbing the ipad block (the expensive part of re-keying).
    let base = HmacSha256::new(password);

    let mut block_index: u32 = 1;
    let mut off = 0usize;
    while off < out.len() {
        // U_1 = PRF(P, S || INT_32_BE(i))
        let mut u = {
            let mut mac = base.clone();
            mac.update(salt);
            mac.update(&block_index.to_be_bytes());
            mac.finalize()
        };
        let mut t = u; // T = U_1

        // U_c = PRF(P, U_{c-1}); T ^= U_c
        for _ in 1..iters {
            let mut mac = base.clone();
            mac.update(&u);
            u = mac.finalize();
            for j in 0..DIGEST_LEN {
                t[j] ^= u[j];
            }
        }

        let take = (out.len() - off).min(DIGEST_LEN);
        out[off..off + take].copy_from_slice(&t[..take]);
        off += take;
        block_index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::new();
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn rfc7914_c1() {
        // RFC 7914 §11: PBKDF2-HMAC-SHA-256(P="passwd", S="salt", c=1, dkLen=64).
        let mut out = [0u8; 64];
        pbkdf2_hmac_sha256(b"passwd", b"salt", 1, &mut out);
        assert_eq!(
            hex(&out),
            "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
             49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
        );
    }

    #[test]
    fn rfc7914_c2() {
        // RFC 7914 §11: PBKDF2-HMAC-SHA-256(P="Password", S="NaCl", c=80000, dkLen=64).
        let mut out = [0u8; 64];
        pbkdf2_hmac_sha256(b"Password", b"NaCl", 80000, &mut out);
        assert_eq!(
            hex(&out),
            "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56\
             a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"
        );
    }

    #[test]
    fn known_c4096_32() {
        // Widely-published vector: P="password", S="salt", c=4096, dkLen=32.
        let mut out = [0u8; 32];
        pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut out);
        assert_eq!(
            hex(&out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn partial_block_output() {
        // A 20-byte request (< one 32-byte block) must equal the truncation of a
        // full-block derivation with the same parameters.
        let mut full = [0u8; 32];
        pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut full);
        let mut short = [0u8; 20];
        pbkdf2_hmac_sha256(b"password", b"salt", 4096, &mut short);
        assert_eq!(short, full[..20]);
    }
}
