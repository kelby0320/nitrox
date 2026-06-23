//! A fixed static-arena bump allocator — init's `#[global_allocator]`.
//!
//! Init runs before the userspace runtime (no `libos`, hence no real allocator),
//! but needs `alloc` for bounded work (a handful of mount specs + TOML strings).
//! A fixed arena with bump allocation is the right shape: init's working set is
//! bounded and the whole arena is reclaimed when the process exits. `dealloc` is
//! a no-op, so `Vec` reallocation leaks within the arena — fine for init's small,
//! one-shot workload; not a general allocator.
//!
//! The pure offset math ([`bump`]) is host-tested; the [`BumpAlloc`] `GlobalAlloc`
//! impl is exercised under QEMU (and indirectly by the bin's alloc proof).

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Arena size. Bounded and sized generously for the bootstrapping slice; revisit
/// if init's working set grows (the open question flagged in the slice-4 plan).
pub const ARENA_SIZE: usize = 64 * 1024;

/// Compute the `[off, end)` byte offsets (relative to the arena base) for an
/// allocation of `size`/`align` starting at byte cursor `cur`, within an arena of
/// `arena_size` bytes based at absolute address `base_addr`. `None` on overflow or
/// exhaustion. `align` must be a power of two (guaranteed by [`Layout`]).
///
/// Pure — the host-testable core of [`BumpAlloc::alloc`].
pub fn bump(
    base_addr: usize,
    cur: usize,
    size: usize,
    align: usize,
    arena_size: usize,
) -> Option<(usize, usize)> {
    let abs = base_addr.checked_add(cur)?;
    // Round the absolute address up to `align`, then back to an arena offset.
    let aligned = abs.checked_add(align - 1)? & !(align - 1);
    let off = aligned - base_addr;
    let end = off.checked_add(size)?;
    if end > arena_size {
        return None;
    }
    Some((off, end))
}

/// 16-byte-aligned arena storage. The bytes are only ever reached through the
/// allocator's raw pointer (`&raw mut ARENA`), never the named field, hence
/// `allow(dead_code)`.
#[repr(align(16))]
#[allow(dead_code)]
struct Arena([u8; ARENA_SIZE]);

static mut ARENA: Arena = Arena([0u8; ARENA_SIZE]);
/// Byte cursor into [`ARENA`]; the next free offset. Atomic so the impl is `Sync`
/// without `unsafe` (init is single-threaded, but `GlobalAlloc` requires `Sync`).
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// init's global allocator: bump allocation over [`ARENA`]; `dealloc` is a no-op.
pub struct BumpAlloc;

unsafe impl GlobalAlloc for BumpAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = (&raw mut ARENA) as *mut u8;
        let base_addr = base as usize;
        let mut cur = NEXT.load(Ordering::Relaxed);
        loop {
            let Some((off, end)) =
                bump(base_addr, cur, layout.size(), layout.align(), ARENA_SIZE)
            else {
                return core::ptr::null_mut();
            };
            match NEXT.compare_exchange_weak(cur, end, Ordering::Relaxed, Ordering::Relaxed) {
                // SAFETY: `bump` bounded `off` to within the arena, so `base.add(off)`
                // is in-bounds of the `ARENA_SIZE` allocation.
                Ok(_) => return unsafe { base.add(off) },
                Err(actual) => cur = actual,
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator: no per-allocation free. The arena is reclaimed wholesale
        // when init exits; its working set is bounded, so this is sound by design.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_and_advances() {
        // base 0 keeps the arithmetic obvious.
        assert_eq!(bump(0, 0, 8, 8, 1024), Some((0, 8)));
        assert_eq!(bump(0, 1, 1, 1, 1024), Some((1, 2)));
        // cursor 8, align 16 -> rounds up to 16.
        assert_eq!(bump(0, 8, 16, 16, 1024), Some((16, 32)));
    }

    #[test]
    fn alignment_accounts_for_base_address() {
        // base 0x1000 + cur 3 = 0x1003; align 16 -> 0x1010 -> off 0x10.
        assert_eq!(bump(0x1000, 3, 4, 16, 4096), Some((0x10, 0x14)));
    }

    #[test]
    fn exhaustion_returns_none_not_panic() {
        assert_eq!(bump(0, 1000, 100, 1, 1024), None); // 1100 > 1024
        assert_eq!(bump(0, 0, 1025, 1, 1024), None);
        // An allocation that exactly fills the arena still succeeds.
        assert_eq!(bump(0, 0, 1024, 1, 1024), Some((0, 1024)));
    }
}
