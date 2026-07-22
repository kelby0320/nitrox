//! x86_64 [`ArchTimer`] implementation: TSC monotonic time + local-APIC timer,
//! both calibrated against the legacy PIT (channel 2) at boot.
//!
//! The LAPIC timer runs in classic **count-down** mode (one-shot / periodic) —
//! **not** TSC-deadline mode — because the project's QEMU dev loop runs under
//! TCG, which does not emulate the TSC-deadline timer (the same reason
//! `apic.rs` uses xAPIC, not x2APIC; see the decision log). There is no HPET or
//! ACPI dependency: the PIT is found at fixed legacy ports and the LAPIC at the
//! MSR-reported MMIO base (see `docs/rationale/why-phased-acpi.md`).
//!
//! Calibration uses **PIT channel 2**, which is software-gated (port `0x61`
//! bit 0) and whose output is pollable (port `0x61` bit 5) — so it needs no
//! interrupt, which matters because it runs at boot before interrupts are
//! enabled (IF is still 0). We run the channel for a fixed ~10 ms window and
//! count how far the TSC and the LAPIC timer advance, yielding both frequencies.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::irq::TIMER_VECTOR;
use crate::arch::timer::ArchTimer;
use crate::arch::x86_64::apic;
use crate::arch::x86_64::regs;

// --- PIT (Intel 8254) channel 2 -------------------------------------------
/// Channel-2 data port (count read/write).
const PIT_CH2_DATA: u16 = 0x42;
/// Mode/command port.
const PIT_CMD: u16 = 0x43;
/// Channel-2 gate + speaker control / output-status port. Bit 0 = gate enable,
/// bit 1 = speaker enable, bit 5 = channel-2 OUT (read-only status).
const PIT_CH2_GATE: u16 = 0x61;
/// PIT input clock — 1.193182 MHz on every PC.
const PIT_INPUT_HZ: u64 = 1_193_182;
/// Command byte: channel 2 (bits 7:6 = 10), access lobyte/hibyte (bits 5:4 =
/// 11), mode 0 / interrupt-on-terminal-count (bits 3:1 = 000), binary (bit 0 =
/// 0) → `0b1011_0000`. In mode 0, OUT is low while counting and goes high at
/// terminal count.
const PIT_CH2_MODE0_LOHI: u8 = 0b1011_0000;
/// Calibration window, milliseconds.
const CAL_MS: u64 = 10;
/// Channel-2 reload count for a `CAL_MS` window (≈ 11932 for 10 ms).
const CAL_PIT_COUNT: u16 = ((PIT_INPUT_HZ * CAL_MS) / 1000) as u16;

// --- LAPIC timer ----------------------------------------------------------
/// Divide-by-16 (DCR encoding `0b0011`).
const LAPIC_TIMER_DIV16_DCR: u32 = 0b0011;
/// LVT Timer bit 16 — delivery mask.
const LVT_TIMER_MASKED: u32 = 1 << 16;
/// LVT Timer bits 18:17 = `01` — periodic mode.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;

// --- Invariant-TSC feature bit --------------------------------------------
/// Extended CPUID leaf carrying the invariant-TSC bit.
const CPUID_INVARIANT_TSC_LEAF: u32 = 0x8000_0007;
/// `CPUID.80000007H:EDX.8` — TSC ticks at a constant rate across P-/C-states.
const CPUID_INVARIANT_TSC_BIT: u32 = 1 << 8;

// --- Calibration results (set once by `init`) -----------------------------
/// Calibrated TSC frequency in Hz. `0` until [`X86Timer::init`] runs.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// Calibrated LAPIC-timer input frequency in Hz (at the chosen divisor).
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// TSC value captured at the end of `init`, the monotonic zero-point.
static TSC_BASE: AtomicU64 = AtomicU64::new(0);
/// Multiply-shift pair for the TSC→ns conversion: `ns = (delta * MULT) >> SHIFT`.
/// Precomputed in `init` so [`X86Timer::read_ns`] needs no per-call division.
static NS_MULT: AtomicU64 = AtomicU64::new(0);
static NS_SHIFT: AtomicU64 = AtomicU64::new(0);

/// The x86_64 [`ArchTimer`] implementation. Zero-sized; re-exported as
/// `crate::arch::Timer`.
pub struct X86Timer;

impl ArchTimer for X86Timer {
    unsafe fn init() {
        debug_assert!(
            apic::is_initialised(),
            "ArchTimer::init before Irq::init — LAPIC MMIO not mapped"
        );

        // Warn (do not fail) if the CPU does not advertise an invariant TSC: on
        // such hardware the monotonic clock could drift with P-states. QEMU/TCG
        // does provide a stable TSC, so this is informational in the dev loop.
        let (_, _, _, edx) = regs::cpuid(CPUID_INVARIANT_TSC_LEAF, 0);
        if edx & CPUID_INVARIANT_TSC_BIT == 0 {
            crate::kprintln!(
                "timer: WARNING — CPU reports no invariant TSC; monotonic clock may drift"
            );
        }

        // Free-run the LAPIC timer from max so its current-count decrements
        // measurably during the PIT window. Masked LVT: it raises nothing.
        // SAFETY: `apic::is_initialised()` (asserted above) means the LAPIC MMIO
        // page is mapped; writing the timer registers is the defined way to
        // configure the count-down timer, and a masked LVT delivers nothing.
        unsafe {
            apic::write_reg_shared(apic::REG_TIMER_DIV_CONFIG, LAPIC_TIMER_DIV16_DCR);
            apic::write_reg_shared(apic::REG_LVT_TIMER, LVT_TIMER_MASKED | TIMER_VECTOR as u32);
            apic::write_reg_shared(apic::REG_TIMER_INIT_COUNT, u32::MAX);
        }

        // Run PIT channel 2 for the window and bracket it with TSC + LAPIC
        // snapshots. The snapshots are taken right after the gate is raised and
        // right after terminal count, so the tiny setup/teardown windows add
        // only a constant sub-microsecond error over the 10 ms span.
        // SAFETY: single CPU during boot with IF=0; nothing else touches the PIT
        // or port 0x61, and the LAPIC page is mapped.
        let (tsc_start, lapic_start, tsc_end, lapic_end) = unsafe {
            pit_ch2_start(CAL_PIT_COUNT);
            let tsc_start = regs::rdtsc();
            let lapic_start = apic::read_reg_shared(apic::REG_TIMER_CUR_COUNT);
            pit_ch2_wait_until_done();
            let tsc_end = regs::rdtsc();
            let lapic_end = apic::read_reg_shared(apic::REG_TIMER_CUR_COUNT);
            (tsc_start, lapic_start, tsc_end, lapic_end)
        };

        // Elapsed real time is `CAL_PIT_COUNT / PIT_INPUT_HZ` seconds, so
        // `hz = ticks * PIT_INPUT_HZ / CAL_PIT_COUNT`. The LAPIC current-count
        // counts *down*, so its elapsed ticks are `start - end`.
        let count = CAL_PIT_COUNT as u64;
        let tsc_hz = tsc_end.wrapping_sub(tsc_start) * PIT_INPUT_HZ / count;
        let lapic_ticks = (lapic_start - lapic_end) as u64;
        let timer_hz = lapic_ticks * PIT_INPUT_HZ / count;

        let (mult, shift) = compute_ns_mul_shift(tsc_hz);
        TSC_HZ.store(tsc_hz, Ordering::Relaxed);
        TIMER_HZ.store(timer_hz, Ordering::Relaxed);
        NS_MULT.store(mult, Ordering::Relaxed);
        NS_SHIFT.store(shift, Ordering::Relaxed);

        // Disarm the calibration countdown and capture the monotonic zero-point.
        // SAFETY: writing 0 to the initial count halts the timer; LAPIC mapped.
        unsafe { apic::write_reg_shared(apic::REG_TIMER_INIT_COUNT, 0) };
        TSC_BASE.store(regs::rdtsc(), Ordering::Relaxed);
    }

    fn read_ns() -> u64 {
        let now = regs::rdtsc();
        let base = TSC_BASE.load(Ordering::Relaxed);
        let delta = now.wrapping_sub(base);
        let mult = NS_MULT.load(Ordering::Relaxed);
        let shift = NS_SHIFT.load(Ordering::Relaxed);
        // u128 intermediate: `delta` and `mult` are each ≤ u64::MAX, and
        // (2^64-1)^2 < 2^128-1, so the product never overflows.
        (((delta as u128) * (mult as u128)) >> shift) as u64
    }

    unsafe fn start_periodic(period_ns: u64) {
        let count = ns_to_timer_ticks(period_ns);
        // SAFETY: LAPIC MMIO mapped (post-init). Unmasked periodic LVT at the
        // timer vector — once IF=1 (preemptive scheduling) this fires the timer
        // IRQ every `period_ns`.
        unsafe {
            apic::write_reg_shared(apic::REG_TIMER_DIV_CONFIG, LAPIC_TIMER_DIV16_DCR);
            apic::write_reg_shared(apic::REG_LVT_TIMER, LVT_TIMER_PERIODIC | TIMER_VECTOR as u32);
            apic::write_reg_shared(apic::REG_TIMER_INIT_COUNT, count);
        }
    }

    unsafe fn arm_oneshot_in(delay_ns: u64) {
        let count = ns_to_timer_ticks(delay_ns);
        // SAFETY: as `start_periodic`; one-shot mode (LVT bits 18:17 = 00).
        unsafe {
            apic::write_reg_shared(apic::REG_TIMER_DIV_CONFIG, LAPIC_TIMER_DIV16_DCR);
            apic::write_reg_shared(apic::REG_LVT_TIMER, TIMER_VECTOR as u32);
            apic::write_reg_shared(apic::REG_TIMER_INIT_COUNT, count);
        }
    }

    unsafe fn stop() {
        // SAFETY: masking the LVT and zeroing the initial count halts both
        // delivery and the countdown; LAPIC mapped.
        unsafe {
            apic::write_reg_shared(apic::REG_LVT_TIMER, LVT_TIMER_MASKED | TIMER_VECTOR as u32);
            apic::write_reg_shared(apic::REG_TIMER_INIT_COUNT, 0);
        }
    }

    fn monotonic_hz() -> u64 {
        TSC_HZ.load(Ordering::Relaxed)
    }

    fn timer_hz() -> u64 {
        TIMER_HZ.load(Ordering::Relaxed)
    }
}

/// Program PIT channel 2 to count down from `count` in mode 0 and start it.
///
/// Channel 2 is software-gated via port `0x61` bit 0; its OUT (bit 5) reads low
/// while counting and high at terminal count. The speaker bit (bit 1) is forced
/// low so nothing is audible.
///
/// # Safety
/// Ring-0 port I/O; the caller must own the PIT and port `0x61` (single CPU,
/// boot).
unsafe fn pit_ch2_start(count: u16) {
    // SAFETY: legacy PIT/port-0x61 I/O, owned by the kernel during boot.
    unsafe {
        // Gate low + speaker off first, so reprogramming starts cleanly.
        let p61 = regs::inb(PIT_CH2_GATE) & 0xFC;
        regs::outb(PIT_CH2_GATE, p61);
        regs::outb(PIT_CMD, PIT_CH2_MODE0_LOHI);
        regs::outb(PIT_CH2_DATA, (count & 0xFF) as u8);
        regs::outb(PIT_CH2_DATA, (count >> 8) as u8);
        // Raise the gate (bit 0) to begin counting; speaker (bit 1) stays clear.
        regs::outb(PIT_CH2_GATE, p61 | 0x01);
    }
}

/// Busy-poll channel-2 OUT (port `0x61` bit 5) until terminal count.
///
/// # Safety
/// As [`pit_ch2_start`].
unsafe fn pit_ch2_wait_until_done() {
    loop {
        // SAFETY: reading the status port has no side effects.
        let out = unsafe { regs::inb(PIT_CH2_GATE) } & 0x20;
        if out != 0 {
            break;
        }
        core::hint::spin_loop();
    }
}

/// Convert a nanosecond interval to a LAPIC initial-count using the calibrated
/// timer frequency, saturating into the 32-bit count register.
fn ns_to_timer_ticks(ns: u64) -> u32 {
    ns_to_ticks_with_hz(ns, TIMER_HZ.load(Ordering::Relaxed))
}

/// Pure core of [`ns_to_timer_ticks`]: `ns * timer_hz / 1e9`, clamped to
/// `1..=u32::MAX` (a never-zero count so an arm always programs a real
/// countdown). Host-tested.
fn ns_to_ticks_with_hz(ns: u64, timer_hz: u64) -> u32 {
    let hz = timer_hz.max(1) as u128;
    let ticks = (ns as u128 * hz) / 1_000_000_000u128;
    ticks.clamp(1, u32::MAX as u128) as u32
}

/// Choose `(mult, shift)` so that `(delta * mult) >> shift ≈ delta * 1e9 /
/// tsc_hz`, with `mult ≤ u64::MAX` and `shift` as large as possible for
/// precision. Pure; host-tested against a u128 reference.
fn compute_ns_mul_shift(tsc_hz: u64) -> (u64, u64) {
    let hz = tsc_hz.max(1) as u128;
    let mut shift: u32 = 63;
    loop {
        // mult = round(1e9 * 2^shift / hz)
        let numer = 1_000_000_000u128 << shift;
        let mult = (numer + hz / 2) / hz;
        if mult <= u64::MAX as u128 {
            return (mult as u64, shift as u64);
        }
        if shift == 0 {
            // Unreachable for any real frequency (hz ≥ 1 ⇒ mult ≤ 1e9 here).
            return (1, 0);
        }
        shift -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference conversion using a full u128 division.
    fn ref_ns(delta: u64, tsc_hz: u64) -> u64 {
        ((delta as u128 * 1_000_000_000u128) / tsc_hz as u128) as u64
    }

    /// Apply the precomputed mul-shift the way `read_ns` does.
    fn apply(delta: u64, mult: u64, shift: u64) -> u64 {
        (((delta as u128) * (mult as u128)) >> shift) as u64
    }

    #[test]
    fn mul_shift_matches_reference_across_frequencies() {
        for &tsc_hz in &[1_000_000_000u64, 2_500_000_000, 3_500_000_000, 4_000_000_000] {
            let (mult, shift) = compute_ns_mul_shift(tsc_hz);
            assert!(mult as u128 <= u64::MAX as u128);
            for &delta in &[0u64, 1, 1_000, 3_000_000_000, 90_000_000_000_000_000] {
                let got = apply(delta, mult, shift);
                let want = ref_ns(delta, tsc_hz);
                // Allow ≤ 1 ns of rounding slack per second of elapsed time.
                let secs = want / 1_000_000_000 + 1;
                let tol = secs + 1; // ≤ 1 ns/s, plus a floor-boundary unit
                let diff = got.abs_diff(want);
                assert!(
                    diff <= tol,
                    "tsc_hz={tsc_hz} delta={delta}: got {got}, want {want}, diff {diff} > tol {tol}"
                );
            }
        }
    }

    #[test]
    fn mul_shift_no_overflow_at_max_delta() {
        // mult ≤ u64::MAX and delta ≤ u64::MAX must not overflow the u128 in
        // `read_ns`: (2^64-1)^2 < 2^128-1.
        let (mult, shift) = compute_ns_mul_shift(1_000_000_000);
        let _ = apply(u64::MAX, mult, shift); // must not panic
        assert!(mult as u128 <= u64::MAX as u128);
        assert!(shift <= 63);
    }

    #[test]
    fn ns_to_ticks_scales_and_clamps() {
        // 100 MHz timer: 1 ms → 100_000 ticks.
        assert_eq!(ns_to_ticks_with_hz(1_000_000, 100_000_000), 100_000);
        // Zero interval still programs a real (non-zero) countdown.
        assert_eq!(ns_to_ticks_with_hz(0, 100_000_000), 1);
        // A huge interval saturates to the 32-bit register width.
        assert_eq!(ns_to_ticks_with_hz(u64::MAX, 1_000_000_000), u32::MAX);
        // A zero/uncalibrated frequency does not divide-by-zero.
        assert_eq!(ns_to_ticks_with_hz(1_000_000, 0), 1);
    }

    #[test]
    fn cal_pit_count_is_about_10ms() {
        // 1_193_182 Hz * 10 ms ≈ 11932 counts.
        assert_eq!(CAL_PIT_COUNT, 11931);
    }
}

/// Raw CPU cycle counter, for self-test micro-measurements only.
///
/// Distinct from [`X86Timer::read_ns`]: no scaling, no monotonicity guarantee across
/// CPUs — just the bare `RDTSC`, so a self-test can price a code path in cycles. Only
/// meaningful under KVM or on real hardware; TCG's `RDTSC` counts emulator progress.
#[cfg(feature = "selftest")]
pub fn read_cycles() -> u64 {
    regs::rdtsc()
}
