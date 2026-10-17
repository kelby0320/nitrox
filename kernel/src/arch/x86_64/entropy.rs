//! x86_64 hardware-entropy source: `RDSEED` preferred, `RDRAND` fallback.
//!
//! The source is detected once via CPUID and cached; each draw retries the instruction a
//! bounded number of times. See `docs/architecture/entropy.md` § Sources.

use core::sync::atomic::{AtomicU8, Ordering};

use super::regs;
use crate::arch::entropy::{ArchEntropy, HwRngSource};

// `PROBED` tells "not yet probed" (`0`) from "probed, no source" (`PROBED` alone).
const PROBED: u8 = 1 << 0;
const HAS_RDRAND: u8 = 1 << 1;
const HAS_RDSEED: u8 = 1 << 2;

// Cached CPUID detection result (`0` = unprobed).
static DETECT: AtomicU8 = AtomicU8::new(0);

// A source may transiently report "no value ready" (CF=0); bounded so it never spins.
const RETRIES: usize = 10;

// Probing is idempotent, so a race between CPUs just recomputes the same value.
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
        // Only execute an instruction the CPU advertises: an unsupported one is `#UD`.
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
