//! HMAC-SHA256 (RFC 2104), streaming. Keyed-hash message authentication used by
//! [`crate::pbkdf2`] and available on its own. No `alloc`: the key is folded into
//! two fixed 64-byte pads.

use crate::sha256::{BLOCK_LEN, DIGEST_LEN, Sha256};

/// A streaming HMAC-SHA256. Construct with a key, `update` the message, `finalize`.
#[derive(Clone)]
pub struct HmacSha256 {
    /// Inner hash, pre-seeded with `K ⊕ ipad`.
    inner: Sha256,
    /// The `K ⊕ opad` block, replayed at finalize.
    opad: [u8; BLOCK_LEN],
}

impl HmacSha256 {
    /// Key the MAC. Keys longer than the block are hashed down first (RFC 2104);
    /// shorter keys are zero-padded.
    pub fn new(key: &[u8]) -> Self {
        // Normalise the key to exactly one block.
        let mut k = [0u8; BLOCK_LEN];
        if key.len() > BLOCK_LEN {
            let d = crate::sha256::sha256(key);
            k[..DIGEST_LEN].copy_from_slice(&d);
        } else {
            k[..key.len()].copy_from_slice(key);
        }

        let mut ipad = [0x36u8; BLOCK_LEN];
        let mut opad = [0x5cu8; BLOCK_LEN];
        for i in 0..BLOCK_LEN {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }

        let mut inner = Sha256::new();
        inner.update(&ipad);
        HmacSha256 { inner, opad }
    }

    /// Absorb message bytes.
    pub fn update(&mut self, data: &[u8]) {
        self.inner.update(data);
    }

    /// Finish and return the 32-byte MAC.
    pub fn finalize(self) -> [u8; DIGEST_LEN] {
        let inner = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(&self.opad);
        outer.update(&inner);
        outer.finalize()
    }
}

/// One-shot HMAC-SHA256 of `msg` under `key`.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; DIGEST_LEN] {
    let mut m = HmacSha256::new(key);
    m.update(msg);
    m.finalize()
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
    fn rfc4231_case1() {
        // RFC 4231 Test Case 1: key = 0x0b×20, data = "Hi There".
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn rfc4231_case2() {
        // RFC 4231 Test Case 2: key = "Jefe" (short key path).
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn rfc4231_case3_long_key() {
        // RFC 4231 Test Case 3: key = 0xaa×131 (> block → hashed-down key path).
        let key = [0xaau8; 131];
        assert_eq!(
            hex(&hmac_sha256(&key, b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn streaming_matches_oneshot() {
        let key = b"a-reasonable-key";
        let msg: &[u8] = b"chunked message input";
        let one = hmac_sha256(key, msg);
        let mut m = HmacSha256::new(key);
        m.update(&msg[..5]);
        m.update(&msg[5..]);
        assert_eq!(m.finalize(), one);
    }
}
