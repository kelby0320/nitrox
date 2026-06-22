//! The kernel entropy subsystem: a global CSPRNG seeded from hardware
//! (`RDSEED`/`RDRAND`) and software (interrupt-timing jitter) sources.
//!
//! Design: `docs/architecture/entropy.md`. A fixed-size **pool** accumulates raw
//! samples; a conservative **entropy estimate** gates a one-shot **seeded** latch
//! at 256 bits; the [`ChaCha20Rng`] CSPRNG is keyed from the pool and reseeded as
//! fresh entropy arrives. Every source — hardware RNG included — is *absorbed into
//! the pool, never used as output directly* (mix-don't-trust).
//!
//! ## Lifecycle
//!
//! [`init`] runs once at boot (after the timer/APIC are up, before the handle
//! table) — it draws a hardware burst, mixes early jitter, **always keys the
//! CSPRNG** from whatever entropy it gathered (so kernel draws work even with no
//! hardware RNG), and latches `seeded` if it crossed the gate. Thereafter
//! [`on_irq_sample`] folds interrupt-timing jitter in from IRQ context, and
//! [`fill`] / [`seed_u64`] draw CSPRNG output.
//!
//! ## Locking
//!
//! All state is behind one [`IrqSpinLock`] — a **leaf** (held alone, briefly, with
//! interrupts masked; no nested locks, no allocation). The IF-masking makes the
//! IRQ-context sampler ([`on_irq_sample`]) and the syscall/boot-context draw
//! ([`fill`]) mutually safe on one CPU. See `kernel/docs/lock-ordering.md`.

use crate::libkern::{ChaCha20Rng, IrqSpinLock};

/// Pool size in bytes. Sized to one ChaCha key — the pool *is* the CSPRNG key
/// material once conditioned by the key schedule.
const POOL_LEN: usize = 32;

/// Estimated entropy bits required before the pool latches **seeded**.
const SEED_BITS: u32 = 256;

/// Entropy credited per successful hardware `u64` draw. Conservative: a full
/// 64-bit hardware sample is credited its width; the real guarantee is "drew from
/// a hardware RNG", not this estimate.
const EST_PER_HW: u32 = 64;

/// Jitter samples required to credit one estimated bit. Interrupt timing carries
/// only a fraction of a bit each, so credit sparsely.
const JITTER_PER_BIT: u32 = 8;

/// Hardware `u64` draws attempted at boot. Eight × 64 bits = 512 estimated bits —
/// well past the gate when a hardware RNG is present.
const HW_DRAWS: usize = 8;

/// Bytes of CSPRNG output between automatic reseeds (fold fresh pool entropy into
/// the key). Bounds how much output rides a single key on a busy system.
const RESEED_BYTES: u32 = 1 << 20; // 1 MiB

/// Diffusion multiplier for the pool absorb (the SplitMix64 / fibonacci-hash
/// constant). Mixing only — cryptographic conditioning is ChaCha's key schedule.
const ABSORB_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// The entropy subsystem's interior state (see the module docs).
struct EntropyState {
    /// Entropy accumulator. Conditioned into the CSPRNG key by the key schedule.
    pool: [u8; POOL_LEN],
    /// Rotating 8-byte absorb offset selector.
    absorb_ctr: u32,
    /// Jitter samples seen, for fractional crediting (`JITTER_PER_BIT` → 1 bit).
    jitter_ctr: u32,
    /// Conservative estimated entropy bits absorbed.
    est_bits: u32,
    /// `true` once [`init`] has keyed the CSPRNG (kernel draws are valid).
    keyed: bool,
    /// One-shot: set when `est_bits` first reaches [`SEED_BITS`]. Never cleared.
    seeded: bool,
    /// The CSPRNG, keyed from the pool.
    csprng: ChaCha20Rng,
    /// CSPRNG output since the last reseed (drives [`RESEED_BYTES`]).
    bytes_since_reseed: u32,
}

impl EntropyState {
    /// A fresh, **unkeyed** state (CSPRNG holds a zero key until [`init`] keys it).
    const fn new() -> Self {
        Self {
            pool: [0; POOL_LEN],
            absorb_ctr: 0,
            jitter_ctr: 0,
            est_bits: 0,
            keyed: false,
            seeded: false,
            csprng: ChaCha20Rng::from_seed([0; 32]),
            bytes_since_reseed: 0,
        }
    }

    /// Fold `x` into 8 bytes of the pool at a rotating offset, with a SplitMix-style
    /// diffusion so successive samples accumulate rather than overwrite. Mixing
    /// only — does not, by itself, credit any entropy.
    fn absorb(&mut self, x: u64) {
        let off = (self.absorb_ctr as usize % (POOL_LEN / 8)) * 8;
        let cur = u64::from_le_bytes(self.pool[off..off + 8].try_into().unwrap());
        let mixed = (cur ^ x).wrapping_mul(ABSORB_MUL).rotate_left(31);
        self.pool[off..off + 8].copy_from_slice(&mixed.to_le_bytes());
        self.absorb_ctr = self.absorb_ctr.wrapping_add(1);
    }

    /// Credit `bits` of estimated entropy (saturating at [`SEED_BITS`] so the
    /// counter never wraps on a long-running system).
    fn credit(&mut self, bits: u32) {
        self.est_bits = self.est_bits.saturating_add(bits).min(SEED_BITS);
    }

    /// Account one jitter sample, crediting a bit every [`JITTER_PER_BIT`] of them.
    fn credit_jitter(&mut self) {
        self.jitter_ctr = self.jitter_ctr.wrapping_add(1);
        if self.jitter_ctr % JITTER_PER_BIT == 0 {
            self.credit(1);
        }
    }

    /// (Re)key the CSPRNG from the current pool and mark the state keyed.
    fn key_from_pool(&mut self) {
        self.csprng.reseed(&self.pool);
        self.keyed = true;
    }

    /// If enough entropy has accumulated and we have not latched yet, key the
    /// CSPRNG from the pool at the crossing point and latch `seeded` (one-shot).
    fn maybe_latch(&mut self) {
        if !self.seeded && self.est_bits >= SEED_BITS {
            self.csprng.reseed(&self.pool);
            self.seeded = true;
        }
    }

    /// Draw `out.len()` CSPRNG bytes, folding fresh pool entropy into the key once
    /// output since the last reseed crosses [`RESEED_BYTES`].
    fn draw(&mut self, out: &mut [u8]) {
        debug_assert!(self.keyed, "entropy::draw before init keyed the CSPRNG");
        self.csprng.fill(out);
        self.bytes_since_reseed = self.bytes_since_reseed.saturating_add(out.len() as u32);
        if self.bytes_since_reseed >= RESEED_BYTES {
            self.csprng.reseed(&self.pool);
            self.bytes_since_reseed = 0;
        }
    }
}

/// The single global entropy state (a leaf `IrqSpinLock`; see the module docs).
static ENTROPY: IrqSpinLock<EntropyState> = IrqSpinLock::new(EntropyState::new());

/// Initialize the entropy subsystem once at boot: draw a hardware burst, mix in
/// early jitter, key the CSPRNG, and latch `seeded` if the gate was crossed.
///
/// Must run after the timer/APIC are up (so the monotonic clock is live) and
/// before the handle table is initialized (so it can seed from the CSPRNG). Keys
/// the CSPRNG **unconditionally** from whatever entropy was gathered, so kernel
/// draws work even on a CPU without a hardware RNG (where `seeded` stays false
/// until interrupt jitter accumulates).
pub fn init() {
    use crate::arch::entropy::{ArchEntropy, HwRngSource};
    use crate::arch::timer::ArchTimer;

    let source = crate::arch::Entropy::source();
    let mut hw = 0usize;
    let mut g = ENTROPY.lock();
    // Hardware burst: each successful draw is absorbed and credited its full width.
    for _ in 0..HW_DRAWS {
        if let Some(v) = crate::arch::Entropy::try_seed_u64() {
            g.absorb(v);
            g.credit(EST_PER_HW);
            hw += 1;
        }
    }
    // Mix in early jitter (no entropy credit — boot-time clock reads are highly
    // correlated; this is defence-in-depth mixing, the hardware burst is the seed).
    for _ in 0..16 {
        g.absorb(crate::arch::Timer::read_ns());
    }
    g.key_from_pool();
    g.maybe_latch();
    let seeded = g.seeded;
    drop(g);

    let src = match source {
        HwRngSource::Seed => "RDSEED",
        HwRngSource::Drbg => "RDRAND",
        HwRngSource::None => "none (jitter-only)",
    };
    crate::kprintln!(
        "entropy: source {src}, {hw} hw draws, seeded={seeded}",
    );
}

/// Absorb one interrupt-timing jitter sample (a raw cycle count) from IRQ context.
/// Cheap and lock-bounded; credits a fraction of a bit and may latch `seeded` once
/// enough jitter has accumulated (the no-hardware-RNG seeding path).
pub fn on_irq_sample(cycles: u64) {
    let mut g = ENTROPY.lock();
    g.absorb(cycles);
    g.credit_jitter();
    g.maybe_latch();
}

/// Fill `out` with CSPRNG output. Valid after [`init`]. Reseeds from the pool
/// automatically past [`RESEED_BYTES`] of output.
pub fn fill(out: &mut [u8]) {
    ENTROPY.lock().draw(out);
}

/// Draw a 64-bit CSPRNG value (e.g. to seed the handle-table free-list shuffle).
pub fn seed_u64() -> u64 {
    let mut buf = [0u8; 8];
    fill(&mut buf);
    u64::from_le_bytes(buf)
}

/// `true` once the pool has latched **seeded** (crossed the 256-bit estimate).
/// Gates the userspace read contract (Part D); kernel draws work once [`init`] has
/// keyed the CSPRNG, seeded or not.
pub fn is_seeded() -> bool {
    ENTROPY.lock().seeded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absorb_changes_pool_and_is_order_sensitive() {
        let mut a = EntropyState::new();
        let mut b = EntropyState::new();
        a.absorb(1);
        a.absorb(2);
        b.absorb(2);
        b.absorb(1);
        assert_ne!(a.pool, [0u8; POOL_LEN], "absorb must perturb the pool");
        assert_ne!(a.pool, b.pool, "absorb order must matter");
    }

    #[test]
    fn hardware_credit_crosses_gate_and_latches_once() {
        let mut s = EntropyState::new();
        // Four 64-bit hardware draws = 256 estimated bits → exactly the gate.
        for i in 0..4 {
            s.absorb(0x1111_1111 * (i + 1));
            s.credit(EST_PER_HW);
        }
        assert_eq!(s.est_bits, SEED_BITS);
        assert!(!s.seeded);
        s.maybe_latch();
        assert!(s.seeded, "latched once the estimate reached the gate");
        // Idempotent: a second latch attempt is a no-op (one-shot).
        s.maybe_latch();
        assert!(s.seeded);
    }

    #[test]
    fn jitter_alone_can_seed() {
        let mut s = EntropyState::new();
        // Each jitter sample credits 1 bit per JITTER_PER_BIT; need 256 bits.
        for i in 0..(SEED_BITS * JITTER_PER_BIT) {
            s.absorb(i as u64);
            s.credit_jitter();
        }
        assert_eq!(s.est_bits, SEED_BITS);
        s.maybe_latch();
        assert!(s.seeded, "jitter-only seeding reaches the gate");
    }

    #[test]
    fn draw_is_deterministic_for_a_fixed_pool() {
        let mut a = EntropyState::new();
        let mut b = EntropyState::new();
        for x in [0xDEADu64, 0xBEEF, 0xCAFE, 0xF00D] {
            a.absorb(x);
            b.absorb(x);
        }
        a.key_from_pool();
        b.key_from_pool();
        let mut xa = [0u8; 64];
        let mut xb = [0u8; 64];
        a.draw(&mut xa);
        b.draw(&mut xb);
        assert_eq!(xa, xb, "same pool → same keyed stream");
    }

    #[test]
    fn successive_draws_differ() {
        let mut s = EntropyState::new();
        s.absorb(0x1234_5678);
        s.key_from_pool();
        let mut first = [0u8; 8];
        let mut second = [0u8; 8];
        s.draw(&mut first);
        s.draw(&mut second);
        assert_ne!(first, second, "fast key erasure advances the stream");
    }

    #[test]
    fn reseed_fires_past_the_byte_threshold() {
        let mut s = EntropyState::new();
        s.absorb(0x9999);
        s.key_from_pool();
        // Draw just over RESEED_BYTES so the threshold trips and the counter resets.
        let mut chunk = [0u8; 4096];
        let mut total = 0u32;
        while total < RESEED_BYTES + 4096 {
            s.draw(&mut chunk);
            total += chunk.len() as u32;
        }
        assert!(
            s.bytes_since_reseed < RESEED_BYTES,
            "the byte-threshold reseed reset the counter",
        );
    }
}
