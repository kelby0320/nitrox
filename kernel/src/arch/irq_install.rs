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
//! The first two members are the two ways a PCI function's interrupt reaches a CPU:
//!
//! - **INTx**, a physical line the system router has to be told about. The line
//!   number comes from firmware, and that is the part which does not survive
//!   contact with real hardware — see `docs/planning/phase-5-bare-metal.md`.
//! - **MSI**, where the device is handed an address and a value and raises the
//!   interrupt by writing them itself. Nothing routes it, so there is nothing
//!   for firmware to get wrong, and two devices can never share a vector.
//!
//! The third is the odd one out: **a vector no device raises**
//! ([`ArchIrqInstall::install_software`]). A RAM disk finishes its copy inside
//! `submit`, but its completion must still run where a device's does — at the
//! device-interrupt tail, which drains the DPC and reschedules — or every block I/O
//! waits for the timer tick. So it installs a handler like any driver and raises the
//! vector on itself.
//!
//! The message's *contents* are architectural: on x86 the address names a local
//! APIC and the data names a vector. The PCI capability the message is written
//! into is not architectural at all, and lives in [`crate::pci`]. That split is
//! why this trait yields a message rather than programming a device.

use crate::arch::DeviceVector;

/// Why a device interrupt could not be installed. Either way it took no vector.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// Every device vector is taken.
    PoolFull,
    /// It cannot reach the boot CPU: its line has no route there, or the CPU no name in a message.
    Unreachable,
}

impl InstallError {
    /// What it says, for a log line or a decline.
    pub fn says(self) -> &'static str {
        match self {
            InstallError::PoolFull => "no device vector is free",
            InstallError::Unreachable => "no interrupt can reach the boot CPU",
        }
    }
}

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
    /// the vector, which dropped masks the line and stays taken.
    ///
    /// # Safety
    /// Ring-0, after the router is up. `handler` must stay valid for the
    /// kernel's lifetime, and the caller must be ready to receive `gsi` once
    /// interrupts are enabled.
    unsafe fn install_intx(gsi: u32, handler: extern "C" fn()) -> Result<DeviceVector, InstallError>;

    /// Register `handler` for a fresh device vector and return the message a
    /// device must write to raise it on the calling CPU. Nothing is routed:
    /// an MSI device delivers the interrupt itself, so the caller's remaining
    /// job is to write the message into the device (see
    /// [`crate::pci::program_msi`]). **The vector stays taken when dropped**: a
    /// device may have its message, unless [`DeviceVector::give_back`] says not.
    ///
    /// # Safety
    /// Ring-0, after the local controller is up. `handler` must stay valid for
    /// the kernel's lifetime, and the caller must be ready to receive the
    /// interrupt from the moment it writes the message into the device.
    unsafe fn install_msi(handler: extern "C" fn()) -> Result<(MsiMessage, DeviceVector), InstallError>;

    /// Register `handler` for a fresh device vector that **no device raises**: the kernel raises
    /// it on the current CPU with
    /// [`ArchIrq::raise_on_self`](crate::arch::irq::ArchIrq::raise_on_self). Returns the vector.
    ///
    /// For a backend with no hardware of its own whose completions must still run at a
    /// device-interrupt tail — a RAM disk. Without one, a completion DPC waits for the next timer
    /// tick, which serialises every block I/O on it. Dropped, the vector is given back.
    ///
    /// # Safety
    /// Ring-0, after the interrupt table is installed. `handler` must stay valid for the kernel's
    /// lifetime.
    unsafe fn install_software(handler: extern "C" fn()) -> Result<DeviceVector, InstallError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A full pool says so**, not that the interrupt cannot reach the CPU: a decline's reason.
    #[test]
    fn a_full_pool_says_so() {
        assert_eq!(InstallError::PoolFull.says(), "no device vector is free");
        assert_eq!(InstallError::Unreachable.says(), "no interrupt can reach the boot CPU");
    }
}
