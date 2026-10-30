//! A RAM-backed block device: the I/O spine's bring-up disk, and a Limine module's.
//!
//! The ramdisk was the first [`BlockBackend`] producer, proving
//! initiate → DPC → `PendingOperation` → `sys_wait` without hardware; the
//! bring-up disk is a 64 KiB pattern-filled [`KVec`] the I/O self-test reads
//! back. It is also how the live image gets a root: every Limine module after
//! the initramfs is published as a block device over the module's own memory,
//! before `drivers::probe`'s GPT pass, so `init.toml` can mount its partitions
//! like an AHCI disk's.
//!
//! ## Completing where a device completes
//!
//! `submit` does the whole transfer (a `memcpy`) and queues the IRP's completion
//! DPC. DPCs drain only at an interrupt tail and a RAM disk raises no interrupt,
//! so a completion would wait for the next 10 ms timer tick and every block I/O
//! would serialise on it (the fs-server "I/O hang" again: 0.10 s against 4.85 s
//! from mount to greeter). **So the disk raises its own completion interrupt**
//! ([`install_software`](crate::arch::irq_install::ArchIrqInstall::install_software)
//! once, [`raise_on_self`](crate::arch::irq::ArchIrq::raise_on_self) after each
//! enqueue), and the completion runs where AHCI's does: `device_irq_dispatch`
//! drains the DPC in a fresh interrupt lock scope and reschedules an idle CPU.
//! Completing inline in `submit` would run the completion (which takes `SCHED`)
//! in whatever lock context the submitter holds.
//!
//! ## Concurrency
//!
//! A root disk takes submits from the fs-server and page-cache fills on any CPU.
//! Each transfer runs under the disk's lock, so a read racing a write of the same
//! block sees one or the other, never a torn block.
//!
//! **Do not `kprintln!` inside `transfer`**: the lock is `Leaf` and the serial
//! port ranks above it, so the rank tracker panics with `acquiring Serial (rank
//! 70) while holding Leaf (rank 90)` before the line prints.
//!
//! **The completion vector is one of eight device vectors** (`DEVICE_IRQ_COUNT`),
//! shared by every ramdisk. A release boot uses five (AHCI, COM1, the i8042's
//! two, this); `register_device_handler` asserts on exhaustion, which a machine
//! with more controllers could reach.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::arch::irq::ArchIrq;
use crate::arch::irq_install::ArchIrqInstall;
use crate::arch::timer::ArchTimer;
use crate::io::block::BlockBackend;
use crate::io::irp::{Irp, IrpOp, IrpStatus, PhysFrag};
use crate::io::stats::Counted;
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME, NameBuf};
use crate::libkern::handle::KObjectType;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, IrqSpinLock, KBox, KVec};
use crate::object::ObjectRef;
use crate::object::device_node::{
    BarWindow, BlockGeometry, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::syscall::error::KError;

/// Backing size of the bring-up ramdisk (64 KiB).
pub const RAMDISK_BYTES: usize = 64 * 1024;
/// Logical block size a ramdisk reports.
pub const RAMDISK_BLOCK: u32 = 512;

/// Where a ramdisk's bytes live.
enum Backing {
    /// The device's own, the bring-up disk, reached through `base`, taken from it mutably once.
    Owned { bytes: KVec<u8>, base: *mut u8 },
    /// Memory the bootloader loaded: a Limine module, never reclaimed.
    Borrowed { base: *mut u8, len: usize },
}

/// A RAM-backed block device. It lives for the kernel's lifetime, like real hardware.
pub struct RamDisk {
    backing: Backing,
    block_size: u32,
    /// Held across each transfer. See the module docs § Concurrency.
    busy: IrqSpinLock<()>,
    /// The disk node's counters, attached once it is registered.
    counted: Counted,
}

// SAFETY: the backing is either owned by this device or bootloader memory handed to it
// exclusively for the kernel's lifetime (`over_memory`'s contract), and every access to its bytes
// goes through `transfer`, under `busy`.
unsafe impl Send for RamDisk {}
// SAFETY: as above.
unsafe impl Sync for RamDisk {}

impl RamDisk {
    /// The deterministic backing byte at offset `i` — the pattern a read should
    /// return, so a test can predict it.
    pub fn pattern_byte(i: usize) -> u8 {
        (i as u8).wrapping_mul(31).wrapping_add(7)
    }

    /// Allocate a ramdisk with its backing filled with [`pattern_byte`], built by
    /// appending into a [`KVec`] (a large stack temporary would overflow the
    /// kernel stack).
    ///
    /// [`pattern_byte`]: RamDisk::pattern_byte
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut backing: KVec<u8> = KVec::new();
        backing.try_reserve(RAMDISK_BYTES)?;
        for i in 0..RAMDISK_BYTES {
            backing.try_push(Self::pattern_byte(i)).expect("within reserved ramdisk capacity");
        }
        let base = backing.as_mut_ptr();
        KBox::try_new(RamDisk {
            backing: Backing::Owned { bytes: backing, base },
            block_size: RAMDISK_BLOCK,
            busy: IrqSpinLock::new(LockRank::Leaf, ()),
            counted: Counted::new(),
        })
    }

    /// A ramdisk over `len` bytes the caller already has — a Limine module. Nothing is copied:
    /// reads and writes go straight to that memory.
    ///
    /// # Safety
    ///
    /// `base..base + len` must be readable and writable for the kernel's lifetime and used by
    /// nothing else from now on.
    pub unsafe fn over_memory(base: *mut u8, len: usize) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(RamDisk {
            backing: Backing::Borrowed { base, len },
            block_size: RAMDISK_BLOCK,
            busy: IrqSpinLock::new(LockRank::Leaf, ()),
            counted: Counted::new(),
        })
    }

    fn base(&self) -> *mut u8 {
        match &self.backing {
            Backing::Owned { base, .. } | Backing::Borrowed { base, .. } => *base,
        }
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        match &self.backing {
            Backing::Owned { bytes, .. } => bytes.len(),
            Backing::Borrowed { len, .. } => *len,
        }
    }

    /// Logical block size.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// **Serve one IRP, counted**: the transfer, timed by `clock` in nanoseconds
    /// (a RAM disk's command is a copy, so its time on the device is the copy's)
    /// and recorded into the node's counters. Returns `(status, transferred)`, as
    /// [`transfer`](Self::transfer).
    fn serve(&self, irp: &Irp, clock: impl Fn() -> u64) -> (i32, u64) {
        let started = clock();
        let (status, transferred) = self.transfer(irp);
        self.counted.record(irp.op, status == IrpStatus::Success as i32, transferred, clock() - started);
        (status, transferred)
    }

    /// The disk's hold on its node's counters, for whoever registers the node to attach.
    pub fn counted(&self) -> &Counted {
        &self.counted
    }

    /// Perform the IRP's transfer against the backing, returning `(status, transferred)`. A
    /// `memcpy` either direction across the buffer's physical fragments (reached through the
    /// HHDM), under the disk's lock. No blocking, no allocation.
    fn transfer(&self, irp: &Irp) -> (i32, u64) {
        // **Every op by name**: inferring a write from "not a read" is how a flush, which
        // carries no range, would be taken for one.
        let is_read = match irp.op {
            op if op == IrpOp::Read as u32 => true,
            op if op == IrpOp::Write as u32 => false,
            // A RAM disk's memory is its medium: nothing is cached, so a flush is done.
            op if op == IrpOp::Flush as u32 => return (IrpStatus::Success as i32, 0),
            // A RAM disk's table is the bootloader's module, read once, and nothing ejects it.
            op if op == IrpOp::Rescan as u32 || op == IrpOp::Eject as u32 => return (KError::Unsupported as i32, 0),
            _ => return (KError::InvalidArgument as i32, 0),
        };
        let dev_off = irp.offset;
        let len = irp.length;
        if dev_off.checked_add(len).is_none_or(|end| end > self.capacity() as u64) {
            return (KError::InvalidArgument as i32, 0);
        }
        let _busy = self.busy.lock();
        let base = self.base();
        // No buffer is an empty slice, never one built from its null pointer, which
        // `from_raw_parts` forbids at any length — as in the AHCI driver.
        let frags: &[PhysFrag] = if irp.buffer.count == 0 {
            &[]
        } else {
            // SAFETY: `irp.buffer.frags` points at a `[PhysFrag; count]` owned by the
            // IRP's box for the IRP's lifetime (see `io::block`).
            unsafe { core::slice::from_raw_parts(irp.buffer.frags as *const PhysFrag, irp.buffer.count as usize) }
        };
        // **The fragments must hold `len`**, and only `len` of them is moved: the range checked is
        // the IRP's, and a fragment's own length is no bound on the disk.
        if frags.iter().try_fold(0u64, |sum, f| sum.checked_add(f.len)).is_none_or(|sum| sum < len) {
            return (KError::InvalidArgument as i32, 0);
        }
        let hhdm = crate::mm::heap::hhdm_offset();
        let (mut dev_pos, mut left) = (dev_off, len);
        for f in frags {
            let n = f.len.min(left);
            let buf_va = (f.base + hhdm) as *mut u8;
            // SAFETY: `dev_pos + n <= dev_off + len`, which was bounds-checked against the
            // capacity; `buf_va` is the HHDM alias of an owned buffer frame of at least `n` bytes;
            // the regions do not overlap.
            unsafe {
                let dev_ptr = base.add(dev_pos as usize);
                if is_read {
                    core::ptr::copy_nonoverlapping(dev_ptr, buf_va, n as usize);
                } else {
                    core::ptr::copy_nonoverlapping(buf_va as *const u8, dev_ptr, n as usize);
                }
            }
            dev_pos += n;
            left -= n;
        }
        (IrpStatus::Success as i32, len)
    }
}

/// The vector every ramdisk raises to complete an IRP, or 0 before the first device exists.
static COMPLETION_VECTOR: AtomicU8 = AtomicU8::new(0);

/// The completion interrupt's handler. **Empty on purpose**: the work is the device-interrupt
/// tail's — drain the DPC `submit` queued, then reschedule an idle CPU — and this vector exists
/// only so that tail runs.
extern "C" fn ramdisk_completion_isr() {}

/// The ramdisk's [`BlockBackend::submit`]: transfer now (a ramdisk has no DMA engine), complete the
/// IRP, and raise the completion interrupt so its DPC drains at once rather than at the next timer
/// tick.
fn ramdisk_submit(irp: *mut Irp, ctx: *mut ()) {
    // SAFETY: `ctx` is the `*const RamDisk` installed in the backend; the device
    // outlives the IRP.
    let rd = unsafe { &*(ctx as *const RamDisk) };
    // SAFETY: `irp` is the live in-flight IRP from `dispatch_block_irp`.
    let (status, transferred) = rd.serve(unsafe { &*irp }, crate::arch::Timer::read_ns);
    // SAFETY: as above, and completed here once.
    unsafe { crate::io::irp::complete(irp, status, transferred) };
    let vector = COMPLETION_VECTOR.load(Ordering::Acquire);
    if vector != 0 {
        // SAFETY: ring 0, and this CPU's local controller is up. The first submits come from
        // `drivers::probe`'s GPT pass on the BSP, sound **only because `kernel_main` runs
        // `Irq::init` before `drivers::probe`** (earlier, this MSR write `#GP`s with x2APIC
        // off); every later submit runs on a CPU `ap_cpu_init` or `Irq::init` has brought up.
        // `vector` was installed with `ramdisk_completion_isr` by `try_new_device`.
        unsafe { crate::arch::Irq::raise_on_self(vector) };
    }
}

/// The ramdisk's [`BlockBackend::poll`]: its `submit` already queued the
/// completion DPC, so a synchronous read just drains it.
fn ramdisk_poll(_ctx: *mut ()) {
    crate::dpc::run_pending();
}

/// Build a block [`DeviceNode`] for `rd`, which must outlive every IRP submitted
/// to it (a ramdisk is leaked `'static`). The first call installs the completion
/// vector; boot-time only, since two first calls racing would each take a vector.
pub fn try_new_device(rd: &'static RamDisk, name: &[u8]) -> Result<KBox<DeviceNode>, AllocError> {
    if COMPLETION_VECTOR.load(Ordering::Acquire) == 0 {
        // SAFETY: ring 0 at boot, after the interrupt table; the handler is a `'static` fn.
        let vector = unsafe { crate::arch::IrqInstall::install_software(ramdisk_completion_isr) };
        COMPLETION_VECTOR.store(vector, Ordering::Release);
    }
    let backend = BlockBackend {
        submit: ramdisk_submit,
        poll: ramdisk_poll,
        ctx: rd as *const RamDisk as *mut (),
        // **No hardware limit to inherit**: the ramdisk copies fragment by fragment
        // with nothing fixed-size to overrun, and a limit nothing enforces would be
        // a number to maintain.
        max_frags: u32::MAX,
    };
    let geometry = BlockGeometry {
        logical_block_size: rd.block_size(),
        block_count: (rd.capacity() / rd.block_size() as usize) as u64,
    };
    let descriptor = ResourceDescriptor {
        identity: DeviceIdentity { vendor: 0, device: 0, class: 0, subclass: 0, prog_if: 0, revision: 0 },
        bars: [BarWindow::ZERO; 6],
        interrupt: InterruptSpec::NONE,
        seg: 0,
        bus: 0,
        dev: 0,
        func: 0,
        _pad: [0; 3],
    };
    // **A RAM disk says so**: it is memory and disappears at power-off, so an
    // installer writing to one would install onto something that will not be there.
    DeviceNode::try_new_block(descriptor, geometry, BlockKind::RamDisk, name, backend)
}

/// Most modules after the initramfs the kernel will publish. The live image uses one.
pub const MAX_MODULE_DISKS: usize = 4;

/// Longest module path kept for the log line; a longer one is cut.
const PATH_BYTES: usize = 64;

#[derive(Clone, Copy)]
struct ModuleDisk {
    base: usize,
    len: usize,
    /// Which module this was in Limine's list (1 is the first after the initramfs).
    index: usize,
    path: [u8; PATH_BYTES],
    path_len: usize,
}

static MODULE_DISKS: IrqSpinLock<[Option<ModuleDisk>; MAX_MODULE_DISKS]> =
    IrqSpinLock::new(LockRank::Leaf, [None; MAX_MODULE_DISKS]);

/// Remember Limine module `index` (at `base`, `len` bytes, loaded from `path`) to be published as
/// a block device by [`publish_modules`]. Recorded at boot before the device table and the
/// interrupt table exist; published once they do. Returns `false` when the table is full.
///
/// # Safety
///
/// `base..base + len` must be the module's memory — never reclaimed, reachable through the HHDM,
/// and used by nothing else.
pub unsafe fn record_module(index: usize, base: *mut u8, len: usize, path: &[u8]) -> bool {
    let mut slots = MODULE_DISKS.lock();
    let Some(slot) = slots.iter_mut().find(|s| s.is_none()) else {
        return false;
    };
    let mut kept = [0u8; PATH_BYTES];
    let path_len = path.len().min(PATH_BYTES);
    kept[..path_len].copy_from_slice(&path[..path_len]);
    *slot = Some(ModuleDisk { base: base as usize, len, index, path: kept, path_len });
    true
}

/// Publish every recorded module as a block device in the device table, so the GPT pass that
/// follows scans it. Called once, from `drivers::probe`, before that pass.
pub fn publish_modules() {
    let recorded = *MODULE_DISKS.lock();
    for disk in recorded.iter().flatten() {
        let path = &disk.path[..disk.path_len];
        let path = core::str::from_utf8(path).unwrap_or("?");
        // SAFETY: `record_module`'s contract: the module's own memory, for the kernel's lifetime.
        let rd = match unsafe { RamDisk::over_memory(disk.base as *mut u8, disk.len) } {
            // SAFETY: leaked to `'static` — a device lives for the kernel's lifetime.
            Ok(rd) => unsafe { &*(KBox::into_raw(rd).as_ptr()) },
            Err(_) => {
                crate::kprintln!("ramdisk: module {} ({}): out of memory", disk.index, path);
                continue;
            }
        };
        let mut name = [0u8; MAX_DEVICE_NAME];
        let mut w = NameBuf::new(&mut name);
        let _ = core::fmt::Write::write_fmt(&mut w, format_args!("module {} ({})", disk.index, path));
        let name_len = w.len();
        let node = match try_new_device(rd, &name[..name_len]) {
            Ok(n) => n,
            Err(_) => {
                crate::kprintln!("ramdisk: module {} ({}): out of memory", disk.index, path);
                continue;
            }
        };
        // SAFETY: adopt the creation reference; the device table takes ownership below.
        let node_ref =
            unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
        crate::device::register(node_ref.clone(), "ramdisk");
        rd.counted().attach(&node_ref);
        crate::kprintln!("ramdisk: module {} ({}), {} bytes, as a block device", disk.index, path, disk.len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::irp::{IRP_BUF_FRAGS, IrpBuffer};
    use crate::mm::test_support::init_global_heap;

    /// An IRP over host memory: on the host the HHDM offset is 0, so a fragment's "physical" base
    /// is simply a host address.
    fn irp(op: IrpOp, offset: u64, frags: &[PhysFrag]) -> Irp {
        let length = frags.iter().map(|f| f.len).sum();
        let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: frags.len() as u32, frags: frags.as_ptr() as u64 };
        Irp::new_block(op, core::ptr::null(), offset, length, buffer, core::ptr::null_mut(), 0)
    }

    fn frag(buf: &mut [u8]) -> PhysFrag {
        PhysFrag { base: buf.as_mut_ptr() as u64, len: buf.len() as u64 }
    }

    #[test]
    fn a_disk_over_borrowed_memory_writes_through_to_it_and_reads_it_back() {
        init_global_heap();
        let mut module = vec![0u8; 4 * 512];
        // SAFETY: `module` outlives `rd` and nothing else touches it while `rd` is used.
        let rd = unsafe { RamDisk::over_memory(module.as_mut_ptr(), module.len()) }.unwrap();
        assert_eq!(rd.capacity(), 2048);

        let mut out = [0xAB_u8; 512];
        let (status, n) = rd.transfer(&irp(IrpOp::Write, 512, &[frag(&mut out)]));
        assert_eq!((status, n), (IrpStatus::Success as i32, 512));

        let (mut a, mut b) = ([0u8; 100], [0u8; 412]);
        let (status, n) = rd.transfer(&irp(IrpOp::Read, 512, &[frag(&mut a), frag(&mut b)]));
        assert_eq!((status, n), (IrpStatus::Success as i32, 512));
        assert!(a.iter().chain(b.iter()).all(|&x| x == 0xAB));
        drop(rd);
        assert!(module[512..1024].iter().all(|&x| x == 0xAB), "the write landed in the module");
        assert!(module[..512].iter().all(|&x| x == 0), "and nowhere else");
    }

    /// **A transfer moves its IRP's length and no more**, whatever its fragments hold: a write at
    /// the last block through one fragment twice its length overran the disk into the memory past
    /// it, the root module's neighbour on the live image. Fragments short of it are refused.
    #[test]
    fn a_transfer_moves_its_length_whatever_its_fragments_hold() {
        init_global_heap();
        let mut module = vec![0xCD_u8; 2560];
        // SAFETY: `module` outlives `rd`, which is over its first 2048 bytes alone.
        let rd = unsafe { RamDisk::over_memory(module.as_mut_ptr(), 2048) }.unwrap();
        let mut long = [0xAB_u8; 1024];
        let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: 1, frags: &frag(&mut long) as *const PhysFrag as u64 };
        let write = Irp::new_block(IrpOp::Write, core::ptr::null(), 1536, 512, buffer, core::ptr::null_mut(), 0);
        assert_eq!(rd.transfer(&write), (IrpStatus::Success as i32, 512));
        let mut short = [0u8; 256];
        let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: 1, frags: &frag(&mut short) as *const PhysFrag as u64 };
        let read = Irp::new_block(IrpOp::Read, core::ptr::null(), 0, 512, buffer, core::ptr::null_mut(), 0);
        assert_eq!(rd.transfer(&read), (KError::InvalidArgument as i32, 0), "fragments short of it");
        drop(rd);
        assert!(module[1536..2048].iter().all(|&x| x == 0xAB), "the last block written");
        assert!(module[2048..].iter().all(|&x| x == 0xCD), "and not a byte past the disk");
    }

    /// **The disk it owns takes writes**, through a pointer that may write.
    // TODO(miri-provenance-tests): it fails only under Miri, which no CI job runs yet.
    #[test]
    fn a_disk_over_its_own_bytes_writes_and_reads_them_back() {
        init_global_heap();
        let rd = RamDisk::try_new().unwrap();
        let mut out = [0x5A_u8; 512];
        assert_eq!(rd.transfer(&irp(IrpOp::Write, 1024, &[frag(&mut out)])), (IrpStatus::Success as i32, 512));
        let mut back = [0u8; 512];
        assert_eq!(rd.transfer(&irp(IrpOp::Read, 1024, &[frag(&mut back)])), (IrpStatus::Success as i32, 512));
        assert_eq!(back, [0x5A; 512]);
        let mut before = [0u8; 512];
        rd.transfer(&irp(IrpOp::Read, 512, &[frag(&mut before)]));
        assert!(before.iter().enumerate().all(|(i, &b)| b == RamDisk::pattern_byte(512 + i)), "and nowhere else");
    }

    /// A RAM disk counts what it serves: each transfer by its op, the bytes it
    /// moved and the clock's time across the copy; a transfer past the end as an
    /// error; a rescan, which is no command, not at all.
    #[test]
    fn a_ram_disk_counts_what_it_serves() {
        static IO: crate::io::stats::IoStats = crate::io::stats::IoStats::new();
        init_global_heap();
        let mut module = vec![0u8; 4 * 512];
        // SAFETY: `module` outlives `rd` and nothing else touches it while `rd` is used.
        let rd = unsafe { RamDisk::over_memory(module.as_mut_ptr(), module.len()) }.unwrap();
        rd.counted().attach_static(&IO);
        let ticks = core::cell::Cell::new(0u64);
        let clock = || {
            ticks.set(ticks.get() + 5);
            ticks.get()
        };
        let mut buf = [0u8; 1024];
        assert_eq!(rd.serve(&irp(IrpOp::Write, 0, &[frag(&mut buf)]), clock), (IrpStatus::Success as i32, 1024));
        assert_eq!(rd.serve(&irp(IrpOp::Read, 512, &[frag(&mut buf[..512])]), clock).1, 512);
        assert_eq!(rd.serve(&irp(IrpOp::Read, 2048, &[frag(&mut buf[..512])]), clock).1, 0, "past the end");
        rd.serve(&irp(IrpOp::Rescan, 0, &[]), clock);
        let r = IO.record_for(0, 0);
        assert_eq!((r.writes, r.written_bytes, r.reads, r.read_bytes), (1, 1024, 1, 512));
        assert_eq!((r.errors, r.busy_ns), (1, 15), "three commands of 5 ticks each; the rescan none");
    }

    /// A flush completes at once and writes nothing (memory is the medium), even
    /// handed a buffer a write would have copied in; an unknown op is refused
    /// rather than taken for a write.
    #[test]
    fn a_flush_is_done_at_once_and_an_unknown_op_is_not_a_write() {
        init_global_heap();
        let mut module = vec![7u8; 1024];
        // SAFETY: as above.
        let rd = unsafe { RamDisk::over_memory(module.as_mut_ptr(), module.len()) }.unwrap();
        assert_eq!(rd.transfer(&irp(IrpOp::Flush, 0, &[])), (IrpStatus::Success as i32, 0));
        let mut out = [0xCD_u8; 512];
        assert_eq!(rd.transfer(&irp(IrpOp::Flush, 0, &[frag(&mut out)])), (IrpStatus::Success as i32, 0));
        let mut unknown = irp(IrpOp::Write, 0, &[frag(&mut out)]);
        unknown.op = 7;
        assert_eq!(rd.transfer(&unknown).0, KError::InvalidArgument as i32);
        assert_eq!(rd.transfer(&irp(IrpOp::Eject, 0, &[])).0, KError::Unsupported as i32, "nothing ejects it");
        drop(rd);
        assert!(module.iter().all(|&x| x == 7), "nothing written");
    }

    #[test]
    fn a_transfer_past_the_end_is_refused_and_touches_nothing() {
        init_global_heap();
        let mut module = vec![7u8; 1024];
        // SAFETY: as above.
        let rd = unsafe { RamDisk::over_memory(module.as_mut_ptr(), module.len()) }.unwrap();
        let mut out = [0u8; 512];
        let (status, n) = rd.transfer(&irp(IrpOp::Write, 768, &[frag(&mut out)]));
        assert_eq!((status, n), (KError::InvalidArgument as i32, 0), "ends 256 bytes past the disk");
        let (status, _) = rd.transfer(&irp(IrpOp::Read, u64::MAX - 10, &[frag(&mut out)]));
        assert_eq!(status, KError::InvalidArgument as i32, "an offset that overflows");
        drop(rd);
        assert!(module.iter().all(|&x| x == 7));
    }

    #[test]
    fn the_bring_up_disk_still_reads_its_pattern() {
        init_global_heap();
        let rd = RamDisk::try_new().unwrap();
        let mut out = [0u8; 64];
        rd.transfer(&irp(IrpOp::Read, 1000, &[frag(&mut out)]));
        assert!(out.iter().enumerate().all(|(i, &x)| x == RamDisk::pattern_byte(1000 + i)));
    }

    #[test]
    fn modules_are_recorded_until_the_table_is_full_with_their_paths_cut_to_fit() {
        let long = [b'p'; 100];
        for i in 0..MAX_MODULE_DISKS {
            // SAFETY: never published — `publish_modules` is not called here.
            assert!(unsafe { record_module(i + 1, core::ptr::null_mut(), 512, &long) });
        }
        // SAFETY: as above.
        assert!(!unsafe { record_module(9, core::ptr::null_mut(), 512, b"x") }, "a full table refuses");
        let slots = *MODULE_DISKS.lock();
        let first = slots[0].unwrap();
        assert_eq!((first.index, first.path_len), (1, PATH_BYTES));
    }
}
