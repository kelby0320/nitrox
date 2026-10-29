//! **The block device over `sys_io_submit`**: the [`BlockReader`] and [`BlockWriter`] a server
//! serves its filesystem through.

use crate::{BlockReader, BlockWriter, FsError};
use core::cell::Cell;
use core::ops::Range;
use libkern::*;

mod sys;

const PAGE: u64 = 4096;
/// The [`Disk`] transfer unit: one **4 KiB filesystem block** in **one** `sys_io_submit`. The
/// ext4 fs is block-aligned within a partition at least its size, so a covering-block access
/// never runs past the device.
const IO_BLOCK: usize = 4096;

/// Wait on one `PendingOperation`, read its `(status, result)` (`IoResult`: status @8, result
/// @16) and close it. **On buffers of its own**: it must not overwrite the batch of results the
/// server loop is walking.
///
/// `None` when the wait returned anything but one signalled record: the operation is **still
/// running**, and a buffer it was given is the device's still.
pub fn po_wait(po: u64) -> Option<(i32, u64)> {
    let mut results = [0u8; WAIT_RESULT_SIZE];
    let waited = sys::wait_one(po, &mut results);
    sys::handle_close(po);
    if waited != 1 {
        return None;
    }
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    let result = u64::from_le_bytes([
        results[16],
        results[17],
        results[18],
        results[19],
        results[20],
        results[21],
        results[22],
        results[23],
    ]);
    Some((status, result))
}

/// **The one buffer a device layer moves every transfer through**, a `MemoryObject` mapped for its
/// life. **Once a wait fails without its transfer complete it is lost**: the device may still read or
/// write it, so nothing is copied into it and nothing is submitted through it again.
pub struct Scratch {
    mem: u64,
    addr: u64,
    len: usize,
    lost: Cell<bool>,
}

impl Scratch {
    /// A scratch of `len` bytes; `None` if it could not be created or mapped.
    pub fn new(len: usize) -> Option<Scratch> {
        let mem = sys::memory_create(len as u64);
        if mem < 0 {
            return None;
        }
        let mem = mem as u64;
        let addr = sys::memory_map(mem, len as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE);
        if addr < 0 {
            sys::handle_close(mem);
            return None;
        }
        Some(Scratch { mem, addr: addr as u64, len, lost: Cell::new(false) })
    }

    /// One transfer of `len` bytes between the scratch and `device` at `offset`, waited for.
    /// `Io` for a refusal, a failed or short transfer, or a scratch already lost.
    pub fn transfer(&self, device: u64, opcode: u32, offset: u64, len: u64) -> Result<(), FsError> {
        self.submit(device, self.mem, opcode, offset, len)
    }

    /// A transfer through `buffer`, a memory object of this process's: `transfer` names the
    /// scratch's own. A wait that fails loses the scratch whichever buffer the transfer named,
    /// since a write of zeroes that lands late is as wrong as any other.
    pub(crate) fn submit(&self, device: u64, buffer: u64, opcode: u32, offset: u64, len: u64) -> Result<(), FsError> {
        if self.lost.get() {
            return Err(FsError::Io);
        }
        let op = IoOp { opcode, flags: 0, buffer, buf_offset: 0, offset, length: len };
        let po = sys::io_submit(device, &op);
        if po < 0 {
            return Err(FsError::Io);
        }
        match po_wait(po as u64) {
            None => {
                self.lost.set(true);
                Err(FsError::Io)
            }
            Some((0, moved)) if moved == len => Ok(()),
            Some(_) => Err(FsError::Io),
        }
    }

    /// Copy `src` into the scratch at `from`; `Io` once it is lost, as a write still in flight
    /// would carry the new bytes.
    pub fn copy_in(&self, from: usize, src: &[u8]) -> Result<(), FsError> {
        assert!(from + src.len() <= self.len);
        if self.lost.get() {
            return Err(FsError::Io);
        }
        // SAFETY: `new` mapped `len` read-write bytes at `addr`, which stay mapped until `drop`,
        // and the range is within them; `src` is a distinct buffer of the caller's.
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), (self.addr as *mut u8).add(from), src.len()) };
        Ok(())
    }

    /// Copy `dst.len()` bytes of the scratch from `from` into `dst`.
    pub fn copy_out(&self, from: usize, dst: &mut [u8]) {
        assert!(from + dst.len() <= self.len);
        // SAFETY: as for `copy_in`, the other way.
        unsafe { core::ptr::copy_nonoverlapping((self.addr as *const u8).add(from), dst.as_mut_ptr(), dst.len()) };
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        sys::memory_unmap(self.addr, self.len as u64);
        sys::handle_close(self.mem);
    }
}

/// A [`BlockReader`]/[`BlockWriter`] over the block device in **4 KiB blocks**, through a
/// one-page scratch `MemoryObject`; `write_at` is a per-block read-modify-write. The metadata a
/// mutation writes still goes a block at a time (`TODO(ext4-metadata-writes)`).
pub struct Disk {
    device: u64,
    scratch: Scratch,
    /// **[`SPAN`] bytes of zeroes**, a `MemoryObject` never written, so a grow's zeroes go to the
    /// device in large commands.
    zeroes: u64,
}

/// **How [`Disk`] writes `len` zeroes from `offset`**, as byte ranges `(head, whole, tail)`, any
/// empty: the part of a block at each end, written by `write_at`, and whole blocks between, in
/// commands of up to [`SPAN`].
pub fn split_zeroes(offset: u64, len: u64) -> (Range<u64>, Range<u64>, Range<u64>) {
    let end = offset + len;
    let first = offset.next_multiple_of(IO_BLOCK as u64).min(end);
    let last = (end / IO_BLOCK as u64 * IO_BLOCK as u64).max(first);
    (offset..first, first..last, last..end)
}

impl Disk {
    /// A disk over `device`, with its scratch made and mapped; or why not, as the line a server
    /// prints.
    pub fn new(device: u64) -> Result<Disk, &'static [u8]> {
        let Some(scratch) = Scratch::new(PAGE as usize) else {
            return Err(b"fs-server: scratch create failed\n");
        };
        let zeroes = sys::memory_create(SPAN as u64);
        if zeroes < 0 {
            return Err(b"fs-server: zero span create failed\n");
        }
        Ok(Disk { device, scratch, zeroes: zeroes as u64 })
    }

    /// The block-device handle: what a Model A reply hands the kernel a duplicate of.
    pub fn device(&self) -> u64 {
        self.device
    }
}

impl Drop for Disk {
    fn drop(&mut self) {
        sys::handle_close(self.zeroes);
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
            self.scratch.transfer(self.device, IO_OPCODE_READ, block * IO_BLOCK as u64, IO_BLOCK as u64)?;
            self.scratch.copy_out(in_block, &mut buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
}

impl BlockWriter for Disk {
    fn write_zeroes(&self, offset: u64, len: u64) -> Result<(), FsError> {
        static ZERO: [u8; IO_BLOCK] = [0; IO_BLOCK];
        let (head, whole, tail) = split_zeroes(offset, len);
        self.write_at(head.start, &ZERO[..(head.end - head.start) as usize])?;
        let mut at = whole.start;
        while at < whole.end {
            let n = (whole.end - at).min(SPAN as u64);
            self.scratch.submit(self.device, self.zeroes, IO_OPCODE_WRITE, at, n)?;
            at += n;
        }
        self.write_at(tail.start, &ZERO[..(tail.end - tail.start) as usize])
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let block = cur / IO_BLOCK as u64;
            let in_block = (cur % IO_BLOCK as u64) as usize;
            let n = core::cmp::min(IO_BLOCK - in_block, buf.len() - done);
            // A partial block is read first so the untouched bytes are preserved.
            if n != IO_BLOCK {
                self.scratch.transfer(self.device, IO_OPCODE_READ, block * IO_BLOCK as u64, IO_BLOCK as u64)?;
            }
            self.scratch.copy_in(in_block, &buf[done..done + n])?;
            self.scratch.transfer(self.device, IO_OPCODE_WRITE, block * IO_BLOCK as u64, IO_BLOCK as u64)?;
            done += n;
        }
        Ok(())
    }
}

/// The [`SectorDisk`]'s largest transfer: 64 KiB, sixteen pages, within every block driver's
/// fragment bound (AHCI's 248, USB storage's 64).
pub const SPAN: usize = 64 * 1024;
const SECTOR: u64 = 512;

/// **The block device in sectors, many to a submit**: any 512-byte-aligned range in one
/// `sys_io_submit` of up to [`SPAN`] bytes. **For `fs-server-fat`**, whose structures are
/// sector-aligned and whose partition's last sectors need not fill a 4 KiB block. A write that is
/// not sector-aligned reads its edge sectors first.
pub struct SectorDisk {
    device: u64,
    scratch: Scratch,
}

impl SectorDisk {
    /// A disk over `device`, with its 64 KiB scratch made and mapped; or why not.
    pub fn new(device: u64) -> Result<SectorDisk, &'static [u8]> {
        let Some(scratch) = Scratch::new(SPAN) else {
            return Err(b"fs-server: scratch create failed\n");
        };
        Ok(SectorDisk { device, scratch })
    }

    /// The block-device handle: what a Model A reply hands the kernel a duplicate of.
    pub fn device(&self) -> u64 {
        self.device
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
            self.scratch.transfer(self.device, IO_OPCODE_READ, at, n as u64)?;
            self.scratch.copy_out(from, &mut buf[done..done + take]);
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
                self.scratch.transfer(self.device, IO_OPCODE_READ, at, n as u64)?;
            }
            self.scratch.copy_in(from, &buf[done..done + take])?;
            self.scratch.transfer(self.device, IO_OPCODE_WRITE, at, n as u64)?;
            done += take;
            Ok(())
        })
    }
}

/// The [`PartitionIo`]'s scratch, and so its largest transfer: 128 KiB, within every block
/// driver's fragment bound. Larger than the 64 KiB `nxinstall`'s copy hands over at once.
pub const WINDOW_SPAN: usize = 128 * 1024;

/// **A window onto a device**: `[base, base + len)` of a block device, addressed from byte 0 — for
/// a program making a filesystem on a disk it holds. A range past the window is refused rather
/// than reaching the neighbouring partition. Any byte range becomes sector-aligned transfers,
/// read first where a write covers a sector only in part (a filesystem writes 32-byte
/// descriptors, a device moves 512-byte sectors). **One scratch object for both directions**.
/// **Not `Sync`**: every transfer passes through the one scratch.
pub struct PartitionIo {
    device: u64,
    /// Byte offset of the window's first sector on that device.
    base: u64,
    len: u64,
    /// A scratch of [`WINDOW_SPAN`] bytes.
    scratch: Scratch,
}

impl PartitionIo {
    /// A window onto `device`'s `[base, base + len)` bytes, `base` sector-aligned; `None` if the
    /// scratch could not be made.
    pub fn new(device: u64, base: u64, len: u64) -> Option<PartitionIo> {
        Some(PartitionIo { device, base, len, scratch: Scratch::new(WINDOW_SPAN)? })
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
            let (start, span, intra) =
                window(self.base, self.len, WINDOW_SPAN, offset + done as u64, buf.len() - done)?;
            self.scratch.transfer(self.device, IO_OPCODE_READ, start, span)?;
            let take = (buf.len() - done).min(span as usize - intra);
            self.scratch.copy_out(intra, &mut buf[done..done + take]);
            done += take;
        }
        Ok(())
    }
}

impl BlockWriter for PartitionIo {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let (start, span, intra) =
                window(self.base, self.len, WINDOW_SPAN, offset + done as u64, buf.len() - done)?;
            let take = (buf.len() - done).min(span as usize - intra);
            // **Read first unless the write covers whole sectors**: a partial sector written
            // without its surroundings replaces bytes the filesystem is still using.
            if intra != 0 || take != span as usize {
                self.scratch.transfer(self.device, IO_OPCODE_READ, start, span)?;
            }
            self.scratch.copy_in(intra, &buf[done..done + take])?;
            self.scratch.transfer(self.device, IO_OPCODE_WRITE, start, span)?;
            done += take;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
