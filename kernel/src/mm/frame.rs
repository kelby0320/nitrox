//! A frame's bytes, through the HHDM: a [`FrameView`] its owner lends, and [`hhdm`] for memory no
//! owner lends (firmware tables, page tables, a DMA buffer's memory). Its owner is a [`Frame`].

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::mm::{PAGE_SIZE, PhysAddr, heap};

const WORD: usize = size_of::<u64>();

/// Where the kernel reaches physical memory `phys`; reaching through it is the caller's `unsafe`.
pub fn hhdm<T>(phys: PhysAddr) -> *mut T {
    (phys.as_u64() + heap::hhdm_offset()) as *mut T
}

/// `2^order` fresh frames from the buddy, zeroed if `zero`; `None` when memory is exhausted.
fn take(order: usize, zero: bool) -> Option<PhysAddr> {
    #[cfg(test)]
    if FRAMES_LEFT.with(|left| left.replace(left.get().map(|n| n.saturating_sub(1))) == Some(0)) {
        return None;
    }
    let block = heap::buddy_alloc(order)?;
    if !zero {
        return Some(block);
    }
    // SAFETY: a block the buddy just handed out, `PAGE_SIZE << order` bytes the HHDM maps, which
    // nothing else reaches until its address is returned.
    unsafe { core::ptr::write_bytes(hhdm::<u8>(block), 0, PAGE_SIZE << order) };
    Some(block)
}

#[cfg(test)]
std::thread_local! {
    static FRAMES_LEFT: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// Let this thread take `n` more frames or blocks, failing every one after, or any with `None`.
#[cfg(test)]
pub fn fail_frames_after(n: Option<usize>) {
    FRAMES_LEFT.with(|left| left.set(n));
}

/// **A frame of RAM this value owns, freed as it drops**: not `Copy`, so freed once. A page table
/// mapping it takes it ([`into_pte`](Self::into_pte)); transparent, so a list lends its addresses.
#[derive(Debug)]
#[repr(transparent)]
pub struct Frame(PhysAddr);

impl Frame {
    /// A fresh frame, zeroed; `None` when memory is exhausted.
    pub fn alloc_zeroed() -> Option<Self> {
        take(0, true).map(Self)
    }

    /// A fresh frame holding whatever it held last, for a caller that writes all of it first.
    pub fn alloc() -> Option<Self> {
        take(0, false).map(Self)
    }

    /// Where the frame is.
    pub fn phys(&self) -> PhysAddr {
        self.0
    }

    /// The frame's bytes, for as long as it is borrowed.
    pub fn view(&self) -> FrameView<'_> {
        // SAFETY: a frame this value owns, so freed only once the borrow ends, and whose bytes
        // its owner lends only to views, to mappings and to devices.
        unsafe { FrameView::new(self.0) }
    }

    /// Hand the frame to the page table about to map it, which owns it from here.
    pub fn into_pte(self) -> PhysAddr {
        ManuallyDrop::new(self).0
    }

    /// Take back a frame a page table owned.
    ///
    /// # Safety
    /// [`into_pte`](Self::into_pte) handed `phys` over, it is mapped no longer, and this takes it back
    /// once; its drop frees it, so must wait until no CPU holds a stale translation to it.
    pub unsafe fn from_pte(phys: PhysAddr) -> Self {
        Self(phys)
    }

    /// The addresses of `frames`, for a device or a page table to borrow.
    pub fn addrs(frames: &[Frame]) -> &[PhysAddr] {
        // SAFETY: `Frame` is a transparent `PhysAddr`; the borrow keeps the frames.
        unsafe { core::slice::from_raw_parts(frames.as_ptr().cast(), frames.len()) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        // SAFETY: a frame `take` made at order 0, which this value alone owned.
        unsafe { heap::buddy_free(self.0, 0) }
    }
}

/// **`2^order` contiguous frames this value owns**, freed together, at their order, as it drops.
#[derive(Debug)]
pub struct FrameBlock {
    phys: PhysAddr,
    order: usize,
}

impl FrameBlock {
    /// `2^order` fresh frames, zeroed; `None` when memory is exhausted or `order` past the largest.
    pub fn alloc_zeroed(order: usize) -> Option<Self> {
        take(order, true).map(|phys| Self { phys, order })
    }

    /// Where the block begins.
    pub fn phys(&self) -> PhysAddr {
        self.phys
    }

    /// The block's order: it is `PAGE_SIZE << order` bytes.
    pub fn order(&self) -> usize {
        self.order
    }
}

impl Drop for FrameBlock {
    fn drop(&mut self) {
        // SAFETY: a block `take` made at `order`, which this value alone owned.
        unsafe { heap::buddy_free(self.phys, self.order) }
    }
}

/// One frame's bytes, lent by its owner for `'a`. **Every access is an aligned 8-byte atomic**
/// (a plain move on x86_64): a user mapping or another CPU may write the frame meanwhile, and a
/// reference to such bytes, a non-atomic copy, or atomics of two sizes over one word race.
#[derive(Clone, Copy)]
pub struct FrameView<'a> {
    base: *mut u8,
    owner: PhantomData<&'a ()>,
}

impl<'a> FrameView<'a> {
    /// A view of `frame`.
    ///
    /// # Safety
    /// `frame` is a page-aligned frame of RAM the HHDM maps, which nothing frees for `'a`, and
    /// whose bytes, for `'a`, nothing reaches but a view or a user mapping.
    pub unsafe fn new(frame: PhysAddr) -> Self {
        debug_assert!(frame.is_page_aligned(), "a frame view of {:#x}", frame.as_u64());
        Self { base: hhdm(frame), owner: PhantomData }
    }

    /// Copy the frame's bytes from `at` into `dst`. Panics past the frame's end.
    pub fn read(self, at: usize, dst: &mut [u8]) {
        in_frame(at, dst.len());
        for (w, bytes, from) in words(at, dst.len()) {
            let word = self.word(w).load(Ordering::Relaxed).to_ne_bytes();
            dst[from..from + bytes.len()].copy_from_slice(&word[bytes]);
        }
    }

    /// Copy `src` into the frame from `at`. Panics past the frame's end.
    pub fn write(self, at: usize, src: &[u8]) {
        in_frame(at, src.len());
        for (w, bytes, from) in words(at, src.len()) {
            let n = bytes.len();
            self.put(w, bytes, &src[from..from + n]);
        }
    }

    /// Set `len` bytes from `at` to `byte`. Panics past the frame's end.
    pub fn fill(self, at: usize, len: usize, byte: u8) {
        in_frame(at, len);
        for (w, bytes, _) in words(at, len) {
            let n = bytes.len();
            self.put(w, bytes, &[byte; WORD][..n]);
        }
    }

    /// Copy `len` bytes of `src` from `src_at` into this frame from `at`. Panics past either end.
    pub fn copy_from(self, at: usize, src: FrameView<'_>, src_at: usize, len: usize) {
        in_frame(at, len);
        in_frame(src_at, len);
        for (w, bytes, from) in words(at, len) {
            let mut piece = [0u8; WORD];
            let piece = &mut piece[..bytes.len()];
            src.read(src_at + from, piece);
            self.put(w, bytes, piece);
        }
    }

    /// Store `src` as bytes `bytes` of the word at `w`: whole, or merged by compare-and-swap into
    /// what the word holds, so another writer's bytes in it are not lost.
    fn put(self, w: usize, bytes: Range<usize>, src: &[u8]) {
        let word = self.word(w);
        if bytes.len() == WORD {
            word.store(u64::from_ne_bytes(src.try_into().expect("a word's bytes")), Ordering::Relaxed);
            return;
        }
        let merge = |old: u64| {
            let mut new = old.to_ne_bytes();
            new[bytes.clone()].copy_from_slice(src);
            u64::from_ne_bytes(new)
        };
        let mut old = word.load(Ordering::Relaxed);
        while let Err(now) = word.compare_exchange_weak(old, merge(old), Ordering::Relaxed, Ordering::Relaxed) {
            old = now;
        }
    }

    fn word(self, at: usize) -> &'a AtomicU64 {
        assert!(at.is_multiple_of(WORD) && at + WORD <= PAGE_SIZE, "a word at {at}");
        // SAFETY: an aligned word inside the frame, which `new`'s contract lends for `'a` and lets
        // nothing else reach but atomically.
        unsafe { AtomicU64::from_ptr(self.base.add(at).cast()) }
    }
}

fn in_frame(at: usize, len: usize) {
    assert!(at.checked_add(len).is_some_and(|end| end <= PAGE_SIZE), "{len} bytes at {at} run past a frame");
}

/// The words `len` bytes from `at` touch: each one's offset, the bytes of it they cover, and where
/// those begin among the `len`.
fn words(at: usize, len: usize) -> impl Iterator<Item = (usize, Range<usize>, usize)> {
    let end = at + len;
    (at - at % WORD..end)
        .step_by(WORD)
        .map(move |w| (w, at.max(w) - w..end.min(w + WORD) - w))
        .filter_map(move |(w, bytes)| (!bytes.is_empty()).then(|| (w, bytes.clone(), w + bytes.start - at)))
}

#[cfg(test)]
mod tests;
