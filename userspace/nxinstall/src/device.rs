//! A [`BlockReader`]/[`BlockWriter`] over one partition of a block device.
//!
//! The ext4 library addresses a filesystem from byte 0 of the thing it lives on; a partition
//! starts part-way down a disk. This adds the base offset and turns arbitrary byte ranges into
//! the sector-aligned transfers `sys_io_submit` takes — the same read-modify-write the
//! fs-server does, for the same reason: a filesystem writes 32-byte group descriptors and
//! 256-byte inodes, and a device moves 512-byte sectors.
//!
//! **One scratch object for both directions.** Every transfer passes through it, so a copy
//! costs a fixed amount of memory whatever the size of the disk.

use libkern::abi::{IO_OPCODE_READ, IO_OPCODE_WRITE, IoOp};
use libkern::syscall::{SYS_HANDLE_CLOSE, SYS_IO_SUBMIT, SYS_WAIT, syscall1, syscall2, syscall4};

use fs_server_ext4::{BlockReader, BlockWriter, FsError};

/// What a block device moves at a time.
const SECTOR: u64 = 512;

/// One window onto a device: a base offset, a length, and a scratch buffer.
pub struct PartitionIo {
    /// The block-device handle, with read and write.
    device: u64,
    /// Byte offset of the partition's first sector on that device.
    base: u64,
    /// The partition's length in bytes. Reads and writes past it are refused rather than
    /// silently reaching the neighbouring partition.
    len: u64,
    /// A `MemoryObject` every transfer passes through.
    mem: u64,
    /// Its mapping in this process.
    addr: u64,
    /// How many bytes that mapping holds.
    cap: usize,
}

impl PartitionIo {
    /// Wrap `device`'s `[base, base + len)` bytes, transferring through `mem`.
    ///
    /// # Safety
    ///
    /// `mem` must be mapped read-write at `addr` for at least `cap` bytes, `cap` must be a
    /// multiple of [`SECTOR`], and both must stay valid for this value's lifetime.
    ///
    /// **`unsafe` because this is where the contract is made.** `read_at` and `write_at` are
    /// safe methods that build a `&mut [u8]` from `addr` and `cap`; if a safe constructor
    /// could set them, then entirely safe code — `PartitionIo::new(dev, base, len, mem, 0,
    /// cap)`, or a `cap` larger than the mapping — would reach undefined behaviour through a
    /// safe call. A `# Safety` section on a safe `fn` is the tell (PR #310 review, finding 2).
    pub unsafe fn new(device: u64, base: u64, len: u64, mem: u64, addr: u64, cap: usize) -> Self {
        PartitionIo { device, base, len, mem, addr, cap }
    }

    /// Move `length` bytes between the device at `offset` and the scratch buffer's start.
    /// `offset` and `length` are sector-aligned and `length` is at most `cap`.
    fn submit(&self, opcode: u32, offset: u64, length: u64) -> Result<(), FsError> {
        let op = IoOp { opcode, flags: 0, buffer: self.mem, buf_offset: 0, offset, length };
        // SAFETY: `device` is a block device handle held by this process; `&op` is a valid
        // `IoOp` naming a `MemoryObject` this process owns.
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

    /// The device range covering `[at, at + want)` within the partition, as
    /// `(sector-aligned device offset, bytes to transfer, offset of `at` within them)`.
    fn window(&self, at: u64, want: usize) -> Result<(u64, u64, usize), FsError> {
        let end = at.checked_add(want as u64).ok_or(FsError::Io)?;
        if end > self.len {
            return Err(FsError::Io); // past the partition: the neighbour is not ours
        }
        let cur = self.base + at;
        let start = cur / SECTOR * SECTOR;
        let intra = (cur - start) as usize;
        // As much as fits in the scratch after the leading partial sector.
        let take = want.min(self.cap - intra);
        let span = (intra + take).div_ceil(SECTOR as usize) as u64 * SECTOR;
        Ok((start, span, intra))
    }

    /// The scratch buffer's first `n` bytes.
    ///
    /// # Safety
    ///
    /// `n <= cap`. That `addr` maps `cap` bytes read-write is [`new`](Self::new)'s contract.
    unsafe fn scratch(&self, n: usize) -> &mut [u8] {
        // SAFETY: `new` promised `addr` maps `cap` read-write bytes; the caller promised
        // `n <= cap`; and this process is single-threaded, so nothing else holds a reference
        // to the mapping.
        unsafe { core::slice::from_raw_parts_mut(self.addr as *mut u8, n) }
    }
}

impl BlockReader for PartitionIo {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let (start, span, intra) = self.window(offset + done as u64, buf.len() - done)?;
            self.submit(IO_OPCODE_READ, start, span)?;
            let take = (buf.len() - done).min(span as usize - intra);
            // SAFETY: `span <= cap` by `window`, and `intra + take <= span`.
            let src = unsafe { self.scratch(span as usize) };
            buf[done..done + take].copy_from_slice(&src[intra..intra + take]);
            done += take;
        }
        Ok(())
    }
}

impl BlockWriter for PartitionIo {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let (start, span, intra) = self.window(offset + done as u64, buf.len() - done)?;
            let take = (buf.len() - done).min(span as usize - intra);
            // **Read first unless the write covers whole sectors.** A partial sector written
            // without its surroundings replaces bytes the filesystem is still using — the
            // superblock shares its sector with nothing, but a group descriptor is 32 bytes
            // and an inode 256, so most metadata writes land inside a sector with neighbours.
            if intra != 0 || take != span as usize {
                self.submit(IO_OPCODE_READ, start, span)?;
            }
            // SAFETY: `span <= cap` by `window`, and `intra + take <= span`.
            let dst = unsafe { self.scratch(span as usize) };
            dst[intra..intra + take].copy_from_slice(&buf[done..done + take]);
            self.submit(IO_OPCODE_WRITE, start, span)?;
            done += take;
        }
        Ok(())
    }
}

/// Wait for one pending operation and return its `(status, value)`.
fn po_wait(po: u64) -> (i32, u64) {
    let mut handles = [po];
    let mut results = [0u8; 24];
    // SAFETY: valid single-waiter buffers; the PO handle is ours.
    let waited = unsafe {
        syscall4(
            SYS_WAIT,
            handles.as_mut_ptr() as u64,
            1,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
    // SAFETY: closing the PO we own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    let value = u64::from_le_bytes([
        results[16], results[17], results[18], results[19], results[20], results[21],
        results[22], results[23],
    ]);
    (status, value)
}
