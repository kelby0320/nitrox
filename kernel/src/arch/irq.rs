//! Architecture-neutral local interrupt-controller contract.
//!
//! [`ArchIrq`] is the per-CPU local interrupt controller (the local APIC on
//! x86_64, the GIC CPU interface on aarch64): bring-up, end-of-interrupt, and
//! identity. The timer local-vector is programmed by the timekeeping slice
//! (`ArchTimer`), and the preemptive-scheduling slice added the IRQ entry stubs
//! and raised `IF=1`, so the periodic timer now drives the scheduler.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Irq` (see `kernel/src/arch/mod.rs`).

use crate::libkern::AllocError;

/// Interrupt vector for the controller's spurious interrupt. The controller is
/// told to deliver unclassifiable interrupts here; the IDT installs a stub that
/// simply `iretq`s (a spurious interrupt takes no EOI).
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// Interrupt vector the per-CPU timer raises. Programmed into the timer's
/// local-vector entry by `ArchTimer`, handled by the IDT's returning timer stub
/// (which drives the preemptive scheduler). `0x20` is the first vector above
/// the 0–31 range the CPU reserves for exceptions.
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
