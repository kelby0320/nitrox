//! x86_64 device-interrupt installation: the INTx and MSI halves of [`ArchIrqInstall`].
//!
//! Separate from [`super::ioapic`] because an MSI install never touches the IOAPIC: the
//! device writes to the **local** APIC, and the only shared piece is [`super::idt`]'s
//! device-vector registry.

use super::idt;
use super::ioapic::X86IoApic;
use crate::arch::irq::ArchIrq;
use crate::arch::irq_install::{ArchIrqInstall, MsiMessage};
use crate::arch::irq_router::{ArchIrqRouter, Polarity, TriggerMode};

/// Base of the local-APIC message address range; a write in `0xFEE0_0000..0xFEF0_0000` is
/// interpreted by the CPU's interrupt hardware rather than reaching memory.
const MSI_ADDR_BASE: u64 = 0xFEE0_0000;

const MSI_ADDR_DEST_SHIFT: u32 = 12;

/// The widest destination the compatibility message format can name.
///
/// The kernel is x2APIC-only and [`ArchIrq::id`] returns the full 32-bit id, but this field
/// holds eight; a wider id needs interrupt remapping, which Nitrox does not enable. The same
/// bound as [`super::smp::hw_apic_id`]'s "sufficient while `MAX_CPUS <= 255`". [`compose`]
/// returns `None` above it rather than truncating.
const MSI_MAX_DEST: u32 = 0xFF;

/// x86_64's device-interrupt installation facility.
pub struct X86IrqInstall;

/// Build the compatibility-format message that raises `vector` on the local controller with
/// id `dest`.
///
/// The address carries the destination in physical mode (redirection hint and destination
/// mode clear). The data is the vector alone because delivery mode is Fixed (bits 10:8
/// clear) and the message edge-triggered (bit 15 clear). Split out so it is host-testable.
fn compose(vector: u8, dest: u32) -> Option<MsiMessage> {
    if dest > MSI_MAX_DEST {
        return None;
    }
    Some(MsiMessage { address: MSI_ADDR_BASE | ((dest as u64) << MSI_ADDR_DEST_SHIFT), data: vector as u16, vector })
}

impl ArchIrqInstall for X86IrqInstall {
    /// Composite over the device-vector registry ([`idt`]), the local controller
    /// ([`ArchIrq::id`], the destination CPU) and the router ([`X86IoApic::route`]).
    unsafe fn install_intx(gsi: u32, handler: extern "C" fn()) -> u8 {
        let bsp = crate::arch::Irq::id();
        let vector = idt::register_device_handler(handler);
        // SAFETY: forwarded from this fn's contract; the handler for `vector`
        // was just registered, so the routed interrupt has somewhere to land.
        unsafe { X86IoApic::route(gsi, vector, bsp, TriggerMode::Level, Polarity::ActiveLow) };
        vector
    }

    /// No router involvement. The destination is checked **before** a vector is taken, so a
    /// refusal costs nothing from the pool of eight.
    unsafe fn install_msi(handler: extern "C" fn()) -> Option<MsiMessage> {
        let dest = crate::arch::Irq::id();
        if dest > MSI_MAX_DEST {
            return None;
        }
        let vector = idt::register_device_handler(handler);
        compose(vector, dest)
    }

    unsafe fn install_software(handler: extern "C" fn()) -> u8 {
        // Nothing to route: the kernel raises the vector through the SELF IPI register.
        idt::register_device_handler(handler)
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
        // 0x100 shifted into bits 19:12 overflows into bit 20, outside the local APIC range.
        assert!(compose(0x30, 0xFF).is_some(), "the widest id that fits");
        assert_eq!(compose(0x30, 0x100), None, "and one past it is refused");
    }
}
