//! Architecture-neutral CPU feature-detection and control contract.
//!
//! [`ArchCpu`] groups per-CPU feature queries and control operations whose
//! implementation is genuinely architecture-specific (CPUID and the GDT/IDT/
//! control registers on x86_64; the ID/feature/system registers on aarch64):
//! installing the CPU control tables, enabling memory-protection features,
//! setting the trap kernel stack, halting, and feature detection.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Cpu` (see `kernel/src/arch/mod.rs`).

/// Per-CPU feature queries and control operations.
pub trait ArchCpu {
    /// Install the CPU's control tables early in boot (on x86_64: the GDT
    /// with its TSS, then the IDT — the order is fixed inside the impl).
    fn init_tables();

    /// Enable every CPU-level memory-protection feature the kernel depends on
    /// (on x86_64: NX paging via `EFER.NXE`, plus SMEP and SMAP). Panics if a
    /// required feature is missing on the running CPU. A future aarch64 port
    /// configures PAN/PXN/equivalents here; the boot caller is unchanged.
    fn init_protections();

    /// Set the kernel stack the CPU loads on a ring3→ring0 trap (the neutral
    /// name for `TSS.RSP0` on x86_64).
    fn set_kernel_stack(top: u64);

    /// Park the CPU forever: disable interrupts and `hlt` in a loop so a
    /// spurious wake-up cannot restart execution. Never returns.
    fn halt_loop() -> !;

    /// `true` if this CPU has an on-chip local interrupt controller (the one
    /// [`crate::arch::Irq`] brings up). On x86_64 this is the on-chip APIC
    /// CPUID feature bit.
    fn has_apic() -> bool;

    /// Halt the current CPU until the next interrupt wakes it. Unlike
    /// [`halt_loop`](ArchCpu::halt_loop) (which disables interrupts and parks
    /// forever), this returns when an interrupt arrives — the primitive the
    /// idle thread will run with interrupts enabled.
    ///
    /// # Safety
    /// Ring-0 only. The caller owns the interrupt-flag state that decides
    /// what (if anything) can wake the CPU: with interrupts masked (IF=0,
    /// as in this Phase-1 slice) only an NMI/SMI resumes it.
    unsafe fn halt();
}
