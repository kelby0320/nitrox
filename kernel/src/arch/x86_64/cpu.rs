//! x86_64 CPU control and feature detection ([`ArchCpu`] impl): the GDT/IDT
//! install, the boot-time memory-protection enables (NX, SMEP, SMAP), the
//! trap kernel-stack setter, halting, and CPUID feature queries.

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

    fn halt_loop() -> ! {
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

    unsafe fn interrupts_restore(prev: bool) {
        if prev {
            // SAFETY: ring-0; restoring a previously-enabled interrupt state.
            unsafe { regs::sti() };
        }
        // else: leave IF clear — it already is.
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
         (see docs/history/decision-log.md). Under QEMU use \
         `-cpu qemu64,+smap,+smep`."
    );
    assert!(
        ebx & CPUID_7_0_EBX_SMAP != 0,
        "SMAP not supported by this CPU — Phase 1 requires SMEP/SMAP \
         (see docs/history/decision-log.md). Under QEMU use \
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
