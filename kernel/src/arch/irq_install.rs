//! Architecture-neutral device-interrupt **installation** contract.
//!
//! [`ArchIrqInstall`] is what a Tier 1 driver uses to acquire an interrupt:
//! register a handler in the architecture's vector table, and arrange for the
//! device's interrupt to reach it. It is deliberately distinct from the two
//! hardware abstractions it composes —
//! [`ArchIrqRouter`](crate::arch::irq_router::ArchIrqRouter), the system router
//! that maps an external line to a `(CPU, vector)`, and
//! [`ArchIrq`](crate::arch::irq::ArchIrq), the per-CPU local controller —
//! because installation spans both plus the handler registry and belongs to
//! none of them.
//!
//! **It became a trait when it acquired a second member.** Through Phase 4 it
//! had exactly one, a neutral `install_pci_irq` free function, and the project's
//! rule is to build an abstraction at its second consumer rather than in
//! anticipation of one. Phase 5 Part A is that second consumer.
//!
//! The two members are the two ways a PCI function's interrupt reaches a CPU:
//!
//! - **INTx**, a physical line the system router has to be told about. The line
//!   number comes from firmware, and that is the part which does not survive
//!   contact with real hardware — see `docs/planning/phase-5-bare-metal.md`.
//! - **MSI**, where the device is handed an address and a value and raises the
//!   interrupt by writing them itself. Nothing routes it, so there is nothing
//!   for firmware to get wrong, and two devices can never share a vector.
//!
//! The message's *contents* are architectural: on x86 the address names a local
//! APIC and the data names a vector. The PCI capability the message is written
//! into is not architectural at all, and lives in [`crate::pci`]. That split is
//! why this trait yields a message rather than programming a device.

/// The write a PCI function performs in order to raise its interrupt.
///
/// Produced by [`ArchIrqInstall::install_msi`] and consumed by
/// [`crate::pci::program_msi`], which knows where in the capability each field
/// goes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MsiMessage {
    /// The address the device writes to.
    pub address: u64,
    /// The value it writes there.
    pub data: u16,
    /// The interrupt vector that write will land on. Carried for diagnostics —
    /// the device never sees it separately from `data`.
    pub vector: u8,
}

/// Installing a device interrupt: handler registration plus whatever delivery
/// needs, for each way a device can deliver one.
pub trait ArchIrqInstall {
    /// Install a **PCI INTx** interrupt end to end: register `handler` for a
    /// fresh device vector, route `gsi` to it on the boot CPU with the PCI INTx
    /// convention (level-triggered, active-low), and unmask the line. Returns
    /// the assigned vector.
    ///
    /// # Safety
    /// Ring-0, after the router is up. `handler` must stay valid for the
    /// kernel's lifetime, and the caller must be ready to receive `gsi` once
    /// interrupts are enabled.
    unsafe fn install_intx(gsi: u32, handler: extern "C" fn()) -> u8;

    /// Register `handler` for a fresh device vector and return the message a
    /// device must write to raise it on the calling CPU. Nothing is routed:
    /// an MSI device delivers the interrupt itself, so the caller's remaining
    /// job is to write the message into the device (see
    /// [`crate::pci::program_msi`]).
    ///
    /// Returns `None` when this CPU cannot be named in a message — the vector
    /// pool is untouched in that case, and the caller should fall back to
    /// [`install_intx`](ArchIrqInstall::install_intx).
    ///
    /// # Safety
    /// Ring-0, after the local controller is up. `handler` must stay valid for
    /// the kernel's lifetime, and the caller must be ready to receive the
    /// interrupt from the moment it writes the message into the device.
    unsafe fn install_msi(handler: extern "C" fn()) -> Option<MsiMessage>;
}
