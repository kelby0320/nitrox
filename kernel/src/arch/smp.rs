//! Architecture-neutral symmetric-multiprocessing contract.
//!
//! Phase 1 is single-CPU; [`ArchSmp`] is a stub that reports one CPU and
//! refuses inter-processor interrupts. It exists so later code can reference a
//! stable neutral surface (`cpu_count`/`current_cpu`) instead of hard-coding
//! `1`/`0`. Phase-3 SMP brings up the real implementation and rewires the
//! current single-CPU stand-ins — `handle::current_ctx_id()` (the grace-tracker
//! shim, constant 0 today) and the syscall `CpuLocal` block — onto it.
//!
//! The active architecture's implementation is re-exported from
//! `crate::arch` as `Smp` (see `kernel/src/arch/mod.rs`).

/// Symmetric-multiprocessing queries and operations.
pub trait ArchSmp {
    /// Number of CPUs the kernel is driving. Phase 1: always `1`.
    fn cpu_count() -> usize;

    /// The CPU the current thread is running on. Phase 1: always `0`.
    fn current_cpu() -> u32;

    /// Send an inter-processor interrupt of `vector` to CPU `target`.
    ///
    /// # Safety
    /// Ring-0 only; valid only on a multi-CPU system (Phase 3). On the
    /// single-CPU Phase-1 kernel there is no second CPU to target, so calling
    /// this is a logic error (the stub panics).
    unsafe fn send_ipi(target: u32, vector: u8);
}
