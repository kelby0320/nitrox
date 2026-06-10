//! Interrupt Descriptor Table: dump-and-halt handlers for the 32 CPU exception
//! vectors (with a recovery path for `#PF`), plus the hardware-IRQ vectors used
//! once interrupts are enabled — the periodic timer (`0x20`) and the local-APIC
//! spurious vector (`0xFF`).
//!
//! Exception handling splits by privilege: a **ring-0 (kernel)** fault prints
//! the faulting state and halts (`dump_and_halt`); a **ring-3 (user)** fault is
//! delivered to the faulting process as a `Notification` (`SegFault` /
//! `IllegalInsn` / `DivideByZero`) and the faulting thread is terminated, so the
//! kernel survives (see `sched::deliver_fault_and_exit` and
//! `docs/architecture/notifications.md`). The timer vector is a **returning**
//! stub (it `iretq`s back) that drives the preemptive scheduler; the spurious
//! vector just `iretq`s. After the preemptive-scheduling slice the kernel runs
//! with interrupts enabled (IF=1).
//!
//! `#PF` (vector 14) is the one exception with a recovery path: the
//! handler consults the user-memory-access exception table (see
//! [`crate::mm::user_access`]) and, on a match, patches the saved RIP
//! to the registered recovery PC and `iretq`s back to it. The first
//! consumer — the copy primitives in slice 2 — uses this to turn a
//! fault during a user-memory copy into a [`Result::Err`] instead of
//! a kernel halt. Faults that miss the table still dump-and-halt.
//!
//! ## Handler entry: naked stubs
//!
//! Rust's `x86-interrupt` calling convention is a nightly feature and is
//! forbidden by the project's stable-only rule. Instead each vector has a
//! naked-function stub ([`exception_stub!`]) that:
//!
//! 1. normalises the stack — for vectors that carry no CPU error code it
//!    pushes a dummy `0` so every vector yields the same frame layout;
//! 2. pushes the vector number and all 15 general-purpose registers,
//!    building an [`ExceptionFrame`] on the stack;
//! 3. calls [`exception_dispatch`] with a pointer to that frame.
//!
//! Vector 14 is built by hand ([`vec14`]) rather than from the macro:
//! its dispatcher is allowed to return when the fault is recoverable,
//! and the stub then unwinds the GPRs / vector / error code and
//! `iretq`s to the patched RIP. The other 31 stubs follow the macro
//! and end in `ud2` — their dispatcher is `-> !` and must not return.
//!
//! `#DF` runs on IST1 (a dedicated stack set up in `gdt.rs`) so it can
//! report even after a stack overflow.

use core::arch::asm;
use core::fmt::Write;

use crate::arch::Cpu;
use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::arch::x86_64::gdt::KERNEL_CODE_SELECTOR;
use crate::arch::x86_64::regs;
use crate::arch::x86_64::serial;
use crate::libkern::notification::{
    FaultKind, KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, Notification,
};

/// A 64-bit IDT gate descriptor (16 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const _: () = assert!(size_of::<IdtEntry>() == 16);

impl IdtEntry {
    /// An empty (not-present) gate.
    const EMPTY: IdtEntry = IdtEntry {
        offset_low: 0,
        selector: 0,
        ist: 0,
        type_attr: 0,
        offset_mid: 0,
        offset_high: 0,
        reserved: 0,
    };

    /// Point this gate at `handler`, running on IST slot `ist` (0 = none,
    /// i.e. use the stack the CPU was already on). Marks the gate present,
    /// DPL 0, 64-bit interrupt gate. Pure arithmetic — host-tested below.
    fn set_handler(&mut self, handler: u64, ist: u8) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = KERNEL_CODE_SELECTOR;
        self.ist = ist & 0x7;
        self.type_attr = 0x8E; // present, DPL 0, 64-bit interrupt gate
        self.reserved = 0;
    }
}

/// The operand of `lidt`: table byte-length minus one, then base address.
#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

/// The Interrupt Descriptor Table. 256 entries; only 0-31 are populated.
static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

/// The register state an exception stub builds on the stack.
///
/// Field order mirrors the push order, so the first field lies at the
/// lowest address — where the stub leaves RSP pointing and what
/// [`exception_dispatch`] receives.
#[repr(C)]
struct ExceptionFrame {
    // 15 general-purpose registers, pushed by the stub. `r15` is pushed
    // last, so it lies lowest — at the start of this struct.
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    // Vector number, pushed by the stub.
    vector: u64,
    // CPU error code — or 0, pushed by the stub for vectors without one.
    error_code: u64,
    // Pushed by the CPU on exception entry.
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

const _: () = assert!(size_of::<ExceptionFrame>() == 22 * 8);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, vector) == 15 * 8);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, error_code) == 16 * 8);
const _: () = assert!(core::mem::offset_of!(ExceptionFrame, rip) == 17 * 8);

/// Define a naked exception-entry stub.
///
/// The `noerr` form pushes a dummy `0` error code first; the `err` form
/// relies on the CPU-pushed error code. Both then push the vector number
/// and the 15 GPRs and call [`exception_dispatch`]. The trailing `ud2`
/// is unreachable (the dispatcher never returns) — a tripwire in case it
/// ever does.
macro_rules! exception_stub {
    (noerr, $name:ident, $vec:expr) => {
        exception_stub!(@build $name, $vec, "push 0\n");
    };
    (err, $name:ident, $vec:expr) => {
        exception_stub!(@build $name, $vec, "");
    };
    (@build $name:ident, $vec:expr, $errcode:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() -> ! {
            ::core::arch::naked_asm!(
                concat!(
                    $errcode,
                    "push ", stringify!($vec), "\n",
                    "push rax\npush rbx\npush rcx\npush rdx\n",
                    "push rsi\npush rdi\npush rbp\n",
                    "push r8\npush r9\npush r10\npush r11\n",
                    "push r12\npush r13\npush r14\npush r15\n",
                    "mov rdi, rsp\n",
                ),
                "call {dispatch}",
                "ud2",
                dispatch = sym exception_dispatch,
            );
        }
    };
}

exception_stub!(noerr, vec0, 0);
exception_stub!(noerr, vec1, 1);
exception_stub!(noerr, vec2, 2);
exception_stub!(noerr, vec3, 3);
exception_stub!(noerr, vec4, 4);
exception_stub!(noerr, vec5, 5);
exception_stub!(noerr, vec6, 6);
exception_stub!(noerr, vec7, 7);
exception_stub!(err, vec8, 8);
exception_stub!(noerr, vec9, 9);
exception_stub!(err, vec10, 10);
exception_stub!(err, vec11, 11);
exception_stub!(err, vec12, 12);
exception_stub!(err, vec13, 13);
// vec14 (#PF) is built by hand below — its dispatcher returns on a
// recoverable fault, so the stub must `iretq` rather than `ud2`.
exception_stub!(noerr, vec15, 15);
exception_stub!(noerr, vec16, 16);
exception_stub!(err, vec17, 17);
exception_stub!(noerr, vec18, 18);
exception_stub!(noerr, vec19, 19);
exception_stub!(noerr, vec20, 20);
exception_stub!(err, vec21, 21);
exception_stub!(noerr, vec22, 22);
exception_stub!(noerr, vec23, 23);
exception_stub!(noerr, vec24, 24);
exception_stub!(noerr, vec25, 25);
exception_stub!(noerr, vec26, 26);
exception_stub!(noerr, vec27, 27);
exception_stub!(noerr, vec28, 28);
exception_stub!(err, vec29, 29);
exception_stub!(err, vec30, 30);
exception_stub!(noerr, vec31, 31);

// --- #PF stub (vector 14) -----------------------------------------------
//
// Differs from the macro stubs in two places: (a) the CPU pushed an
// error code so we don't push a dummy zero, and (b) on a recoverable
// fault the dispatcher returns instead of halting, so the stub must
// unwind the GPRs / vector / error code and `iretq` to the patched
// RIP. Fatal faults are handled inside the dispatcher (it calls
// `dump_and_halt`, which is `-> !`), so the post-`call` instructions
// run only on the recovery path.

/// `#PF` (vector 14) entry stub. See the module doc for the recovery
/// contract.
#[unsafe(naked)]
extern "C" fn vec14() -> ! {
    ::core::arch::naked_asm!(
        // The CPU pushed a #PF error code; push the vector and the 15
        // GPRs to build a full `ExceptionFrame` on the stack, then pass
        // its address to the dispatcher.
        concat!(
            "push 14\n",
            "push rax\npush rbx\npush rcx\npush rdx\n",
            "push rsi\npush rdi\npush rbp\n",
            "push r8\npush r9\npush r10\npush r11\n",
            "push r12\npush r13\npush r14\npush r15\n",
            "mov rdi, rsp\n",
        ),
        "call {dispatch}",
        // Recovery path: the dispatcher patched `frame.rip` and
        // returned. Pop the GPRs, drop the vector + error-code slots
        // (16 bytes), and `iretq` to the patched RIP. The CPU-pushed
        // RIP/CS/RFLAGS/RSP/SS are popped by `iretq` itself.
        concat!(
            "pop r15\npop r14\npop r13\npop r12\n",
            "pop r11\npop r10\npop r9\npop r8\n",
            "pop rbp\npop rdi\npop rsi\n",
            "pop rdx\npop rcx\npop rbx\npop rax\n",
            "add rsp, 16\n",
            "iretq\n",
        ),
        dispatch = sym pf_dispatch,
    );
}

/// `#PF` dispatcher. Looks up the faulting RIP in the user-access
/// exception table; on a match, patches `frame.rip` to the recovery PC
/// and returns so the stub can `iretq` to it. On a miss, calls
/// [`dump_and_halt`] which never returns.
///
/// `*mut ExceptionFrame` rather than `*const` because the recovery
/// path writes back the new RIP in place. The frame lives on the
/// current stack (the stub built it with `push`es), and only this
/// thread can be using that stack region.
///
/// Reached only from [`vec14`] via `call`; `extern "C"` matches the
/// stub's `mov rdi, rsp` argument pass.
extern "C" fn pf_dispatch(frame: *mut ExceptionFrame) {
    // SAFETY: the naked stub built a complete `ExceptionFrame` at the
    // stack top and passed its address in RDI. It is valid, 8-byte
    // aligned, and not aliased for the duration of this call.
    let f = unsafe { &mut *frame };
    debug_assert_eq!(f.vector, 14);

    if let Some(recovery) = crate::mm::user_access::lookup_recovery(f.rip) {
        f.rip = recovery;
        return;
    }

    if is_user_fault(f) {
        // A genuine ring-3 page fault (not a kernel copy-primitive fault that
        // missed the recovery table): deliver SegFault + terminate. The CR2
        // read here is the faulting linear address. Never returns.
        let cr2 = regs::read_cr2();
        crate::sched::deliver_fault_and_exit(notif_for_vector(f, Some(cr2)));
    }

    dump_and_halt(f);
}

// --- Timer IRQ stub (vector 0x20) ---------------------------------------
//
// A *returning* stub (unlike the `-> !` exception stubs): the dispatcher EOIs
// and may reschedule, then control returns here to `iretq` back to the
// interrupted context — or, after a preemptive context switch, to whichever
// context last parked in this same stub. Runs with IF=0 (the interrupt gate
// clears it) and the dispatcher never `sti`s, so there is no nesting until the
// `iretq`. The frame layout matches [`ExceptionFrame`]; vectors ≥ 32 carry no
// CPU error code, so a dummy `0` fills that slot.

/// Timer IRQ (vector `0x20`) entry stub. See the module doc.
#[unsafe(naked)]
extern "C" fn timer_stub() {
    ::core::arch::naked_asm!(
        // Build a full `ExceptionFrame`: dummy error code, vector, 15 GPRs.
        concat!(
            "push 0\n",
            "push 0x20\n",
            "push rax\npush rbx\npush rcx\npush rdx\n",
            "push rsi\npush rdi\npush rbp\n",
            "push r8\npush r9\npush r10\npush r11\n",
            "push r12\npush r13\npush r14\npush r15\n",
            "mov rdi, rsp\n",
        ),
        "call {dispatch}",
        // The dispatcher returned: pop the GPRs, drop the vector + dummy
        // error-code slots (16 bytes), and `iretq` to the interrupted context
        // (restoring its RIP/CS/RFLAGS/RSP/SS, including IF).
        concat!(
            "pop r15\npop r14\npop r13\npop r12\n",
            "pop r11\npop r10\npop r9\npop r8\n",
            "pop rbp\npop rdi\npop rsi\n",
            "pop rdx\npop rcx\npop rbx\npop rax\n",
            "add rsp, 16\n",
            "iretq\n",
        ),
        dispatch = sym timer_dispatch,
    );
}

/// Spurious-interrupt (vector `0xFF`) entry stub. A spurious local-APIC
/// interrupt requires **no** EOI (the controller is telling us it had nothing
/// to deliver), and we touch no registers, so this just `iretq`s.
#[unsafe(naked)]
extern "C" fn spurious_stub() {
    ::core::arch::naked_asm!("iretq");
}

/// Timer IRQ dispatcher. Signals end-of-interrupt **first** — the handler may
/// switch away via [`crate::sched::on_timer_tick`] and not return to this frame
/// for a long time, so EOI-ing late would block all further timer delivery —
/// then drives the scheduler tick. Returns to [`timer_stub`], which `iretq`s.
///
/// Runs entirely with IF=0 (the interrupt gate), so it never nests and may take
/// the (interrupt-safe) run-queue lock inside `on_timer_tick`.
///
/// `*mut ExceptionFrame` matches the stub's `mov rdi, rsp`; the frame is read
/// only for future use (frame patching) and is otherwise unused today.
extern "C" fn timer_dispatch(_frame: *mut ExceptionFrame) {
    // SAFETY: ring-0, only ever reached from the timer IRQ after `Irq::init`
    // mapped the local APIC; a single MMIO write acknowledges the interrupt.
    unsafe { crate::arch::Irq::eoi() };
    crate::sched::on_timer_tick();
}

/// Entry stubs for CPU exception vectors 0-31, indexed by vector number.
const STUBS: [extern "C" fn() -> !; 32] = [
    vec0, vec1, vec2, vec3, vec4, vec5, vec6, vec7, vec8, vec9, vec10, vec11, vec12, vec13, vec14,
    vec15, vec16, vec17, vec18, vec19, vec20, vec21, vec22, vec23, vec24, vec25, vec26, vec27,
    vec28, vec29, vec30, vec31,
];

/// Short mnemonic for a CPU exception vector.
fn vector_name(vector: u64) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point",
        20 => "#VE virtualization",
        21 => "#CP control protection",
        29 => "#VC VMM communication",
        30 => "#SX security exception",
        _ => "reserved",
    }
}

/// Common handler for every CPU exception except `#PF`. A **ring-3** fault is
/// delivered to the faulting process as a [`Notification`] and the faulting
/// thread is terminated (the kernel survives); a **ring-0** fault is fatal
/// ([`dump_and_halt`]). Never returns either way.
///
/// Reached only from a naked stub via `call`; `extern "C"` matches the
/// stub's `mov rdi, rsp` argument pass.
extern "C" fn exception_dispatch(frame: *const ExceptionFrame) -> ! {
    // SAFETY: the naked stub built a complete `ExceptionFrame` at the
    // stack top and passed its address in RDI. It is valid, 8-byte
    // aligned, and not aliased for the duration of this call.
    let f = unsafe { &*frame };
    if is_user_fault(f) {
        // Deliver the fault as a notification + terminate the faulting thread.
        // `deliver_fault_and_exit` never returns (it switches away forever).
        crate::sched::deliver_fault_and_exit(notif_for_vector(f, None));
    }
    // Ring-0 (kernel) fault: unrecoverable — dump and halt.
    dump_and_halt(f);
}

/// `true` iff the saved frame is from ring 3 (user). The CPU stores the code
/// selector with its RPL in the low two bits; ring 3 is `cs & 3 == 3`.
fn is_user_fault(f: &ExceptionFrame) -> bool {
    f.cs & 3 == 3
}

/// Build the post-mortem [`Notification`] for a ring-3 fault. `cr2` carries the
/// faulting linear address for `#PF` (read from CR2); other vectors use the
/// faulting `rip`. Reads the current thread's tid for the notification's
/// `thread` field.
fn notif_for_vector(f: &ExceptionFrame, cr2: Option<u64>) -> Notification {
    let tid = crate::sched::current_tid();
    let (kind, addr, fault) = fault_shape(f.vector, f.error_code, f.rip, cr2);
    match kind {
        KIND_DIVIDE_BY_ZERO => Notification::divide_by_zero(tid, addr),
        KIND_ILLEGAL_INSN => Notification::illegal_insn(tid, addr),
        // KIND_SEG_FAULT and the generic fallback.
        _ => Notification::seg_fault(tid, addr, fault),
    }
}

/// Pure vector→notification-shape mapping (host-testable; no scheduler/CR2
/// access). Returns `(kind discriminant, fault address, FaultKind)`. `cr2` is
/// the #PF faulting address; other vectors use `rip`.
fn fault_shape(vector: u64, error_code: u64, rip: u64, cr2: Option<u64>) -> (u32, u64, FaultKind) {
    match vector {
        0 => (KIND_DIVIDE_BY_ZERO, rip, FaultKind::UnknownFault), // #DE
        6 => (KIND_ILLEGAL_INSN, rip, FaultKind::UnknownFault),   // #UD
        14 => (KIND_SEG_FAULT, cr2.unwrap_or(rip), pf_fault_kind(error_code)), // #PF
        // #GP/#SS/#NP/#AC and the rest: a protection/segmentation fault.
        _ => (KIND_SEG_FAULT, rip, FaultKind::UnknownFault),
    }
}

/// Decode a `#PF` error code (Intel SDM Vol.3 §4.7) into a [`FaultKind`].
/// bit0 P (0 = not present), bit1 W/R (1 = write), bit4 I/D (1 = insn fetch).
fn pf_fault_kind(error_code: u64) -> FaultKind {
    let present = error_code & 1 != 0;
    let write = error_code & (1 << 1) != 0;
    let insn = error_code & (1 << 4) != 0;
    if !present {
        FaultKind::NotMapped
    } else if insn {
        FaultKind::NotExecutable
    } else if write {
        FaultKind::NotWritable
    } else {
        FaultKind::NotReadable
    }
}

/// Dump the faulting state to the serial console and halt. Shared
/// between [`exception_dispatch`] (always) and [`pf_dispatch`] (only on
/// faults that miss the user-access exception table).
fn dump_and_halt(f: &ExceptionFrame) -> ! {
    // The emergency writer bypasses `SERIAL`'s lock: the fault may have
    // occurred while that lock was held. Sound under Phase 1's
    // single-CPU, interrupts-masked model.
    let mut w = serial::emergency_writer();

    let _ = writeln!(w, "\n*** CPU EXCEPTION ***");
    let _ = writeln!(w, "  vector  {:#04x}  {}", f.vector, vector_name(f.vector));
    let _ = writeln!(w, "  error   {:#018x}", f.error_code);
    if f.vector == 14 {
        // #PF: CR2 holds the faulting linear address.
        let _ = writeln!(w, "  cr2     {:#018x}", regs::read_cr2());
    }
    let _ = writeln!(w, "  rip {:#018x}  cs {:#06x}", f.rip, f.cs);
    let _ = writeln!(w, "  rsp {:#018x}  ss {:#06x}", f.rsp, f.ss);
    let _ = writeln!(w, "  rflags {:#018x}", f.rflags);
    let _ = writeln!(w, "  rax {:#018x}  rbx {:#018x}", f.rax, f.rbx);
    let _ = writeln!(w, "  rcx {:#018x}  rdx {:#018x}", f.rcx, f.rdx);
    let _ = writeln!(w, "  rsi {:#018x}  rdi {:#018x}", f.rsi, f.rdi);
    let _ = writeln!(w, "  rbp {:#018x}  r8  {:#018x}", f.rbp, f.r8);
    let _ = writeln!(w, "  r9  {:#018x}  r10 {:#018x}", f.r9, f.r10);
    let _ = writeln!(w, "  r11 {:#018x}  r12 {:#018x}", f.r11, f.r12);
    let _ = writeln!(w, "  r13 {:#018x}  r14 {:#018x}", f.r13, f.r14);
    let _ = writeln!(w, "  r15 {:#018x}", f.r15);
    let _ = writeln!(w, "halting.");

    Cpu::halt_loop()
}

/// Build and load the IDT. Call once, early in boot, after
/// [`crate::arch::x86_64::gdt::init`] (the gates reference the kernel
/// code selector, and `#DF` uses the TSS's IST1). Does not enable
/// interrupts.
pub fn init() {
    let base = &raw const IDT as usize as u64;
    let limit = (size_of::<[IdtEntry; 256]>() - 1) as u16;

    // SAFETY: boot is single-threaded and no other reference to `IDT`
    // exists. Forming the `&mut` through a raw pointer is the sanctioned
    // way to reach a `static mut`.
    let idt = unsafe { &mut *(&raw mut IDT) };
    for (vector, &stub) in STUBS.iter().enumerate() {
        // #DF (vector 8) runs on IST1 so it has a known-good stack even
        // if the fault was caused by a stack overflow.
        let ist = if vector == 8 { 1 } else { 0 };
        idt[vector].set_handler(stub as usize as u64, ist);
    }

    // Hardware-IRQ vectors used once interrupts are enabled (preemptive
    // scheduling): the periodic timer and the local-APIC spurious vector. Both
    // use IST0 — the timer can fire from ring 3, landing on TSS.RSP0 (kept
    // current by the scheduler), or from ring 0 on the current kernel stack.
    // Coerce the fn items to fn pointers before the address cast (the STUBS
    // array entries are already pointers; these two are bare items).
    let timer: extern "C" fn() = timer_stub;
    let spurious: extern "C" fn() = spurious_stub;
    idt[TIMER_VECTOR as usize].set_handler(timer as usize as u64, 0);
    idt[SPURIOUS_VECTOR as usize].set_handler(spurious as usize as u64, 0);

    let ptr = IdtPointer { limit, base };
    // SAFETY: `ptr` describes the fully-populated IDT. Interrupts are
    // masked, so no handler can fire while IDTR is being updated.
    unsafe {
        load_idt(&ptr);
    }
}

/// Execute `lidt` against `ptr`.
///
/// # Safety
/// `ptr` must describe a valid, fully-populated IDT.
unsafe fn load_idt(ptr: &IdtPointer) {
    // SAFETY: the caller guarantees `ptr` points at a valid IDT operand.
    // `lidt` reads 10 bytes from it and updates IDTR; no flags, no stack.
    unsafe {
        asm!("lidt [{}]", in(reg) ptr, options(readonly, nostack, preserves_flags));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_handler_splits_address() {
        let mut e = IdtEntry::EMPTY;
        e.set_handler(0x1234_5678_9ABC_DEF0, 1);
        assert_eq!(e.offset_low, 0xDEF0);
        assert_eq!(e.offset_mid, 0x9ABC);
        assert_eq!(e.offset_high, 0x1234_5678);
        assert_eq!(e.selector, KERNEL_CODE_SELECTOR);
        assert_eq!(e.ist, 1);
        assert_eq!(e.type_attr, 0x8E);
    }

    #[test]
    fn set_handler_masks_ist_to_three_bits() {
        let mut e = IdtEntry::EMPTY;
        e.set_handler(0, 0xFF);
        assert_eq!(e.ist, 0x7);
    }

    #[test]
    fn pf_fault_kind_decodes_error_bits() {
        // bit0 P, bit1 W/R, bit4 I/D.
        assert_eq!(pf_fault_kind(0b0000), FaultKind::NotMapped); // not present
        assert_eq!(pf_fault_kind(0b0001), FaultKind::NotReadable); // present, read
        assert_eq!(pf_fault_kind(0b0011), FaultKind::NotWritable); // present, write
        assert_eq!(pf_fault_kind(0b1_0001), FaultKind::NotExecutable); // present, insn fetch
        // not-present wins regardless of W/I bits.
        assert_eq!(pf_fault_kind(0b1_0010), FaultKind::NotMapped);
    }

    #[test]
    fn fault_shape_maps_vectors() {
        // #DE → divide-by-zero, addr = rip.
        assert_eq!(fault_shape(0, 0, 0xAAAA, None), (KIND_DIVIDE_BY_ZERO, 0xAAAA, FaultKind::UnknownFault));
        // #UD → illegal instruction, addr = rip.
        assert_eq!(fault_shape(6, 0, 0xBBBB, None).0, KIND_ILLEGAL_INSN);
        // #PF → seg fault, addr = cr2, kind from error code.
        let (k, addr, fk) = fault_shape(14, 0b0011, 0xCCCC, Some(0x1000));
        assert_eq!((k, addr, fk), (KIND_SEG_FAULT, 0x1000, FaultKind::NotWritable));
        // #GP and others → seg fault, addr = rip.
        assert_eq!(fault_shape(13, 0, 0xDDDD, None), (KIND_SEG_FAULT, 0xDDDD, FaultKind::UnknownFault));
    }

    #[test]
    fn stub_table_covers_all_32_vectors() {
        assert_eq!(STUBS.len(), 32);
    }
}
