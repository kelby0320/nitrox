//! Kernel-thread context switch, shared by the cooperative (`yield_now`/`exit`) and
//! preemptive (timer-IRQ) scheduler paths.
//!
//! [`context_switch`] is the xv6/Linux `swtch` as a `#[unsafe(naked)]` function, so the
//! scheduler couples `Thread.arch.rsp` to it by `offset_of!` (decision log, 2026-05-29): park
//! the six callee-saved registers on the outgoing stack, store RSP in the outgoing slot, load
//! the incoming RSP, pop its six registers, `ret`. Caller-saved registers and RFLAGS are not
//! saved (a normal `call`; the scheduler masks interrupts across the switch, see
//! `kernel/docs/lock-ordering.md`). CR3 is not switched here.
//!
//! On the preemptive path the interrupted thread's full frame (CPU-pushed
//! `rip/cs/rflags/rsp/ss` plus the stub's GPRs) sits *below* the frame parked here, so on
//! resume it returns up through `context_switch` into the timer-stub epilogue, whose `iretq`
//! restores IF.

use core::arch::naked_asm;

/// A parked kernel thread's saved state: its kernel stack pointer at the resume point. The
/// callee-saved registers live on that stack (parked by [`context_switch`] or zeroed by
/// [`fabricate_frame`]). Private so callers use [`saved_sp`](ArchThreadContext::saved_sp) /
/// [`sp_slot`](ArchThreadContext::sp_slot).
#[repr(C)]
pub struct ArchThreadContext {
    /// Saved kernel stack pointer at the thread's resume point, written by `context_switch`'s
    /// `mov [rdi], rsp`.
    sp: u64,
}

impl ArchThreadContext {
    /// A zeroed context; the boot thread's slot is written by its first switch-out.
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

    /// A raw pointer to the saved-stack-pointer word, for [`context_switch`]'s store.
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

/// Number of `u64` slots in the fabricated initial frame: six callee-saved slots plus the
/// `ret` target; one more than the `push`/`pop` pairs in [`context_switch`].
pub const FABRICATED_FRAME_SLOTS: usize = 7;
/// Byte size of the fabricated initial frame.
pub const FABRICATED_FRAME_BYTES: u64 = (FABRICATED_FRAME_SLOTS as u64) * 8;

const fn fabricated_sp(top: u64) -> u64 {
    top - FABRICATED_FRAME_BYTES
}

/// Cooperatively switch from the current thread to another.
///
/// `prev_sp_slot` is `&mut current.arch.rsp`, where the outgoing resume RSP is stored;
/// `next_sp` is the incoming thread's saved RSP (parked, or from [`fabricate_frame`]).
/// Returns only when a later switch selects the outgoing thread again.
///
/// `prev_on_cpu` points at the outgoing thread's `on_cpu` guard (see
/// [`Thread::on_cpu`](crate::object::Thread)), cleared to `0` only after the resume RSP is
/// committed: a stealer on another CPU skips the thread while it is set, so it never loads a
/// half-written `saved_sp`. x86's TSO orders the two plain stores, so no fence is needed.
/// Null skips the clear, for a thread no stealer can see.
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
#[allow(clippy::unnecessary_safety_comment, reason = "the asm's safety argument; `naked_asm!` has no `unsafe` block")]
#[unsafe(naked)]
pub unsafe extern "C" fn context_switch(prev_sp_slot: *mut u64, next_sp: u64, prev_on_cpu: *mut u8) {
    // SAFETY: naked — no prologue/epilogue; every register effect is written explicitly.
    // Reached only via a normal `call` from `sched::yield_now`/`exit`: rdi is a live
    // `*mut u64` into the outgoing `arch.rsp`, rsi the saved RSP of a live, refcount-pinned
    // incoming thread, rdx null or a live `*mut u8` at its `on_cpu`. Exactly the six
    // callee-saved registers are preserved; the push order MUST mirror `fabricate_frame`.
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
/// RSP is at the (16-aligned) stack top, the post-`ret` state rather than the post-`call`
/// state SysV expects at entry; it re-aligns and `call`s `thread_enter` so that sees
/// `rsp % 16 == 8`. [`thread_enter`] never returns, so the `ud2` is a tripwire.
///
/// [`thread_enter`]: crate::sched::thread_enter
#[allow(clippy::unnecessary_safety_comment, reason = "the asm's safety argument; `naked_asm!` has no `unsafe` block")]
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

/// Write a fabricated initial switch frame into the `FABRICATED_FRAME_SLOTS` `u64` slots
/// ending at `top` (exclusive) and return the matching [`ArchThreadContext`].
///
/// The first [`context_switch`] into the thread pops six zeroed callee-saved registers and
/// `ret`s into `trampoline`:
///
/// ```text
///   top-8  : trampoline           ← consumed by `ret`
///   top-16 : 0 (rbp)   top-24 : 0 (rbx)   top-32 : 0 (r12)
///   top-40 : 0 (r13)   top-48 : 0 (r14)   top-56 : 0 (r15)  ← saved sp
/// ```
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
    ArchThreadContext { sp: fabricated_sp(top) }
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
        // The seven-slot frame lands in buf[1..=7], below the exclusive top.
        let mut buf = [0xAAu64; 8];
        // SAFETY: one-past-the-end pointer used only as an address; the
        // seven writes land in valid buf slots [1, 8).
        let top = unsafe { buf.as_mut_ptr().add(8) } as u64;
        let tramp = 0xDEAD_BEEF_u64;

        // SAFETY: `top` has 64 writable bytes below it (the array).
        let ctx = unsafe { fabricate_frame(top, tramp) };

        assert_eq!(buf[7], tramp, "top-8 must hold the trampoline (ret target)");
        #[allow(clippy::needless_range_loop, reason = "the index names the entry in each assertion's message")]
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
