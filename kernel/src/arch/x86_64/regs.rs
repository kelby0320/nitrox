//! Raw x86_64 hardware-register access: I/O ports, control registers,
//! and model-specific registers.
//!
//! Per `kernel/CLAUDE.md`, port I/O and hardware-register reads/writes
//! live behind wrapper functions in this module rather than as `asm!`
//! calls scattered through the codebase. The diagnostics slice needs the
//! port primitives (the 16550 UART speaks port I/O) and a `CR2` read (the
//! page-fault handler reports the faulting linear address from it). The
//! paging slice adds `CR3` access, `invlpg`, and MSR read/write — the
//! page-table root, single-page TLB invalidation, and `EFER.NXE`. The
//! timekeeping slice adds `rdtsc` — the cycle counter the monotonic clock
//! reads.

use core::arch::asm;

/// Write a byte to I/O port `port`.
///
/// # Safety
/// Port I/O has arbitrary, device-specific side effects. The caller must
/// own `port` and ensure the write is meaningful for the device behind it.
#[inline]
pub unsafe fn outb(port: u16, val: u8) {
    // SAFETY: `out dx, al` writes `al` to the I/O port named by `dx`. The
    // caller upholds the device-level contract. `nomem`/`preserves_flags`
    // hold: the instruction touches no memory and no arithmetic flags.
    unsafe {
        asm!("out dx, al", in("dx") port, in("al") val,
             options(nomem, nostack, preserves_flags));
    }
}

/// Read a byte from I/O port `port`.
///
/// # Safety
/// See [`outb`] — the caller must own `port`.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let val: u8;
    // SAFETY: `in al, dx` reads the I/O port named by `dx` into `al`. The
    // caller owns the port; the instruction touches no memory or flags.
    unsafe {
        asm!("in al, dx", out("al") val, in("dx") port,
             options(nomem, nostack, preserves_flags));
    }
    val
}

// Word (16-bit) and doubleword (32-bit) port I/O (`outw`/`inw`/`outl`/`inl`)
// were removed when the arch boundary was made private: only the byte
// variants are used today (the 16550 serial driver). Re-add the wider
// variants here when a device driver needs them.

/// Read control register `CR2` — the linear address of the most recent
/// page fault.
///
/// Safe: reading `CR2` has no side effects and is always valid in ring 0,
/// which is the only ring the kernel runs in.
#[inline]
pub fn read_cr2() -> u64 {
    let val: u64;
    // SAFETY: `mov reg, cr2` reads CR2 into a general register. It has no
    // side effects, touches no normal memory, and leaves flags untouched.
    unsafe {
        asm!("mov {}, cr2", out(reg) val,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Read control register `CR4` — the bag of feature-enable bits for
/// paging extensions, user-access protections, and others.
///
/// Safe: reading `CR4` has no side effects and is always valid in ring 0.
#[inline]
pub fn read_cr4() -> u64 {
    let val: u64;
    // SAFETY: `mov reg, cr4` reads CR4 into a general register. No
    // memory side effects, no flag changes.
    unsafe {
        asm!("mov {}, cr4", out(reg) val,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Write control register `CR4`.
///
/// # Safety
/// CR4 controls fundamental CPU features (paging extensions,
/// user-access protections, performance counters, virtualisation
/// gates). Clearing a bit that the running kernel depends on
/// (e.g. PAE, PSE) is undefined; setting a bit whose feature the
/// CPU does not implement `#GP`s. The caller must ensure both.
#[inline]
pub unsafe fn write_cr4(value: u64) {
    // SAFETY: `mov cr4, reg` installs the new control bits. The
    // caller upholds the feature-bit contract. `nomem` is omitted
    // because flipping CR4 bits changes how subsequent accesses are
    // interpreted.
    unsafe {
        asm!("mov cr4, {}", in(reg) value,
             options(nostack, preserves_flags));
    }
}

/// Execute `cpuid` with `leaf` in `EAX` and `subleaf` in `ECX`,
/// returning `(eax, ebx, ecx, edx)`.
///
/// Safe in ring 0: `cpuid` has no memory side effects and touches no
/// arithmetic flags. The leaf must be one the CPU actually
/// implements; querying an unsupported leaf yields zeros rather than
/// faulting (this is the architectural contract since the original
/// Pentium).
#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    // SAFETY: `cpuid` reads `eax`/`ecx` for input and writes
    // `eax`/`ebx`/`ecx`/`edx` for output. No memory side effects, no
    // flag changes. LLVM reserves `rbx` for its own use and refuses
    // to accept it as a register operand, so we route the cpuid
    // result through a different register via `xchg`: save the
    // kernel's rbx into `tmp`, run cpuid (clobbers rbx with the
    // result), swap again so `tmp` holds the result and rbx is
    // restored. The compiler then reads `tmp` as `ebx`.
    unsafe {
        asm!(
            "xchg rbx, {tmp:r}",
            "cpuid",
            "xchg rbx, {tmp:r}",
            tmp = lateout(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") subleaf => ecx,
            lateout("edx") edx,
            options(nomem, nostack, preserves_flags),
        );
    }
    (eax, ebx, ecx, edx)
}

// `stac` and `clac` are deliberately not Rust-visible wrappers. They
// would be `unsafe fn` callable from anywhere in the kernel, which
// breaks the project's "only inside copy primitives" SMAP discipline
// (kernel/CLAUDE.md). The instructions are emitted directly inside
// the copy primitives' inline asm in `arch::x86_64::user_access`,
// where they are bracketed by the exception-table window and never
// outlive it.

/// Read control register `CR3` — the physical base of the active
/// top-level page table in bits 51:12, plus its low control flags.
///
/// Safe: reading `CR3` has no side effects and is always valid in ring 0.
#[inline]
pub fn read_cr3() -> u64 {
    let val: u64;
    // SAFETY: `mov reg, cr3` reads CR3 into a general register. It has no
    // side effects, touches no normal memory, and leaves flags untouched.
    unsafe {
        asm!("mov {}, cr3", out(reg) val,
             options(nomem, nostack, preserves_flags));
    }
    val
}

/// Load control register `CR3`, switching the active page-table root.
/// Writing `CR3` also flushes every non-global TLB entry.
///
/// # Safety
/// `value` must hold the physical base of a fully-formed top-level page
/// table (see [`crate::arch::paging::ArchPaging::set_page_table`]).
/// Loading an incomplete table triple-faults the CPU instantly.
#[inline]
pub unsafe fn write_cr3(value: u64) {
    // SAFETY: `mov cr3, reg` installs `value` as the page-table root; the
    // caller guarantees it names a valid table. The instruction touches
    // no arithmetic flags. `nomem` is omitted: the write changes how
    // every subsequent memory access is translated.
    unsafe {
        asm!("mov cr3, {}", in(reg) value,
             options(nostack, preserves_flags));
    }
}

/// Invalidate the TLB entry for the page containing linear address
/// `virt` on the current CPU.
///
/// # Safety
/// `invlpg` is valid only in ring 0, which is the only ring the kernel
/// runs in. The caller should already have updated the page tables so
/// the invalidation reflects a real change.
#[inline]
pub unsafe fn invlpg(virt: u64) {
    // Host unit tests run in ring 3, where `invlpg` would `#GP`. The kernel
    // proper always runs in ring 0; the instruction only evicts a TLB entry,
    // which a host test (no MMU state of its own) cannot observe, so a test
    // build elides it. This lets the demand-paging fault-in path — which must
    // flush after installing a PTE — be exercised host-side.
    #[cfg(test)]
    let _ = virt;
    #[cfg(not(test))]
    // SAFETY: `invlpg [mem]` invalidates the TLB entry for the addressed
    // page; it is a ring-0 instruction with no flag effects. `nomem` is
    // omitted because it has memory-ordering semantics over the page
    // tables; the operand is consumed as an address, not dereferenced.
    unsafe {
        asm!("invlpg [{}]", in(reg) virt,
             options(nostack, preserves_flags));
    }
}

/// Read `RFLAGS`.
///
/// Safe in ring 0: `pushfq` then a pop snapshots the flags into a GPR with
/// no lasting side effect (the push/pop pair is balanced). Used to read the
/// interrupt-enable bit (`IF`, bit 9) for the `IrqSpinLock` save/restore.
#[inline]
pub fn read_rflags() -> u64 {
    let val: u64;
    // SAFETY: `pushfq; pop reg` reads RFLAGS into `val`. It uses the stack
    // (balanced push+pop), so `nostack` is omitted; we must read the flags,
    // so `preserves_flags` is omitted too. No memory we model is touched.
    unsafe {
        asm!("pushfq", "pop {}", out(reg) val, options(nomem));
    }
    val
}

/// Clear the interrupt flag (`cli`) — mask maskable interrupts on this CPU.
///
/// # Safety
/// Ring-0 only. Leaving interrupts masked indefinitely stalls preemption;
/// callers must bound the masked window (see [`crate::libkern::IrqSpinLock`]).
#[inline]
pub unsafe fn cli() {
    // SAFETY: `cli` clears IF; ring-0 instruction, no memory effect. IF is not
    // one of the arithmetic flags `preserves_flags` tracks, so the option set
    // matches the `cli` in `cpu.rs::halt_loop`.
    unsafe {
        asm!("cli", options(nomem, nostack, preserves_flags));
    }
}

/// Set the interrupt flag (`sti`) — unmask maskable interrupts on this CPU.
///
/// # Safety
/// Ring-0 only. The IDT must be installed and a sane interrupt environment
/// established before enabling delivery.
#[inline]
pub unsafe fn sti() {
    // SAFETY: `sti` sets IF; ring-0 instruction, no memory effect. See `cli`
    // re: `preserves_flags`.
    unsafe {
        asm!("sti", options(nomem, nostack, preserves_flags));
    }
}

/// Read the Time-Stamp Counter — a 64-bit cycle counter that increments
/// monotonically with the core clock (the value is returned across
/// `edx:eax`).
///
/// Safe in ring 0: `rdtsc` has no memory side effects and touches no
/// arithmetic flags. It is **not** serializing — the CPU may reorder it a
/// few instructions either way. That is acceptable for the monotonic wall
/// clock built on it (a handful of cycles of skew is far below the clock's
/// nanosecond resolution); a caller needing an exact instruction-boundary
/// timestamp would use a serializing variant, which the kernel does not
/// yet need.
#[inline]
pub fn rdtsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdtsc` reads the counter into `edx:eax`. No memory side
    // effects, no flag changes; valid in ring 0.
    unsafe {
        asm!("rdtsc", out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

/// Read model-specific register `msr`.
///
/// # Safety
/// `rdmsr` is a ring-0 instruction and `#GP`s on an MSR index the CPU
/// does not implement. The caller must pass a valid index.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdmsr` reads the MSR named by `ecx` into `edx:eax`. The
    // caller guarantees `msr` is implemented. It touches no normal
    // memory and no arithmetic flags.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags));
    }
    ((high as u64) << 32) | (low as u64)
}

/// Write `value` to model-specific register `msr`.
///
/// # Safety
/// `wrmsr` is a ring-0 instruction that can change fundamental CPU state
/// (paging mode, NX-enable, the `syscall` target). The caller must pass
/// a valid index and a value that is sound for the running kernel.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    let low = value as u32;
    let high = (value >> 32) as u32;
    // SAFETY: `wrmsr` writes `edx:eax` to the MSR named by `ecx`. The
    // caller upholds the index/value contract. No arithmetic flags are
    // touched.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") low, in("edx") high,
             options(nomem, nostack, preserves_flags));
    }
}
