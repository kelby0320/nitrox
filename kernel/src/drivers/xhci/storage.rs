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
use super::desc;
use super::hub::{self, DeviceMem, Failed};
use super::ring::{Producer, Slots, Trb, code, kind};
use super::{RING_TRBS, Xhci};
use crate::arch::timer::ArchTimer;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpOp, PhysFrag};
use crate::io::stats::Counted;
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME, NameBuf};
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

mod bind;
mod recover;
pub(super) use bind::*;
pub(super) use recover::*;

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
    /// Waiting for the status wrapper's TRB at this physical address; `short` if the data was.
    Status { csw_trb: u64, short: bool },
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
    /// A read's data stage came up short, yet its status said nothing was missing: the pages it
    /// did not fill would be read as the file, holding whatever they held before.
    Short,
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
    fn ask_status(&mut self, short: bool) {
        let mut csw_trb = 0;
        let trb = Trb::bulk(self.wrap_phys + CSW_AT, CSW_LEN as u32, false, true, true, 0);
        self.bulk_in.push_td(&[trb], |_, at| csw_trb = at);
        if let Some(c) = self.inflight.as_mut() {
            c.stage = Stage::Status { csw_trb, short };
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
                    dev.ask_status(code == code::SHORT_PACKET);
                    return Some(Action::None);
                }
                Stage::Data { .. } => false,
                Stage::Status { csw_trb, .. } => dci == dev.in_dci && pointer == csw_trb,
            };
            if ok && !at_csw {
                return Some(Action::None);
            }
            let fault = if !ok {
                Some(Fault::Transfer { dci, code })
            } else {
                let short = matches!((cmd.stage, cmd.owner), (Stage::Status { short: true, .. }, Owner::Irp { .. }));
                match csw(&dev.read_csw(), cmd.tag) {
                    Status::Passed { residue: 0 } if short => Some(Fault::Short),
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
    // SAFETY: `irp` is live and uniquely this driver's, completed here once.
    unsafe { crate::io::irp::complete(irp, status, transferred) };
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

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod tests;
