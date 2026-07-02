//! Kernel-thread context switch, emitted from Rust. Shared by the cooperative
//! (`yield_now`/`exit`) and preemptive (timer-IRQ) scheduler paths.
//!
//! The switch is a `#[unsafe(naked)]` function using `naked_asm!`, not a
//! standalone NASM file — consistent with every other piece of kernel
//! assembly (`idt.rs`, `gdt.rs`, `regs.rs`, `user_access.rs`), and so the
//! scheduler can couple `Thread.arch.rsp` to this code via compile-time
//! `offset_of!` rather than a hand-maintained numeric offset (decision
//! log, 2026-05-29).
//!
//! ## Model
//!
//! Standard xv6/Linux `swtch`: [`context_switch`] parks the six callee-
//! saved registers on the *outgoing* thread's stack, records the resulting
//! RSP into the outgoing thread's saved-rsp slot, loads the *incoming*
//! thread's saved RSP, restores its six callee-saved registers, and
//! `ret`s. The caller-saved registers are not touched — by the SysV C ABI
//! they are already clobbered across any `call`, and `context_switch` is
//! reached only by a normal `call` from the scheduler.
//!
//! RFLAGS is deliberately not saved/restored here: DF / the arithmetic flags
//! are caller-saved per the ABI exactly as after any `call`, and the interrupt
//! flag is handled around the switch by the scheduler, not inside it. The
//! **preemptive** path reuses this same function from inside the timer IRQ
//! handler: the interrupted thread's full register frame (CPU-pushed
//! `rip/cs/rflags/rsp/ss` + the stub-pushed GPRs) already sits on its kernel
//! stack *below* the callee-saved frame this switch parks, so when that thread
//! is later resumed it returns up through `context_switch` into the timer-stub
//! epilogue, which `iretq`s the original interrupt frame (including IF) back.
//! The scheduler holds interrupts masked across the switch in both paths (see
//! `sched.rs` and `kernel/docs/lock-ordering.md`).
//!
//! CR3 is not switched: every kernel thread shares the boot PML4 and the
//! shared kernel vmap. Address-space switching arrives with the userspace
//! slice.

use core::arch::naked_asm;

/// Saved architectural register state for a parked kernel thread.
///
/// Phase 1 stores only the saved kernel **stack pointer** at the thread's
/// resume point (the x86_64 RSP; AArch64's SP); the six callee-saved
/// registers live on the stack that pointer addresses (parked by
/// [`context_switch`] or zeroed by [`fabricate_frame`]). The field is
/// private so the arch-specific representation never leaks out of this
/// module — callers go through [`saved_sp`](ArchThreadContext::saved_sp) /
/// [`sp_slot`](ArchThreadContext::sp_slot).
#[repr(C)]
pub struct ArchThreadContext {
    /// Saved kernel stack pointer at the thread's resume point. Written by
    /// `context_switch`'s `mov [rdi], rsp` when the thread is switched out;
    /// loaded into RSP when it is switched in.
    sp: u64,
}

impl ArchThreadContext {
    /// A zeroed context. The boot thread starts here — its saved stack
    /// pointer is written by the first switch-out before it is ever read.
    pub const fn new() -> Self {
        Self { sp: 0 }
    }

    /// Read the saved stack pointer.
    ///
    /// # Safety
    /// `ctx` must point at a valid, aligned `ArchThreadContext`.
    pub unsafe fn saved_sp(ctx: *const ArchThreadContext) -> u64 {
        // SAFETY: forwarded — `ctx` is a valid, aligned context pointer.
        unsafe { core::ptr::read(&raw const (*ctx).sp) }
    }

    /// A raw pointer to the saved-stack-pointer word, for
    /// [`context_switch`]'s store into the outgoing thread.
    ///
    /// # Safety
    /// `ctx` must point at a valid, aligned `ArchThreadContext`.
    pub unsafe fn sp_slot(ctx: *mut ArchThreadContext) -> *mut u64 {
        // SAFETY: forwarded — `ctx` is a valid, aligned context pointer.
        unsafe { &raw mut (*ctx).sp }
    }
}

impl Default for ArchThreadContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of `u64` slots the fabricated initial frame occupies: six
/// callee-saved register slots plus the `ret` target. Must equal the
/// number of `push`/`pop` pairs in [`context_switch`] plus one.
pub const FABRICATED_FRAME_SLOTS: usize = 7;
/// Byte size of the fabricated initial frame.
pub const FABRICATED_FRAME_BYTES: u64 = (FABRICATED_FRAME_SLOTS as u64) * 8;

/// The saved stack pointer for a freshly fabricated frame whose stack top
/// (exclusive) is `top`.
const fn fabricated_sp(top: u64) -> u64 {
    top - FABRICATED_FRAME_BYTES
}

/// Cooperatively switch from the current thread to another.
///
/// `prev_sp_slot` is `&mut current.arch.rsp` — `context_switch` stores the
/// outgoing thread's resume RSP there. `next_sp` is the incoming thread's
/// saved RSP (parked by a prior switch, or fabricated by
/// [`fabricate_frame`] for a never-run thread). Returns to the caller only
/// when some later switch selects the outgoing thread again.
///
/// `prev_on_cpu` is a `*mut u8` at the outgoing thread's `on_cpu` guard (see
/// [`Thread::on_cpu`](crate::object::Thread)). After the outgoing resume RSP is
/// committed — and *only* then — this routine clears the guard to `0`,
/// publishing "my parked context is now valid to resume". A stealer on another
/// CPU spins/skips while the guard is set, so it can never load a half-written
/// `saved_sp`. On x86's TSO the plain store ordering (sp-commit store, then
/// guard-clear store) is a release with respect to the stealer's load, so no
/// explicit fence is needed. `prev_on_cpu` may be null to skip the clear (a
/// path whose outgoing thread is not re-enqueued anywhere a stealer can see —
/// e.g. a thread parked in `blocked`/`reap`); today all callers pass it.
///
/// SysV AMD64: `rdi = prev_sp_slot`, `rsi = next_sp`, `rdx = prev_on_cpu`.
///
/// # Safety
/// `prev_sp_slot` must be a valid, writable `*mut u64` (the outgoing
/// thread's `arch.rsp`), `next_sp` must be the saved RSP of a thread
/// whose stack holds a matching parked or fabricated frame, and
/// `prev_on_cpu` must be null or a valid `*mut u8` at the outgoing thread's
/// `on_cpu` byte. Passing anything else corrupts the stack and is undefined
/// behaviour.
#[unsafe(naked)]
pub unsafe extern "C" fn context_switch(
    prev_sp_slot: *mut u64,
    next_sp: u64,
    prev_on_cpu: *mut u8,
) {
    // SAFETY: naked — no prologue/epilogue; every register effect is
    // written explicitly. Reached only via a normal `call` from
    // `sched::yield_now`/`exit`, so on entry SysV holds: rdi is a live,
    // writable `*mut u64` into the outgoing thread's `arch.rsp`, rsi is
    // the saved RSP of a live, refcount-pinned incoming thread, and rdx is
    // null or a live `*mut u8` at the outgoing thread's `on_cpu` byte. Caller-
    // saved registers are already dead per the ABI; we preserve exactly the
    // six callee-saved registers by parking them on the outgoing stack and
    // restoring them from the incoming stack. The push order here MUST
    // mirror the slot layout in `fabricate_frame`.
    naked_asm!(
        // Park callee-saved registers on the outgoing stack.
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // Record the outgoing thread's resume RSP.
        "mov [rdi], rsp", // *prev_sp_slot = rsp
        // Publish "parked context valid": clear the outgoing thread's on_cpu
        // guard *after* the resume RSP is committed (TSO orders these stores),
        // so a stealer that observes the clear also sees the final `saved_sp`.
        // Skip if null.
        "test rdx, rdx",
        "jz 2f",
        "mov byte ptr [rdx], 0", // *prev_on_cpu = 0
        "2:",
        // Switch to the incoming thread's stack.
        "mov rsp, rsi", // rsp = next_sp
        // Restore the incoming thread's callee-saved registers (mirror of
        // the pushes above: last pushed is first popped).
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        // Resume: a parked thread returns into its `context_switch` caller;
        // a never-run thread returns into `thread_trampoline`.
        "ret",
    );
}

/// Trampoline a never-run thread lands on via the fabricated frame's `ret`.
///
/// The fabricated frame leaves RSP at the (16-aligned) stack top, which is
/// the *post-`ret`* state, not the post-`call` state SysV expects at a
/// function entry. We re-align defensively and `call` the Rust entry so it
/// observes the normal `rsp % 16 == 8` on entry. [`thread_enter`] never
/// returns, so the `ud2` is an unreachable tripwire.
///
/// [`thread_enter`]: crate::sched::thread_enter
#[unsafe(naked)]
pub extern "C" fn thread_trampoline() -> ! {
    // SAFETY: naked — reached only via the fabricated frame's `ret` with an
    // empty, 16-aligned stack. `and rsp, -16` is a no-op for a page-aligned
    // kernel-stack top; `sti` enables interrupts so this freshly-scheduled
    // thread runs preemptible (it did not arrive via an `iretq` that would
    // restore IF — the switcher left interrupts masked across the stack swap);
    // the `call` then establishes the SysV entry alignment for `thread_enter`,
    // which is `-> !`.
    naked_asm!(
        "and rsp, -16",
        "sti",
        "call {enter}",
        "ud2",
        enter = sym crate::sched::thread_enter,
    );
}

/// Write a fabricated initial switch frame into the `FABRICATED_FRAME_SLOTS`
/// `u64` slots ending at `top` (exclusive) and return the matching initial
/// [`ArchThreadContext`] (saved stack pointer `top - FABRICATED_FRAME_BYTES`).
///
/// The frame is laid out so that the first [`context_switch`] *into* this
/// thread pops six zeroed callee-saved registers and `ret`s into
/// `trampoline`:
///
/// ```text
///   top-8  : trampoline           ← consumed by `ret`
///   top-16 : 0 (rbp)   top-24 : 0 (rbx)   top-32 : 0 (r12)
///   top-40 : 0 (r13)   top-48 : 0 (r14)   top-56 : 0 (r15)  ← saved sp
/// ```
///
/// Returning the whole `ArchThreadContext` (rather than a bare stack
/// pointer) keeps the saved-context representation entirely inside the arch
/// layer — the caller stores the result opaquely.
///
/// # Safety
/// `top` must be 8-aligned with at least `FABRICATED_FRAME_BYTES` writable
/// bytes below it (a freshly allocated [`KernelStack`](crate::mm) top
/// satisfies this).
pub unsafe fn fabricate_frame(top: u64, trampoline: u64) -> ArchThreadContext {
    let p = top as *mut u64;
    // SAFETY: the caller guarantees `top` has `FABRICATED_FRAME_BYTES`
    // writable bytes below it; we write exactly the seven slots in
    // `[top - 56, top)`. Slot offsets mirror `context_switch`'s pop order.
    unsafe {
        core::ptr::write(p.sub(1), trampoline); // ret target
        core::ptr::write(p.sub(2), 0); // rbp
        core::ptr::write(p.sub(3), 0); // rbx
        core::ptr::write(p.sub(4), 0); // r12
        core::ptr::write(p.sub(5), 0); // r13
        core::ptr::write(p.sub(6), 0); // r14
        core::ptr::write(p.sub(7), 0); // r15
    }
    ArchThreadContext {
        sp: fabricated_sp(top),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fabricated_sp_is_seven_slots_below_top() {
        assert_eq!(fabricated_sp(0x1_0000), 0x1_0000 - 56);
        assert_eq!(FABRICATED_FRAME_BYTES, 56);
    }

    #[test]
    fn fabricate_frame_lays_out_trampoline_and_zeroed_regs() {
        // Eight u64 slots = 64 bytes; treat one-past-the-end as the
        // exclusive stack top so the seven-slot frame lands in buf[1..=7].
        let mut buf = [0xAAu64; 8];
        // SAFETY: one-past-the-end pointer used only as an address; the
        // seven writes land in valid buf slots [1, 8).
        let top = unsafe { buf.as_mut_ptr().add(8) } as u64;
        let tramp = 0xDEAD_BEEF_u64;

        // SAFETY: `top` has 64 writable bytes below it (the array).
        let ctx = unsafe { fabricate_frame(top, tramp) };

        assert_eq!(buf[7], tramp, "top-8 must hold the trampoline (ret target)");
        for i in 1..7 {
            assert_eq!(buf[i], 0, "callee-saved slot {i} must be zeroed");
        }
        assert_eq!(buf[0], 0xAA, "slot below the frame must be untouched");
        assert_eq!(ctx.sp, top - 56, "saved sp must be seven slots below top");
    }

    #[test]
    fn arch_context_new_is_zeroed() {
        assert_eq!(ArchThreadContext::new().sp, 0);
    }
}
