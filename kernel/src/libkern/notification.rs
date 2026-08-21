//! [`Notification`] — the structured async event the kernel delivers into a
//! process's [`NotificationChannel`](crate::object::NotificationChannel).
//!
//! Nitrox has no Unix signals (`docs/rationale/why-no-signals.md`); instead the
//! kernel enqueues `Notification`s and the process reads them with
//! `sys_notif_recv`. The wire format (`docs/spec/notification-format.md`) is a
//! **fixed 64-byte record**: a `u32` discriminant at offset 0 followed by a
//! 60-byte little-endian variant body (shorter variants zero-padded).
//!
//! The kernel models this as a flat `{ kind, body }` byte record with typed
//! constructors that write the spec field offsets — not a fieldful
//! `#[repr(C, u32)]` enum. The flat form *is* the wire bytes (a single
//! `copy_to_user` of 64 bytes, no per-variant marshalling), and userspace
//! decodes by discriminant after the copy. This mirrors the `IoResult`
//! precedent ([`crate::libkern::io_result`]).
//!
//! ABI: the layout is part of the kernel ABI version hash
//! (`docs/spec/abi-version-hash.md` § "Notification enum layout"). The hash is
//! not yet computed in code, so nothing is enforced today — but the field
//! offsets here are a contract; the compile-time asserts pin them.

/// Forward-compat fallback: a discriminant the recipient's ABI doesn't know.
pub const KIND_UNKNOWN: u32 = 0x0000;
/// Hardware exception: page fault / access violation.
pub const KIND_SEG_FAULT: u32 = 0x0100;
/// Hardware exception: invalid opcode.
pub const KIND_ILLEGAL_INSN: u32 = 0x0101;
/// Hardware exception: divide error.
pub const KIND_DIVIDE_BY_ZERO: u32 = 0x0102;
/// Hardware exception: stack overflow.
pub const KIND_STACK_OVERFLOW: u32 = 0x0103;
/// Process lifecycle: a child process exited. Carries the child's **pid** and its
/// exit status; the pid is not matchable to a process handle, which is why a
/// supervisor with several children discriminates on their control channels
/// instead — see `TODO(child-exit-attribution)`.
pub const KIND_CHILD_EXITED: u32 = 0x0200;
/// Process lifecycle: an IPC peer closed.
///
/// **Declared, specified, and never emitted.** `notification-format.md` gives it a
/// body (`PeerClosed { handle }`), and nothing in the kernel constructs one: a dead
/// peer is observed instead by `sys_channel_recv` answering
/// [`PeerClosed`](crate::syscall::error::KError::PeerClosed), and a waiter is woken
/// for it by `sched::ipc_endpoint_closing`. Kept because that is the discriminant a
/// producer would use, and removing it would renumber the ABI for nothing.
pub const KIND_PEER_CLOSED: u32 = 0x0201;
/// `TerminateRequested {}` — a holder of this process's handle has **asked it to
/// exit** (§11h).
///
/// A request, not an act: the kernel delivers it and does nothing else. There is no
/// forcible kill in this system, so a process that does not listen keeps running — which
/// is a stated property rather than a gap. Cooperative shutdown is what the no-signals
/// design has instead of `SIGTERM`, and the notification queue is where async events
/// already arrive.
pub const KIND_TERMINATE_REQUESTED: u32 = 0x0202;

/// A handle this process holds was invalidated.
pub const KIND_HANDLE_INVALIDATED: u32 = 0x0400;
/// Resource: notifications were dropped due to queue overflow (synthetic).
pub const KIND_NOTIFICATIONS_DROPPED: u32 = 0x0401;

/// Inclusive-exclusive bounds of the hardware-exception category (`0x0100`
/// range). Exception notifications get priority on overflow.
const EXCEPTION_LO: u32 = 0x0100;
const EXCEPTION_HI: u32 = 0x0200;

/// How a [`KIND_SEG_FAULT`] arose (`docs/spec/notification-format.md`).
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum FaultKind {
    /// Page not present.
    NotMapped = 0,
    NotReadable = 1,
    NotWritable = 2,
    /// Instruction fetch from a no-execute page.
    NotExecutable = 3,
    Misaligned = 4,
    UnknownFault = 0xFFFF_FFFF,
}

/// How a process/thread ended (the `kind` of an [`ExitStatus`]). Producer lands
/// with process spawn; defined here for the `ChildExited` ABI.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ExitKind {
    /// Voluntary exit; the exit code is in `code`.
    Normal = 0,
    /// Terminated by a supervisor; signal-equivalent in `code`.
    Killed = 1,
    /// Crashed on a fault; the fault kind is in `code`.
    Crashed = 2,
}

/// A `ChildExited` status (`kind: ExitKind` as `u32`, `code: i32`).
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ExitStatus {
    pub kind: u32,
    pub code: i32,
}

/// One 64-byte notification record. `#[repr(C, align(8))]` so `u64` body fields
/// land naturally aligned at their spec offsets.
#[repr(C, align(8))]
#[derive(Copy, Clone)]
pub struct Notification {
    kind: u32,
    body: [u8; 60],
}

const _: () = assert!(core::mem::size_of::<Notification>() == 64);
const _: () = assert!(core::mem::align_of::<Notification>() == 8);
const _: () = assert!(core::mem::offset_of!(Notification, kind) == 0);
const _: () = assert!(core::mem::offset_of!(Notification, body) == 4);

impl Notification {
    /// A notification with the given discriminant and a zeroed body.
    pub fn zeroed(kind: u32) -> Self {
        Self { kind, body: [0u8; 60] }
    }

    /// The discriminant.
    pub fn kind(&self) -> u32 {
        self.kind
    }

    /// `true` iff this is a hardware-exception notification (`0x0100` range) —
    /// the category that gets priority on queue overflow.
    pub fn is_exception(&self) -> bool {
        (EXCEPTION_LO..EXCEPTION_HI).contains(&self.kind)
    }

    /// The fault address — `addr` at body+4 — for an exception notification
    /// (`seg_fault`/`illegal_insn`/`divide_by_zero` all store it there). For a
    /// `#PF` this is the faulting linear address (CR2); for `#UD`/`#DE` the faulting
    /// instruction pointer. Only meaningful when [`is_exception`](Self::is_exception).
    pub fn fault_addr(&self) -> u64 {
        u64::from_le_bytes(self.body[4..12].try_into().unwrap())
    }

    /// `SegFault { thread, addr, kind }` — `thread` at body+0, `addr` at body+4,
    /// `kind` at body+12 (per the spec).
    pub fn seg_fault(thread: u32, addr: u64, fault: FaultKind) -> Self {
        let mut n = Self::zeroed(KIND_SEG_FAULT);
        n.body[0..4].copy_from_slice(&thread.to_le_bytes());
        n.body[4..12].copy_from_slice(&addr.to_le_bytes());
        n.body[12..16].copy_from_slice(&(fault as u32).to_le_bytes());
        n
    }

    /// `IllegalInsn { thread, addr }` — `thread` at body+0, `addr` (PC) at body+4.
    pub fn illegal_insn(thread: u32, addr: u64) -> Self {
        let mut n = Self::zeroed(KIND_ILLEGAL_INSN);
        n.body[0..4].copy_from_slice(&thread.to_le_bytes());
        n.body[4..12].copy_from_slice(&addr.to_le_bytes());
        n
    }

    /// `DivideByZero { thread, addr }`.
    pub fn divide_by_zero(thread: u32, addr: u64) -> Self {
        let mut n = Self::zeroed(KIND_DIVIDE_BY_ZERO);
        n.body[0..4].copy_from_slice(&thread.to_le_bytes());
        n.body[4..12].copy_from_slice(&addr.to_le_bytes());
        n
    }

    /// `ChildExited { child, status }` — `child` (pid) at body+0, `status.kind`
    /// at body+4, `status.code` at body+8 (per the spec). Delivered to the
    /// parent's notification channel when a child process exits.
    pub fn child_exited(child: u32, status: ExitStatus) -> Self {
        let mut n = Self::zeroed(KIND_CHILD_EXITED);
        n.body[0..4].copy_from_slice(&child.to_le_bytes());
        n.body[4..8].copy_from_slice(&status.kind.to_le_bytes());
        n.body[8..12].copy_from_slice(&status.code.to_le_bytes());
        n
    }

    /// `TerminateRequested {}` — no body: the request carries no argument, and the
    /// *reason* is the requester's business, not the kernel's.
    pub fn terminate_requested() -> Self {
        Self::zeroed(KIND_TERMINATE_REQUESTED)
    }

    /// `NotificationsDropped { count }` — synthesized by a channel on overflow.
    pub fn notifications_dropped(count: u32) -> Self {
        let mut n = Self::zeroed(KIND_NOTIFICATIONS_DROPPED);
        n.body[0..4].copy_from_slice(&count.to_le_bytes());
        n
    }

    /// The 64 wire bytes, for `copy_to_user`.
    pub fn as_bytes(&self) -> &[u8; 64] {
        // SAFETY: `#[repr(C, align(8))]` with size 64 (asserted above); every
        // byte is initialised (`u32` + `[u8; 60]`) with no interior padding, so
        // reinterpreting as `[u8; 64]` exposes only defined bytes.
        unsafe { &*(self as *const Self as *const [u8; 64]) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_stable() {
        assert_eq!(core::mem::size_of::<Notification>(), 64);
        assert_eq!(core::mem::align_of::<Notification>(), 8);
        assert_eq!(core::mem::offset_of!(Notification, kind), 0);
        assert_eq!(core::mem::offset_of!(Notification, body), 4);
    }

    #[test]
    fn seg_fault_writes_spec_offsets() {
        let n = Notification::seg_fault(7, 0xDEAD_BEEF, FaultKind::NotWritable);
        let b = n.as_bytes();
        // kind @0 (overall) = 0x0100.
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), KIND_SEG_FAULT);
        // thread @ body+0 (overall +4).
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 7);
        // addr @ body+4 (overall +8).
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 0xDEAD_BEEF);
        // kind (FaultKind) @ body+12 (overall +16).
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), FaultKind::NotWritable as u32);
    }

    #[test]
    fn divide_and_illegal_carry_thread_and_addr() {
        let d = Notification::divide_by_zero(3, 0x4000);
        assert_eq!(d.kind(), KIND_DIVIDE_BY_ZERO);
        assert_eq!(u32::from_le_bytes(d.as_bytes()[4..8].try_into().unwrap()), 3);
        let i = Notification::illegal_insn(9, 0x5000);
        assert_eq!(i.kind(), KIND_ILLEGAL_INSN);
        assert_eq!(u64::from_le_bytes(i.as_bytes()[8..16].try_into().unwrap()), 0x5000);
    }

    #[test]
    fn child_exited_writes_spec_offsets() {
        let n = Notification::child_exited(
            42,
            ExitStatus { kind: ExitKind::Crashed as u32, code: -7 },
        );
        let b = n.as_bytes();
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), KIND_CHILD_EXITED);
        // child pid @ body+0 (overall +4).
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 42);
        // status.kind @ body+4 (overall +8).
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), ExitKind::Crashed as u32);
        // status.code @ body+8 (overall +12).
        assert_eq!(i32::from_le_bytes(b[12..16].try_into().unwrap()), -7);
        assert!(!n.is_exception());
    }

    #[test]
    fn notifications_dropped_writes_count() {
        let n = Notification::notifications_dropped(42);
        assert_eq!(n.kind(), KIND_NOTIFICATIONS_DROPPED);
        assert_eq!(u32::from_le_bytes(n.as_bytes()[4..8].try_into().unwrap()), 42);
    }

    #[test]
    fn is_exception_only_for_0x01xx() {
        assert!(Notification::zeroed(KIND_SEG_FAULT).is_exception());
        assert!(Notification::zeroed(KIND_ILLEGAL_INSN).is_exception());
        assert!(Notification::zeroed(KIND_STACK_OVERFLOW).is_exception());
        assert!(!Notification::zeroed(KIND_UNKNOWN).is_exception());
        assert!(!Notification::zeroed(KIND_CHILD_EXITED).is_exception());
        assert!(!Notification::zeroed(KIND_NOTIFICATIONS_DROPPED).is_exception());
    }

    /// §11h: a request carries no argument — the *reason* is the requester's business,
    /// not the kernel's, and there is nothing for a target to interpret beyond "you were
    /// asked".
    #[test]
    fn terminate_requested_is_a_bare_kind() {
        let n = Notification::terminate_requested();
        assert_eq!(n.kind(), KIND_TERMINATE_REQUESTED);
        assert!(n.body.iter().all(|b| *b == 0), "the request carries no body");
        // It is not a fault, so nothing on the exception path should pick it up.
        assert!(!n.is_exception());
        // And it is distinct from every other kind a process may already be handling.
        for other in [
            KIND_CHILD_EXITED,
            KIND_PEER_CLOSED,
            KIND_HANDLE_INVALIDATED,
            KIND_NOTIFICATIONS_DROPPED,
            KIND_SEG_FAULT,
        ] {
            assert_ne!(KIND_TERMINATE_REQUESTED, other);
        }
    }
}
