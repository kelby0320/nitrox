//! x86_64 device-interrupt installation: the INTx and MSI halves of
//! [`ArchIrqInstall`].
//!
//! This is a module of its own rather than more of [`super::ioapic`] because
//! only one of the two halves is the IOAPIC's business. An INTx install routes a
//! line through the IOAPIC; an MSI install never touches it — the device writes
//! to the **local** APIC, and the only shared piece is [`super::idt`]'s
//! device-vector registry.

use super::idt;
use super::ioapic::X86IoApic;
use crate::arch::irq::ArchIrq;
use crate::arch::irq_install::{ArchIrqInstall, MsiMessage};
use crate::arch::irq_router::{ArchIrqRouter, Polarity, TriggerMode};

/// Base of the local-APIC message address range. A write anywhere in
/// `0xFEE0_0000..0xFEF0_0000` is interpreted by the CPU's interrupt hardware
/// rather than reaching memory.
const MSI_ADDR_BASE: u64 = 0xFEE0_0000;

/// Destination-id field of the message address: **eight bits**, at 19:12.
const MSI_ADDR_DEST_SHIFT: u32 = 12;

/// The widest destination the compatibility message format can name.
///
/// This is a real narrowing and it is recorded rather than assumed: the kernel
/// is x2APIC-only and [`ArchIrq::id`] returns the full 32-bit x2APIC id, while
/// this field holds eight. Addressing a wider id needs interrupt remapping,
/// which Nitrox does not enable. **The bound is not new** — the dense-index
/// binding in [`super::smp::hw_apic_id`] already reads the 8-bit initial xAPIC
/// id and documents "sufficient while `MAX_CPUS <= 255`" — so the two
/// assumptions are one assumption, and they fail together. [`compose`] returns
/// `None` above it rather than truncating silently.
const MSI_MAX_DEST: u32 = 0xFF;

/// x86_64's device-interrupt installation facility.
pub struct X86IrqInstall;

/// Build the compatibility-format message that raises `vector` on the local
/// controller with id `dest`.
///
/// The address carries the destination in physical mode (redirection hint and
/// destination mode both clear). The data carries the vector alone, which is
/// true only because delivery mode is Fixed (bits 10:8 clear) and the message is
/// edge-triggered (bit 15 clear) — both of which are the zero value, so the
/// composition is `data == vector` rather than a masked assembly.
///
/// Separate from [`X86IrqInstall::install_msi`] so it is host-testable: the
/// install half reads an MSR and mutates the vector registry, and this half is
/// the arithmetic that a wrong answer would be silent about.
fn compose(vector: u8, dest: u32) -> Option<MsiMessage> {
    if dest > MSI_MAX_DEST {
        return None;
    }
    Some(MsiMessage {
        address: MSI_ADDR_BASE | ((dest as u64) << MSI_ADDR_DEST_SHIFT),
        data: vector as u16,
        vector,
    })
}

impl ArchIrqInstall for X86IrqInstall {
    /// Composite over three concerns that no single hardware abstraction owns:
    /// the device-vector **handler registry** ([`idt`], which
    /// [`ArchIrqRouter`] deliberately keeps off itself), the **local
    /// controller** ([`ArchIrq::id`], for the destination CPU), and the
    /// **router** ([`X86IoApic::route`], the resolved-`(line, vector)`
    /// primitive).
    unsafe fn install_intx(gsi: u32, handler: extern "C" fn()) -> u8 {
        let bsp = crate::arch::Irq::id();
        let vector = idt::register_device_handler(handler);
        // SAFETY: forwarded from this fn's contract; the handler for `vector`
        // was just registered, so the routed interrupt has somewhere to land.
        unsafe { X86IoApic::route(gsi, vector, bsp, TriggerMode::Level, Polarity::ActiveLow) };
        vector
    }

    /// No router involvement: the device is told where to write. The
    /// destination is checked **before** a vector is taken, so a refusal costs
    /// nothing from a pool of eight.
    unsafe fn install_msi(handler: extern "C" fn()) -> Option<MsiMessage> {
        let dest = crate::arch::Irq::id();
        if dest > MSI_MAX_DEST {
            return None;
        }
        let vector = idt::register_device_handler(handler);
        compose(vector, dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_names_the_boot_cpu_and_the_vector() {
        let m = compose(0x30, 0).expect("destination 0 fits");
        assert_eq!(m.address, 0xFEE0_0000, "local APIC base, destination 0");
        assert_eq!(m.data, 0x30, "fixed delivery, edge — the data is the vector");
        assert_eq!(m.vector, 0x30);
    }

    #[test]
    fn the_destination_lands_in_address_bits_19_through_12() {
        assert_eq!(compose(0x31, 3).unwrap().address, 0xFEE0_3000);
        assert_eq!(compose(0x31, 0xFF).unwrap().address, 0xFEEF_F000);
    }

    #[test]
    fn a_destination_wider_than_the_field_is_refused_rather_than_truncated() {
        // The last id the field can hold, and the first it cannot. 0x100 shifted
        // into bits 19:12 overflows into bit 20 — an address outside the local
        // APIC range entirely, which the hardware would treat as a memory write.
        assert!(compose(0x30, 0xFF).is_some(), "the widest id that fits");
        assert_eq!(compose(0x30, 0x100), None, "and one past it is refused");
    }
}
