//! Interrupt Descriptor Table: dump-and-halt handlers for the 32 CPU exception
//! vectors (with a recovery path for `#PF`), plus the hardware-IRQ vectors used
//! once interrupts are enabled — the periodic timer (`0x20`) and the local-APIC
//! spurious vector (`0xFF`).
//!
//! Exception handling splits by privilege: a **ring-0 (kernel)** fault prints
//! the faulting state and halts (`dump_and_halt`); a **ring-3 (user)** fault
//! **suspends** the faulting thread — a `Notification` (`SegFault` /
//! `IllegalInsn` / `DivideByZero`) is delivered to the faulting process and a
//! supervisor decides via `sys_exception_resume` whether to retry the
//! instruction (`Resume`) or terminate the thread (`Terminate`), so the kernel
//! survives (see `sched::suspend_with_fault`, `user_fault`, and
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
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::Cpu;
use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::arch::x86_64::gdt::KERNEL_CODE_SELECTOR;
use crate::arch::x86_64::regs;
use crate::arch::x86_64::serial;
use crate::libkern::notification::{
    FaultKind, KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, Notification,
};
use super::registers::RegisterValues;

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
/// and the 15 GPRs and call [`exception_dispatch`].
///
/// Like [`vec14`] and [`timer_stub`], the stub ends in a pop+`iretq` epilogue
/// rather than `ud2`: [`exception_dispatch`] **returns** on the ring-3
/// fault→`Resume` path (the supervisor chose to retry the instruction), and the
/// stub then unwinds the GPRs / vector / error-code slots and `iretq`s the
/// unmodified frame. A kernel-mode fault diverges in `dump_and_halt`, and a
/// `Terminate` disposition diverges in `exit_thread`, so the epilogue is only
/// *reached* on the resume path.
macro_rules! exception_stub {
    (noerr, $name:ident, $vec:expr) => {
        exception_stub!(@build $name, $vec, "push 0\n");
    };
    (err, $name:ident, $vec:expr) => {
        exception_stub!(@build $name, $vec, "");
    };
    (@build $name:ident, $vec:expr, $errcode:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
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
                // Resume path: pop the GPRs, drop the vector + error-code slots
                // (16 bytes), and `iretq` to the (unmodified) faulting RIP.
                concat!(
                    "pop r15\npop r14\npop r13\npop r12\n",
                    "pop r11\npop r10\npop r9\npop r8\n",
                    "pop rbp\npop rdi\npop rsi\n",
                    "pop rdx\npop rcx\npop rbx\npop rax\n",
                    "add rsp, 16\n",
                    "iretq\n",
                ),
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
extern "C" fn vec14() {
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
        // The CR2 read here is the faulting linear address.
        let cr2 = regs::read_cr2();

        // Demand paging: a *not-present* ring-3 fault may fall in a reserved-
        // but-unbacked anonymous VMA (a lazily-mapped stack/anon page). Try to
        // fault it in; on success the stub `iretq`s and the access retries.
        // error_code bit0 = present (see `pf_fault_kind`).
        let present = f.error_code & 1 != 0;
        if !present && try_fault_in(cr2, f.error_code) {
            return;
        }

        // A genuine fatal ring-3 fault (no covering VMA, a protection
        // violation, or OOM): suspend → supervised resume/terminate. On
        // `Resume` this returns and the stub `iretq`s; on `Terminate` it
        // diverges.
        user_fault(f, Some(cr2));
        return;
    }

    dump_and_halt(f);
}

/// Try to demand-fault the page at `cr2` into the running process's address
/// space. `error_code` is the `#PF` code (bit1 = write, bit4 = instruction
/// fetch); the caller has already confirmed the not-present (bit0 = 0) case, so
/// only the access *kind* is taken from it. Returns `true` iff a frame was
/// faulted in and the faulting instruction may be retried.
///
/// Reached only for ring-3 (`cs & 3 == 3`) faults, so the faulting thread is in
/// user mode and holds no kernel locks — taking the address-space lock here
/// cannot deadlock against the faulting context. Kernel copy-primitive faults
/// are caught earlier by `lookup_recovery` and never reach this path. (Kernel
/// access to a not-yet-faulted user page is therefore *not* auto-populated;
/// nothing does that today — see `docs/rationale/deferred-decisions.md`.)
fn try_fault_in(cr2: u64, error_code: u64) -> bool {
    use crate::mm::vmm::FaultAccess;

    let write = error_code & (1 << 1) != 0;
    let insn = error_code & (1 << 4) != 0;
    let access = if insn {
        FaultAccess::Execute
    } else if write {
        FaultAccess::Write
    } else {
        FaultAccess::Read
    };

    // No owning process (a kernel/boot thread) ⇒ nothing to demand-page.
    let Some(proc_ref) = crate::sched::current_process() else {
        return false;
    };
    // SAFETY: `proc_ref` references a live `Process` (its `KObjectHeader` is at
    // offset 0), pinned by the current user thread for the duration of this
    // fault. The borrow does not outlive `proc_ref`.
    let proc = unsafe { &*(proc_ref.as_ptr() as *const crate::object::Process) };
    let Some(asp) = proc.address_space() else {
        return false;
    };
    let addr = crate::mm::VirtAddr::new(cr2);
    match asp.fault_in(addr, access) {
        crate::mm::addr_space::FaultIn::Mapped => true,
        // A file-backed not-present fault: fetch the backing (a fresh AS-lock
        // acquisition — never nested with the file cache), **page it in** (this may
        // park the faulting thread on the producer's fill — `proc_ref`/`asp` stay
        // live across the block), then install the PTE. `false` if the file is gone
        // or the fill failed → a fatal fault (SegFault), like any other miss.
        crate::mm::addr_space::FaultIn::FileBacked => {
            let Some((file_obj, index)) = asp.file_backing(addr) else {
                return false;
            };
            match crate::object::FileObject::fault_in_page(&file_obj, index) {
                Some(frame) => asp.map_file_page(addr, &file_obj, frame),
                None => false,
            }
        }
        _ => false,
    }
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
    // Sample interrupt-timing jitter into the entropy pool (the fine low bits of
    // the cycle counter at IRQ-arrival time). Cheap and lock-bounded; see
    // `crate::entropy`. Sampled first, before the EOI/DPC/tick work perturbs it.
    crate::entropy::on_irq_sample(regs::rdtsc());
    // SAFETY: ring-0, only ever reached from the timer IRQ after `Irq::init`
    // mapped the local APIC; a single MMIO write acknowledges the interrupt.
    unsafe { crate::arch::Irq::eoi() };
    // Drain any pending DPCs before the tick's reschedule, so threads a device
    // DPC woke are already in `ready` when `on_timer_tick` picks the next one.
    crate::dpc::run_pending();
    crate::sched::on_timer_tick();
}

// --- Device-IRQ vectors (external interrupts routed by the IOAPIC) ----------
//
// The system interrupt router (`arch::IrqRouter`) routes a device's interrupt
// line to one of these vectors; the stub builds a frame, the shared dispatcher
// runs the registered handler and EOIs the local controller. Each vector has its
// own stub (it must push its own vector immediate); a small registry maps the
// vector back to a handler so a driver registers without touching the IDT.

/// First device-IRQ vector. Above the timer (`0x20`) and the exception range.
const DEVICE_IRQ_BASE: u8 = 0x30;
/// Number of device-IRQ vectors (`0x30..=0x37`). Plenty for Phase 2 (AHCI +
/// a few); grow by adding stubs to `DEVICE_STUBS` if a board needs more.
const DEVICE_IRQ_COUNT: usize = 8;

/// Registered handler per device vector, as a function pointer stored in a
/// `usize` (`0` = none). Written once at registration (boot), read in IRQ
/// context — lock-free.
static DEVICE_HANDLERS: [AtomicUsize; DEVICE_IRQ_COUNT] =
    [const { AtomicUsize::new(0) }; DEVICE_IRQ_COUNT];
/// Next free device-vector slot, handed out by [`register_device_handler`].
static NEXT_DEVICE_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Shared dispatcher for every device-IRQ vector. Runs the registered handler,
/// then signals end-of-interrupt to the local controller. Edge-triggered only
/// for now (the PIT bring-up source and the level-triggered IOAPIC-EOI path
/// land with the first level-triggered device).
extern "C" fn device_irq_dispatch(frame: *mut ExceptionFrame) {
    // Sample interrupt-timing jitter into the entropy pool (see `timer_dispatch`).
    crate::entropy::on_irq_sample(regs::rdtsc());
    // SAFETY: `frame` is the stub-built `ExceptionFrame` in RDI; `vector` is the
    // immediate the stub pushed, always in `DEVICE_IRQ_BASE..+COUNT`.
    let vector = unsafe { (*frame).vector } as usize;
    let slot = vector.wrapping_sub(DEVICE_IRQ_BASE as usize);
    if slot < DEVICE_IRQ_COUNT {
        let h = DEVICE_HANDLERS[slot].load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: a non-zero slot holds a function pointer installed by
            // `register_device_handler` (a real `extern "C" fn()`); single
            // writer at boot, so the value is valid.
            let f: extern "C" fn() = unsafe { core::mem::transmute(h) };
            f();
        }
    }
    // SAFETY: ring-0, reached only from a device IRQ after `Irq::init`.
    unsafe { crate::arch::Irq::eoi() };
    // Run any DPCs the handler queued — the deferred completion work (run at the
    // interrupt tail, IF=0; see `crate::dpc`).
    crate::dpc::run_pending();
}

/// Register `handler` for the next free device-IRQ vector and return that
/// vector. The IDT gate for every device vector is pre-installed by [`init`];
/// this only fills the handler registry, so a driver wires `(GSI → vector)` at
/// the router and `(vector → handler)` here without touching the IDT.
///
/// Panics if the device-vector pool is exhausted — a static configuration
/// error, not a runtime condition.
pub(crate) fn register_device_handler(handler: extern "C" fn()) -> u8 {
    let slot = NEXT_DEVICE_SLOT.fetch_add(1, Ordering::Relaxed);
    assert!(slot < DEVICE_IRQ_COUNT, "device-IRQ vector pool exhausted");
    DEVICE_HANDLERS[slot].store(handler as usize, Ordering::Release);
    DEVICE_IRQ_BASE + slot as u8
}

/// One device-IRQ entry stub — mirrors [`timer_stub`] but pushes its own vector
/// and routes through the shared [`device_irq_dispatch`].
macro_rules! device_irq_stub {
    ($name:ident, $vec:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            ::core::arch::naked_asm!(
                concat!(
                    "push 0\n",
                    "push ", $vec, "\n",
                    "push rax\npush rbx\npush rcx\npush rdx\n",
                    "push rsi\npush rdi\npush rbp\n",
                    "push r8\npush r9\npush r10\npush r11\n",
                    "push r12\npush r13\npush r14\npush r15\n",
                    "mov rdi, rsp\n",
                ),
                "call {dispatch}",
                concat!(
                    "pop r15\npop r14\npop r13\npop r12\n",
                    "pop r11\npop r10\npop r9\npop r8\n",
                    "pop rbp\npop rdi\npop rsi\n",
                    "pop rdx\npop rcx\npop rbx\npop rax\n",
                    "add rsp, 16\n",
                    "iretq\n",
                ),
                dispatch = sym device_irq_dispatch,
            );
        }
    };
}

device_irq_stub!(dev_irq_30, "0x30");
device_irq_stub!(dev_irq_31, "0x31");
device_irq_stub!(dev_irq_32, "0x32");
device_irq_stub!(dev_irq_33, "0x33");
device_irq_stub!(dev_irq_34, "0x34");
device_irq_stub!(dev_irq_35, "0x35");
device_irq_stub!(dev_irq_36, "0x36");
device_irq_stub!(dev_irq_37, "0x37");

/// Device-IRQ entry stubs, indexed by `vector - DEVICE_IRQ_BASE`.
const DEVICE_STUBS: [extern "C" fn(); DEVICE_IRQ_COUNT] = [
    dev_irq_30, dev_irq_31, dev_irq_32, dev_irq_33, dev_irq_34, dev_irq_35, dev_irq_36, dev_irq_37,
];

/// Entry stubs for CPU exception vectors 0-31, indexed by vector number. Each
/// `iretq`s on the ring-3 fault→`Resume` path (see [`exception_stub`]) and
/// otherwise diverges, so the type is `extern "C" fn()` (not `-> !`).
const STUBS: [extern "C" fn(); 32] = [
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

/// Common handler for every CPU exception except `#PF`. A **ring-3** fault
/// suspends the faulting thread (delivering a [`Notification`] to its process)
/// and acts on the supervisor's disposition via [`user_fault`] — returning here
/// (so the stub `iretq`s the unmodified frame) on `Resume`, or diverging into
/// `exit_thread` on `Terminate`. A **ring-0** fault is fatal ([`dump_and_halt`],
/// diverges).
///
/// `*mut ExceptionFrame` (not `*const`): the `Resume` path leaves the frame in
/// place for the stub to `iretq`, and matches [`pf_dispatch`]. Reached only from
/// a naked stub via `call`; `extern "C"` matches the stub's `mov rdi, rsp`.
extern "C" fn exception_dispatch(frame: *mut ExceptionFrame) {
    // SAFETY: the naked stub built a complete `ExceptionFrame` at the
    // stack top and passed its address in RDI. It is valid, 8-byte
    // aligned, and not aliased for the duration of this call.
    let f = unsafe { &mut *frame };
    if is_user_fault(f) {
        // Suspend → supervised resume/terminate (no CR2 for non-#PF vectors).
        user_fault(f, None);
        return;
    }
    // Ring-0 (kernel) fault: unrecoverable — dump and halt.
    dump_and_halt(f);
}

/// Shared ring-3 fault handler for every exception vector: suspend the faulting
/// thread with a fault [`Notification`] (delivered to its process's channel,
/// waking the supervisor), then act on the [`ResumeDisposition`] the supervisor
/// chose via `sys_exception_resume`. `Terminate` exits the thread (diverges);
/// `Resume` returns, so the entry stub `iretq`s the **unmodified** frame and the
/// faulting instruction retries. `cr2` carries the faulting address for `#PF`.
fn user_fault(f: &mut ExceptionFrame, cr2: Option<u64>) {
    let notif = notif_for_vector(f, cr2);
    let frame_ptr = f as *mut ExceptionFrame as usize;
    match crate::sched::suspend_with_fault(frame_ptr, notif) {
        crate::sched::ResumeDisposition::Terminate(code) => {
            // Diverges: switches away from this (now-terminated) thread forever.
            crate::sched::exit_thread(crate::libkern::ExitStatus {
                kind: crate::libkern::ExitKind::Crashed as u32,
                code,
            });
        }
        // Return to the stub, which `iretq`s back to the faulting RIP (retry).
        crate::sched::ResumeDisposition::Resume => {}
    }
}

/// Reads a suspended thread's captured user registers out of the private
/// [`ExceptionFrame`] the entry stub built. The impl lives here (rather than in
/// [`registers`](super::registers), which owns the [`RegisterValues`] type)
/// precisely so `ExceptionFrame` stays private to this module — the arch
/// boundary's [`ArchRegisters`](crate::arch::registers::ArchRegisters) contract,
/// re-exported as `crate::arch::Registers`, behind `sys_thread_get_registers`.
impl crate::arch::registers::ArchRegisters for super::registers::X86Registers {
    type Values = RegisterValues;

    fn read_from_exception_frame(frame_ptr: usize) -> RegisterValues {
        // SAFETY: `frame_ptr` is the kernel-stack address of a suspended
        // thread's `ExceptionFrame` (the thread stays parked while suspended, so
        // the frame is stable); read-only, 8-byte aligned. The caller pins the
        // thread.
        let f = unsafe { &*(frame_ptr as *const ExceptionFrame) };
        RegisterValues {
            rax: f.rax,
            rbx: f.rbx,
            rcx: f.rcx,
            rdx: f.rdx,
            rsi: f.rsi,
            rdi: f.rdi,
            rbp: f.rbp,
            rsp: f.rsp,
            r8: f.r8,
            r9: f.r9,
            r10: f.r10,
            r11: f.r11,
            r12: f.r12,
            r13: f.r13,
            r14: f.r14,
            r15: f.r15,
            rip: f.rip,
            rflags: f.rflags,
        }
    }
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

    // Device-IRQ vectors (0x30..): the gates are pre-installed here; a driver
    // routes a GSI to one of these and registers a handler via
    // `register_device_handler` (the IDT is not touched again). IST0.
    for (i, &stub) in DEVICE_STUBS.iter().enumerate() {
        idt[DEVICE_IRQ_BASE as usize + i].set_handler(stub as usize as u64, 0);
    }

    // Gates installed; load IDTR. The table is shared across CPUs — only the
    // `lidt` is per-CPU, so APs call [`load`] directly.
    load();
}

/// Load the (already-built) shared IDT into IDTR on the running CPU. [`init`]
/// builds the table once (BSP); APs only need this `lidt`.
pub fn load() {
    let base = &raw const IDT as usize as u64;
    let limit = (size_of::<[IdtEntry; 256]>() - 1) as u16;
    let ptr = IdtPointer { limit, base };
    // SAFETY: `ptr` describes the IDT built by `init`. With interrupts masked no
    // handler can fire while IDTR is being updated.
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
