//! Architecture-neutral user-register-snapshot contract.
//!
//! [`ArchRegisters`] abstracts reading a *suspended* thread's saved user
//! registers out of the exception frame the entry stub captured on its kernel
//! stack — the arch-divergent half of `sys_thread_get_registers`. The register
//! set itself is genuinely per-architecture (x86_64's `rax…r15`/`rip`/`rflags`
//! vs. aarch64's `x0…x30`/`sp`/`pc`/`pstate`), and it crosses the
//! kernel/userspace boundary, so the concrete `Values` type is both an
//! arch-specific type and an ABI-version-hash input.
//!
//! The active architecture's implementation is re-exported from `crate::arch`
//! as [`Registers`](crate::arch::Registers) (mirroring `ArchPaging` → `Paging`);
//! its `Values` type is re-exported as `crate::arch::RegisterValues`. The
//! neutral consumer (`sys_thread_get_registers`) names those two and copies the
//! result to user memory without ever reading an individual register.
//!
//! Phase 1 reads registers only; register *writeback* (for the deferred
//! `ModifyAndResume` exception disposition) joins this trait when it lands.

/// Reads a suspended thread's captured user registers from its exception frame.
///
/// All-static (like the other arch traits); the active architecture's marker
/// type implements it. The implementation lives wherever the arch's exception
/// frame is defined (on x86_64, the private `ExceptionFrame` in `idt.rs`), so
/// the frame type need not be exposed beyond its module.
pub trait ArchRegisters {
    /// The arch's `#[repr(C)]` user-register snapshot — the type
    /// `sys_thread_get_registers` writes to userspace. A cross-boundary ABI
    /// layout (an ABI-version-hash input).
    type Values: Copy + Default;

    /// Decode the registers captured in the exception frame at `frame_ptr` (the
    /// kernel-stack address returned by
    /// [`sched::thread_exception_frame`](crate::sched::thread_exception_frame)
    /// for a suspended thread).
    ///
    /// The thread stays parked while suspended, so the frame is stable; the
    /// caller pins the thread (via its handle) for the read's duration.
    fn read_from_exception_frame(frame_ptr: usize) -> Self::Values;
}
