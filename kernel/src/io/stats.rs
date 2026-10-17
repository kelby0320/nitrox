//! **What each block device has done**: the commands its driver issued and saw
//! retire, the bytes they moved and the time on the device, counted by the driver
//! where a command is issued and where it retires, so the time is the device's
//! own, not an IRP's wait in a driver's queue. A partition's I/O is its disk's:
//! its window forwards to the disk's driver, which counts it there.
//!
//! **Kept on the device's node** ([`DeviceNode::io`](crate::object::DeviceNode::io))
//! rather than in a driver's state, because a USB driver's slot goes to the next
//! device to arrive and a node never does. A driver holds a [`Counted`] (the
//! node's counters, attached once the node is registered); `/dev/registry/io`
//! serves every disk's (`docs/spec/device-node.md` § *The registry*).

use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::io::irp::IrpOp;
use crate::libkern::device::IoRecord;
use crate::libkern::handle::KObjectType;
use crate::object::{DeviceNode, ObjectRef};

/// **One device's counters, since it was registered.** Relaxed throughout: they are statistics,
/// and a reader between two updates for one command can see one of them and not the other.
#[derive(Debug, Default)]
pub struct IoStats {
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    errors: AtomicU64,
    read_bytes: AtomicU64,
    written_bytes: AtomicU64,
    busy_ns: AtomicU64,
}

impl IoStats {
    /// Nothing done yet.
    pub const fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            flushes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
            written_bytes: AtomicU64::new(0),
            busy_ns: AtomicU64::new(0),
        }
    }

    /// **One command retired**: its IRP's `op`, whether it succeeded, the bytes it moved and the
    /// nanoseconds it was on the device. A failed command is an error, and its time is still busy
    /// time; an op that is neither a transfer nor a flush — a rescan — is not a command here.
    pub fn record(&self, op: u32, ok: bool, bytes: u64, ns: u64) {
        let (count, moved) = match op {
            o if o == IrpOp::Read as u32 => (&self.reads, Some(&self.read_bytes)),
            o if o == IrpOp::Write as u32 => (&self.writes, Some(&self.written_bytes)),
            o if o == IrpOp::Flush as u32 => (&self.flushes, None),
            _ => return,
        };
        self.busy_ns.fetch_add(ns, Ordering::Relaxed);
        if !ok {
            self.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        count.fetch_add(1, Ordering::Relaxed);
        if let Some(moved) = moved {
            moved.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// The counters as `/dev/registry/io` serves them, for the device with registry id `id`.
    pub fn record_for(&self, id: u32, flags: u32) -> IoRecord {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        IoRecord {
            id,
            flags,
            reads: get(&self.reads),
            writes: get(&self.writes),
            flushes: get(&self.flushes),
            errors: get(&self.errors),
            read_bytes: get(&self.read_bytes),
            written_bytes: get(&self.written_bytes),
            busy_ns: get(&self.busy_ns),
        }
    }
}

/// **A driver's hold on a device's counters**: empty until the device's node is registered and
/// [`attach`](Self::attach)ed, and recording nothing while empty — a host test's disk, or a command
/// that retires between a node's registration and its attach.
pub struct Counted(AtomicPtr<IoStats>);

impl Counted {
    /// Attached to nothing.
    pub const fn new() -> Self {
        Self(AtomicPtr::new(core::ptr::null_mut()))
    }

    /// Hold `node`'s counters.
    ///
    /// # Safety
    /// `node` is a `DeviceNode` registered in the device table, which holds an owning reference to
    /// every node for the kernel's lifetime — a departed one included — so its counters outlive
    /// every command recorded into them.
    pub unsafe fn attach(&self, node: &ObjectRef) {
        if node.object_type() != KObjectType::DeviceNode {
            return;
        }
        // SAFETY: a `DeviceNode`, by its type; live, by the caller's contract.
        let io = unsafe { &*(node.as_ptr() as *const DeviceNode) }.io();
        self.0.store(io as *const IoStats as *mut IoStats, Ordering::Release);
    }

    /// Hold counters that live as long as the kernel: a host test's.
    pub fn attach_static(&self, io: &'static IoStats) {
        self.0.store(io as *const IoStats as *mut IoStats, Ordering::Release);
    }

    /// Record one command into the counters held, if any ([`IoStats::record`]).
    pub fn record(&self, op: u32, ok: bool, bytes: u64, ns: u64) {
        let io = self.0.load(Ordering::Acquire);
        if !io.is_null() {
            // SAFETY: only `attach` and `attach_static` store here, each a pointer to counters that
            // live for the kernel's lifetime.
            unsafe { &*io }.record(op, ok, bytes, ns);
        }
    }
}

impl core::fmt::Debug for Counted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Counted({:p})", self.0.load(Ordering::Relaxed))
    }
}

impl Default for Counted {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each op lands in its own counters; a failure is an error with its time and
    /// nothing else; a rescan counts for nothing.
    #[test]
    fn each_op_is_counted_as_itself() {
        let io = IoStats::new();
        io.record(IrpOp::Read as u32, true, 4096, 10);
        io.record(IrpOp::Read as u32, true, 8192, 20);
        io.record(IrpOp::Write as u32, true, 512, 5);
        io.record(IrpOp::Flush as u32, true, 0, 7);
        io.record(IrpOp::Write as u32, false, 4096, 3);
        io.record(IrpOp::Rescan as u32, true, 0, 1000);
        let r = io.record_for(9, 0);
        assert_eq!((r.id, r.reads, r.writes, r.flushes, r.errors), (9, 2, 1, 1, 1));
        assert_eq!((r.read_bytes, r.written_bytes), (12288, 512), "a failed write moved nothing");
        assert_eq!(r.busy_ns, 10 + 20 + 5 + 7 + 3, "every command's time, the rescan's not");
    }

    /// Unattached, a hold records nothing and does not fault; attached, into what it holds.
    #[test]
    fn a_hold_records_once_attached() {
        static IO: IoStats = IoStats::new();
        let c = Counted::new();
        c.record(IrpOp::Read as u32, true, 4096, 1);
        assert_eq!(IO.record_for(0, 0).reads, 0);
        c.attach_static(&IO);
        c.record(IrpOp::Read as u32, true, 4096, 1);
        assert_eq!((IO.record_for(0, 0).reads, IO.record_for(0, 0).read_bytes), (1, 4096));
    }
}
