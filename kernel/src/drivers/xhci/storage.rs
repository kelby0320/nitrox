//! **USB mass storage, bulk-only**: a stick bound as a disk.
//!
//! See `docs/planning/phase-6-usb.md`. **Binding is the hub thread's** ([`bind`]):
//! `GET MAX LUN`, then for each unit `INQUIRY`, `TEST UNIT READY` until it is ready, and `READ
//! CAPACITY(10)` — `(16)` for a unit too big for it — then its partition table,
//! read before the disk is published — every command the thread's own, waited for and bounded at
//! five seconds, with nothing else yet waiting on the disk.
//!
//! **A unit of 2³² blocks or more** — 2 TiB of 512-byte blocks — is read and written with the
//! sixteen-byte commands, `READ(16)` and `WRITE(16)`; every other unit keeps the ten-byte ones,
//! which every stick answers and older ones answer alone.
//!
//! **A rescan is the hub thread's too** ([`rescan`]): a `Rescan` IRP on a disk's
//! node wakes it, and it retires the disk's partitions' windows, lets what they forwarded drain,
//! holds the device, departs the partitions, reads the table again as at the disk's arrival, and
//! publishes what it says.
//!
//! **The I/O path is the DPC's**, as AHCI's is its interrupt's. A block IRP's `submit` queues it
//! behind the one in flight, since bulk-only runs one command at a time. **A command goes on the
//! rings in two steps**: its command wrapper on bulk OUT and its data as one Normal TRB per IRP
//! fragment, the last interrupting; then, at that event, its status wrapper on bulk IN — QEMU's
//! device never answers a status wrapper queued behind data still moving. The DPC completes the IRP
//! at the status wrapper's event, and starts the next.
//!
//! **Everything else is the hub thread's** ([`recover`]): a stall, a status wrapper that is not a
//! pass, a command past its thirty-second deadline. The DPC marks the device and wakes the thread,
//! which follows bulk-only §6.7 — `REQUEST SENSE` for a failed command, a data-stage stall's halt
//! cleared and the status read, reset recovery for the rest — and a device that does not recover is
//! ended. **A departure** takes the device out of the table and completes what it held `PeerClosed`.
//!
//! **The devices are a value** ([`Disks`]), one static in a boot, eight slots under an epoch as the
//! HID nodes are: a node's context names its slot and the epoch it was made in, so a handle to a
//! departed stick is refused rather than served the next one's. Its methods decide and return what
//! to do — an IRP to complete, the hub thread to wake — and a host test drives them with heap
//! memory standing in for the rings.

use core::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use super::context;
use super::hub::{self, DeviceMem, Failed};
use super::ring::{Producer, Slots, Trb, code, kind};
use super::{RING_TRBS, Xhci};
use crate::arch::timer::ArchTimer;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpOp, PhysFrag};
use crate::io::stats::Counted;
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME, NameBuf};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::printable::Printable;
use crate::libkern::{IrqSpinLock, KVec};
use crate::mm::dma::DmaBuffer;
use crate::object::device_node::{BlockGeometry, DeviceNode, ResourceDescriptor};
use crate::syscall::error::KError;

/// Storage devices bound at once.
pub(super) const MAX_DEVICES: usize = 8;
/// Logical units a device may have; `GET MAX LUN` answers up to fifteen.
const MAX_LUNS: usize = 16;
/// Commands queued behind the one in flight, per device: AHCI's depth.
const QUEUE: usize = 32;
/// **The most fragments one command moves**: 64 pages, 256 KiB — `nxinstall`'s chunk, the largest
/// any client submits. A larger transfer is refused before it reaches here
/// (`TODO(block-transfer-split)`).
pub const MAX_FRAGS: u32 = 64;
/// The bound on each of the binding's commands, and on each of the hub thread's in recovery.
const BIND_NS: u64 = 5_000_000_000;
/// `TEST UNIT READY`'s bound: a stick still becoming ready, or reporting its reset.
const READY_NS: u64 = 5_000_000_000;
/// **A command's deadline once its disk is published**: what Linux's `sd` gives one.
pub const DEADLINE_NS: u64 = 30_000_000_000;
/// The binding's data buffer: a GPT's entries at most, 32 blocks.
const BIND_DATA: usize = 32 * 512;
/// The status wrapper's place in the wrapper page; the command wrapper is at 0.
const CSW_AT: u64 = 64;

/// A command block wrapper's length, and its signature, `USBC`.
pub const CBW_LEN: usize = 31;
const CBW_SIG: u32 = 0x4342_5355;
/// A command status wrapper's length, and its signature, `USBS`.
pub const CSW_LEN: usize = 13;
const CSW_SIG: u32 = 0x5342_5355;

/// **A command block wrapper**: `tag` echoed by the status, `len` bytes of data in the direction
/// `input` says, for logical unit `lun`, carrying `cdb`.
pub fn cbw(tag: u32, len: u32, input: bool, lun: u8, cdb: &[u8]) -> [u8; CBW_LEN] {
    let mut w = [0u8; CBW_LEN];
    w[0..4].copy_from_slice(&CBW_SIG.to_le_bytes());
    w[4..8].copy_from_slice(&tag.to_le_bytes());
    w[8..12].copy_from_slice(&len.to_le_bytes());
    w[12] = if input { 0x80 } else { 0 };
    w[13] = lun & 0xF;
    let n = cdb.len().min(16);
    w[14] = n as u8;
    w[15..15 + n].copy_from_slice(&cdb[..n]);
    w
}

/// **What a command status wrapper says** (§5.2, §6.7).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Status 0: the command passed, with this many bytes not moved.
    Passed { residue: u32 },
    /// Status 1: the command failed; `REQUEST SENSE` says why.
    Failed,
    /// Status 2: the device and host disagree about the phase; reset recovery.
    Phase,
    /// Not a wrapper for this command: wrong signature, wrong tag, a status beyond 2. Reset
    /// recovery, as a phase error.
    Invalid,
}

/// Read a status wrapper `b` for the command tagged `tag`.
pub fn csw(b: &[u8], tag: u32) -> Status {
    if b.len() < CSW_LEN || le32(b, 0) != CSW_SIG || le32(b, 4) != tag {
        return Status::Invalid;
    }
    match b[12] {
        0 => Status::Passed { residue: le32(b, 8) },
        1 => Status::Failed,
        2 => Status::Phase,
        _ => Status::Invalid,
    }
}

/// The commands this driver sends, each its CDB's bytes, and the answers it reads.
pub mod scsi;

const FREE: u8 = 0;
const BOUND: u8 = 1;
/// A slot whose device has departed: the next device may take it, under a new epoch.
const RETIRED: u8 = 2;

/// A node's `BlockBackend` context: its slot, its logical unit, and the slot's epoch when it was
/// made.
fn context(slot: usize, lun: u8, epoch: u32) -> *mut () {
    ((epoch as usize) << 16 | (lun as usize) << 8 | slot) as *mut ()
}

fn decode(ctx: *mut ()) -> (usize, u8, u32) {
    let c = ctx as usize;
    (c & 0xFF, (c >> 8) as u8, (c >> 16) as u32)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Owner {
    /// A block IRP's command; `retried` once a unit attention has sent it again.
    Irp { cmd: IrpCmd, retried: bool },
    /// One of the hub thread's own, which waits on the operation at `po`.
    Hub { po: *mut () },
}

/// **Where a command is**: its data stage — or its command wrapper, with no data — still moving, or
/// its status wrapper asked for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Stage {
    /// Waiting for the event of the stage's last TRB, on endpoint `dci`, which interrupts.
    Data { dci: u8 },
    /// Waiting for the status wrapper's TRB at this physical address.
    Status { csw_trb: u64 },
}

/// A command in flight: whose, its tag, where it is, its deadline, and when it was issued.
#[derive(Copy, Clone, Debug)]
struct Command {
    owner: Owner,
    tag: u32,
    stage: Stage,
    deadline: u64,
    started: u64,
}

/// **What went wrong with a command**, recorded by the DPC for the hub thread.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A transfer event with `code` on endpoint `dci`, before the status wrapper.
    Transfer { dci: u8, code: u8 },
    /// The status wrapper arrived, and said this.
    Status(Status),
    /// The deadline passed.
    Deadline,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum HubResult {
    Status(Status),
    Fault(Fault),
}

#[derive(Debug)]
struct Ring {
    producer: Producer,
    virt: u64,
    phys: u64,
}

/// A ring's TRBs at a virtual address — written by the DPC and by `submit`, which hold the address
/// rather than the buffer the hub thread owns.
struct RawSlots(u64);

impl Slots for RawSlots {
    fn len(&self) -> usize {
        RING_TRBS
    }

    fn read(&self, i: usize) -> Trb {
        let p = self.0 as *const u32;
        let mut d = [0u32; 4];
        for (n, dw) in d.iter_mut().enumerate() {
            // SAFETY: `i < RING_TRBS`, and the ring at this address holds that many TRBs for as long
            // as its device is in the table.
            *dw = unsafe { core::ptr::read_volatile(p.add(i * 4 + n)) };
        }
        Trb(d)
    }

    fn write(&mut self, i: usize, trb: Trb) {
        let p = self.0 as *mut u32;
        for n in 0..3 {
            // SAFETY: as for `read`.
            unsafe { core::ptr::write_volatile(p.add(i * 4 + n), trb.0[n]) };
        }
        // The cycle bit hands the TRB over, so it is written last.
        core::sync::atomic::fence(Ordering::Release);
        // SAFETY: as for `read`.
        unsafe { core::ptr::write_volatile(p.add(i * 4 + 3), trb.0[3]) };
    }
}

impl Ring {
    /// **Push a transfer descriptor of `trbs` TRBs**, padding with No Ops first if it would
    /// straddle the Link. The address of each TRB pushed is handed to `each`.
    fn push_td(&mut self, trbs: &[Trb], mut each: impl FnMut(usize, u64)) {
        if self.producer.room_before_link(RING_TRBS) < trbs.len() {
            while self.producer.room_before_link(RING_TRBS) != RING_TRBS - 1 {
                self.producer.push(&mut RawSlots(self.virt), Trb::no_op());
            }
        }
        for (k, t) in trbs.iter().enumerate() {
            let at = self.producer.push(&mut RawSlots(self.virt), *t);
            each(k, self.phys + at as u64 * 16);
        }
    }

    fn next(&self) -> (u64, bool) {
        (self.phys + self.producer.next_slot() as u64 * 16, self.producer.cycle())
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct Lun {
    blocks: u64,
    block_len: u32,
    /// Its disk's registry record, once published: what a rescan departs the partitions of.
    record: Option<u32>,
    /// **Stopped**: every later request is refused `PeerClosed`, as a pulled stick's are.
    ejected: bool,
    /// A refusal since was logged, once: nothing should ask an ejected unit for anything.
    refusal_said: bool,
}

/// **A bound storage device.** Its rings, wrapper page and binding buffer are the device's memory,
/// which the hub thread holds and frees after its slot is disabled; this keeps where they are.
#[derive(Debug)]
pub(super) struct Dev {
    slot: u8,
    interface: u8,
    in_dci: u8,
    out_dci: u8,
    in_address: u8,
    out_address: u8,
    in_mps: u16,
    out_mps: u16,
    bulk_in: Ring,
    bulk_out: Ring,
    /// The wrapper page: the command wrapper at 0, the status wrapper at [`CSW_AT`].
    wrap_virt: u64,
    wrap_phys: u64,
    /// The binding's data buffer, [`BIND_DATA`] bytes: what the hub thread's commands read into.
    data_virt: u64,
    data_phys: u64,
    db: u64,
    tag: u32,
    luns: [Lun; MAX_LUNS],
    inflight: Option<Command>,
    fault: Option<Fault>,
    hub_result: Option<HubResult>,
    /// **The hub thread is recovering it**: a submit queues rather than starting a command under
    /// the recovery's feet.
    recovering: bool,
    /// **An IRP a failed recovery could not stop**: its command's TRBs may still move its data, so it
    /// is completed only by the departure that follows, once the slot is disabled.
    stranded: Option<*mut Irp>,
    /// **A `Rescan` waiting for the hub thread**, and the unit it names: not a command on the
    /// rings, so held here rather than queued. **The device goes on meanwhile**: what is queued
    /// drains, so what the disk's windows forwarded before they were retired reaches the disk
    /// before the new table is published.
    rescan: Option<(*mut Irp, u8)>,
    queue: Queue,
    counted: [Counted; MAX_LUNS],
}

// SAFETY: `Dev` holds raw pointers to in-flight IRPs and an operation, which its lock serialises;
// the IRPs are owned by their submitters until completed, and the operation by the waiting thread.
unsafe impl Send for Dev {}

/// **A command's CDB**: ten bytes, or sixteen for a unit of 2³² blocks or more.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Cdb {
    bytes: [u8; 16],
    len: u8,
}

impl Cdb {
    fn of(b: &[u8]) -> Cdb {
        let mut bytes = [0u8; 16];
        let len = b.len().min(16);
        bytes[..len].copy_from_slice(&b[..len]);
        Cdb { bytes, len: len as u8 }
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// **The read or write of `count` blocks from `lba`** on a unit of `blocks` blocks: the ten-byte
/// command while every block's address fits in 32 bits, the sixteen-byte one past that.
fn rw_cdb(blocks: u64, write: bool, lba: u64, count: u16) -> Cdb {
    if blocks > 1 << 32 {
        Cdb::of(&scsi::rw16(write, lba, count as u32))
    } else {
        Cdb::of(&scsi::rw10(write, lba as u32, count))
    }
}

/// **A block IRP's command**, worked out and checked at submission: so an IRP that waits starts
/// without a second check that could fail where nothing can complete it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct IrpCmd {
    irp: *mut Irp,
    lun: u8,
    cdb: Cdb,
    len: u32,
    input: bool,
    /// The IRP's [`IrpOp`], as a `u32`.
    op: u32,
}

#[derive(Debug)]
struct Queue {
    ring: [Option<IrpCmd>; QUEUE],
    head: usize,
    len: usize,
}

impl Queue {
    const fn new() -> Queue {
        Queue { ring: [None; QUEUE], head: 0, len: 0 }
    }

    fn push(&mut self, c: IrpCmd) -> bool {
        if self.len == QUEUE {
            return false;
        }
        self.ring[(self.head + self.len) % QUEUE] = Some(c);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<IrpCmd> {
        if self.len == 0 {
            return None;
        }
        let e = self.ring[self.head].take();
        self.head = (self.head + 1) % QUEUE;
        self.len -= 1;
        e
    }

    /// Take out the first command `want` picks, the rest kept in their order.
    fn pop_where(&mut self, want: impl Fn(&IrpCmd) -> bool) -> Option<IrpCmd> {
        let (mut taken, mut kept) = (None, Queue::new());
        while let Some(c) = self.pop() {
            if taken.is_none() && want(&c) {
                taken = Some(c);
            } else {
                kept.push(c);
            }
        }
        *self = kept;
        taken
    }
}

/// **What the caller does after a method of [`Disks`]**, with no lock held: complete an IRP, end
/// one of the hub thread's waits, or wake the hub thread.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Nothing more.
    None,
    /// Complete `irp` with `status` and `transferred` bytes.
    Complete { irp: *mut Irp, status: i32, transferred: u64 },
    /// Complete the hub thread's operation at `po`.
    Hub { po: *mut () },
    /// The hub thread has a fault to handle.
    Wake,
    /// Complete `irp` `PeerClosed`, its unit ejected; `said` names it the first time, to log.
    Ejected { irp: *mut Irp, said: Option<(u8, u8)> },
    /// Complete the eject `irp`, and wake the hub thread to answer what waited on its unit.
    Stopped { irp: *mut Irp },
}

/// **The storage devices**, eight slots under an epoch. One static in a boot ([`DISKS`]); a value so
/// a host test can drive it.
pub(super) struct Disks {
    states: [AtomicU8; MAX_DEVICES],
    epochs: [AtomicU32; MAX_DEVICES],
    devs: [IrqSpinLock<Option<Dev>>; MAX_DEVICES],
}

static DISKS: Disks = Disks::new();

/// **The command for a block IRP**: its CDB, its length in the wrapper and its direction — or the
/// error to refuse it with, when it does not fit `lun`.
fn irp_command(op: u32, offset: u64, length: u64, lun: Lun) -> Result<(Cdb, u32, bool), KError> {
    if lun.blocks == 0 {
        return Err(KError::InvalidArgument);
    }
    if op == IrpOp::Flush as u32 {
        return Ok((Cdb::of(&scsi::synchronize_cache()), 0, false));
    }
    if op == IrpOp::Eject as u32 {
        return Ok((Cdb::of(&scsi::eject()), 0, false));
    }
    if op != IrpOp::Read as u32 && op != IrpOp::Write as u32 {
        return Err(KError::InvalidArgument);
    }
    let bl = lun.block_len as u64;
    if bl == 0 || length == 0 || !offset.is_multiple_of(bl) || !length.is_multiple_of(bl) {
        return Err(KError::InvalidArgument);
    }
    let (lba, count) = (offset / bl, length / bl);
    if lba.checked_add(count).is_none_or(|end| end > lun.blocks) || count > u16::MAX as u64 {
        return Err(KError::InvalidArgument);
    }
    let write = op == IrpOp::Write as u32;
    Ok((rw_cdb(lun.blocks, write, lba, count as u16), length as u32, !write))
}

fn packets(remaining: u64, mps: u16) -> u32 {
    remaining.div_ceil(mps.max(1) as u64).min(31) as u32
}

impl Dev {
    /// **Put a command on the rings**: its wrapper written, the command wrapper on bulk OUT, the
    /// data as a Normal TRB per fragment on the side it moves, the last interrupting on
    /// completion — or, with no data, the command wrapper itself — then the doorbells. **The status
    /// wrapper is asked for when that stage ends** ([`Dev::ask_status`]), not queued behind it:
    /// QEMU's `usb-storage` takes a status read that arrives while its data is still coming as
    /// part of the data stage, and never answers it, so a `READ(10)`, whose data comes from QEMU's
    /// disk asynchronously, hung while an `INQUIRY` answered at once. Linux asks for each stage
    /// after the last, which is why it never meets this.
    #[allow(clippy::too_many_arguments, reason = "a command's owner, unit, CDB, length, direction and buffer")]
    fn issue(&mut self, owner: Owner, lun: u8, cdb: &[u8], len: u32, input: bool, frags: &[PhysFrag], now: u64) {
        self.tag = self.tag.wrapping_add(1);
        let w = cbw(self.tag, len, input, lun, cdb);
        for (i, b) in w.iter().enumerate() {
            // SAFETY: the wrapper page holds a command wrapper at 0 and a status wrapper at CSW_AT,
            // and no transfer is using it: one command runs at a time.
            unsafe { core::ptr::write_volatile((self.wrap_virt as *mut u8).add(i), *b) };
        }
        for i in 0..CSW_LEN {
            // SAFETY: as above; a stale status must not read as this command's.
            unsafe { core::ptr::write_volatile((self.wrap_virt as *mut u8).add(CSW_AT as usize + i), 0) };
        }
        let frags = &frags[..frags.len().min(MAX_FRAGS as usize)];
        let no_data = frags.is_empty();
        self.bulk_out.push_td(&[Trb::bulk(self.wrap_phys, CBW_LEN as u32, false, no_data, false, 0)], |_, _| {});
        let mps = if input { self.in_mps } else { self.out_mps };
        let total: u64 = frags.iter().map(|f| f.len).sum();
        let mut data = [Trb::default(); MAX_FRAGS as usize];
        let mut moved = 0u64;
        for (k, f) in frags.iter().enumerate() {
            moved += f.len;
            let last = k + 1 == frags.len();
            data[k] = Trb::bulk(f.base, f.len as u32, !last, last, input, packets(total - moved, mps));
        }
        if !no_data {
            let side = if input { &mut self.bulk_in } else { &mut self.bulk_out };
            side.push_td(&data[..frags.len()], |_, _| {});
        }
        let dci = if !no_data && input { self.in_dci } else { self.out_dci };
        self.inflight = Some(Command {
            owner,
            tag: self.tag,
            stage: Stage::Data { dci },
            deadline: now + DEADLINE_NS,
            started: now,
        });
        self.ring(self.out_dci);
        if !no_data && input {
            self.ring(self.in_dci);
        }
    }

    /// **Ask for the status wrapper**, the command's data stage over: on bulk IN, interrupting.
    fn ask_status(&mut self) {
        let mut csw_trb = 0;
        let trb = Trb::bulk(self.wrap_phys + CSW_AT, CSW_LEN as u32, false, true, true, 0);
        self.bulk_in.push_td(&[trb], |_, at| csw_trb = at);
        if let Some(c) = self.inflight.as_mut() {
            c.stage = Stage::Status { csw_trb };
        }
        self.ring(self.in_dci);
    }

    fn ring(&self, dci: u8) {
        // SAFETY: `db` is the controller's doorbell array, or a host test's stand-in for it, with an
        // entry per slot.
        unsafe { core::ptr::write_volatile((self.db + 4 * self.slot as u64) as *mut u32, dci as u32) };
    }

    fn read_csw(&self) -> [u8; CSW_LEN] {
        let mut b = [0u8; CSW_LEN];
        for (i, v) in b.iter_mut().enumerate() {
            // SAFETY: the wrapper page, which the controller finished writing with the event that
            // brought this here.
            *v = unsafe { core::ptr::read_volatile((self.wrap_virt as *const u8).add(CSW_AT as usize + i)) };
        }
        b
    }

    fn ejected(&self, lun: u8) -> bool {
        self.luns.get(lun as usize).is_some_and(|u| u.ejected)
    }

    /// **The next queued command to start**: never one for an ejected unit.
    fn next(&mut self) -> Option<IrpCmd> {
        let luns = self.luns;
        self.queue.pop_where(|c| !luns.get(c.lun as usize).is_some_and(|u| u.ejected))
    }

    fn command_for(&self, irp: *mut Irp, lun: u8) -> Result<IrpCmd, KError> {
        let unit = self.luns.get(lun as usize).copied().unwrap_or_default();
        // SAFETY: `irp` is live and owned by its submitter until completed.
        let (op, offset, length) = unsafe { ((*irp).op, (*irp).offset, (*irp).length) };
        let (cdb, len, input) = irp_command(op, offset, length, unit)?;
        Ok(IrpCmd { irp, lun, cdb, len, input, op })
    }

    fn start(&mut self, c: IrpCmd, retried: bool, now: u64) {
        // SAFETY: `c.irp` is live and owned by its submitter until completed.
        let (count, frags) = unsafe { ((*c.irp).buffer.count, (*c.irp).buffer.frags) };
        let frags: &[PhysFrag] = if count == 0 || frags == 0 {
            &[]
        } else {
            // SAFETY: an IRP's fragments are `count` `PhysFrag`s its owner keeps for its life;
            // `dispatch_block_irp` refused more than `MAX_FRAGS`.
            unsafe { core::slice::from_raw_parts(frags as *const PhysFrag, (count as usize).min(MAX_FRAGS as usize)) }
        };
        self.issue(Owner::Irp { cmd: c, retried }, c.lun, c.cdb.as_slice(), c.len, c.input, frags, now);
    }
}

impl Disks {
    /// **Device `i` has gone**: out of the table, its slot retired. What it held, to be completed
    /// `PeerClosed` by the caller: its queue at once, and — once the slot is disabled — the command in
    /// flight and one a failed recovery stranded.
    fn depart(&self, i: usize) -> ([Option<*mut Irp>; QUEUE], [Option<*mut Irp>; 3]) {
        let dev = self.devs[i].lock().take();
        self.states[i].store(RETIRED, Ordering::Release);
        let mut queued = [None; QUEUE];
        let Some(mut dev) = dev else {
            return (queued, [None; 3]);
        };
        for q in queued.iter_mut() {
            *q = dev.queue.pop().map(|c| c.irp);
        }
        let inflight = match dev.inflight.take().map(|c| c.owner) {
            Some(Owner::Irp { cmd, .. }) => Some(cmd.irp),
            _ => None,
        };
        (queued, [inflight, dev.stranded.take(), dev.rescan.take().map(|(irp, _)| irp)])
    }

    fn slot_of(&self, slot: u8) -> Option<usize> {
        (0..MAX_DEVICES).find(|&i| self.devs[i].lock().as_ref().is_some_and(|d| d.slot == slot))
    }

    const fn new() -> Disks {
        Disks {
            states: [const { AtomicU8::new(FREE) }; MAX_DEVICES],
            epochs: [const { AtomicU32::new(0) }; MAX_DEVICES],
            devs: [const { IrqSpinLock::new(LockRank::Leaf, None) }; MAX_DEVICES],
        }
    }

    fn current(&self, ctx: *mut ()) -> bool {
        let (i, _, epoch) = decode(ctx);
        self.epochs.get(i).is_some_and(|e| e.load(Ordering::Acquire) == epoch)
    }

    /// **Take a slot for `dev`**: a free one, else a retired one under a new epoch. The slot and its
    /// epoch, or `None` when all are bound. From the hub thread.
    fn take(&self, dev: Dev) -> Option<(usize, u32)> {
        let states: [u8; MAX_DEVICES] = core::array::from_fn(|i| self.states[i].load(Ordering::Acquire));
        let i = states.iter().position(|&s| s == FREE).or_else(|| states.iter().position(|&s| s == RETIRED))?;
        if states[i] == RETIRED {
            self.epochs[i].fetch_add(1, Ordering::AcqRel);
        }
        *self.devs[i].lock() = Some(dev);
        self.states[i].store(BOUND, Ordering::Release);
        Some((i, self.epochs[i].load(Ordering::Acquire)))
    }

    /// **A block IRP submitted on any CPU**: started if nothing is in flight, else queued; refused
    /// `PeerClosed` once its device has gone, by its slot's epoch under the device's lock.
    fn submit(&self, irp: *mut Irp, ctx: *mut (), now: u64) -> Action {
        let refuse = |e: KError| Action::Complete { irp, status: e as i32, transferred: 0 };
        let (i, lun, _) = decode(ctx);
        let Some(slot) = self.devs.get(i) else {
            return refuse(KError::InvalidArgument);
        };
        let mut d = slot.lock();
        // **Under the device's lock**, against which a departure and a reused slot are ordered.
        let Some(dev) = d.as_mut().filter(|_| self.current(ctx)) else {
            return refuse(KError::PeerClosed);
        };
        // SAFETY: `irp` is live and owned by its submitter until completed.
        let op = unsafe { (*irp).op };
        if let Some(u) = dev.luns.get_mut(lun as usize).filter(|u| u.ejected) {
            if op == IrpOp::Flush as u32 {
                return Action::Complete { irp, status: 0, transferred: 0 };
            }
            let said = !core::mem::replace(&mut u.refusal_said, true);
            return Action::Ejected { irp, said: said.then_some((dev.slot, lun)) };
        }
        // **A rescan is the hub thread's**: held for it, and it woken — it reads the table through
        // its own commands once the device is quiet.
        if op == IrpOp::Rescan as u32 {
            if dev.luns.get(lun as usize).and_then(|u| u.record).is_none() {
                return refuse(KError::Unsupported);
            }
            if dev.rescan.is_some() {
                return refuse(KError::WouldBlock);
            }
            dev.rescan = Some((irp, lun));
            return Action::Wake;
        }
        let c = match dev.command_for(irp, lun) {
            Ok(c) => c,
            Err(e) => return refuse(e),
        };
        // **Not while a rescan waits**: until the hub thread takes the device, it runs as ever, so
        // what is queued drains before the table is read. Once taken, `recovering` holds it.
        if dev.inflight.is_some() || dev.fault.is_some() || dev.recovering {
            return if dev.queue.push(c) { Action::None } else { refuse(KError::IoError) };
        }
        dev.start(c, false, now);
        Action::None
    }

    /// **A transfer event for endpoint `dci` of xHCI slot `slot`**, from the DPC: `None` when it is
    /// no storage device's. The data stage's end asks for the status; the command ends at its status
    /// wrapper's event, and the next starts.
    fn on_transfer(&self, slot: u8, dci: u8, code: u8, pointer: u64, now: u64) -> Option<Action> {
        for s in &self.devs {
            let mut d = s.lock();
            let Some(dev) = d.as_mut() else { continue };
            if dev.slot != slot || (dci != dev.in_dci && dci != dev.out_dci) {
                continue;
            }
            let Some(cmd) = dev.inflight else {
                return Some(Action::None);
            };
            // **A command with a fault recorded is the hub thread's**, which will reset what is on
            // its rings: a later event of it changes nothing. A deadline marked as the command was
            // ending is not such a fault — the command made it, and its completion below clears
            // the mark rather than leave it to be charged to the next command.
            if dev.fault.is_some_and(|f| f != Fault::Deadline) {
                return Some(Action::None);
            }
            let ok = matches!(code, code::SUCCESS | code::SHORT_PACKET);
            let at_csw = match cmd.stage {
                // **The data stage is over**, whole or short — a short one ends at the TRB that came
                // up short, which the status wrapper's residue then says — so the status is asked for.
                Stage::Data { dci: data } if ok && dci == data => {
                    dev.ask_status();
                    return Some(Action::None);
                }
                Stage::Data { .. } => false,
                Stage::Status { csw_trb } => dci == dev.in_dci && pointer == csw_trb,
            };
            if ok && !at_csw {
                return Some(Action::None);
            }
            let fault = if !ok {
                Some(Fault::Transfer { dci, code })
            } else {
                match csw(&dev.read_csw(), cmd.tag) {
                    Status::Passed { residue: 0 } => None,
                    s => Some(Fault::Status(s)),
                }
            };
            if let Owner::Hub { po } = cmd.owner {
                dev.inflight = None;
                dev.hub_result = Some(match fault {
                    None => HubResult::Status(Status::Passed { residue: 0 }),
                    Some(Fault::Status(s)) => HubResult::Status(s),
                    Some(f) => HubResult::Fault(f),
                });
                return Some(Action::Hub { po });
            }
            let Owner::Irp { cmd: c, .. } = cmd.owner else { return Some(Action::None) };
            if let Some(f) = fault {
                dev.fault = Some(f);
                return Some(Action::Wake);
            }
            dev.inflight = None;
            dev.fault = None;
            dev.counted[c.lun as usize % MAX_LUNS].record(c.op, true, c.len as u64, now.saturating_sub(cmd.started));
            // **Ejected before the next starts**, so nothing queued behind the eject reaches the unit.
            let stopped = c.op == IrpOp::Eject as u32
                && dev.luns.get_mut(c.lun as usize).is_some_and(|u| !core::mem::replace(&mut u.ejected, true));
            // **Not while the hub thread holds the device.** A rescan waiting does not hold it: the
            // queue drains for it, and it takes the device once nothing is left.
            if !dev.recovering
                && let Some(next) = dev.next()
            {
                dev.start(next, false, now);
            }
            return Some(match stopped {
                true => Action::Stopped { irp: c.irp },
                false => Action::Complete { irp: c.irp, status: 0, transferred: c.len as u64 },
            });
        }
        None
    }
}

fn complete(irp: *mut Irp, status: i32, transferred: u64) {
    // SAFETY: `irp` is live and uniquely this driver's until its DPC runs.
    unsafe {
        (*irp).set_completion(status, transferred);
        crate::dpc::enqueue(&(*irp).dpc);
    }
}

/// Do what an [`Action`] says. No lock held. `x` is the controller, to wake the hub thread through:
/// the DPC's, the hub thread's, and a submit's — which wakes it only for a rescan; a command's
/// deadline it learns from [`Disks::bound`].
fn act(a: Action, x: Option<&Xhci>) {
    match a {
        Action::None => {}
        Action::Complete { irp, status, transferred } => complete(irp, status, transferred),
        Action::Ejected { irp, said } => {
            if let Some((slot, lun)) = said {
                crate::kprintln!(
                    "usb: slot {slot}: LUN {lun}: ejected, and asked for more; refused until plugged in again"
                );
            }
            complete(irp, KError::PeerClosed as i32, 0);
        }
        Action::Stopped { irp } => {
            complete(irp, 0, 0);
            act(Action::Wake, x);
        }
        // SAFETY: the hub thread holds `po`'s reference while its command is in flight, waiting on it.
        Action::Hub { po } => unsafe { crate::sched::complete_pending_op(po, 0, 0) },
        Action::Wake => {
            if let Some(x) = x {
                // SAFETY: `hub_wake` is the controller's `InterruptObject`, held for its life.
                unsafe { crate::sched::signal_interrupt(x.hub_wake.as_ptr()) };
            }
        }
    }
}

/// [`BlockBackend::submit`] for a USB disk: [`Disks::submit`], on the boot's.
fn submit(irp: *mut Irp, ctx: *mut ()) {
    let a = DISKS.submit(irp, ctx, crate::arch::Timer::read_ns());
    act(a, super::controller());
}

/// [`BlockBackend::poll`]: nothing. A USB disk exists only once the scheduler runs, so the boot's
/// polled reads never reach one.
fn poll(_ctx: *mut ()) {}

/// **A transfer event for an endpoint other than the default one**, from the DPC: whether it was a
/// storage device's.
pub(super) fn on_transfer(x: &Xhci, trb: &Trb) -> bool {
    let now = crate::arch::Timer::read_ns();
    match DISKS.on_transfer(trb.slot_id(), trb.endpoint_id(), trb.completion_code(), trb.pointer(), now) {
        Some(a) => {
            act(a, Some(x));
            true
        }
        None => false,
    }
}

/// A bulk-only interface prepared for its device's one configuration: its endpoints' rings and its
/// pages, which [`bind`] puts in the table.
pub(super) struct Prepared {
    interface: u8,
    in_ep: super::desc::Endpoint,
    out_ep: super::desc::Endpoint,
    bulk_in: Ring,
    bulk_out: Ring,
    wrap_virt: u64,
    wrap_phys: u64,
    data_virt: u64,
    data_phys: u64,
}

impl Prepared {
    /// The endpoints Configure Endpoint adds for it.
    pub(super) fn endpoints(&self) -> [context::Bulk; 2] {
        let (i, o) = (&self.in_ep, &self.out_ep);
        [
            context::Bulk {
                dci: i.dci(),
                input: true,
                max_packet: i.max_packet,
                burst: i.burst,
                ring: self.bulk_in.phys,
                cycle: self.bulk_in.producer.cycle(),
            },
            context::Bulk {
                dci: o.dci(),
                input: false,
                max_packet: o.max_packet,
                burst: o.burst,
                ring: self.bulk_out.phys,
                cycle: self.bulk_out.producer.cycle(),
            },
        ]
    }
}

/// **The device's bulk-only interface, prepared**: its rings, a wrapper page and the binding's data
/// buffer, all the device's memory. `None` when it has none, or memory ran out, which is said.
pub(super) fn prepare(port: u8, mem: &mut DeviceMem, config: &[u8]) -> Option<Prepared> {
    let s = super::desc::storage_interface(config)?;
    let (Ok(rin), Ok(rout), Ok(wrap), Ok(data)) = (
        DmaBuffer::alloc(RING_TRBS * 16),
        DmaBuffer::alloc(RING_TRBS * 16),
        DmaBuffer::alloc(crate::mm::PAGE_SIZE),
        DmaBuffer::alloc(BIND_DATA),
    ) else {
        crate::kprintln!("usb: port {port}: no memory for its storage endpoints; not bound");
        return None;
    };
    let ring = |b: &DmaBuffer| {
        let (virt, phys) = (b.virt() as u64, b.phys().as_u64());
        Ring { producer: Producer::new(&mut RawSlots(virt), phys), virt, phys }
    };
    let p = Prepared {
        interface: s.number,
        in_ep: s.bulk_in,
        out_ep: s.bulk_out,
        bulk_in: ring(&rin),
        bulk_out: ring(&rout),
        wrap_virt: wrap.virt() as u64,
        wrap_phys: wrap.phys().as_u64(),
        data_virt: data.virt() as u64,
        data_phys: data.phys().as_u64(),
    };
    for b in [rin, rout, wrap, data] {
        if mem.class.try_push(b).is_err() {
            crate::kprintln!("usb: port {port}: no memory for its storage endpoints; not bound");
            return None;
        }
    }
    Some(p)
}

/// **Bind the bulk-only interface `p` of the device in `slot`**, its configuration set: put it in
/// the table, then for each logical unit that is a ready direct-access device, read its partition
/// table and publish it — a `Disk` under the `UsbDevice` record `parent`, named by its INQUIRY
/// strings and `serial`, and its partitions under it.
pub(super) fn bind(x: &Xhci, port: u8, slot: u8, mem: &mut DeviceMem, p: Prepared, parent: Option<u32>, serial: &[u8]) {
    let dev = Dev {
        slot,
        interface: p.interface,
        in_dci: p.in_ep.dci(),
        out_dci: p.out_ep.dci(),
        in_address: p.in_ep.address,
        out_address: p.out_ep.address,
        in_mps: p.in_ep.max_packet,
        out_mps: p.out_ep.max_packet,
        bulk_in: p.bulk_in,
        bulk_out: p.bulk_out,
        wrap_virt: p.wrap_virt,
        wrap_phys: p.wrap_phys,
        data_virt: p.data_virt,
        data_phys: p.data_phys,
        db: x.db,
        tag: 0,
        luns: [Lun::default(); MAX_LUNS],
        inflight: None,
        fault: None,
        hub_result: None,
        recovering: false,
        stranded: None,
        rescan: None,
        queue: Queue::new(),
        counted: [const { Counted::new() }; MAX_LUNS],
    };
    let Some((i, epoch)) = DISKS.take(dev) else {
        crate::kprintln!("usb: port {port}: {MAX_DEVICES} storage devices are bound already; not bound");
        return;
    };
    let mut b = Binding { x, port, i, epoch, mem, parent, serial };
    let max_lun = match hub::control_in_byte(x, slot, b.mem, [0xA1, 0xFE, 0, 0, p.interface, 0, 1, 0]) {
        Ok(n) => n.min(MAX_LUNS as u8 - 1),
        // A stall means one unit (bulk-only §3.2); its endpoint is recovered.
        Err(Failed::Code(code::STALL)) => {
            let _ = hub::recover_ep0(x, slot, b.mem);
            0
        }
        Err(e) => {
            crate::kprintln!("usb: port {port}: GET MAX LUN failed: {e}; not bound");
            retire(i);
            return;
        }
    };
    let mut published = 0;
    for lun in 0..=max_lun {
        match b.unit(lun) {
            Ok(Some(u)) => {
                if b.publish(lun, &u) {
                    published += 1;
                }
            }
            Ok(None) => {}
            Err(why) => {
                crate::kprintln!("usb: port {port}: LUN {lun}: {why}; not bound, nor any unit after it");
                break;
            }
        }
    }
    if published == 0 {
        retire(i);
    }
}

struct Unit {
    inquiry: scsi::Inquiry,
    blocks: u64,
    block_len: u32,
}

struct Binding<'a> {
    x: &'a Xhci,
    port: u8,
    i: usize,
    epoch: u32,
    mem: &'a mut DeviceMem,
    parent: Option<u32>,
    serial: &'a [u8],
}

impl Binding<'_> {
    /// **What unit `lun` is**: a ready direct-access device's INQUIRY, block count and size, or
    /// `None` for one left alone — another kind, or no medium — which is said. `Err` when the
    /// device stopped answering, which ends the binding. **A unit of 2³² blocks or more**, which
    /// `READ CAPACITY(10)` cannot describe, is asked again with `READ CAPACITY(16)`.
    fn unit(&mut self, lun: u8) -> Result<Option<Unit>, &'static str> {
        let port = self.port;
        let mut buf = [0u8; scsi::INQUIRY_LEN];
        let inquiry = match self.run(lun, &scsi::inquiry(), &mut buf) {
            Ok(Status::Passed { .. }) => scsi::inquiry_answer(&buf).ok_or("INQUIRY's answer is short")?,
            Ok(_) => return Err("INQUIRY failed"),
            Err(_) => return Err("INQUIRY went unanswered"),
        };
        if !inquiry.direct_access {
            crate::kprintln!("usb: port {port}: LUN {lun}: not a direct-access block device; left alone");
            return Ok(None);
        }
        // **Ready**: a stick still starting up, or reporting the unit attention its reset raised, is
        // asked again; one with no medium is left alone.
        let start = crate::arch::Timer::read_ns();
        loop {
            match self.run(lun, &scsi::test_unit_ready(), &mut []) {
                Ok(Status::Passed { .. }) => break,
                Ok(Status::Failed) => {
                    let (key, asc, _) = self.sense(lun)?;
                    if key == scsi::NOT_READY && asc == scsi::MEDIUM_NOT_PRESENT {
                        crate::kprintln!("usb: port {port}: LUN {lun}: no medium; left alone");
                        return Ok(None);
                    }
                }
                Ok(_) | Err(_) => return Err("TEST UNIT READY went wrong"),
            }
            if crate::arch::Timer::read_ns().wrapping_sub(start) > READY_NS {
                crate::kprintln!("usb: port {port}: LUN {lun}: not ready after five seconds; left alone");
                return Ok(None);
            }
            hub::sleep(100_000_000);
        }
        let mut cap = [0u8; scsi::CAPACITY_LEN];
        let (blocks, block_len) = match self.run(lun, &scsi::read_capacity(), &mut cap) {
            Ok(Status::Passed { .. }) => match scsi::capacity_answer(&cap) {
                Some(c) => c,
                None => {
                    let mut cap16 = [0u8; scsi::CAPACITY16_LEN];
                    match self.run(lun, &scsi::read_capacity16(), &mut cap16) {
                        Ok(Status::Passed { .. }) if let Some(c) = scsi::capacity16_answer(&cap16) => {
                            crate::kprintln!(
                                "usb: port {port}: LUN {lun}: 2 TiB or more: {} blocks, \
                                 read with the sixteen-byte commands",
                                c.0
                            );
                            c
                        }
                        _ => {
                            crate::kprintln!(
                                "usb: port {port}: LUN {lun}: 2 TiB or more, and READ CAPACITY(16) went \
                                 unanswered; left alone"
                            );
                            return Ok(None);
                        }
                    }
                }
            },
            _ => return Err("READ CAPACITY went wrong"),
        };
        Ok(Some(Unit { inquiry, blocks, block_len }))
    }

    fn sense(&mut self, lun: u8) -> Result<(u8, u8, u8), &'static str> {
        let mut buf = [0u8; scsi::SENSE_LEN];
        match self.run(lun, &scsi::request_sense(), &mut buf) {
            Ok(Status::Passed { .. }) => Ok(scsi::sense_answer(&buf)),
            _ => Err("REQUEST SENSE went wrong"),
        }
    }

    fn run(&mut self, lun: u8, cdb: &[u8], out: &mut [u8]) -> Result<Status, Failed> {
        run(self.x, self.i, lun, cdb, out, self.mem)
    }

    /// **Publish unit `lun`**: its table read through the binding's commands, then its node under
    /// the device's record and its partitions under the node. Whether it was published.
    fn publish(&mut self, lun: u8, u: &Unit) -> bool {
        let port = self.port;
        let Some(parent) = self.parent else {
            crate::kprintln!("usb: port {port}: its device has no record; its disk is not published");
            return false;
        };
        let mut name = [0u8; MAX_DEVICE_NAME];
        let name_len = disk_name(&u.inquiry, self.serial, &mut name);
        let unit = Lun { blocks: u.blocks, block_len: u.block_len, ..Lun::default() };
        {
            let mut d = DISKS.devs[self.i].lock();
            if let Some(dev) = d.as_mut() {
                dev.luns[lun as usize] = unit;
            }
        }
        // **The table first**, before the disk can be reached: nothing else waits on it yet.
        let table = read_table(self.x, self.i, port, lun, unit, self.mem);
        let backend = BlockBackend { submit, poll, ctx: context(self.i, lun, self.epoch), max_frags: MAX_FRAGS };
        let geometry = BlockGeometry { logical_block_size: u.block_len, block_count: u.blocks };
        let Ok(node) =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, &name[..name_len], backend)
        else {
            crate::kprintln!("usb: port {port}: no memory for its disk's node");
            return false;
        };
        let node = crate::drivers::adopt(node, KObjectType::DeviceNode);
        let Some(id) = crate::device::register_block_under(node.clone(), parent, "usb-storage") else {
            return false;
        };
        // Kept with the unit, for a rescan to depart the disk's partitions by, and its counters.
        if let Some(dev) = DISKS.devs[self.i].lock().as_mut() {
            dev.luns[lun as usize].record = Some(id);
            // SAFETY: `node` was just registered, and the table holds it for the kernel's lifetime.
            unsafe { dev.counted[lun as usize].attach(&node) };
        }
        crate::kprintln!(
            "usb: port {port}: LUN {lun}: disk \"{}\", {} blocks of {} bytes, record {id}",
            Printable(&name[..name_len]),
            u.blocks,
            u.block_len
        );
        if let Some(table) = table {
            crate::drivers::gpt::publish(&node, table, crate::drivers::gpt::Names::None);
        }
        true
    }
}

/// **Read unit `lun`'s partition table** through the hub thread's commands, as at its disk's
/// arrival and again for a rescan: `None` for a unit of other than 512-byte blocks, which is said,
/// since the table reader takes those alone.
fn read_table(
    x: &Xhci,
    i: usize,
    port: u8,
    lun: u8,
    unit: Lun,
    mem: &mut DeviceMem,
) -> Option<Result<crate::drivers::partitions::Table, crate::drivers::partitions::Unread>> {
    use crate::drivers::partitions::{self, BLOCK};
    if unit.block_len as usize != BLOCK {
        crate::kprintln!("usb: port {port}: LUN {lun}: {}-byte blocks; its table is not read", unit.block_len);
        return None;
    }
    Some(partitions::read(unit.blocks, &mut |lba, count, out: &mut [u8]| {
        let len = count as usize * BLOCK;
        if len > BIND_DATA || out.len() < len || count > u16::MAX as u64 {
            return false;
        }
        let cdb = rw_cdb(unit.blocks, false, lba, count as u16);
        matches!(run(x, i, lun, cdb.as_slice(), &mut out[..len], mem), Ok(Status::Passed { residue: 0 }))
    }))
}

/// **Rescan a USB disk's table**, for the `Rescan` IRP device `i` holds, from the hub thread with
/// the device's memory:
/// 1. **the disk's partitions' windows retired first**, so nothing more comes through them, and
///    waited out until no submit is between a window's check and its forward ([`window_busy`]);
/// 2. **the device left to drain** — what those windows forwarded before, queued or in flight,
///    reaches the disk at the old table's offsets now, never after the new one is published — a
///    fault recovered on the way; then taken, every later submit queued;
/// 3. the partitions' records departed, the table read again as at the disk's arrival and
///    published, and the IRP completed with how many partitions it found — or `IoError` if the
///    table would not read.
///
/// **`Ended` for a device to end as on an unplug**: one whose recovery failed meanwhile, which
/// nothing will make quiet. Its departure completes the rescan.
///
/// [`window_busy`]: crate::io::block::window_busy
pub(super) fn rescan(x: &Xhci, i: usize, port: u8, mem: &mut DeviceMem) -> Recovered {
    use crate::io::block::{retire_window, window_busy};
    let start = crate::arch::Timer::read_ns();
    let late = || crate::arch::Timer::read_ns().wrapping_sub(start) > DEADLINE_NS + BIND_NS;
    let Some(unit) = DISKS.rescan_unit(i) else { return Recovered::Done };
    let gone = unit.record.map(crate::device::children).unwrap_or_default();
    let mut windows = KVec::new();
    for &child in gone.iter() {
        if let Some(node) = crate::device::node(child)
            && retire_window(&node)
        {
            // The window is retired whether or not it is kept to be waited out.
            let _ = windows.try_push(node);
        }
    }
    // **Giving up** — a device that never went quiet — leaves the retired windows' records
    // departed, since they refuse everything: the disk lists no partition until a rescan succeeds.
    let give_up = |why: &str| {
        if let Some(irp) = DISKS.drop_rescan(i) {
            crate::kprintln!("usb: port {port}: its disk {why}; not rescanned");
            complete(irp, KError::IoError as i32, 0);
        }
        for &child in gone.iter() {
            crate::device::depart(child);
        }
        Recovered::Done
    };
    while windows.iter().any(window_busy) {
        if late() {
            return give_up("kept a window busy");
        }
        hub::sleep(1_000_000);
    }
    let (irp, lun, unit) = loop {
        match DISKS.begin_rescan(i) {
            BeginRescan::Nothing => return Recovered::Done,
            BeginRescan::Ended => return Recovered::Ended,
            BeginRescan::Ready { irp, lun, unit } => break (irp, lun, unit),
            BeginRescan::Busy => {
                // A fault the DPC recorded is recovered first; a device that does not recover is
                // ended by the caller, and its departure completes the rescan. An ejected unit's first.
                drain_stopped(i);
                if let Recovered::Ended = recover(x, i, mem) {
                    return Recovered::Ended;
                }
                if late() {
                    return give_up("never went quiet");
                }
                hub::sleep(1_000_000);
            }
        }
    };
    let found = match unit.record.and_then(crate::device::node) {
        Some(disk) => {
            for &child in gone.iter() {
                crate::device::depart(child);
            }
            use crate::drivers::partitions::Unread;
            let table = read_table(x, i, port, lun, unit, mem);
            let found = match table {
                Some(Err(Unread::Io | Unread::NoMemory)) => None,
                Some(t) => Some(crate::drivers::gpt::publish(&disk, t, crate::drivers::gpt::Names::None)),
                None => Some(0),
            };
            match found {
                Some(n) => crate::kprintln!(
                    "usb: port {port}: LUN {lun}: rescanned: {} partition(s) departed, {n} published",
                    gone.len()
                ),
                None => crate::kprintln!(
                    "usb: port {port}: LUN {lun}: rescanned: {} partition(s) departed, and its table would not read",
                    gone.len()
                ),
            }
            Some(found)
        }
        None => None,
    };
    act(DISKS.end_rescan(i, crate::arch::Timer::read_ns()), Some(x));
    match found {
        Some(Some(n)) => complete(irp, 0, n as u64),
        Some(None) => complete(irp, KError::IoError as i32, 0),
        None => complete(irp, KError::PeerClosed as i32, 0),
    }
    Recovered::Done
}

/// **A USB disk's name**: its INQUIRY vendor and product, their padding trimmed, then the device's
/// serial in brackets, as a SATA disk's is its model and serial. The length written into `out`.
fn disk_name(inquiry: &scsi::Inquiry, serial: &[u8], out: &mut [u8]) -> usize {
    let trim = |b: &[u8]| -> usize { b.iter().rposition(|&c| c != b' ' && c != 0).map_or(0, |p| p + 1) };
    let mut w = NameBuf::new(out);
    let vendor = &inquiry.vendor[..trim(&inquiry.vendor)];
    let product = &inquiry.product[..trim(&inquiry.product)];
    let _ = core::fmt::Write::write_fmt(&mut w, format_args!("{} {}", Printable(vendor), Printable(product)));
    if !serial.is_empty() {
        let _ = core::fmt::Write::write_fmt(&mut w, format_args!(" ({})", Printable(serial)));
    }
    w.len()
}

/// **One of the hub thread's own commands** on device `i`: `cdb` for unit `lun`, its data read into
/// `out` — none when `out` is empty — waited for within [`BIND_NS`]. What its status wrapper said, or
/// why there is none. **A stall is recovered here**, as bulk-only §6.7 says: a data stage's halt
/// cleared and the status read; anything else, a timeout included, by reset recovery, which also
/// skips whatever the command left on the rings.
fn run(x: &Xhci, i: usize, lun: u8, cdb: &[u8], out: &mut [u8], mem: &mut DeviceMem) -> Result<Status, Failed> {
    let len = out.len().min(BIND_DATA) as u32;
    let result = match run_once(i, lun, cdb, len, false) {
        Err(e) => Err(e),
        Ok(HubResult::Status(s)) => Ok(s),
        Ok(HubResult::Fault(Fault::Transfer { dci, code: code::STALL }))
            if len > 0 && dci == dev_field(i, |d| d.in_dci) =>
        {
            clear_halt(x, i, dci, mem)?;
            match run_once(i, lun, cdb, 0, true) {
                Ok(HubResult::Status(s)) => Ok(s),
                Ok(HubResult::Fault(_)) => Err(Failed::Device("the status after a stall did not read")),
                Err(e) => Err(e),
            }
        }
        Ok(HubResult::Fault(Fault::Transfer { code, .. })) => Err(Failed::Code(code)),
        Ok(HubResult::Fault(_)) => Err(Failed::Device("a command went wrong")),
    };
    match result {
        Ok(Status::Phase | Status::Invalid) | Err(_) => {
            reset_recovery(x, i, mem)?;
        }
        Ok(_) => {}
    }
    if len > 0 && matches!(result, Ok(Status::Passed { .. })) {
        let d = DISKS.devs[i].lock();
        if let Some(dev) = d.as_ref() {
            for (k, b) in out[..len as usize].iter_mut().enumerate() {
                // SAFETY: the binding buffer holds BIND_DATA bytes, which the controller finished
                // writing with the command just waited for.
                *b = unsafe { core::ptr::read_volatile((dev.data_virt as *const u8).add(k)) };
            }
        }
    }
    result
}

fn dev_field<T: Default>(i: usize, f: impl FnOnce(&Dev) -> T) -> T {
    DISKS.devs[i].lock().as_ref().map(f).unwrap_or_default()
}

/// **Put one hub command on device `i`'s rings and wait for it**, within [`BIND_NS`]: its data, `len`
/// bytes, into the binding buffer. `status_only` reads a status wrapper alone, after a data stage's
/// stall was cleared, for the command last sent.
fn run_once(i: usize, lun: u8, cdb: &[u8], len: u32, status_only: bool) -> Result<HubResult, Failed> {
    let po = hub::new_operation()?;
    let now = crate::arch::Timer::read_ns();
    if !DISKS.issue_hub(i, po.as_ptr(), lun, cdb, len, status_only, now) {
        return Err(Failed::Device("its device has gone"));
    }
    let at = crate::arch::Timer::read_ns();
    let signalled = matches!(
        // SAFETY: `po` pins the `PendingOperation` waited on.
        unsafe { crate::sched::wait_on(&[po.as_ptr() as usize], at + BIND_NS, at) },
        crate::sched::WaitResult::Signaled(_)
    );
    if !signalled {
        match DISKS.take_back(i) {
            TakeBack::Taken => return Err(Failed::Timeout),
            TakeBack::Gone => return Err(Failed::Device("its device has gone")),
            // **The DPC took it first**, and completes `po` only once it has let the device's lock
            // go: so `po` is not dropped until then, as `hub::wait` waits.
            TakeBack::OnItsWay => {
                use crate::sched::{WaitResult, wait_on};
                // SAFETY: `po` pins the `PendingOperation` waited on, and outlives the closure.
                let done =
                    || matches!(unsafe { wait_on(&[po.as_ptr() as usize], u64::MAX, 0) }, WaitResult::Signaled(_));
                while !done() {
                    hub::sleep(1_000_000);
                }
            }
        }
    }
    let mut d = DISKS.devs[i].lock();
    let Some(dev) = d.as_mut() else { return Err(Failed::Device("its device has gone")) };
    dev.hub_result.take().ok_or(Failed::Timeout)
}

/// **Clear endpoint `dci`'s halt** on device `i`: Reset Endpoint — or Stop Endpoint, for one still
/// running — then its dequeue pointer past everything queued, then `CLEAR_FEATURE(ENDPOINT_HALT)` to
/// the device, which resets its side's data toggle.
fn clear_halt(x: &Xhci, i: usize, dci: u8, mem: &mut DeviceMem) -> Result<(), Failed> {
    let (slot, next, address) = {
        let d = DISKS.devs[i].lock();
        let Some(dev) = d.as_ref() else { return Err(Failed::Device("its device has gone")) };
        let input = dci == dev.in_dci;
        let ring = if input { &dev.bulk_in } else { &dev.bulk_out };
        (dev.slot, ring.next(), if input { dev.in_address } else { dev.out_address })
    };
    match hub::command(x, Trb::endpoint_command(kind::RESET_ENDPOINT, slot, dci)) {
        Ok(_) => {}
        Err(Failed::Code(code::CONTEXT_STATE_ERROR)) => {
            match hub::command(x, Trb::endpoint_command(kind::STOP_ENDPOINT, slot, dci)) {
                Ok(_) | Err(Failed::Code(code::CONTEXT_STATE_ERROR)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }
    hub::command(x, Trb::set_dequeue(next.0, next.1, slot, dci))?;
    hub::control_out(x, slot, mem, [0x02, 0x01, 0, 0, address, 0, 0, 0])
}

/// **Reset recovery** (bulk-only §5.3.4): the class's reset request, then both endpoints' halts
/// cleared.
fn reset_recovery(x: &Xhci, i: usize, mem: &mut DeviceMem) -> Result<(), Failed> {
    let (slot, interface, in_dci, out_dci) = {
        let d = DISKS.devs[i].lock();
        let Some(dev) = d.as_ref() else { return Err(Failed::Device("its device has gone")) };
        (dev.slot, dev.interface, dev.in_dci, dev.out_dci)
    };
    hub::control_out(x, slot, mem, [0x21, 0xFF, 0, 0, interface, 0, 0, 0])?;
    clear_halt(x, i, in_dci, mem)?;
    clear_halt(x, i, out_dci, mem)
}

fn retire(i: usize) {
    let _ = DISKS.depart(i);
}

/// **How long the hub thread may sleep** at `now` ([`Disks::bound`]).
pub(super) fn bound(now: u64) -> Option<u64> {
    DISKS.bound(now)
}

/// **The devices the hub thread must look at** ([`Disks::due`]).
pub(super) fn due(now: u64) -> [Option<u8>; MAX_DEVICES] {
    DISKS.due(now)
}

/// **Answer what waited on device `i`'s ejected units**, from the hub thread.
pub(super) fn drain_stopped(i: usize) {
    while let Some(a) = DISKS.take_stopped(i) {
        act(a, None);
    }
}

/// What [`recover`] did.
pub(super) enum Recovered {
    /// The command was finished or failed, and the device goes on.
    Done,
    /// The device did not recover: it is to be ended, as on an unplug.
    Ended,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum End {
    /// The IRP completes with this status, and the next queued starts.
    Complete(i32),
    /// A unit attention: the command again, once.
    Retry,
    /// **Reset recovery failed**, so the bulk endpoints may never have stopped and the command's
    /// TRBs may still move the IRP's data. It is held for the departure, which completes it once
    /// the slot is disabled, as it completes a command in flight.
    Stranded,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TakeBack {
    /// Taken back before the DPC reached it: nothing will complete its operation.
    Taken,
    /// The DPC took it first: its completion of the operation is on its way.
    OnItsWay,
    /// The device has gone.
    Gone,
}

impl Disks {
    /// **How long the hub thread may sleep at `now`**: until the earliest deadline of an IRP's
    /// command in flight, and **never longer than a deadline's length while any disk is bound**, so
    /// a command started while it sleeps is seen by its deadline with nothing waking the thread:
    /// one wake a deadline's length on an idle machine with a stick in, rather than one a command.
    /// `None` with no disk bound.
    fn bound(&self, now: u64) -> Option<u64> {
        let mut bound = None;
        for s in &self.devs {
            let d = s.lock();
            let Some(dev) = d.as_ref() else { continue };
            let mut until = bound.unwrap_or(now.saturating_add(DEADLINE_NS));
            if let Some(c) = dev.inflight
                && matches!(c.owner, Owner::Irp { .. })
            {
                until = until.min(c.deadline);
            }
            bound = Some(until);
        }
        bound
    }

    /// **The devices the hub thread must look at**: by table slot, the xHCI slot of each with a
    /// fault the DPC recorded, an IRP's command past its deadline at `now`, which is marked here,
    /// or a rescan waiting.
    fn due(&self, now: u64) -> [Option<u8>; MAX_DEVICES] {
        core::array::from_fn(|i| {
            let mut d = self.devs[i].lock();
            let dev = d.as_mut()?;
            if dev.fault.is_none()
                && let Some(c) = dev.inflight
                && matches!(c.owner, Owner::Irp { .. })
                && now > c.deadline
            {
                dev.fault = Some(Fault::Deadline);
            }
            let stopped = dev.queue.ring.iter().flatten().any(|c| dev.ejected(c.lun));
            (dev.fault.is_some() || dev.rescan.is_some() || stopped).then_some(dev.slot)
        })
    }

    /// **Begin recovering device `i`**: its fault and its IRP's command taken, so the hub thread's own
    /// commands can use the rings, and every submit queued until [`Disks::end_recovery`]. `None` when
    /// there is nothing to recover: no fault, or the command it was for has completed.
    fn begin_recovery(&self, i: usize) -> Option<(Fault, IrpCmd, bool)> {
        let mut d = self.devs[i].lock();
        let dev = d.as_mut()?;
        match (dev.fault.take(), dev.inflight.take()) {
            (Some(f), Some(Command { owner: Owner::Irp { cmd, retried }, .. })) => {
                dev.recovering = true;
                Some((f, cmd, retried))
            }
            (_, inflight) => {
                dev.inflight = inflight;
                None
            }
        }
    }

    /// **End device `i`'s recovery of `cmd`** as `end` says, in the one lock scope that lets
    /// submits start again: so none can start between the recovery's end and a retry, and have its
    /// command replaced by the retry's. A stranded command keeps the device recovering, so nothing
    /// starts on a device about to be ended. What to do after, with no lock held.
    fn end_recovery(&self, i: usize, cmd: IrpCmd, end: End, now: u64) -> Action {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else {
            return Action::Complete { irp: cmd.irp, status: KError::PeerClosed as i32, transferred: 0 };
        };
        match end {
            End::Stranded => {
                dev.stranded = Some(cmd.irp);
                Action::None
            }
            End::Retry => {
                dev.recovering = false;
                dev.start(cmd, true, now);
                Action::None
            }
            End::Complete(status) => {
                dev.recovering = false;
                if dev.inflight.is_none()
                    && dev.fault.is_none()
                    && let Some(next) = dev.next()
                {
                    dev.start(next, false, now);
                }
                let transferred = if status == 0 { cmd.len as u64 } else { 0 };
                // Counted, with none of the recovery's time: that was the hub thread's commands.
                dev.counted[cmd.lun as usize % MAX_LUNS].record(cmd.op, status == 0, transferred, 0);
                Action::Complete { irp: cmd.irp, status, transferred }
            }
        }
    }

    /// **Put one of the hub thread's commands on device `i`'s rings**, its operation at `po`: `cdb`
    /// for unit `lun`, its data, `len` bytes, into the binding buffer — or, `status_only`, a status
    /// wrapper alone, for the command last sent. Whether the device is still there.
    #[allow(clippy::too_many_arguments, reason = "a command's device, operation, unit, CDB, length and stage")]
    fn issue_hub(&self, i: usize, po: *mut (), lun: u8, cdb: &[u8], len: u32, status_only: bool, now: u64) -> bool {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return false };
        dev.hub_result = None;
        let owner = Owner::Hub { po };
        if status_only {
            let tag = dev.tag;
            dev.inflight =
                Some(Command { owner, tag, stage: Stage::Data { dci: 0 }, deadline: now + BIND_NS, started: now });
            dev.ask_status();
        } else {
            let frags = [PhysFrag { base: dev.data_phys, len: len as u64 }];
            let frags = if len == 0 { &frags[..0] } else { &frags[..] };
            dev.issue(owner, lun, cdb, len, len > 0, frags, now);
        }
        true
    }

    /// **The unit device `i`'s waiting rescan names**, for the hub thread to retire its partitions'
    /// windows by before it waits for the device to drain; `None` with no rescan waiting.
    fn rescan_unit(&self, i: usize) -> Option<Lun> {
        let d = self.devs[i].lock();
        let dev = d.as_ref()?;
        let (_, lun) = dev.rescan?;
        dev.luns.get(lun as usize).copied()
    }

    /// **Begin device `i`'s rescan**: once nothing is in flight, queued or recovering, its `Rescan`
    /// taken and the device held — every submit queued — until [`Disks::end_rescan`]. **`Ended` for
    /// a device a failed recovery stranded**, which nothing will make quiet: the hub thread ends
    /// it, and the departure completes the rescan.
    fn begin_rescan(&self, i: usize) -> BeginRescan {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return BeginRescan::Nothing };
        if dev.rescan.is_none() {
            return BeginRescan::Nothing;
        }
        if dev.stranded.is_some() {
            return BeginRescan::Ended;
        }
        if dev.inflight.is_some() || dev.fault.is_some() || dev.recovering || dev.queue.len > 0 {
            return BeginRescan::Busy;
        }
        let Some((irp, lun)) = dev.rescan.take() else { return BeginRescan::Nothing };
        dev.recovering = true;
        BeginRescan::Ready { irp, lun, unit: dev.luns.get(lun as usize).copied().unwrap_or_default() }
    }

    /// **End device `i`'s rescan**: submits start again, and the first queued behind it now.
    fn end_rescan(&self, i: usize, now: u64) -> Action {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return Action::None };
        dev.recovering = false;
        if dev.inflight.is_none()
            && dev.fault.is_none()
            && let Some(next) = dev.next()
        {
            dev.start(next, false, now);
        }
        Action::None
    }

    /// **One request that waited on an ejected unit of device `i`**, answered as one sent after the
    /// eject is ([`Disks::submit`]): each queued command, then a rescan held for the unit.
    fn take_stopped(&self, i: usize) -> Option<Action> {
        let mut d = self.devs[i].lock();
        let dev = d.as_mut()?;
        let luns = dev.luns;
        let (irp, op, lun) = match dev.queue.pop_where(|c| luns.get(c.lun as usize).is_some_and(|u| u.ejected)) {
            Some(c) => (c.irp, c.op, c.lun),
            None => {
                let (irp, lun) =
                    dev.rescan.take_if(|&mut (_, lun)| luns.get(lun as usize).is_some_and(|u| u.ejected))?;
                (irp, IrpOp::Rescan as u32, lun)
            }
        };
        if op == IrpOp::Flush as u32 {
            return Some(Action::Complete { irp, status: 0, transferred: 0 });
        }
        let said = dev.luns.get_mut(lun as usize).is_some_and(|u| !core::mem::replace(&mut u.refusal_said, true));
        Some(Action::Ejected { irp, said: said.then_some((dev.slot, lun)) })
    }

    /// **Give up device `i`'s waiting rescan**: its IRP, for the caller to complete.
    fn drop_rescan(&self, i: usize) -> Option<*mut Irp> {
        self.devs[i].lock().as_mut().and_then(|dev| dev.rescan.take()).map(|(irp, _)| irp)
    }

    /// **A hub command's wait ran out**: taken back if the DPC has not reached it, so nothing
    /// completes an operation the hub thread is about to drop.
    fn take_back(&self, i: usize) -> TakeBack {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return TakeBack::Gone };
        if matches!(dev.inflight, Some(Command { owner: Owner::Hub { .. }, .. })) {
            dev.inflight = None;
            return TakeBack::Taken;
        }
        TakeBack::OnItsWay
    }
}

/// **What a failed command's sense key comes to**, or `None` to send it again: an illegal request
/// to flush is no cache to flush, and one to eject a stick that cannot.
fn sense_outcome(key: u8, op: u32, retried: bool) -> Option<i32> {
    match key {
        scsi::ILLEGAL_REQUEST if op == IrpOp::Flush as u32 => Some(0),
        scsi::ILLEGAL_REQUEST if op == IrpOp::Eject as u32 => Some(KError::Unsupported as i32),
        scsi::UNIT_ATTENTION if !retried => None,
        _ => Some(KError::IoError as i32),
    }
}

/// **Recover device `i`**, whose command went wrong (bulk-only §6.7): a failed command's sense
/// read and judged by [`sense_outcome`] — a unit attention sends the command
/// again once — a data stage's stall cleared and its status read, and reset recovery for anything
/// else, a deadline included. The command is completed, and the next started. From the hub thread,
/// with the device's memory.
pub(super) fn recover(x: &Xhci, i: usize, mem: &mut DeviceMem) -> Recovered {
    let Some((fault, cmd, retried)) = DISKS.begin_recovery(i) else {
        return Recovered::Done;
    };
    let mut sense = [0u8; scsi::SENSE_LEN];
    let outcome = match fault {
        Fault::Status(Status::Failed) => match run(x, i, cmd.lun, &scsi::request_sense(), &mut sense, mem) {
            Ok(Status::Passed { .. }) => Ok(sense_outcome(scsi::sense_answer(&sense).0, cmd.op, retried)),
            _ => Err(()),
        },
        Fault::Transfer { dci, code: code::STALL }
            if dci == dev_field(i, |d| if cmd.input { d.in_dci } else { d.out_dci }) =>
        {
            match clear_halt(x, i, dci, mem).and_then(|_| run_once(i, cmd.lun, &[], 0, true)) {
                Ok(_) => Ok(Some(KError::IoError as i32)),
                Err(_) => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
            }
        }
        _ => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
    };
    let end = match outcome {
        Ok(Some(status)) => End::Complete(status),
        Ok(None) => End::Retry,
        Err(()) => End::Stranded,
    };
    act(DISKS.end_recovery(i, cmd, end, crate::arch::Timer::read_ns()), Some(x));
    if end == End::Stranded { Recovered::Ended } else { Recovered::Done }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum BeginRescan {
    /// No rescan is waiting, or the device has gone.
    Nothing,
    /// One is waiting, behind a command in flight, one queued, or a recovery.
    Busy,
    /// One is waiting on a device a failed recovery stranded, which is to be ended.
    Ended,
    /// It is the hub thread's, for unit `lun`, and the device is held.
    Ready { irp: *mut Irp, lun: u8, unit: Lun },
}

/// What a departure leaves for after the slot is disabled: the command that was in flight, one a
/// failed recovery stranded, and a rescan waiting.
pub(super) struct Departed([Option<*mut Irp>; 3]);

/// **The device in xHCI slot `slot` has gone**: out of the table, so the DPC touches none of its
/// memory; its queue completed `PeerClosed` now, and any later submit refused so by its slot's
/// epoch. The command in flight is handed back, for [`finish`] once the slot is disabled: until
/// then the controller may still move its data. From the hub thread.
pub(super) fn depart(slot: u8) -> Departed {
    let Some(i) = DISKS.slot_of(slot) else {
        return Departed([None; 3]);
    };
    let (queued, held) = DISKS.depart(i);
    for irp in queued.into_iter().flatten() {
        complete(irp, KError::PeerClosed as i32, 0);
    }
    Departed(held)
}

/// **Complete what a departure handed back**, `PeerClosed`, now that the slot is disabled.
pub(super) fn finish(d: Departed) {
    for irp in d.0.into_iter().flatten() {
        complete(irp, KError::PeerClosed as i32, 0);
    }
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod tests;
