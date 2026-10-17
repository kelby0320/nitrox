//! Architecture-neutral symmetric-multiprocessing contract.
//!
//! [`ArchSmp`] is the neutral surface for CPU count, CPU identity and
//! inter-processor interrupts.
//!
//! `current_cpu()` is the neutral "which CPU am I" primitive: a **dense** logical
//! index in `0..MAX_CPUS`, used to index the per-CPU `CPUS[]` arrays. The
//! mechanism behind it is arch-internal (x86 reads `IA32_TSC_AUX` via `RDTSCP`,
//! set per CPU by `init_this_cpu`); see `docs/architecture/scheduler.md`
//! §Per-CPU access.

/// Upper bound on logical CPUs the kernel supports — sizes the per-CPU arrays
/// (both the arch `CpuLocal[]` and the neutral scheduler `CPUS[]`). Raising it is
/// a constant change. QEMU is exercised with `-smp 4`.
pub const MAX_CPUS: usize = 8;

/// Symmetric-multiprocessing queries and operations.
pub trait ArchSmp {
    /// Number of CPUs the kernel is driving. The x86_64 port still reports `1` on every CPU
    /// (`TODO(smp-cpu-count)`).
    fn cpu_count() -> usize;

    /// The dense logical index (`0..MAX_CPUS`) of the CPU this runs on — the
    /// primitive neutral code uses to index per-CPU state (`CPUS[current_cpu()]`).
    /// Read from the hardware, not hard-coded, so it is correct on every running CPU.
    fn current_cpu() -> u32;

    /// Whether [`current_cpu`](ArchSmp::current_cpu) is reporting an identity this CPU
    /// actually established, rather than a default.
    ///
    /// **`current_cpu` cannot answer this itself, and the difference is dangerous.** On x86
    /// the identity lives in `IA32_TSC_AUX`, whose reset default is `0` — so a CPU that has
    /// not run [`init_this_cpu`](ArchSmp::init_this_cpu) reports `0`, indistinguishable from
    /// a correctly-identified CPU 0. Any caller that would *act on another CPU's behalf* if
    /// it guessed wrong — clearing its bit in a mask, taking its slot — must ask here first.
    /// Callers that merely index per-CPU state defensively (the lock tracker folds an
    /// out-of-range id onto 0) do not need it.
    fn identity_bound() -> bool;

    /// Establish this CPU's logical `index` so [`current_cpu`](ArchSmp::current_cpu)
    /// reports it. Called once per CPU during its own early init (the BSP with `0`
    /// at boot; each AP with its index at SMP bring-up). On x86 this programs
    /// `IA32_TSC_AUX`, which `RDTSCP` reads back; it affects only that returned id,
    /// not the timestamp counter.
    fn init_this_cpu(index: u32);

    /// Send an inter-processor interrupt of `vector` to the CPU whose interrupt-controller id
    /// is `target` (on x86 its x2APIC id, not its dense index).
    ///
    /// # Safety
    /// Ring 0, with the local interrupt controller enabled on the calling CPU.
    unsafe fn send_ipi(target: u32, vector: u8);
}
