//! The kernel entropy subsystem: a global CSPRNG seeded from hardware (`RDSEED`/`RDRAND`) and
//! software (interrupt-timing jitter) sources.
//!
//! Design: `docs/architecture/entropy.md`. A fixed-size **pool** accumulates raw samples; a
//! conservative **entropy estimate** gates a one-shot **seeded** latch at 256 bits; the
//! [`ChaCha20Rng`] CSPRNG is keyed from the pool and reseeded as fresh entropy arrives. Every
//! source, the hardware RNG included, is *absorbed into the pool, never used as output directly*.
//!
//! [`init`] runs once at boot (after the timer/APIC, before the handle table): it draws a hardware
//! burst, mixes early jitter, **always keys the CSPRNG** from what it gathered (so kernel draws
//! work with no hardware RNG), and latches `seeded` if it crossed the gate. Then [`on_irq_sample`]
//! folds in jitter from IRQ context, and [`fill`] / [`seed_u64`] draw output.
//!
//! All state is behind one [`IrqSpinLock`], a **leaf**: held alone, briefly, with interrupts
//! masked, no nested locks, no allocation. See `kernel/docs/lock-ordering.md`.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::lockrank::LockRank;
use crate::libkern::{ChaCha20Rng, IrqSpinLock, KVec};
use crate::object::ObjectRef;

/// Pool size in bytes: one ChaCha key.
const POOL_LEN: usize = 32;

/// Estimated entropy bits required before the pool latches **seeded**.
const SEED_BITS: u32 = 256;

/// Entropy credited per hardware `u64` draw: its full width. The real guarantee is "drew from a
/// hardware RNG", not this estimate.
const EST_PER_HW: u32 = 64;

/// Jitter samples per estimated bit: interrupt timing carries a fraction of a bit each.
const JITTER_PER_BIT: u32 = 8;

/// Hardware `u64` draws attempted at boot: 8 x 64 bits, well past the gate.
const HW_DRAWS: usize = 8;

/// Bytes of CSPRNG output between automatic reseeds; bounds how much output rides one key.
const RESEED_BYTES: u32 = 1 << 20; // 1 MiB

/// Diffusion multiplier for the pool absorb (the SplitMix64 constant); mixing only.
const ABSORB_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// Maximum `sys_entropy_read` callers blocked at once on the pool seeding. Only reachable on a CPU
/// with no hardware RNG (otherwise it seeds at boot), so a small cap suffices; an overflowing
/// reader is told to retry rather than queued.
pub const SEED_WAITERS_MAX: usize = 4;

/// Outcome of registering a `PendingOperation` to be woken when the pool seeds.
pub enum SeedWaitReg {
    /// Queued; the timer-tick drain will complete it once the pool seeds.
    Queued,
    /// The pool was already seeded (a race after the caller's `is_seeded` check) —
    /// the caller should complete the returned PO now so its wait returns at once.
    AlreadySeeded(ObjectRef),
    /// The waiter list was full; the caller should complete the returned PO with a
    /// retry status. Pathological (> `SEED_WAITERS_MAX` concurrent unseeded reads).
    Full(ObjectRef),
}

/// Set while the seed-waiter list is non-empty, so the timer tick can skip the lock otherwise.
static SEED_WAKE_PENDING: AtomicBool = AtomicBool::new(false);

/// The entropy subsystem's interior state (see the module docs).
struct EntropyState {
    /// Entropy accumulator. Conditioned into the CSPRNG key by the key schedule.
    pool: [u8; POOL_LEN],
    absorb_ctr: u32,
    jitter_ctr: u32,
    est_bits: u32,
    /// `true` once [`init`] has keyed the CSPRNG (kernel draws are valid).
    keyed: bool,
    /// One-shot: set when `est_bits` first reaches [`SEED_BITS`]. Never cleared.
    seeded: bool,
    csprng: ChaCha20Rng,
    bytes_since_reseed: u32,
    /// `PendingOperation`s parked on the pool seeding. Pre-reserved in [`init`]; the subsystem owns
    /// these refs until the timer-tick drain moves them out to be signalled and dropped.
    seed_waiters: KVec<ObjectRef>,
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
            seed_waiters: KVec::new(),
        }
    }

    /// Fold `x` into 8 bytes of the pool at a rotating offset, with SplitMix-style diffusion so
    /// samples accumulate rather than overwrite. Mixing only: credits no entropy.
    fn absorb(&mut self, x: u64) {
        let off = (self.absorb_ctr as usize % (POOL_LEN / 8)) * 8;
        let cur = u64::from_le_bytes(self.pool[off..off + 8].try_into().unwrap());
        let mixed = (cur ^ x).wrapping_mul(ABSORB_MUL).rotate_left(31);
        self.pool[off..off + 8].copy_from_slice(&mixed.to_le_bytes());
        self.absorb_ctr = self.absorb_ctr.wrapping_add(1);
    }

    fn credit(&mut self, bits: u32) {
        self.est_bits = self.est_bits.saturating_add(bits).min(SEED_BITS);
    }

    fn credit_jitter(&mut self) {
        self.jitter_ctr = self.jitter_ctr.wrapping_add(1);
        if self.jitter_ctr.is_multiple_of(JITTER_PER_BIT) {
            self.credit(1);
        }
    }

    fn key_from_pool(&mut self) {
        self.csprng.reseed(&self.pool);
        self.keyed = true;
    }

    /// Key the CSPRNG from the pool at the crossing point and latch `seeded` (one-shot).
    fn maybe_latch(&mut self) {
        if !self.seeded && self.est_bits >= SEED_BITS {
            self.csprng.reseed(&self.pool);
            self.seeded = true;
        }
    }

    /// Draw `out.len()` CSPRNG bytes, folding fresh pool entropy into the key past
    /// [`RESEED_BYTES`].
    fn draw(&mut self, out: &mut [u8]) {
        debug_assert!(self.keyed, "entropy::draw before init keyed the CSPRNG");
        self.csprng.fill(out);
        self.bytes_since_reseed = self.bytes_since_reseed.saturating_add(out.len() as u32);
        if self.bytes_since_reseed >= RESEED_BYTES {
            self.csprng.reseed(&self.pool);
            self.bytes_since_reseed = 0;
        }
    }

    /// Queue `po` to be woken on seeding, or hand it back (see [`SeedWaitReg`]); refs are moved,
    /// never dropped, here.
    ///
    /// **The push refuses rather than growing.** [`init`]'s reserve is best-effort: if it fails,
    /// `len() >= SEED_WAITERS_MAX` is `0 >= 4`, false, so control reaches the push on an unreserved
    /// list, and `try_push` would `kmalloc` under this leaf-ranked lock. `push_within_capacity`
    /// cannot allocate, so an unreserved list reports itself full, which the caller handles.
    fn register_waiter(&mut self, po: ObjectRef) -> SeedWaitReg {
        if self.seeded {
            return SeedWaitReg::AlreadySeeded(po);
        }
        if self.seed_waiters.len() >= SEED_WAITERS_MAX {
            return SeedWaitReg::Full(po);
        }
        match self.seed_waiters.push_within_capacity(po) {
            Ok(()) => SeedWaitReg::Queued,
            // The reserve did not happen. Hand the ref back rather than dropping it here:
            // an `ObjectRef` drop under this lock is the same inversion one step further on.
            Err(returned) => SeedWaitReg::Full(returned),
        }
    }

    /// If seeded, move up to `out.len()` waiter refs into `out`; 0 if not. No `ObjectRef` drop
    /// here.
    fn drain_waiters(&mut self, out: &mut [Option<ObjectRef>]) -> usize {
        if !self.seeded {
            return 0;
        }
        let mut n = 0;
        while n < out.len() && !self.seed_waiters.is_empty() {
            out[n] = Some(self.seed_waiters.remove(0));
            n += 1;
        }
        n
    }
}

/// The global entropy state, a leaf `IrqSpinLock`.
static ENTROPY: IrqSpinLock<EntropyState> = IrqSpinLock::new(LockRank::Leaf, EntropyState::new());

/// Key the CSPRNG for a host test, which [`init`] cannot do: it reads hardware and logs to the
/// serial port. A fixed pool: a host test's draws need not be secret.
#[cfg(test)]
pub fn key_for_tests() {
    let mut g = ENTROPY.lock();
    g.absorb(0x6e69_7472_6f78);
    g.key_from_pool();
}

/// Initialize the entropy subsystem once at boot: draw a hardware burst, mix in early jitter, key
/// the CSPRNG, and latch `seeded` if the gate was crossed.
///
/// After the timer/APIC are up (the monotonic clock is live) and before the handle table (it seeds
/// from the CSPRNG). Keys the CSPRNG **unconditionally**, so kernel draws work without a hardware
/// RNG, where `seeded` stays false until interrupt jitter accumulates.
pub fn init() {
    use crate::arch::entropy::{ArchEntropy, HwRngSource};
    use crate::arch::timer::ArchTimer;

    let source = crate::arch::Entropy::source();
    let mut hw = 0usize;
    // Pre-reserve the seed-waiter list so `register_seed_waiter` never allocates under the lock,
    // and reserve **outside** it: allocating (slab, rank 6a) under this leaf-ranked lock is an
    // inversion the debug lock-order tracker rejects (as in `dpc::init`). Best-effort: an
    // unreserved list reports itself full, so callers are told to retry, because the push in
    // `register_waiter` refuses instead of growing.
    let mut waiters = KVec::new();
    let _ = waiters.try_reserve(SEED_WAITERS_MAX);
    let mut g = ENTROPY.lock();
    g.seed_waiters = waiters;
    for _ in 0..HW_DRAWS {
        if let Some(v) = crate::arch::Entropy::try_seed_u64() {
            g.absorb(v);
            g.credit(EST_PER_HW);
            hw += 1;
        }
    }
    // Early jitter is mixed without credit: boot-time clock reads are highly correlated, so this is
    // defence in depth and the hardware burst is the seed.
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
    crate::kprintln!("entropy: source {src}, {hw} hw draws, seeded={seeded}",);
}

/// Absorb one interrupt-timing jitter sample (a raw cycle count) from IRQ context. Credits a
/// fraction of a bit and may latch `seeded` (the no-hardware-RNG seeding path).
pub fn on_irq_sample(cycles: u64) {
    let mut g = ENTROPY.lock();
    g.absorb(cycles);
    g.credit_jitter();
    g.maybe_latch();
}

/// Fill `out` with CSPRNG output; valid after [`init`]. Reseeds past [`RESEED_BYTES`] of output.
pub fn fill(out: &mut [u8]) {
    ENTROPY.lock().draw(out);
}

/// Draw a 64-bit CSPRNG value.
pub fn seed_u64() -> u64 {
    let mut buf = [0u8; 8];
    fill(&mut buf);
    u64::from_le_bytes(buf)
}

/// `true` once the pool has latched **seeded** (crossed the 256-bit estimate). Gates the userspace
/// read contract; kernel draws work once [`init`] has keyed the CSPRNG, seeded or not.
pub fn is_seeded() -> bool {
    ENTROPY.lock().seeded
}

/// Register `po` to be completed when the pool seeds (the unseeded `sys_entropy_read` path). On
/// [`SeedWaitReg::Queued`] the subsystem owns `po` until [`drain_seed_waiters`] signals it;
/// otherwise `po` is handed back to complete now. The re-check under the lock closes the race
/// between the caller's `is_seeded` check and registering.
pub fn register_seed_waiter(po: ObjectRef) -> SeedWaitReg {
    let reg = ENTROPY.lock().register_waiter(po);
    if matches!(reg, SeedWaitReg::Queued) {
        SEED_WAKE_PENDING.store(true, Ordering::Release);
    }
    reg
}

/// `true` iff seed-waiters are queued: the lock-free gate the timer tick checks.
pub fn seed_wake_pending() -> bool {
    SEED_WAKE_PENDING.load(Ordering::Acquire)
}

/// If the pool is seeded, move up to `out.len()` queued waiter refs into `out` (returning the
/// count) for the caller to signal and drop, and clear [`seed_wake_pending`] once the list empties;
/// 0, touching nothing, when unseeded. No `ObjectRef` drop happens under the lock.
pub fn drain_seed_waiters(out: &mut [Option<ObjectRef>]) -> usize {
    let mut g = ENTROPY.lock();
    let n = g.drain_waiters(out);
    if g.seed_waiters.is_empty() {
        SEED_WAKE_PENDING.store(false, Ordering::Release);
    }
    n
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
        for i in 0..4 {
            s.absorb(0x1111_1111 * (i + 1));
            s.credit(EST_PER_HW);
        }
        assert_eq!(s.est_bits, SEED_BITS);
        assert!(!s.seeded);
        s.maybe_latch();
        assert!(s.seeded, "latched once the estimate reached the gate");
        s.maybe_latch();
        assert!(s.seeded);
    }

    #[test]
    fn jitter_alone_can_seed() {
        let mut s = EntropyState::new();
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
        let mut chunk = [0u8; 4096];
        let mut total = 0u32;
        while total < RESEED_BYTES + 4096 {
            s.draw(&mut chunk);
            total += chunk.len() as u32;
        }
        assert!(s.bytes_since_reseed < RESEED_BYTES, "the byte-threshold reseed reset the counter",);
    }

    /// A live `PendingOperation`, adopted into an `ObjectRef`, as a seed-waiter.
    fn po() -> ObjectRef {
        use crate::libkern::KBox;
        use crate::object::PendingOperation;
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                crate::libkern::handle::KObjectType::PendingOperation,
            )
        }
    }

    fn reserved_state() -> EntropyState {
        crate::mm::test_support::init_global_heap();
        let mut s = EntropyState::new();
        s.seed_waiters.try_reserve(SEED_WAITERS_MAX).unwrap();
        s
    }

    /// An **unreserved** waiter list reports itself full rather than allocating: the case `init`'s
    /// best-effort `try_reserve` leaves on failure, which the length check cannot catch (`0 >= 4`
    /// is false), and where the push once grew under the lock.
    #[test]
    fn register_reports_full_when_the_reserve_never_happened() {
        crate::mm::test_support::init_global_heap();
        let mut s = EntropyState::new(); // deliberately *not* `reserved_state()`
        assert_eq!(s.seed_waiters.capacity(), 0);

        assert!(matches!(s.register_waiter(po()), SeedWaitReg::Full(_)));
        assert_eq!(s.seed_waiters.capacity(), 0, "refusing must not allocate");
        assert_eq!(s.seed_waiters.len(), 0);
    }

    #[test]
    fn register_returns_already_seeded_when_seeded() {
        let mut s = reserved_state();
        s.seeded = true;
        assert!(matches!(s.register_waiter(po()), SeedWaitReg::AlreadySeeded(_)));
        assert_eq!(s.seed_waiters.len(), 0, "no queuing when already seeded");
    }

    #[test]
    fn register_queues_when_unseeded_then_full_at_capacity() {
        let mut s = reserved_state();
        for _ in 0..SEED_WAITERS_MAX {
            assert!(matches!(s.register_waiter(po()), SeedWaitReg::Queued));
        }
        assert_eq!(s.seed_waiters.len(), SEED_WAITERS_MAX);
        assert!(matches!(s.register_waiter(po()), SeedWaitReg::Full(_)));
    }

    #[test]
    fn drain_yields_nothing_until_seeded_then_all_waiters() {
        let mut s = reserved_state();
        s.register_waiter(po());
        s.register_waiter(po());
        let mut out: [Option<ObjectRef>; SEED_WAITERS_MAX] = [const { None }; SEED_WAITERS_MAX];
        assert_eq!(s.drain_waiters(&mut out), 0, "no drain while unseeded");
        assert_eq!(s.seed_waiters.len(), 2);
        s.seeded = true;
        assert_eq!(s.drain_waiters(&mut out), 2, "drains all once seeded");
        assert_eq!(s.seed_waiters.len(), 0);
    }
}
