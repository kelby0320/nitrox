//! x86_64 I/O APIC bring-up and interrupt routing ([`ArchIrqRouter`] impl).
//!
//! The IOAPIC maps an external interrupt line (a Global System Interrupt) to a
//! redirection-table entry delivering a vector to a local APIC. It is found from the MADT
//! (`super::acpi::ioapics()`), its MMIO page mapped uncached, every entry masked, and the
//! legacy 8259 PICs disabled. ISA-IRQ→GSI resolution (MADT source overrides) and the
//! device-vector registry (`super::idt::register_device_handler`) are arch-internal; the
//! neutral [`ArchIrqRouter`] takes a resolved line and vector. [`encode_rte`] and
//! [`resolve_isa_irq`] are host-tested; the rest runs on-target via [`X86IoApic::self_test`].

use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::Cpu;
use crate::arch::cpu::ArchCpu;
use crate::arch::irq::ArchIrq;
use crate::arch::irq_router::{ArchIrqRouter, Polarity, TriggerMode};
use crate::arch::timer::ArchTimer;
use crate::arch::x86_64::{acpi, idt, regs};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, IrqSpinLock};
use crate::mm::kvmap;

// --- IOAPIC MMIO indirect registers ---
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;
/// Version register: bits 23:16 hold the last redirection entry index (count − 1).
const REG_VER: u32 = 0x01;
/// First redirection-table register; entry `n` is `0x10 + 2n` (low) and `0x11 + 2n` (high).
const REG_REDTBL_BASE: u32 = 0x10;

// --- Legacy 8259 PIC data ports ---
const PIC1_DATA: u16 = 0x21;
const PIC2_DATA: u16 = 0xA1;

// --- PIT (for the self-test) ---
const PIT_CMD: u16 = 0x43;
const PIT_CH0_DATA: u16 = 0x40;
const PIT_INPUT_HZ: u32 = 1_193_182;
#[allow(clippy::unusual_byte_groupings, reason = "grouped by the register's fields")]
const PIT_CH0_MODE2: u8 = 0b00_11_010_0;

/// The IOAPIC once mapped; `None` before [`X86IoApic::init`], or on a machine without one.
static IOAPIC: IrqSpinLock<Option<IoApic>> = IrqSpinLock::new(LockRank::Leaf, None);
static PIT_TICKS: AtomicU32 = AtomicU32::new(0);
static PIT_DPC_RUNS: AtomicU32 = AtomicU32::new(0);
/// The self-test's DPC; a `static`, so queuing it allocates nothing.
static PIT_DPC: crate::dpc::Dpc = crate::dpc::Dpc::new(pit_dpc_handler, core::ptr::null_mut());

/// **The IOAPIC's registers**, reached (IOREGSEL, then IOWIN) only under [`IOAPIC`]'s guard.
struct IoApic {
    base: u64,
    gsi_base: u32,
    entries: u32,
}

impl IoApic {
    fn read(&mut self, idx: u32) -> u32 {
        // SAFETY: `base` is the uncached page `init` mapped for good, and only `init` makes one.
        unsafe {
            core::ptr::write_volatile((self.base + IOREGSEL) as *mut u32, idx);
            core::ptr::read_volatile((self.base + IOWIN) as *const u32)
        }
    }

    fn write(&mut self, idx: u32, val: u32) {
        // SAFETY: as `read`, for a 32-bit write.
        unsafe {
            core::ptr::write_volatile((self.base + IOREGSEL) as *mut u32, idx);
            core::ptr::write_volatile((self.base + IOWIN) as *mut u32, val);
        }
    }

    /// The redirection entry serving `gsi`, if this IOAPIC has one.
    fn entry(&self, gsi: u32) -> Option<u32> {
        gsi.checked_sub(self.gsi_base).filter(|&idx| idx < self.entries)
    }

    /// `gsi`'s entry, and `dest` in its eight bits: a wider id needs interrupt remapping, not enabled.
    fn names(&self, gsi: u32, dest: u32) -> Option<(u32, u8)> {
        Some((self.entry(gsi)?, u8::try_from(dest).ok()?))
    }

    /// Write entry `idx`, its destination first, so its mask bit takes effect last.
    fn write_entry(&mut self, idx: u32, (low, high): (u32, u32)) {
        self.write(REG_REDTBL_BASE + 2 * idx + 1, high);
        self.write(REG_REDTBL_BASE + 2 * idx, low);
    }

    fn set_mask(&mut self, gsi: u32, masked: bool) {
        if let Some(idx) = self.entry(gsi) {
            let low = self.read(REG_REDTBL_BASE + 2 * idx);
            let low = if masked { low | 1 << 16 } else { low & !(1 << 16) };
            self.write(REG_REDTBL_BASE + 2 * idx, low);
        }
    }
}

/// Encode a redirection-table entry into `(low, high)` dwords: fixed delivery, physical
/// destination mode, destination local-APIC id in bits 56:63.
fn encode_rte(vector: u8, dest: u8, polarity: Polarity, trigger: TriggerMode, masked: bool) -> (u32, u32) {
    // low: vector[7:0], delivery[10:8]=000 (fixed), dest mode[11]=0 (physical), polarity[13],
    // trigger[15], mask[16].
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
    // high: destination id in bits 56:63 of the 64-bit entry, i.e. 31:24 of the high dword.
    let high = (dest as u32) << 24;
    (low, high)
}

/// Resolve a legacy ISA IRQ to `(gsi, polarity, trigger)` via the MADT interrupt source
/// overrides; without one it is identity (`IRQ n → GSI n`), edge-triggered, active-high.
fn resolve_isa_irq(irq: u8, overrides: &[acpi::SourceOverride]) -> (u32, Polarity, TriggerMode) {
    for ov in overrides {
        if ov.source_irq == irq {
            // MPS INTI flags: polarity in bits 1:0, trigger in 3:2; `00` is the bus default
            // (ISA: high / edge).
            let polarity = if ov.flags & 0b11 == 0b11 { Polarity::ActiveLow } else { Polarity::ActiveHigh };
            let trigger = if (ov.flags >> 2) & 0b11 == 0b11 { TriggerMode::Level } else { TriggerMode::Edge };
            return (ov.gsi, polarity, trigger);
        }
    }
    (irq as u32, Polarity::ActiveHigh, TriggerMode::Edge)
}

/// The self-test interrupt handler: count PIT ticks (EOI is the dispatcher's).
extern "C" fn pit_tick() {
    PIT_TICKS.fetch_add(1, Ordering::Relaxed);
    // Defer to a DPC, as device drivers do (an AHCI ISR queues the IRP completion).
    crate::dpc::enqueue(&PIT_DPC);
}

fn pit_dpc_handler(_ctx: *mut ()) {
    PIT_DPC_RUNS.fetch_add(1, Ordering::Relaxed);
}

/// Program PIT channel 0 as a ~`PIT_INPUT_HZ / count` Hz rate generator (mode 2) raising IRQ0.
///
/// # Safety
///
/// Ring-0; owns the PIT ports.
unsafe fn pit_ch0_periodic(count: u16) {
    // SAFETY: writing the PIT command + channel-0 data ports per the 8254 spec.
    unsafe {
        regs::outb(PIT_CMD, PIT_CH0_MODE2);
        regs::outb(PIT_CH0_DATA, (count & 0xFF) as u8);
        regs::outb(PIT_CH0_DATA, (count >> 8) as u8);
    }
}

/// The x86_64 [`ArchIrqRouter`] implementation (IOAPIC); re-exported as `crate::arch::IrqRouter`.
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

        // SAFETY: `io.addr` is the IOAPIC's MMIO frame from the MADT, the kernel's for good.
        let va = unsafe { kvmap::map_mmio(io.addr, 1) }?;
        let mut ioapic = IoApic { base: va.as_u64(), gsi_base: io.gsi_base, entries: 0 };
        ioapic.entries = ((ioapic.read(REG_VER) >> 16) & 0xFF) + 1;
        let masked = encode_rte(0, 0, Polarity::ActiveHigh, TriggerMode::Edge, true);
        for idx in 0..ioapic.entries {
            ioapic.write_entry(idx, masked);
        }
        let max_entries = ioapic.entries;
        *IOAPIC.lock() = Some(ioapic);

        // Mask the legacy 8259 PICs so external IRQs flow only via the IOAPIC.
        // SAFETY: ring-0 writes to the PIC data ports; `0xFF` masks all lines.
        unsafe {
            regs::outb(PIC1_DATA, 0xFF);
            regs::outb(PIC2_DATA, 0xFF);
        }

        crate::kprintln!("ioapic: up ({} entries), 8259 masked", max_entries);
        Ok(())
    }

    unsafe fn route(irq: u32, vector: u8, dest: u32, trigger: TriggerMode, polarity: Polarity) -> bool {
        let mut guard = IOAPIC.lock();
        let Some(io) = guard.as_mut() else { return false };
        let Some((idx, dest)) = io.names(irq, dest) else { return false };
        io.write_entry(idx, encode_rte(vector, dest, polarity, trigger, false));
        true
    }

    unsafe fn mask(irq: u32) {
        if let Some(io) = IOAPIC.lock().as_mut() {
            io.set_mask(irq, true);
        }
    }

    unsafe fn unmask(irq: u32) {
        if let Some(io) = IOAPIC.lock().as_mut() {
            io.set_mask(irq, false);
        }
    }

    unsafe fn self_test() {
        if IOAPIC.lock().is_none() {
            return; // no IOAPIC to test
        }
        let bsp = crate::arch::Irq::id();
        let (gsi, polarity, trigger) = resolve_isa_irq(0, acpi::source_overrides());
        let vector = idt::register_device_handler(pit_tick);
        PIT_TICKS.store(0, Ordering::Relaxed);
        PIT_DPC_RUNS.store(0, Ordering::Relaxed);

        // Route the PIT's GSI to our vector on the boot CPU and start the PIT.
        // SAFETY: ring-0, post-init; the handler for `vector` is registered.
        if !unsafe { Self::route(gsi, vector, bsp, trigger, polarity) } {
            crate::kprintln!("ioapic: the PIT's GSI{} cannot be routed to the boot CPU", gsi);
            return;
        }
        // SAFETY: ring-0; we own the PIT.
        unsafe { pit_ch0_periodic((PIT_INPUT_HZ / 100) as u16) };

        // Brief interrupt-enabled window: only the PIT GSI is unmasked (LAPIC timer LVT and
        // 8259 masked) and the scheduler is not running. Wait for 3 ticks or ~100 ms.
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
            "ioapic: routed PIT IRQ0\u{2192}GSI{}\u{2192}vec{:#x}; took {} interrupts ({} via DPC)",
            gsi,
            vector,
            PIT_TICKS.load(Ordering::Relaxed),
            PIT_DPC_RUNS.load(Ordering::Relaxed),
        );
    }
}

/// Install a legacy **ISA** interrupt end-to-end for an in-kernel driver: resolve the ISA IRQ
/// to its GSI and trigger/polarity (honouring ACPI source overrides), register `handler` for a
/// fresh device-interrupt vector, route the GSI to it on the boot CPU, and leave the line
/// unmasked. Returns the IDT vector, or `None`, taking none, if the GSI cannot reach the boot CPU.
///
/// Arch-internal (`pub(crate)`, not re-exported by `crate::arch`): "ISA" is x86-only, so a
/// fixed legacy device wires its interrupt through its own arch module (the serial console via
/// [`super::serial::console_arm_rx`]). See `docs/conventions/arch-boundary.md`. The PCI
/// siblings are [`ArchIrqInstall`](crate::arch::irq_install::ArchIrqInstall)'s, in
/// [`super::irq_install`].
///
/// # Safety
/// Ring-0, after [`X86IoApic::init`]. `handler` must stay valid for the kernel's
/// lifetime; the caller must be ready to receive the IRQ once interrupts are enabled.
pub(crate) unsafe fn install_isa_irq(isa_irq: u8, handler: extern "C" fn()) -> Option<u8> {
    let (gsi, polarity, trigger) = resolve_isa_irq(isa_irq, acpi::source_overrides());
    // SAFETY: forwarded from this fn's contract.
    unsafe { install(gsi, crate::arch::Irq::id(), handler, trigger, polarity) }
}

/// **Route `gsi` to `dest` on a fresh vector for `handler`**; `None`, taking none, if it cannot.
///
/// # Safety
/// As [`install_isa_irq`].
pub(super) unsafe fn install(
    gsi: u32,
    dest: u32,
    handler: extern "C" fn(),
    trigger: TriggerMode,
    polarity: Polarity,
) -> Option<u8> {
    if !IOAPIC.lock().as_ref().is_some_and(|io| io.names(gsi, dest).is_some()) {
        return None;
    }
    let vector = idt::register_device_handler(handler);
    // SAFETY: forwarded from this fn's contract; the handler for `vector` was just
    // registered, so the routed interrupt has somewhere to land.
    unsafe { X86IoApic::route(gsi, vector, dest, trigger, polarity) }.then_some(vector)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::x86_64::acpi::SourceOverride;

    #[test]
    fn encode_rte_bit_layout() {
        // Vector 0x30, APIC id 0, active-high edge, unmasked.
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

    /// **An IOAPIC serves the GSIs from its base to its count**, and neither neighbour.
    #[test]
    fn an_ioapic_serves_its_gsis_and_neither_neighbour() {
        let io = IoApic { base: 0, gsi_base: 24, entries: 24 };
        assert_eq!(io.entry(23), None);
        assert_eq!(io.entry(24), Some(0));
        assert_eq!(io.entry(47), Some(23));
        assert_eq!(io.entry(48), None);
        assert_eq!(IoApic { base: 0, gsi_base: u32::MAX, entries: 24 }.entry(0), None);
    }

    /// **A destination wider than the entry's eight bits is refused**, not truncated.
    #[test]
    fn a_destination_wider_than_the_entry_is_refused_rather_than_truncated() {
        let io = IoApic { base: 0, gsi_base: 0, entries: 24 };
        assert_eq!(io.names(9, 0xFF), Some((9, 0xFF)));
        assert_eq!(io.names(9, 0x100), None);
        assert_eq!(io.names(24, 0), None);
    }

    /// **A refused install takes no vector**, and one that routes takes one.
    #[test]
    fn a_refused_install_takes_no_vector() {
        extern "C" fn handler() {}
        let mut regs = [0u32; 8];
        *IOAPIC.lock() = Some(IoApic { base: regs.as_mut_ptr() as u64, gsi_base: 0, entries: 24 });
        let taken = idt::device_slots_taken();
        let install = |gsi, dest| {
            // SAFETY: a host test, where nothing delivers the interrupt.
            unsafe { install(gsi, dest, handler, TriggerMode::Level, Polarity::ActiveLow) }
        };
        assert_eq!(install(9, 0x100), None);
        assert_eq!(install(24, 0), None);
        assert_eq!(idt::device_slots_taken(), taken);
        assert!(install(9, 0).is_some());
        assert_eq!(idt::device_slots_taken(), taken + 1);
        *IOAPIC.lock() = None;
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
