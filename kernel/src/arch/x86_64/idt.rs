//! Interrupt Descriptor Table: dump-and-halt handlers for the 32 CPU exception vectors (with
//! a recovery path for `#PF`), plus the hardware-IRQ vectors: timer (`0x20`), TLB-shootdown
//! and reschedule IPIs, device vectors, and the local-APIC spurious vector (`0xFF`).
//!
//! A **ring-0** fault prints the faulting state and **stops every online CPU**
//! (`dump_and_halt` → `Cpu::stop_the_machine`). A **ring-3** fault **suspends** the thread and
//! sends its process a `Notification`; a supervisor decides via `sys_exception_resume` whether
//! to retry the instruction or terminate the thread (`sched::suspend_with_fault`,
//! `docs/architecture/notifications.md`). `#PF` alone has a recovery path: a match in the
//! user-memory-access exception table ([`crate::mm::user_access`]) patches the saved RIP and
//! `iretq`s, turning a fault in a user-memory copy into a [`Result::Err`].
//!
//! Rust's `x86-interrupt` convention is nightly-only, so each vector has a naked stub
//! ([`exception_stub!`]) that pushes a dummy `0` error code where the CPU pushes none (one
//! frame layout for every vector), then the vector and all 15 GPRs to build an
//! [`ExceptionFrame`], and calls [`exception_dispatch`]. On the ring-3 `Resume` path the
//! dispatcher returns and the stub unwinds the frame and `iretq`s; otherwise it diverges.
//! Vector 14 is built by hand ([`vec14`]). `#DF` runs on IST1 (`gdt.rs`) so it can report after
//! a stack overflow.

use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::registers::RegisterValues;
use crate::arch::Cpu;
use crate::arch::Paging;
use crate::arch::cpu::ArchCpu;
use crate::arch::irq::{ArchIrq, SPURIOUS_VECTOR, TIMER_VECTOR};
use crate::arch::paging::ArchPaging;
use crate::arch::x86_64::gdt::KERNEL_CODE_SELECTOR;
use crate::arch::x86_64::regs;
use crate::arch::x86_64::serial;
use crate::libkern::notification::{FaultKind, KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, Notification};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr};

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
    const EMPTY: IdtEntry =
        IdtEntry { offset_low: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_high: 0, reserved: 0 };

    /// Point this gate at `handler`, running on IST slot `ist` (0 = none, i.e. the current
    /// stack). Present, DPL 0, 64-bit interrupt gate. Host-tested.
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

#[repr(C, packed)]
struct IdtPointer {
    limit: u16,
    base: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::EMPTY; 256];

/// The register state an exception stub builds on the stack. Field order mirrors the push
/// order: the first field is at the lowest address, where the stub leaves RSP and what
/// [`exception_dispatch`] receives.
#[repr(C)]
struct ExceptionFrame {
    // 15 general-purpose registers pushed by the stub; `r15` is pushed last, so it is lowest.
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

/// Define an interrupt/exception **dispatcher**, the `extern "C"` function a naked stub `call`s.
///
/// Every dispatcher must open a lock-ordering scope on entry: the acquisition order restarts at
/// an interrupt boundary (`crate::libkern::lockrank` § Interrupt scopes). This macro is the
/// only way to define one, and `cargo xtask check-irq-scope` fails the build if a stub's
/// `dispatch = sym …` names a function it did not generate. The guard is skipped on the
/// diverging paths, reachable only from a context holding nothing, so the scope it would restore
/// is the empty one.
///
/// It is every interrupt's way back to ring 3, so it ends a thread whose process is exiting there
/// ([`crate::sched::end_if_ending`]); the body is a function of its own, so a `return` comes back.
macro_rules! irq_dispatcher {
    ($(#[$attr:meta])* fn $name:ident($frame:ident: *mut ExceptionFrame) $body:block) => {
        $(#[$attr])*
        extern "C" fn $name($frame: *mut ExceptionFrame) {
            let _lock_scope = crate::libkern::lockrank::enter_interrupt();
            fn body($frame: *mut ExceptionFrame) $body
            body($frame);
            // SAFETY: the stub-built frame in RDI, valid for this call.
            if unsafe { (*$frame).cs } & 3 == 3 {
                crate::sched::end_if_ending();
            }
        }
    };
}

/// Every interrupt and exception stub's first instructions: `cld; clac`, since ring 3 may leave DF
/// or AC set and only `syscall` masks them (SFMASK); then the error-code slot, vector and 15 GPRs
/// of an [`ExceptionFrame`], its address in RDI. `clac` needs SMAP, which the kernel requires.
macro_rules! entry_prologue {
    ($errcode:literal, $vec:tt) => {
        concat!(
            "cld\nclac\n",
            $errcode,
            "push ",
            stringify!($vec),
            "\n",
            "push rax\npush rbx\npush rcx\npush rdx\n",
            "push rsi\npush rdi\npush rbp\n",
            "push r8\npush r9\npush r10\npush r11\n",
            "push r12\npush r13\npush r14\npush r15\n",
            "mov rdi, rsp\n",
        )
    };
}

/// Pop the 15 GPRs, drop the vector and error-code slots, and `iretq`.
macro_rules! entry_epilogue {
    () => {
        concat!(
            "pop r15\npop r14\npop r13\npop r12\n",
            "pop r11\npop r10\npop r9\npop r8\n",
            "pop rbp\npop rdi\npop rsi\n",
            "pop rdx\npop rcx\npop rbx\npop rax\n",
            "add rsp, 16\n",
            "iretq\n",
        )
    };
}

/// Define a naked exception-entry stub. The `noerr` form pushes a dummy `0` error code first;
/// `err` relies on the CPU-pushed one. Both push the vector and the 15 GPRs and call
/// [`exception_dispatch`], which **returns** on the ring-3 `Resume` path; the stub then
/// unwinds the frame and `iretq`s it unmodified. Kernel faults and `Terminate` diverge.
macro_rules! exception_stub {
    (noerr, $name:ident, $vec:tt) => {
        exception_stub!(@build $name, $vec, "push 0\n");
    };
    (err, $name:ident, $vec:tt) => {
        exception_stub!(@build $name, $vec, "");
    };
    (@build $name:ident, $vec:tt, $errcode:literal) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            ::core::arch::naked_asm!(
                entry_prologue!($errcode, $vec),
                "call {dispatch}",
                // Resume path: pop the GPRs, drop the vector + error-code slots
                // (16 bytes), and `iretq` to the (unmodified) faulting RIP.
                entry_epilogue!(),
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

// --- #PF stub (vector 14) ---
//
// The CPU pushed an error code, so there is no dummy zero; the stub `iretq`s on recovery.

#[unsafe(naked)]
extern "C" fn vec14() {
    ::core::arch::naked_asm!(
        // The CPU pushed a #PF error code; push the vector and the 15 GPRs to build a full
        // `ExceptionFrame`, then pass its address to the dispatcher.
        entry_prologue!("", 14),
        "call {dispatch}",
        // Recovery path: the dispatcher patched `frame.rip` and returned. Pop the GPRs, drop
        // the vector + error-code slots (16 bytes), and `iretq` to the patched RIP; `iretq`
        // pops the CPU-pushed RIP/CS/RFLAGS/RSP/SS itself.
        entry_epilogue!(),
        dispatch = sym pf_dispatch,
    );
}

irq_dispatcher! {
/// `#PF` dispatcher. Looks up the faulting RIP in the user-access exception table; on a match,
/// patches `frame.rip` to the recovery PC and returns so the stub can `iretq` to it. On a
/// miss, calls [`dump_and_halt`]. `*mut ExceptionFrame` because recovery writes the new RIP
/// in place.
fn pf_dispatch(frame: *mut ExceptionFrame) {
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
        let cr2 = regs::read_cr2();

        // Demand paging: a not-present ring-3 fault may fall in a reserved-but-unbacked
        // anonymous VMA. Try to fault it in; on success the stub `iretq`s and the access retries.
        let present = f.error_code & 1 != 0;
        if !present && try_fault_in(cr2, f.error_code) {
            return;
        }

        // A genuine fatal ring-3 fault (no covering VMA, a protection violation, or OOM):
        // suspend, then supervised resume/terminate. `Resume` returns and the stub `iretq`s;
        // `Terminate` diverges.
        user_fault(f, Some(cr2));
        return;
    }

    dump_and_halt(f);
}
}

/// Try to demand-fault the page at `cr2` into the running process's address space, taking only
/// the access *kind* from `error_code` (the caller has confirmed not-present). Returns `true`
/// iff a frame was faulted in and the instruction may be retried.
///
/// Reached only for ring-3 faults, so the thread holds no kernel locks and taking the
/// address-space lock cannot deadlock. Kernel copy-primitive faults are caught earlier by
/// `lookup_recovery`; a kernel access to a not-yet-faulted user page is not auto-populated
/// (see `docs/rationale/deferred-decisions.md`).
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
        // A file-backed not-present fault: fetch the backing (a fresh AS-lock acquisition, never
        // nested with the file cache), **page it in** (this may park the faulting thread on the
        // producer's fill; `proc_ref`/`asp` stay live across the block), then install the PTE.
        // `false` if the file is gone or the fill failed: a fatal fault, like any other miss.
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

// --- Timer IRQ stub (vector 0x20) ---
//
// A *returning* stub: the dispatcher EOIs and may reschedule, then control returns here to
// `iretq`, or after a preemptive switch to whichever context last parked in this stub. IF=0
// throughout. Vectors ≥ 32 carry no CPU error code, so a dummy `0` fills that slot.

#[unsafe(naked)]
extern "C" fn timer_stub() {
    ::core::arch::naked_asm!(
        // Build a full `ExceptionFrame`: dummy error code, vector, 15 GPRs.
        entry_prologue!("push 0\n", 0x20),
        "call {dispatch}",
        // The dispatcher returned: pop the GPRs, drop the vector + dummy error-code slots
        // (16 bytes), and `iretq` to the interrupted context (restoring its RIP/CS/RFLAGS/RSP/SS,
        // including IF).
        entry_epilogue!(),
        dispatch = sym timer_dispatch,
    );
}

/// Spurious-interrupt (vector `0xFF`) entry stub. A spurious local-APIC interrupt requires
/// **no** EOI and the stub touches no registers, so it just `iretq`s.
#[unsafe(naked)]
extern "C" fn spurious_stub() {
    ::core::arch::naked_asm!("iretq");
}

irq_dispatcher! {
/// Timer IRQ dispatcher. EOIs **first**: [`crate::sched::on_timer_tick`] may switch away and not
/// return to this frame for a long time, and a late EOI would block all further timer delivery.
/// Runs with IF=0, so it never nests and may take the interrupt-safe run-queue lock. The frame
/// is unused.
fn timer_dispatch(_frame: *mut ExceptionFrame) {
    // Sample interrupt-timing jitter first, before the EOI/DPC/tick work perturbs it.
    crate::entropy::on_irq_sample(regs::rdtsc());
    // SAFETY: ring-0, only ever reached from the timer IRQ after `Irq::init`
    // mapped the local APIC; a single MMIO write acknowledges the interrupt.
    unsafe { crate::arch::Irq::eoi() };
    // Sweep up input the i8042's interrupt path can lose (see `drivers::ps2::poll`): one `inb`
    // per tick when idle. **Before `run_pending`, not after**: `poll` enqueues the PS/2 DPC when
    // it recovers a byte, and enqueueing after the drain would park the wake-up until the next
    // tick, adding 10 ms on the path that exists because an interrupt was lost. Same shape as
    // the fs-server "I/O hang" in `device_irq_dispatch`.
    crate::drivers::ps2::poll();
    // Drain DPCs before the reschedule, so threads they woke are already in `ready`.
    crate::dpc::run_pending();
    crate::sched::on_timer_tick();
}
}

// --- TLB-shootdown IPI stub (vector 0x40) ---
//
// A returning stub like the timer: another CPU sent it to have us invalidate a stale translation.

#[unsafe(naked)]
extern "C" fn tlb_shootdown_stub() {
    ::core::arch::naked_asm!(
        entry_prologue!("push 0\n", 0x40),
        "call {dispatch}",
        entry_epilogue!(),
        dispatch = sym tlb_shootdown_dispatch,
    );
}

irq_dispatcher! {
/// TLB-shootdown IPI dispatcher: invalidate and acknowledge (in [`crate::tlb`]), then EOI.
/// The frame is unused. Its body never reschedules or blocks.
fn tlb_shootdown_dispatch(_frame: *mut ExceptionFrame) {
    crate::tlb::on_ipi();
    // SAFETY: ring-0 IPI context; a single write acknowledges the local APIC.
    // `Irq::init` has run on this CPU (IPIs arrive only after bring-up).
    unsafe { crate::arch::Irq::eoi() };
}
}

// --- Reschedule IPI stub (vector 0x41) ---
//
// A returning stub like the timer: another CPU made a thread runnable here and wants the
// scheduler to run now. The dispatcher EOIs *first*; it may switch away.

#[unsafe(naked)]
extern "C" fn reschedule_ipi_stub() {
    ::core::arch::naked_asm!(
        entry_prologue!("push 0\n", 0x41),
        "call {dispatch}",
        entry_epilogue!(),
        dispatch = sym reschedule_ipi_dispatch,
    );
}

irq_dispatcher! {
/// Reschedule IPI dispatcher: EOI **first** (the scheduler may switch away and not return
/// promptly, so a late EOI would stall further IPIs), then drive the scheduler. The frame is
/// unused.
fn reschedule_ipi_dispatch(_frame: *mut ExceptionFrame) {
    // SAFETY: ring-0 IPI context; a single write acknowledges the local APIC.
    unsafe { crate::arch::Irq::eoi() };
    crate::sched::on_reschedule_ipi();
}
}

// --- Device-IRQ vectors (external interrupts routed by the IOAPIC) ---
//
// `arch::IrqRouter` routes a device's line to one of these vectors. Each has its own stub (it
// pushes its own vector immediate); a registry maps the vector to a handler, so a driver
// registers without touching the IDT.

/// First device-IRQ vector. Above the timer (`0x20`) and the exception range.
const DEVICE_IRQ_BASE: u8 = 0x30;
/// Number of device-IRQ vectors (`0x30..=0x37`); add stubs to `DEVICE_STUBS` for more.
const DEVICE_IRQ_COUNT: usize = 8;

/// Registered handler per device vector, a function pointer stored in a `usize` (`0` = none).
/// Written once at registration (boot), read in IRQ context, lock-free.
static DEVICE_HANDLERS: [AtomicUsize; DEVICE_IRQ_COUNT] = [const { AtomicUsize::new(0) }; DEVICE_IRQ_COUNT];
static NEXT_DEVICE_SLOT: AtomicUsize = AtomicUsize::new(0);

irq_dispatcher! {
/// Shared dispatcher for every device-IRQ vector: runs the registered handler, then signals
/// end-of-interrupt to the local controller.
fn device_irq_dispatch(frame: *mut ExceptionFrame) {
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
    crate::dpc::run_pending();
    // A scheduling point at the device-IRQ tail. A completion DPC may have made a thread
    // runnable on THIS CPU (`complete_pending_op` wakes its waiter), and a same-CPU placement
    // sends no reschedule IPI. With no other scheduling point here, an idle CPU would `iretq`
    // back into `hlt` and the woken thread would wait for the next 10 ms tick (the fs-server
    // "I/O hang"; decision log 2026-07-23). `resched_if_idle` resumes only an *idle* CPU. EOI
    // was already sent; a delayed return resumes into the stub's epilogue and `iretq`s.
    crate::sched::resched_if_idle();
}
}

/// Register `handler` for the next free device-IRQ vector and return that vector. The gates
/// for every device vector are pre-installed by [`init`]; this only fills the registry, so a
/// driver wires `(GSI → vector)` at the router and `(vector → handler)` here.
///
/// Panics if the device-vector pool is exhausted, a static configuration error.
pub(crate) fn register_device_handler(handler: extern "C" fn()) -> u8 {
    let slot = NEXT_DEVICE_SLOT.fetch_add(1, Ordering::Relaxed);
    assert!(slot < DEVICE_IRQ_COUNT, "device-IRQ vector pool exhausted");
    DEVICE_HANDLERS[slot].store(handler as usize, Ordering::Release);
    DEVICE_IRQ_BASE + slot as u8
}

macro_rules! device_irq_stub {
    ($name:ident, $vec:tt) => {
        #[unsafe(naked)]
        extern "C" fn $name() {
            ::core::arch::naked_asm!(
                entry_prologue!("push 0\n", $vec),
                "call {dispatch}",
                entry_epilogue!(),
                dispatch = sym device_irq_dispatch,
            );
        }
    };
}

device_irq_stub!(dev_irq_30, 0x30);
device_irq_stub!(dev_irq_31, 0x31);
device_irq_stub!(dev_irq_32, 0x32);
device_irq_stub!(dev_irq_33, 0x33);
device_irq_stub!(dev_irq_34, 0x34);
device_irq_stub!(dev_irq_35, 0x35);
device_irq_stub!(dev_irq_36, 0x36);
device_irq_stub!(dev_irq_37, 0x37);

const DEVICE_STUBS: [extern "C" fn(); DEVICE_IRQ_COUNT] =
    [dev_irq_30, dev_irq_31, dev_irq_32, dev_irq_33, dev_irq_34, dev_irq_35, dev_irq_36, dev_irq_37];

/// Entry stubs for CPU exception vectors 0-31, indexed by vector. Each `iretq`s on the ring-3
/// `Resume` path (see [`exception_stub`]) and otherwise diverges, hence `extern "C" fn()`.
const STUBS: [extern "C" fn(); 32] = [
    vec0, vec1, vec2, vec3, vec4, vec5, vec6, vec7, vec8, vec9, vec10, vec11, vec12, vec13, vec14, vec15, vec16, vec17,
    vec18, vec19, vec20, vec21, vec22, vec23, vec24, vec25, vec26, vec27, vec28, vec29, vec30, vec31,
];

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
        // `TODO(unexplained-df)` — an intermittent `#DF` that ran at ~14 % of local **TCG**
        // boots (unseen in 83 CI boots, all `--kvm`): kernel `rip`, a *user* `rsp`, stack
        // "not scannable". Two causes found and fixed — the `sysretq` stub's unmasked window
        // on the user stack, and `interrupts_restore(false)` being a no-op, which let
        // `tlb::shootdown` leave `IF` set for the rest of every SMP `sys_memory_unmap`.
        // 45/45 TCG boots clean since. The entry stays open until it has soaked; see
        // `deferred-decisions.md` for the evidence and what was ruled out.
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

/// Set once a CPU has decided the machine is going down, before it sends the stop NMIs; read
/// by every CPU's vector-2 handler to tell a stop from a hardware NMI. `Release`/`Acquire`.
pub(crate) static STOPPING: AtomicBool = AtomicBool::new(false);

/// `true` once some CPU has begun taking the machine down (for a core still in bring-up).
pub fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}

/// Whether this exception is a stop notice rather than a fault to diagnose. The ordering
/// (decided before the ring test) is structural, so [`dump_and_halt`] checks [`STOPPING`] too.
pub(crate) fn is_stop_notice(vector: u64, stopping: bool) -> bool {
    vector == 2 && stopping
}

/// Vector 2 with [`STOPPING`] set: halt without dumping, as N register dumps would bury the
/// deciding CPU's diagnosis.
fn stop_notice() -> ! {
    Cpu::halt_loop()
}

irq_dispatcher! {
/// Common handler for every CPU exception except `#PF`. A **ring-3** fault suspends the
/// thread and acts on the supervisor's disposition via [`user_fault`]: it returns on `Resume`
/// (the stub `iretq`s the unmodified frame) or diverges on `Terminate`. A **ring-0** fault is
/// fatal ([`dump_and_halt`]).
fn exception_dispatch(frame: *mut ExceptionFrame) {
    // SAFETY: the naked stub built a complete `ExceptionFrame` at the
    // stack top and passed its address in RDI. It is valid, 8-byte
    // aligned, and not aliased for the duration of this call.
    let f = unsafe { &mut *frame };
    // The stop notice, before anything else: an NMI while `STOPPING` is set is this CPU being
    // told the machine is going down. Checked ahead of the ring test because the CPUs most
    // worth stopping are in ring 0, spinning on a lock the faulting CPU holds; the generic
    // ring-0 path would send each through `dump_and_halt` and bury the diagnosis.
    if is_stop_notice(f.vector, STOPPING.load(Ordering::Acquire)) {
        stop_notice();
    }
    if is_user_fault(f) {
        user_fault(f, None);
        return;
    }
    dump_and_halt(f);
}
}

/// Shared ring-3 fault handler: suspend the thread with a fault [`Notification`], then act on
/// the [`ResumeDisposition`] chosen via `sys_exception_resume`. `Terminate` exits the thread;
/// `Resume` returns and the instruction retries. `cr2` is the faulting address for `#PF`.
fn user_fault(f: &mut ExceptionFrame, cr2: Option<u64>) {
    let notif = notif_for_vector(f, cr2);
    let frame_ptr = f as *mut ExceptionFrame as usize;
    match crate::sched::suspend_with_fault(frame_ptr, notif) {
        crate::sched::ResumeDisposition::Terminate(code) => {
            crate::sched::exit_thread(crate::libkern::ExitStatus {
                kind: crate::libkern::ExitKind::Crashed as u32,
                code,
            });
        }
        crate::sched::ResumeDisposition::Resume => {}
    }
}

/// Reads a suspended thread's user registers out of the private [`ExceptionFrame`]; the impl
/// lives here so the frame stays private (behind `sys_thread_get_registers`).
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

fn is_user_fault(f: &ExceptionFrame) -> bool {
    f.cs & 3 == 3
}

fn notif_for_vector(f: &ExceptionFrame, cr2: Option<u64>) -> Notification {
    let tid = crate::sched::current_tid();
    let (kind, addr, fault) = fault_shape(f.vector, f.error_code, f.rip, cr2);
    match kind {
        KIND_DIVIDE_BY_ZERO => Notification::divide_by_zero(tid, addr),
        KIND_ILLEGAL_INSN => Notification::illegal_insn(tid, addr),
        _ => Notification::seg_fault(tid, addr, fault),
    }
}

/// Pure vector→notification-shape mapping (host-testable). Returns `(kind discriminant,
/// fault address, FaultKind)`; `cr2` is the #PF address, other vectors use `rip`.
fn fault_shape(vector: u64, error_code: u64, rip: u64, cr2: Option<u64>) -> (u32, u64, FaultKind) {
    match vector {
        0 => (KIND_DIVIDE_BY_ZERO, rip, FaultKind::UnknownFault), // #DE
        6 => (KIND_ILLEGAL_INSN, rip, FaultKind::UnknownFault),   // #UD
        14 => (KIND_SEG_FAULT, cr2.unwrap_or(rip), pf_fault_kind(error_code)), // #PF
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

/// Dump the faulting state to the serial console, then **stop the machine**. Shared by
/// [`exception_dispatch`] and [`pf_dispatch`] (on faults that miss the exception table).
fn dump_and_halt(f: &ExceptionFrame) -> ! {
    // Defence in depth: a CPU already being stopped must not print a register dump, since N of
    // them into an unsynchronised serial port bury the diagnosis. `exception_dispatch` catches
    // this earlier, but that check is a *position* in a function; this one holds for any path
    // that reaches here without passing through it.
    if STOPPING.load(Ordering::Acquire) {
        stop_notice();
    }
    // The emergency writer bypasses `SERIAL`'s lock, which the fault may have held.
    let mut w = serial::emergency_writer();

    let _ = writeln!(w, "\n*** CPU EXCEPTION ***");
    let _ = writeln!(w, "  vector  {:#04x}  {}", f.vector, vector_name(f.vector));
    let _ = writeln!(w, "  error   {:#018x}", f.error_code);
    if f.vector == 14 || f.vector == 8 {
        // #PF: CR2 holds the faulting linear address. **#DF too**: a double fault is usually a
        // fault taken while delivering another, so CR2 still holds the address the *first* could
        // not translate, and the pushed `rip` is architecturally undefined for #DF.
        let _ = writeln!(w, "  cr2     {:#018x}", regs::read_cr2());
    }
    if f.vector == 8 {
        // `LSTAR`, so the dump decodes itself. A `#DF` with a kernel `cs` and a *user* `rsp`
        // comes from the `syscall` stub's two windows where ring 0 runs on the user stack: at
        // entry before `mov rsp, gs:[…]`, and at exit around `pop rsp` / `sysretq`. `rip -
        // lstar` says whether the fault was inside the stub and which end; the address is
        // otherwise unrecoverable, as every rebuild moves it.
        // SAFETY: `IA32_LSTAR` is architectural in long mode and this reads it in ring 0.
        let lstar = unsafe { regs::rdmsr(super::syscall::MSR_LSTAR) };
        let _ = writeln!(w, "  lstar   {:#018x}  (rip - lstar = offset into syscall_entry)", lstar);
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
    dump_return_addresses(f, &mut w);
    let _ = writeln!(w, "stopping every CPU.");

    // Printed first so the diagnosis is out before the other cores are frozen.
    Cpu::stop_the_machine()
}

/// Kernel text: the higher-half image base up to a generous bound. A stack word outside this
/// range is data, not a return address.
const TEXT_LO: u64 = 0xffff_ffff_8000_0000;
const TEXT_HI: u64 = 0xffff_ffff_8100_0000;

/// Stack words to scan. The interesting frames are within a few hundred bytes; the page probe
/// in [`scan_stack_candidates`] stops the walk earlier whenever the stack does.
const STACK_SCAN_WORDS: usize = 96;

/// Scan the faulting stack for values that look like kernel-text addresses and print them,
/// innermost first: a poor man's backtrace. A `call` leaves its return address on the stack,
/// so scanning for the kernel text range recovers the call chain (false positives from stale
/// slots, hence "candidates").
///
/// Placed on the dying path, not as a check in the code under investigation: hunting the
/// stale-`us_reg` teardown crash (decision log, 2026-07-29), every probe in the hot path
/// changed its incidence, because the bug was timing-sensitive; code that runs only after the
/// fault cannot perturb what it measures.
fn dump_return_addresses(f: &ExceptionFrame, w: &mut impl Write) {
    // A ring-0 fault keeps the faulting stack (long mode always pushes SS:RSP), so `f.rsp` is
    // where that context's stack continues.
    let sp = f.rsp;
    if sp < TEXT_LO && !(0xffff_8000_0000_0000..0xffff_ffff_8000_0000).contains(&sp) {
        let _ = writeln!(w, "  (stack pointer {sp:#018x} not scannable)");
        return;
    }
    // Probe each page before reading it. A kernel stack's guard page is at the *bottom*, so
    // walking **up** from RSP leaves the mapped region at the stack top and lands in unmapped
    // vmap, which would turn this diagnostic into a second fault. Stop at the first unmapped
    // page.
    let root = PhysAddr::new(regs::read_cr3() & !0xFFF);
    // The first five words verbatim. When the faulting instruction is the `iretq` at the tail of
    // an entry stub, these are the frame it is consuming (RIP, CS, RFLAGS, RSP, SS), and `SS`
    // distinguishes the candidate causes of a `#GP` naming a data selector. Printed raw because
    // none are kernel text and the filtered scan below would drop them.
    let _ = writeln!(w, "  words at rsp (iretq frame if the fault is a stub tail):");
    for i in 0..5usize {
        let addr = sp + (i as u64) * 8;
        let page = addr & !(PAGE_SIZE as u64 - 1);
        // SAFETY: `root` is the live page-table root; `translate` only walks it.
        if unsafe { Paging::translate(root, VirtAddr::new(page)) }.is_none() {
            break;
        }
        // SAFETY: the containing page was just confirmed mapped.
        let val = unsafe { (addr as *const u64).read_volatile() };
        let label = ["rip", "cs", "rflags", "rsp", "ss"][i];
        let _ = writeln!(w, "    [rsp+{:#04x}] {label:>6} = {val:#018x}", i * 8);
    }
    scan_stack_candidates(sp, w);
}

/// Dump return-address candidates from the **caller's own** stack, for the panic handler (no
/// [`ExceptionFrame`]). Its own stack pointer puts the caller's return address in range.
pub fn dump_stack_candidates(w: &mut impl Write) {
    let sp: u64;
    // SAFETY: reads the stack pointer into a register; touches no memory.
    unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack)) };
    scan_stack_candidates(sp, w);
}

/// Print the words of the stack at `sp` that fall in kernel text, probing each page before
/// reading it so a stack's top edge ends the scan instead of faulting.
fn scan_stack_candidates(sp: u64, w: &mut impl Write) {
    if sp < TEXT_LO && !(0xffff_8000_0000_0000..0xffff_ffff_8000_0000).contains(&sp) {
        let _ = writeln!(w, "  (stack pointer {sp:#018x} not scannable)");
        return;
    }
    let root = PhysAddr::new(regs::read_cr3() & !0xFFF);
    let _ = writeln!(w, "  return-address candidates (innermost first):");
    let mut probed_page = u64::MAX;
    for i in 0..STACK_SCAN_WORDS {
        let addr = sp + (i as u64) * 8;
        let page = addr & !(PAGE_SIZE as u64 - 1);
        if page != probed_page {
            // SAFETY: `root` is the live page-table root read from CR3; `translate`
            // only walks it and returns whether the address resolves.
            if unsafe { Paging::translate(root, VirtAddr::new(page)) }.is_none() {
                let _ = writeln!(w, "    (stack ends at {page:#018x} — stopping)");
                return;
            }
            probed_page = page;
        }
        // SAFETY: the containing page was just confirmed mapped.
        let val = unsafe { (addr as *const u64).read_volatile() };
        if (TEXT_LO..TEXT_HI).contains(&val) {
            let _ = writeln!(w, "    [rsp+{:#05x}] {:#018x}", i * 8, val);
        }
    }
}

/// Build and load the IDT. Call once, early in boot, after [`crate::arch::x86_64::gdt::init`]
/// (the gates reference the kernel code selector, and `#DF` uses the TSS's IST1). Does not
/// enable interrupts.
pub fn init() {
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: boot is single-threaded and no other reference to `IDT`
    // exists. Forming the `&mut` through a raw pointer is the sanctioned
    // way to reach a `static mut`.
    let idt = unsafe { &mut *(&raw mut IDT) };
    for (vector, &stub) in STUBS.iter().enumerate() {
        // #DF (vector 8) runs on IST1 so it has a known-good stack even after a stack overflow.
        //
        // **NMI (vector 2) runs on IST2**: an IST=0 gate does not switch stacks, and NMI can
        // arrive when RSP is not a kernel stack, so the frame push would `#PF` under SMAP, fault
        // again on the same stack, and `#DF`.
        //
        // **`syscall_entry` has two such windows, and IST2 is the only thing covering the
        // second.** At entry, RSP is the user stack for two instructions after `swapgs`; there
        // SFMASK masks `IF`, so IST2 is what NMI needs. At exit, RSP is the user stack between
        // `pop rsp` and `sysretq`; there a `cli` masks `IF`, but `cli` does not mask NMI, so IST2
        // is again the only cover. SFMASK alone does not make IST2 redundant: it reaches only the
        // entry half.
        let ist = match vector {
            8 => 1,
            2 => 2,
            _ => 0,
        };
        idt[vector].set_handler(stub as usize as u64, ist);
    }

    // Both IST0: the timer can fire from ring 3 (landing on TSS.RSP0) or ring 0.
    let timer: extern "C" fn() = timer_stub;
    let spurious: extern "C" fn() = spurious_stub;
    idt[TIMER_VECTOR as usize].set_handler(timer as usize as u64, 0);
    idt[SPURIOUS_VECTOR as usize].set_handler(spurious as usize as u64, 0);
    let shootdown: extern "C" fn() = tlb_shootdown_stub;
    idt[super::tlb::TLB_SHOOTDOWN_VECTOR as usize].set_handler(shootdown as usize as u64, 0);
    let reschedule: extern "C" fn() = reschedule_ipi_stub;
    idt[super::resched::RESCHEDULE_VECTOR as usize].set_handler(reschedule as usize as u64, 0);

    for (i, &stub) in DEVICE_STUBS.iter().enumerate() {
        idt[DEVICE_IRQ_BASE as usize + i].set_handler(stub as usize as u64, 0);
    }

    load();
}

/// Load the shared IDT into IDTR on the running CPU; APs call only this, [`init`] builds it.
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

    /// Vector 2 is the only vector that can be a stop notice, and only during a stop: a
    /// hardware NMI outside one is a real fault, and any other vector during a stop is a fault
    /// the stopping CPU took on its way down.
    #[test]
    fn is_stop_notice_is_vector_2_during_a_stop_and_nothing_else() {
        use super::is_stop_notice;

        assert!(is_stop_notice(2, true), "vector 2 during a stop is the notice");
        assert!(!is_stop_notice(2, false), "a hardware NMI outside a stop is a real fault");
        for v in [0u64, 1, 3, 6, 8, 13, 14, 32, 255] {
            assert!(!is_stop_notice(v, true), "vector {v} during a stop is not the notice");
            assert!(!is_stop_notice(v, false), "vector {v} outside a stop is not the notice");
        }
    }

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
        assert_eq!(pf_fault_kind(0b0000), FaultKind::NotMapped); // not present
        assert_eq!(pf_fault_kind(0b0001), FaultKind::NotReadable); // present, read
        assert_eq!(pf_fault_kind(0b0011), FaultKind::NotWritable); // present, write
        assert_eq!(pf_fault_kind(0b1_0001), FaultKind::NotExecutable); // present, insn fetch
        assert_eq!(pf_fault_kind(0b1_0010), FaultKind::NotMapped);
    }

    #[test]
    fn fault_shape_maps_vectors() {
        assert_eq!(fault_shape(0, 0, 0xAAAA, None), (KIND_DIVIDE_BY_ZERO, 0xAAAA, FaultKind::UnknownFault));
        assert_eq!(fault_shape(6, 0, 0xBBBB, None).0, KIND_ILLEGAL_INSN);
        let (k, addr, fk) = fault_shape(14, 0b0011, 0xCCCC, Some(0x1000));
        assert_eq!((k, addr, fk), (KIND_SEG_FAULT, 0x1000, FaultKind::NotWritable));
        assert_eq!(fault_shape(13, 0, 0xDDDD, None), (KIND_SEG_FAULT, 0xDDDD, FaultKind::UnknownFault));
    }

    #[test]
    fn stub_table_covers_all_32_vectors() {
        assert_eq!(STUBS.len(), 32);
    }
}
