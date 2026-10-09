//! **A cache of the metadata a server reads** (Phase 6 Part H.4).
//!
//! A filesystem server walks a path, scans a directory, reads an inode, a bitmap, a descriptor and
//! the superblock for every operation, and the laptop's runs found it doing so for every file of a
//! copy: a file of `small/` cost 782 reads on the stick's FAT, a directory scanned a sector at a
//! time, and 432 within ext4 (the decision log, 2026-10-09). [`MetaCache`] sits between the server
//! and its device and keeps the blocks those reads touch.
//!
//! **What it may hold is what only the server writes.** The kernel writes a file's data to the
//! device itself, behind the server's back (Model A), so a cached copy of a data block could be
//! stale. Only [`BlockReader::read_meta`] reads through the cache — the filesystem libraries call
//! it for their own structures and nothing else — and every other read goes to the device. Every
//! write goes to the device and then into any cached copy, so a cached block is never behind the
//! device for anything the server wrote; and [`BlockWriter::forget`], which a server calls on what
//! it frees, lets the blocks go before they can become a file's data.

use crate::{BlockReader, BlockWriter, FsError};
use core::cell::{Cell, RefCell};

/// The cache's unit: a 4 KiB block of the device, aligned to the device's start.
pub const BLOCK: usize = 4096;
/// **How many blocks a server keeps**: 1 MiB. A copy's working set — the two directories it is
/// between, a few inode-table blocks, the bitmaps, the descriptors and the superblock — is a few
/// dozen; the rest keeps a tree's directories as a copy walks it.
pub const SERVER_BLOCKS: usize = 256;
/// A slot holding nothing. A tag is a block's number plus one, so an empty store is all zeroes and a
/// server's `static` of one costs its binary nothing (`.bss`).
const EMPTY: u64 = 0;

/// **The cache's memory**: `N` blocks, each with the device block it holds and when it was last
/// used. A server keeps one in a `static`, since it holds no heap.
pub struct Store<const N: usize> {
    blocks: [[u8; BLOCK]; N],
    tags: [u64; N],
    used: [u64; N],
}

impl<const N: usize> Store<N> {
    /// An empty store.
    pub const fn new() -> Store<N> {
        Store { blocks: [[0; BLOCK]; N], tags: [EMPTY; N], used: [0; N] }
    }

    fn slot_of(&self, block: u64) -> Option<usize> {
        self.tags.iter().position(|&t| t == block + 1)
    }

    /// The slot to fill: an empty one, or the one used longest ago.
    fn victim(&self) -> usize {
        (0..N).min_by_key(|&i| if self.tags[i] == EMPTY { 0 } else { self.used[i] + 1 }).unwrap_or(0)
    }

    /// The device blocks `[offset, offset + len)` touches, each with the part of it touched.
    fn spans(offset: u64, len: usize) -> impl Iterator<Item = (u64, usize, usize)> {
        let end = offset + len as u64;
        let first = offset / BLOCK as u64;
        let last = if len == 0 { first } else { (end - 1) / BLOCK as u64 + 1 };
        (first..last).map(move |b| {
            let from = offset.max(b * BLOCK as u64);
            let to = end.min((b + 1) * BLOCK as u64);
            (b, (from - b * BLOCK as u64) as usize, (to - from) as usize)
        })
    }
}

impl<const N: usize> Default for Store<N> {
    fn default() -> Store<N> {
        Store::new()
    }
}

/// **A device with a metadata cache in front of it** — what a server serves its filesystem through
/// (see the module's doc for what it may hold).
pub struct MetaCache<'a, D, const N: usize> {
    dev: &'a D,
    store: RefCell<&'a mut Store<N>>,
    clock: Cell<u64>,
}

impl<'a, D: BlockReader + BlockWriter, const N: usize> MetaCache<'a, D, N> {
    /// `dev`, read through `store`, which starts empty whatever it held.
    pub fn new(dev: &'a D, store: &'a mut Store<N>) -> MetaCache<'a, D, N> {
        store.tags = [EMPTY; N];
        MetaCache { dev, store: RefCell::new(store), clock: Cell::new(0) }
    }

    /// The device underneath.
    pub fn device(&self) -> &D {
        self.dev
    }
}

impl<D: BlockReader + BlockWriter, const N: usize> BlockReader for MetaCache<'_, D, N> {
    /// **Straight to the device**: a file's data, which the cache never holds.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        self.dev.read_at(offset, buf)
    }

    /// **Through the cache**: a block it holds is answered from memory, and one it does not is read
    /// whole, kept, and answered. A block the device cannot read whole — the end of a partition that
    /// is not whole blocks — is read as asked, and not kept.
    fn read_meta(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0;
        for (block, within, n) in Store::<N>::spans(offset, buf.len()) {
            let out = &mut buf[done..done + n];
            done += n;
            let mut s = self.store.borrow_mut();
            let i = match s.slot_of(block) {
                Some(i) => i,
                None => {
                    let i = s.victim();
                    s.tags[i] = EMPTY;
                    if self.dev.read_at(block * BLOCK as u64, &mut s.blocks[i]).is_err() {
                        drop(s);
                        self.dev.read_at(block * BLOCK as u64 + within as u64, out)?;
                        continue;
                    }
                    s.tags[i] = block + 1;
                    i
                }
            };
            self.clock.set(self.clock.get() + 1);
            s.used[i] = self.clock.get();
            out.copy_from_slice(&s.blocks[i][within..within + n]);
        }
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.dev.read_only()
    }
}

impl<D: BlockReader + BlockWriter, const N: usize> BlockWriter for MetaCache<'_, D, N> {
    /// **To the device, then into any copy the cache holds** — or, if the write failed and the
    /// device's bytes are not known, the copy let go.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let written = self.dev.write_at(offset, buf);
        let mut s = self.store.borrow_mut();
        let mut done = 0;
        for (block, within, n) in Store::<N>::spans(offset, buf.len()) {
            if let Some(i) = s.slot_of(block) {
                match written {
                    Ok(()) => s.blocks[i][within..within + n].copy_from_slice(&buf[done..done + n]),
                    Err(_) => s.tags[i] = EMPTY,
                }
            }
            done += n;
        }
        written
    }

    fn forget(&self, offset: u64, len: u64) {
        let mut s = self.store.borrow_mut();
        let (first, end) = (offset / BLOCK as u64, (offset + len).div_ceil(BLOCK as u64));
        for t in s.tags.iter_mut() {
            if *t != EMPTY && *t - 1 >= first && *t - 1 < end {
                *t = EMPTY;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// An image in memory that counts the reads and writes reaching it, and can refuse a range.
    struct Device {
        bytes: RefCell<Vec<u8>>,
        reads: Cell<u32>,
        writes: Cell<u32>,
        fail_writes: Cell<bool>,
    }
    impl Device {
        fn new(len: usize) -> Device {
            let bytes = (0..len).map(|i| (i / BLOCK) as u8).collect();
            Device { bytes: RefCell::new(bytes), reads: 0.into(), writes: 0.into(), fail_writes: false.into() }
        }
        fn taken(&self) -> (u32, u32) {
            (self.reads.take(), self.writes.take())
        }
    }
    impl BlockReader for Device {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
            self.reads.set(self.reads.get() + 1);
            let v = self.bytes.borrow();
            let end = offset as usize + buf.len();
            buf.copy_from_slice(v.get(offset as usize..end).ok_or(FsError::Io)?);
            Ok(())
        }
    }
    impl BlockWriter for Device {
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
            self.writes.set(self.writes.get() + 1);
            if self.fail_writes.get() {
                return Err(FsError::Io);
            }
            let mut v = self.bytes.borrow_mut();
            let end = offset as usize + buf.len();
            v.get_mut(offset as usize..end).ok_or(FsError::Io)?.copy_from_slice(buf);
            Ok(())
        }
    }

    /// **A block read as metadata is read from the device once**, whole, and every later read of it
    /// — of any part — answered from memory; a read across two blocks reads each once. A read that
    /// is not metadata always reaches the device.
    #[test]
    fn metadata_is_read_from_the_device_once_and_data_every_time() {
        let dev = Device::new(8 * BLOCK);
        let mut store = Box::new(Store::<4>::new());
        let c = MetaCache::new(&dev, &mut store);
        let mut b = [0u8; 100];
        c.read_meta(2 * BLOCK as u64 + 10, &mut b).unwrap();
        assert_eq!((b[0], dev.taken()), (2, (1, 0)));
        c.read_meta(2 * BLOCK as u64 + 3000, &mut b).unwrap();
        assert_eq!(dev.taken(), (0, 0), "the same block, from memory");
        let mut across = [0u8; 200];
        c.read_meta(4 * BLOCK as u64 - 100, &mut across).unwrap();
        assert_eq!((across[0], across[199], dev.taken()), (3, 4, (2, 0)));
        c.read_at(2 * BLOCK as u64, &mut b).unwrap();
        c.read_at(2 * BLOCK as u64, &mut b).unwrap();
        assert_eq!(dev.taken(), (2, 0), "data reads reach the device every time");
    }

    /// **A write reaches the device, and then any copy the cache holds**, so the next metadata read
    /// sees it without a read; a write the device refused lets the copy go, so the next read asks the
    /// device. `forget` lets a block go.
    #[test]
    fn a_write_reaches_the_device_and_the_copy_and_forget_lets_a_block_go() {
        let dev = Device::new(8 * BLOCK);
        let mut store = Box::new(Store::<4>::new());
        let c = MetaCache::new(&dev, &mut store);
        let mut b = [0u8; 4];
        c.read_meta(BLOCK as u64, &mut b).unwrap();
        dev.taken();
        c.write_at(BLOCK as u64 + 1, &[9, 9]).unwrap();
        c.read_meta(BLOCK as u64, &mut b).unwrap();
        assert_eq!((b, dev.taken()), ([1, 9, 9, 1], (0, 1)));
        assert_eq!(&dev.bytes.borrow()[BLOCK..BLOCK + 4], &[1, 9, 9, 1], "and on the device");

        dev.fail_writes.set(true);
        assert_eq!(c.write_at(BLOCK as u64, &[7]), Err(FsError::Io));
        dev.fail_writes.set(false);
        dev.taken();
        c.read_meta(BLOCK as u64, &mut b).unwrap();
        assert_eq!((b, dev.taken()), ([1, 9, 9, 1], (1, 0)), "a failed write's block is read again");

        c.forget(BLOCK as u64, BLOCK as u64);
        dev.bytes.borrow_mut()[BLOCK] = 42; // the kernel, writing what is now a file's block
        c.read_meta(BLOCK as u64, &mut b).unwrap();
        assert_eq!((b[0], dev.taken()), (42, (1, 0)), "a forgotten block is read afresh");
    }

    /// **The block used longest ago goes**, so a working set the size of the cache stays in it; and
    /// a block the device cannot read whole — a partition's ragged end — is read as asked and not
    /// kept.
    #[test]
    fn the_block_used_longest_ago_goes_and_a_ragged_end_is_read_as_asked() {
        let dev = Device::new(6 * BLOCK + 512);
        let mut store = Box::new(Store::<2>::new());
        let c = MetaCache::new(&dev, &mut store);
        let mut b = [0u8; 1];
        for blk in [0u64, 1, 0, 2] {
            c.read_meta(blk * BLOCK as u64, &mut b).unwrap();
        }
        assert_eq!(dev.taken(), (3, 0));
        c.read_meta(0, &mut b).unwrap();
        assert_eq!(dev.taken(), (0, 0), "block 0, used more lately than 1, stayed");
        c.read_meta(BLOCK as u64, &mut b).unwrap();
        assert_eq!(dev.taken(), (1, 0), "block 1 went");

        let mut tail = vec![0u8; 512];
        c.read_meta(6 * BLOCK as u64, &mut tail).unwrap();
        assert_eq!((tail[0], dev.taken()), (6, (2, 0)), "the whole block failed, the part asked for read");
        c.read_meta(6 * BLOCK as u64, &mut tail).unwrap();
        assert_eq!(dev.taken(), (2, 0), "and not kept");
    }
}
