//! Architecture-neutral CPU feature-detection and control contract.
//!
//! [`ArchCpu`] groups per-CPU feature queries and control operations whose
//! implementation is genuinely architecture-specific (CPUID on x86_64, the
//! ID/feature registers on aarch64). It is the home for *new* CPU surface;
//! the existing boot-time CPU free functions in [`crate::arch`]
//! (`init_cpu_tables`, `init_protections`, `set_kernel_stack`, `halt_loop`)
//! are folded in by the later arch-boundary-normalization slice.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Cpu` (see `kernel/src/arch/mod.rs`).

/// Per-CPU feature queries and control operations.
pub trait ArchCpu {
    /// `true` if this CPU has an on-chip local interrupt controller (the one
    /// [`crate::arch::Irq`] brings up). On x86_64 this is the on-chip APIC
    /// CPUID feature bit.
    fn has_apic() -> bool;

    /// Halt the current CPU until the next interrupt wakes it. Unlike
    /// [`crate::arch::halt_loop`] (which disables interrupts and parks
    /// forever), this returns when an interrupt arrives — the primitive the
    /// idle thread will run with interrupts enabled.
    ///
    /// # Safety
    /// Ring-0 only. The caller owns the interrupt-flag state that decides
    /// what (if anything) can wake the CPU: with interrupts masked (IF=0,
    /// as in this Phase-1 slice) only an NMI/SMI resumes it.
    unsafe fn halt();
}
