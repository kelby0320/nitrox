//! `libheap` — the freeing userspace heap, the `#[global_allocator]` backing `extern crate alloc`
//! for `no_std` userspace. Design contract: `docs/architecture/libheap.md`.
//!
//! Small allocations (≤ [`SMALL_MAX`]) round up to a size class and are carved from **arenas**
//! (each a mapped `MemoryObject`); freed slots return to a per-class LIFO freelist, with no
//! coalescing. Large allocations get a dedicated mapping, unmapped on free.
//!
//! [`HeapEngine`] is the reusable allocator, generic over an [`ArenaSource`] so it is
//! host-testable with no kernel; [`Heap`] is the thin `GlobalAlloc` newtype forwarding to a
//! process-global engine. A future `std` port forwards to the same engine instead of taking
//! the single `#[global_allocator]` slot.
//!
//! ## Invariants
//!
//! - **No self-reentrancy.** Growing the heap (mapping an arena) uses only stack locals and
//!   syscalls; [`ArenaSource`] impls must not allocate through libheap.
//! - OOM surfaces as a null return (→ `alloc`'s `handle_alloc_error`), never a panic.

#![cfg_attr(not(test), no_std)]

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

/// Page size (bytes). Arenas and large mappings are page multiples.
pub const PAGE_SIZE: usize = 4096;

/// Small-allocation size classes (bytes), ascending powers of two: a request (rounded up
/// by alignment) at or below [`SMALL_MAX`] takes the smallest class that fits.
const SIZE_CLASSES: [usize; 8] = [16, 32, 64, 128, 256, 512, 1024, 2048];
/// The small/large boundary.
pub const SMALL_MAX: usize = 2048;

/// Bytes per arena: modest, as `MemoryObject` frames are **eager**, all allocated at create.
const ARENA_SIZE: usize = 64 * 1024;

/// Mirror of the kernel's `MemoryObject::MAX_SIZE`: a larger object is rejected at create.
const MEMOBJ_MAX_SIZE: usize = 16 * 1024 * 1024;

/// Marker in a large header, checked on free.
const LARGE_MAGIC: u32 = 0x4C48_4248; // "LHBH"

/// Round `x` up to a multiple of the power of two `align`; callers rule out overflow.
#[inline]
fn round_up(x: usize, align: usize) -> usize {
    (x + (align - 1)) & !(align - 1)
}

/// The smallest size-class index whose slot is `>= needed` (`needed <= SMALL_MAX`).
#[inline]
fn class_index(needed: usize) -> usize {
    let mut i = 0;
    while i < SIZE_CLASSES.len() {
        if SIZE_CLASSES[i] >= needed {
            return i;
        }
        i += 1;
    }
    SIZE_CLASSES.len() - 1
}

/// A backing mapping from an [`ArenaSource`], with the token needed to release it.
#[derive(Clone, Copy)]
pub struct Mapping {
    /// Base of the mapped region (page-aligned).
    pub ptr: *mut u8,
    /// Source-private release token (target: the `MemoryObject` handle).
    pub token: u64,
}

/// The heap's backing-memory provider: maps and unmaps page-aligned regions.
///
/// # Safety
/// `map` must return a region that is readable/writable for `size` bytes and stays
/// valid until the exact `(Mapping, size)` pair is passed to `unmap`. Impls must not
/// allocate through libheap (the no-self-reentrancy invariant).
pub unsafe trait ArenaSource {
    /// Map `size` bytes (a page multiple), page-aligned. `None` on failure.
    fn map(size: usize) -> Option<Mapping>;
    /// Release a mapping previously returned by [`map`](ArenaSource::map).
    ///
    /// # Safety
    /// `m`/`size` must be an exact, still-live pair from `map`, with no outstanding
    /// references into the region.
    unsafe fn unmap(m: Mapping, size: usize);
}

/// Intrusive per-arena header at the base of each arena, listing them for release.
#[repr(C)]
struct ArenaHeader {
    next: *mut ArenaHeader,
    token: u64,
    size: usize,
}

/// Large-allocation header, just below the returned pointer; holds the `unmap` arguments.
#[repr(C)]
struct LargeHeader {
    base: *mut u8,
    token: u64,
    map_len: usize,
    magic: u32,
}

/// The allocator state; reached only through [`HeapEngine`]'s lock.
struct Inner {
    /// Per-size-class freelist heads (a free slot stores the next pointer in its
    /// first 8 bytes; every class is `>= 16` bytes, so there is room).
    class_heads: [*mut u8; 8],
    /// The current arena's mapping, which every slot carved from it is derived from, so a slot's
    /// pointer carries the arena's provenance; null before the first arena maps.
    arena_base: *mut u8,
    /// Next carve address in the current arena (`0` before the first arena maps).
    arena_cur: usize,
    arena_end: usize,
    arena_list: *mut ArenaHeader,
}

impl Inner {
    const fn new() -> Self {
        Inner {
            class_heads: [core::ptr::null_mut(); 8],
            arena_base: core::ptr::null_mut(),
            arena_cur: 0,
            arena_end: 0,
            arena_list: core::ptr::null_mut(),
        }
    }
}

/// The reusable freeing allocator; its [`ArenaSource`] is a zero-sized marker, so it is
/// `const`-constructible as a `static`.
pub struct HeapEngine<S: ArenaSource> {
    /// Spinlock over [`Inner`]; uncontended while userspace is single-threaded.
    locked: AtomicBool,
    inner: UnsafeCell<Inner>,
    _src: PhantomData<S>,
}

// SAFETY: every access to `inner` goes through the spinlock (`lock`), which
// serialises concurrent callers; the raw pointers inside `Inner` are only
// dereferenced while the lock is held, all within one address space.
unsafe impl<S: ArenaSource> Sync for HeapEngine<S> {}

/// RAII lock guard: early returns can never leave the engine lock held.
struct Guard<'a, S: ArenaSource> {
    engine: &'a HeapEngine<S>,
}

impl<S: ArenaSource> Drop for Guard<'_, S> {
    fn drop(&mut self) {
        self.engine.locked.store(false, Ordering::Release);
    }
}

impl<S: ArenaSource> Guard<'_, S> {
    /// The locked state. `&mut self` so the borrow checker forbids aliasing `&mut`s.
    fn inner(&mut self) -> &mut Inner {
        // SAFETY: holding the guard means we hold the lock, so this is the unique
        // live reference to `inner`.
        unsafe { &mut *self.engine.inner.get() }
    }
}

impl<S: ArenaSource> HeapEngine<S> {
    /// A fresh engine; the first allocation maps an arena.
    pub const fn new() -> Self {
        HeapEngine { locked: AtomicBool::new(false), inner: UnsafeCell::new(Inner::new()), _src: PhantomData }
    }

    fn lock(&self) -> Guard<'_, S> {
        while self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        Guard { engine: self }
    }

    /// Allocate for `layout`. Returns null on OOM (→ `handle_alloc_error`).
    pub fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        // A slot of a power-of-two class `>= align` is `align`-aligned, so `max(size, align)`
        // picks a class (or the large path) satisfying both; `dealloc` recomputes it.
        let needed = layout.size().max(layout.align()).max(1);
        if needed > SMALL_MAX {
            return self.alloc_large(layout.size(), layout.align());
        }
        let class = class_index(needed);
        let mut g = self.lock();
        let inner = g.inner();
        let head = inner.class_heads[class];
        if !head.is_null() {
            // SAFETY: `head` is a live free slot of `>= 16` bytes we handed to the
            // freelist; its first word is the next link.
            let next = unsafe { *(head as *const *mut u8) };
            inner.class_heads[class] = next;
            return head;
        }
        self.carve(inner, SIZE_CLASSES[class])
    }

    /// Free `ptr`, which was returned by [`alloc`](Self::alloc) for `layout`.
    ///
    /// # Safety
    /// `ptr`/`layout` must be a matching pair from a prior `alloc`, not yet freed.
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        let needed = layout.size().max(layout.align()).max(1);
        if needed > SMALL_MAX {
            // SAFETY: forwarded matching pair; large path recovers the header.
            unsafe { self.dealloc_large(ptr) };
            return;
        }
        let class = class_index(needed);
        let mut g = self.lock();
        let inner = g.inner();
        // SAFETY: `ptr` is a live slot of this class (`>= 16` bytes); writing its
        // first word is in-bounds.
        unsafe { *(ptr as *mut *mut u8) = inner.class_heads[class] };
        inner.class_heads[class] = ptr;
    }

    /// Carve a `class_size` slot, mapping a new arena if it won't fit. Lock held. Null on OOM.
    fn carve(&self, inner: &mut Inner, class_size: usize) -> *mut u8 {
        let aligned = round_up(inner.arena_cur, class_size);
        // Past the arena's end, or no arena yet (cur == end == 0).
        if aligned.checked_add(class_size).is_none_or(|e| e > inner.arena_end) {
            if !self.grow(inner) {
                return core::ptr::null_mut();
            }
            let aligned = round_up(inner.arena_cur, class_size);
            if aligned + class_size > inner.arena_end {
                return core::ptr::null_mut();
            }
            inner.arena_cur = aligned + class_size;
            return inner.arena_base.with_addr(aligned);
        }
        inner.arena_cur = aligned + class_size;
        inner.arena_base.with_addr(aligned)
    }

    /// Map a new arena, make it current and link it into the list. Lock held.
    fn grow(&self, inner: &mut Inner) -> bool {
        let m = match S::map(ARENA_SIZE) {
            Some(m) => m,
            None => return false,
        };
        let base = m.ptr.addr();
        let hdr = m.ptr.cast::<ArenaHeader>();
        // SAFETY: `m.ptr` is a fresh writable mapping of `ARENA_SIZE >= size_of
        // ArenaHeader`; nothing else references it yet.
        unsafe {
            hdr.write(ArenaHeader { next: inner.arena_list, token: m.token, size: ARENA_SIZE });
        }
        inner.arena_list = hdr;
        inner.arena_base = m.ptr;
        inner.arena_cur = base + size_of::<ArenaHeader>();
        inner.arena_end = base + ARENA_SIZE;
        true
    }

    /// Serve a large allocation from its own mapping. Lock-free: no shared state is touched.
    fn alloc_large(&self, size: usize, align: usize) -> *mut u8 {
        let hdr = size_of::<LargeHeader>();
        let raw = match hdr.checked_add(align).and_then(|v| v.checked_add(size)) {
            Some(v) => v,
            None => return core::ptr::null_mut(),
        };
        let map_len = round_up(raw, PAGE_SIZE);
        if map_len > MEMOBJ_MAX_SIZE {
            return core::ptr::null_mut();
        }
        let m = match S::map(map_len) {
            Some(m) => m,
            None => return core::ptr::null_mut(),
        };
        // Aligned user pointer with at least `hdr` bytes of header room below it.
        let p = round_up(m.ptr.addr() + hdr, align);
        // SAFETY: `p - hdr >= base` and `p + size <= base + map_len` (map_len bounds
        // hdr + align + size), so the header write is inside the fresh mapping.
        unsafe {
            m.ptr.with_addr(p - hdr).cast::<LargeHeader>().write(LargeHeader {
                base: m.ptr,
                token: m.token,
                map_len,
                magic: LARGE_MAGIC,
            });
        }
        m.ptr.with_addr(p)
    }

    /// Free a large allocation: recover its header and unmap.
    ///
    /// # Safety
    /// `ptr` must be a live pointer from [`alloc_large`](Self::alloc_large).
    unsafe fn dealloc_large(&self, ptr: *mut u8) {
        let hdr_size = size_of::<LargeHeader>();
        // SAFETY: `alloc_large` wrote a `LargeHeader` at `ptr - hdr_size`.
        let hdr = unsafe { &*ptr.map_addr(|a| a - hdr_size).cast::<LargeHeader>() };
        debug_assert_eq!(hdr.magic, LARGE_MAGIC, "libheap: large free corruption");
        let m = Mapping { ptr: hdr.base, token: hdr.token };
        // SAFETY: `m`/`map_len` are the exact pair `alloc_large` mapped.
        unsafe { S::unmap(m, hdr.map_len) };
    }
}

impl<S: ArenaSource> Default for HeapEngine<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: ArenaSource> Drop for HeapEngine<S> {
    /// Release every arena. Large allocations are released by `dealloc`, not here.
    fn drop(&mut self) {
        let mut a = self.inner.get_mut().arena_list;
        while !a.is_null() {
            // SAFETY: every node was written by `grow` into live arena memory.
            let hdr = unsafe { &*a };
            let (next, token, size) = (hdr.next, hdr.token, hdr.size);
            // SAFETY: `(base, size)` is the exact pair `grow` mapped for this arena.
            unsafe {
                S::unmap(Mapping { ptr: a.cast::<u8>(), token }, size);
            }
            a = next;
        }
    }
}

// Target backing and the `#[global_allocator]` registration (bare build only).

/// The target [`ArenaSource`]: backs arenas with `MemoryObject`s via `libkern`.
#[cfg(not(test))]
pub struct SyscallSource;

#[cfg(not(test))]
// SAFETY: `map` returns a kernel-mapped, read/write region valid until `unmap`
// unmaps it and closes the backing object; it issues only raw syscalls (no heap use).
unsafe impl ArenaSource for SyscallSource {
    fn map(size: usize) -> Option<Mapping> {
        // SAFETY: `sys_memory_create(size, flags=0)` — a valid create; returns a
        // handle (>= 0) or a negative error.
        let h = unsafe { libkern::syscall2(libkern::SYS_MEMORY_CREATE, size as u64, 0) };
        if h < 0 {
            return None;
        }
        let rights = libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE;
        // SAFETY: map the just-created object read/write, kernel-chosen address
        // (hint = 0). Returns the mapped address or a negative error.
        let addr = unsafe { libkern::syscall4(libkern::SYS_MEMORY_MAP, h as u64, 0, size as u64, rights) };
        if addr < 0 {
            // SAFETY: `h` is the handle we just created.
            unsafe { libkern::syscall1(libkern::SYS_HANDLE_CLOSE, h as u64) };
            return None;
        }
        // The one place an address becomes a pointer: the kernel's mapping, which no Rust
        // allocation gave.
        Some(Mapping { ptr: core::ptr::with_exposed_provenance_mut(addr as usize), token: h as u64 })
    }

    unsafe fn unmap(m: Mapping, size: usize) {
        // SAFETY: unmap the region then close the backing object — unmap-before-close
        // so no mapping outlives the object's frames.
        unsafe {
            libkern::syscall2(libkern::SYS_MEMORY_UNMAP, m.ptr.addr() as u64, size as u64);
            libkern::syscall1(libkern::SYS_HANDLE_CLOSE, m.token);
        }
    }
}

#[cfg(not(test))]
static ENGINE: HeapEngine<SyscallSource> = HeapEngine::new();

/// The `#[global_allocator]` registration: a zero-sized newtype forwarding to
/// [`ENGINE`]. Register it in a bare userspace crate with
/// `#[global_allocator] static A: libheap::Heap = libheap::Heap;`.
#[cfg(not(test))]
pub struct Heap;

#[cfg(not(test))]
// SAFETY: forwards to a correctly-synchronised engine; returns null on OOM and never
// unwinds, satisfying the `GlobalAlloc` contract.
unsafe impl core::alloc::GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        ENGINE.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: core::alloc::Layout) {
        // SAFETY: forwarded matching (ptr, layout) pair from a prior `alloc`.
        unsafe { ENGINE.dealloc(ptr, layout) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    /// Host [`ArenaSource`]: page-aligned `std::alloc` regions.
    struct TestSource;
    // SAFETY: `std::alloc` returns a valid region for `size` bytes, page-aligned,
    // valid until `dealloc` with the same layout.
    unsafe impl ArenaSource for TestSource {
        fn map(size: usize) -> Option<Mapping> {
            let layout = std::alloc::Layout::from_size_align(size, PAGE_SIZE).ok()?;
            // SAFETY: non-zero `size` (arenas/large maps are always > 0).
            let p = unsafe { std::alloc::alloc(layout) };
            if p.is_null() { None } else { Some(Mapping { ptr: p, token: 0 }) }
        }
        unsafe fn unmap(m: Mapping, size: usize) {
            let layout = std::alloc::Layout::from_size_align(size, PAGE_SIZE).unwrap();
            // SAFETY: `m.ptr`/`layout` match the `map` allocation.
            unsafe { std::alloc::dealloc(m.ptr, layout) };
        }
    }

    fn engine() -> HeapEngine<TestSource> {
        HeapEngine::new()
    }

    // TODO(miri-provenance-tests): it fails only under Miri, which no CI job runs yet.
    #[test]
    fn basic_alloc_is_nonnull_aligned_writable() {
        let e = engine();
        let l = Layout::from_size_align(24, 8).unwrap();
        let p = e.alloc(l);
        assert!(!p.is_null());
        assert_eq!(p as usize % 8, 0);
        // SAFETY: `p` is a live 24-byte allocation from `alloc` above, referenced by nothing else.
        unsafe { core::ptr::write_bytes(p, 0xAB, 24) };
        // SAFETY: `p` is live and its first byte was just written.
        assert_eq!(unsafe { *p }, 0xAB);
        // SAFETY: `p` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(p, l) };
    }

    #[test]
    fn small_requests_round_to_size_classes() {
        let e = engine();
        for &sz in &[1usize, 8, 16] {
            let l = Layout::from_size_align(sz, 1).unwrap();
            let p = e.alloc(l);
            assert!(!p.is_null());
            // SAFETY: `p` came from `e.alloc(l)` in this loop, with this `l`, and is freed only here.
            unsafe { e.dealloc(p, l) };
        }
    }

    #[test]
    fn free_then_alloc_same_class_reuses_slot() {
        let e = engine();
        let l = Layout::from_size_align(40, 8).unwrap(); // 64-byte class
        let a = e.alloc(l);
        // SAFETY: `a` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(a, l) };
        let b = e.alloc(l);
        assert_eq!(a, b, "LIFO freelist should hand back the just-freed slot");
        // SAFETY: `b` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(b, l) };
    }

    #[test]
    fn honors_large_alignment_via_class_promotion() {
        let e = engine();
        let l = Layout::from_size_align(16, 64).unwrap();
        let p = e.alloc(l);
        assert!(!p.is_null());
        assert_eq!(p as usize % 64, 0);
        // SAFETY: `p` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(p, l) };
    }

    #[test]
    fn many_allocations_grow_arenas_without_overlap() {
        let e = engine();
        let l = Layout::from_size_align(2048, 8).unwrap();
        // About 31 such slots fit a 64 KiB arena, so 200 forces several.
        let n = 200usize;
        let mut ptrs = std::vec::Vec::with_capacity(n);
        for i in 0..n {
            let p = e.alloc(l);
            assert!(!p.is_null(), "alloc {i} failed");
            // SAFETY: `p` is non-null (asserted above) and points at the start of a live 2048-byte slot.
            unsafe { *p = (i & 0xFF) as u8 };
            ptrs.push(p);
        }
        for (i, &p) in ptrs.iter().enumerate() {
            // SAFETY: `p` is a slot from the allocations above, live until the free loop below.
            assert_eq!(unsafe { *p }, (i & 0xFF) as u8, "slot {i} overlapped/corrupted");
        }
        for &p in &ptrs {
            // SAFETY: `p` came from `e.alloc(l)` above, with this `l`, and is freed only here.
            unsafe { e.dealloc(p, l) };
        }
    }

    #[test]
    fn large_allocation_roundtrips_and_frees() {
        let e = engine();
        let l = Layout::from_size_align(5000, 8).unwrap(); // > SMALL_MAX -> large path
        let p = e.alloc(l);
        assert!(!p.is_null());
        assert_eq!(p as usize % 8, 0);
        // SAFETY: `p` is a live 5000-byte allocation, and the write stays within it.
        unsafe { core::ptr::write_bytes(p, 0xCD, 5000) };
        // SAFETY: byte 4999 is the last byte of the live 5000-byte allocation.
        assert_eq!(unsafe { *p.add(4999) }, 0xCD);
        // SAFETY: `p` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(p, l) }; // must unmap without crashing
    }

    #[test]
    fn large_allocation_honors_page_alignment() {
        let e = engine();
        let l = Layout::from_size_align(9000, PAGE_SIZE).unwrap();
        let p = e.alloc(l);
        assert!(!p.is_null());
        assert_eq!(p as usize % PAGE_SIZE, 0);
        // SAFETY: `p` came from `e.alloc(l)` with this `l` and is freed only here.
        unsafe { e.dealloc(p, l) };
    }

    #[test]
    fn distinct_large_allocations_do_not_overlap() {
        let e = engine();
        let l = Layout::from_size_align(3000, 8).unwrap();
        let a = e.alloc(l);
        let b = e.alloc(l);
        assert!(!a.is_null() && !b.is_null());
        // SAFETY: `a` and `b` are distinct live 3000-byte allocations, written and read within their extents, and each is freed once.
        unsafe {
            core::ptr::write_bytes(a, 0x11, 3000);
            core::ptr::write_bytes(b, 0x22, 3000);
            assert_eq!(*a, 0x11);
            assert_eq!(*b, 0x22);
            e.dealloc(a, l);
            e.dealloc(b, l);
        }
    }

    #[test]
    fn class_index_picks_smallest_fitting_class() {
        assert_eq!(class_index(1), 0); // 16
        assert_eq!(class_index(16), 0); // 16
        assert_eq!(class_index(17), 1); // 32
        assert_eq!(class_index(2048), 7); // 2048
    }
}
