//! The [`Thread`] kernel object.
//!
//! A thread is a register state, kernel stack, scheduling state, entry point, owning process,
//! wait bookkeeping and — when schedulable — its FP/SIMD register file, swapped eagerly by the
//! scheduler on each context switch.
//! TLS (`fs_base`) is not yet a field: `sys_thread_set_tls` is deferred with the `std` port
//! (`docs/rationale/deferred-decisions.md`).
//!
//! ## Mutation discipline
//!
//! [`Thread`] is shared through an [`ObjectRef`](crate::object::ObjectRef), yet the scheduler
//! mutates `arch`, `state`, `entry` and `arg`, sound only because it does so exclusively while
//! holding its run-queue lock. The scheduler-only accessors take a type-erased `*mut ()` and
//! reach fields through raw pointers, never forming a `&mut Thread` that would alias the
//! atomically-accessed [`KObjectHeader`].

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::arch::paging::ArchPaging;
use crate::arch::{ArchFpuState, ArchThreadContext, fabricate_frame, thread_trampoline};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::mm::PhysAddr;
use crate::mm::kstack::KernelStack;
use crate::object::ObjectRef;
use crate::object::Process;
use crate::object::header::KObjectHeader;

/// Maximum number of handles one `sys_wait` call may block on. Bounds the per-thread wait
/// arrays (so registration never allocates under the scheduler lock) and a caller's `count`.
///
/// **This is the fan-out limit of every resource server**: a server waiting on its serving
/// endpoint plus one slot per client serves at most `MAX_WAIT_HANDLES - 1` clients at once.
/// Userspace mirrors it as `libkern::abi::MAX_WAIT_HANDLES`, and both fan-out servers derive
/// their caps from it (decision log, 2026-07-29). The arrays are fixed, paid per thread: at 32
/// they are 288 B, against the 1 KiB `ArchFpuState` every thread carries. Escaping the limit
/// rather than raising it is `TODO(server-fanout)` in `docs/rationale/deferred-decisions.md`.
pub const MAX_WAIT_HANDLES: usize = 32;

/// The second cost of raising [`MAX_WAIT_HANDLES`]: `sys_wait` and the scheduler hold
/// **several** arrays of this width on the **kernel stack** (~3 KiB at 32 against 16 KiB,
/// growing linearly). This check fails the **build** rather than faulting on the guard page;
/// raising past it means growing `KERNEL_STACK_PAGES` or moving the arrays off the stack.
const _: () = {
    // Bytes of kernel stack the wait path may spend on these arrays: a quarter.
    const WAIT_STACK_BUDGET: usize = (crate::mm::kstack::KERNEL_STACK_BYTES as usize) / 4;
    // Per element across the live arrays: handles (8) + copied bytes (8) + refs (16) + objs (8) +
    // types (1) + IoResult out (24) + `(usize, bool)` snapshot (16) + signaled bits (1).
    const PER_HANDLE: usize = 8 + 8 + 16 + 8 + 1 + 24 + 16 + 1;
    assert!(
        MAX_WAIT_HANDLES * PER_HANDLE <= WAIT_STACK_BUDGET,
        "MAX_WAIT_HANDLES too large for the kernel stack — grow KERNEL_STACK_PAGES or \
         move the wait arrays off the stack"
    );
};

/// A kernel thread's entry point (`extern "C"` for the trampoline); the argument is opaque.
pub type ThreadEntry = extern "C" fn(arg: usize);

/// Lifecycle state of a thread, tracked by the scheduler.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ThreadState {
    /// On the run queue, not currently executing.
    Ready,
    /// The currently executing thread.
    Running,
    /// Off the run queue, blocked in `sys_wait`; parked in the scheduler's `blocked` list.
    Blocked,
    /// Off the run queue after a ring-3 fault, `ExceptionFrame` preserved, awaiting a resume.
    Suspended,
    /// Body returned or `exit` called; awaiting reclamation by the next scheduler entry.
    Exited,
}

/// A thread's **scheduling class**, in strict dispatch precedence: `RealTime` preempts
/// `TimeShared`, which preempts `Idle`. See `docs/architecture/scheduler.md` §Scheduling classes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum SchedClass {
    /// Fixed-priority FIFO (`0..=99`, `0` highest), `REAL_TIME`-gated (deferred); no vruntime.
    RealTime = 0,
    /// CFS-like vruntime-fair scheduling — the default for every thread.
    TimeShared = 1,
    /// The per-CPU idle thread (`hlt`); never enqueued in `ready`.
    Idle = 2,
}

/// The wait wake-handshake state, an [`AtomicU8`] per [`Thread`], so a waker (under `SCHED`)
/// and the blocking thread agree on whether it was claimed for wakeup: `Running` (not waiting),
/// `Waiting` (registered, about to block), `Woken` (claimed by a signal or the deadline). The
/// CAS `Waiting → Woken` ([`Thread::wait_try_wake`]) lets only the first of several signals
/// for one multi-handle wait make the thread runnable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum WaitPhase {
    Running = 0,
    Waiting = 1,
    Woken = 2,
}

/// A dense CPU index must fit in [`Thread`]'s `cpu_mask` (a `u8`): raising `MAX_CPUS` past 8
/// means widening that field, the shifts that read it (`1u8 << 8` panics in debug and wraps to
/// bit 0 in release) and `sys_thread_set_affinity`'s truncation **together**.
const _: () = assert!(
    crate::arch::MAX_CPUS <= 8,
    "MAX_CPUS > 8 does not fit Thread::cpu_mask (u8) — widen the field, the `1 << c` shifts \
     in sched/placement.rs, and sys_thread_set_affinity's mask trim together"
);

/// A thread kernel object, `#[repr(C)]` with [`KObjectHeader`] first. The fields after the
/// identity pair are scheduler-owned (see "Mutation discipline").
#[repr(C)]
pub struct Thread {
    header: KObjectHeader,
    tid: u32,
    owner_pid: u32,
    arch: ArchThreadContext,
    state: ThreadState,
    entry: ThreadEntry,
    arg: usize,
    /// Owned kernel stack; `None` for the boot thread (bootloader stack) and inert threads.
    stack: Option<KernelStack>,
    /// Page-table root to load when this thread runs: `None` for kernel/boot threads (the
    /// scheduler uses its cached boot root), `Some(root)` for a user thread.
    addr_space_root: Option<PhysAddr>,
    user_entry: Option<(u64, u64)>,
    /// Bootstrap registers (`rdi`, `rsi`, `rdx`, `rcx`) seeded at a user thread's ring-3 entry.
    user_boot_args: [u64; 4],
    /// Set on the thread whose exit **ends its process**; the reaper reads it to run the handle
    /// sweep ([`handle::global::close_all_owned_by`](crate::handle::global::close_all_owned_by)),
    /// which cannot run at exit time: the exiting thread holds rank-1 `SCHED` and never returns
    /// from `finish_exit`.
    process_ended: bool,
    /// Owning process, for a user thread. This [`ObjectRef`] keeps the `Process` and its
    /// `AddressSpace` alive until the thread is reaped. No cycle: the `Process` does not hold the
    /// thread. `None` for kernel/boot threads.
    process: Option<ObjectRef>,
    /// Wait bookkeeping (meaningful only while `Blocked`), touched only under `SCHED` except
    /// `wait_phase`: the type-erased addresses this thread waits on, and per-slot fired flags set
    /// by the waker.
    wait_objs: [usize; MAX_WAIT_HANDLES],
    wait_signaled: [bool; MAX_WAIT_HANDLES],
    wait_count: u8,
    wait_has_deadline: bool,
    wait_phase: AtomicU8,
    /// Kernel-stack address of the `ExceptionFrame` captured when this thread faulted in ring 3
    /// (meaningful only while `Suspended`); the supervisor reads it, the thread unwinds it.
    exception_frame: usize,
    /// Resume disposition set by `sys_exception_resume`: `0` = Resume, `2` = Terminate.
    disp_tag: u8,
    disp_code: i32,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
    vruntime: u64,
    /// The dense index of the CPU this thread **last ran on**: a waking thread returns there.
    last_cpu: u8,
    /// CPU **affinity** bitmask (bit `c` ⇒ may run on CPU `c`); a `u8` because `MAX_CPUS ≤ 8`,
    /// **enforced** by the assertion above this struct. Default all-ones.
    cpu_mask: u8,
    /// **Mid-switch-out guard** (the SMP `on_cpu` invariant). Set `true` under `SCHED` just before
    /// a switch releases the run-queue lock, and cleared by
    /// [`context_switch`](crate::arch::context_switch) **after** it commits this thread's
    /// `saved_sp`. While `true`, the parked context is **not yet valid to resume**: another CPU
    /// must not steal and run it (it would load a stale `saved_sp` and double-run the thread), so
    /// work-stealing skips it. An [`AtomicBool`] whose storage the asm pokes as a raw `u8`.
    on_cpu: AtomicBool,
    /// This thread's FP/SIMD register file while it is not running, swapped eagerly by
    /// `sched::switch_into`. `None` only for **inert** threads, never scheduled, so the switch path
    /// treats a null area as "nothing to swap". Boxed because the area is 1 KiB and needs 64-byte
    /// alignment. Written only by the CPU running the thread, and only while its `on_cpu` guard is
    /// raised, so a stealer that observes the guard clear also observes the finished image.
    fpu: Option<KBox<ArchFpuState>>,
}

/// Allocate and initialise a thread's FP/SIMD save area to the architectural power-on state (it
/// is restored on the first switch-in, so merely zeroed would not do).
fn new_fpu_area() -> Result<KBox<ArchFpuState>, AllocError> {
    let mut area = KBox::try_new(ArchFpuState::zeroed())?;
    // SAFETY: `area` is a live, uniquely owned, correctly aligned `ArchFpuState`
    // (`KBox` allocates at `align_of::<ArchFpuState>()` = 64).
    unsafe { crate::arch::fpu_init_area(&raw mut *area) };
    Ok(area)
}

extern "C" fn inert_entry(_arg: usize) {}

impl Thread {
    /// Allocate an **inert** thread (no kernel stack, never scheduled), refcount one.
    pub fn try_new(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch: ArchThreadContext::new(),
            state: ThreadState::Ready,
            entry: inert_entry,
            arg: 0,
            stack: None,
            addr_space_root: None,
            user_entry: None,
            user_boot_args: [0; 4],
            process_ended: false,
            process: None,
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            exception_frame: 0,
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
            on_cpu: AtomicBool::new(false),
            fpu: None,
        })
    }

    /// Allocate the **boot** thread: the already-running boot context, adopted so the first
    /// [`context_switch`](crate::arch::context_switch) has a slot to save into. It owns no stack.
    pub fn try_new_boot(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch: ArchThreadContext::new(),
            state: ThreadState::Running,
            entry: inert_entry,
            arg: 0,
            stack: None,
            addr_space_root: None,
            user_entry: None,
            user_boot_args: [0; 4],
            process_ended: false,
            process: None,
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            exception_frame: 0,
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
            on_cpu: AtomicBool::new(false),
            fpu: Some(new_fpu_area()?),
        })
    }

    /// Allocate a **runnable** thread: a fresh kernel stack and fabricated frame that runs
    /// `entry(arg)` via the trampoline.
    pub fn try_new_runnable(
        tid: u32,
        owner_pid: u32,
        entry: ThreadEntry,
        arg: usize,
    ) -> Result<KBox<Self>, AllocError> {
        let stack = KernelStack::new(crate::arch::Paging::active_root())?;
        let top = stack.top().as_u64();
        // SAFETY: a freshly allocated `KernelStack` has its top
        // `KERNEL_STACK_PAGES` pages mapped writable in the shared kernel
        // vmap; `fabricate_frame` writes only the seven `u64` slots in
        // `[top - 56, top)`. `top` is page-aligned, hence 8-aligned. The
        // returned `ArchThreadContext` is stored opaquely.
        let arch = unsafe { fabricate_frame(top, thread_trampoline as *const () as u64) };
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch,
            state: ThreadState::Ready,
            entry,
            arg,
            stack: Some(stack),
            addr_space_root: None,
            user_entry: None,
            user_boot_args: [0; 4],
            process_ended: false,
            process: None,
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            exception_frame: 0,
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
            on_cpu: AtomicBool::new(false),
            fpu: Some(new_fpu_area()?),
        })
    }

    /// Allocate a **user** thread: a fresh kernel stack and fabricated frame that lands in
    /// [`thread_enter`](crate::sched), which descends to ring 3 at `(entry, user_sp)`. `process`
    /// must be a live `Process` with an address space root; the held [`ObjectRef`] keeps it alive.
    pub fn try_new_user(
        tid: u32,
        process: ObjectRef,
        entry: u64,
        user_sp: u64,
        boot_args: [u64; 4],
    ) -> Result<KBox<Self>, AllocError> {
        let (owner_pid, root) = {
            let p = process.as_ptr() as *const Process;
            // SAFETY: `process` pins a live `Process`, whose pid and root nothing mutates.
            let pid = unsafe { (*p).pid() };
            // SAFETY: as above.
            let root = unsafe { (*p).address_space_root() }.expect("user process must have an address space");
            (pid, root)
        };
        let stack = KernelStack::new(root)?;
        let top = stack.top().as_u64();
        // SAFETY: as `try_new_runnable` — a fresh KernelStack top has the
        // seven fabricated-frame slots writable; `top` is page-aligned.
        let arch = unsafe { fabricate_frame(top, thread_trampoline as *const () as u64) };
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch,
            state: ThreadState::Ready,
            entry: inert_entry, // unused: user threads descend via user_entry
            arg: 0,
            process_ended: false,
            stack: Some(stack),
            addr_space_root: Some(root),
            user_entry: Some((entry, user_sp)),
            user_boot_args: boot_args,
            process: Some(process),
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            exception_frame: 0,
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
            on_cpu: AtomicBool::new(false),
            fpu: Some(new_fpu_area()?),
        })
    }

    /// The thread identifier.
    pub fn tid(&self) -> u32 {
        self.tid
    }

    /// The id of the process this thread belongs to.
    pub fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    /// Clone the owning [`Process`](crate::object::Process) reference of a user thread; `None`
    /// for kernel/boot threads.
    pub fn process_ref(&self) -> Option<ObjectRef> {
        self.process.clone()
    }

    /// `true` if this is a user thread. Unlike [`process_ref`](Self::process_ref) it touches no
    /// refcount, so it is safe with the rank-1 `SCHED` lock held.
    pub fn has_process(&self) -> bool {
        self.process.is_some()
    }

    // SAFETY (shared by all): `obj` addresses a live `Thread` (pinned by an
    // `ObjectRef` the caller holds), and the scheduler invokes these only
    // while holding its run-queue lock, which — single-CPU — serialises all
    // access to these fields.

    /// Read the scheduler lifecycle state. Test-only: production code sets it but never reads it.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[cfg(test)]
    pub(crate) unsafe fn state(obj: *mut ()) -> ThreadState {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).state) }
    }

    /// Set the scheduler lifecycle state.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_state(obj: *mut (), s: ThreadState) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).state, s) }
    }

    /// Mark this thread as the one whose exit ends its process, so the reaper knows
    /// to sweep the process's handles. See [`process_ended`](Self::process_ended).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_process_ended(obj: *mut ()) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).process_ended, true) }
    }

    /// Whether this thread's exit ended its process (set by the exit paths under
    /// `SCHED`; read by the reaper, which then closes the process's handles).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn process_ended(obj: *mut ()) -> bool {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).process_ended) }
    }

    /// The thread's scheduling class.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn sched_class(obj: *mut ()) -> SchedClass {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).class) }
    }

    /// RealTime fixed priority (`0` highest); meaningful only for `RealTime`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn rt_priority(obj: *mut ()) -> u8 {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).rt_priority) }
    }

    /// TimeShared niceness (`-20..=19`).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn nice(obj: *mut ()) -> i8 {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).nice) }
    }

    /// TimeShared virtual runtime.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn vruntime(obj: *mut ()) -> u64 {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).vruntime) }
    }

    /// Set the TimeShared virtual runtime.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_vruntime(obj: *mut (), v: u64) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).vruntime, v) }
    }

    /// Set the scheduling class and its parameters (the `spawn`-with-class path).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_sched(obj: *mut (), class: SchedClass, rt_priority: u8, nice: i8) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe {
            core::ptr::write(&raw mut (*p).class, class);
            core::ptr::write(&raw mut (*p).rt_priority, rt_priority);
            core::ptr::write(&raw mut (*p).nice, nice);
        }
    }

    /// The dense index of the CPU this thread last ran on.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn last_cpu(obj: *mut ()) -> u8 {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).last_cpu) }
    }

    /// Record the CPU this thread is (about to be) running on.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_last_cpu(obj: *mut (), cpu: u8) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).last_cpu, cpu) }
    }

    /// The CPU affinity bitmask (bit `c` ⇒ may run on CPU `c`).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn cpu_mask(obj: *mut ()) -> u8 {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).cpu_mask) }
    }

    /// Set the CPU affinity bitmask (`sys_thread_set_affinity`).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_cpu_mask(obj: *mut (), mask: u8) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).cpu_mask, mask) }
    }

    /// Read the lifecycle state (for the teardown sibling scan and suspend/resume checks).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn state_now(obj: *mut ()) -> ThreadState {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).state) }
    }

    /// Record the kernel-stack address of the faulting `ExceptionFrame` when this
    /// thread suspends.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_exception_frame(obj: *mut (), frame: usize) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::write(&raw mut (*p).exception_frame, frame) }
    }

    /// The stored `ExceptionFrame` address, or `None` if this thread is not
    /// suspended on a fault.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn exception_frame(obj: *mut ()) -> Option<usize> {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        let v = unsafe { core::ptr::read(&raw const (*p).exception_frame) };
        if v == 0 { None } else { Some(v) }
    }

    /// Set the resume disposition (`sys_exception_resume`): tag `0` = Resume,
    /// `2` = Terminate (with `code`).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_disposition(obj: *mut (), tag: u8, code: i32) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe {
            core::ptr::write(&raw mut (*p).disp_tag, tag);
            core::ptr::write(&raw mut (*p).disp_code, code);
        }
    }

    /// Read the resume disposition `(tag, code)` set by `sys_exception_resume`,
    /// and clear the stored frame (the thread is unwinding it). Read when a
    /// suspended thread resumes.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_disposition(obj: *mut ()) -> (u8, i32) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe {
            let tag = core::ptr::read(&raw const (*p).disp_tag);
            let code = core::ptr::read(&raw const (*p).disp_code);
            core::ptr::write(&raw mut (*p).exception_frame, 0);
            (tag, code)
        }
    }

    /// Read the saved stack pointer of this thread's parked context.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn saved_sp(obj: *mut ()) -> u64 {
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a live Thread; the arch layer owns the
        // representation of the saved stack pointer.
        unsafe { ArchThreadContext::saved_sp(&raw const (*p).arch) }
    }

    /// A raw pointer to the parked context's saved-stack-pointer word, for
    /// `context_switch`'s store into the outgoing thread. The pointee
    /// outlives the call because the `Thread` is refcount-pinned by the
    /// caller for the duration of the switch.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn saved_sp_mut_ptr(obj: *mut ()) -> *mut u64 {
        let p = obj as *mut Thread;
        // SAFETY: as `saved_sp`; the arch layer yields the slot pointer.
        unsafe { ArchThreadContext::sp_slot(&raw mut (*p).arch) }
    }

    /// Set the mid-switch-out guard (`on_cpu`; see the field docs). The
    /// scheduler sets `true` under `SCHED` immediately before releasing the
    /// run-queue lock for a switch; [`context_switch`](crate::arch::context_switch)
    /// clears it after committing `saved_sp`.
    ///
    /// # Safety
    /// See the accessor contract above (`obj` is a live, pinned `Thread`).
    pub(crate) unsafe fn set_on_cpu(obj: *mut (), val: bool) {
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a live Thread; `on_cpu` is an `AtomicBool` valid for
        // the thread's lifetime. `Release` so a prior `saved_sp` publish is
        // ordered before a `true`→visible transition seen by a stealer's acquire.
        unsafe { (*p).on_cpu.store(val, Ordering::Release) };
    }

    /// Read the mid-switch-out guard. A stealer must treat `true` as "not yet
    /// resumable" and leave the thread on its queue.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn is_on_cpu(obj: *mut ()) -> bool {
        let p = obj as *mut Thread;
        // SAFETY: as `set_on_cpu`. `Acquire` pairs with the arch switch's
        // release-store clearing the flag (so the committed `saved_sp` is
        // visible once we observe `false`).
        unsafe { (*p).on_cpu.load(Ordering::Acquire) }
    }

    /// Raw `*mut u8` to the `on_cpu` guard's storage, for the arch
    /// [`context_switch`](crate::arch::context_switch) to clear (an
    /// `AtomicBool` is `repr(transparent)` over `u8`). The pointee outlives the
    /// call: the `Thread` is refcount-pinned for the switch's duration.
    ///
    /// # Safety
    /// See the accessor contract above; the returned pointer is only for the
    /// switch's single relaxed byte store on this CPU.
    pub(crate) unsafe fn on_cpu_ptr(obj: *mut ()) -> *mut u8 {
        let p = obj as *mut Thread;
        // SAFETY: `on_cpu` is an `AtomicBool` (a single `u8`); expose its byte.
        unsafe { (&raw mut (*p).on_cpu) as *mut u8 }
    }

    /// A raw pointer to this thread's FP/SIMD save area, or **null** for an
    /// inert thread that has none.
    ///
    /// The switch path passes it straight to [`fpu_save`](crate::arch::fpu_save) /
    /// [`fpu_restore`](crate::arch::fpu_restore), skipping the swap on null. The pointee outlives
    /// the switch: the caller refcount-pins the `Thread`, and the `KBox` drops only with it.
    ///
    /// # Safety
    /// See the accessor contract above. The returned pointer may be used only for the switch's
    /// save/restore on the CPU running this thread, under the `on_cpu` guard discipline that
    /// already protects `saved_sp` — the FP image is part of the same parked context.
    pub(crate) unsafe fn fpu_area_ptr(obj: *mut ()) -> *mut ArchFpuState {
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a live Thread. Reading the `Option`'s payload pointer
        // does not move or alias the box; a `None` yields null.
        unsafe {
            match &(*p).fpu {
                Some(area) => &raw const **area as *mut ArchFpuState,
                None => core::ptr::null_mut(),
            }
        }
    }

    /// Read the entry point and its argument.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn entry_and_arg(obj: *mut ()) -> (ThreadEntry, usize) {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { (core::ptr::read(&raw const (*p).entry), core::ptr::read(&raw const (*p).arg)) }
    }

    /// The page-table root to load when scheduling this thread in, or `None`
    /// to use the kernel/boot root (the scheduler resolves it).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn addr_space_root(obj: *mut ()) -> Option<PhysAddr> {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).addr_space_root) }
    }

    /// `Some((entry, user_sp))` if this is a user thread (descend to ring 3
    /// on first run), else `None` (kernel thread).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn user_entry(obj: *mut ()) -> Option<(u64, u64)> {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).user_entry) }
    }

    /// The bootstrap argument registers (`[rdi, rsi, rdx, rcx]`) to seed at this user
    /// thread's first ring-3 entry. `[0; 4]` for kernel threads.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn user_boot_args(obj: *mut ()) -> [u64; 4] {
        let p = obj as *mut Thread;
        // SAFETY: the accessor contract: `obj` is a live `Thread` its caller pins, and `SCHED` is held.
        unsafe { core::ptr::read(&raw const (*p).user_boot_args) }
    }

    /// The top of this thread's kernel stack (the value for `TSS.RSP0` and
    /// the per-CPU syscall stack), or `None` for stackless boot/inert
    /// threads.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn kstack_top(obj: *mut ()) -> Option<u64> {
        let p = obj as *mut Thread;
        // SAFETY: read the Option<KernelStack> by reference (not by value —
        // KernelStack is not Copy and owns mappings) and project to its top.
        unsafe { (*p).stack.as_ref().map(|s| s.top().as_u64()) }
    }

    /// This thread's owning process id, or `0` for a kernel/boot thread. Raw-pointer
    /// form of [`Thread::owner_pid`], for the scheduler's stack-watermark attribution —
    /// a depth figure is only actionable if it names whose thread reached it.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[cfg(feature = "test-harness")]
    pub(crate) unsafe fn owner_pid_of(obj: *mut ()) -> u32 {
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a pinned live `Thread` under the accessor contract.
        unsafe { (*p).owner_pid() }
    }

    /// Register `objs` (type-erased addresses, `len <= MAX_WAIT_HANDLES`) as this thread's wait
    /// set, all un-signaled, and set the wake phase to `Waiting`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn wait_register(obj: *mut (), objs: &[usize], has_deadline: bool) {
        debug_assert!(objs.len() <= MAX_WAIT_HANDLES);
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a live Thread; SCHED serialises these field writes.
        unsafe {
            for (i, &o) in objs.iter().enumerate() {
                (*p).wait_objs[i] = o;
                (*p).wait_signaled[i] = false;
            }
            (*p).wait_count = objs.len() as u8;
            (*p).wait_has_deadline = has_deadline;
            (*p).wait_phase.store(WaitPhase::Waiting as u8, Ordering::Release);
        }
    }

    /// Mark the slot registered for object `target` signaled, if present.
    /// Returns `true` iff a slot matched.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn wait_mark_signaled(obj: *mut (), target: usize) -> bool {
        let p = obj as *mut Thread;
        // SAFETY: live Thread, SCHED held.
        unsafe {
            let n = (*p).wait_count as usize;
            for i in 0..n {
                if (*p).wait_objs[i] == target {
                    (*p).wait_signaled[i] = true;
                    return true;
                }
            }
        }
        false
    }

    /// Copy the per-slot `(object_addr, signaled)` pairs into `out` and return
    /// the wait count. `out` must be at least `MAX_WAIT_HANDLES` long.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn wait_snapshot(obj: *mut (), out: &mut [(usize, bool)]) -> usize {
        let p = obj as *mut Thread;
        // SAFETY: live Thread, SCHED held.
        unsafe {
            let n = (*p).wait_count as usize;
            debug_assert!(out.len() >= n);
            for (i, o) in out.iter_mut().enumerate().take(n) {
                *o = ((*p).wait_objs[i], (*p).wait_signaled[i]);
            }
            n
        }
    }

    /// `true` iff this wait registered a finite deadline (heap entry to remove
    /// on resume).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn wait_has_deadline(obj: *mut ()) -> bool {
        let p = obj as *mut Thread;
        // SAFETY: live Thread, SCHED held.
        unsafe { core::ptr::read(&raw const (*p).wait_has_deadline) }
    }

    /// Clear the wait set (count 0) and reset the wake phase to `Running`.
    /// Called on resume after the thread has unregistered from all objects.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn wait_clear(obj: *mut ()) {
        let p = obj as *mut Thread;
        // SAFETY: live Thread, SCHED held.
        unsafe {
            (*p).wait_count = 0;
            (*p).wait_has_deadline = false;
            (*p).wait_phase.store(WaitPhase::Running as u8, Ordering::Release);
        }
    }

    /// `true` if this caller won the CAS and should make the thread runnable.
    ///
    /// # Safety
    /// See the accessor contract above (the `AtomicU8` is valid for the live
    /// Thread; the CAS is the cross-context handshake).
    pub(crate) unsafe fn wait_try_wake(obj: *mut ()) -> bool {
        let p = obj as *mut Thread;
        // SAFETY: live Thread; `wait_phase` is an atomic, so a shared borrow to
        // CAS it is sound even though other fields are SCHED-serialised.
        unsafe {
            (*p).wait_phase
                .compare_exchange(WaitPhase::Waiting as u8, WaitPhase::Woken as u8, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        }
    }

    /// Read the current wake phase. Test-only.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[cfg(test)]
    pub(crate) unsafe fn wait_phase(obj: *mut ()) -> u8 {
        let p = obj as *mut Thread;
        // SAFETY: live Thread; atomic load.
        unsafe { (*p).wait_phase.load(Ordering::Acquire) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn inert_thread_has_no_stack_and_is_ready() {
        init_global_heap();
        let t = Thread::try_new(7, 3).unwrap();
        assert_eq!(t.tid(), 7);
        assert_eq!(t.owner_pid(), 3);
        assert!(t.stack.is_none());
        assert_eq!(t.state, ThreadState::Ready);
    }

    #[test]
    fn boot_thread_is_running_with_no_stack() {
        init_global_heap();
        let t = Thread::try_new_boot(0, 0).unwrap();
        assert_eq!(t.state, ThreadState::Running);
        assert!(t.stack.is_none());
    }

    #[test]
    fn state_accessors_round_trip() {
        init_global_heap();
        let t = Thread::try_new(1, 1).unwrap();
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: `obj` is a live Thread; this test is single-threaded and
        // holds the only reference, satisfying the accessor contract.
        unsafe {
            assert_eq!(Thread::state(obj), ThreadState::Ready);
            Thread::set_state(obj, ThreadState::Running);
            assert_eq!(Thread::state(obj), ThreadState::Running);
            Thread::set_state(obj, ThreadState::Exited);
            assert_eq!(Thread::state(obj), ThreadState::Exited);
            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
        }
    }

    #[test]
    fn saved_sp_accessor_reads_and_writes_through_slot() {
        init_global_heap();
        let t = Thread::try_new(1, 1).unwrap();
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: as above.
        unsafe {
            assert_eq!(Thread::saved_sp(obj), 0);
            *Thread::saved_sp_mut_ptr(obj) = 0xDEAD_0000;
            assert_eq!(Thread::saved_sp(obj), 0xDEAD_0000);
            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
        }
    }

    fn user_process_ref(pid: u32) -> (ObjectRef, crate::mm::PhysAddr) {
        use crate::mm::addr_space::AddressSpace;
        let asp = AddressSpace::new().unwrap();
        let root = asp.root();
        let p = Process::try_new_user(pid, asp, crate::libkern::SysCaps::empty()).unwrap();
        let ptr = KBox::into_raw(p).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        (unsafe { ObjectRef::from_raw(ptr, KObjectType::Process) }, root)
    }

    /// A user thread built **without** a kernel stack, avoiding the QEMU-only `fabricate_frame`,
    /// which writes a kernel-vmap address that is not host memory.
    fn user_thread_no_stack(tid: u32, process: ObjectRef, entry: u64, user_sp: u64) -> KBox<Thread> {
        let root = {
            let p = process.as_ptr() as *const Process;
            // SAFETY: `process` is a live Process ref this test holds.
            unsafe { (*p).address_space_root() }.unwrap()
        };
        KBox::try_new(Thread {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid: 0,
            arch: ArchThreadContext::new(),
            state: ThreadState::Ready,
            entry: inert_entry,
            arg: 0,
            stack: None,
            addr_space_root: Some(root),
            user_entry: Some((entry, user_sp)),
            user_boot_args: [0; 4],
            process_ended: false,
            process: Some(process),
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            exception_frame: 0,
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
            on_cpu: AtomicBool::new(false),
            fpu: Some(new_fpu_area().expect("fpu area")),
        })
        .unwrap()
    }

    #[test]
    fn user_thread_records_descent_params_and_root() {
        init_global_heap();
        let (proc_ref, root) = user_process_ref(1);
        let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: live Thread, single-threaded test.
        unsafe {
            assert_eq!(Thread::user_entry(obj), Some((0x40_0000, 0x7fff_f000)));
            assert_eq!(Thread::addr_space_root(obj), Some(root));
            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
        }
    }

    #[test]
    fn dropping_a_user_thread_releases_its_process_no_cycle() {
        use crate::object::header::test_probe;
        init_global_heap();
        test_probe::reset();
        let (proc_ref, _root) = user_process_ref(1);
        let t = user_thread_no_stack(1, proc_ref, 0x40_0000, 0x7fff_f000);
        let tref = {
            let ptr = KBox::into_raw(t).as_ptr() as *mut ();
            // SAFETY: into_raw yielded the single creation ref; adopt it.
            unsafe { ObjectRef::from_raw(ptr, KObjectType::Thread) }
        };
        assert_eq!(test_probe::process_destroys(), 0);
        assert_eq!(test_probe::thread_destroys(), 0);
        drop(tref);
        assert_eq!(test_probe::thread_destroys(), 1);
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn wait_bookkeeping_round_trips() {
        init_global_heap();
        let t = Thread::try_new(1, 1).unwrap();
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: live Thread, single-threaded test (stands in for SCHED).
        unsafe {
            assert_eq!(Thread::wait_phase(obj), WaitPhase::Running as u8);

            Thread::wait_register(obj, &[0xAA, 0xBB], true);
            assert_eq!(Thread::wait_phase(obj), WaitPhase::Waiting as u8);
            assert!(Thread::wait_has_deadline(obj));

            assert!(Thread::wait_mark_signaled(obj, 0xBB));
            assert!(!Thread::wait_mark_signaled(obj, 0xCC));

            let mut out = [(0usize, false); MAX_WAIT_HANDLES];
            let n = Thread::wait_snapshot(obj, &mut out);
            assert_eq!(n, 2);
            assert_eq!(out[0], (0xAA, false));
            assert_eq!(out[1], (0xBB, true));

            assert!(Thread::wait_try_wake(obj));
            assert!(!Thread::wait_try_wake(obj));
            assert_eq!(Thread::wait_phase(obj), WaitPhase::Woken as u8);

            Thread::wait_clear(obj);
            assert_eq!(Thread::wait_phase(obj), WaitPhase::Running as u8);
            assert!(!Thread::wait_has_deadline(obj));
            assert_eq!(Thread::wait_snapshot(obj, &mut out), 0);

            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Thread)));
        }
    }
}
