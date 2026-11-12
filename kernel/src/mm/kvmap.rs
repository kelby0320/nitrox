//! Kernel-half virtual address allocator (the "kernel vmap").
//!
//! VA, reused only as [`VmapFrames`] give it back, over the vmap region (16 TiB; layout in
//! `docs/architecture/overview.md`), for kernel-half mappings the boot path did not pre-establish.
//!
//! A vmap mapping must be visible under every address space (a stack allocated under one AS is
//! used while another is loaded), via
//! [Kernel-half PML4 sharing](../../docs/architecture/memory-management.md#kernel-half-pml4-sharing):
//! [`window`] is linked at boot **before** `init_kernel_template`, pre-allocating the vmap's top-level
//! intermediate page tables, the template snapshots those PML4 entries, and every
//! `AddressSpace::new` inherits them. Later `map_page` calls modify a PD/PT chain under the shared
//! PDPTs, visible to every AS with no shootdown. So VA is handed out only in that window, the
//! tables are edited through the boot root alone, and every unmap is shot down on every CPU.

use crate::arch::paging::PageFlags;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, SpinLock};
use crate::mm::frame::Frame;
use crate::mm::page_tables::KernelTables;
use crate::mm::{Mmio, PAGE_SIZE, PhysAddr, VirtAddr};

/// Inclusive lower bound of the kernel vmap region (`docs/architecture/overview.md`).
pub const KERNEL_VMAP_START: u64 = 0xFFFF_C000_0000_0000;
/// Exclusive upper bound of the kernel vmap region (16 TiB).
pub const KERNEL_VMAP_END: u64 = 0xFFFF_D000_0000_0000;

const PDPT_SPAN: u64 = 512 << 30;
/// PDPTs [`KernelTables::adopt`] links, from the region's start.
const WINDOW_PDPTS: u64 = 1;
/// Exclusive upper bound of the window, the part of the region every root shares.
pub const VMAP_WINDOW_END: u64 = KERNEL_VMAP_START + WINDOW_PDPTS * PDPT_SPAN;
const _: () = assert!(VMAP_WINDOW_END <= KERNEL_VMAP_END);

/// Never nested, and nothing allocates or frees under it; lock rank 6d.
static VMAP: SpinLock<Vmap> = SpinLock::new(LockRank::KernelVmap, Vmap::new(KERNEL_VMAP_START, VMAP_WINDOW_END));

struct Vmap {
    next: u64,
    end: u64,
    free: Option<KBox<Slot>>,
}

/// A reusable range, allocated with it so that giving it back allocates nothing.
struct Slot {
    start: u64,
    pages: u64,
    next: Option<KBox<Slot>>,
}

impl Vmap {
    const fn new(start: u64, end: u64) -> Self {
        Self { next: start, end, free: None }
    }

    fn bump(&mut self, pages: u64) -> Result<u64, AllocError> {
        let bytes = pages.checked_mul(PAGE_SIZE as u64).ok_or(AllocError)?;
        let end = self.next.checked_add(bytes).ok_or(AllocError)?;
        if end > self.end {
            return Err(AllocError);
        }
        Ok(core::mem::replace(&mut self.next, end))
    }

    /// A range given back, or `spare` filled; an unused spare comes back, to be freed unlocked.
    fn take(&mut self, mut spare: KBox<Slot>) -> (Result<KBox<Slot>, AllocError>, Option<KBox<Slot>>) {
        if let Some(reused) = unlink(&mut self.free, spare.pages) {
            return (Ok(reused), Some(spare));
        }
        match self.bump(spare.pages) {
            Ok(start) => {
                spare.start = start;
                (Ok(spare), None)
            }
            Err(e) => (Err(e), Some(spare)),
        }
    }

    /// Only once no CPU may hold a translation for `slot`.
    fn give_back(&mut self, mut slot: KBox<Slot>) {
        slot.next = self.free.take();
        self.free = Some(slot);
    }
}

fn unlink(mut list: &mut Option<KBox<Slot>>, pages: u64) -> Option<KBox<Slot>> {
    loop {
        match list {
            None => return None,
            Some(node) if node.pages == pages => break,
            Some(node) => list = &mut node.next,
        }
    }
    let mut node = list.take()?;
    *list = node.next.take();
    Some(node)
}

/// The window's top-level tables, which [`KernelTables::adopt`] links for every root to share.
pub fn window() -> impl Iterator<Item = VirtAddr> {
    (0..WINDOW_PDPTS).map(|i| VirtAddr::new(KERNEL_VMAP_START + i * PDPT_SPAN))
}

fn page(base: u64, i: u64) -> VirtAddr {
    VirtAddr::new(base + i * PAGE_SIZE as u64)
}

/// Map `frames` from `base`, all or none.
///
/// # Safety
/// `[base, base + frames.len() pages)` is vmap VA the caller owns, unmapped, and each frame is
/// safe to map with `flags`.
unsafe fn map_all(base: u64, frames: impl Iterator<Item = PhysAddr>, flags: PageFlags) -> Result<(), AllocError> {
    for (mapped, phys) in (0..).zip(frames) {
        // SAFETY: forwarded; the window's PDPTs are linked, so only shared PDs and PTs are made.
        if unsafe { KernelTables::get().map(page(base, mapped), phys, flags) }.is_err() {
            // SAFETY: the pages just mapped, which nothing has seen.
            unsafe { unmap_all(base, mapped) };
            return Err(AllocError);
        }
    }
    Ok(())
}

/// # Safety
/// The range is vmap VA the caller owns and nothing uses from here on.
unsafe fn unmap_all(base: u64, pages: u64) {
    for i in 0..pages {
        // SAFETY: forwarded from this function's contract.
        let _ = unsafe { KernelTables::get().unmap(page(base, i)) };
    }
    crate::tlb::shootdown_all();
}

/// Map `n_pages` of device memory from `phys` with `flags`, **permanently**.
///
/// # Safety
/// `phys .. phys + n_pages pages` must be a real device range safe to map with `flags`, owned by
/// the caller for as long as the mapping is used.
pub unsafe fn map_device(phys: PhysAddr, n_pages: u64, flags: PageFlags) -> Result<VirtAddr, AllocError> {
    let base = VMAP.lock().bump(n_pages)?;
    let frames = (0..n_pages).map(|i| PhysAddr::new(phys.as_u64() + i * PAGE_SIZE as u64));
    // SAFETY: fresh VA; the frames are the caller's device range, per the contract.
    unsafe { map_all(base, frames, flags)? };
    Ok(VirtAddr::new(base))
}

/// [`map_device`], **uncached** and writable: the `len` bytes of registers from `phys`, which
/// need not be page-aligned, for good.
///
/// # Safety
/// As [`map_device`], for the pages `phys .. phys + len` touch.
pub unsafe fn map_mmio(phys: PhysAddr, len: u64) -> Result<Mmio<'static>, AllocError> {
    let lead = phys.as_u64() % PAGE_SIZE as u64;
    let pages = (lead + len).div_ceil(PAGE_SIZE as u64).max(1);
    // SAFETY: forwarded.
    let va =
        unsafe { map_device(PhysAddr::new(phys.as_u64() - lead), pages, PageFlags::WRITABLE | PageFlags::NO_CACHE)? };
    // SAFETY: the registers just mapped, which a device mapping keeps for good.
    Ok(unsafe { Mmio::new(VirtAddr::new(va.as_u64() + lead), len) })
}

/// Undo [`map_device`]: clear the mapping and invalidate it on every CPU. The VA is not reused.
///
/// # Safety
/// `base .. base + n_pages pages` must be a mapping [`map_device`] returned, unused from here on.
pub unsafe fn unmap_device(base: VirtAddr, n_pages: u64) {
    // SAFETY: forwarded from this function's contract.
    unsafe { unmap_all(base.as_u64(), n_pages) };
}

/// One vmap page pointed at device frames in turn, uncached: a scan window.
pub struct MmioWindow {
    va: VirtAddr,
    mapped: bool,
}

impl MmioWindow {
    /// Reserve the page, mapped to nothing yet.
    pub fn new() -> Result<Self, AllocError> {
        Ok(Self { va: VirtAddr::new(VMAP.lock().bump(1)?), mapped: false })
    }

    /// Point the window at the device frame `phys`, invalidating the old translation on every CPU:
    /// its registers, until the window is pointed again.
    ///
    /// # Safety
    /// `phys` must be a real MMIO frame safe to map uncached.
    pub unsafe fn repoint(&mut self, phys: PhysAddr) -> Result<Mmio<'_>, AllocError> {
        if core::mem::take(&mut self.mapped) {
            // SAFETY: the window's own page, mapped by the previous repoint.
            let _ = unsafe { KernelTables::get().unmap(self.va) };
            crate::tlb::shootdown_page(self.va);
        }
        // SAFETY: the window's own page, unmapped; `phys` is a device frame, per the contract.
        unsafe { map_all(self.va.as_u64(), core::iter::once(phys), PageFlags::WRITABLE | PageFlags::NO_CACHE)? };
        self.mapped = true;
        // SAFETY: the page just mapped to `phys`, which only the next repoint, needing `&mut self`
        // and so this borrow's end, unmaps.
        Ok(unsafe { Mmio::new(self.va, PAGE_SIZE as u64) })
    }
}

/// `N` frames of the kernel's own, mapped into the vmap above `guard` unmapped pages. Dropping it
/// clears the mapping, invalidates it on every CPU, then frees the frames and gives the VA back.
pub struct VmapFrames<const N: usize> {
    base: VirtAddr,
    /// Handed to the mapping by [`Frame::into_pte`], so no view reaches what the kernel uses it for.
    frames: [PhysAddr; N],
    slot: Option<KBox<Slot>>,
}

impl<const N: usize> VmapFrames<N> {
    /// Allocate `N` frames and map them with `flags` above `guard` unmapped pages.
    pub fn new(flags: PageFlags, guard: u64) -> Result<Self, AllocError> {
        let spare = KBox::try_new(Slot { start: 0, pages: guard + N as u64, next: None })?;
        let (slot, unused) = VMAP.lock().take(spare);
        drop(unused);
        let slot = slot?;
        let base = VirtAddr::new(slot.start + guard * PAGE_SIZE as u64);
        let frames = core::array::from_fn::<_, N, _>(|_| Frame::alloc());
        let phys = frames.iter().flatten().map(Frame::phys);
        // SAFETY: the slot's VA is this range's alone, fresh or given back after its shootdown.
        let mapped = frames.iter().all(Option::is_some) && unsafe { map_all(base.as_u64(), phys, flags) }.is_ok();
        if !mapped {
            VMAP.lock().give_back(slot);
            return Err(AllocError);
        }
        Ok(Self { base, slot: Some(slot), frames: frames.map(|f| f.expect("every frame, checked above").into_pte()) })
    }

    /// The first mapped byte; the guard pages are below it.
    pub fn base(&self) -> VirtAddr {
        self.base
    }

    /// One past the last mapped byte.
    pub fn end(&self) -> VirtAddr {
        page(self.base.as_u64(), N as u64)
    }

    /// The first reserved byte: the guard pages' base, or [`Self::base`] with none.
    pub fn guard_base(&self) -> VirtAddr {
        self.slot.as_ref().map_or(self.base, |s| VirtAddr::new(s.start))
    }
}

impl<const N: usize> Drop for VmapFrames<N> {
    fn drop(&mut self) {
        // SAFETY: this range's own mapping, which its owner uses no longer.
        unsafe { unmap_all(self.base.as_u64(), N as u64) };
        for &phys in &self.frames {
            // SAFETY: a frame `new` handed the mapping, cleared and shot down above.
            drop(unsafe { Frame::from_pte(phys) });
        }
        if let Some(slot) = self.slot.take() {
            VMAP.lock().give_back(slot);
        }
    }
}

#[cfg(test)]
mod tests;
