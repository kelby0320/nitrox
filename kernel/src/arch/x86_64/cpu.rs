//! x86_64 CPU control and feature detection ([`ArchCpu`] impl): the GDT/IDT
//! install, the boot-time memory-protection enables (NX, SMEP, SMAP), the
//! trap kernel-stack setter, halting, and CPUID feature queries.

use core::sync::atomic::Ordering;
use core::arch::asm;

use crate::arch::cpu::ArchCpu;
use crate::arch::x86_64::{gdt, idt, regs};

/// CPUID.01H:EDX bit 9 — on-chip local APIC present.
const CPUID_1_EDX_APIC: u32 = 1 << 9;

/// `RFLAGS` bit 9 — the interrupt-enable flag (`IF`).
const RFLAGS_IF: u64 = 1 << 9;

/// The Extended Feature Enable Register MSR.
const MSR_EFER: u32 = 0xC000_0080;
/// `EFER` bit 11 — no-execute enable.
const EFER_NXE: u64 = 1 << 11;

/// CR4.SMEP — supervisor mode execution prevention. With this bit set,
/// instruction fetches from user pages while in ring 0 `#PF`.
const CR4_SMEP: u64 = 1 << 20;
/// CR4.SMAP — supervisor mode access prevention. With this bit set, data
/// accesses to user pages while in ring 0 `#PF` unless EFLAGS.AC is set
/// (via `stac`).
const CR4_SMAP: u64 = 1 << 21;

/// CPUID 7.0:EBX bit 7 — SMEP supported.
const CPUID_7_0_EBX_SMEP: u32 = 1 << 7;
/// CPUID 7.0:EBX bit 20 — SMAP supported.
const CPUID_7_0_EBX_SMAP: u32 = 1 << 20;

/// The x86_64 [`ArchCpu`] implementation. Zero-sized; re-exported as
/// `crate::arch::Cpu`.
pub struct X86Cpu;

impl ArchCpu for X86Cpu {
    fn init_tables() {
        // The GDT (with its TSS) must come before the IDT: the IDT's gates
        // reference the kernel code selector the GDT installs, and the
        // double-fault gate needs the TSS's IST stack.
        gdt::init();
        idt::init();
    }

    fn init_protections() {
        ensure_nxe();
        ensure_smap_smep();
    }

    fn set_kernel_stack(top: u64) {
        gdt::set_kernel_stack(top);
    }

    fn stop_the_machine() -> ! {
        // **Mask first, and run to completion on one CPU.** This inherits the caller's IF:
        // from `dump_and_halt` that is already 0 (the IDT gate clears it), but from `panic!`
        // it is whatever the panicking context held, and IF=1 in ring 0 is reachable —
        // `tlb::shootdown` spins for acknowledgements with interrupts *deliberately* enabled,
        // and the idle and boot threads run with IF=1. A tick landing inside the send loop can
        // deschedule this thread (`on_timer_tick` calls `switch_to_next` straight out of the
        // handler), leaving `STOPPING` latched, no NMI sent and the machine running — and if
        // the thread resumes on a *different* CPU, the `me` captured below names the old one,
        // so the loop would NMI the core it is now running on and take itself out partway
        // through the scan. Masking makes the whole sequence uninterruptible.
        //
        // SAFETY: ring 0. This diverges into `halt_loop`, so nothing later depends on IF
        // being restored.
        unsafe {
            <Self as ArchCpu>::interrupts_disable();
        }

        // **Only if this CPU can actually send.** An AP that has not run `ap_cpu_init` has not
        // entered x2APIC mode, and writing the ICR MSR there `#GP`s — inside the panic path,
        // which would fault while handling a fault. Early boot is precisely where that state
        // occurs, and precisely where panics are most likely, so this is the common case
        // rather than a corner: such a core prints, halts, and lets whoever is watching it
        // (today the BSP's AP-online deadline) take the machine down.
        if super::apic::x2apic_enabled_here() {
            // **Announce inside the branch**, so "STOPPING is set" and "NMIs are coming" are
            // the same statement. Stored before the sends, because a target that takes the
            // NMI first would read the flag clear and treat the notice as a hardware NMI —
            // which on a ring-0 CPU means `dump_and_halt`, the register-dump storm this
            // exists to avoid. Stored *only* here because the fallback below sends nothing:
            // latching it there would leave a flag that silently converts any genuine
            // hardware NMI, on any core, into an undiagnosed halt.
            super::idt::STOPPING.store(true, Ordering::Release);
            let me = super::smp::hw_apic_id();
            for cpu in 0..crate::arch::MAX_CPUS {
                if crate::sched::online_mask() & (1u64 << cpu) == 0 {
                    continue;
                }
                let Some(apic) = super::smp::apic_of_dense(cpu) else {
                    continue;
                };
                if apic == me {
                    continue;
                }
                // SAFETY: ring 0, x2APIC enabled on this CPU (checked above); `apic` came
                // from the identity map, so it names a core that was brought up.
                unsafe { super::apic::send_nmi(apic) };
            }
        }
        <Self as ArchCpu>::halt_loop()
    }

    fn halt_loop() -> ! {
        // **Leave the online set first.** This CPU is about to stop servicing
        // interrupts for good, and a CPU counted online but unable to acknowledge a
        // TLB-shootdown IPI deadlocks every later shootdown on the machine — see
        // `sched::leave_online`, which carries the failure this fixed.
        crate::sched::leave_online();
        loop {
            // SAFETY: `cli` and `hlt` are always valid in ring 0. Neither
            // touches memory; both are allowed under the kernel's lock
            // ordering since no locks are held at the call site.
            unsafe {
                asm!("cli", "hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    fn has_apic() -> bool {
        let (_, _, _, edx) = regs::cpuid(1, 0);
        edx & CPUID_1_EDX_APIC != 0
    }

    unsafe fn halt() {
        // SAFETY: `hlt` is a ring-0 instruction with no memory side effects;
        // it parks the CPU until the next interrupt. The caller owns the
        // interrupt-flag state that governs wake-up (see the trait contract).
        unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }

    unsafe fn idle_halt() {
        // SAFETY: ring-0. `sti; hlt` is the canonical idle idiom: `sti` enables
        // interrupts but its one-instruction interrupt shadow defers delivery
        // until *after* the `hlt` retires, so the CPU is guaranteed to park with
        // IF=1 (wakeable by the periodic timer / a reschedule IPI) yet cannot miss
        // a wake that races the enable. Unlike bare `halt`, it does not trust the
        // caller's inbound IF — an idle CPU that parked with IF=0 would sleep
        // forever, since a maskable IRQ cannot resume `hlt` while IF=0.
        unsafe { asm!("sti; hlt", options(nomem, nostack, preserves_flags)) };
    }

    fn interrupts_enabled() -> bool {
        regs::read_rflags() & RFLAGS_IF != 0
    }

    unsafe fn interrupts_disable() -> bool {
        let was = Self::interrupts_enabled();
        // SAFETY: ring-0; the caller bounds the masked window (IrqSpinLock).
        unsafe { regs::cli() };
        was
    }

    unsafe fn interrupts_enable() {
        // SAFETY: ring-0; called at boot after the IDT + timer are live, and
        // by `interrupts_restore`.
        unsafe { regs::sti() };
    }

    /// Restore `IF` to `prev` — **set it either way**, rather than assuming the current
    /// state.
    ///
    /// The `else` arm used to be a no-op, commented "leave IF clear — it already is". That
    /// held for every caller that had reached here through `interrupts_disable`, and was
    /// false for the one that had enabled `IF` *itself*: `tlb::shootdown` unmasks for the
    /// duration of the IPI round-trip (it must — an `IF`-masked spinner cannot service a
    /// peer's shootdown IPI), then called this with the `prev = false` it captured on the way
    /// in. The no-op left interrupts **enabled** for the rest of the syscall, and its return
    /// through the `sysretq` stub's ring-0-on-user-stack window (PR #231 review, finding 2).
    ///
    /// Making it unconditional costs one `cli` on a cold path and removes an unstated
    /// precondition — the function now does what its name says regardless of how the caller
    /// got here.
    unsafe fn interrupts_restore(prev: bool) {
        if prev {
            // SAFETY: ring-0; restoring a previously-enabled interrupt state.
            unsafe { regs::sti() };
        } else {
            // SAFETY: ring-0; restoring a previously-masked interrupt state.
            unsafe { regs::cli() };
        }
    }
}

/// Enable the no-execute (NX) paging extension by setting `EFER.NXE`.
///
/// Until `EFER.NXE` is set, a page-table entry with the NX bit faults as a
/// reserved-bit violation. Limine enables long mode but does not guarantee
/// NXE, so the kernel sets it itself before any mapping uses
/// [`PageFlags::NO_EXECUTE`](crate::arch::paging::PageFlags::NO_EXECUTE).
/// Idempotent.
fn ensure_nxe() {
    // SAFETY: `MSR_EFER` is implemented on every x86_64 CPU. Reading it,
    // OR-ing in the NXE bit, and writing it back enables NX support without
    // disturbing any other EFER field (long-mode-enable, syscall-enable), so
    // the running kernel is unaffected.
    unsafe {
        let efer = regs::rdmsr(MSR_EFER);
        regs::wrmsr(MSR_EFER, efer | EFER_NXE);
    }
}

/// Enable SMEP and SMAP — the CPU-level "kernel can't accidentally touch user
/// memory" protections. Panics if either feature is missing on this CPU.
///
/// SMEP prevents the kernel fetching instructions from user pages (hardware
/// only). SMAP prevents the kernel reading/writing user data pages unless
/// EFLAGS.AC is set; the copy primitives in [`crate::arch::UserAccess`] open
/// the AC window with `stac` and close it with `clac` (inline-asm-only — no
/// Rust-visible wrappers, to enforce the "only inside copy routines"
/// discipline). Phase 1 hard-requires both; the dev loop runs QEMU with
/// `-cpu qemu64,+smap,+smep`. Idempotent.
fn ensure_smap_smep() {
    let (_, ebx, _, _) = regs::cpuid(7, 0);
    assert!(
        ebx & CPUID_7_0_EBX_SMEP != 0,
        "SMEP not supported by this CPU — Phase 1 requires SMEP/SMAP \
         (see docs/decision-log.md). Under QEMU use \
         `-cpu qemu64,+smap,+smep`."
    );
    assert!(
        ebx & CPUID_7_0_EBX_SMAP != 0,
        "SMAP not supported by this CPU — Phase 1 requires SMEP/SMAP \
         (see docs/decision-log.md). Under QEMU use \
         `-cpu qemu64,+smap,+smep`."
    );
    // SAFETY: both feature bits are present per the assertions above, so
    // setting CR4.SMEP|CR4.SMAP is architecturally defined: the CPU begins
    // enforcing the protections immediately. No other CR4 bits are touched,
    // so paging extensions and other features Limine configured remain
    // unchanged.
    unsafe {
        let cr4 = regs::read_cr4();
        regs::write_cr4(cr4 | CR4_SMEP | CR4_SMAP);
    }
}
