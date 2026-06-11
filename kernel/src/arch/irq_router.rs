//! Architecture-neutral system interrupt-router contract.
//!
//! [`ArchIrqRouter`] is the **system** interrupt router — the controller that
//! maps an external interrupt line (a device's IRQ) to a destination
//! `(CPU, vector)`. On x86_64 this is the **IOAPIC**; on aarch64 it is the
//! **GIC distributor**. It is a distinct concern from [`ArchIrq`](crate::arch::irq::ArchIrq),
//! which is the per-CPU **local** controller (the local APIC / GIC CPU
//! interface — end-of-interrupt, controller id, IPIs). The router is brought up
//! once for the system; the local controller is brought up on every CPU.
//!
//! Keeping these in separate traits mirrors the hardware (two controllers on
//! both architectures, e.g. the GICv3 distributor vs. redistributor), makes the
//! SMP cardinality explicit (the router grows per-IRQ affinity; the local
//! controller is per-CPU), and lets message-signalled interrupts (MSI/MSI-X)
//! later unify under "route an external source → CPU+vector" on the router. See
//! the decision log (2026-06-11) for the long-term rationale.
//!
//! The active architecture's implementation is re-exported from `crate::arch`
//! as `IrqRouter` (see `kernel/src/arch/mod.rs`).

use crate::libkern::AllocError;

/// How an external interrupt line asserts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    /// Asserts on a transition (legacy ISA / PCI INTx are typically edge).
    Edge,
    /// Asserts for the duration of the condition (PCI level-triggered lines).
    Level,
}

/// The active polarity of an external interrupt line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh,
    ActiveLow,
}

/// The system interrupt router: maps external interrupt lines to a
/// `(CPU, vector)` destination.
pub trait ArchIrqRouter {
    /// Bring up the router: mask every line, and disable any legacy router
    /// (the 8259 PICs on x86) so external interrupts flow only through it. Call
    /// once at boot, after the local controller ([`ArchIrq::init`](crate::arch::irq::ArchIrq::init))
    /// is up (routed interrupts are delivered to a local controller).
    ///
    /// # Safety
    /// Ring-0 only; reconfigures system interrupt delivery. Call once during
    /// boot. Returns [`AllocError`] if a mapping it needs (the router's MMIO
    /// page) cannot be established.
    unsafe fn init() -> Result<(), AllocError>;

    /// Route external interrupt line `irq` to interrupt `vector` on the CPU
    /// identified by local-controller id `dest`, with the given trigger mode and
    /// polarity, and **unmask** it. `irq` is the platform line number (a GSI on
    /// x86, a SPI number on aarch64); resolving a legacy/device IRQ to that line
    /// number is the architecture's own concern.
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchIrqRouter::init). The caller must
    /// have installed a handler for `vector` first.
    unsafe fn route(irq: u32, vector: u8, dest: u32, trigger: TriggerMode, polarity: Polarity);

    /// Mask an external interrupt line (stop delivering it).
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchIrqRouter::init).
    unsafe fn mask(irq: u32);

    /// Unmask a previously-routed external interrupt line.
    ///
    /// # Safety
    /// Ring-0 only; valid after [`init`](ArchIrqRouter::init) and a prior
    /// [`route`](ArchIrqRouter::route) of `irq`.
    unsafe fn unmask(irq: u32);

    /// Bring-up diagnostic: route a known periodic source through the router and
    /// confirm an interrupt is delivered end-to-end. On x86 this routes the
    /// legacy PIT in a brief controlled window; on a platform without one it is
    /// a no-op. Useful to prove the routing path works on a new CPU/board before
    /// any device driver exists.
    ///
    /// # Safety
    /// Ring-0 only; call once during boot after [`init`](ArchIrqRouter::init),
    /// before the scheduler's periodic timer is armed (it briefly enables
    /// interrupts).
    unsafe fn self_test();
}
