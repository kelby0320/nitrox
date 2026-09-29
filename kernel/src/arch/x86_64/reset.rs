//! **Resetting the machine** (administration Part E.3), in the conventional order: the FADT's reset
//! register when the FADT advertises one, then the 8042's reset line, then a triple fault. None of
//! it needs AML.
//!
//! Each step gets [`SETTLE_NS`] to take effect before the next is tried, and says on COM1 which
//! one it is, so a machine that resets on its second step shows that its first did nothing. Each
//! resets q35 on its own. The last cannot fail: an exception with no IDT to deliver it through is
//! a triple fault, which shuts the processor down, and a PC answers a shutdown by resetting.
//!
//! [`reset`] runs with every other processor stopped, some possibly holding a lock, so it takes
//! none and allocates nothing. The one step that would — a reset register in **memory** space,
//! which needs a mapping — is mapped by [`prepare`] first, while the machine still runs.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use super::acpi::{Fadt, GAS_PCI_CONFIG, GAS_SYSTEM_IO, GAS_SYSTEM_MEMORY, Gas};
use super::regs::{outb, outl};
use crate::arch::timer::ArchTimer;
use crate::mm::{PAGE_SIZE, PhysAddr};

/// How long each step is given to reset the machine before the next is tried.
const SETTLE_NS: u64 = 500_000_000;

/// The legacy PCI configuration mechanism: the address port, then the data port.
const PCI_CONFIG_ADDRESS: u16 = 0xcf8;
const PCI_CONFIG_DATA: u16 = 0xcfc;

/// Where [`prepare`] mapped a reset register in memory space; 0 when it did not.
static MAPPED: AtomicU64 = AtomicU64::new(0);

/// One way to reset the machine that may not work: each is followed by [`settle`], and then the
/// next. The triple fault, which cannot fail, is not one — it is what [`reset`] ends in.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Write the byte to the FADT's reset register.
    Register(Gas, u8),
    /// Pulse the 8042's reset line.
    Kbc,
}

/// **The steps a reset tries before its triple fault, in order.** The FADT's register comes first
/// when it has one this kernel can write; the 8042 is tried on every machine.
pub(crate) fn steps(fadt: Option<Fadt>) -> [Option<Step>; 2] {
    let register = fadt.and_then(|f| f.reset).filter(|(gas, _)| writable(gas));
    [register.map(|(gas, value)| Step::Register(gas, value)), Some(Step::Kbc)]
}

/// Whether a reset register is one this kernel can write: an I/O port, physical memory, or a
/// configuration register the legacy PCI mechanism reaches. ACPI allows those three spaces, and
/// no other, for the reset register.
fn writable(gas: &Gas) -> bool {
    match gas.space {
        GAS_SYSTEM_IO => gas.address <= u16::MAX as u64,
        GAS_SYSTEM_MEMORY => gas.address != 0,
        GAS_PCI_CONFIG => pci_config(gas.address).is_some(),
        _ => false,
    }
}

/// A PCI configuration address as a GAS packs it — bus 0, the device in bits 32–47, the function
/// in 16–31 and the offset in 0–15 (ACPI 6.5 §5.2.3.2) — or `None` for one the legacy mechanism
/// cannot reach, which addresses 256 bytes of each function.
pub(crate) fn pci_config(address: u64) -> Option<(u8, u8, u8)> {
    let device = (address >> 32) & 0xffff;
    let (function, offset) = ((address >> 16) & 0xffff, address & 0xffff);
    let fits = address >> 48 == 0 && device < 32 && function < 8 && offset < 256;
    fits.then_some((device as u8, function as u8, offset as u8))
}

/// The legacy mechanism's address for a register of bus 0: enable, device, function, and the
/// offset's doubleword.
fn pci_config_address(device: u8, function: u8, offset: u8) -> u32 {
    1 << 31 | (device as u32) << 11 | (function as u32) << 8 | (offset as u32 & 0xfc)
}

/// Map a reset register in memory space, so that [`reset`] can write it without allocating.
/// Nothing to do for the other spaces, or for a machine with no reset register.
pub(crate) fn prepare() {
    let Some(Step::Register(gas, _)) = steps(super::acpi::fadt())[0] else {
        return;
    };
    if gas.space != GAS_SYSTEM_MEMORY || MAPPED.load(Ordering::Acquire) != 0 {
        return;
    }
    let page = gas.address & !(PAGE_SIZE as u64 - 1);
    // SAFETY: the FADT names this page as holding the reset register, which is device memory the
    // kernel maps for no other purpose; the mapping is kept for the rest of this boot, which is
    // not long.
    match unsafe { crate::mm::kvmap::map_mmio(PhysAddr(page), 1) } {
        Ok(va) => MAPPED.store(va.as_u64() + (gas.address - page), Ordering::Release),
        Err(_) => {
            crate::kprintln!("power: the reset register at {:#x} could not be mapped", gas.address)
        }
    }
}

/// Reset the machine: each of [`steps`] in turn, then a triple fault.
///
/// # Safety
/// Ring 0, with interrupts masked and every other processor stopped.
pub(crate) unsafe fn reset() -> ! {
    let mut w = super::serial::emergency_writer();
    for step in steps(super::acpi::fadt()).into_iter().flatten() {
        match step {
            Step::Register(gas, value) => {
                let _ = writeln!(w, "power: resetting through the FADT's reset register");
                // SAFETY: forwarded from this function's contract; the FADT names the register.
                unsafe { write_register(&gas, value) };
            }
            Step::Kbc => {
                let _ = writeln!(w, "power: resetting through the 8042");
                // SAFETY: forwarded from this function's contract.
                unsafe { super::ps2::pulse_reset() };
            }
        }
        settle();
    }
    let _ = writeln!(w, "power: resetting by a triple fault");
    // SAFETY: forwarded from this function's contract.
    unsafe { triple_fault() }
}

/// Write `value` to the reset register `gas`, which [`writable`] accepted.
///
/// # Safety
/// Ring 0; the register is the FADT's reset register, and writing it resets the machine.
unsafe fn write_register(gas: &Gas, value: u8) {
    match gas.space {
        // SAFETY: the FADT names this port as the reset register.
        GAS_SYSTEM_IO => unsafe { outb(gas.address as u16, value) },
        GAS_SYSTEM_MEMORY => {
            let va = MAPPED.load(Ordering::Acquire);
            if va != 0 {
                // SAFETY: `prepare` mapped the register's page uncached at `va`'s page.
                unsafe { (va as *mut u8).write_volatile(value) };
            }
        }
        GAS_PCI_CONFIG => {
            if let Some((device, function, offset)) = pci_config(gas.address) {
                // SAFETY: the FADT names this configuration register; the legacy mechanism's two
                // ports are the platform's, and nothing else uses them once the machine stops.
                unsafe {
                    outl(PCI_CONFIG_ADDRESS, pci_config_address(device, function, offset));
                    outb(PCI_CONFIG_DATA + (offset & 3) as u16, value);
                }
            }
        }
        _ => {}
    }
}

/// Give a step [`SETTLE_NS`] to reset the machine. The clock is read from the processor's
/// counter, which needs neither interrupts nor a lock.
fn settle() {
    let until = crate::arch::Timer::read_ns().saturating_add(SETTLE_NS);
    while crate::arch::Timer::read_ns() < until {
        core::hint::spin_loop();
    }
}

/// Load an empty IDT and raise an exception. Nothing can deliver it, nor the general protection
/// fault that follows, nor the double fault after that, so the processor shuts down.
///
/// # Safety
/// Ring 0, with interrupts masked: nothing after this runs.
unsafe fn triple_fault() -> ! {
    #[repr(C, packed)]
    struct Idtr {
        limit: u16,
        base: u64,
    }
    let empty = Idtr { limit: 0, base: 0 };
    // SAFETY: `lidt` reads the ten bytes at `empty`, which live on this stack; the `int3` after it
    // cannot be delivered, which is the point. Neither returns.
    unsafe {
        asm!("lidt [{}]", "int3", "2: hlt", "jmp 2b", in(reg) &empty, options(noreturn, nostack))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IO: Gas =
        Gas { space: GAS_SYSTEM_IO, bit_width: 8, bit_offset: 0, access_size: 1, address: 0xcf9 };

    fn with_reset(reset: Option<(Gas, u8)>) -> Option<Fadt> {
        Some(Fadt { reset, ..Fadt::default() })
    }

    /// **The FADT's register first, when it has one this kernel can write**, then the 8042 —
    /// which every machine gets, whatever its FADT says — before the triple fault.
    #[test]
    fn a_reset_tries_the_register_then_the_8042() {
        let q35 = steps(with_reset(Some((IO, 0x0f))));
        assert_eq!(q35, [Some(Step::Register(IO, 0x0f)), Some(Step::Kbc)]);
        let rest = [None, Some(Step::Kbc)];
        assert_eq!(steps(None), rest, "no FADT");
        assert_eq!(steps(with_reset(None)), rest, "no reset register");
        // Functional fixed hardware, and an I/O address past the port space: neither is written.
        let ffh = Gas { space: 0x7f, ..IO };
        assert_eq!(steps(with_reset(Some((ffh, 1)))), rest);
        let far = Gas { address: 0x1_0000, ..IO };
        assert_eq!(steps(with_reset(Some((far, 1)))), rest);
        let memory = Gas { space: GAS_SYSTEM_MEMORY, address: 0xfed0_3000, ..IO };
        assert_eq!(steps(with_reset(Some((memory, 1))))[0], Some(Step::Register(memory, 1)));
        let nowhere = Gas { space: GAS_SYSTEM_MEMORY, address: 0, ..IO };
        assert_eq!(steps(with_reset(Some((nowhere, 1)))), rest);
    }

    /// **A PCI configuration address is bus 0's**, with the device, function and offset in their
    /// own 16-bit fields, and only what the legacy mechanism can reach is written.
    #[test]
    fn a_pci_config_reset_register_decodes_to_device_function_and_offset() {
        // Device 31, function 0, offset 0xac — where an Intel chipset's reset control sits.
        assert_eq!(pci_config(0x001f_0000_00ac), Some((31, 0, 0xac)));
        assert_eq!(pci_config(0x0001_0002_0003), Some((1, 2, 3)));
        assert_eq!(pci_config(0x0020_0000_0000), None, "device 32");
        assert_eq!(pci_config(0x0000_0008_0000), None, "function 8");
        assert_eq!(pci_config(0x0000_0000_0100), None, "offset 256");
        assert_eq!(pci_config(0x0001_0000_0000_0000), None, "the reserved word");
        assert_eq!(pci_config_address(31, 0, 0xac), 0x8000_f8ac);
        assert_eq!(pci_config_address(1, 2, 0x07), 0x8000_0a04, "the doubleword holding offset 7");
    }
}
