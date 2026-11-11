//! A frame's bytes, through the HHDM: a [`FrameView`] its owner lends, and [`hhdm`] for memory no
//! owner lends (firmware tables, page tables, an I/O request's fragments).

use core::marker::PhantomData;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use crate::mm::{PAGE_SIZE, PhysAddr, heap};

const WORD: usize = size_of::<u64>();

/// Where the kernel reaches physical memory `phys`; reaching through it is the caller's `unsafe`.
pub fn hhdm<T>(phys: PhysAddr) -> *mut T {
    (phys.as_u64() + heap::hhdm_offset()) as *mut T
}

/// `2^order` fresh frames from the buddy, zeroed; `None` when memory is exhausted.
pub fn alloc_zeroed(order: usize) -> Option<PhysAddr> {
    let block = heap::buddy_alloc(order)?;
    // SAFETY: a block the buddy just handed out, `PAGE_SIZE << order` bytes the HHDM maps, which
    // nothing else reaches until its address is returned.
    unsafe { core::ptr::write_bytes(hhdm::<u8>(block), 0, PAGE_SIZE << order) };
    Some(block)
}

/// One frame's bytes, lent by its owner for `'a`, **copied in relaxed atomic words** (plain moves on
/// x86_64): a user mapping or another CPU may write them meanwhile, and a reference to such bytes,
/// or a non-atomic copy of them, is a data race.
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
        walk(
            dst.len(),
            |i| (at + i).is_multiple_of(WORD),
            |i, word| match word {
                true => dst[i..i + WORD].copy_from_slice(&self.word(at + i).load(Ordering::Relaxed).to_ne_bytes()),
                false => dst[i] = self.byte(at + i).load(Ordering::Relaxed),
            },
        );
    }

    /// Copy `src` into the frame from `at`. Panics past the frame's end.
    pub fn write(self, at: usize, src: &[u8]) {
        in_frame(at, src.len());
        walk(
            src.len(),
            |i| (at + i).is_multiple_of(WORD),
            |i, word| match word {
                true => self.word(at + i).store(u64::from_ne_bytes(word_of(&src[i..])), Ordering::Relaxed),
                false => self.byte(at + i).store(src[i], Ordering::Relaxed),
            },
        );
    }

    /// Set `len` bytes from `at` to `byte`. Panics past the frame's end.
    pub fn fill(self, at: usize, len: usize, byte: u8) {
        in_frame(at, len);
        let whole = u64::from_ne_bytes([byte; WORD]);
        walk(
            len,
            |i| (at + i).is_multiple_of(WORD),
            |i, word| match word {
                true => self.word(at + i).store(whole, Ordering::Relaxed),
                false => self.byte(at + i).store(byte, Ordering::Relaxed),
            },
        );
    }

    /// Copy `len` bytes of `src` from `src_at` into this frame from `at`. Panics past either end.
    pub fn copy_from(self, at: usize, src: FrameView<'_>, src_at: usize, len: usize) {
        in_frame(at, len);
        in_frame(src_at, len);
        walk(
            len,
            |i| (at + i).is_multiple_of(WORD) && (src_at + i).is_multiple_of(WORD),
            |i, word| match word {
                true => self.word(at + i).store(src.word(src_at + i).load(Ordering::Relaxed), Ordering::Relaxed),
                false => self.byte(at + i).store(src.byte(src_at + i).load(Ordering::Relaxed), Ordering::Relaxed),
            },
        );
    }

    fn word(self, at: usize) -> &'a AtomicU64 {
        assert!(at.is_multiple_of(WORD) && at + WORD <= PAGE_SIZE, "a word at {at}");
        // SAFETY: an aligned word inside the frame, which `new`'s contract lends for `'a` and lets
        // nothing else reach but atomically.
        unsafe { AtomicU64::from_ptr(self.base.add(at).cast()) }
    }

    fn byte(self, at: usize) -> &'a AtomicU8 {
        assert!(at < PAGE_SIZE, "a byte at {at}");
        // SAFETY: as `word`'s, for a byte.
        unsafe { AtomicU8::from_ptr(self.base.add(at)) }
    }
}

fn in_frame(at: usize, len: usize) {
    assert!(at.checked_add(len).is_some_and(|end| end <= PAGE_SIZE), "{len} bytes at {at} run past a frame");
}

/// Hand `step` each offset of `len` bytes, and whether a word moves from it: where `aligned`, if one fits.
fn walk(len: usize, aligned: impl Fn(usize) -> bool, mut step: impl FnMut(usize, bool)) {
    let mut i = 0;
    while i < len {
        let word = len - i >= WORD && aligned(i);
        step(i, word);
        i += if word { WORD } else { 1 };
    }
}

fn word_of(bytes: &[u8]) -> [u8; WORD] {
    bytes[..WORD].try_into().expect("a word's bytes")
}

#[cfg(test)]
mod tests;
