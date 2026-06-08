//! Architecture-neutral local interrupt-controller contract.
//!
//! [`ArchIrq`] is the per-CPU local interrupt controller (the local APIC on
//! x86_64, the GIC CPU interface on aarch64). This slice only *brings it up*
//! and exposes end-of-interrupt + identity; interrupts stay masked (IF=0) for
//! the whole slice, so nothing is delivered yet. The timer source (the
//! controller's timer local-vector) is programmed by the Timers slice, and
//! the IRQ entry stub / `IF=1` / masking-lock land with the Preemptive
//! scheduling slice.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Irq` (see `kernel/src/arch/mod.rs`).

use crate::libkern::AllocError;

/// Interrupt vector reserved for the controller's spurious interrupt. The
/// controller is told to deliver unclassifiable interrupts here; no handler
/// is installed this slice (with IF=0 nothing is delivered — the spurious
/// handler lands with the preemptive-scheduling slice that raises IF).
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Interrupt vector the per-CPU timer will raise. Programmed into the timer's
/// local-vector entry by the Timers slice; reserved here so the two slices
/// agree on the number. `0x20` is the first vector above the 0–31 range the
/// CPU reserves for exceptions.
pub const TIMER_VECTOR: u8 = 0x20;

/// Per-CPU local interrupt-controller operations.
pub trait ArchIrq {
    /// Bring up this CPU's local interrupt controller and software-enable it
    /// with the spurious vector. Does **not** unmask CPU interrupts (IF stays
    /// 0 this slice) and arms no interrupt source. Returns [`AllocError`] if a
    /// mapping the bring-up needs (e.g. the controller's MMIO page) cannot be
    /// established.
    ///
    /// # Safety
    /// Ring-0 only; reconfigures interrupt delivery for the current CPU. Call
    /// once per CPU during bring-up, after CPU feature enablement and after
    /// the kernel-vmap allocator is initialised.
    unsafe fn init() -> Result<(), AllocError>;

    /// Signal end-of-interrupt to the local controller. Called once from the
    /// handler of every controller-delivered interrupt.
    ///
    /// # Safety
    /// Ring-0 only; valid only from an interrupt handler after [`init`] has
    /// run on this CPU.
    ///
    /// [`init`]: ArchIrq::init
    unsafe fn eoi();

    /// This CPU's local-controller identifier.
    fn id() -> u32;
}
