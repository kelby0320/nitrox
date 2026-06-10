//! x86_64 user-register snapshot ([`RegisterValues`]) and the
//! [`ArchRegisters`](crate::arch::registers::ArchRegisters) marker
//! [`X86Registers`].
//!
//! The `impl ArchRegisters for X86Registers` lives in [`idt`](super::idt),
//! where the private `ExceptionFrame` it decodes is defined — so reading a
//! suspended thread's registers needs no widening of that frame's visibility.
//! This module owns only the data type and the marker; `crate::arch`
//! re-exports them as `RegisterValues` and `Registers`.

/// The x86_64 marker implementing
/// [`ArchRegisters`](crate::arch::registers::ArchRegisters); re-exported as
/// [`crate::arch::Registers`].
pub struct X86Registers;

/// A snapshot of an x86_64 thread's user register state, written by
/// `sys_thread_get_registers` for a suspended (faulted) thread.
///
/// `#[repr(C)]` x86_64 layout: the 16 general-purpose registers (including
/// `rsp`), then `rip` and `rflags`. This is the cross-kernel/userspace ABI
/// contract for the register set; its layout is an ABI-version-hash input (like
/// [`ThreadArgs`](crate::libkern::ThreadArgs) / `IpcMsg`). aarch64 will define
/// its own snapshot type when that arch lands.
#[repr(C)]
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct RegisterValues {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags: u64,
}

const _: () = assert!(core::mem::size_of::<RegisterValues>() == 18 * 8);
const _: () = assert!(core::mem::align_of::<RegisterValues>() == 8);
const _: () = assert!(core::mem::offset_of!(RegisterValues, rax) == 0);
const _: () = assert!(core::mem::offset_of!(RegisterValues, rip) == 16 * 8);
const _: () = assert!(core::mem::offset_of!(RegisterValues, rflags) == 17 * 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_values_layout_is_stable() {
        assert_eq!(core::mem::size_of::<RegisterValues>(), 144);
        assert_eq!(core::mem::offset_of!(RegisterValues, rsp), 7 * 8);
        assert_eq!(core::mem::offset_of!(RegisterValues, rip), 16 * 8);
        assert_eq!(core::mem::offset_of!(RegisterValues, rflags), 17 * 8);
        assert_eq!(
            RegisterValues::default(),
            RegisterValues {
                rax: 0, rbx: 0, rcx: 0, rdx: 0, rsi: 0, rdi: 0, rbp: 0, rsp: 0, r8: 0, r9: 0,
                r10: 0, r11: 0, r12: 0, r13: 0, r14: 0, r15: 0, rip: 0, rflags: 0,
            }
        );
    }
}
