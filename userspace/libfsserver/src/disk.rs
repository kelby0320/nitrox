//! **The block device over `sys_io_submit`**: the [`BlockReader`] and [`BlockWriter`] a server
//! serves its filesystem through.

use crate::{BlockReader, BlockWriter, FsError};
use core::cell::Cell;
use core::marker::PhantomData;
use libkern::*;

/// One page; the scratch buffer's size.
const PAGE: u64 = 4096;
/// The [`Disk`] device transfer unit: one **4 KiB filesystem block** (= one scratch
/// page). The AHCI path does the 8-sector transfer in a single command (it computes
/// `sector_count = length / 512`), so reading or writing a whole block costs **one**
/// `sys_io_submit` + one completion wake, not eight — 8× fewer block-I/O round trips than
/// the original 512-byte-sector-at-a-time path. The device's logical sector is still 512
/// bytes; this is the I/O granularity, aligned to the 4 KiB block. See
/// `docs/rationale/deferred-decisions.md` (fs-server 4 KiB-block I/O). The ext4 fs is
/// 4 KiB-block-aligned within a partition ≥ its size, so a covering-block access never
/// runs past the device.
const IO_BLOCK: usize = 4096;

/// Wait on one `PendingOperation`, then read its `(status, result)` (`IoResult`: status @8,
/// result @16) and close it.
///
/// **On buffers of its own** (Phase 6 Part E.1). Until the loop moved here this used the server
/// loop's wait arrays, and a read made while the loop was walking a batch of results overwrote
/// the batch's first record: one extra trip round the loop, since a handle so lost was still
/// signalled the next time. `File::Forget` already waited on its own for that reason.
pub fn po_wait(po: u64) -> (i32, u64) {
    let handles = [po];
    let mut results = [0u8; WAIT_RESULT_SIZE];
    // SAFETY: `handles` and `results` are valid local buffers for one waiter.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    let result = u64::from_le_bytes([
        results[16], results[17], results[18], results[19], results[20], results[21], results[22], results[23],
    ]);
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// A [`BlockReader`]/[`BlockWriter`] over the block device: each `read_at` reads the
/// covering **4 KiB blocks** via `sys_io_submit` into a one-page scratch `MemoryObject`
/// (mapped R/W) and copies the requested sub-range out; `write_at` is a per-block
/// read-modify-write. One `sys_io_submit` per 4 KiB block (the AHCI path does the 8-sector
/// transfer in a single command), not one per 512-byte sector — 8× fewer block-I/O round
/// trips, which is what made the same-CPU wake latency so acute (see the decision log,
/// 2026-07-23). The parser's individual reads are small (≤ one 4 KiB filesystem block).
///
/// **One block per `sys_io_submit` is a suspect in `TODO(fs-throughput)`** — `copy` and
/// `remove` are slower on real storage than a 5400 rpm disk accounts for (measured on the
/// laptop, 2026-09-17), and every metadata read and write a mutation makes comes through here
/// one 4 KiB block at a time. It is *a* suspect and not a diagnosis: see
/// `docs/rationale/deferred-decisions.md`, which lists the others and says the measurement
/// comes first.
pub struct Disk {
    /// The block-device handle (from the setup message).
    device: u64,
    /// A one-page scratch `MemoryObject` the device DMAs sectors into.
    scratch: u64,
    /// `scratch`, mapped R/W into this process — where read sectors land.
    scratch_addr: u64,
    /// **Not `Sync`**: every transfer passes through the one scratch, which two threads would
    /// share (Phase 6 Part G.2, found moving [`PartitionIo`] beside it).
    _one_thread: PhantomData<Cell<()>>,
}

impl Disk {
    /// **A disk over `device`**, with its scratch page made and mapped; or why it could not be,
    /// as the line a server prints.
    pub fn new(device: u64) -> Result<Disk, &'static [u8]> {
        // SAFETY: register-only syscall.
        let scratch = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
        if scratch < 0 {
            return Err(b"fs-server: scratch create failed\n");
        }
        let scratch = scratch as u64;
        // SAFETY: register-only syscall; `scratch` is ours.
        let scratch_addr = unsafe { syscall4(SYS_MEMORY_MAP, scratch, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if scratch_addr < 0 {
            return Err(b"fs-server: scratch map failed\n");
        }
        Ok(Disk { device, scratch, scratch_addr: scratch_addr as u64, _one_thread: PhantomData })
    }

    /// The block-device handle: what a Model A reply hands the kernel a duplicate of.
    pub fn device(&self) -> u64 {
        self.device
    }

    /// DMA `block` (4 KiB) into the scratch object in a single command; `Io` on any failure.
    fn read_block(&self, block: u64) -> Result<(), FsError> {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.scratch,
            buf_offset: 0,
            offset: block * IO_BLOCK as u64,
            length: IO_BLOCK as u64,
        };
        // SAFETY: `device` is a block DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != IO_BLOCK as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }

    /// Write the scratch object's first 4 KiB to device `block` in a single command; `Io`
    /// on failure.
    fn write_block(&self, block: u64) -> Result<(), FsError> {
        let op = IoOp {
            opcode: IO_OPCODE_WRITE,
            flags: 0,
            buffer: self.scratch,
            buf_offset: 0,
            offset: block * IO_BLOCK as u64,
            length: IO_BLOCK as u64,
        };
        // SAFETY: `device` is a block DeviceNode with WRITE; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != IO_BLOCK as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }
}

impl BlockReader for Disk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let block = cur / IO_BLOCK as u64;
            let in_block = (cur % IO_BLOCK as u64) as usize;
            let n = core::cmp::min(IO_BLOCK - in_block, buf.len() - done);
            self.read_block(block)?;
            // SAFETY: `scratch_addr` maps a full page R/W; the read block occupies
            // `[0, 4096)`, so `[in_block, in_block + n)` is in bounds.
            let src = unsafe { core::slice::from_raw_parts((self.scratch_addr as usize + in_block) as *const u8, n) };
            buf[done..done + n].copy_from_slice(src);
            done += n;
        }
        Ok(())
    }
}

impl BlockWriter for Disk {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let block = cur / IO_BLOCK as u64;
            let in_block = (cur % IO_BLOCK as u64) as usize;
            let n = core::cmp::min(IO_BLOCK - in_block, buf.len() - done);
            // Read-modify-write for a partial block: read it into scratch first so the
            // untouched bytes are preserved. A full-block write skips the read.
            if n != IO_BLOCK {
                self.read_block(block)?;
            }
            // SAFETY: `scratch_addr` maps a full page R/W; `[in_block, in_block + n)` is
            // within the block region `[0, 4096)`.
            let dst = unsafe { core::slice::from_raw_parts_mut((self.scratch_addr as usize + in_block) as *mut u8, n) };
            dst.copy_from_slice(&buf[done..done + n]);
            self.write_block(block)?;
            done += n;
        }
        Ok(())
    }
}

/// The [`SectorDisk`]'s largest transfer: 64 KiB, sixteen pages, within every block driver's
/// fragment bound (AHCI's 248, USB storage's 64).
pub const SPAN: usize = 64 * 1024;
/// A sector: the unit [`SectorDisk`] aligns to.
const SECTOR: u64 = 512;

/// **The block device in sectors, many to a submit** (Phase 6 Part E.3): a [`BlockReader`] and
/// [`BlockWriter`] that moves any 512-byte-aligned range in one `sys_io_submit` of up to [`SPAN`]
/// bytes, through a 64 KiB scratch `MemoryObject`.
///
/// **For `fs-server-fat`**, whose structures are sector-aligned rather than 4 KiB-aligned, whose
/// partition's last sectors need not fill a 4 KiB block, and whose write path batches — a grow
/// zeroes what it adds 64 KiB at a time, and the FAT's dirty sectors go out a run at a time. A
/// write that is not sector-aligned reads its edge sectors first. **`fs-server-ext4` keeps
/// [`Disk`]**, so Phase 6 Part H measures it as it is.
pub struct SectorDisk {
    device: u64,
    scratch: u64,
    scratch_addr: u64,
    /// Not `Sync`, as [`Disk`] is not.
    _one_thread: PhantomData<Cell<()>>,
}

impl SectorDisk {
    /// **A disk over `device`**, with its 64 KiB scratch made and mapped; or why it could not be.
    pub fn new(device: u64) -> Result<SectorDisk, &'static [u8]> {
        // SAFETY: register-only syscall.
        let scratch = unsafe { syscall4(SYS_MEMORY_CREATE, SPAN as u64, 0, 0, 0) };
        if scratch < 0 {
            return Err(b"fs-server: scratch create failed\n");
        }
        let scratch = scratch as u64;
        // SAFETY: register-only syscall; `scratch` is ours.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, scratch, 0, SPAN as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr < 0 {
            return Err(b"fs-server: scratch map failed\n");
        }
        Ok(SectorDisk { device, scratch, scratch_addr: addr as u64, _one_thread: PhantomData })
    }

    /// The block-device handle: what a Model A reply hands the kernel a duplicate of.
    pub fn device(&self) -> u64 {
        self.device
    }

    /// Move `len` bytes, sector-aligned and at most [`SPAN`], between device byte `at` and the
    /// scratch's start, in one submit.
    fn transfer(&self, opcode: u32, at: u64, len: usize) -> Result<(), FsError> {
        let op = IoOp { opcode, flags: 0, buffer: self.scratch, buf_offset: 0, offset: at, length: len as u64 };
        // SAFETY: `device` is a block DeviceNode with READ (and WRITE for a write); `&op` is valid.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != len as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }

    /// Copy scratch bytes `[from, from + dst.len())` out into `dst`.
    fn copy_out(&self, from: usize, dst: &mut [u8]) {
        assert!(from + dst.len() <= SPAN);
        // SAFETY: `scratch_addr` maps `SPAN` bytes R/W for this process's life, and the range is
        // within it; `dst` is a distinct buffer of the caller's.
        unsafe { core::ptr::copy_nonoverlapping((self.scratch_addr as *const u8).add(from), dst.as_mut_ptr(), dst.len()) };
    }

    /// Copy `src` into scratch bytes `[from, from + src.len())`.
    fn copy_in(&self, from: usize, src: &[u8]) {
        assert!(from + src.len() <= SPAN);
        // SAFETY: as for `copy_out`, the other way.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), (self.scratch_addr as *mut u8).add(from), src.len()) };
    }

    /// **The sector-aligned chunks covering `[offset, offset + len)`**, each at most [`SPAN`]:
    /// `(chunk start, chunk length, where the request's bytes begin in it, how many)`.
    fn chunks(
        offset: u64,
        len: usize,
        mut each: impl FnMut(u64, usize, usize, usize) -> Result<(), FsError>,
    ) -> Result<(), FsError> {
        let end = offset + len as u64;
        let mut at = offset - offset % SECTOR;
        while at < end {
            let span = (end - at).div_ceil(SECTOR) * SECTOR;
            let n = (span as usize).min(SPAN);
            let from = (offset.max(at) - at) as usize;
            let take = ((at + n as u64).min(end) - offset.max(at)) as usize;
            each(at, n, from, take)?;
            at += n as u64;
        }
        Ok(())
    }
}

impl BlockReader for SectorDisk {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        SectorDisk::chunks(offset, buf.len(), |at, n, from, take| {
            self.transfer(IO_OPCODE_READ, at, n)?;
            self.copy_out(from, &mut buf[done..done + take]);
            done += take;
            Ok(())
        })
    }
}

impl BlockWriter for SectorDisk {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        SectorDisk::chunks(offset, buf.len(), |at, n, from, take| {
            // A chunk the request does not cover whole keeps the bytes it does not write.
            if from != 0 || take != n {
                self.transfer(IO_OPCODE_READ, at, n)?;
            }
            self.copy_in(from, &buf[done..done + take]);
            self.transfer(IO_OPCODE_WRITE, at, n)?;
            done += take;
            Ok(())
        })
    }
}

/// The [`PartitionIo`]'s scratch, and so its largest transfer: 128 KiB, within every block
/// driver's fragment bound, as [`SPAN`] is. Larger than the 64 KiB `nxinstall`'s copy hands over
/// at once, so a file's data crosses in one transfer rather than two.
pub const WINDOW_SPAN: usize = 128 * 1024;

/// **A window onto a device** (Phase 6 Part G.2): a [`BlockReader`] and [`BlockWriter`] over
/// `[base, base + len)` of a block device, which a filesystem library addresses from byte 0 — what a
/// program making a filesystem on a disk it holds writes through, where a server is handed a
/// partition of its own.
///
/// **`nxinstall`'s, until `disk --format` needed it too**: a helper with two consumers belongs below
/// both. A range past the window is refused rather than reaching the neighbouring partition. Any
/// byte range is turned into the sector-aligned transfers `sys_io_submit` takes, read first where a
/// write covers a sector only in part, since a filesystem writes 32-byte group descriptors and
/// 256-byte inodes, and a device moves 512-byte sectors.
///
/// **One scratch object for both directions**, made and mapped by [`PartitionIo::new`] and
/// unmapped when the window is dropped, so a copy costs a fixed amount of memory whatever the size
/// of the disk. **Not `Sync`**, as the device types above are not: every transfer passes through
/// the one scratch, which two threads would share.
pub struct PartitionIo {
    /// The block-device handle, with read and write.
    device: u64,
    /// Byte offset of the window's first sector on that device.
    base: u64,
    /// The window's length in bytes.
    len: u64,
    /// A `MemoryObject` of [`WINDOW_SPAN`] bytes every transfer passes through.
    scratch: u64,
    /// `scratch`, mapped read-write into this process.
    scratch_addr: u64,
    _one_thread: PhantomData<Cell<()>>,
}

impl PartitionIo {
    /// **A window onto `device`'s `[base, base + len)` bytes**, `base` sector-aligned, with its
    /// scratch made and mapped; or `None` if the scratch could not be.
    pub fn new(device: u64, base: u64, len: u64) -> Option<PartitionIo> {
        // SAFETY: register-only syscall.
        let scratch = unsafe { syscall4(SYS_MEMORY_CREATE, WINDOW_SPAN as u64, 0, 0, 0) };
        if scratch < 0 {
            return None;
        }
        let scratch = scratch as u64;
        // SAFETY: register-only syscall; `scratch` is ours.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, scratch, 0, WINDOW_SPAN as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr < 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, scratch) };
            return None;
        }
        Some(PartitionIo { device, base, len, scratch, scratch_addr: addr as u64, _one_thread: PhantomData })
    }

    /// Move `length` bytes between the device at `offset` and the scratch's start. `offset` and
    /// `length` are sector-aligned and `length` is at most [`WINDOW_SPAN`].
    fn submit(&self, opcode: u32, offset: u64, length: u64) -> Result<(), FsError> {
        let op = IoOp { opcode, flags: 0, buffer: self.scratch, buf_offset: 0, offset, length };
        // SAFETY: `device` is a block device handle held by this process; `&op` is a valid `IoOp`
        // naming a `MemoryObject` this process owns.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, moved) = po_wait(po as u64);
        if status != 0 || moved != length {
            return Err(FsError::Io);
        }
        Ok(())
    }

    /// The scratch's first `n` bytes, `n` at most [`WINDOW_SPAN`].
    fn scratch(&self, n: usize) -> &mut [u8] {
        assert!(n <= WINDOW_SPAN);
        // SAFETY: `new` mapped `WINDOW_SPAN` read-write bytes at `scratch_addr`, which stay mapped
        // until `drop`; `n` is within them; and the type is not `Sync` and no method holds the
        // slice past its own use, so nothing else refers to the mapping while this does.
        unsafe { core::slice::from_raw_parts_mut(self.scratch_addr as *mut u8, n) }
    }
}

/// **The device range covering `[at, at + want)` of a window** of `len` bytes at `base`, at most
/// `cap` bytes: `(sector-aligned device offset, bytes to transfer, offset of at within them)`; or
/// `Io` when the range runs past the window.
fn window(base: u64, len: u64, cap: usize, at: u64, want: usize) -> Result<(u64, u64, usize), FsError> {
    let end = at.checked_add(want as u64).ok_or(FsError::Io)?;
    if end > len {
        return Err(FsError::Io); // past the window: the neighbour is not ours
    }
    let cur = base + at;
    let start = cur / SECTOR * SECTOR;
    let intra = (cur - start) as usize;
    // As much as fits in the scratch after the leading partial sector.
    let take = want.min(cap - intra);
    let span = (intra + take).div_ceil(SECTOR as usize) as u64 * SECTOR;
    Ok((start, span, intra))
}

impl BlockReader for PartitionIo {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let (start, span, intra) = window(self.base, self.len, WINDOW_SPAN, offset + done as u64, buf.len() - done)?;
            self.submit(IO_OPCODE_READ, start, span)?;
            let take = (buf.len() - done).min(span as usize - intra);
            buf[done..done + take].copy_from_slice(&self.scratch(span as usize)[intra..intra + take]);
            done += take;
        }
        Ok(())
    }
}

impl BlockWriter for PartitionIo {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let (start, span, intra) = window(self.base, self.len, WINDOW_SPAN, offset + done as u64, buf.len() - done)?;
            let take = (buf.len() - done).min(span as usize - intra);
            // **Read first unless the write covers whole sectors**: a partial sector written without
            // its surroundings replaces bytes the filesystem is still using.
            if intra != 0 || take != span as usize {
                self.submit(IO_OPCODE_READ, start, span)?;
            }
            self.scratch(span as usize)[intra..intra + take].copy_from_slice(&buf[done..done + take]);
            self.submit(IO_OPCODE_WRITE, start, span)?;
            done += take;
        }
        Ok(())
    }
}

impl Drop for PartitionIo {
    fn drop(&mut self) {
        // SAFETY: our own mapping and handle, made by `new`; nothing refers to the mapping once the
        // window is gone.
        unsafe {
            syscall4(SYS_MEMORY_UNMAP, self.scratch_addr, WINDOW_SPAN as u64, 0, 0);
            syscall1(SYS_HANDLE_CLOSE, self.scratch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The chunks a request moves in** cover it exactly: sector-aligned, contiguous, none over
    /// [`SPAN`], and the request's bytes within each — for aligned and unaligned starts and ends,
    /// one sector, and a request just over a span.
    #[test]
    fn a_request_is_covered_by_aligned_chunks_of_at_most_a_span() {
        for (offset, len) in [(0, 512), (100, 70_000), (512, SPAN), (511, 2), (1000, SPAN + 1), (4096, 3 * SPAN), (7, 1)] {
            let mut next = offset - offset % SECTOR;
            let mut covered = 0usize;
            SectorDisk::chunks(offset, len, |at, n, from, take| {
                assert_eq!(at, next, "contiguous");
                assert_eq!(at % SECTOR, 0);
                assert_eq!(n as u64 % SECTOR, 0);
                assert!(n <= SPAN && from + take <= n && take > 0);
                assert_eq!(at + from as u64, offset + covered as u64, "the request's bytes in order");
                covered += take;
                next = at + n as u64;
                Ok(())
            })
            .unwrap();
            assert_eq!(covered, len, "({offset}, {len})");
        }
    }

    /// **A window's transfers** stay inside it and inside the scratch: a range past its end is
    /// refused, a range up to it is taken, and each transfer is sector-aligned on the device and
    /// covers the bytes asked for from where they begin.
    #[test]
    fn a_window_refuses_past_its_end_and_moves_sector_aligned_spans() {
        let (base, len) = (1 << 20, 10 * 512);
        assert_eq!(window(base, len, WINDOW_SPAN, 0, len as usize + 1), Err(FsError::Io));
        assert_eq!(window(base, len, WINDOW_SPAN, len, 1), Err(FsError::Io), "the first byte past it");
        assert_eq!(window(base, len, WINDOW_SPAN, u64::MAX, 2), Err(FsError::Io), "no overflow");
        assert_eq!(window(base, len, WINDOW_SPAN, len - 1, 1), Ok((base + len - 512, 512, 511)));
        assert_eq!(window(base, len, WINDOW_SPAN, 100, 1000), Ok((base, 1536, 100)), "three sectors");
        let big = 4 * WINDOW_SPAN as u64;
        let (start, span, intra) = window(base, big, WINDOW_SPAN, 3, big as usize - 3).unwrap();
        assert_eq!((start, span, intra), (base, WINDOW_SPAN as u64, 3), "at most a scratch's worth");
    }
}
