//! Architecture-neutral symmetric-multiprocessing contract.
//!
//! [`ArchSmp`] is the neutral surface for CPU count, CPU identity and
//! inter-processor interrupts.
//!
//! "Which CPU am I" is a **dense** logical index in `0..MAX_CPUS`, read by
//! [`CpuIndex::current`](crate::arch::percpu::CpuIndex::current) under a pin. The
//! mechanism behind it is arch-internal (x86 reads `IA32_TSC_AUX` via `RDTSCP`,
//! set as each CPU adopts its index); see `docs/architecture/scheduler.md`
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

    /// This CPU's index, unpinned, if this CPU established it: for one about to halt.
    ///
    /// **The raw read cannot answer this itself, and the difference is dangerous.** On x86
    /// the identity lives in `IA32_TSC_AUX`, whose reset default is `0` — so a CPU that has
    /// not adopted an index reports `0`, indistinguishable from
    /// a correctly-identified CPU 0. Any caller that would *act on another CPU's behalf* if
    /// it guessed wrong — clearing its bit in a mask, taking its slot — must ask here first.
    fn bound_index() -> Option<crate::arch::percpu::CpuIndex>;

    /// Bind [`CpuIndex::BSP`](crate::arch::percpu::CpuIndex::BSP) to this CPU and adopt it, as an
    /// AP adopts its own: refused on a second CPU. On x86 this programs
    /// `IA32_TSC_AUX`, which `RDTSCP` reads back; it affects only that returned id,
    /// not the timestamp counter.
    fn adopt_boot_index() -> crate::arch::percpu::CpuIndex;

    /// Send an inter-processor interrupt of `vector` to the CPU whose interrupt-controller id
    /// is `target` (on x86 its x2APIC id, not its dense index).
    ///
    /// # Safety
    /// Ring 0, with the local interrupt controller enabled on the calling CPU.
    unsafe fn send_ipi(target: u32, vector: u8);
}
