//! Architecture abstraction. Phase 0 implements only x86_64.
//!
//! ## The arch boundary (read before adding to this module)
//!
//! This module is the kernel's **only** architecture-neutral interface to
//! CPU- and platform-specific machinery. The architecture implementation
//! lives under a **private** submodule (`x86_64`, and eventually
//! `aarch64`); the rest of the kernel — everything outside
//! `kernel/src/arch/` — must reach it solely through the neutral names
//! re-exported here. Because the arch submodule is private,
//! `crate::arch::x86_64::…` does not compile outside `arch/`; the
//! `cargo xtask check-arch` lint additionally rejects any such reference
//! in comments or new code. See `docs/conventions/arch-boundary.md`.
//!
//! To expose a new arch operation: add a neutral function (or re-export a
//! neutral-named item) here, backed by an implementation in the active
//! architecture's private submodule. Do not surface x86 jargon
//! (`gdt`, `idt`, `cr3`, `rsp`, MSR names, …) in the names re-exported
//! here — wrap it in a neutral name (e.g. `set_kernel_stack`,
//! `init_syscalls`).
//!
//! [`paging`] is the one architecture-neutral *trait* module — it holds
//! the [`ArchPaging`](paging::ArchPaging) trait and its supporting types;
//! the active architecture's implementation is re-exported here as
//! [`Paging`].

pub mod cpu;
pub mod entropy;
pub mod irq;
pub mod irq_router;
pub mod paging;
pub mod platform;
pub mod registers;
pub mod smp;
pub mod timer;
pub mod user_access;

#[cfg(target_arch = "x86_64")]
mod x86_64;

// Neutral data/singleton modules defined at the x86_64 root. (CPU control —
// init/protections/kernel-stack/halt — now lives on the `Cpu` trait below;
// paging companions on `Paging`.)
#[cfg(target_arch = "x86_64")]
pub use x86_64::{abi, serial};

// Architecture-trait implementations, re-exported under neutral names (see
// `docs/conventions/arch-boundary.md`): one trait per divergent behavioural
// subsystem, mirroring `paging::ArchPaging` → `Paging`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::apic::XApic as Irq;
// The system interrupt router (the IOAPIC on x86; GIC distributor on aarch64) —
// distinct from `Irq`, the per-CPU local controller. See `arch/irq_router.rs`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::ioapic::X86IoApic as IrqRouter;
/// Install a PCI INTx interrupt (register an ISR + route a GSI) for an in-kernel
/// driver — a composite helper, not a router method. See
/// [`x86_64::ioapic::install_pci_irq`] (and the TODO there to promote the
/// device-interrupt family into its own trait when MSI/teardown land).
#[cfg(target_arch = "x86_64")]
pub use x86_64::ioapic::install_pci_irq;
// Legacy *ISA* interrupt installation is **not** re-exported neutrally: "ISA" is an
// x86-only concept (ARM has no ISA IRQs), so surfacing it here would leak arch
// jargon (`docs/conventions/arch-boundary.md`). A fixed legacy platform device wires
// its own interrupt inside the arch layer — the PIT does (via `resolve_isa_irq`), and
// the serial console does via `arch::serial::console_arm_rx`. The neutral concept is
// "arm the console's RX interrupt", not "install ISA IRQ 4".
#[cfg(target_arch = "x86_64")]
pub use x86_64::cpu::X86Cpu as Cpu;
// Platform/firmware discovery (the x86 impl parses ACPI tables; aarch64 would
// parse a DTB). Exposes neutral facts only — the PCIe ECAM regions; the
// arch-specific interrupt-routing facts stay inside the arch layer. See
// `arch/platform.rs`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::acpi::X86Platform as Platform;
#[cfg(target_arch = "x86_64")]
pub use x86_64::smp::X86Smp as Smp;
/// Per-CPU architecture bring-up for an application processor (run on the AP).
#[cfg(target_arch = "x86_64")]
pub use x86_64::smp::ap_cpu_init;
/// APIC-id-based dense-index assignment: the BSP binds each dense index to a
/// hardware APIC id ([`bind_cpu_identity`]); each core adopts its own index by
/// matching its APIC id ([`adopt_dense_index`]), so indices are unique by
/// construction (no reliance on a handed-off value that could be stale/colliding).
#[cfg(target_arch = "x86_64")]
pub use x86_64::smp::{adopt_dense_index, bind_cpu_identity};
// TLB-shootdown transport: send the shootdown IPI to a dense CPU index, and the
// vector it uses. The architecture-neutral coordinator lives in `crate::tlb`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::tlb::{TLB_SHOOTDOWN_VECTOR, send_shootdown_ipi};
// `MAX_CPUS` (neutral) sizes the per-CPU arrays; re-exported as `crate::arch::MAX_CPUS`.
pub use smp::MAX_CPUS;
// `Timer` here is the *hardware* timer (monotonic time + the per-CPU countdown
// timer); distinct from the future `crate::object::Timer` waitable kernel
// object. See `arch/timer.rs`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::timer::X86Timer as Timer;
#[cfg(target_arch = "x86_64")]
pub use x86_64::user_access::X86UserAccess as UserAccess;
// Hardware-entropy source (the x86 impl uses RDSEED/RDRAND; aarch64 would use
// RNDR / SMCCC TRNG). See `arch/entropy.rs`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::entropy::X86Entropy as Entropy;

#[cfg(target_arch = "x86_64")]
pub use x86_64::context::{ArchThreadContext, context_switch, fabricate_frame, thread_trampoline};

// Syscall fast-path: arm it once at boot, set the per-thread kernel stack on
// switch-in, and descend to ring 3 from a scheduled user thread.
#[cfg(target_arch = "x86_64")]
pub use x86_64::syscall::{
    arm_user_entry_cpu_base, enter_user, init_syscall_entry, set_syscall_kernel_stack,
};

#[cfg(target_arch = "x86_64")]
pub use x86_64::paging::X86Paging as Paging;

// Suspended-thread register snapshot (behind `sys_thread_get_registers`): the
// `ArchRegisters` trait's active impl + its `#[repr(C)]` ABI value type. The
// `impl` lives in `idt.rs` (where the private exception frame is); this module
// owns the type + marker. See `arch::registers`.
#[cfg(target_arch = "x86_64")]
pub use x86_64::registers::{RegisterValues, X86Registers as Registers};
