//! The page tables, owned: [`KernelTables`], whose kernel half every root shares, and
//! [`UserTables`], an address space's own. A walk of either is safe: an unmap frees no table.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::Paging;
use crate::arch::abi::USER_VIRT_END;
use crate::arch::paging::{ArchPaging, MapError, PageFlags, UnmapError};
use crate::libkern::AllocError;
use crate::mm::frame::Frame;
use crate::mm::{PhysAddr, VirtAddr};

/// The boot root, never freed, as the buddy takes no table of the bootloader's.
pub struct KernelTables {
    root: AtomicU64,
}

static KERNEL: KernelTables = KernelTables { root: AtomicU64::new(0) };

impl KernelTables {
    /// Adopt `boot_root`, link the top-level tables over `tops`, then capture the half all share.
    /// # Safety
    /// `boot_root` is the live root, whose tables nothing frees; called once, at boot, on the one
    /// CPU running, before anything else reaches the kernel's tables.
    pub unsafe fn adopt(boot_root: PhysAddr, tops: impl Iterator<Item = VirtAddr>) {
        for va in tops {
            // SAFETY: the live boot root, which nothing else edits yet, per the contract.
            unsafe { Paging::ensure_kernel_intermediate(boot_root, va) }.expect("a kernel top-level table at boot");
        }
        // SAFETY: once, after every top-level table the kernel half needs, before any user half.
        unsafe { Paging::init_kernel_template(boot_root) };
        KERNEL.root.store(boot_root.as_u64(), Ordering::Release);
    }

    /// The kernel's tables. Panics before [`adopt`](Self::adopt).
    pub fn get() -> &'static Self {
        assert_ne!(KERNEL.root.load(Ordering::Acquire), 0, "the kernel's page tables before boot adopted them");
        &KERNEL
    }

    /// The root a CPU loads to run in no address space.
    pub fn root(&self) -> PhysAddr {
        PhysAddr::new(self.root.load(Ordering::Acquire))
    }

    /// Map `phys` at `va`, linking the tables its path lacks.
    /// # Safety
    /// `va` is kernel-half VA the caller owns, unmapped, and `phys` a frame or device page fit to
    /// map with `flags`, live for as long as it is mapped.
    pub unsafe fn map(&self, va: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        // SAFETY: the boot root, live for good; the rest is the caller's, forwarded.
        unsafe { Paging::map_page(self.root(), va, phys, flags) }
    }

    /// Unmap `va`, giving back what it mapped.
    /// # Safety
    /// `va`'s mapping is the caller's, and nothing uses it from here on.
    pub unsafe fn unmap(&self, va: VirtAddr) -> Result<PhysAddr, UnmapError> {
        // SAFETY: as `map`.
        unsafe { Paging::unmap_page(self.root(), va) }
    }
}

/// An address space's own top-level table, freed as it drops; the tables under it leak.
pub struct UserTables {
    root: PhysAddr,
}

impl UserTables {
    /// An empty user half under the kernel's. Panics before [`KernelTables::adopt`].
    pub fn new() -> Result<Self, AllocError> {
        // Handed over as every table is, so no view reaches it.
        let root = Frame::alloc_zeroed().ok_or(AllocError)?.into_pte();
        // SAFETY: a fresh table this value owns, which no CPU has loaded.
        unsafe { Paging::inherit_kernel_mappings(root) };
        Ok(Self { root })
    }

    /// The address a CPU loads to run in this space.
    pub fn root(&self) -> PhysAddr {
        self.root
    }

    /// The address `va` maps to, or `None`.
    pub fn translate(&self, va: VirtAddr) -> Option<PhysAddr> {
        // SAFETY: this value's tree, every table of which stays linked while it lives.
        unsafe { Paging::translate(self.root, va) }
    }

    /// The cache-attribute entry the mapping of `va` selects, or `None`.
    pub fn attribute_index(&self, va: VirtAddr) -> Option<u8> {
        // SAFETY: as `translate`.
        unsafe { Paging::attribute_index(self.root, va) }
    }

    /// Whether `va` is a 4 KiB page mapped with `flags` and nothing else.
    #[cfg(test)]
    pub fn mapped_with(&self, va: VirtAddr, flags: PageFlags) -> bool {
        // SAFETY: as `translate`.
        unsafe { crate::arch::mapped_with(self.root, va, flags) }
    }

    /// Map `frame` at `va`, the tables' from here, or hand it back with the error.
    pub fn map_owned(&mut self, va: VirtAddr, frame: Frame, flags: PageFlags) -> Result<(), (Frame, MapError)> {
        if refused(va, flags, true) {
            return Err((frame, MapError::NotUser));
        }
        // SAFETY: this value's tree, which `&mut self` alone edits; a frame it owned, at a user
        // page, mapped write-back as the direct map holds it.
        match unsafe { Paging::map_page(self.root, va, frame.phys(), flags) } {
            Ok(()) => {
                frame.into_pte();
                Ok(())
            }
            Err(e) => Err((frame, e)),
        }
    }

    /// Map `phys`, a frame its owner keeps: an object's or a file's.
    /// # Safety
    /// `phys` is RAM or a device page fit to map with `flags`, kept live by its owner until its
    /// mapping here is unmapped and shot down.
    pub unsafe fn map(&mut self, va: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        if refused(va, flags, false) {
            return Err(MapError::NotUser);
        }
        // SAFETY: this value's tree, which `&mut self` alone edits, at a user page; `phys` as the
        // caller says.
        unsafe { Paging::map_page(self.root, va, phys, flags) }
    }

    /// Unmap `va`, giving back what it mapped: a frame `map_owned` took is the caller's again.
    pub fn unmap(&mut self, va: VirtAddr) -> Result<PhysAddr, UnmapError> {
        if va.as_u64() >= USER_VIRT_END {
            return Err(UnmapError::NotUser);
        }
        // SAFETY: this value's tree, which `&mut self` alone edits, at a user page, which the
        // kernel reaches only by a copy that survives a fault; nothing is freed.
        unsafe { Paging::unmap_page(self.root, va) }
    }
}

impl Drop for UserTables {
    fn drop(&mut self) {
        // SAFETY: the table `new` handed over, taken back once: a CPU loads it only while it
        // lives, by `set_page_table`'s contract.
        drop(unsafe { Frame::from_pte(self.root) });
    }
}

/// What a user half refuses: a kernel address, a global mapping, or its own frame not write-back.
fn refused(va: VirtAddr, flags: PageFlags, owned: bool) -> bool {
    let cached = [PageFlags::NO_CACHE, PageFlags::WRITE_THROUGH, PageFlags::WRITE_COMBINING];
    va.as_u64() >= USER_VIRT_END
        || flags.contains(PageFlags::GLOBAL)
        || owned && cached.iter().any(|&c| flags.contains(c))
}

#[cfg(test)]
mod tests;
