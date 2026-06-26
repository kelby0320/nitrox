//! Handle encoding and rights — mirrored from `kernel/src/libkern/handle.rs`.
//!
//! [`RawHandle`] is the opaque 64-bit handle value; [`Rights`] is the rights
//! bitfield; [`KObjectType`] is the object-type discriminant. Convenience
//! `RIGHT_*` (`u64`) and `KOBJ_*` (`u32`) aliases are provided for raw-syscall
//! call sites that pass plain integers; they derive from the canonical typed
//! constants below, so there is one source of truth for each bit/discriminant.

use core::ops::{BitAnd, BitOr};

/// An opaque kernel handle value (`docs/spec/handle-encoding.md`). `0` is the
/// reserved null handle, never issued by the kernel.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct RawHandle(pub u64);

impl RawHandle {
    /// The reserved null handle.
    pub const NULL: RawHandle = RawHandle(0);

    /// The raw bit pattern.
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` iff this is the null handle.
    pub const fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// A rights bitfield. Subset semantics: `r1.is_subset_of(r2)` iff `(r1 & r2) == r1`.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub struct Rights(u64);

impl Rights {
    // --- Generic rights (apply to all handle types) --------------------
    /// `sys_handle_duplicate` is permitted.
    pub const DUPLICATE: Rights = Rights(1 << 0);
    /// Handle may be transferred across an IPC boundary.
    pub const TRANSFER: Rights = Rights(1 << 1);
    /// Handle metadata is readable via `sys_handle_stat`.
    pub const INSPECT: Rights = Rights(1 << 2);
    /// `sys_wait` accepts this handle.
    pub const WAIT: Rights = Rights(1 << 3);

    // --- Principal rights (type-specific) ------------------------------
    /// I/O read.
    pub const READ: Rights = Rights(1 << 8);
    /// I/O write.
    pub const WRITE: Rights = Rights(1 << 9);
    /// Execute (mapped memory).
    pub const EXECUTE: Rights = Rights(1 << 10);
    /// Send an out-of-band signal (`Process`, `Thread`).
    pub const SIGNAL: Rights = Rights(1 << 11);
    /// Terminate (`Process`, `Thread`).
    pub const TERMINATE: Rights = Rights(1 << 12);
    /// Resolve names within a namespace.
    pub const LOOKUP: Rights = Rights(1 << 13);
    /// Bind a name into a namespace.
    pub const BIND: Rights = Rights(1 << 14);
    /// Map readable.
    pub const MAP_READ: Rights = Rights(1 << 15);
    /// Map writable.
    pub const MAP_WRITE: Rights = Rights(1 << 16);
    /// Map executable.
    pub const MAP_EXEC: Rights = Rights(1 << 17);
    /// IPC send.
    pub const SEND: Rights = Rights(1 << 18);
    /// IPC receive.
    pub const RECV: Rights = Rights(1 << 19);

    // --- Modifier rights -----------------------------------------------
    /// Seek within a stream resource.
    pub const SEEK: Rights = Rights(1 << 32);
    /// Append-only write.
    pub const APPEND: Rights = Rights(1 << 33);
    /// Truncate a resource to zero.
    pub const TRUNCATE: Rights = Rights(1 << 34);
    /// Remove an existing namespace binding.
    pub const UNBIND: Rights = Rights(1 << 35);
    /// Enumerate a namespace's bindings.
    pub const ENUMERATE: Rights = Rights(1 << 36);
    /// Inspect a process's memory map.
    pub const INSPECT_MEMORY: Rights = Rights(1 << 37);

    /// The empty set.
    pub const fn empty() -> Self {
        Rights(0)
    }

    /// Wrap a raw bit pattern.
    pub const fn from_bits(bits: u64) -> Self {
        Rights(bits)
    }

    /// The raw bit pattern (the syscall ABI crossing value).
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// `true` if every bit set in `other` is also set in `self`.
    pub const fn contains(self, other: Rights) -> bool {
        (self.0 & other.0) == other.0
    }

    /// `true` iff `self ⊆ other`.
    pub const fn is_subset_of(self, other: Rights) -> bool {
        (self.0 & other.0) == self.0
    }
}

impl BitOr for Rights {
    type Output = Rights;
    fn bitor(self, rhs: Rights) -> Rights {
        Rights(self.0 | rhs.0)
    }
}

impl BitAnd for Rights {
    type Output = Rights;
    fn bitand(self, rhs: Rights) -> Rights {
        Rights(self.0 & rhs.0)
    }
}

// --- `u64` rights aliases (for raw-syscall call sites) ---------------------
// Derived from the typed constants above — single source of truth per bit.
pub const RIGHT_DUPLICATE: u64 = Rights::DUPLICATE.bits();
pub const RIGHT_TRANSFER: u64 = Rights::TRANSFER.bits();
pub const RIGHT_INSPECT: u64 = Rights::INSPECT.bits();
pub const RIGHT_WAIT: u64 = Rights::WAIT.bits();
pub const RIGHT_READ: u64 = Rights::READ.bits();
pub const RIGHT_WRITE: u64 = Rights::WRITE.bits();
pub const RIGHT_EXECUTE: u64 = Rights::EXECUTE.bits();
pub const RIGHT_SIGNAL: u64 = Rights::SIGNAL.bits();
pub const RIGHT_TERMINATE: u64 = Rights::TERMINATE.bits();
pub const RIGHT_LOOKUP: u64 = Rights::LOOKUP.bits();
pub const RIGHT_BIND: u64 = Rights::BIND.bits();
pub const RIGHT_MAP_READ: u64 = Rights::MAP_READ.bits();
pub const RIGHT_MAP_WRITE: u64 = Rights::MAP_WRITE.bits();
pub const RIGHT_MAP_EXEC: u64 = Rights::MAP_EXEC.bits();
pub const RIGHT_SEND: u64 = Rights::SEND.bits();
pub const RIGHT_RECV: u64 = Rights::RECV.bits();
pub const RIGHT_UNBIND: u64 = Rights::UNBIND.bits();
pub const RIGHT_ENUMERATE: u64 = Rights::ENUMERATE.bits();

/// Discriminant identifying which kind of kernel object a handle refers to.
/// Values are fixed by `docs/spec/handle-encoding.md`; never renumber.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KObjectType {
    /// Reserved; never appears in a live entry.
    Invalid = 0,
    Process = 1,
    Thread = 2,
    Namespace = 3,
    MemoryObject = 4,
    IpcChannel = 5,
    NotificationChannel = 6,
    Timer = 7,
    InterruptObject = 8,
    PendingOperation = 9,
    IoRing = 10,
    EntropyObject = 11,
    DeviceNode = 12,
    UserspaceServerReg = 13,
    FileObject = 14,
}

// --- `u32` object-type aliases (for raw `sys_handle_stat` decoding) ---------
pub const KOBJ_PROCESS: u32 = KObjectType::Process as u32;
pub const KOBJ_THREAD: u32 = KObjectType::Thread as u32;
pub const KOBJ_NAMESPACE: u32 = KObjectType::Namespace as u32;
pub const KOBJ_MEMORY_OBJECT: u32 = KObjectType::MemoryObject as u32;
pub const KOBJ_IPC_CHANNEL: u32 = KObjectType::IpcChannel as u32;
pub const KOBJ_NOTIFICATION_CHANNEL: u32 = KObjectType::NotificationChannel as u32;
pub const KOBJ_TIMER: u32 = KObjectType::Timer as u32;
pub const KOBJ_PENDING_OPERATION: u32 = KObjectType::PendingOperation as u32;
pub const KOBJ_ENTROPY_OBJECT: u32 = KObjectType::EntropyObject as u32;
pub const KOBJ_FILE_OBJECT: u32 = KObjectType::FileObject as u32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_aliases_match_typed() {
        assert_eq!(RIGHT_MAP_READ, 1 << 15);
        assert_eq!(RIGHT_LOOKUP, 1 << 13);
        assert_eq!(RIGHT_SEND | RIGHT_RECV, (Rights::SEND | Rights::RECV).bits());
    }

    #[test]
    fn kobj_aliases_match_typed() {
        assert_eq!(KOBJ_PROCESS, 1);
        assert_eq!(KOBJ_THREAD, 2);
        assert_eq!(KOBJ_NAMESPACE, 3);
    }

    #[test]
    fn subset_semantics() {
        let rw = Rights::READ | Rights::WRITE;
        assert!(Rights::READ.is_subset_of(rw));
        assert!(!rw.is_subset_of(Rights::READ));
    }
}
