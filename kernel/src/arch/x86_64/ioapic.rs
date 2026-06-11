//! x86_64 I/O APIC bring-up + interrupt routing ([`ArchIrqRouter`] impl).
//!
//! The IOAPIC is the **system interrupt router**: it maps an external interrupt
//! line (a Global System Interrupt) to a redirection-table entry that delivers
//! a chosen vector to a chosen local APIC. We find it from the MADT (cached by
//! the ACPI parser, `super::acpi::ioapics()`), map its MMIO page uncached, mask
//! every entry, and disable the legacy 8259 PICs so external interrupts flow
//! only through the IOAPIC.
//!
//! ## Arch boundary
//!
//! Distinct from `super::apic` (the per-CPU local controller, `ArchIrq`). The
//! ISA-IRQ→GSI resolution (from the MADT source overrides) and the device-vector
//! handler registry (`super::idt::register_device_handler`) are arch-internal;
//! the neutral [`ArchIrqRouter`] trait takes an already-resolved line + vector.
//! IOAPIC / GSI / RTE / 8259 jargon never crosses into a neutral name.
//!
//! The byte-level encoders [`encode_rte`] and [`resolve_isa_irq`] are pure and
//! host-tested; the MMIO/PIT/IDT paths are exercised on-target via [`X86IoApic::self_test`].

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::arch::Cpu;
use crate::arch::Paging;
use crate::arch::cpu::ArchCpu;
use crate::arch::irq::ArchIrq;
use crate::arch::irq_router::{ArchIrqRouter, Polarity, TriggerMode};
use crate::arch::paging::{ArchPaging, PageFlags};
use crate::arch::timer::ArchTimer;
use crate::arch::x86_64::{acpi, idt, regs};
use crate::libkern::AllocError;
use crate::mm::kvmap;

// --- IOAPIC MMIO indirect registers ----------------------------------------
/// I/O register select — write the index of the register to access.
const IOREGSEL: u64 = 0x00;
/// I/O window — read/write the register selected by `IOREGSEL`.
const IOWIN: u64 = 0x10;
/// Version register: bits 16:23 hold `max redirection entry` (count − 1).
const REG_VER: u32 = 0x01;
/// First redirection-table register; entry `n` occupies `0x10 + 2n` (low dword)
/// and `0x11 + 2n` (high dword).
const REG_REDTBL_BASE: u32 = 0x10;

// --- Legacy 8259 PIC data ports (masked off when using the IOAPIC) ---------
const PIC1_DATA: u16 = 0x21;
const PIC2_DATA: u16 = 0xA1;

// --- PIT (for the bring-up self-test) --------------------------------------
const PIT_CMD: u16 = 0x43;
const PIT_CH0_DATA: u16 = 0x40;
const PIT_INPUT_HZ: u32 = 1_193_182;
/// Channel 0, lobyte/hibyte access, mode 2 (rate generator), binary.
const PIT_CH0_MODE2: u8 = 0b00_11_010_0;

/// Kernel-virtual base of the mapped IOAPIC MMIO page (`0` = not initialised).
static IOAPIC_BASE: AtomicU64 = AtomicU64::new(0);
/// The IOAPIC's GSI base (the first GSI its entries cover).
static IOAPIC_GSI_BASE: AtomicU32 = AtomicU32::new(0);
/// Number of redirection-table entries (GSIs this IOAPIC covers).
static IOAPIC_MAX_ENTRIES: AtomicU32 = AtomicU32::new(0);
/// PIT interrupt counter, bumped by the self-test handler.
static PIT_TICKS: AtomicU32 = AtomicU32::new(0);

/// Read an IOAPIC register by index (IOREGSEL → IOWIN).
///
/// # Safety
/// [`X86IoApic::init`] must have mapped the MMIO page; `idx` a valid register.
unsafe fn read_reg(idx: u32) -> u32 {
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "IOAPIC accessed before init");
    // SAFETY: `base` is the mapped uncached IOAPIC page; select then read the
    // 32-bit window — the defined indirect-access protocol.
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, idx);
        core::ptr::read_volatile((base + IOWIN) as *const u32)
    }
}

/// Write an IOAPIC register by index. # Safety: as [`read_reg`].
unsafe fn write_reg(idx: u32, val: u32) {
    let base = IOAPIC_BASE.load(Ordering::Relaxed);
    debug_assert!(base != 0, "IOAPIC accessed before init");
    // SAFETY: as `read_reg`, for a 32-bit indirect write.
    unsafe {
        core::ptr::write_volatile((base + IOREGSEL) as *mut u32, idx);
        core::ptr::write_volatile((base + IOWIN) as *mut u32, val);
    }
}

/// Encode a redirection-table entry into its `(low, high)` 32-bit dwords:
/// fixed delivery, physical destination mode, with the given vector, polarity,
/// trigger, mask bit, and destination local-APIC id (bits 56:63).
fn encode_rte(
    vector: u8,
    dest: u8,
    polarity: Polarity,
    trigger: TriggerMode,
    masked: bool,
) -> (u32, u32) {
    // low dword: vector[7:0], delivery mode[10:8]=000 (fixed), dest mode[11]=0
    // (physical), polarity[13], trigger[15], mask[16].
    let mut low = vector as u32;
    if matches!(polarity, Polarity::ActiveLow) {
        low |= 1 << 13;
    }
    if matches!(trigger, TriggerMode::Level) {
        low |= 1 << 15;
    }
    if masked {
        low |= 1 << 16;
    }
    // high dword: destination local-APIC id in bits 56:63 of the 64-bit RTE,
    // i.e. bits 24:31 of the high dword.
    let high = (dest as u32) << 24;
    (low, high)
}

/// Resolve a legacy ISA IRQ line to its `(gsi, polarity, trigger)`, applying the
/// MADT interrupt source overrides. Without an override the mapping is identity
/// (`IRQ n → GSI n`) with the ISA defaults (edge-triggered, active-high).
fn resolve_isa_irq(irq: u8, overrides: &[acpi::SourceOverride]) -> (u32, Polarity, TriggerMode) {
    for ov in overrides {
        if ov.source_irq == irq {
            // MPS INTI flags: polarity in bits 1:0, trigger in bits 3:2;
            // `00` means "conforms to the bus default" — ISA = high / edge.
            let polarity = if ov.flags & 0b11 == 0b11 {
                Polarity::ActiveLow
            } else {
                Polarity::ActiveHigh
            };
            let trigger = if (ov.flags >> 2) & 0b11 == 0b11 {
                TriggerMode::Level
            } else {
                TriggerMode::Edge
            };
            return (ov.gsi, polarity, trigger);
        }
    }
    (irq as u32, Polarity::ActiveHigh, TriggerMode::Edge)
}

/// The redirection-entry index for a GSI, if this IOAPIC covers it.
fn rte_index(gsi: u32) -> Option<u32> {
    let base = IOAPIC_GSI_BASE.load(Ordering::Relaxed);
    let max = IOAPIC_MAX_ENTRIES.load(Ordering::Relaxed);
    if gsi >= base && gsi < base + max {
        Some(gsi - base)
    } else {
        None
    }
}

/// Set or clear an entry's mask bit in place. # Safety: ring-0, post-`init`.
unsafe fn set_rte_mask(gsi: u32, masked: bool) {
    if let Some(idx) = rte_index(gsi) {
        // SAFETY: valid index, MMIO mapped.
        let mut low = unsafe { read_reg(REG_REDTBL_BASE + 2 * idx) };
        if masked {
            low |= 1 << 16;
        } else {
            low &= !(1 << 16);
        }
        // SAFETY: as above.
        unsafe { write_reg(REG_REDTBL_BASE + 2 * idx, low) };
    }
}

/// The self-test interrupt handler: count PIT ticks. (EOI is the dispatcher's.)
extern "C" fn pit_tick() {
    PIT_TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Program PIT channel 0 as a ~`PIT_INPUT_HZ / count` Hz rate generator (mode 2)
/// so it raises IRQ0 periodically. # Safety: ring-0; owns the PIT ports.
unsafe fn pit_ch0_periodic(count: u16) {
    // SAFETY: writing the PIT command + channel-0 data ports per the 8254 spec.
    unsafe {
        regs::outb(PIT_CMD, PIT_CH0_MODE2);
        regs::outb(PIT_CH0_DATA, (count & 0xFF) as u8);
        regs::outb(PIT_CH0_DATA, (count >> 8) as u8);
    }
}

/// The x86_64 [`ArchIrqRouter`] implementation (IOAPIC). Zero-sized; re-exported
/// as `crate::arch::IrqRouter`.
pub struct X86IoApic;

impl ArchIrqRouter for X86IoApic {
    unsafe fn init() -> Result<(), AllocError> {
        let io = match acpi::ioapics().first() {
            Some(io) => *io,
            None => {
                crate::kprintln!("ioapic: none in MADT — external device IRQs unavailable");
                return Ok(());
            }
        };

        // Map the IOAPIC register page uncached into the shared kernel vmap
        // (mirrors the local-APIC mapping in `apic.rs`).
        let va = kvmap::vmap_alloc_pages(1)?;
        let flags = PageFlags::WRITABLE | PageFlags::NO_CACHE;
        // SAFETY: `va` is a fresh kernel-vmap page; `io.addr` is the IOAPIC MMIO
        // frame from the MADT; mapping into the boot root is visible from every
        // address space (shared kernel-half PDPTs).
        unsafe { Paging::map_page(Paging::active_root(), va, io.addr, flags) }
            .map_err(|_| AllocError)?;
        IOAPIC_BASE.store(va.as_u64(), Ordering::Relaxed);
        IOAPIC_GSI_BASE.store(io.gsi_base, Ordering::Relaxed);

        // SAFETY: MMIO now mapped; the version register reports the entry count.
        let ver = unsafe { read_reg(REG_VER) };
        let max_entries = ((ver >> 16) & 0xFF) + 1;
        IOAPIC_MAX_ENTRIES.store(max_entries, Ordering::Relaxed);

        // Mask every redirection entry (bring-up: nothing is routed yet).
        let (masked_low, masked_high) =
            encode_rte(0, 0, Polarity::ActiveHigh, TriggerMode::Edge, true);
        for idx in 0..max_entries {
            // SAFETY: idx < max_entries; MMIO mapped.
            unsafe {
                write_reg(REG_REDTBL_BASE + 2 * idx + 1, masked_high);
                write_reg(REG_REDTBL_BASE + 2 * idx, masked_low);
            }
        }

        // Mask the legacy 8259 PICs so external IRQs flow only via the IOAPIC.
        // SAFETY: ring-0 writes to the PIC data ports; `0xFF` masks all lines.
        unsafe {
            regs::outb(PIC1_DATA, 0xFF);
            regs::outb(PIC2_DATA, 0xFF);
        }

        crate::kprintln!("ioapic: up ({} entries), 8259 masked", max_entries);
        Ok(())
    }

    unsafe fn route(irq: u32, vector: u8, dest: u32, trigger: TriggerMode, polarity: Polarity) {
        if let Some(idx) = rte_index(irq) {
            let (low, high) = encode_rte(vector, dest as u8, polarity, trigger, false);
            // SAFETY: valid index, MMIO mapped. Write the high (destination)
            // dword first, then the low dword — which clears the mask bit last.
            unsafe {
                write_reg(REG_REDTBL_BASE + 2 * idx + 1, high);
                write_reg(REG_REDTBL_BASE + 2 * idx, low);
            }
        }
    }

    unsafe fn mask(irq: u32) {
        // SAFETY: ring-0, post-init.
        unsafe { set_rte_mask(irq, true) };
    }

    unsafe fn unmask(irq: u32) {
        // SAFETY: ring-0, post-init.
        unsafe { set_rte_mask(irq, false) };
    }

    unsafe fn self_test() {
        if IOAPIC_BASE.load(Ordering::Relaxed) == 0 {
            return; // no IOAPIC to test
        }
        let bsp = crate::arch::Irq::id();
        let (gsi, polarity, trigger) = resolve_isa_irq(0, acpi::source_overrides());
        let vector = idt::register_device_handler(pit_tick);
        PIT_TICKS.store(0, Ordering::Relaxed);

        // Route the PIT's GSI to our vector on the boot CPU, and start the PIT.
        // SAFETY: ring-0, post-init; the handler for `vector` is registered.
        unsafe { Self::route(gsi, vector, bsp, trigger, polarity) };
        // SAFETY: ring-0; we own the PIT.
        unsafe { pit_ch0_periodic((PIT_INPUT_HZ / 100) as u16) };

        // Brief interrupt-enabled window. Only the PIT GSI is unmasked (the LAPIC
        // timer LVT is still masked, the 8259 is masked) and the scheduler is not
        // yet running, so nothing else can fire. Wait for a few ticks or ~100 ms.
        let start = crate::arch::Timer::read_ns();
        // SAFETY: ring-0; IF was 0 here, we restore it to 0 below.
        unsafe { Cpu::interrupts_enable() };
        while PIT_TICKS.load(Ordering::Acquire) < 3 {
            if crate::arch::Timer::read_ns().wrapping_sub(start) > 100_000_000 {
                break;
            }
            core::hint::spin_loop();
        }
        // SAFETY: ring-0; restore interrupts-masked.
        let _ = unsafe { Cpu::interrupts_disable() };

        // SAFETY: ring-0; mask the line again (the PIT free-runs, ignored).
        unsafe { Self::mask(gsi) };
        crate::kprintln!(
            "ioapic: routed PIT IRQ0\u{2192}GSI{}\u{2192}vec{:#x}; took {} interrupts",
            gsi,
            vector,
            PIT_TICKS.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::x86_64::acpi::SourceOverride;

    #[test]
    fn encode_rte_bit_layout() {
        // Vector 0x30 to APIC id 0, active-high edge, unmasked.
        let (low, high) = encode_rte(0x30, 0, Polarity::ActiveHigh, TriggerMode::Edge, false);
        assert_eq!(low & 0xFF, 0x30, "vector in bits 7:0");
        assert_eq!(low & (1 << 13), 0, "active-high => polarity bit clear");
        assert_eq!(low & (1 << 15), 0, "edge => trigger bit clear");
        assert_eq!(low & (1 << 16), 0, "unmasked => mask bit clear");
        assert_eq!(high, 0, "dest 0");

        // Active-low, level-triggered, masked, dest APIC id 3.
        let (low, high) = encode_rte(0x41, 3, Polarity::ActiveLow, TriggerMode::Level, true);
        assert_eq!(low & 0xFF, 0x41);
        assert_ne!(low & (1 << 13), 0, "active-low => polarity bit set");
        assert_ne!(low & (1 << 15), 0, "level => trigger bit set");
        assert_ne!(low & (1 << 16), 0, "masked => mask bit set");
        assert_eq!(high >> 24, 3, "dest in bits 56:63 (high dword 31:24)");
    }

    #[test]
    fn resolve_isa_irq_identity_without_override() {
        let (gsi, pol, trig) = resolve_isa_irq(4, &[]);
        assert_eq!(gsi, 4, "identity GSI");
        assert_eq!(pol, Polarity::ActiveHigh);
        assert_eq!(trig, TriggerMode::Edge);
    }

    #[test]
    fn resolve_isa_irq_applies_override() {
        // The canonical PIT remap: ISA IRQ0 -> GSI2, flags 0 (bus default).
        let overrides = [SourceOverride { source_irq: 0, gsi: 2, flags: 0 }];
        let (gsi, pol, trig) = resolve_isa_irq(0, &overrides);
        assert_eq!(gsi, 2);
        assert_eq!(pol, Polarity::ActiveHigh, "flags 00 => ISA default high");
        assert_eq!(trig, TriggerMode::Edge, "flags 00 => ISA default edge");

        // An override that specifies active-low, level-triggered.
        let overrides = [SourceOverride { source_irq: 9, gsi: 9, flags: 0b1111 }];
        let (gsi, pol, trig) = resolve_isa_irq(9, &overrides);
        assert_eq!(gsi, 9);
        assert_eq!(pol, Polarity::ActiveLow);
        assert_eq!(trig, TriggerMode::Level);
    }
}
