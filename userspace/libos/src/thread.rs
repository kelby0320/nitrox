//! Threads in this process, each on a [`Stack`] of its own.

use libkern::{RawHandle, RegisterValues, Rights, ThreadArgs};

use crate::error::{Error, Result, check};
use crate::handle::{Handle, MapReadWrite, Memory, Only, Thread, both};
use crate::memory::Mapping;
use crate::sys;
use libkern::KError;

/// Where a thread begins: `rdi`, `rsi` and `rdx` are `0`, and `rcx`, the fourth, its `arg0`.
pub type Entry = extern "C" fn(u64, u64, u64, u64) -> !;

/// A thread's stack: a mapping of its own, which nothing below it guards, as yet.
pub struct Stack(Mapping<MapReadWrite>);

impl Stack {
    /// `bytes` of stack, rounded up to a page.
    // TODO(thread-stack-guard): a hint-less map places a mapping flush against the one below, so an
    // overrun lands in it rather than faulting.
    pub fn new(bytes: usize) -> Result<Stack> {
        let len = bytes.checked_next_multiple_of(4096).ok_or(Error::from(KError::TooLarge))?;
        Ok(Stack(Handle::<Memory, MapReadWrite>::create(len)?.map(len)?))
    }
}

/// Start a thread at `entry` on `stack`, its fourth argument `arg0`.
///
/// **The stack is the thread's for the rest of the process's life**: nothing tells a process that
/// a thread of its has ended, so nothing could give the stack back safely.
pub fn start(entry: Entry, stack: Stack, arg0: u64) -> Result<Handle<Thread, Only>> {
    start_on(entry, stack, arg0, 0)
}

/// [`start`] on the CPUs whose bits are set in `cpus`; `0` is any.
pub fn start_on(entry: Entry, stack: Stack, arg0: u64, cpus: u8) -> Result<Handle<Thread, Only>> {
    let top = stack.0.as_ptr() as u64 + stack.0.len() as u64;
    let args = ThreadArgs {
        entry: entry as usize as u64,
        // The top: the kernel enters with `rsp` 8 below a 16-byte boundary, as after a `call`.
        user_sp: top,
        arg0,
        class: 0,
        rt_priority: 0,
        nice: 0,
        cpu_affinity: cpus,
        _reserved: [0; 36],
    };
    // SAFETY: `entry` never returns and takes the registers the kernel sets, and the stack is this
    // thread's alone from here on: forgotten below, never unmapped.
    let h = check(unsafe { sys::thread_create(&args) })?;
    let _kept = stack.0.into_raw();
    Ok(Handle::fresh(h, Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE))
}

/// End the calling thread with `status`. The process goes on while another of its threads does.
pub fn exit(status: i32) -> ! {
    sys::thread_exit(status)
}

/// How a thread suspended on a fault goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Run the instruction that faulted again, which faults again unless its cause has gone.
    Resume,
    /// End the thread with this status.
    Terminate(i32),
}

both!([] Thread, Only {
    /// The registers it faulted with: `InvalidArgument` unless it is suspended on a fault. Needs
    /// `SIGNAL`.
    pub fn fault_registers(&self) -> Result<RegisterValues> {
        let mut regs = RegisterValues { regs: [0; 18] };
        check(sys::thread_get_registers(self.raw().0, &mut regs))?;
        Ok(regs)
    }

    /// Go on with it, suspended on a fault, as `how` says: `InvalidArgument` unless it is
    /// suspended. Needs `SIGNAL`.
    pub fn settle(&self, how: Disposition) -> Result<()> {
        let (disposition, code) = match how {
            Disposition::Resume => (0, 0),
            Disposition::Terminate(status) => (2, status as u32 as u64),
        };
        check(sys::exception_resume(self.raw().0, disposition, code)).map(|_| ())
    }
});

/// Start a thread in this process from `args` (a libkern [`ThreadArgs`]), returning an
/// owning handle. The `RealTime` class in `args` requires the `REAL_TIME` syscap
/// (else `Err`). The kernel grants `SIGNAL | TERMINATE | INSPECT | DUPLICATE`.
/// # Safety
/// `args.entry` is an `extern "C"` function that never returns, and `args.user_sp` tops a stack
/// that nothing else uses, mapped for as long as the thread runs.
pub unsafe fn thread_create(args: &ThreadArgs) -> Result<Handle<Thread, Only>> {
    // SAFETY: forwarded from this fn's contract.
    let h = check(unsafe { sys::thread_create(args) })?;
    // SAFETY: `sys_thread_create` returned a fresh thread handle we own.
    Ok(unsafe {
        Handle::from_raw(RawHandle(h), Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn worker(_: u64, _: u64, _: u64, _arg0: u64) -> ! {
        unreachable!("the mock starts no thread")
    }

    #[test]
    fn a_thread_keeps_its_stack_and_is_handed_its_top() {
        sys::reset();
        let stack = Stack::new(5000).unwrap();
        let (base, len) = (stack.0.as_ptr() as u64, stack.0.len() as u64);
        assert_eq!(len, 8192);
        let t = start(worker, stack, 42).unwrap();
        let args = sys::threads().pop().unwrap();
        assert_eq!((args.user_sp, args.arg0, args.cpu_affinity), (base + len, 42, 0));
        drop(t);
        assert_eq!(sys::mappings(), 1, "the stack outlives the handle");
    }

    #[test]
    fn a_pinned_thread_asks_for_its_cpus() {
        sys::reset();
        start_on(worker, Stack::new(4096).unwrap(), 0, 1 << 3).unwrap();
        assert_eq!(sys::threads().pop().unwrap().cpu_affinity, 1 << 3);
    }

    #[test]
    fn a_stack_too_large_to_round_is_refused() {
        sys::reset();
        assert_eq!(Stack::new(usize::MAX).err().unwrap().code(), KError::TooLarge);
    }

    /// Its registers are there only while it is suspended on a fault, and settling it once ends
    /// that.
    #[test]
    fn a_faulted_thread_is_read_and_settled_once() {
        sys::reset();
        let t = start(worker, Stack::new(4096).unwrap(), 0).unwrap();
        let invalid = Err(KError::InvalidArgument);
        assert_eq!(t.fault_registers().map(|r| r.regs).map_err(|e| e.code()), invalid, "running");
        let mut regs = RegisterValues { regs: [0; 18] };
        regs.regs[16] = 0x40_1000;
        sys::fault_thread(t.raw().0, regs);
        assert_eq!(t.fault_registers().unwrap().regs, regs.regs);
        assert!(t.settle(Disposition::Terminate(7)).is_ok());
        assert_eq!(sys::settled(), [(2, 7)], "terminated with its status");
        assert_eq!(t.fault_registers().map(|r| r.regs).map_err(|e| e.code()), invalid, "settled");
        assert_eq!(t.settle(Disposition::Resume).map_err(|e| e.code()), Err(KError::InvalidArgument), "settled once");
    }

    /// Each disposition reaches the kernel as its own number, a negative status whole.
    #[test]
    fn a_resume_and_a_terminate_say_which_they_are() {
        sys::reset();
        let t = start(worker, Stack::new(4096).unwrap(), 0).unwrap();
        sys::fault_thread(t.raw().0, RegisterValues { regs: [0; 18] });
        t.settle(Disposition::Resume).unwrap();
        sys::fault_thread(t.raw().0, RegisterValues { regs: [0; 18] });
        t.settle(Disposition::Terminate(-3)).unwrap();
        let settled = sys::settled();
        assert_eq!(settled[0], (0, 0));
        assert_eq!((settled[1].0, settled[1].1 as i32), (2, -3), "as the kernel reads it, an `i32`");
    }

    #[test]
    fn a_thread_handle_without_signal_can_neither_read_nor_settle() {
        sys::reset();
        let t = start(worker, Stack::new(4096).unwrap(), 0).unwrap();
        sys::fault_thread(t.raw().0, RegisterValues { regs: [0; 18] });
        let blind = t.duplicate(Rights::INSPECT).unwrap();
        assert_eq!(blind.fault_registers().map(|r| r.regs).map_err(|e| e.code()), Err(KError::NoAccess));
        assert_eq!(blind.settle(Disposition::Resume).map_err(|e| e.code()), Err(KError::NoAccess));
        assert!(t.fault_registers().is_ok(), "still suspended: the refusals settled nothing");
    }

    #[test]
    fn a_refused_thread_gives_its_stack_back() {
        sys::reset();
        sys::fail_next_thread(libkern::KError::NoAccess as i32);
        assert!(start(worker, Stack::new(4096).unwrap(), 0).is_err());
        assert_eq!(sys::mappings(), 0, "no thread runs on it");
    }
}
