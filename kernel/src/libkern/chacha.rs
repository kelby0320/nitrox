//! ChaCha20 keystream generator and the [`ChaCha20Rng`] CSPRNG.
//!
//! A hand-rolled ChaCha20 (RFC 8439) — no external crates (per `kernel/CLAUDE.md`).
//! Two layers:
//!
//! - [`chacha20_block`]: the RFC 8439 §2.3 block function — `(key, counter,
//!   nonce) -> 64` keystream bytes. Pure, deterministic, integer-only (no FPU /
//!   AES-NI; see `docs/architecture/entropy.md` § "Why ChaCha20").
//! - [`ChaCha20Rng`]: the kernel CSPRNG built on it, with **fast key erasure** for
//!   forward secrecy (DJB's design): every [`fill`](ChaCha20Rng::fill) derives the
//!   next key from fresh keystream and overwrites the old key before returning, so
//!   capturing the current key cannot reconstruct earlier output.
//!
//! The pool that *seeds* this (hardware RNG + interrupt jitter) and the *policy*
//! for when to [`reseed`](ChaCha20Rng::reseed) live in the entropy subsystem
//! (Phase 2 slice 2, Part C); this module is the deterministic primitive.

/// The four ChaCha state constants — the ASCII of `"expand 32-byte k"` as
/// little-endian `u32`s (RFC 8439 §2.3).
const CONSTANTS: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// Number of ChaCha double-rounds (20 rounds total) — the "20" in ChaCha20.
const DOUBLE_ROUNDS: usize = 10;

/// The keystream block size in bytes.
pub const BLOCK_LEN: usize = 64;

/// One ChaCha quarter-round on four state words (RFC 8439 §2.1).
#[inline]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The ChaCha20 block function (RFC 8439 §2.3): produce the 64-byte keystream
/// block for `key`, the 32-bit block `counter`, and the 96-bit `nonce`.
///
/// Little-endian throughout, per the RFC. Pure and deterministic.
pub fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u8; BLOCK_LEN] {
    // Build the initial state: constants | key (8 words) | counter | nonce (3 words).
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&CONSTANTS);
    for i in 0..8 {
        state[4 + i] = u32::from_le_bytes([
            key[4 * i],
            key[4 * i + 1],
            key[4 * i + 2],
            key[4 * i + 3],
        ]);
    }
    state[12] = counter;
    for i in 0..3 {
        state[13 + i] = u32::from_le_bytes([
            nonce[4 * i],
            nonce[4 * i + 1],
            nonce[4 * i + 2],
            nonce[4 * i + 3],
        ]);
    }

    // 20 rounds = 10 double-rounds (column rounds then diagonal rounds).
    let mut work = state;
    for _ in 0..DOUBLE_ROUNDS {
        // Column rounds.
        quarter_round(&mut work, 0, 4, 8, 12);
        quarter_round(&mut work, 1, 5, 9, 13);
        quarter_round(&mut work, 2, 6, 10, 14);
        quarter_round(&mut work, 3, 7, 11, 15);
        // Diagonal rounds.
        quarter_round(&mut work, 0, 5, 10, 15);
        quarter_round(&mut work, 1, 6, 11, 12);
        quarter_round(&mut work, 2, 7, 8, 13);
        quarter_round(&mut work, 3, 4, 9, 14);
    }

    // Add the working state back into the original and serialize little-endian.
    let mut out = [0u8; BLOCK_LEN];
    for i in 0..16 {
        let word = work[i].wrapping_add(state[i]);
        out[4 * i..4 * i + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

/// A ChaCha20-based CSPRNG with fast key erasure.
///
/// Holds only its 32-byte key. Each [`fill`](Self::fill) generates a contiguous
/// keystream from the current key (nonce and starting counter fixed at zero —
/// sound because the key is unique per fill), reserves the **first 32 bytes** as
/// the next key, hands out the remainder, and then overwrites the key. A capture
/// of the post-fill key therefore cannot reconstruct the just-served output (the
/// block function is one-way), giving forward secrecy.
///
/// Key material is a plain `[u8; 32]`; zeroize-on-drop is a later hardening item
/// (`docs/architecture/entropy.md` § Deferred), not built here.
pub struct ChaCha20Rng {
    key: [u8; 32],
}

impl ChaCha20Rng {
    /// Construct a generator from a 32-byte seed (the seed becomes the initial
    /// key). Deterministic: the same seed yields the same output sequence. `const`
    /// so it can initialize a `static` (the entropy subsystem keys it at boot).
    pub const fn from_seed(seed: [u8; 32]) -> Self {
        Self { key: seed }
    }

    /// Fold fresh entropy into the key by XOR. The next [`fill`](Self::fill) draws
    /// from the mixed key. Used by the entropy subsystem (Part C) to reseed from
    /// the pool; XOR so a reseed can only add uncertainty, never reduce it.
    pub fn reseed(&mut self, seed: &[u8; 32]) {
        for (k, s) in self.key.iter_mut().zip(seed.iter()) {
            *k ^= *s;
        }
    }

    /// Fill `out` with CSPRNG output, then erase the key (fast key erasure).
    ///
    /// The logical keystream from the current key is `[0..32)` = the next key and
    /// `[32..)` = output. We walk it block by block so an arbitrarily large `out`
    /// needs no large scratch buffer. The key is rotated to the freshly-derived
    /// value before returning — this overwrite is the load-bearing forward-secrecy
    /// step (output above came from the *old* key, unrecoverable from the new one).
    pub fn fill(&mut self, out: &mut [u8]) {
        const NONCE: [u8; 12] = [0; 12];
        let mut new_key = [0u8; 32];
        let mut counter: u32 = 0;
        let end = 32 + out.len();
        let mut pos = 0usize; // index into the logical stream
        let mut out_off = 0usize;
        while pos < end {
            let block = chacha20_block(&self.key, counter, &NONCE);
            counter = counter.wrapping_add(1);
            for &b in block.iter() {
                if pos >= end {
                    break;
                }
                if pos < 32 {
                    new_key[pos] = b;
                } else {
                    out[out_off] = b;
                    out_off += 1;
                }
                pos += 1;
            }
        }
        self.key = new_key;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_round_rfc_vector() {
        // RFC 8439 §2.1.1: a single quarter-round on four words.
        let mut s = [0u32; 16];
        s[0] = 0x1111_1111;
        s[1] = 0x0102_0304;
        s[2] = 0x9b8d_6f43;
        s[3] = 0x0123_4567;
        quarter_round(&mut s, 0, 1, 2, 3);
        assert_eq!(s[0], 0xea2a_92f4);
        assert_eq!(s[1], 0xcb1c_f8ce);
        assert_eq!(s[2], 0x4581_472e);
        assert_eq!(s[3], 0x5881_c4bb);
    }

    #[test]
    fn block_function_rfc_vector() {
        // RFC 8439 §2.3.2: key 00..1f, counter 1, nonce 00:00:00:09:..:4a:..:00.
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let block = chacha20_block(&key, 1, &nonce);
        let expected: [u8; 64] = [
            0x10, 0xf1, 0xe7, 0xe4, 0xd1, 0x3b, 0x59, 0x15, 0x50, 0x0f, 0xdd, 0x1f, 0xa3, 0x20,
            0x71, 0xc4, 0xc7, 0xd1, 0xf4, 0xc7, 0x33, 0xc0, 0x68, 0x03, 0x04, 0x22, 0xaa, 0x9a,
            0xc3, 0xd4, 0x6c, 0x4e, 0xd2, 0x82, 0x64, 0x46, 0x07, 0x9f, 0xaa, 0x09, 0x14, 0xc2,
            0xd7, 0x05, 0xd9, 0x8b, 0x02, 0xa2, 0xb5, 0x12, 0x9c, 0xd1, 0xde, 0x16, 0x4e, 0xb9,
            0xcb, 0xd0, 0x83, 0xe8, 0xa2, 0x50, 0x3c, 0x4e,
        ];
        assert_eq!(block, expected);
    }

    /// Reference fast-key-erasure fill computed directly from the block function:
    /// the logical stream is `[0..32)` = next key, `[32..)` = output.
    fn ref_output(seed: [u8; 32], len: usize) -> Vec<u8> {
        const NONCE: [u8; 12] = [0; 12];
        let mut stream = Vec::new();
        let mut counter = 0u32;
        while stream.len() < 32 + len {
            stream.extend_from_slice(&chacha20_block(&seed, counter, &NONCE));
            counter += 1;
        }
        stream[32..32 + len].to_vec()
    }

    #[test]
    fn fill_matches_fast_key_erasure_layout() {
        let seed = [7u8; 32];
        for &len in &[0usize, 1, 31, 32, 33, 63, 64, 65, 200] {
            let mut rng = ChaCha20Rng::from_seed(seed);
            let mut got = vec![0u8; len];
            rng.fill(&mut got);
            assert_eq!(got, ref_output(seed, len), "len {len}");
        }
    }

    #[test]
    fn same_seed_same_stream() {
        let mut a = ChaCha20Rng::from_seed([0x42; 32]);
        let mut b = ChaCha20Rng::from_seed([0x42; 32]);
        let mut xa = [0u8; 96];
        let mut xb = [0u8; 96];
        a.fill(&mut xa);
        b.fill(&mut xb);
        assert_eq!(xa, xb);
    }

    #[test]
    fn fast_key_erasure_advances_the_key() {
        // Two RNGs from the same seed; advance one by an extra fill. Their next
        // outputs must differ — proof the first fill rotated the key.
        let mut a = ChaCha20Rng::from_seed([0x99; 32]);
        let mut b = ChaCha20Rng::from_seed([0x99; 32]);
        a.fill(&mut [0u8; 16]); // a rekeys once; b does not
        let mut xa = [0u8; 32];
        let mut xb = [0u8; 32];
        a.fill(&mut xa);
        b.fill(&mut xb);
        assert_ne!(xa, xb);
    }

    #[test]
    fn reseed_changes_the_stream() {
        let mut a = ChaCha20Rng::from_seed([0u8; 32]);
        let mut b = ChaCha20Rng::from_seed([0u8; 32]);
        let mut t = [0u8; 32];
        t[0] = 1;
        b.reseed(&t);
        let mut xa = [0u8; 32];
        let mut xb = [0u8; 32];
        a.fill(&mut xa);
        b.fill(&mut xb);
        assert_ne!(xa, xb);
    }
}
