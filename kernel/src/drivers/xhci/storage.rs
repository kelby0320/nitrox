//! **USB mass storage, bulk-only** (Phase 6 Part D.2–D.3): a stick bound as a disk.
//!
//! `docs/planning/phase-6-usb.md` § *Part D in detail*. **Binding is the hub thread's** ([`bind`]):
//! `GET MAX LUN`, then for each unit `INQUIRY`, `TEST UNIT READY` until it is ready, and `READ
//! CAPACITY(10)`, then its partition table, read before the disk is published — every command the
//! thread's own, waited for and bounded at five seconds, with nothing else yet waiting on the disk.
//!
//! **The I/O path is the DPC's**, as AHCI's is its interrupt's. A block IRP's `submit` queues it
//! behind the one in flight, since bulk-only runs one command at a time. **A command goes on the
//! rings whole**: its command wrapper on bulk OUT, its data as one Normal TRB per IRP fragment, and
//! its status wrapper on bulk IN, whose completion alone interrupts. The DPC completes the IRP at the
//! status wrapper's event, and starts the next.
//!
//! **Everything else is the hub thread's** ([`recover`]): a stall, a status wrapper that is not a
//! pass, a command past its thirty-second deadline. The DPC marks the device and wakes the thread,
//! which follows bulk-only §6.7 — `REQUEST SENSE` for a failed command, a data-stage stall's halt
//! cleared and the status read, reset recovery for the rest — and a device that does not recover is
//! ended. **A departure** takes the device out of the table and completes what it held `PeerClosed`.
//!
//! **The devices are a value** ([`Disks`]), one static in a boot, eight slots under an epoch as the
//! HID nodes are (PR #361's lesson): a node's context names its slot and the epoch it was made in,
//! so a handle to a departed stick is refused rather than served the next one's. Its methods decide
//! and return what to do — an IRP to complete, the hub thread to wake — and a host test drives them
//! with heap memory standing in for the rings.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

use super::context;
use super::hub::{self, DeviceMem, Failed};
use super::ring::{Producer, Slots, Trb, code, kind};
use super::{RING_TRBS, Xhci};
use crate::arch::timer::ArchTimer;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpOp, PhysFrag};
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME, NameBuf};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::printable::Printable;
use crate::libkern::IrqSpinLock;
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

// --- Bulk-only's wrappers (USB Mass Storage Class Bulk-Only Transport 1.0 §5) ------------------

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

// --- SCSI (SPC-4, SBC-3) ---------------------------------------------------------------------

/// The commands this driver sends, each its CDB's bytes, and the answers it reads.
pub mod scsi {
    /// `TEST UNIT READY`.
    pub const fn test_unit_ready() -> [u8; 6] {
        [0x00, 0, 0, 0, 0, 0]
    }

    /// `REQUEST SENSE` for 18 bytes of fixed-format sense data.
    pub const fn request_sense() -> [u8; 6] {
        [0x03, 0, 0, 0, SENSE_LEN as u8, 0]
    }

    /// `INQUIRY` for the standard data's first 36 bytes.
    pub const fn inquiry() -> [u8; 6] {
        [0x12, 0, 0, 0, INQUIRY_LEN as u8, 0]
    }

    /// `READ CAPACITY(10)`.
    pub const fn read_capacity() -> [u8; 10] {
        [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    /// `READ(10)` or `WRITE(10)` of `count` blocks from `lba`, big-endian.
    pub fn rw10(write: bool, lba: u32, count: u16) -> [u8; 10] {
        let l = lba.to_be_bytes();
        let c = count.to_be_bytes();
        [if write { 0x2A } else { 0x28 }, 0, l[0], l[1], l[2], l[3], 0, c[0], c[1], 0]
    }

    /// `SYNCHRONIZE CACHE(10)` of the whole medium: an LBA and a count of zero.
    pub const fn synchronize_cache() -> [u8; 10] {
        [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]
    }

    pub const SENSE_LEN: usize = 18;
    pub const INQUIRY_LEN: usize = 36;
    pub const CAPACITY_LEN: usize = 8;

    /// Sense keys this driver acts on.
    pub const NOT_READY: u8 = 0x2;
    pub const ILLEGAL_REQUEST: u8 = 0x5;
    pub const UNIT_ATTENTION: u8 = 0x6;
    /// The additional sense code for "medium not present", under `NOT_READY`.
    pub const MEDIUM_NOT_PRESENT: u8 = 0x3A;

    /// What `INQUIRY` says of a unit.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Inquiry {
        /// A direct-access block device, connected: qualifier 0, type 0.
        pub direct_access: bool,
        pub vendor: [u8; 8],
        pub product: [u8; 16],
    }

    /// Read `INQUIRY`'s answer, or `None` if it is too short to be one.
    pub fn inquiry_answer(b: &[u8]) -> Option<Inquiry> {
        if b.len() < INQUIRY_LEN {
            return None;
        }
        let mut vendor = [0u8; 8];
        vendor.copy_from_slice(&b[8..16]);
        let mut product = [0u8; 16];
        product.copy_from_slice(&b[16..32]);
        Some(Inquiry { direct_access: b[0] == 0x00, vendor, product })
    }

    /// **What `READ CAPACITY(10)` says**: the block count and size, or `None` for a unit of 2 TiB or
    /// more — its last LBA reads `0xFFFFFFFF`, which the ten-byte commands cannot address past.
    pub fn capacity_answer(b: &[u8]) -> Option<(u64, u32)> {
        if b.len() < CAPACITY_LEN {
            return None;
        }
        let last = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let block = u32::from_be_bytes([b[4], b[5], b[6], b[7]]);
        (last != u32::MAX).then_some((last as u64 + 1, block))
    }

    /// Fixed-format sense data's key, additional sense code and qualifier.
    pub fn sense_answer(b: &[u8]) -> (u8, u8, u8) {
        if b.len() < 14 {
            return (0, 0, 0);
        }
        (b[2] & 0xF, b[12], b[13])
    }
}

// --- The devices ------------------------------------------------------------------------------

/// A slot no device has had.
const FREE: u8 = 0;
/// A slot whose device is bound.
const BOUND: u8 = 1;
/// A slot whose device has departed: the next device may take it, under a new epoch.
const RETIRED: u8 = 2;

/// A node's `BlockBackend` context: its slot, its logical unit, and the slot's epoch when it was
/// made.
fn context(slot: usize, lun: u8, epoch: u32) -> *mut () {
    ((epoch as usize) << 16 | (lun as usize) << 8 | slot) as *mut ()
}

/// The slot, unit and epoch a context names.
fn decode(ctx: *mut ()) -> (usize, u8, u32) {
    let c = ctx as usize;
    (c & 0xFF, (c >> 8) as u8, (c >> 16) as u32)
}

/// What a command in flight is for.
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

/// A command in flight: whose, its tag, where it is, and its deadline.
#[derive(Copy, Clone, Debug)]
struct Command {
    owner: Owner,
    tag: u32,
    stage: Stage,
    deadline: u64,
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

/// What the hub thread's own command found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum HubResult {
    Status(Status),
    Fault(Fault),
}

/// A ring the driver writes, by its virtual and physical bases.
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

    /// Where the controller will next read: the dequeue pointer a Set TR Dequeue Pointer skips to.
    fn next(&self) -> (u64, bool) {
        (self.phys + self.producer.next_slot() as u64 * 16, self.producer.cycle())
    }
}

/// A logical unit: its size in blocks and its block size; zero blocks for one not published.
#[derive(Copy, Clone, Debug, Default)]
struct Lun {
    blocks: u64,
    block_len: u32,
}

/// **A bound storage device.** Its rings, wrapper page and binding buffer are the device's memory,
/// which the hub thread holds and frees after its slot is disabled; this keeps where they are.
#[derive(Debug)]
pub(super) struct Dev {
    /// Its xHCI slot, its interface, and its endpoints' indices, addresses and maximum packets.
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
    /// The doorbell array's base: the controller's, or a host test's.
    db: u64,
    tag: u32,
    luns: [Lun; MAX_LUNS],
    inflight: Option<Command>,
    /// A command the DPC found going wrong, for the hub thread.
    fault: Option<Fault>,
    /// What the hub thread's own last command found.
    hub_result: Option<HubResult>,
    /// **The hub thread is recovering it**: a submit queues rather than starting a command under
    /// the recovery's feet.
    recovering: bool,
    queue: Queue,
}

// SAFETY: `Dev` holds raw pointers to in-flight IRPs and an operation, which its lock serialises;
// the IRPs are owned by their submitters until completed, and the operation by the waiting thread.
unsafe impl Send for Dev {}

/// **A block IRP's command**, worked out and checked at submission: so an IRP that waits starts
/// without a second check that could fail where nothing can complete it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct IrpCmd {
    irp: *mut Irp,
    lun: u8,
    cdb: [u8; 10],
    len: u32,
    input: bool,
    flush: bool,
}

/// The IRPs waiting for the one in flight, in order.
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
}

/// **The storage devices**, eight slots under an epoch. One static in a boot ([`DISKS`]); a value so
/// a host test can drive it.
pub(super) struct Disks {
    states: [AtomicU8; MAX_DEVICES],
    epochs: [AtomicU32; MAX_DEVICES],
    devs: [IrqSpinLock<Option<Dev>>; MAX_DEVICES],
}

/// The boot's storage devices.
static DISKS: Disks = Disks::new();

/// The hub thread has a fault or a deadline to look at.
static DUE: AtomicBool = AtomicBool::new(false);

/// **The command for a block IRP**: its CDB, its length in the wrapper, its direction, whether it is
/// a flush — or the error to refuse it with, when it does not fit `lun`.
fn irp_command(op: u32, offset: u64, length: u64, lun: Lun) -> Result<([u8; 10], u32, bool, bool), KError> {
    if lun.blocks == 0 {
        return Err(KError::InvalidArgument);
    }
    if op == IrpOp::Flush as u32 {
        return Ok((scsi::synchronize_cache(), 0, false, true));
    }
    let bl = lun.block_len as u64;
    if bl == 0 || length == 0 || offset % bl != 0 || length % bl != 0 {
        return Err(KError::InvalidArgument);
    }
    let (lba, count) = (offset / bl, length / bl);
    if lba.checked_add(count).is_none_or(|end| end > lun.blocks) || count > u16::MAX as u64 || lba > u32::MAX as u64 {
        return Err(KError::InvalidArgument);
    }
    let write = op == IrpOp::Write as u32;
    Ok((scsi::rw10(write, lba as u32, count as u16), length as u32, !write, false))
}

/// How many packets of `mps` bytes `remaining` bytes take, for a TRB's TD Size.
fn packets(remaining: u64, mps: u16) -> u32 {
    remaining.div_ceil(mps.max(1) as u64).min(31) as u32
}

impl Dev {
    /// **Put a command on the rings**: its wrapper written, the command wrapper on bulk OUT, the
    /// data as a Normal TRB per fragment on the side it moves, the last interrupting on completion —
    /// or, with no data, the command wrapper itself — then the doorbells. **The status wrapper is
    /// asked for when that stage ends** ([`Dev::ask_status`]), not queued behind it: QEMU's
    /// `usb-storage` takes a status read that arrives while its data is still coming as part of the
    /// data stage, and never answers it — measured in `test-qemu`, where a `READ(10)`, whose data
    /// comes from QEMU's disk asynchronously, hung while `INQUIRY`, answered at once, did not. Linux
    /// asks for each stage after the last, which is why it never meets this.
    #[allow(clippy::too_many_arguments)]
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
        self.inflight = Some(Command { owner, tag: self.tag, stage: Stage::Data { dci }, deadline: now + DEADLINE_NS });
        self.ring(self.out_dci);
        if !no_data && input {
            self.ring(self.in_dci);
        }
    }

    /// **Ask for the status wrapper**, the command's data stage over: its TRB on bulk IN,
    /// interrupting on completion, and IN's doorbell.
    fn ask_status(&mut self) {
        let mut csw_trb = 0;
        let trb = Trb::bulk(self.wrap_phys + CSW_AT, CSW_LEN as u32, false, true, true, 0);
        self.bulk_in.push_td(&[trb], |_, at| csw_trb = at);
        if let Some(c) = self.inflight.as_mut() {
            c.stage = Stage::Status { csw_trb };
        }
        self.ring(self.in_dci);
    }

    /// Ring endpoint `dci`'s doorbell.
    fn ring(&self, dci: u8) {
        // SAFETY: `db` is the controller's doorbell array, or a host test's stand-in for it, with an
        // entry per slot.
        unsafe { core::ptr::write_volatile((self.db + 4 * self.slot as u64) as *mut u32, dci as u32) };
    }

    /// The status wrapper as the device wrote it.
    fn read_csw(&self) -> [u8; CSW_LEN] {
        let mut b = [0u8; CSW_LEN];
        for (i, v) in b.iter_mut().enumerate() {
            // SAFETY: the wrapper page, which the controller finished writing with the event that
            // brought this here.
            *v = unsafe { core::ptr::read_volatile((self.wrap_virt as *const u8).add(CSW_AT as usize + i)) };
        }
        b
    }

    /// **The command for `irp` on unit `lun`**, or the error to refuse it with.
    fn command_for(&self, irp: *mut Irp, lun: u8) -> Result<IrpCmd, KError> {
        let unit = self.luns.get(lun as usize).copied().unwrap_or_default();
        // SAFETY: `irp` is live and owned by its submitter until completed.
        let (op, offset, length) = unsafe { ((*irp).op, (*irp).offset, (*irp).length) };
        let (cdb, len, input, flush) = irp_command(op, offset, length, unit)?;
        Ok(IrpCmd { irp, lun, cdb, len, input, flush })
    }

    /// **Start an IRP's command**: on the rings whole.
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
        self.issue(Owner::Irp { cmd: c, retried }, c.lun, &c.cdb, c.len, c.input, frags, now);
    }
}

impl Disks {
    /// **Device `i` has gone**: out of the table, its slot retired. What it held, to be completed
    /// `PeerClosed` by the caller: its queue, and the command in flight.
    fn depart(&self, i: usize) -> ([Option<*mut Irp>; QUEUE], Option<*mut Irp>) {
        let dev = self.devs[i].lock().take();
        self.states[i].store(RETIRED, Ordering::Release);
        let mut queued = [None; QUEUE];
        let Some(mut dev) = dev else {
            return (queued, None);
        };
        for q in queued.iter_mut() {
            *q = dev.queue.pop().map(|c| c.irp);
        }
        let inflight = match dev.inflight.take().map(|c| c.owner) {
            Some(Owner::Irp { cmd, .. }) => Some(cmd.irp),
            _ => None,
        };
        (queued, inflight)
    }

    /// The xHCI slot's storage device's table slot, if one is bound there.
    fn slot_of(&self, slot: u8) -> Option<usize> {
        (0..MAX_DEVICES).find(|&i| self.devs[i].lock().as_ref().is_some_and(|d| d.slot == slot))
    }

    /// Every slot free.
    const fn new() -> Disks {
        Disks {
            states: [const { AtomicU8::new(FREE) }; MAX_DEVICES],
            epochs: [const { AtomicU32::new(0) }; MAX_DEVICES],
            devs: [const { IrqSpinLock::new(LockRank::Leaf, None) }; MAX_DEVICES],
        }
    }

    /// Whether a context's node is still its slot's: the slot has not been given to another device.
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
        let c = match dev.command_for(irp, lun) {
            Ok(c) => c,
            Err(e) => return refuse(e),
        };
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
                // Left in flight, for the hub thread to recover and end.
                dev.fault = Some(f);
                return Some(Action::Wake);
            }
            dev.inflight = None;
            if let Some(next) = dev.queue.pop() {
                dev.start(next, false, now);
            }
            return Some(Action::Complete { irp: c.irp, status: 0, transferred: c.len as u64 });
        }
        None
    }
}

/// Complete an IRP: its status set and its DPC queued. No lock held.
fn complete(irp: *mut Irp, status: i32, transferred: u64) {
    // SAFETY: `irp` is live and uniquely this driver's until its DPC runs.
    unsafe {
        (*irp).set_completion(status, transferred);
        crate::dpc::enqueue(&(*irp).dpc);
    }
}

/// Do what an [`Action`] says. No lock held.
fn act(a: Action, x: Option<&Xhci>) {
    match a {
        Action::None => {}
        Action::Complete { irp, status, transferred } => complete(irp, status, transferred),
        Action::Hub { po } => crate::sched::complete_pending_op(po, 0, 0),
        Action::Wake => {
            DUE.store(true, Ordering::Release);
            if let Some(x) = x {
                crate::sched::signal_interrupt(x.hub_wake.as_ptr());
            }
        }
    }
}

// --- The backend --------------------------------------------------------------------------------

/// [`BlockBackend::submit`] for a USB disk: [`Disks::submit`], on the boot's.
fn submit(irp: *mut Irp, ctx: *mut ()) {
    let a = DISKS.submit(irp, ctx, crate::arch::Timer::read_ns());
    act(a, None);
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

// --- Binding, in the hub thread ----------------------------------------------------------------

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
    // **The device's memory**: freed with it, and only after its slot is disabled.
    for b in [rin, rout, wrap, data] {
        if mem.class.try_push(b).is_err() {
            crate::kprintln!("usb: port {port}: no memory for its storage endpoints; not bound");
            return None;
        }
    }
    Some(p)
}

/// **Bind the bulk-only interface `p` of the device in `slot`**, its configuration set: put it in the
/// table, then for each logical unit that is a ready direct-access device of under 2 TiB, read its
/// partition table and publish it — a `Disk` under the `UsbDevice` record `parent`, named by its
/// INQUIRY strings and `serial`, and its partitions under it.
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
        queue: Queue::new(),
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

/// What a unit is, from the binding's commands.
struct Unit {
    inquiry: scsi::Inquiry,
    blocks: u64,
    block_len: u32,
}

/// **A binding in progress**: the hub thread's, on device slot `i`, with the device's memory for its
/// default endpoint's requests.
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
    /// `None` for one left alone — another kind, no medium, or 2 TiB or more — which is said. `Err`
    /// when the device stopped answering, which ends the binding.
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
                    crate::kprintln!("usb: port {port}: LUN {lun}: 2 TiB or more, which READ(10) cannot address; left alone");
                    return Ok(None);
                }
            },
            _ => return Err("READ CAPACITY went wrong"),
        };
        Ok(Some(Unit { inquiry, blocks, block_len }))
    }

    /// `REQUEST SENSE`'s key, code and qualifier.
    fn sense(&mut self, lun: u8) -> Result<(u8, u8, u8), &'static str> {
        let mut buf = [0u8; scsi::SENSE_LEN];
        match self.run(lun, &scsi::request_sense(), &mut buf) {
            Ok(Status::Passed { .. }) => Ok(scsi::sense_answer(&buf)),
            _ => Err("REQUEST SENSE went wrong"),
        }
    }

    /// One of the hub thread's commands on this device, its data read into `out`: [`run`].
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
        {
            let mut d = DISKS.devs[self.i].lock();
            if let Some(dev) = d.as_mut() {
                dev.luns[lun as usize] = Lun { blocks: u.blocks, block_len: u.block_len };
            }
        }
        // **The table first**, before the disk can be reached: nothing else waits on it yet.
        let table = if u.block_len as usize == crate::drivers::partitions::BLOCK {
            Some(crate::drivers::partitions::read(u.blocks, &mut |lba, count, out: &mut [u8]| {
                let len = count as usize * crate::drivers::partitions::BLOCK;
                if lba > u32::MAX as u64 || len > BIND_DATA || out.len() < len {
                    return false;
                }
                let cdb = scsi::rw10(false, lba as u32, count as u16);
                matches!(self.run(lun, &cdb, &mut out[..len]), Ok(Status::Passed { residue: 0 }))
            }))
        } else {
            crate::kprintln!("usb: port {port}: LUN {lun}: {}-byte blocks; its table is not read", u.block_len);
            None
        };
        let backend = BlockBackend { submit, poll, ctx: context(self.i, lun, self.epoch), max_frags: MAX_FRAGS };
        let geometry = BlockGeometry { logical_block_size: u.block_len, block_count: u.blocks };
        let Ok(node) = DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, &name[..name_len], backend)
        else {
            crate::kprintln!("usb: port {port}: no memory for its disk's node");
            return false;
        };
        let node = crate::drivers::adopt(node, KObjectType::DeviceNode);
        let Some(id) = crate::device::register_block_under(node.clone(), parent, "usb-storage") else {
            return false;
        };
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
        Ok(HubResult::Fault(Fault::Transfer { dci, code: code::STALL })) if len > 0 && dci == dev_field(i, |d| d.in_dci) => {
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

/// A field of device `i`, or zero if it has gone.
fn dev_field<T: Default>(i: usize, f: impl FnOnce(&Dev) -> T) -> T {
    DISKS.devs[i].lock().as_ref().map(f).unwrap_or_default()
}

/// **Put one hub command on device `i`'s rings and wait for it**, within [`BIND_NS`]: its data, `len`
/// bytes, into the binding buffer. `status_only` reads a status wrapper alone, after a data stage's
/// stall was cleared, for the command last sent.
fn run_once(i: usize, lun: u8, cdb: &[u8], len: u32, status_only: bool) -> Result<HubResult, Failed> {
    let po = hub::new_operation()?;
    let now = crate::arch::Timer::read_ns();
    {
        let mut d = DISKS.devs[i].lock();
        let Some(dev) = d.as_mut() else { return Err(Failed::Device("its device has gone")) };
        dev.hub_result = None;
        let owner = Owner::Hub { po: po.as_ptr() };
        if status_only {
            let tag = dev.tag;
            dev.inflight = Some(Command { owner, tag, stage: Stage::Data { dci: 0 }, deadline: now + BIND_NS });
            dev.ask_status();
        } else {
            let frags = [PhysFrag { base: dev.data_phys, len: len as u64 }];
            let frags = if len == 0 { &frags[..0] } else { &frags[..] };
            dev.issue(owner, lun, cdb, len, len > 0, frags, now);
        }
    }
    let at = crate::arch::Timer::read_ns();
    let signalled =
        matches!(crate::sched::wait_on(&[po.as_ptr() as usize], at + BIND_NS, at), crate::sched::WaitResult::Signaled(_));
    let mut d = DISKS.devs[i].lock();
    let Some(dev) = d.as_mut() else { return Err(Failed::Device("its device has gone")) };
    if !signalled && matches!(dev.inflight, Some(Command { owner: Owner::Hub { .. }, .. })) {
        // Taken back before the DPC can complete an operation this thread is about to drop. If the
        // DPC took it first, its result is here, and its completion of `po` already happened.
        dev.inflight = None;
        return Err(Failed::Timeout);
    }
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

/// Give device `i`'s slot back: nothing of it was published, so nothing was submitted to it.
fn retire(i: usize) {
    let _ = DISKS.depart(i);
}

// --- Recovery, deadlines and departure, in the hub thread ----------------------------------------

/// **The earliest deadline of a command in flight**, for the hub thread to sleep until; `None` when
/// nothing is in flight.
pub(super) fn next_deadline() -> Option<u64> {
    DISKS
        .devs
        .iter()
        .filter_map(|s| s.lock().as_ref().and_then(|d| d.inflight))
        .filter(|c| matches!(c.owner, Owner::Irp { .. }))
        .map(|c| c.deadline)
        .min()
}

/// **A device the hub thread must look at**: its xHCI slot, by table slot — one with a fault the DPC
/// recorded, or a command past its deadline at `now`. The flag the DPC raises is taken here.
pub(super) fn due(now: u64) -> [Option<u8>; MAX_DEVICES] {
    DUE.store(false, Ordering::Release);
    core::array::from_fn(|i| {
        let mut d = DISKS.devs[i].lock();
        let dev = d.as_mut()?;
        if dev.fault.is_none()
            && let Some(c) = dev.inflight
            && matches!(c.owner, Owner::Irp { .. })
            && now > c.deadline
        {
            dev.fault = Some(Fault::Deadline);
        }
        dev.fault.is_some().then_some(dev.slot)
    })
}

/// What [`recover`] did.
pub(super) enum Recovered {
    /// The command was finished or failed, and the device goes on.
    Done,
    /// The device did not recover: it is to be ended, as on an unplug.
    Ended,
}

/// **Recover device `i`**, whose command went wrong (bulk-only §6.7): a failed command's sense
/// read — an illegal request to flush is no cache to flush, and a unit attention sends the command
/// again once — a data stage's stall cleared and its status read, and reset recovery for anything
/// else, a deadline included. The command is completed, and the next started. From the hub thread,
/// with the device's memory.
pub(super) fn recover(x: &Xhci, i: usize, mem: &mut DeviceMem) -> Recovered {
    let taken = {
        let mut d = DISKS.devs[i].lock();
        let Some(dev) = d.as_mut() else { return Recovered::Done };
        match (dev.fault.take(), dev.inflight.take()) {
            (Some(f), Some(Command { owner: Owner::Irp { cmd, retried }, .. })) => {
                dev.recovering = true;
                Some((f, cmd, retried))
            }
            _ => None,
        }
    };
    let Some((fault, cmd, retried)) = taken else {
        return Recovered::Done;
    };
    let mut sense = [0u8; scsi::SENSE_LEN];
    let outcome = match fault {
        Fault::Status(Status::Failed) => match run(x, i, cmd.lun, &scsi::request_sense(), &mut sense, mem) {
            Ok(Status::Passed { .. }) => {
                let (key, _, _) = scsi::sense_answer(&sense);
                if key == scsi::ILLEGAL_REQUEST && cmd.flush {
                    Ok(Some(0))
                } else if key == scsi::UNIT_ATTENTION && !retried {
                    Ok(None)
                } else {
                    Ok(Some(KError::IoError as i32))
                }
            }
            _ => Err(()),
        },
        Fault::Transfer { dci, code: code::STALL } if dci == dev_field(i, |d| if cmd.input { d.in_dci } else { d.out_dci }) => {
            match clear_halt(x, i, dci, mem).and_then(|_| run_once(i, cmd.lun, &[], 0, true)) {
                Ok(_) => Ok(Some(KError::IoError as i32)),
                Err(_) => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
            }
        }
        _ => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
    };
    let now = crate::arch::Timer::read_ns();
    if let Some(dev) = DISKS.devs[i].lock().as_mut() {
        dev.recovering = false;
    }
    match outcome {
        Ok(Some(status)) => {
            let transferred = if status == 0 { cmd.len as u64 } else { 0 };
            complete(cmd.irp, status, transferred);
            start_next(i, now);
            Recovered::Done
        }
        // A unit attention: the command again, once.
        Ok(None) => {
            let mut d = DISKS.devs[i].lock();
            if let Some(dev) = d.as_mut() {
                dev.start(cmd, true, now);
                return Recovered::Done;
            }
            drop(d);
            complete(cmd.irp, KError::PeerClosed as i32, 0);
            Recovered::Done
        }
        Err(()) => {
            complete(cmd.irp, KError::IoError as i32, 0);
            Recovered::Ended
        }
    }
}

/// Start device `i`'s next queued command, if nothing is in flight.
fn start_next(i: usize, now: u64) {
    let mut d = DISKS.devs[i].lock();
    if let Some(dev) = d.as_mut()
        && dev.inflight.is_none()
        && dev.fault.is_none()
        && let Some(next) = dev.queue.pop()
    {
        dev.start(next, false, now);
    }
}

/// What a departure leaves for after the slot is disabled: the command that was in flight.
pub(super) struct Departed(Option<*mut Irp>);

/// **The device in xHCI slot `slot` has gone** (Phase 6 Part D.3): out of the table, so the DPC
/// touches none of its memory; its queue completed `PeerClosed` now, and any later submit refused so
/// by its slot's epoch. The command in flight is handed back, for [`finish`] once the slot is
/// disabled: until then the controller may still move its data. From the hub thread.
pub(super) fn depart(slot: u8) -> Departed {
    let Some(i) = DISKS.slot_of(slot) else {
        return Departed(None);
    };
    let (queued, inflight) = DISKS.depart(i);
    for irp in queued.into_iter().flatten() {
        complete(irp, KError::PeerClosed as i32, 0);
    }
    Departed(inflight)
}

/// **Complete what a departure handed back**, `PeerClosed`, now that the slot is disabled.
pub(super) fn finish(d: Departed) {
    if let Some(irp) = d.0 {
        complete(irp, KError::PeerClosed as i32, 0);
    }
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::irp::IrpBuffer;
    use crate::mm::test_support::init_global_heap;

    /// **A command block wrapper** where bulk-only puts each field: the signature, the tag, the
    /// length, IN in the flags' top bit, the unit, the CDB's length and the CDB.
    #[test]
    fn a_command_wrapper_is_laid_out_as_bulk_only_says() {
        let w = cbw(0x1234_5678, 4096, true, 3, &scsi::rw10(false, 0x0102_0304, 8));
        assert_eq!(&w[0..4], b"USBC");
        assert_eq!(le32(&w, 4), 0x1234_5678);
        assert_eq!(le32(&w, 8), 4096);
        assert_eq!((w[12], w[13], w[14]), (0x80, 3, 10));
        assert_eq!(&w[15..25], &[0x28, 0, 1, 2, 3, 4, 0, 0, 8, 0]);
        assert!(w[25..].iter().all(|&b| b == 0));
        assert_eq!(cbw(1, 0, false, 0, &scsi::test_unit_ready())[12], 0, "OUT, or no data");
    }

    fn status(sig: &[u8; 4], tag: u32, residue: u32, status: u8) -> [u8; CSW_LEN] {
        let mut b = [0u8; CSW_LEN];
        b[0..4].copy_from_slice(sig);
        b[4..8].copy_from_slice(&tag.to_le_bytes());
        b[8..12].copy_from_slice(&residue.to_le_bytes());
        b[12] = status;
        b
    }

    /// **A status wrapper reads as its status only when it is this command's**: a wrong signature
    /// or tag is no wrapper, nor a status past 2; and a residue is carried.
    #[test]
    fn a_status_wrapper_is_this_commands_or_invalid() {
        assert_eq!(csw(&status(b"USBS", 7, 0, 0), 7), Status::Passed { residue: 0 });
        assert_eq!(csw(&status(b"USBS", 7, 512, 0), 7), Status::Passed { residue: 512 });
        assert_eq!(csw(&status(b"USBS", 7, 0, 1), 7), Status::Failed);
        assert_eq!(csw(&status(b"USBS", 7, 0, 2), 7), Status::Phase);
        assert_eq!(csw(&status(b"USBS", 7, 0, 3), 7), Status::Invalid, "no status 3");
        assert_eq!(csw(&status(b"USBC", 7, 0, 0), 7), Status::Invalid, "a command's signature");
        assert_eq!(csw(&status(b"USBS", 8, 0, 0), 7), Status::Invalid, "another command's tag");
        assert_eq!(csw(&status(b"USBS", 7, 0, 0)[..12], 7), Status::Invalid, "short");
    }

    /// **Each CDB's bytes**, as SPC-4 and SBC-3 give them, the LBA and count big-endian.
    #[test]
    fn each_cdb_is_as_scsi_gives_it() {
        assert_eq!(scsi::test_unit_ready(), [0; 6]);
        assert_eq!(scsi::request_sense(), [0x03, 0, 0, 0, 18, 0]);
        assert_eq!(scsi::inquiry(), [0x12, 0, 0, 0, 36, 0]);
        assert_eq!(scsi::read_capacity()[0], 0x25);
        assert_eq!(scsi::rw10(true, 0xAABB_CCDD, 0x0102), [0x2A, 0, 0xAA, 0xBB, 0xCC, 0xDD, 0, 0x01, 0x02, 0]);
        assert_eq!(scsi::synchronize_cache(), [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// **What the answers say**: a direct-access device and a CD-ROM; a capacity, and one of 2 TiB,
    /// which the ten-byte commands cannot address; a sense key and its code.
    #[test]
    fn the_answers_read_as_scsi_gives_them() {
        let mut inq = [0u8; 36];
        inq[8..16].copy_from_slice(b"QEMU    ");
        inq[16..32].copy_from_slice(b"QEMU HARDDISK   ");
        let a = scsi::inquiry_answer(&inq).unwrap();
        assert!(a.direct_access);
        assert_eq!(&a.vendor, b"QEMU    ");
        inq[0] = 0x05;
        assert!(!scsi::inquiry_answer(&inq).unwrap().direct_access, "a CD-ROM");
        inq[0] = 0x20;
        assert!(!scsi::inquiry_answer(&inq).unwrap().direct_access, "qualifier 1: not connected");
        assert_eq!(scsi::inquiry_answer(&inq[..35]), None);

        assert_eq!(scsi::capacity_answer(&[0, 0, 0x7F, 0xFF, 0, 0, 2, 0]), Some((0x8000, 512)));
        assert_eq!(scsi::capacity_answer(&[0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 2, 0]), Some((0xFFFF_FFFF, 512)));
        assert_eq!(scsi::capacity_answer(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 2, 0]), None, "2 TiB or more");

        let mut sense = [0u8; 18];
        sense[2] = 0x06;
        sense[12] = 0x28;
        assert_eq!(scsi::sense_answer(&sense), (scsi::UNIT_ATTENTION, 0x28, 0));
    }

    /// **A block IRP's command fits its unit or is refused**: aligned to its blocks, within it, and
    /// not empty; a flush whatever its range; nothing on a unit not published.
    #[test]
    fn an_irp_fits_its_unit_or_is_refused() {
        let unit = Lun { blocks: 1024, block_len: 512 };
        let r = IrpOp::Read as u32;
        let (cdb, len, input, flush) = irp_command(r, 4096, 8192, unit).unwrap();
        assert_eq!((cdb, len, input, flush), (scsi::rw10(false, 8, 16), 8192, true, false));
        assert!(!irp_command(IrpOp::Write as u32, 0, 512, unit).unwrap().2, "a write moves out");
        assert_eq!(irp_command(r, 100, 512, unit), Err(KError::InvalidArgument), "unaligned");
        assert_eq!(irp_command(r, 0, 0, unit), Err(KError::InvalidArgument), "empty");
        assert!(irp_command(r, 1023 * 512, 512, unit).is_ok(), "the last block");
        assert_eq!(irp_command(r, 1023 * 512, 1024, unit), Err(KError::InvalidArgument), "past the end");
        assert!(irp_command(IrpOp::Flush as u32, 0, 0, unit).unwrap().3);
        assert_eq!(irp_command(IrpOp::Flush as u32, 0, 0, Lun::default()), Err(KError::InvalidArgument));
    }

    /// A TD's packets still to come, clamped to 31.
    #[test]
    fn a_td_size_counts_the_packets_left() {
        assert_eq!(packets(0, 512), 0);
        assert_eq!(packets(4096, 512), 8);
        assert_eq!(packets(4097, 512), 9);
        assert_eq!(packets(1 << 20, 512), 31);
    }

    /// **A USB disk is named as a SATA one**: its model, padding trimmed, and its serial.
    #[test]
    fn a_usb_disk_is_named_by_its_model_and_serial() {
        let inq = scsi::Inquiry { direct_access: true, vendor: *b"QEMU    ", product: *b"QEMU HARDDISK   " };
        let mut out = [0u8; MAX_DEVICE_NAME];
        let n = disk_name(&inq, b"1-0000:00:04.0-1", &mut out);
        assert_eq!(&out[..n], b"QEMU QEMU HARDDISK (1-0000:00:04.0-1)");
        let n = disk_name(&inq, b"", &mut out);
        assert_eq!(&out[..n], b"QEMU QEMU HARDDISK");
    }

    // --- The fast path, driven ----------------------------------------------------------------

    /// Heap memory standing in for a device's rings, wrapper page, binding buffer and the doorbell
    /// array: its "physical" addresses are its virtual ones, which only appear inside TRBs.
    struct Fake {
        rings: [crate::libkern::KVec<u64>; 2],
        wrap: crate::libkern::KVec<u64>,
        data: crate::libkern::KVec<u64>,
        db: crate::libkern::KVec<u32>,
    }

    fn zeroed<T: Copy + Default>(n: usize) -> crate::libkern::KVec<T> {
        let mut v = crate::libkern::KVec::new();
        v.try_reserve(n).unwrap();
        while v.len() < n {
            v.try_push(T::default()).unwrap();
        }
        v
    }

    impl Fake {
        fn new() -> Fake {
            Fake { rings: [zeroed(RING_TRBS * 2), zeroed(RING_TRBS * 2)], wrap: zeroed(512), data: zeroed(BIND_DATA / 8), db: zeroed(32) }
        }

        fn dev(&mut self, slot: u8) -> Dev {
            let ring = |v: &mut crate::libkern::KVec<u64>| {
                let at = v.as_mut_ptr() as u64;
                Ring { producer: Producer::new(&mut RawSlots(at), at), virt: at, phys: at }
            };
            let (r0, r1) = self.rings.split_at_mut(1);
            let wrap = self.wrap.as_mut_ptr() as u64;
            let data = self.data.as_mut_ptr() as u64;
            Dev {
                slot,
                interface: 0,
                in_dci: 3,
                out_dci: 4,
                in_address: 0x81,
                out_address: 0x02,
                in_mps: 512,
                out_mps: 512,
                bulk_in: ring(&mut r0[0]),
                bulk_out: ring(&mut r1[0]),
                wrap_virt: wrap,
                wrap_phys: wrap,
                data_virt: data,
                data_phys: data,
                db: self.db.as_mut_ptr() as u64,
                tag: 0,
                luns: { let mut l = [Lun::default(); MAX_LUNS]; l[0] = Lun { blocks: 2048, block_len: 512 }; l },
                inflight: None,
                fault: None,
                hub_result: None,
                recovering: false,
                queue: Queue::new(),
            }
        }

        /// The device answers its last command: a status wrapper for `tag` with `status`.
        fn answer(&mut self, tag: u32, status_byte: u8) {
            let b = status(b"USBS", tag, 0, status_byte);
            let w = self.wrap.as_mut_ptr() as *mut u8;
            for (k, v) in b.iter().enumerate() {
                // SAFETY: the fake wrapper page is 4 KiB.
                unsafe { w.add(CSW_AT as usize + k).write(*v) };
            }
        }

        /// TRB `i` of ring `r` (0 IN, 1 OUT).
        fn trb(&self, r: usize, i: usize) -> Trb {
            RawSlots(self.rings[r].as_ptr() as u64).read(i)
        }
    }

    fn irp(op: IrpOp, offset: u64, length: u64, frags: &[PhysFrag]) -> Irp {
        let buffer = if frags.is_empty() {
            IrpBuffer::NONE
        } else {
            IrpBuffer { kind: crate::io::irp::IRP_BUF_FRAGS, count: frags.len() as u32, frags: frags.as_ptr() as u64 }
        };
        Irp::new_block(op, core::ptr::null(), offset, length, buffer, core::ptr::null_mut(), 0)
    }

    /// The status wrapper's TRB a command waits on, once asked for.
    fn csw_trb(d: &Disks, i: usize) -> u64 {
        match d.devs[i].lock().as_ref().unwrap().inflight.unwrap().stage {
            Stage::Status { csw_trb } => csw_trb,
            s => panic!("the status is not asked for yet: {s:?}"),
        }
    }

    /// **A read's status is asked for when its data stage ends, and it completes at its status
    /// wrapper**: the command wrapper on OUT, a TRB per fragment on IN, the last interrupting, both
    /// doorbells rung; then, at the data stage's event — short, here — the status wrapper on IN; and
    /// the next IRP, queued meanwhile, starts when the first completes.
    #[test]
    fn a_reads_status_is_asked_for_when_its_data_ends_and_the_next_starts_at_its_status() {
        init_global_heap();
        let disks = Disks::new();
        let mut fake = Fake::new();
        let (i, epoch) = disks.take(fake.dev(9)).unwrap();
        let ctx = context(i, 0, epoch);
        let frags = [PhysFrag { base: 0x10_000, len: 4096 }, PhysFrag { base: 0x20_000, len: 4096 }];
        let mut first = irp(IrpOp::Read, 8 * 512, 8192, &frags);
        assert_eq!(disks.submit(&mut first, ctx, 0), Action::None);
        let cbw_trb = fake.trb(1, 0);
        assert_eq!(cbw_trb.0[2] & 0x1_FFFF, CBW_LEN as u32, "the command wrapper on OUT");
        assert_eq!((fake.trb(0, 0).0[0], fake.trb(0, 1).0[0]), (0x10_000, 0x20_000), "the data on IN");
        assert_ne!(fake.trb(0, 0).0[3] & (1 << 4), 0, "chained to the next fragment");
        assert_eq!(fake.trb(0, 1).0[3] & (1 << 5), 1 << 5, "the last interrupts");
        assert_eq!(fake.trb(0, 2), Trb::default(), "and nothing behind it yet");
        assert_eq!(fake.db[9], 3, "the last doorbell rung was IN's");
        let w = fake.wrap.as_ptr() as *const u8;
        // SAFETY: the fake wrapper page.
        let wrapper: [u8; CBW_LEN] = core::array::from_fn(|k| unsafe { w.add(k).read() });
        assert_eq!(&wrapper[15..25], &scsi::rw10(false, 8, 16));

        let mut second = irp(IrpOp::Write, 0, 512, &frags[..1]);
        assert_eq!(disks.submit(&mut second, ctx, 0), Action::None, "queued");
        assert_eq!(disks.on_transfer(9, 4, code::SUCCESS, 0x99, 1), Some(Action::None), "OUT's event is not the data's");
        assert_eq!(fake.trb(0, 2), Trb::default(), "so no status yet");
        assert_eq!(disks.on_transfer(9, 3, code::SHORT_PACKET, 0x10_000, 1), Some(Action::None), "the data stage ends short");
        assert_eq!(fake.trb(0, 2).0[2] & 0x1_FFFF, CSW_LEN as u32, "and the status is asked for");
        fake.answer(1, 0);
        let at = csw_trb(&disks, i);
        assert_eq!(
            disks.on_transfer(9, 3, code::SUCCESS, at, 1),
            Some(Action::Complete { irp: &mut first, status: 0, transferred: 8192 })
        );
        assert_eq!(fake.trb(1, 1).0[2] & 0x1_FFFF, CBW_LEN as u32, "the next command's wrapper");
        assert_eq!(fake.trb(1, 2).0[0], 0x10_000, "and its data, on OUT for a write");
        assert_eq!(disks.on_transfer(8, 3, code::SUCCESS, at, 1), None, "another slot's event is not this driver's");
    }

    /// **A command that goes wrong is the hub thread's**: a failed status or a stall leaves it in
    /// flight with its fault, wakes the thread, and a submit meanwhile waits.
    #[test]
    fn a_command_that_goes_wrong_is_left_for_the_hub_thread() {
        init_global_heap();
        let disks = Disks::new();
        let mut fake = Fake::new();
        let (i, epoch) = disks.take(fake.dev(2)).unwrap();
        let ctx = context(i, 0, epoch);
        let mut a = irp(IrpOp::Flush, 0, 0, &[]);
        disks.submit(&mut a, ctx, 0);
        assert_eq!(fake.trb(1, 0).0[3] & (1 << 5), 1 << 5, "with no data, the command wrapper interrupts");
        assert_eq!(disks.on_transfer(2, 4, code::SUCCESS, 0, 1), Some(Action::None), "and its event asks for the status");
        fake.answer(1, 1);
        assert_eq!(disks.on_transfer(2, 3, code::SUCCESS, csw_trb(&disks, i), 1), Some(Action::Wake));
        assert_eq!(disks.devs[i].lock().as_ref().unwrap().fault, Some(Fault::Status(Status::Failed)));
        let mut b = irp(IrpOp::Flush, 0, 0, &[]);
        assert_eq!(disks.submit(&mut b, ctx, 0), Action::None);
        assert_eq!(disks.devs[i].lock().as_ref().unwrap().queue.len, 1, "waits behind the recovery");

        // While the hub thread recovers — the fault taken, nothing in flight — a submit still waits.
        {
            let mut d = disks.devs[i].lock();
            let dev = d.as_mut().unwrap();
            dev.fault = None;
            dev.inflight = None;
            dev.recovering = true;
        }
        let mut during = irp(IrpOp::Flush, 0, 0, &[]);
        assert_eq!(disks.submit(&mut during, ctx, 0), Action::None);
        assert_eq!(disks.devs[i].lock().as_ref().unwrap().queue.len, 2, "queued, not started under the recovery");
        assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none());

        let (j, e2) = disks.take(fake.dev(3)).unwrap();
        let mut c = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
        disks.submit(&mut c, context(j, 0, e2), 0);
        assert_eq!(disks.on_transfer(3, 4, code::STALL, 0, 1), Some(Action::Wake), "a stall on the command wrapper");
        assert_eq!(disks.devs[j].lock().as_ref().unwrap().fault, Some(Fault::Transfer { dci: 4, code: code::STALL }));
    }

    /// **A slot taken, departed and taken again serves its old node nothing of the new device**
    /// (PR #361's lesson): the departure hands back the queue and the command in flight, a submit
    /// through the old context is refused `PeerClosed`, and the new device's context is served.
    #[test]
    fn a_departed_devices_node_is_refused_and_its_slot_reused_under_a_new_epoch() {
        init_global_heap();
        let disks = Disks::new();
        let mut fake = Fake::new();
        let mut taken = [(0, 0); MAX_DEVICES];
        for (k, t) in taken.iter_mut().enumerate() {
            *t = disks.take(fake.dev(10 + k as u8)).unwrap();
        }
        assert!(disks.take(fake.dev(99)).is_none(), "every slot bound");
        let (i, old_epoch) = taken[4];
        let old = context(i, 0, old_epoch);
        let mut inflight = irp(IrpOp::Flush, 0, 0, &[]);
        let mut queued = irp(IrpOp::Flush, 0, 0, &[]);
        disks.submit(&mut inflight, old, 0);
        disks.submit(&mut queued, old, 0);
        let (q, f) = disks.depart(disks.slot_of(14).unwrap());
        assert_eq!(q[0], Some(&mut queued as *mut Irp), "the queue handed back");
        assert_eq!(f, Some(&mut inflight as *mut Irp), "and the command in flight");
        let mut late = irp(IrpOp::Flush, 0, 0, &[]);
        assert_eq!(disks.submit(&mut late, old, 0), Action::Complete { irp: &mut late, status: KError::PeerClosed as i32, transferred: 0 });

        let (again, new_epoch) = disks.take(fake.dev(50)).unwrap();
        assert_eq!(again, i, "the departed device's slot");
        assert_ne!(new_epoch, old_epoch, "under a new epoch");
        let mut stale = irp(IrpOp::Flush, 0, 0, &[]);
        assert_eq!(
            disks.submit(&mut stale, old, 0),
            Action::Complete { irp: &mut stale, status: KError::PeerClosed as i32, transferred: 0 },
            "the old node reaches nothing of the new device"
        );
        let mut fresh = irp(IrpOp::Flush, 0, 0, &[]);
        assert_eq!(disks.submit(&mut fresh, context(i, 0, new_epoch), 0), Action::None, "the new node is served");
        assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_some());
    }
}
