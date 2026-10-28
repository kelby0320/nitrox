//! x86_64 CPU control and feature detection ([`ArchCpu`] impl): GDT/IDT install, the NX, SMEP
//! and SMAP enables, the trap kernel-stack setter, halting, and CPUID queries.

use core::arch::asm;
use core::sync::atomic::Ordering;

use crate::arch::cpu::ArchCpu;
use crate::arch::x86_64::{gdt, idt, regs};

const CPUID_1_EDX_APIC: u32 = 1 << 9;

const RFLAGS_IF: u64 = 1 << 9;

const MSR_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

/// CR4.SMEP: ring-0 instruction fetches from user pages `#PF`.
const CR4_SMEP: u64 = 1 << 20;
/// CR4.SMAP: ring-0 data accesses to user pages `#PF` unless EFLAGS.AC is set (`stac`).
const CR4_SMAP: u64 = 1 << 21;

const CPUID_7_0_EBX_SMEP: u32 = 1 << 7;
const CPUID_7_0_EBX_SMAP: u32 = 1 << 20;

/// The x86_64 [`ArchCpu`] implementation; re-exported as `crate::arch::Cpu`.
pub struct X86Cpu;

impl ArchCpu for X86Cpu {
    fn init_tables() {
        // The IDT's gates reference the kernel code selector the GDT installs, and the
        // double-fault gate needs the TSS's IST stack, so the GDT comes first.
        gdt::init();
        idt::init();
    }

    fn init_protections() {
        ensure_nxe();
        ensure_smap_smep();
    }

    fn set_kernel_stack(top: crate::mm::kstack::KernelStackTop) {
        gdt::set_kernel_stack(top.get());
    }

    fn stop_the_machine() -> ! {
        <Self as ArchCpu>::stop_other_cpus();
        // The screen comes back last, on both of `stop_other_cpus`' branches: after every
        // other CPU is told to halt, so a compositor stops drawing over it, and after the
        // diagnosis printed. A panic before `init` lands in the fallback branch.
        crate::fbcon::reclaim_for_stop();
        <Self as ArchCpu>::halt_loop()
    }

    fn stop_other_cpus() {
        // Mask first, and run to completion on one CPU. This inherits the caller's IF, which
        // from `panic!` can be 1 in ring 0 (`tlb::shootdown` and the idle and boot threads run
        // with IF=1). A tick inside the send loop can deschedule this thread
        // (`on_timer_tick` calls `switch_to_next` from the handler), leaving `STOPPING` latched
        // with no NMI sent; resuming on another CPU, `me` below would name the old one and
        // the loop would NMI its own core. Masking makes the sequence uninterruptible.
        //
        // SAFETY: ring 0. The caller goes on to halt or reset, so nothing later depends on IF
        // being restored.
        unsafe {
            <Self as ArchCpu>::interrupts_disable();
        }

        // Only if this CPU can send: an AP that has not run `ap_cpu_init` has not entered
        // x2APIC mode, and the ICR write there `#GP`s inside the panic path. Early boot is
        // where panics are likeliest, so such a core prints, halts, and lets whoever watches
        // it (the BSP's AP-online deadline) take the machine down.
        if super::apic::x2apic_enabled_here() {
            // Announce inside the branch, so "STOPPING is set" and "NMIs are coming" mean the
            // same. Stored before the sends: a target taking the NMI first would read the flag
            // clear and treat it as a hardware NMI, which on a ring-0 CPU means
            // `dump_and_halt`. Not stored in the fallback, which sends nothing: a latched flag
            // there would turn any genuine hardware NMI into an undiagnosed halt.
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
    }

    fn halt_loop() -> ! {
        // Leave the online set first: a CPU counted online that cannot acknowledge a
        // TLB-shootdown IPI deadlocks every later shootdown (see `sched::leave_online`).
        crate::sched::leave_online();
        loop {
            // SAFETY: `cli` and `hlt` are always valid in ring 0, and allowed under the kernel's
            // lock ordering since no locks are held at the call site.
            unsafe {
                asm!("cli", "hlt", options(nostack, preserves_flags));
            }
        }
    }

    fn has_apic() -> bool {
        let (_, _, _, edx) = regs::cpuid(1, 0);
        edx & CPUID_1_EDX_APIC != 0
    }

    unsafe fn halt() {
        // SAFETY: `hlt` is a ring-0 instruction; it parks the CPU until the next interrupt,
        // whose handler may change memory, hence not `nomem`. The caller owns the
        // interrupt-flag state that governs wake-up (see the trait contract).
        unsafe { asm!("hlt", options(nostack, preserves_flags)) };
    }

    unsafe fn idle_halt() {
        // SAFETY: ring-0. `sti; hlt`: `sti`'s one-instruction interrupt shadow defers delivery
        // until after the `hlt` retires, so the CPU parks with IF=1 yet cannot miss a wake
        // racing the enable. Unlike bare `halt` it does not trust the caller's IF: a CPU that
        // parked with IF=0 would sleep forever.
        unsafe { asm!("sti; hlt", options(nostack, preserves_flags)) };
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

    /// Restore `IF` to `prev`, setting it either way rather than assuming the current state.
    ///
    /// `tlb::shootdown` enables `IF` itself for the IPI round-trip, so a caller can arrive here
    /// with `prev = false` and `IF` set; a no-op `else` arm would leave interrupts enabled for
    /// the rest of the syscall, including the `sysretq` stub's ring-0-on-user-stack window.
    unsafe fn interrupts_restore(prev: bool) {
        if prev {
            // SAFETY: ring-0; restoring a previously-enabled interrupt state.
            unsafe { regs::sti() };
        } else {
            // SAFETY: ring-0; restoring a previously-masked interrupt state.
            unsafe { regs::cli() };
        }
    }

    fn log_identity() {
        log_identity();
    }
}

/// Enable the no-execute (NX) paging extension by setting `EFER.NXE`.
///
/// Until `EFER.NXE` is set, a page-table entry with the NX bit faults as a reserved-bit
/// violation. Limine does not guarantee it, so this runs before any mapping uses
/// [`PageFlags::NO_EXECUTE`](crate::arch::paging::PageFlags::NO_EXECUTE). Idempotent.
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

/// Enable SMEP and SMAP, panicking if either is missing on this CPU. Idempotent.
///
/// SMEP stops the kernel fetching instructions from user pages. SMAP stops it touching user
/// data pages unless EFLAGS.AC is set; the copy primitives in [`crate::arch::UserAccess`]
/// open that window with `stac` and close it with `clac`, which have no Rust-visible wrappers
/// so they stay inside the copy routines. The dev loop runs QEMU with `-cpu qemu64,+smap,+smep`.
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

// --- What this processor is ---

/// `(family, model, stepping)` from `CPUID.01H:EAX`, with the extended fields folded in as the
/// SDM specifies: the extended family is added only when the base family is `0xF`, and the
/// extended model is prefixed only for base families `0x6` and `0xF`. Reading the base fields
/// alone names every modern Intel part "model 0xe".
fn decode_signature(eax: u32) -> (u32, u32, u32) {
    let stepping = eax & 0xF;
    let base_model = (eax >> 4) & 0xF;
    let base_family = (eax >> 8) & 0xF;
    let family = if base_family == 0xF { base_family + ((eax >> 20) & 0xFF) } else { base_family };
    let model =
        if base_family == 0x6 || base_family == 0xF { ((eax >> 16) & 0xF) << 4 | base_model } else { base_model };
    (family, model, stepping)
}

/// The brand string's 48 bytes from the three `CPUID.8000_0002H..=8000_0004H` results, each
/// register little-endian in `EAX, EBX, ECX, EDX` order.
fn brand_bytes(leaves: [(u32, u32, u32, u32); 3]) -> [u8; 48] {
    let mut out = [0u8; 48];
    for (i, (a, b, c, d)) in leaves.into_iter().enumerate() {
        for (j, reg) in [a, b, c, d].into_iter().enumerate() {
            let at = (i * 4 + j) * 4;
            out[at..at + 4].copy_from_slice(&reg.to_le_bytes());
        }
    }
    out
}

/// The brand string without the leading spaces some Intel parts right-justify it with.
/// Trailing padding and the NUL are [`Printable`](crate::libkern::printable::Printable)'s.
fn brand_trimmed(brand: &[u8; 48]) -> &[u8] {
    let start = brand.iter().position(|&b| b != b' ').unwrap_or(brand.len());
    &brand[start..]
}

/// A group of features as a log line shows it: ` +name` present, ` -name` absent.
struct Features<'a>(&'a [(&'static str, bool)]);

impl core::fmt::Display for Features<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for &(name, present) in self.0 {
            write!(f, " {}{}", if present { '+' } else { '-' }, name)?;
        }
        Ok(())
    }
}

/// Logical processors in this CPU's package: the core level of `CPUID.0BH` where the part has
/// one, else `CPUID.01H:EBX[23:16]` when HTT says that field is meaningful, else one.
fn logical_per_package(max_leaf: u32, ebx1: u32, edx1: u32) -> u32 {
    const CPUID_0B_LEVEL_CORE: u32 = 2;
    if max_leaf >= 0xB {
        for sub in 0..8 {
            let (_, ebx, ecx, _) = regs::cpuid(0xB, sub);
            match (ecx >> 8) & 0xFF {
                0 => break,
                CPUID_0B_LEVEL_CORE => return ebx & 0xFFFF,
                _ => {}
            }
        }
    }
    if edx1 & (1 << 28) != 0 { (ebx1 >> 16) & 0xFF } else { 1 }
}

/// [`ArchCpu::log_identity`] for x86_64: identity then features. A leaf is read only when the
/// maximum-leaf query says it exists, so an absent leaf reports its features absent.
fn log_identity() {
    let (max_leaf, vb, vc, vd) = regs::cpuid(0, 0);
    let mut vendor = [0u8; 12];
    vendor[0..4].copy_from_slice(&vb.to_le_bytes());
    vendor[4..8].copy_from_slice(&vd.to_le_bytes());
    vendor[8..12].copy_from_slice(&vc.to_le_bytes());
    let (eax1, ebx1, ecx1, edx1) = regs::cpuid(1, 0);
    let (family, model, stepping) = decode_signature(eax1);
    let ebx7 = if max_leaf >= 7 { regs::cpuid(7, 0).1 } else { 0 };
    let (max_ext, _, _, _) = regs::cpuid(0x8000_0000, 0);
    let edx_ext1 = if max_ext >= 0x8000_0001 { regs::cpuid(0x8000_0001, 0).3 } else { 0 };
    let edx_ext7 = if max_ext >= 0x8000_0007 { regs::cpuid(0x8000_0007, 0).3 } else { 0 };
    let brand = if max_ext >= 0x8000_0004 {
        brand_bytes([regs::cpuid(0x8000_0002, 0), regs::cpuid(0x8000_0003, 0), regs::cpuid(0x8000_0004, 0)])
    } else {
        [0u8; 48]
    };
    let hypervisor = ecx1 & (1 << 31) != 0;

    crate::kprintln!(
        "cpu: {} family {:#x} model {:#x} stepping {}, {} logical per package, \"{}\"{}",
        crate::libkern::printable::Printable(&vendor),
        family,
        model,
        stepping,
        logical_per_package(max_leaf, ebx1, edx1),
        crate::libkern::printable::Printable(brand_trimmed(&brand)),
        if hypervisor { ", under a hypervisor" } else { "" },
    );
    crate::kprintln!(
        "cpu: requires{}; uses{}; warns{}; reports{} (unused: the timer counts down)",
        Features(&[
            ("x2apic", ecx1 & (1 << 21) != 0),
            ("rdtscp", edx_ext1 & (1 << 27) != 0),
            ("nx", edx_ext1 & (1 << 20) != 0),
            ("smep", ebx7 & CPUID_7_0_EBX_SMEP != 0),
            ("smap", ebx7 & CPUID_7_0_EBX_SMAP != 0),
        ]),
        Features(&[
            ("xsave", ecx1 & (1 << 26) != 0),
            ("avx", ecx1 & (1 << 28) != 0),
            ("rdrand", ecx1 & (1 << 30) != 0),
            ("rdseed", ebx7 & (1 << 18) != 0),
        ]),
        Features(&[("invariant-tsc", edx_ext7 & (1 << 8) != 0)]),
        Features(&[("tsc-deadline", ecx1 & (1 << 24) != 0)]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_laptops_signature_needs_the_extended_model() {
        // The laptop's i5-7200U (Kaby Lake-U): CPUID.01H:EAX = 0x000806E9; base fields say 0xe.
        assert_eq!(decode_signature(0x0008_06E9), (6, 0x8E, 9));
    }

    #[test]
    fn family_0xf_adds_the_extended_family_and_others_ignore_it() {
        // AMD Zen+ (family 0x17): base family 0xF + extended 0x8.
        assert_eq!(decode_signature(0x0080_0F82), (0x17, 0x08, 2));
        // A base family below 0xF with junk in the extended-family field keeps its own.
        assert_eq!(decode_signature(0x0FF0_0663), (6, 6, 3));
        // A family-5 part does not get the extended model prefixed.
        assert_eq!(decode_signature(0x000F_0543), (5, 4, 3));
    }

    #[test]
    fn the_brand_string_reads_across_leaves_and_registers_in_order() {
        let text = *b"  Intel(R) Core(TM) i5-7200U CPU @ 2.50GHz\0\0\0\0\0\0";
        let reg = |at: usize| u32::from_le_bytes(text[at..at + 4].try_into().unwrap());
        let leaf = |i: usize| (reg(i * 16), reg(i * 16 + 4), reg(i * 16 + 8), reg(i * 16 + 12));
        let brand = brand_bytes([leaf(0), leaf(1), leaf(2)]);
        assert_eq!(brand, text);
        assert_eq!(
            format!("{}", crate::libkern::printable::Printable(brand_trimmed(&brand))),
            "Intel(R) Core(TM) i5-7200U CPU @ 2.50GHz"
        );
    }

    #[test]
    fn a_feature_group_marks_each_name_present_or_absent() {
        assert_eq!(format!("{}", Features(&[("nx", true), ("smap", false)])), " +nx -smap");
    }
}
