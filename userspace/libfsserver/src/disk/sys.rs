//! The syscalls the device layer makes, behind a seam: `cfg(not(test))` forwards to `libkern`,
//! `cfg(test)` to a scripted fake. Every return is the kernel's `i64`, negative for a `KError`.

use libkern::{IoOp, WAIT_RESULT_SIZE};

/// `sys_io_submit` — returns a `PendingOperation` handle or an error; never blocks.
#[cfg(not(test))]
pub fn io_submit(device: u64, op: &IoOp) -> i64 {
    // SAFETY: `op` is a live `IoOp` the kernel copies in; `device` is the caller's handle.
    unsafe { libkern::syscall2(libkern::SYS_IO_SUBMIT, device, (op as *const IoOp) as u64) }
}

/// `sys_wait` on one handle, forever: the number of handles signalled, and its record in `out`.
#[cfg(not(test))]
pub fn wait_one(po: u64, out: &mut [u8; WAIT_RESULT_SIZE]) -> i64 {
    let handles = [po];
    // SAFETY: `handles` holds one handle and `out` has room for its one record.
    unsafe { libkern::syscall4(libkern::SYS_WAIT, handles.as_ptr() as u64, 1, out.as_mut_ptr() as u64, u64::MAX) }
}

/// `sys_memory_create(size)` — a zeroed `MemoryObject`; returns its handle.
#[cfg(not(test))]
pub fn memory_create(size: u64) -> i64 {
    // SAFETY: register-only syscall.
    unsafe { libkern::syscall4(libkern::SYS_MEMORY_CREATE, size, 0, 0, 0) }
}

/// `sys_memory_map` — maps `size` bytes of `handle` where the kernel chooses; returns the address.
#[cfg(not(test))]
pub fn memory_map(handle: u64, size: u64, rights: u64) -> i64 {
    // SAFETY: register-only syscall; the kernel checks `handle` and `rights`.
    unsafe { libkern::syscall4(libkern::SYS_MEMORY_MAP, handle, 0, size, rights) }
}

/// `sys_memory_unmap` — unmaps the region at `addr`.
#[cfg(not(test))]
pub fn memory_unmap(addr: u64, size: u64) {
    // SAFETY: register-only syscall; the kernel checks the range.
    unsafe { libkern::syscall4(libkern::SYS_MEMORY_UNMAP, addr, size, 0, 0) };
}

/// `sys_handle_close` — releases the caller's reference.
#[cfg(not(test))]
pub fn handle_close(handle: u64) {
    // SAFETY: register-only syscall; closing is sound for any handle value.
    unsafe { libkern::syscall1(libkern::SYS_HANDLE_CLOSE, handle) };
}

#[cfg(test)]
pub use fake::*;

/// A kernel that scripts what the device layer asks of it, per thread (a test is a thread).
#[cfg(test)]
mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    /// One `io_submit` the fake saw.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Submit {
        pub opcode: u32,
        pub buffer: u64,
        pub offset: u64,
        pub length: u64,
    }

    /// A scripted answer: `None` is the default, `Some(v)` is returned as it stands.
    pub type Script = VecDeque<Option<i64>>;

    #[derive(Default)]
    pub struct Fake {
        next: u64,
        /// The memory objects, by handle; a map answers the address of the boxed bytes.
        memory: BTreeMap<u64, Box<[u8]>>,
        /// Each pending operation's submitted length, by handle.
        pending: BTreeMap<u64, u64>,
        /// The device: bytes a READ copies from and a WRITE copies to, grown on write.
        pub device: Vec<u8>,
        pub creates: Script,
        pub maps: Script,
        pub submits: Script,
        /// What `wait_one` returns instead of 1; its record is then left untouched.
        pub waits: Script,
        /// The `(status, bytes moved)` a wait that completes reports in place of a full transfer.
        pub completions: VecDeque<(i32, u64)>,
        pub submitted: Vec<Submit>,
        pub closes: Vec<u64>,
        pub unmaps: Vec<(u64, u64)>,
        pub created: Vec<u64>,
    }

    thread_local! {
        static FAKE: RefCell<Fake> = RefCell::new(Fake { next: 100, ..Fake::default() });
    }

    /// Runs `f` on this thread's fake: the script to set up, and the record to read back.
    pub fn with<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
        FAKE.with(|fake| f(&mut fake.borrow_mut()))
    }

    /// The bytes of memory object `handle` as they are now.
    pub fn peek(handle: u64) -> Vec<u8> {
        with(|f| f.memory[&handle].to_vec())
    }

    pub fn io_submit(_device: u64, op: &IoOp) -> i64 {
        with(|f| {
            f.submitted.push(Submit { opcode: op.opcode, buffer: op.buffer, offset: op.offset, length: op.length });
            if let Some(Some(v)) = f.submits.pop_front() {
                return v;
            }
            let (off, len) = (op.offset as usize, op.length as usize);
            let bo = op.buf_offset as usize;
            if op.opcode == libkern::IO_OPCODE_READ {
                if f.device.len() < off + len {
                    f.device.resize(off + len, 0);
                }
                let src = f.device[off..off + len].to_vec();
                f.memory.get_mut(&op.buffer).unwrap()[bo..bo + len].copy_from_slice(&src);
            } else {
                let src = f.memory[&op.buffer][bo..bo + len].to_vec();
                if f.device.len() < off + len {
                    f.device.resize(off + len, 0);
                }
                f.device[off..off + len].copy_from_slice(&src);
            }
            f.next += 1;
            let po = f.next;
            f.pending.insert(po, op.length);
            po as i64
        })
    }

    pub fn wait_one(po: u64, out: &mut [u8; WAIT_RESULT_SIZE]) -> i64 {
        with(|f| {
            if let Some(Some(v)) = f.waits.pop_front() {
                return v;
            }
            let full = f.pending.get(&po).copied().unwrap_or(0);
            let (status, moved) = f.completions.pop_front().unwrap_or((0, full));
            out[8..12].copy_from_slice(&status.to_le_bytes());
            out[16..24].copy_from_slice(&moved.to_le_bytes());
            1
        })
    }

    pub fn memory_create(size: u64) -> i64 {
        with(|f| {
            if let Some(Some(v)) = f.creates.pop_front() {
                return v;
            }
            f.next += 1;
            let h = f.next;
            f.memory.insert(h, vec![0u8; size as usize].into_boxed_slice());
            f.created.push(h);
            h as i64
        })
    }

    pub fn memory_map(handle: u64, size: u64, _rights: u64) -> i64 {
        with(|f| {
            if let Some(Some(v)) = f.maps.pop_front() {
                return v;
            }
            let bytes = f.memory.get_mut(&handle).expect("a map of a handle the fake made");
            assert!(bytes.len() as u64 >= size);
            bytes.as_mut_ptr() as i64
        })
    }

    pub fn memory_unmap(addr: u64, size: u64) {
        with(|f| f.unmaps.push((addr, size)));
    }

    pub fn handle_close(handle: u64) {
        with(|f| f.closes.push(handle));
    }
}
