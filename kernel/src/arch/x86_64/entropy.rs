//! x86_64 hardware-entropy source: `RDSEED` preferred, `RDRAND` fallback.
//!
//! Implements [`ArchEntropy`](crate::arch::entropy::ArchEntropy) for x86_64. The
//! available source is detected once via CPUID and cached; each draw retries the
//! single-instruction primitive ([`regs::rdseed_u64`] / [`regs::rdrand_u64`]) a
//! bounded number of times before giving up (a hardware source can legitimately
//! have no value ready). See `docs/architecture/entropy.md` § Sources.

use core::sync::atomic::{AtomicU8, Ordering};

use super::regs;
use crate::arch::entropy::{ArchEntropy, HwRngSource};

/// Bits in the cached detection byte. `PROBED` distinguishes "not yet probed"
/// (`0`) from "probed, no source" (`PROBED` alone).
const PROBED: u8 = 1 << 0;
const HAS_RDRAND: u8 = 1 << 1;
const HAS_RDSEED: u8 = 1 << 2;

/// Cached CPUID detection result (`0` = unprobed). One-shot; idempotent.
static DETECT: AtomicU8 = AtomicU8::new(0);

/// Bounded per-instruction retry. A hardware source may transiently report "no
/// value ready" (CF=0); a handful of attempts covers normal contention without
/// risking an unbounded spin.
const RETRIES: usize = 10;

/// CPUID-detect the available sources, returning the cached byte. Probing is
/// idempotent, so a benign race between CPUs just recomputes the same value.
fn detect() -> u8 {
    let cached = DETECT.load(Ordering::Acquire);
    if cached & PROBED != 0 {
        return cached;
    }
    // CPUID.01H:ECX bit 30 = RDRAND; CPUID.07H:EBX bit 18 = RDSEED.
    let (_, _, ecx1, _) = regs::cpuid(1, 0);
    let (_, ebx7, _, _) = regs::cpuid(7, 0);
    let mut flags = PROBED;
    if ecx1 & (1 << 30) != 0 {
        flags |= HAS_RDRAND;
    }
    if ebx7 & (1 << 18) != 0 {
        flags |= HAS_RDSEED;
    }
    DETECT.store(flags, Ordering::Release);
    flags
}

/// The x86_64 hardware-entropy source.
pub struct X86Entropy;

impl ArchEntropy for X86Entropy {
    fn source() -> HwRngSource {
        let f = detect();
        if f & HAS_RDSEED != 0 {
            HwRngSource::Seed
        } else if f & HAS_RDRAND != 0 {
            HwRngSource::Drbg
        } else {
            HwRngSource::None
        }
    }

    fn try_seed_u64() -> Option<u64> {
        let f = detect();
        // Prefer the conditioned seed source; only execute an instruction the CPU
        // actually advertises (executing an unsupported one is `#UD`).
        if f & HAS_RDSEED != 0 {
            for _ in 0..RETRIES {
                if let Some(v) = regs::rdseed_u64() {
                    return Some(v);
                }
            }
        }
        if f & HAS_RDRAND != 0 {
            for _ in 0..RETRIES {
                if let Some(v) = regs::rdrand_u64() {
                    return Some(v);
                }
            }
        }
        None
    }
}
