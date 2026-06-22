//! Architecture-neutral hardware-entropy contract.
//!
//! [`ArchEntropy`] exposes the CPU's hardware random-number source as a single
//! fallible draw, used by the entropy subsystem to *seed* the software CSPRNG
//! (`docs/architecture/entropy.md` § Sources). On x86_64 this is `RDSEED`
//! (preferred) with an `RDRAND` fallback, both CPUID-detected (see
//! `arch/x86_64/entropy.rs`); a future aarch64 port would back it with `RNDR` /
//! the SMCCC TRNG.
//!
//! The draw is **fallible by design**: a hardware source can legitimately have no
//! value ready, and a CPU may have no source at all (older parts, some
//! hypervisors). Callers treat `None` as "no hardware sample this round" and lean
//! on the software jitter source — they never block on it.
//!
//! **Trust model.** Output here is raw hardware-RNG material. It is *absorbed into
//! the entropy pool, never used as CSPRNG output directly* (mix-don't-trust); see
//! the design doc. This module only exposes the raw draw.
//!
//! The active architecture's implementation is re-exported from `crate::arch` as
//! `Entropy` (see `kernel/src/arch/mod.rs`).

/// The best hardware random source the current CPU offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HwRngSource {
    /// No hardware RNG instruction is available; seed from software jitter alone.
    None,
    /// A hardware DRBG draw is available (x86_64 `RDRAND`).
    Drbg,
    /// A conditioned-entropy seed draw is available (x86_64 `RDSEED`) — preferred
    /// for seeding. Implies a DRBG draw is also available.
    Seed,
}

/// The CPU's hardware-entropy source.
pub trait ArchEntropy {
    /// The best hardware source available, CPUID-detected (cached after the first
    /// probe). [`HwRngSource::None`] when the CPU offers neither.
    fn source() -> HwRngSource;

    /// Attempt one 64-bit hardware draw, preferring the conditioned seed source
    /// and falling back to the DRBG. Retries a bounded number of times internally
    /// (a hardware source can transiently have no value ready). Returns `None`
    /// only when no source exists or the bounded retry was exhausted — the caller
    /// then relies on software entropy for this round, never blocking.
    fn try_seed_u64() -> Option<u64>;
}
