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
//! A thread is shared, by the scheduler's lists and by its handles, and changes as it runs, so its
//! fields are split by who changes them:
//! - its **identity** (ids, entry, stack, root, process) is fixed when it is made;
//! - what **`SCHED` guards** is a [`SchedCell`], borrowed with the token the held lock keeps;
//! - its **wait phase**, **`on_cpu` guard** and **ended mark** are atomics, read on either side of
//!   the lock;
//! - its **parked context**, the saved stack pointer and the FP/SIMD image, is written by the CPU
//!   switching it out after `SCHED` is released, so it is reached only by address, under the
//!   `on_cpu` guard (see `Parked`).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crate::arch::{ArchFpuState, ArchThreadContext, RegisterValues, thread_trampoline};
use crate::libkern::{AllocError, KBox};
use crate::mm::PhysAddr;
use crate::mm::kstack::{KernelStack, KernelStackTop};
use crate::object::Process;
use crate::object::header::{Header, Ref, Shared, kobject};
use crate::sched::{SchedCell, SchedToken};

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
/// bit 0 in release) and `sys_thread_create`'s truncation of `cpu_affinity` **together**.
const _: () = assert!(
    crate::arch::MAX_CPUS <= 8,
    "MAX_CPUS > 8 does not fit Thread::cpu_mask (u8) — widen the field, the `1 << c` shifts \
     in sched/placement.rs, and sys_thread_create's mask trim together"
);

/// A thread kernel object, `#[repr(C)]` with [`KObjectHeader`] first. See "Mutation discipline".
#[repr(C)]
pub struct Thread {
    header: Header<Self>,
    tid: u32,
    owner_pid: u32,
    entry: ThreadEntry,
    arg: usize,
    /// Owned kernel stack; `None` for the boot thread (bootloader stack) and inert threads.
    stack: Option<KernelStack>,
    /// Page-table root to load when this thread runs: `None` for kernel/boot threads (the
    /// scheduler uses the boot root), `Some(root)` for a user thread.
    addr_space_root: Option<PhysAddr>,
    user_entry: Option<(u64, u64)>,
    /// Bootstrap registers (`rdi`, `rsi`, `rdx`, `rcx`) seeded at a user thread's ring-3 entry.
    user_boot_args: [u64; 4],
    /// Owning process, for a user thread. This [`Ref`] keeps the `Process` and its
    /// `AddressSpace` alive until the thread is reaped. No cycle: the `Process` does not hold the
    /// thread. `None` for kernel/boot threads.
    process: Option<Ref<Process>>,
    sched: SchedCell<ThreadSched>,
    /// The wait wake-handshake ([`WaitPhase`]), CAS'd by wakers.
    wait_phase: AtomicU8,
    /// **Mid-switch-out guard** (the SMP `on_cpu` invariant). Raised under `SCHED` just before a
    /// switch releases the run-queue lock, and cleared by
    /// [`context_switch`](crate::arch::context_switch) **after** it commits this thread's saved
    /// stack pointer. While raised, the parked context is **not yet valid to resume**: another CPU
    /// must not steal and run it (it would load a stale stack pointer and double-run the thread),
    /// so work-stealing skips it. Only the switch's asm clears it, through its storage's address.
    on_cpu: AtomicBool,
    /// Set on the thread whose exit **ends its process**, under `SCHED`; the reaper reads it,
    /// without the lock, to run the handle sweep
    /// ([`handle::global::close_all_owned_by`](crate::handle::global::close_all_owned_by)), which
    /// cannot run at exit time: the exiting thread holds rank-1 `SCHED` and never returns from
    /// `finish_exit`.
    process_ended: AtomicBool,
    parked: Parked,
}

kobject!(Thread);
// SAFETY: once shared, a `Thread`'s identity is never written; what changes is in `sched`, a
// `SchedCell`, in atomics, or in `parked`, whose cells are reached only by address.
unsafe impl Shared for Thread {}

/// What `SCHED` guards in a thread.
struct ThreadSched {
    state: ThreadState,
    /// Set when the thread's process is exiting: it ends itself at its next return to ring 3,
    /// letting go on the way of whatever its kernel stack holds, and its waits return at once.
    ending: bool,
    /// Wait bookkeeping, meaningful only while `Blocked`: the type-erased addresses this thread
    /// waits on, and per-slot fired flags set by the waker.
    wait_objs: [usize; MAX_WAIT_HANDLES],
    wait_signaled: [bool; MAX_WAIT_HANDLES],
    wait_count: u8,
    wait_has_deadline: bool,
    /// The user registers it last faulted with, copied as it suspended; meaningful while `Suspended`.
    fault_regs: RegisterValues,
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
    /// **enforced** by the assertion above [`Thread`]. Default all-ones.
    cpu_mask: u8,
}

/// A thread's **parked context**: the kernel stack pointer it resumes at, and its FP/SIMD register
/// file while it is not running, swapped eagerly by `sched::switch_into`. The CPU switching the
/// thread out writes both after `SCHED` is released, so neither is reached by reference: only by
/// address, under the `on_cpu` guard's protocol.
struct Parked {
    arch: UnsafeCell<ArchThreadContext>,
    /// `None` only for **inert** threads, never scheduled, whose area is null to the switch. Boxed
    /// because the area is 1 KiB and needs 64-byte alignment.
    fpu: Option<KBox<UnsafeCell<ArchFpuState>>>,
}

// SAFETY: the parked context is reached only by address, by the switch: the CPU switching the
// thread out raises its `on_cpu` guard under `SCHED` before it writes, and clears it once the
// writes are committed; a CPU switching it in reads only once it has seen the guard clear, and
// nothing else touches it. So no two CPUs reach it at once.
unsafe impl Sync for Parked {}

impl ThreadSched {
    fn new(state: ThreadState) -> Self {
        Self {
            state,
            ending: false,
            wait_objs: [0; MAX_WAIT_HANDLES],
            wait_signaled: [false; MAX_WAIT_HANDLES],
            wait_count: 0,
            wait_has_deadline: false,
            fault_regs: RegisterValues::default(),
            disp_tag: 0,
            disp_code: 0,
            class: SchedClass::TimeShared,
            rt_priority: 0,
            nice: 0,
            vruntime: 0,
            last_cpu: 0,
            cpu_mask: u8::MAX,
        }
    }
}

/// A thread's FP/SIMD save area, at the architectural power-on state: it is restored on the first
/// switch-in.
fn new_fpu_area() -> Result<KBox<UnsafeCell<ArchFpuState>>, AllocError> {
    KBox::try_new(UnsafeCell::new(ArchFpuState::initial()))
}

extern "C" fn inert_entry(_arg: usize) {}

impl Thread {
    /// An inert thread's fields, which the other constructors start from.
    fn inert(tid: u32, owner_pid: u32) -> Self {
        Self {
            header: Header::new(),
            tid,
            owner_pid,
            entry: inert_entry,
            arg: 0,
            stack: None,
            addr_space_root: None,
            user_entry: None,
            user_boot_args: [0; 4],
            process: None,
            sched: SchedCell::new(ThreadSched::new(ThreadState::Ready)),
            wait_phase: AtomicU8::new(WaitPhase::Running as u8),
            on_cpu: AtomicBool::new(false),
            process_ended: AtomicBool::new(false),
            parked: Parked { arch: UnsafeCell::new(ArchThreadContext::new()), fpu: None },
        }
    }

    /// Allocate an **inert** thread (no kernel stack, never scheduled), refcount one.
    pub fn try_new(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self::inert(tid, owner_pid))
    }

    /// Allocate the **boot** thread: the already-running boot context, adopted so the first
    /// [`context_switch`](crate::arch::context_switch) has a slot to save into. It owns no stack.
    pub fn try_new_boot(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        let fpu = new_fpu_area()?;
        let mut t = KBox::try_new(Self::inert(tid, owner_pid))?;
        t.sched.get_mut().state = ThreadState::Running;
        t.parked.fpu = Some(fpu);
        Ok(t)
    }

    /// Allocate a **runnable** thread: a fresh kernel stack and fabricated frame that runs
    /// `entry(arg)` via the trampoline.
    pub fn try_new_runnable(
        tid: u32,
        owner_pid: u32,
        entry: ThreadEntry,
        arg: usize,
    ) -> Result<KBox<Self>, AllocError> {
        let mut t = KBox::try_new(Self::inert(tid, owner_pid))?;
        (t.entry, t.arg) = (entry, arg);
        t.runnable()?;
        Ok(t)
    }

    /// Allocate a **user** thread: a fresh kernel stack and fabricated frame that lands in
    /// [`thread_enter`](crate::sched), which descends to ring 3 at `(entry, user_sp)`. `process`
    /// must be a live `Process` with an address space root; the held [`Ref`] keeps it alive.
    pub fn try_new_user(
        tid: u32,
        process: Ref<Process>,
        entry: u64,
        user_sp: u64,
        boot_args: [u64; 4],
    ) -> Result<KBox<Self>, AllocError> {
        let mut t = KBox::try_new(Self::user(tid, process, entry, user_sp, boot_args))?;
        t.runnable()?;
        Ok(t)
    }

    /// Give a thread its maker alone holds a fresh kernel stack, a frame fabricated on it that runs
    /// the trampoline, and an FP/SIMD area. Filled in place: a `Thread` is too large to build twice
    /// on a kernel stack.
    fn runnable(&mut self) -> Result<(), AllocError> {
        let mut stack = KernelStack::new()?;
        *self.parked.arch.get_mut() = stack.fabricate(thread_trampoline as *const () as u64);
        self.parked.fpu = Some(new_fpu_area()?);
        self.stack = Some(stack);
        Ok(())
    }

    /// A user thread's fields, its context and stack left to the caller. `entry` is unused: a user
    /// thread descends via `user_entry`.
    fn user(tid: u32, process: Ref<Process>, entry: u64, user_sp: u64, boot_args: [u64; 4]) -> Self {
        let (root, owner_pid) =
            (process.address_space_root().expect("user process must have an address space"), process.pid());
        Self {
            addr_space_root: Some(root),
            user_entry: Some((entry, user_sp)),
            user_boot_args: boot_args,
            process: Some(process),
            ..Self::inert(tid, owner_pid)
        }
    }

    /// Set the scheduling class, its parameters and the affinity of a thread its maker alone holds.
    pub(crate) fn set_policy(&mut self, class: SchedClass, rt_priority: u8, nice: i8, cpu_mask: u8) {
        let s = self.sched.get_mut();
        (s.class, s.rt_priority, s.nice, s.cpu_mask) = (class, rt_priority, nice, cpu_mask);
    }

    /// The thread identifier.
    pub fn tid(&self) -> u32 {
        self.tid
    }

    /// The id of the process this thread belongs to; `0` for a kernel/boot thread.
    pub fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    /// The process a user thread belongs to, which the thread pins; `None` for kernel/boot threads.
    pub fn process(&self) -> Option<&Process> {
        self.process.as_deref()
    }

    /// Clone the owning [`Process`](crate::object::Process) reference of a user thread; `None`
    /// for kernel/boot threads.
    pub fn process_ref(&self) -> Option<Ref<Process>> {
        self.process.clone()
    }

    /// The entry point and its argument.
    pub(crate) fn entry_and_arg(&self) -> (ThreadEntry, usize) {
        (self.entry, self.arg)
    }

    /// The page-table root to load when scheduling this thread in, or `None` to use the
    /// kernel/boot root (the scheduler resolves it).
    pub(crate) fn addr_space_root(&self) -> Option<PhysAddr> {
        self.addr_space_root
    }

    /// `Some((entry, user_sp))` if this is a user thread (descend to ring 3 on first run), else
    /// `None` (kernel thread).
    pub(crate) fn user_entry(&self) -> Option<(u64, u64)> {
        self.user_entry
    }

    /// The bootstrap argument registers (`[rdi, rsi, rdx, rcx]`) to seed at this user thread's
    /// first ring-3 entry. `[0; 4]` for kernel threads.
    pub(crate) fn user_boot_args(&self) -> [u64; 4] {
        self.user_boot_args
    }

    /// The top of this thread's kernel stack (the value for `TSS.RSP0` and the per-CPU syscall
    /// stack), or `None` for stackless boot/inert threads.
    pub(crate) fn kstack_top(&self) -> Option<KernelStackTop> {
        self.stack.as_ref().map(KernelStack::stack_top)
    }

    /// The scheduler lifecycle state.
    pub(crate) fn state(&self, tok: &SchedToken) -> ThreadState {
        self.sched.read(tok).state
    }

    /// Set the scheduler lifecycle state.
    pub(crate) fn set_state(&self, tok: &mut SchedToken, s: ThreadState) {
        self.sched.get(tok).state = s;
    }

    /// Mark this thread as the one whose exit ends its process, so the reaper knows to sweep the
    /// process's handles. Set under `SCHED`.
    pub(crate) fn set_process_ended(&self) {
        self.process_ended.store(true, Ordering::Release);
    }

    /// Whether this thread's exit ended its process, read by the reaper without `SCHED`.
    pub(crate) fn process_ended(&self) -> bool {
        self.process_ended.load(Ordering::Acquire)
    }

    /// Mark the thread to end itself at its next return to ring 3.
    pub(crate) fn set_ending(&self, tok: &mut SchedToken) {
        self.sched.get(tok).ending = true;
    }

    /// Whether the thread's process is exiting and it is to end itself.
    pub(crate) fn is_ending(&self, tok: &SchedToken) -> bool {
        self.sched.read(tok).ending
    }

    /// The thread's scheduling class.
    pub(crate) fn sched_class(&self, tok: &SchedToken) -> SchedClass {
        self.sched.read(tok).class
    }

    /// RealTime fixed priority (`0` highest); meaningful only for `RealTime`.
    pub(crate) fn rt_priority(&self, tok: &SchedToken) -> u8 {
        self.sched.read(tok).rt_priority
    }

    /// TimeShared niceness (`-20..=19`).
    pub(crate) fn nice(&self, tok: &SchedToken) -> i8 {
        self.sched.read(tok).nice
    }

    /// TimeShared virtual runtime.
    pub(crate) fn vruntime(&self, tok: &SchedToken) -> u64 {
        self.sched.read(tok).vruntime
    }

    /// Set the TimeShared virtual runtime.
    pub(crate) fn set_vruntime(&self, tok: &mut SchedToken, v: u64) {
        self.sched.get(tok).vruntime = v;
    }

    /// The dense index of the CPU this thread last ran on.
    pub(crate) fn last_cpu(&self, tok: &SchedToken) -> u8 {
        self.sched.read(tok).last_cpu
    }

    /// Record the CPU this thread is (about to be) running on.
    pub(crate) fn set_last_cpu(&self, tok: &mut SchedToken, cpu: u8) {
        self.sched.get(tok).last_cpu = cpu;
    }

    /// The CPU affinity bitmask (bit `c` ⇒ may run on CPU `c`).
    pub(crate) fn cpu_mask(&self, tok: &SchedToken) -> u8 {
        self.sched.read(tok).cpu_mask
    }

    /// Set the CPU affinity bitmask: a test's, as a thread's is set when it is made.
    #[cfg(test)]
    pub(crate) fn set_cpu_mask(&self, tok: &mut SchedToken, mask: u8) {
        self.sched.get(tok).cpu_mask = mask;
    }

    /// Record the registers this thread faulted with, as it suspends.
    pub(crate) fn set_fault_registers(&self, tok: &mut SchedToken, regs: RegisterValues) {
        self.sched.get(tok).fault_regs = regs;
    }

    /// The registers this thread last faulted with: its fault's while it is `Suspended`.
    pub(crate) fn fault_registers(&self, tok: &SchedToken) -> RegisterValues {
        self.sched.read(tok).fault_regs
    }

    /// Set the resume disposition (`sys_exception_resume`): tag `0` = Resume, `2` = Terminate
    /// (with `code`).
    pub(crate) fn set_disposition(&self, tok: &mut SchedToken, tag: u8, code: i32) {
        let s = self.sched.get(tok);
        (s.disp_tag, s.disp_code) = (tag, code);
    }

    /// The resume disposition `(tag, code)` set by `sys_exception_resume`, read when a suspended
    /// thread resumes.
    pub(crate) fn take_disposition(&self, tok: &SchedToken) -> (u8, i32) {
        let s = self.sched.read(tok);
        (s.disp_tag, s.disp_code)
    }

    /// Register `objs` (type-erased addresses, `len <= MAX_WAIT_HANDLES`) as this thread's wait
    /// set, all un-signaled, and set the wake phase to `Waiting`.
    pub(crate) fn wait_register(&self, tok: &mut SchedToken, objs: &[usize], has_deadline: bool) {
        let s = self.sched.get(tok);
        s.wait_objs[..objs.len()].copy_from_slice(objs);
        s.wait_signaled[..objs.len()].fill(false);
        s.wait_count = objs.len() as u8;
        s.wait_has_deadline = has_deadline;
        self.wait_phase.store(WaitPhase::Waiting as u8, Ordering::Release);
    }

    /// Mark the slot registered for object `target` signaled, if present. Returns `true` iff a
    /// slot matched.
    pub(crate) fn wait_mark_signaled(&self, tok: &mut SchedToken, target: usize) -> bool {
        let s = self.sched.get(tok);
        let n = s.wait_count as usize;
        let Some(i) = s.wait_objs[..n].iter().position(|&o| o == target) else { return false };
        s.wait_signaled[i] = true;
        true
    }

    /// Which of this wait's slots were signalled, by slot; none past the wait's count.
    pub(crate) fn wait_signaled(&self, tok: &SchedToken) -> [bool; MAX_WAIT_HANDLES] {
        let s = self.sched.read(tok);
        let n = s.wait_count as usize;
        let mut signaled = [false; MAX_WAIT_HANDLES];
        signaled[..n].copy_from_slice(&s.wait_signaled[..n]);
        signaled
    }

    /// `true` iff this wait registered a finite deadline (heap entry to remove on resume).
    pub(crate) fn wait_has_deadline(&self, tok: &SchedToken) -> bool {
        self.sched.read(tok).wait_has_deadline
    }

    /// Clear the wait set (count 0) and reset the wake phase to `Running`. Called on resume after
    /// the thread has unregistered from all objects.
    pub(crate) fn wait_clear(&self, tok: &mut SchedToken) {
        let s = self.sched.get(tok);
        s.wait_count = 0;
        s.wait_has_deadline = false;
        self.wait_phase.store(WaitPhase::Running as u8, Ordering::Release);
    }

    /// `true` if this caller won the CAS `Waiting → Woken` and should make the thread runnable.
    pub(crate) fn wait_try_wake(&self) -> bool {
        self.wait_phase
            .compare_exchange(WaitPhase::Waiting as u8, WaitPhase::Woken as u8, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The current wake phase. Test-only.
    #[cfg(test)]
    pub(crate) fn wait_phase(&self) -> u8 {
        self.wait_phase.load(Ordering::Acquire)
    }

    /// Raise the mid-switch-out guard (`on_cpu`; see the field docs), under `SCHED`, just before
    /// the run-queue lock is released for a switch. `Release`, so what the hold wrote is ordered
    /// before it. Only the switch clears it, through [`on_cpu_ptr`](Self::on_cpu_ptr).
    pub(crate) fn raise_on_cpu(&self, _: &SchedToken) {
        self.on_cpu.store(true, Ordering::Release);
    }

    /// Whether the mid-switch-out guard is raised: a stealer must treat `true` as "not yet
    /// resumable" and leave the thread on its queue. `Acquire` pairs with the switch's clearing
    /// store, so the committed context is visible once `false` is seen.
    pub(crate) fn is_on_cpu(&self) -> bool {
        self.on_cpu.load(Ordering::Acquire)
    }

    /// The `on_cpu` guard's storage, for [`context_switch`](crate::arch::context_switch) to
    /// clear once it has committed the saved stack pointer.
    pub(crate) fn on_cpu_ptr(&self) -> *mut u8 {
        self.on_cpu.as_ptr().cast()
    }

    /// The saved stack pointer of the parked context, or `None` while the thread is mid-switch-out
    /// and it may still be being written. A switch-in spins until it reads one.
    pub(crate) fn parked_sp(&self, _: &SchedToken) -> Option<u64> {
        if self.is_on_cpu() {
            return None;
        }
        // SAFETY: the context is written only by a switch-out, which raises the guard under
        // `SCHED` first and clears it once the write is committed: with `SCHED` held (the token)
        // and the guard seen clear, no write is under way, and none begins until the lock is let go.
        Some(unsafe { ArchThreadContext::saved_sp(self.parked.arch.get()) })
    }

    /// The parked context's saved-stack-pointer word, for `context_switch`'s store into the
    /// outgoing thread. Only the switch writes through it, with the thread's `on_cpu` guard raised.
    pub(crate) fn sp_slot(&self) -> *mut u64 {
        // SAFETY: the context lives as long as `self`; this forms no reference to it.
        unsafe { ArchThreadContext::sp_slot(self.parked.arch.get()) }
    }

    /// This thread's FP/SIMD save area, or **null** for an inert thread that has none. The switch
    /// passes it to [`fpu_save`](crate::arch::fpu_save) and [`fpu_restore`](crate::arch::fpu_restore)
    /// on the CPU switching the thread out or in, under the `on_cpu` guard that orders its saved
    /// stack pointer: the FP image is part of the same parked context.
    pub(crate) fn fpu_area(&self) -> *mut ArchFpuState {
        self.parked.fpu.as_deref().map_or(core::ptr::null_mut(), UnsafeCell::get)
    }
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests;
