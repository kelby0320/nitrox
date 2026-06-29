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
//!
//! `current_cpu()` is the neutral "which CPU am I" primitive: a **dense** logical
//! index in `0..cpu_count()`, used to index the per-CPU `CPUS[]` arrays. The
//! mechanism behind it is arch-internal (x86 reads `IA32_TSC_AUX` via `RDTSCP`,
//! set per CPU by `init_this_cpu`); see `docs/architecture/scheduler.md`
//! §Per-CPU access.

/// Upper bound on logical CPUs the kernel supports — sizes the per-CPU arrays
/// (both the arch `CpuLocal[]` and the neutral scheduler `CPUS[]`). Raising it is
/// a constant change. QEMU is exercised with `-smp 4`.
pub const MAX_CPUS: usize = 8;

/// Symmetric-multiprocessing queries and operations.
pub trait ArchSmp {
    /// Number of CPUs the kernel is driving. Phase 1 / slice 0: always `1`;
    /// the real count is learned from the bootloader at SMP bring-up (slice 1).
    fn cpu_count() -> usize;

    /// The dense logical index (`0..cpu_count()`) of the CPU this runs on — the
    /// primitive neutral code uses to index per-CPU state (`CPUS[current_cpu()]`).
    /// Slice 0 has one CPU, so this is `0`, but it is read from the hardware (not
    /// hard-coded) so it stays correct once APs run.
    fn current_cpu() -> u32;

    /// Establish this CPU's logical `index` so [`current_cpu`](ArchSmp::current_cpu)
    /// reports it. Called once per CPU during its own early init (the BSP with `0`
    /// at boot; each AP with its index at SMP bring-up). On x86 this programs
    /// `IA32_TSC_AUX`, which `RDTSCP` reads back; it affects only that returned id,
    /// not the timestamp counter.
    fn init_this_cpu(index: u32);

    /// Send an inter-processor interrupt of `vector` to CPU `target`.
    ///
    /// # Safety
    /// Ring-0 only; valid only on a multi-CPU system (Phase 3). On the
    /// single-CPU Phase-1 kernel there is no second CPU to target, so calling
    /// this is a logic error (the stub panics).
    unsafe fn send_ipi(target: u32, vector: u8);
}
