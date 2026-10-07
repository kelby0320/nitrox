//! **The block device over `sys_io_submit`**: the [`BlockReader`] and [`BlockWriter`] a server
//! serves its filesystem through.

use crate::{BlockReader, BlockWriter, FsError};
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
        Ok(Disk { device, scratch, scratch_addr: scratch_addr as u64 })
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
        Ok(SectorDisk { device, scratch, scratch_addr: addr as u64 })
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
}
