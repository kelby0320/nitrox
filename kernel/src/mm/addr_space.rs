//! Per-process virtual address space.
//!
//! [`AddressSpace`] pairs a [`VmaTree`] (per-AS catalogue of virtual
//! memory areas) with a top-level page-table root (the PML4 base on
//! x86_64) under a single [`SpinLock`]. It is the bridge between the
//! VMM's bookkeeping (the tree) and the hardware MMU's actual
//! translations (the page tables): `map_vma` updates both atomically;
//! `unmap_covering` does the inverse.
//!
//! ## What this layer is for
//!
//! Every process gets exactly one `AddressSpace`. Today nothing
//! *consumes* one — there is no scheduler, no first userspace process —
//! but the address-spaces-and-paging slice needs to deliver something
//! that can actually represent a process's memory. Without
//! `AddressSpace`, the [`VmaTree`] is structurally testable but
//! disconnected from any real translation; with it, a `Vma` going into
//! the tree is the same operation as PTEs going into the hardware.
//!
//! ## Phase 1 limitations
//!
//! - **No kernel-half mapping yet.** A fresh `AddressSpace` has an
//!   all-zero PML4: switching `CR3` to its root would triple-fault
//!   immediately because the kernel code currently executing wouldn't
//!   be mapped. The next sub-item in the slice (higher-half kernel
//!   mapping shared across all address spaces) closes that gap. Until
//!   then this type is "installable but not loadable."
//! - **No TLB flushing.** No `AddressSpace` is ever loaded onto a CPU
//!   today, so the TLB never holds entries for the new mappings. When
//!   the scheduler arrives it will gain a `set_active` entry point
//!   that takes responsibility for flushing.
//! - **Eager vs lazy anonymous allocation.** [`map_vma`](AddressSpace::map_vma)
//!   allocates one frame per page up front; [`map_vma_lazy`](AddressSpace::map_vma_lazy)
//!   reserves the range with no frames and lets
//!   [`fault_in`](AddressSpace::fault_in) back each page on first touch (the
//!   demand-paging slice; the `#PF` handler now resolves not-present user
//!   faults against the VMA tree). The ELF loader reserves stacks lazily;
//!   object- and the (future) file-backed paths have their own backing
//!   sources. Lazy `MemoryObject` backing — which would lift the eager-
//!   allocation DoS cap — is the next consumer of this machinery.
//! - **Intermediate page-table frames are not reclaimed on unmap.**
//!   This matches the deferred decision documented for
//!   `ArchPaging::unmap_page` (see [deferred-decisions.md]). `Drop`
//!   reclaims leaf frames and the top-level PML4; the PDPT / PD / PT
//!   levels in between leak. For a single Phase 1 address space this
//!   is negligible.
//!
//! [deferred-decisions.md]: ../../docs/rationale/deferred-decisions.md

use core::ptr;

use crate::arch::Paging;
use crate::arch::abi::{DEFAULT_USER_STACK_SIZE, DEFAULT_USER_STACK_TOP, USER_VIRT_END};
use crate::arch::paging::{ArchPaging, MapError as ArchMapError, PageFlags};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, SpinLock};
use crate::mm::vmm::{FaultAccess, MappingKind, Protection, VAddrRange, Vma, VmaTree};
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, heap};
use crate::object::{MemoryObject, ObjectRef};

/// Base of the `hint == 0` ("anywhere") mapping window for
/// [`AddressSpace::find_free_range`]: above the ELF image (loaded at
/// `0x40_0000`) and well below the default user stack. See `arch::abi`.
const MMAP_BASE: u64 = 0x1_0000_0000;

/// Exclusive upper bound of the mapping window — kept below the default user
/// stack region so an "anywhere" mapping can never collide with the stack the
/// ELF loader installs near [`DEFAULT_USER_STACK_TOP`].
const MMAP_MAX: u64 = DEFAULT_USER_STACK_TOP - DEFAULT_USER_STACK_SIZE;

/// Why [`AddressSpace::map_vma`] could not install a mapping.
///
/// On error the caller's [`KBox<Vma>`] is returned in the tuple
/// alongside the variant so it can be inspected, dropped, or retried.
#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    /// A range endpoint was not canonical for 4-level paging.
    NotCanonical,
    /// The range falls outside the user half (`[0, USER_VIRT_END)`).
    /// Kernel-half mappings are installed by a separate path.
    NotUserHalf,
    /// The range overlaps an existing VMA in this address space.
    Overlap,
    /// Physical-frame allocation (a leaf, or an intermediate page-table
    /// frame) failed. Any frames installed before the failure are
    /// rolled back before this is returned.
    OutOfMemory,
}

/// Outcome of [`AddressSpace::fault_in`] — the resolution of a page fault
/// against this address space. Only [`Mapped`](FaultIn::Mapped) lets the
/// faulting instruction retry; every other variant means the fault is fatal to
/// the thread (the arch handler turns them into a SegFault notification).
#[derive(Debug, PartialEq, Eq)]
pub enum FaultIn {
    /// A frame was faulted in (or was already present): the faulting
    /// instruction may be retried.
    Mapped,
    /// No VMA covers the faulting address — an access to unmapped memory.
    NoVma,
    /// A VMA covers the address but does not permit the attempted access
    /// (a write to a read-only mapping, or a fetch from a non-exec one).
    Protection,
    /// A VMA covers the address and permits the access, but a backing frame
    /// could not be allocated.
    Oom,
    /// The address falls in a [`FileBacked`](MappingKind::FileBacked) mapping: a
    /// **not-present file fault** the caller must service through the page cache —
    /// fetch the backing ([`file_backing`](AddressSpace::file_backing)), ensure the
    /// page is resident (blocking on the producer if needed), and install the PTE
    /// ([`map_file_page`](AddressSpace::map_file_page)). `fault_in` itself does *not*
    /// touch the file's page cache (its lock is rank-4, like this AS lock — they are
    /// never nested), so this arm only *signals*; the fill happens outside the AS
    /// lock. Until the fault path wires that up (slice 8 Part 2b) a file fault stays
    /// fatal.
    FileBacked,
}

/// A per-process virtual address space.
///
/// All state lives behind a single `SpinLock<Inner>` (rank 4 per
/// [kernel/docs/lock-ordering.md]). Public methods acquire and release
/// the lock internally; callers cannot reach the inner state otherwise.
pub struct AddressSpace {
    inner: SpinLock<Inner>,
}

struct Inner {
    vma_tree: VmaTree,
    /// Physical base of the top-level page-table frame (the PML4 on
    /// x86_64). Allocated at construction, freed in `Drop` after the
    /// tree is torn down.
    root: PhysAddr,
}

impl AddressSpace {
    /// Build an empty (user-half) address space sharing the boot
    /// kernel's higher-half mappings: allocate a fresh PML4 frame, zero
    /// it, inherit the kernel-half entries from the boot template
    /// (`ArchPaging::inherit_kernel_mappings`), and pair it with an
    /// empty VMA tree. Returns [`AllocError`] if the PML4 frame
    /// allocation fails; panics if called before
    /// `arch::init_kernel_template`.
    pub fn new() -> Result<Self, AllocError> {
        let root = heap::buddy_alloc(0).ok_or(AllocError)?;
        // SAFETY: the frame was just returned by the buddy and is not
        // aliased; HHDM access is the standard way to reach a fresh
        // physical frame. Zeroing it makes every PML4 entry absent;
        // `inherit_kernel_mappings` then refills the kernel half so
        // the AS is loadable without losing kernel code, stack, or HHDM.
        unsafe {
            let virt = (root.as_u64() + heap::hhdm_offset()) as *mut u8;
            ptr::write_bytes(virt, 0, PAGE_SIZE);
            Paging::inherit_kernel_mappings(root);
        }
        Ok(AddressSpace {
            inner: SpinLock::new(Inner {
                vma_tree: VmaTree::new(),
                root,
            }),
        })
    }

    /// Physical base of this address space's top-level page table. The
    /// future scheduler will hand this to
    /// [`ArchPaging::set_page_table`](crate::arch::paging::ArchPaging::set_page_table)
    /// to make the address space active.
    pub fn root(&self) -> PhysAddr {
        self.inner.lock().root
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().vma_tree.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().vma_tree.len()
    }

    /// Install a VMA: allocate and zero a frame for every page, install
    /// each PTE, then insert the VMA into the tree. The whole sequence
    /// runs under the AS lock so the tree and the page tables can never
    /// disagree about what is mapped.
    ///
    /// Returns the box back to the caller on any rejection.
    pub fn map_vma(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        let range = boxed.range;

        // Canonicality: both endpoints of the half-open range must be
        // canonical. The end is exclusive, so we test `end - 1` (the
        // last byte covered); that handles the edge case where `end ==
        // USER_VIRT_END` (a non-canonical address in itself, but the
        // covered bytes stop one short of it).
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((boxed, MapError::NotUserHalf));
        }

        let mut guard = self.inner.lock();

        // Pre-check overlap so a doomed insert doesn't run frame
        // allocations that we'd just have to roll back.
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        let flags = protection_to_page_flags(boxed.prot);
        let root = guard.root;
        let total_pages = range.pages();
        let mut installed: u64 = 0;

        for i in 0..total_pages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));

            let Some(phys) = heap::buddy_alloc(0) else {
                rollback_partial_map(root, range.start(), installed);
                return Err((boxed, MapError::OutOfMemory));
            };

            // Anonymous mapping: zero the freshly allocated frame.
            // SAFETY: `phys` was just allocated, is not aliased, and is
            // HHDM-reachable.
            unsafe {
                let v = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
                ptr::write_bytes(v, 0, PAGE_SIZE);
            }

            // SAFETY: `root` is the PML4 owned by this AS; `phys` is a
            // fresh frame we just allocated and zeroed; `virt` is in
            // the validated, canonical, user-half range. No TLB flush
            // is needed today — no CPU has this address space loaded.
            let r = unsafe { Paging::map_page(root, virt, phys, flags) };
            match r {
                Ok(()) => installed += 1,
                Err(ArchMapError::OutOfMemory) => {
                    heap::buddy_free(phys, 0);
                    rollback_partial_map(root, range.start(), installed);
                    return Err((boxed, MapError::OutOfMemory));
                }
                Err(ArchMapError::AlreadyMapped) => {
                    // Impossible: we hold the AS lock and pre-checked
                    // overlap; a PTE for this virt can't already exist.
                    unreachable!(
                        "ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check"
                    );
                }
                Err(ArchMapError::Misaligned) => {
                    // Impossible: VAddrRange enforces page alignment,
                    // and the per-page virt is start + i*PAGE_SIZE.
                    unreachable!(
                        "ArchPaging::map_page returned Misaligned for a page-aligned per-page address"
                    );
                }
            }
        }

        // Commit. The tree's insert can't reject — we still hold the
        // lock and re-checked overlap above.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(b) => {
                rollback_partial_map(root, range.start(), installed);
                Err((b, MapError::Overlap))
            }
        }
    }

    /// Reserve an **anonymous** VMA without backing it: insert the `Vma` into
    /// the tree but allocate **no** frames and install **no** PTEs. The pages
    /// are faulted in lazily — zeroed one at a time on first touch — by
    /// [`fault_in`](Self::fault_in) when the running thread takes a not-present
    /// page fault in the range.
    ///
    /// This is the demand-paged counterpart to [`map_vma`](Self::map_vma)'s
    /// eager allocation. It is anonymous-only: object- and (future)
    /// file-backed mappings have their own backing-frame sources.
    /// `boxed.mapping` must be [`MappingKind::Anonymous`].
    ///
    /// Same canonicality / user-half / overlap rejections as `map_vma`; the
    /// box is returned on any rejection. Allocates nothing, so the only error
    /// is a structural rejection — never [`OutOfMemory`](MapError::OutOfMemory).
    pub fn map_vma_lazy(&self, boxed: KBox<Vma>) -> Result<(), (KBox<Vma>, MapError)> {
        debug_assert_eq!(
            boxed.mapping,
            MappingKind::Anonymous,
            "map_vma_lazy is for anonymous mappings only"
        );
        let range = boxed.range;

        // Endpoint canonicality (same reasoning as map_vma: test end-1).
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((boxed, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((boxed, MapError::NotUserHalf));
        }

        let mut guard = self.inner.lock();
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((boxed, MapError::Overlap));
        }

        // No frame allocation, no PTE install — the range is reserved in the
        // tree only; `fault_in` backs each page on demand.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            // Unreachable: overlap was pre-checked under the same lock.
            Err(b) => Err((b, MapError::Overlap)),
        }
    }

    /// Resolve a page fault at `addr` for the access `access`, against this
    /// (currently-loaded) address space. Called from the architecture's
    /// page-fault handler for a **not-present** ring-3 fault, before it falls
    /// back to delivering a fatal segmentation fault.
    ///
    /// Looks up the covering VMA, checks the attempted access against its
    /// protection, and — for an [`Anonymous`](MappingKind::Anonymous) mapping —
    /// allocates and zeroes one frame, installs the leaf PTE for the faulting
    /// page, and flushes that page's stale (not-present) TLB entry. Returns
    /// [`FaultIn::Mapped`] on success (the instruction may retry); every other
    /// outcome is fatal to the thread.
    ///
    /// The faulting address space **is live in the MMU** when this runs (unlike
    /// the eager `map_vma`, which maps into a not-yet-loaded AS), so the TLB
    /// flush here is mandatory: the CPU cached the not-present translation when
    /// it faulted.
    pub fn fault_in(&self, addr: VirtAddr, access: FaultAccess) -> FaultIn {
        // The page-table install writes through `guard.root` (a `PhysAddr`
        // copied out below), not through the guard, and the tree is only read
        // here — so an immutable guard suffices.
        let guard = self.inner.lock();

        let Some(vma) = guard.vma_tree.find_covering(addr) else {
            return FaultIn::NoVma;
        };

        // Protection check: a present VMA is always readable, so only writes
        // and instruction fetches can be refused.
        match access {
            FaultAccess::Write if !vma.prot.contains(Protection::WRITE) => {
                return FaultIn::Protection;
            }
            FaultAccess::Execute if !vma.prot.contains(Protection::EXEC) => {
                return FaultIn::Protection;
            }
            _ => {}
        }

        let flags = protection_to_page_flags(vma.prot);
        let kind = vma.mapping;
        let root = guard.root;
        // Round the faulting address down to its page base.
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));

        match kind {
            MappingKind::Anonymous => {
                let Some(phys) = heap::buddy_alloc(0) else {
                    return FaultIn::Oom;
                };
                // SAFETY: `phys` was just allocated, is not aliased, and is
                // HHDM-reachable; we zero the whole frame before it is mapped.
                unsafe {
                    let v = (phys.as_u64() + heap::hhdm_offset()) as *mut u8;
                    ptr::write_bytes(v, 0, PAGE_SIZE);
                }
                // SAFETY: `root` is this AS's PML4; `phys` is the fresh,
                // zeroed frame; `page_base` is the page-aligned faulting
                // address, in the validated user-half VMA range. The PTE is
                // absent (this was a not-present fault), so `map_page` installs
                // a new leaf rather than aliasing.
                match unsafe { Paging::map_page(root, page_base, phys, flags) } {
                    Ok(()) => {}
                    Err(ArchMapError::OutOfMemory) => {
                        heap::buddy_free(phys, 0);
                        return FaultIn::Oom;
                    }
                    // Impossible: not-present fault ⇒ no existing PTE; and
                    // `page_base` is page-aligned by construction.
                    Err(ArchMapError::AlreadyMapped | ArchMapError::Misaligned) => {
                        heap::buddy_free(phys, 0);
                        unreachable!(
                            "fault_in: map_page rejected a fresh page-aligned not-present mapping"
                        );
                    }
                }
                // SAFETY: ring-0 `invlpg` evicting the not-present entry the
                // CPU cached for this page on the fault we are servicing.
                unsafe {
                    Paging::flush_tlb_page(page_base);
                }
                crate::mm::record_demand_fault();
                FaultIn::Mapped
            }
            // Object mappings are installed eagerly by `map_object`; a
            // not-present fault on one is a bug or a torn-down race — treat it
            // as an access to nothing.
            MappingKind::Object => FaultIn::NoVma,
            // A file-backed not-present fault: signal the caller to page it in
            // from the file's cache. We do NOT touch that cache here — its lock is
            // rank 4, same as this AS lock, and the two must never nest — so the
            // caller fetches the backing + fills + installs the PTE outside this
            // lock (`file_backing` → fill → `map_file_page`). See slice 8 Part 2.
            MappingKind::FileBacked => FaultIn::FileBacked,
        }
    }

    /// Map a [`MemoryObject`]'s **own** frames over `range`, holding `object`
    /// alive for the mapping's lifetime. Unlike [`map_vma`](Self::map_vma)
    /// this allocates and zeroes **nothing** — it installs PTEs pointing at
    /// the object's pre-allocated frames — so mapping the same object twice
    /// aliases the same physical memory. The recorded VMA owns `object`; the
    /// frames are freed by the object on its last-ref drop, not by unmap.
    ///
    /// `object` must reference a [`MemoryObject`] with at least
    /// `range.pages()` frames (the syscall layer checks `size <= obj.size`).
    /// On any failure the `object` reference is handed back in the error
    /// tuple so the caller can drop it.
    pub fn map_object(
        &self,
        range: VAddrRange,
        prot: Protection,
        object: ObjectRef,
    ) -> Result<(), (ObjectRef, MapError)> {
        // Canonicality / user-half checks (as map_vma), before consuming
        // anything — `object` is returned untouched on rejection.
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((object, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((object, MapError::NotUserHalf));
        }

        // Read the backing frames through the live object. The slice is
        // derived from a raw pointer, so its lifetime is not tied to
        // `object`'s ownership — we may move `object` into the VMA later
        // while still holding `frames` for the install loop.
        debug_assert_eq!(object.object_type(), KObjectType::MemoryObject);
        // SAFETY: `object` holds a reference to a live `MemoryObject` (its
        // header is at offset 0); we only read its frame list.
        let mobj = unsafe { &*(object.as_ptr() as *const MemoryObject) };
        let frames = mobj.frames();
        let npages = range.pages();
        debug_assert!(frames.len() as u64 >= npages, "object too small for range");

        // Build the VMA box (with no object yet) BEFORE taking the lock — the
        // no-alloc-under-spinlock rule. This is the only fallible step that
        // happens before `object` is consumed, so its failure path can still
        // return `object`.
        let mut boxed = match KBox::try_new(Vma::new(range, prot, MappingKind::Object)) {
            Ok(b) => b,
            Err(_) => return Err((object, MapError::OutOfMemory)),
        };

        let mut guard = self.inner.lock();

        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((object, MapError::Overlap));
        }

        let flags = protection_to_page_flags(prot);
        let root = guard.root;
        let mut installed: u64 = 0;

        for i in 0..npages {
            let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
            // SAFETY: `root` is this AS's PML4; `frames[i]` is owned by the
            // `MemoryObject` the VMA will keep alive; `virt` is in the
            // validated canonical user-half range. Aliasing a frame already
            // mapped elsewhere is intended (shared memory). No frame is
            // allocated or zeroed here — the object owns them.
            match unsafe { Paging::map_page(root, virt, frames[i as usize], flags) } {
                Ok(()) => installed += 1,
                Err(ArchMapError::OutOfMemory) => {
                    rollback_object_map(root, range.start(), installed);
                    return Err((object, MapError::OutOfMemory));
                }
                Err(ArchMapError::AlreadyMapped) => {
                    // Impossible: overlap was pre-checked under the lock.
                    unreachable!(
                        "ArchPaging::map_page returned AlreadyMapped after VmaTree overlap pre-check"
                    );
                }
                Err(ArchMapError::Misaligned) => {
                    // Impossible: VAddrRange enforces page alignment.
                    unreachable!(
                        "ArchPaging::map_page returned Misaligned for a page-aligned per-page address"
                    );
                }
            }
        }

        // PTEs are in; now move `object` into the VMA and commit to the tree.
        boxed.object = Some(object);
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(mut b) => {
                rollback_object_map(root, range.start(), installed);
                let object = b
                    .object
                    .take()
                    .expect("object-backed VMA carries its ObjectRef");
                Err((object, MapError::Overlap))
            }
        }
    }

    /// Reserve `range` as a **lazy file-backed** mapping of `object` (a
    /// [`FileObject`](crate::object::FileObject)) — like [`map_vma_lazy`], but
    /// `MappingKind::FileBacked` with the object held in the VMA, and **no PTEs**.
    /// Each page is faulted in from the file's page cache on first touch
    /// (`fault_in` → [`FaultIn::FileBacked`] → fill → [`map_file_page`]). The VMA
    /// keeps `object` alive, so its cache frames outlive the mapping; `unmap`
    /// removes the PTEs but never frees them (the object owns them). On any
    /// rejection `object` is handed back untouched.
    ///
    /// [`map_vma_lazy`]: AddressSpace::map_vma_lazy
    /// [`map_file_page`]: AddressSpace::map_file_page
    pub fn map_file(
        &self,
        range: VAddrRange,
        prot: Protection,
        object: ObjectRef,
    ) -> Result<(), (ObjectRef, MapError)> {
        debug_assert_eq!(object.object_type(), KObjectType::FileObject);
        // Canonicality / user-half checks before consuming `object`.
        let last_byte = VirtAddr::new(range.end().as_u64() - 1);
        if !range.start().is_canonical() || !last_byte.is_canonical() {
            return Err((object, MapError::NotCanonical));
        }
        if range.end().as_u64() > USER_VIRT_END {
            return Err((object, MapError::NotUserHalf));
        }
        // Build the VMA box (no object yet) before the lock (no alloc under it).
        let mut boxed = match KBox::try_new(Vma::new(range, prot, MappingKind::FileBacked)) {
            Ok(b) => b,
            Err(_) => return Err((object, MapError::OutOfMemory)),
        };
        let mut guard = self.inner.lock();
        if guard.vma_tree.find_first_overlapping(range).is_some() {
            return Err((object, MapError::Overlap));
        }
        boxed.object = Some(object);
        // No PTEs — `fault_in` backs each page on demand from the page cache.
        match guard.vma_tree.insert(boxed) {
            Ok(()) => Ok(()),
            Err(mut b) => {
                let object = b
                    .object
                    .take()
                    .expect("file-backed VMA carries its ObjectRef");
                Err((object, MapError::Overlap))
            }
        }
    }

    /// For a `FileBacked` fault at `addr` (signalled by [`FaultIn::FileBacked`]),
    /// return the backing [`FileObject`](crate::object::FileObject) (a **cloned**
    /// `ObjectRef`, owned by the caller) and the **page index** within the file
    /// (`(page_base − vma.start) / PAGE`; the mapping covers the file from offset 0).
    /// `None` if no VMA covers `addr` or it is not file-backed (a teardown race).
    /// A separate, fresh lock acquisition from `fault_in` — so the file's page cache
    /// is reached only **outside** this AS lock, never nested under it.
    pub fn file_backing(&self, addr: VirtAddr) -> Option<(ObjectRef, usize)> {
        let guard = self.inner.lock();
        let vma = guard.vma_tree.find_covering(addr)?;
        if vma.mapping != MappingKind::FileBacked {
            return None;
        }
        let object = vma.object.as_ref()?.clone();
        let page_base = addr.as_u64() & !(PAGE_SIZE as u64 - 1);
        let index = ((page_base - vma.range.start().as_u64()) / PAGE_SIZE as u64) as usize;
        Some((object, index))
    }

    /// Install the leaf PTE for the file-backed page containing `addr`, pointing at
    /// `frame` (a now-resident cache frame the producer filled), with the covering
    /// VMA's protection. Re-validates under the AS lock that a `FileBacked` VMA for
    /// the **same** `object` still covers `addr` (the fault may have raced an unmap /
    /// process exit while it blocked on the fill). Returns `true` if the page is now
    /// present (installed here, or already mapped by a racing fault) — the
    /// instruction may retry; `false` if the VMA is gone/changed (fatal) or a
    /// page-table allocation failed. The frame is **not** owned by the VMA (the
    /// object owns it), so `unmap` will remove this PTE without freeing it.
    pub fn map_file_page(&self, addr: VirtAddr, object: &ObjectRef, frame: PhysAddr) -> bool {
        let guard = self.inner.lock();
        let Some(vma) = guard.vma_tree.find_covering(addr) else {
            return false;
        };
        if vma.mapping != MappingKind::FileBacked
            || vma.object.as_ref().map(|o| o.as_ptr()) != Some(object.as_ptr())
        {
            return false;
        }
        let flags = protection_to_page_flags(vma.prot);
        let root = guard.root;
        let page_base = VirtAddr::new(addr.as_u64() & !(PAGE_SIZE as u64 - 1));
        // SAFETY: `root` is this AS's PML4; `frame` is a live, resident cache frame
        // the file object owns and keeps alive (the VMA holds its `ObjectRef`);
        // `page_base` is page-aligned and in the validated user-half VMA range.
        match unsafe { Paging::map_page(root, page_base, frame, flags) } {
            // Installed, or a concurrent fault already mapped it — either way the
            // page is present and the instruction may retry.
            Ok(()) | Err(ArchMapError::AlreadyMapped) => {
                // SAFETY: ring-0 `invlpg` evicting the not-present entry cached on
                // the fault we are servicing.
                unsafe { Paging::flush_tlb_page(page_base) };
                true
            }
            Err(ArchMapError::OutOfMemory) => false,
            // Impossible: `page_base` is page-aligned by construction.
            Err(ArchMapError::Misaligned) => {
                unreachable!("map_file_page: Misaligned for a page-aligned address")
            }
        }
    }

    /// Find the lowest free virtual range of `size` bytes in the "anywhere"
    /// mapping window (`[MMAP_BASE, MMAP_MAX)`), for `sys_memory_map(hint=0)`.
    pub fn find_free_range(&self, size: u64) -> Option<VAddrRange> {
        let guard = self.inner.lock();
        guard
            .vma_tree
            .find_free_range(VirtAddr::new(MMAP_BASE), VirtAddr::new(MMAP_MAX), size)
    }

    /// Remove the VMA covering `addr`: drop it from the tree, then walk
    /// its range uninstalling every PTE and freeing the backing frame
    /// (for anonymous mappings). Returns the VMA box, or `None` if no
    /// VMA covers `addr`.
    pub fn unmap_covering(&self, addr: VirtAddr) -> Option<KBox<Vma>> {
        let mut guard = self.inner.lock();
        let boxed = guard.vma_tree.remove_covering(addr)?;
        free_vma_pages(guard.root, &boxed);
        Some(boxed)
    }
}

impl Drop for AddressSpace {
    /// Tear down every VMA (uninstall PTEs, free leaf frames), drop
    /// the (now-empty) tree, then free the top-level PML4 frame.
    /// Intermediate page-table frames leak per the deferred decision.
    fn drop(&mut self) {
        let mut guard = self.inner.lock();

        // Drain the tree leftmost-first: peek via iter (an immutable
        // borrow that ends before the mutating remove), then remove.
        loop {
            let leftmost_start = {
                let mut it = guard.vma_tree.iter();
                let Some(v) = it.next() else { break };
                v.range.start()
            };
            let Some(boxed) = guard.vma_tree.remove_covering(leftmost_start) else {
                break;
            };
            free_vma_pages(guard.root, &boxed);
            // `boxed` drops at the end of this iteration, returning
            // the Vma to the slab.
        }

        heap::buddy_free(guard.root, 0);
    }
}

/// Walk a VMA's range, uninstall every PTE, and free each leaf frame.
/// Used by both `unmap_covering` and `Drop`.
fn free_vma_pages(root: PhysAddr, vma: &Vma) {
    let range = vma.range;
    for i in 0..range.pages() {
        let virt = VirtAddr::new(range.start().as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: every page in `range` was mapped by a prior `map_vma`
        // under the same AS lock; the PTEs exist now and `root` is the
        // valid top-level table they were installed into.
        let r = unsafe { Paging::unmap_page(root, virt) };
        if let Ok(phys) = r {
            match vma.mapping {
                MappingKind::Anonymous => heap::buddy_free(phys, 0),
                // Object-backed: the frames are owned by the `MemoryObject`
                // (held alive by `vma.object`). They are freed by the object
                // on its last-ref drop — which includes this VMA's `object`
                // reference being released when `vma` drops — never here.
                MappingKind::Object => {}
                // TODO(phase-2 page cache): file pages are owned by the page
                // cache, not this mapping, so (like Object) the PTE is removed
                // but the frame is not freed here. No FileBacked VMA is ever
                // constructed yet, so this arm is currently unreachable.
                MappingKind::FileBacked => {}
            }
        }
    }
}

/// Roll back a partial map_vma: walk `installed` pages starting at
/// `start`, uninstall each PTE, and free each leaf frame. Used only on
/// failure paths inside `map_vma`.
fn rollback_partial_map(root: PhysAddr, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: we successfully installed exactly these `installed`
        // pages in `map_vma` immediately before this call.
        let r = unsafe { Paging::unmap_page(root, virt) };
        if let Ok(phys) = r {
            heap::buddy_free(phys, 0);
        }
    }
}

/// Roll back a partial [`map_object`](AddressSpace::map_object): uninstall the
/// `installed` PTEs starting at `start`. Like [`rollback_partial_map`] but
/// **does not free the frames** — they belong to the `MemoryObject`, not the
/// mapping.
fn rollback_object_map(root: PhysAddr, start: VirtAddr, installed: u64) {
    for i in 0..installed {
        let virt = VirtAddr::new(start.as_u64() + i * (PAGE_SIZE as u64));
        // SAFETY: we successfully installed exactly these `installed` pages in
        // `map_object` immediately before this call. The returned phys is the
        // object's frame, which the object still owns — discard it.
        let _ = unsafe { Paging::unmap_page(root, virt) };
    }
}

/// Translate the VMA-level [`Protection`] into hardware
/// [`PageFlags`]. `NO_EXECUTE` is inverted because the hardware default
/// is executable (NX is opt-in), but [`Protection`]'s default is
/// non-executable (W^X by default).
fn protection_to_page_flags(prot: Protection) -> PageFlags {
    let mut f = PageFlags::empty();
    if prot.contains(Protection::WRITE) {
        f = f | PageFlags::WRITABLE;
    }
    if !prot.contains(Protection::EXEC) {
        f = f | PageFlags::NO_EXECUTE;
    }
    if prot.contains(Protection::USER) {
        f = f | PageFlags::USER;
    }
    f
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::Paging;
    use crate::arch::paging::ArchPaging;
    use crate::mm::test_support::init_global_heap;
    use crate::mm::vmm::VAddrRange;

    const PAGE: u64 = PAGE_SIZE as u64;

    fn va(v: u64) -> VirtAddr {
        VirtAddr::new(v)
    }

    fn range(start: u64, end: u64) -> VAddrRange {
        VAddrRange::new(va(start), va(end)).expect("test range must be valid")
    }

    fn anon_box(r: VAddrRange, prot: Protection) -> KBox<Vma> {
        KBox::try_new(Vma::new(r, prot, MappingKind::Anonymous))
            .expect("test heap exhausted")
    }

    #[test]
    fn new_yields_empty_address_space_with_real_root() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.is_empty());
        assert_eq!(asp.len(), 0);
        // Root must be a real, page-aligned physical address.
        let root = asp.root();
        assert!(root.is_page_aligned());
        assert_ne!(root.as_u64(), 0);
    }

    #[test]
    fn map_single_page_installs_pte_translate_finds_it() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 5);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        // SAFETY: translate is read-only against the live tables we
        // just populated. The root is owned by `asp`.
        let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) };
        assert!(phys.is_some());
        // Address just past the page should be unmapped.
        let beyond = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) };
        assert!(beyond.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_multi_page_installs_every_pte() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 8, PAGE * 16);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        // Each page in the range is mapped; the addresses immediately
        // before and after are not.
        for i in 8..16 {
            let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_some(), "page {i} not mapped");
        }
        let before = unsafe { Paging::translate(asp.root(), va(PAGE * 7)) };
        let after = unsafe { Paging::translate(asp.root(), va(PAGE * 16)) };
        assert!(before.is_none());
        assert!(after.is_none());
    }

    #[test]
    fn map_then_unmap_removes_the_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("map must succeed");

        let removed = asp
            .unmap_covering(va(PAGE * 5))
            .expect("unmap must find covering vma");
        assert_eq!(removed.range, r);
        assert!(asp.is_empty());

        for i in 4..8 {
            let p = unsafe { Paging::translate(asp.root(), va(PAGE * i)) };
            assert!(p.is_none(), "page {i} still mapped after unmap");
        }
    }

    #[test]
    fn map_rejects_overlap_with_existing_vma() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        asp.map_vma(anon_box(
            range(PAGE * 4, PAGE * 8),
            Protection::WRITE | Protection::USER,
        ))
        .expect("first map must succeed");

        let err = asp
            .map_vma(anon_box(
                range(PAGE * 6, PAGE * 10),
                Protection::WRITE | Protection::USER,
            ))
            .expect_err("overlapping map must fail");
        assert_eq!(err.1, MapError::Overlap);
        // The returned box should still carry the rejected range.
        assert_eq!(err.0.range, range(PAGE * 6, PAGE * 10));

        // The original mapping must be untouched.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_some());
        // The rejected range's pages must NOT have been installed.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 9)) }.is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_rejects_kernel_half_range() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        // A range that ends past USER_VIRT_END is rejected.
        let r = VAddrRange::new(
            va(USER_VIRT_END - PAGE),
            va(USER_VIRT_END + PAGE),
        )
        .unwrap();
        let err = asp
            .map_vma(anon_box(r, Protection::WRITE | Protection::USER))
            .expect_err("kernel-half range must be rejected");
        // The error is NotUserHalf, or NotCanonical depending on which
        // check triggers first — both are acceptable rejections.
        assert!(matches!(
            err.1,
            MapError::NotUserHalf | MapError::NotCanonical
        ));
    }

    #[test]
    fn unmap_on_address_with_no_vma_returns_none() {
        init_global_heap();
        let asp = AddressSpace::new().expect("new must succeed");
        assert!(asp.unmap_covering(va(PAGE * 100)).is_none());
        // Even with one mapped VMA, an addr not covered returns None.
        asp.map_vma(anon_box(
            range(PAGE * 4, PAGE * 8),
            Protection::WRITE | Protection::USER,
        ))
        .unwrap();
        assert!(asp.unmap_covering(va(PAGE * 20)).is_none());
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn drop_tears_down_populated_address_space() {
        // Verify Drop is well-behaved: build a populated AS, drop it,
        // do it again. Across 8 iterations of 16 pages each this
        // would gradually exhaust the 16 MiB test heap if Drop leaked
        // leaf frames or the PML4.
        init_global_heap();
        for _ in 0..8 {
            let asp = AddressSpace::new().expect("new must succeed");
            for i in 0..4u64 {
                let start = (i * 8) * PAGE;
                asp.map_vma(anon_box(
                    range(start, start + PAGE * 4),
                    Protection::WRITE | Protection::USER,
                ))
                .expect("map must succeed");
            }
            // Drop runs at end of iteration.
        }
    }

    // --- Object-backed mappings --------------------------------------

    use crate::object::header::test_probe;

    /// Adopt a `MemoryObject`'s creation reference into an `ObjectRef`.
    fn into_obj(m: KBox<MemoryObject>) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(KBox::into_raw(m).as_ptr() as *mut (), KObjectType::MemoryObject)
        }
    }

    fn uprot() -> Protection {
        Protection::WRITE | Protection::USER
    }

    #[test]
    fn map_object_points_ptes_at_object_frames() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize * 3).unwrap();
        let frames = [m.frames()[0], m.frames()[1], m.frames()[2]];
        let obj = into_obj(m);

        asp.map_object(range(PAGE * 4, PAGE * 7), uprot(), obj)
            .expect("map_object must succeed");

        // Each mapped page translates to the object's own frame — not a
        // freshly allocated one (this is what makes the object shareable).
        for i in 0..3u64 {
            let phys = unsafe { Paging::translate(asp.root(), va(PAGE * (4 + i))) }
                .expect("page must be mapped");
            assert_eq!(phys, frames[i as usize], "page {i} not pointing at object frame");
        }
        assert_eq!(asp.len(), 1);
    }

    #[test]
    fn map_object_twice_aliases_same_frame() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize).unwrap();
        let frame0 = m.frames()[0];
        let obj = into_obj(m);
        let obj2 = obj.clone(); // second mapping needs its own reference

        asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        asp.map_object(range(PAGE * 8, PAGE * 9), uprot(), obj2).unwrap();

        let p1 = unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.unwrap();
        let p2 = unsafe { Paging::translate(asp.root(), va(PAGE * 8)) }.unwrap();
        assert_eq!(p1, frame0);
        assert_eq!(p2, frame0);
        assert_eq!(p1, p2, "two mappings of one object must alias the same frame");
    }

    #[test]
    fn unmap_object_mapping_does_not_free_the_objects_frames() {
        init_global_heap();
        test_probe::reset();
        let asp = AddressSpace::new().unwrap();
        let m = MemoryObject::try_new(PAGE as usize).unwrap();
        let obj = into_obj(m);
        // Hold an extra reference so unmapping (which drops the VMA's ref)
        // does not destroy the object — then we can prove it survived.
        let keep = obj.clone();

        asp.map_object(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        let removed = asp
            .unmap_covering(va(PAGE * 4))
            .expect("unmap must find the covering vma");
        // PTE is gone.
        assert!(unsafe { Paging::translate(asp.root(), va(PAGE * 4)) }.is_none());
        // Dropping the removed VMA releases its object reference, but `keep`
        // still holds one — the object (and its frames) must NOT be freed.
        drop(removed);
        assert_eq!(test_probe::memory_object_destroys(), 0, "unmap freed shared frames");
        // The last reference now drops: object destroyed, frames freed.
        drop(keep);
        assert_eq!(test_probe::memory_object_destroys(), 1);
    }

    #[test]
    fn find_free_range_picks_a_gap_in_the_mmap_window() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // An empty AS yields the window base.
        let r = asp.find_free_range(PAGE).expect("empty AS has room");
        assert_eq!(r.start().as_u64(), MMAP_BASE);
        assert_eq!(r.len(), PAGE);
    }

    // --- File-backed mappings (slice 8 Part 2a) --------------------------

    use crate::object::{FileObject, Producer, Reserve};

    /// Adopt a `FileObject`'s creation reference into an `ObjectRef`.
    fn into_file(f: KBox<FileObject>) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(KBox::into_raw(f).as_ptr() as *mut (), KObjectType::FileObject)
        }
    }

    #[test]
    fn map_file_reserves_lazily_with_no_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let obj = into_file(FileObject::try_new(3 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        asp.map_file(range(PAGE * 4, PAGE * 7), uprot(), obj)
            .expect("map_file must succeed");
        assert_eq!(asp.len(), 1, "the VMA is reserved");
        // No frame is backed yet — every page is lazy.
        for i in 0..3u64 {
            assert!(!is_mapped(&asp, va(PAGE * (4 + i))), "page {i} must be lazy");
        }
    }

    #[test]
    fn fault_in_file_backed_signals_and_file_backing_resolves() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let obj = into_file(FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        let obj_ptr = obj.as_ptr();
        asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj).unwrap();

        // A not-present fault in the mapping signals FileBacked (no fill here).
        assert_eq!(asp.fault_in(va(PAGE * 5), FaultAccess::Read), FaultIn::FileBacked);

        // file_backing returns the same object + the page index within the file
        // (the VMA maps the file from offset 0, so VMA page i == file page i).
        for (vma_page, want_index) in [(4u64, 0usize), (5, 1), (7, 3)] {
            let (got_obj, index) = asp
                .file_backing(va(PAGE * vma_page))
                .expect("covered by the file mapping");
            assert_eq!(got_obj.as_ptr(), obj_ptr, "same backing object");
            assert_eq!(index, want_index, "page index for vma page {vma_page}");
        }
        // Outside the mapping, no backing.
        assert!(asp.file_backing(va(PAGE * 2)).is_none());
    }

    #[test]
    fn map_file_page_installs_pte_for_a_ready_frame() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(4 * PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        // Reserve + ready file page 1, mimicking a completed fill.
        let frame = match f.reserve(1) {
            Reserve::New(frame) => frame,
            other => panic!("expected New, got {other:?}"),
        };
        f.mark_ready(1);
        let obj = into_file(f);
        asp.map_file(range(PAGE * 4, PAGE * 8), uprot(), obj.clone()).unwrap();

        // File page 1 lives at VMA page 5 (range starts at page 4). Map it.
        assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
        let phys = unsafe { Paging::translate(asp.root(), va(PAGE * 5)) }
            .expect("page must now be mapped");
        assert_eq!(phys, frame, "PTE points at the cache frame");
        // Idempotent: a racing re-map of the same page still reports present.
        assert!(asp.map_file_page(va(PAGE * 5), &obj, frame));
    }

    #[test]
    fn map_file_page_rejects_wrong_object_or_unmapped() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        let frame = match f.reserve(0) {
            Reserve::New(frame) => frame,
            other => panic!("{other:?}"),
        };
        f.mark_ready(0);
        let obj = into_file(f);
        asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj.clone()).unwrap();

        // A different object is rejected (the VMA backs `obj`, not `other`).
        let other = into_file(FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap());
        assert!(!asp.map_file_page(va(PAGE * 4), &other, frame));
        // An address outside any VMA is rejected.
        assert!(!asp.map_file_page(va(PAGE * 99), &obj, frame));
        // The legitimate mapping still works.
        assert!(asp.map_file_page(va(PAGE * 4), &obj, frame));
    }

    #[test]
    fn unmap_file_mapping_keeps_the_objects_frames() {
        init_global_heap();
        test_probe::reset();
        let asp = AddressSpace::new().unwrap();
        let f = FileObject::try_new(PAGE as usize, Producer::Stub { base: 0 }).unwrap();
        let frame = match f.reserve(0) {
            Reserve::New(frame) => frame,
            other => panic!("{other:?}"),
        };
        f.mark_ready(0);
        let obj = into_file(f);
        let keep = obj.clone();
        asp.map_file(range(PAGE * 4, PAGE * 5), uprot(), obj).unwrap();
        asp.map_file_page(va(PAGE * 4), &keep, frame);
        assert!(is_mapped(&asp, va(PAGE * 4)));

        // Unmap: the PTE goes, but the cache frame is owned by the FileObject —
        // the FileBacked teardown arm removes the PTE without freeing it.
        let removed = asp.unmap_covering(va(PAGE * 4)).expect("covering vma");
        assert!(!is_mapped(&asp, va(PAGE * 4)), "PTE removed");
        drop(removed);
        assert_eq!(test_probe::file_object_destroys(), 0, "object still referenced");
        // The last reference drops → the object is destroyed and frees its frames.
        drop(keep);
        assert_eq!(test_probe::file_object_destroys(), 1);
    }

    // --- Demand paging: lazy reservation + on-fault backing --------------

    /// Probe whether a page currently translates in `asp`.
    fn is_mapped(asp: &AddressSpace, v: VirtAddr) -> bool {
        // SAFETY: read-only walk of `asp`'s own page tables.
        unsafe { Paging::translate(asp.root(), v) }.is_some()
    }

    #[test]
    fn map_vma_lazy_installs_no_ptes() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER))
            .expect("lazy reserve must succeed");
        // The VMA is in the tree...
        assert_eq!(asp.len(), 1);
        // ...but no page is backed yet.
        for i in 4..8 {
            assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} unexpectedly backed");
        }
    }

    #[test]
    fn fault_in_backs_one_anonymous_page_on_demand() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER))
            .unwrap();

        // Fault in only the page at PAGE*5 (a sub-page-aligned address rounds
        // down to its page base).
        let outcome = asp.fault_in(va(PAGE * 5 + 0x40), FaultAccess::Write);
        assert_eq!(outcome, FaultIn::Mapped);
        assert!(is_mapped(&asp, va(PAGE * 5)), "faulted page not backed");
        // Its neighbours stay unbacked — fault-in is per-page, not per-VMA.
        assert!(!is_mapped(&asp, va(PAGE * 4)));
        assert!(!is_mapped(&asp, va(PAGE * 6)));
    }

    #[test]
    fn fault_in_with_no_covering_vma_is_no_vma() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 9), FaultAccess::Read), FaultIn::NoVma);
    }

    #[test]
    fn fault_in_write_to_readonly_is_protection() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // USER but not WRITE: a read-only user mapping.
        asp.map_vma_lazy(anon_box(range(PAGE * 4, PAGE * 5), Protection::USER))
            .unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Protection);
        // Refused — still unbacked.
        assert!(!is_mapped(&asp, va(PAGE * 4)));
        // A read to the same range is permitted and backs the page.
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Read), FaultIn::Mapped);
        assert!(is_mapped(&asp, va(PAGE * 4)));
    }

    #[test]
    fn fault_in_exec_from_nonexec_is_protection() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        // WRITE|USER but not EXEC.
        asp.map_vma_lazy(anon_box(
            range(PAGE * 4, PAGE * 5),
            Protection::WRITE | Protection::USER,
        ))
        .unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Execute), FaultIn::Protection);
        assert!(!is_mapped(&asp, va(PAGE * 4)));
    }

    #[test]
    fn dropping_a_partially_faulted_lazy_as_frees_only_backed_frames() {
        // A lazily-reserved 4-page VMA with only 2 pages faulted in: unmap
        // must free the 2 backed frames and skip the 2 never-backed pages
        // (their unmap_page returns Err and is ignored) — no panic, no leak.
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let r = range(PAGE * 4, PAGE * 8);
        asp.map_vma_lazy(anon_box(r, Protection::WRITE | Protection::USER))
            .unwrap();
        assert_eq!(asp.fault_in(va(PAGE * 4), FaultAccess::Write), FaultIn::Mapped);
        assert_eq!(asp.fault_in(va(PAGE * 6), FaultAccess::Write), FaultIn::Mapped);

        let removed = asp.unmap_covering(va(PAGE * 5)).expect("covering vma");
        assert_eq!(removed.range, r);
        assert!(asp.is_empty());
        // Every page — backed or not — is unmapped afterwards.
        for i in 4..8 {
            assert!(!is_mapped(&asp, va(PAGE * i)), "page {i} still mapped");
        }
    }

    #[test]
    fn fault_in_is_idempotent_friendly_across_a_lazy_drop() {
        // Repeatedly build, lazily reserve, fault a few pages, and drop: this
        // would exhaust the test heap if drop leaked the demand-faulted frames.
        init_global_heap();
        for _ in 0..8 {
            let asp = AddressSpace::new().unwrap();
            asp.map_vma_lazy(anon_box(
                range(PAGE * 4, PAGE * 12),
                Protection::WRITE | Protection::USER,
            ))
            .unwrap();
            for i in 4..12u64 {
                assert_eq!(asp.fault_in(va(PAGE * i), FaultAccess::Write), FaultIn::Mapped);
            }
            // Drop frees all eight faulted frames + the PML4.
        }
    }
}
