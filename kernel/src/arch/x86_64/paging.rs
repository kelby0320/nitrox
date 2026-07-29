//! x86_64 4-level page-table management: the [`ArchPaging`]
//! implementation, the page-table-entry format, and the table walk.
//!
//! x86_64 long mode translates a 48-bit canonical virtual address through
//! four levels — PML4 → PDPT → PD → PT — each a 4 KiB frame of 512
//! 8-byte entries, indexed by a 9-bit slice of the address; the low 12
//! bits are the page offset. This module implements only 4 KiB leaf
//! pages: [`map_page`](X86Paging::map_page) never sets the `PS` (huge)
//! bit. [`translate`] *does* understand huge pages, because it is run
//! against Limine's live tables, which may map memory with 2 MiB or
//! 1 GiB pages.
//!
//! Physical frames are reached through the higher-half direct map (HHDM):
//! a table at physical address `p` is addressed at `p + hhdm_offset()`.
//!
//! 5-level paging (57-bit addresses) is a documented non-goal — see
//! `docs/rationale/deferred-decisions.md`.

use crate::arch::paging::{ArchPaging, MapError, PageFlags, UnmapError};
use crate::arch::x86_64::regs;
use crate::libkern::SpinLock;
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr, heap};

/// Entries per page table at every level. A 4 KiB frame of 8-byte entries.
const PAGE_TABLE_ENTRIES: usize = 512;

// One page table is exactly one page frame — the invariant the HHDM walk
// and `alloc_page_table` both rely on.
const _: () = assert!(size_of::<[Pte; PAGE_TABLE_ENTRIES]>() == PAGE_SIZE);

// --- Page-table-entry bits ----------------------------------------------

/// Entry is valid; the CPU may use it for translation.
const PTE_PRESENT: u64 = 1 << 0;
/// Read/write. Cleared means the mapping is read-only.
const PTE_WRITABLE: u64 = 1 << 1;
/// User/supervisor. Set means ring 3 may access the page.
const PTE_USER: u64 = 1 << 2;
/// Page write-through caching.
const PTE_PWT: u64 = 1 << 3;
/// Page cache disable.
const PTE_PCD: u64 = 1 << 4;
/// Page size — at a non-leaf level this entry maps a huge page directly.
/// This module never sets it; [`translate`] reads it.
const PTE_HUGE: u64 = 1 << 7;
/// Global — the translation survives a CR3 reload.
const PTE_GLOBAL: u64 = 1 << 8;
/// No-execute. Faults unless `EFER.NXE` is set; see [`ensure_nxe`].
const PTE_NX: u64 = 1 << 63;

/// Physical-address field of an entry: bits 51:12.
const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;


// --- Page-table entry ---------------------------------------------------

/// A single x86_64 page-table entry, used at every level of the 4-level
/// hierarchy. `#[repr(transparent)]` over `u64` so `[Pte; 512]` is
/// exactly one 4 KiB page-table frame.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
struct Pte(u64);

impl Pte {
    /// The all-zero (not-present) entry.
    const fn empty() -> Pte {
        Pte(0)
    }

    /// `true` if the entry is valid for translation.
    const fn is_present(self) -> bool {
        self.0 & PTE_PRESENT != 0
    }

    /// `true` if a non-leaf entry maps a huge page directly (`PS` set).
    const fn is_huge(self) -> bool {
        self.0 & PTE_HUGE != 0
    }

    /// The physical address held in the entry's address field (bits 51:12).
    const fn phys(self) -> PhysAddr {
        PhysAddr::new(self.0 & PTE_ADDR_MASK)
    }

    /// A non-leaf entry pointing at child table `child`.
    ///
    /// Intermediate entries are made writable and user-accessible: on
    /// x86 the *leaf* entry's flags decide a mapping's effective
    /// permission, so permissive parents are the conventional choice and
    /// let one sub-tree hold both kernel and (later) user leaves.
    fn new_table(child: PhysAddr) -> Pte {
        Pte((child.as_u64() & PTE_ADDR_MASK) | PTE_PRESENT | PTE_WRITABLE | PTE_USER)
    }

    /// A 4 KiB leaf entry mapping physical frame `phys` with `flags`.
    fn new_leaf(phys: PhysAddr, flags: PageFlags) -> Pte {
        Pte((phys.as_u64() & PTE_ADDR_MASK) | PTE_PRESENT | flags_to_pte_bits(flags))
    }
}

/// Translate architecture-neutral [`PageFlags`] into x86_64 entry bits.
/// The present bit is added by [`Pte::new_leaf`]; this handles only the
/// permission and caching attributes.
fn flags_to_pte_bits(flags: PageFlags) -> u64 {
    let mut bits = 0;
    if flags.contains(PageFlags::WRITABLE) {
        bits |= PTE_WRITABLE;
    }
    if flags.contains(PageFlags::USER) {
        bits |= PTE_USER;
    }
    if flags.contains(PageFlags::NO_EXECUTE) {
        bits |= PTE_NX;
    }
    if flags.contains(PageFlags::GLOBAL) {
        bits |= PTE_GLOBAL;
    }
    if flags.contains(PageFlags::NO_CACHE) {
        bits |= PTE_PCD;
    }
    if flags.contains(PageFlags::WRITE_THROUGH) {
        bits |= PTE_PWT;
    }
    bits
}

// --- Virtual-address index split (9-9-9-9-12) ---------------------------

/// PML4 index of `v` — virtual-address bits 47:39.
const fn pml4_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 39) & 0x1FF) as usize
}

/// PDPT index of `v` — virtual-address bits 38:30.
const fn pdpt_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 30) & 0x1FF) as usize
}

/// PD index of `v` — virtual-address bits 29:21.
const fn pd_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 21) & 0x1FF) as usize
}

/// PT index of `v` — virtual-address bits 20:12.
const fn pt_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 12) & 0x1FF) as usize
}

/// 4 KiB page offset of `v` — virtual-address bits 11:0.
const fn page_offset(v: VirtAddr) -> u64 {
    v.as_u64() & 0xFFF
}

// --- Table access -------------------------------------------------------

/// Pointer to the 512-entry page table at physical address `table`, via
/// the higher-half direct map.
///
/// Constructing the pointer is safe; dereferencing it is sound only while
/// `table` genuinely holds a live page-table frame.
fn table_ptr(table: PhysAddr) -> *mut Pte {
    (table.as_u64() + crate::mm::heap::hhdm_offset()) as *mut Pte
}

/// Allocate one zeroed 4 KiB physical frame to use as a page table.
/// Returns `None` if the physical frame allocator is exhausted.
///
/// The frame is page-aligned (the buddy allocator guarantees this for
/// order-0 allocations) and reachable through the HHDM.
fn alloc_page_table() -> Option<PhysAddr> {
    let frame = crate::mm::heap::buddy_alloc(0)?;
    // SAFETY: `buddy_alloc(0)` returned a fresh, exclusively-owned 4 KiB
    // frame; `table_ptr` addresses it through the HHDM. We are its only
    // writer and zero all `PAGE_SIZE` bytes, so every entry reads as
    // not-present before the frame is linked into a table tree.
    unsafe {
        core::ptr::write_bytes(table_ptr(frame).cast::<u8>(), 0, PAGE_SIZE);
    }
    Some(frame)
}

/// Return the child table referenced by slot `index` of `table`,
/// allocating and linking a fresh zeroed table if the slot is empty.
///
/// # Safety
/// `table` must point at a valid 512-entry page table reachable through
/// the HHDM, and `index` must be `< 512`.
unsafe fn ensure_table(table: *mut Pte, index: usize) -> Result<*mut Pte, MapError> {
    // SAFETY: the caller guarantees `table` is a valid 512-entry table
    // and `index < 512`, so `table.add(index)` is in bounds. A present
    // entry points at a real child table — this API creates no huge
    // pages — and `alloc_page_table` yields a zeroed, HHDM-reachable
    // frame.
    unsafe {
        let slot = table.add(index);
        let entry = *slot;
        if entry.is_present() {
            Ok(table_ptr(entry.phys()))
        } else {
            let child = alloc_page_table().ok_or(MapError::OutOfMemory)?;
            *slot = Pte::new_table(child);
            Ok(table_ptr(child))
        }
    }
}

/// Walk PML4 → PDPT → PD in the tree rooted at `root`, returning a
/// pointer to the page table that holds the leaf entry for `virt`, or
/// `None` if any intermediate table is absent.
///
/// # Safety
/// `root` must be the physical base of a valid top-level page table
/// reachable through the HHDM, with no huge-page entries (the property
/// every tree built by this module has).
unsafe fn walk_to_pt(root: PhysAddr, virt: VirtAddr) -> Option<*mut Pte> {
    // SAFETY: per the contract `root` is a valid table; each present
    // entry therefore points at a real child table, also HHDM-reachable.
    // Every `.add(index)` is in bounds — the index helpers mask to
    // 0..512 and each table is exactly 512 entries.
    unsafe {
        let pml4e = *table_ptr(root).add(pml4_index(virt));
        if !pml4e.is_present() {
            return None;
        }
        let pdpte = *table_ptr(pml4e.phys()).add(pdpt_index(virt));
        if !pdpte.is_present() {
            return None;
        }
        let pde = *table_ptr(pdpte.phys()).add(pd_index(virt));
        if !pde.is_present() {
            return None;
        }
        Some(table_ptr(pde.phys()))
    }
}

// --- Kernel-half PML4 template ------------------------------------------
//
// On 4-level x86_64 paging the kernel half spans PML4 entries 256..512
// (canonical addresses `0xFFFF_8000_0000_0000` and up). Every process
// address space must see the same kernel-half mappings as the boot
// address space — otherwise switching CR3 to it would unmap the
// currently-executing kernel code, stack, and HHDM. We achieve this by
// snapshotting the kernel-half PML4 entries at boot and copying them
// into every new top-level table at construction time. The entries
// point at intermediate page tables (PDPTs) which are then shared
// across all address spaces, so modifications at the leaf level
// (future kernel-vmap allocations, for example) propagate to every AS
// automatically.

/// First PML4 entry that belongs to the kernel half on 4-level paging.
const KERNEL_PML4_BASE: usize = 256;
/// Number of PML4 entries in the kernel half.
const KERNEL_PML4_COUNT: usize = PAGE_TABLE_ENTRIES - KERNEL_PML4_BASE;

/// Captured kernel-half PML4 entries used by
/// [`X86Paging::inherit_kernel_mappings`]. `None` until
/// [`init_kernel_template`] runs at boot.
///
/// Stored as raw `u64` PML4 entries: the entries themselves are
/// inert data (they only become live page-table references when
/// the array is copied into a PML4 frame and that frame is loaded
/// into `CR3`).
static KERNEL_TEMPLATE: SpinLock<Option<[u64; KERNEL_PML4_COUNT]>> = SpinLock::new(None);

/// Read entries `256..512` from the PML4 at `root` into a fresh array.
///
/// # Safety
/// `root` must be the physical base of a valid PML4 reachable through
/// the HHDM.
unsafe fn read_kernel_half_entries(root: PhysAddr) -> [u64; KERNEL_PML4_COUNT] {
    // SAFETY: forwarded from this function's contract; we read 256
    // 8-byte entries from a 512-entry page-aligned table.
    unsafe {
        let pml4 = (root.as_u64() + heap::hhdm_offset()) as *const u64;
        let mut out = [0u64; KERNEL_PML4_COUNT];
        let mut i = 0;
        while i < KERNEL_PML4_COUNT {
            out[i] = *pml4.add(KERNEL_PML4_BASE + i);
            i += 1;
        }
        out
    }
}

/// Write `entries` into the kernel-half slots of the PML4 at `root`.
/// Entries `0..256` (the user half) are left untouched.
///
/// # Safety
/// `root` must be the physical base of a writable PML4 reachable
/// through the HHDM and owned by the caller.
unsafe fn write_kernel_half_entries(root: PhysAddr, entries: &[u64; KERNEL_PML4_COUNT]) {
    // SAFETY: forwarded from this function's contract; we write 256
    // entries into a 512-entry page-aligned table.
    unsafe {
        let pml4 = (root.as_u64() + heap::hhdm_offset()) as *mut u64;
        let mut i = 0;
        while i < KERNEL_PML4_COUNT {
            *pml4.add(KERNEL_PML4_BASE + i) = entries[i];
            i += 1;
        }
    }
}

// --- ArchPaging implementation ------------------------------------------

/// The x86_64 implementation of [`ArchPaging`].
///
/// A zero-sized type: the page-table root is an explicit argument to
/// every operation, so there is no per-instance state. Re-exported as
/// `crate::arch::Paging`.
pub struct X86Paging;

impl ArchPaging for X86Paging {
    unsafe fn map_page(
        root: PhysAddr,
        virt: VirtAddr,
        phys: PhysAddr,
        flags: PageFlags,
    ) -> Result<(), MapError> {
        if !virt.is_canonical() || !virt.is_page_aligned() || !phys.is_page_aligned() {
            return Err(MapError::Misaligned);
        }
        // SAFETY: the caller guarantees `root` is a valid, caller-owned
        // top-level table reachable via the HHDM. `ensure_table`
        // allocates and links any missing child and returns a valid
        // 512-entry table, so each indexed access stays in bounds. The
        // tree holds no huge pages (this API never creates them), so
        // every present non-leaf entry is genuinely a table.
        unsafe {
            let pdpt = ensure_table(table_ptr(root), pml4_index(virt))?;
            let pd = ensure_table(pdpt, pdpt_index(virt))?;
            let pt = ensure_table(pd, pd_index(virt))?;
            let leaf = pt.add(pt_index(virt));
            if (*leaf).is_present() {
                return Err(MapError::AlreadyMapped);
            }
            *leaf = Pte::new_leaf(phys, flags);
        }
        Ok(())
    }

    unsafe fn unmap_page(root: PhysAddr, virt: VirtAddr) -> Result<PhysAddr, UnmapError> {
        if !virt.is_canonical() || !virt.is_page_aligned() {
            return Err(UnmapError::Misaligned);
        }
        // SAFETY: the caller guarantees `root` is a valid, caller-owned
        // top-level table reachable via the HHDM with no huge pages.
        // `walk_to_pt` returns a valid 512-entry table; `pt_index` masks
        // to 0..512, so the leaf access is in bounds.
        unsafe {
            let pt = walk_to_pt(root, virt).ok_or(UnmapError::NotMapped)?;
            let leaf = pt.add(pt_index(virt));
            let entry = *leaf;
            if !entry.is_present() {
                return Err(UnmapError::NotMapped);
            }
            *leaf = Pte::empty();
            // TODO: reclaim intermediate tables that are now empty —
            // deferred (see docs/rationale/deferred-decisions.md).
            Ok(entry.phys())
        }
    }

    #[cfg(not(test))]
    unsafe fn flush_tlb_page(virt: VirtAddr) {
        // SAFETY: `invlpg` is a ring-0 instruction — the only ring the
        // kernel runs in. The caller owns the page-table change this
        // invalidation reflects.
        unsafe {
            regs::invlpg(virt.as_u64());
        }
    }

    // Host tests run in ring 3, where `invlpg` `#GP`s; the page-table *memory*
    // edits are what the tests exercise (via HHDM), and there is no TLB to flush
    // under `cargo test`. Mirrors the `current_cpu`/`init_this_cpu` cfg(test)
    // stubs in `smp.rs`.
    #[cfg(test)]
    unsafe fn flush_tlb_page(_virt: VirtAddr) {}

    #[cfg(not(test))]
    unsafe fn flush_tlb_all() {
        // SAFETY: reloading CR3 with the value it already holds is sound
        // in ring 0; it leaves the active address space unchanged while
        // dropping every non-global TLB entry.
        unsafe {
            regs::write_cr3(regs::read_cr3());
        }
    }

    // Host-test stub: `mov cr3` is privileged (see `flush_tlb_page`).
    #[cfg(test)]
    unsafe fn flush_tlb_all() {}

    unsafe fn set_page_table(root: PhysAddr) {
        // SAFETY: forwarded to the caller — per `ArchPaging::set_page_table`
        // `root` must be a fully-formed top-level table. It is a
        // page-aligned frame, so the low 12 bits are zero and no stale
        // PCD/PWT control bits are carried into CR3.
        unsafe {
            regs::write_cr3(root.as_u64());
        }
    }

    unsafe fn inherit_kernel_mappings(root: PhysAddr) {
        let template = KERNEL_TEMPLATE.lock();
        let entries = template
            .as_ref()
            .expect("inherit_kernel_mappings called before init_kernel_template");
        // SAFETY: forwarded from `ArchPaging::inherit_kernel_mappings` —
        // `root` is a writable PML4 owned by the caller, reachable via
        // HHDM. We touch only entries 256..512 (the kernel half);
        // entries 0..256 (the user half) are preserved.
        unsafe {
            write_kernel_half_entries(root, entries);
        }
    }

    unsafe fn ensure_kernel_intermediate(
        root: PhysAddr,
        virt: VirtAddr,
    ) -> Result<(), MapError> {
        let index = pml4_index(virt);
        // SAFETY: forwarded from `ArchPaging::ensure_kernel_intermediate` —
        // `root` is a valid PML4 reachable through HHDM.
        unsafe {
            let pml4 = table_ptr(root);
            let entry = *pml4.add(index);
            if entry.is_present() {
                return Ok(());
            }
            let new_pdpt = alloc_page_table().ok_or(MapError::OutOfMemory)?;
            *pml4.add(index) = Pte::new_table(new_pdpt);
            Ok(())
        }
    }

    unsafe fn translate(root: PhysAddr, virt: VirtAddr) -> Option<PhysAddr> {
        if !virt.is_canonical() {
            return None;
        }
        // SAFETY: per the contract `root` is a valid table; each present
        // non-huge entry points at a real child table reachable via the
        // HHDM. Indices are masked to 0..512, so every access is in bounds.
        // Understands 2 MiB / 1 GiB huge pages, so it is correct against the
        // bootloader's live tables.
        unsafe {
            let pml4e = *table_ptr(root).add(pml4_index(virt));
            if !pml4e.is_present() {
                return None;
            }
            let pdpte = *table_ptr(pml4e.phys()).add(pdpt_index(virt));
            if !pdpte.is_present() {
                return None;
            }
            if pdpte.is_huge() {
                // 1 GiB page: frame base in bits 51:30, offset in bits 29:0.
                return Some(PhysAddr::new(pdpte.phys().as_u64() | (virt.as_u64() & 0x3FFF_FFFF)));
            }
            let pde = *table_ptr(pdpte.phys()).add(pd_index(virt));
            if !pde.is_present() {
                return None;
            }
            if pde.is_huge() {
                // 2 MiB page: frame base in bits 51:21, offset in bits 20:0.
                return Some(PhysAddr::new(pde.phys().as_u64() | (virt.as_u64() & 0x1F_FFFF)));
            }
            let pte = *table_ptr(pde.phys()).add(pt_index(virt));
            if !pte.is_present() {
                return None;
            }
            Some(PhysAddr::new(pte.phys().as_u64() | page_offset(virt)))
        }
    }

    fn active_root() -> PhysAddr {
        PhysAddr::new(regs::read_cr3() & PTE_ADDR_MASK)
    }

    unsafe fn init_kernel_template(boot_root: PhysAddr) {
        // SAFETY: forwarded from `ArchPaging::init_kernel_template` —
        // `boot_root` points at a live PML4 reachable via the HHDM. The
        // entries at indices 256..512 are read but not modified.
        let entries = unsafe { read_kernel_half_entries(boot_root) };
        *KERNEL_TEMPLATE.lock() = Some(entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn index_split_isolates_each_level() {
        // A distinct value in each 9-bit window plus a page offset.
        let v = VirtAddr::new((1 << 39) | (2 << 30) | (3 << 21) | (4 << 12) | 0x5);
        assert_eq!(pml4_index(v), 1);
        assert_eq!(pdpt_index(v), 2);
        assert_eq!(pd_index(v), 3);
        assert_eq!(pt_index(v), 4);
        assert_eq!(page_offset(v), 0x5);
    }

    #[test]
    fn index_helpers_mask_to_nine_bits() {
        let v = VirtAddr::new(u64::MAX);
        assert_eq!(pml4_index(v), 0x1FF);
        assert_eq!(pdpt_index(v), 0x1FF);
        assert_eq!(pd_index(v), 0x1FF);
        assert_eq!(pt_index(v), 0x1FF);
        assert_eq!(page_offset(v), 0xFFF);
    }

    #[test]
    fn flags_translate_to_expected_bits() {
        assert_eq!(flags_to_pte_bits(PageFlags::empty()), 0);
        assert_eq!(flags_to_pte_bits(PageFlags::WRITABLE), PTE_WRITABLE);
        assert_eq!(flags_to_pte_bits(PageFlags::USER), PTE_USER);
        assert_eq!(flags_to_pte_bits(PageFlags::NO_EXECUTE), PTE_NX);
        assert_eq!(flags_to_pte_bits(PageFlags::GLOBAL), PTE_GLOBAL);
        assert_eq!(flags_to_pte_bits(PageFlags::NO_CACHE), PTE_PCD);
        assert_eq!(flags_to_pte_bits(PageFlags::WRITE_THROUGH), PTE_PWT);
        let combo = PageFlags::WRITABLE | PageFlags::USER | PageFlags::NO_EXECUTE;
        assert_eq!(flags_to_pte_bits(combo), PTE_WRITABLE | PTE_USER | PTE_NX);
    }

    #[test]
    fn pte_leaf_round_trip() {
        let phys = PhysAddr::new(0x1234_5000);
        let pte = Pte::new_leaf(phys, PageFlags::WRITABLE);
        assert!(pte.is_present());
        assert!(!pte.is_huge());
        assert_eq!(pte.phys(), phys);
        assert_eq!(pte.0 & PTE_WRITABLE, PTE_WRITABLE);
        assert!(!Pte::empty().is_present());
    }

    #[test]
    fn pte_table_entry_is_present_writable_user() {
        let child = PhysAddr::new(0x9_A000);
        let pte = Pte::new_table(child);
        assert!(pte.is_present());
        assert_eq!(pte.phys(), child);
        assert_eq!(pte.0 & PTE_WRITABLE, PTE_WRITABLE);
        assert_eq!(pte.0 & PTE_USER, PTE_USER);
    }

    #[test]
    fn map_translate_unmap_round_trip() {
        init_global_heap();
        let root = alloc_page_table().expect("root frame");
        // Any owned, page-aligned frame stands in for the target page.
        let phys = alloc_page_table().expect("target frame");
        let virt = VirtAddr::new(0x4000_0000);

        // SAFETY: `root` is a freshly zeroed table; `virt`/`phys` are
        // page-aligned and `virt` is canonical; the host heap hands out
        // HHDM-reachable frames (HHDM offset 0 under `init_global_heap`).
        unsafe {
            X86Paging::map_page(root, virt, phys, PageFlags::WRITABLE).unwrap();
            assert_eq!(X86Paging::translate(root, virt), Some(phys));
        }
    }

    #[test]
    fn double_map_is_rejected() {
        init_global_heap();
        let root = alloc_page_table().unwrap();
        let phys = alloc_page_table().unwrap();
        let virt = VirtAddr::new(0x8000_0000);
        // SAFETY: see `map_translate_unmap_round_trip`.
        unsafe {
            X86Paging::map_page(root, virt, phys, PageFlags::empty()).unwrap();
            assert_eq!(
                X86Paging::map_page(root, virt, phys, PageFlags::empty()),
                Err(MapError::AlreadyMapped),
            );
        }
    }

    #[test]
    fn unmap_returns_frame_then_reports_not_mapped() {
        init_global_heap();
        let root = alloc_page_table().unwrap();
        let phys = alloc_page_table().unwrap();
        let virt = VirtAddr::new(0xC000_0000);
        // SAFETY: see `map_translate_unmap_round_trip`.
        unsafe {
            X86Paging::map_page(root, virt, phys, PageFlags::WRITABLE).unwrap();
            assert_eq!(X86Paging::unmap_page(root, virt), Ok(phys));
            assert_eq!(X86Paging::translate(root, virt), None);
            assert_eq!(X86Paging::unmap_page(root, virt), Err(UnmapError::NotMapped));
        }
    }

    #[test]
    fn unmap_of_never_mapped_address_reports_not_mapped() {
        init_global_heap();
        let root = alloc_page_table().unwrap();
        let virt = VirtAddr::new(0x1_0000_0000);
        // SAFETY: `root` is a valid, empty table; nothing is mapped.
        unsafe {
            assert_eq!(X86Paging::unmap_page(root, virt), Err(UnmapError::NotMapped));
        }
    }

    #[test]
    fn misaligned_and_noncanonical_inputs_are_rejected() {
        init_global_heap();
        let root = alloc_page_table().unwrap();
        let phys = alloc_page_table().unwrap();
        // SAFETY: `root` is a valid, empty table; each call fails an
        // alignment or canonical check before any table walk.
        unsafe {
            assert_eq!(
                X86Paging::map_page(root, VirtAddr::new(0x1234), phys, PageFlags::empty()),
                Err(MapError::Misaligned),
                "unaligned virtual address",
            );
            assert_eq!(
                X86Paging::map_page(
                    root,
                    VirtAddr::new(0x0001_0000_0000_0000),
                    phys,
                    PageFlags::empty(),
                ),
                Err(MapError::Misaligned),
                "non-canonical virtual address",
            );
            assert_eq!(
                X86Paging::map_page(
                    root,
                    VirtAddr::new(0x1000),
                    PhysAddr::new(0x800),
                    PageFlags::empty(),
                ),
                Err(MapError::Misaligned),
                "unaligned physical address",
            );
        }
    }

    #[test]
    fn maps_spanning_distinct_top_level_entries() {
        init_global_heap();
        let root = alloc_page_table().unwrap();
        // Three addresses in distinct PML4 slots, forcing intermediate
        // table allocation at every level for each.
        let cases = [
            VirtAddr::new(0x0000_0000_0000_1000), // PML4 slot 0
            VirtAddr::new(0x0000_0080_0000_1000), // PML4 slot 1 (512 GiB)
            VirtAddr::new(0x0000_0100_0000_1000), // PML4 slot 2 (1 TiB)
        ];
        for v in cases {
            let phys = alloc_page_table().unwrap();
            // SAFETY: see `map_translate_unmap_round_trip`.
            unsafe {
                X86Paging::map_page(root, v, phys, PageFlags::WRITABLE).unwrap();
                assert_eq!(X86Paging::translate(root, v), Some(phys));
            }
        }
    }

    // ----- Kernel-half PML4 template -----

    /// Fill a freshly-allocated PML4 frame with `value` in every entry,
    /// returning its physical base.
    fn fill_pml4(value: u64) -> PhysAddr {
        let frame = alloc_page_table().unwrap();
        // SAFETY: just allocated, page-aligned, HHDM-reachable; we write
        // exactly the 512 entries the frame holds.
        unsafe {
            let pml4 = (frame.as_u64() + heap::hhdm_offset()) as *mut u64;
            for i in 0..PAGE_TABLE_ENTRIES {
                *pml4.add(i) = value.wrapping_add(i as u64);
            }
        }
        frame
    }

    /// Read all 512 entries from a PML4 frame.
    fn read_all_entries(frame: PhysAddr) -> [u64; PAGE_TABLE_ENTRIES] {
        let mut out = [0u64; PAGE_TABLE_ENTRIES];
        // SAFETY: PML4 is page-aligned and HHDM-reachable.
        unsafe {
            let pml4 = (frame.as_u64() + heap::hhdm_offset()) as *const u64;
            for i in 0..PAGE_TABLE_ENTRIES {
                out[i] = *pml4.add(i);
            }
        }
        out
    }

    #[test]
    fn read_kernel_half_entries_captures_only_kernel_half() {
        init_global_heap();
        // Base of 0xDEAD_0000_0000_0000 + index gives a distinct value
        // per entry across both halves.
        let source = fill_pml4(0xDEAD_0000_0000_0000);

        // SAFETY: `source` was just constructed and is HHDM-reachable.
        let captured = unsafe { read_kernel_half_entries(source) };

        for i in 0..KERNEL_PML4_COUNT {
            let expected = 0xDEAD_0000_0000_0000u64
                .wrapping_add((KERNEL_PML4_BASE + i) as u64);
            assert_eq!(
                captured[i], expected,
                "kernel entry {i} (PML4 slot {}): expected {expected:#x}, got {:#x}",
                KERNEL_PML4_BASE + i,
                captured[i]
            );
        }
    }

    #[test]
    fn write_kernel_half_entries_preserves_user_half() {
        init_global_heap();
        // Target PML4 prepopulated with a user-half-marker pattern so we
        // can verify the write doesn't touch entries 0..256.
        let target = fill_pml4(0xC0DE_0000_0000_0000);
        let entries: [u64; KERNEL_PML4_COUNT] =
            core::array::from_fn(|i| 0xBEEF_0000_0000_0000u64.wrapping_add(i as u64));

        // SAFETY: `target` is page-aligned and HHDM-reachable.
        unsafe { write_kernel_half_entries(target, &entries) };

        let after = read_all_entries(target);
        for i in 0..KERNEL_PML4_BASE {
            // User-half entries untouched: they still match the original
            // `fill_pml4` pattern.
            let expected = 0xC0DE_0000_0000_0000u64.wrapping_add(i as u64);
            assert_eq!(after[i], expected, "user-half entry {i} was modified");
        }
        for i in 0..KERNEL_PML4_COUNT {
            assert_eq!(
                after[KERNEL_PML4_BASE + i],
                entries[i],
                "kernel-half entry {i} was not written"
            );
        }
    }

    #[test]
    fn read_then_write_round_trips_the_kernel_half() {
        init_global_heap();
        let source = fill_pml4(0xFEED_0000_0000_0000);
        // SAFETY: see fill_pml4.
        let captured = unsafe { read_kernel_half_entries(source) };

        let target = alloc_page_table().unwrap();
        // Zero the target so any leak through from `alloc_page_table` is
        // a clear bug.
        // SAFETY: just allocated, page-aligned, HHDM-reachable.
        unsafe {
            let pml4 = (target.as_u64() + heap::hhdm_offset()) as *mut u8;
            core::ptr::write_bytes(pml4, 0, PAGE_SIZE);
            write_kernel_half_entries(target, &captured);
        }

        let after = read_all_entries(target);
        for i in 0..KERNEL_PML4_BASE {
            assert_eq!(after[i], 0, "user half not zero at entry {i}");
        }
        for i in 0..KERNEL_PML4_COUNT {
            let expected = 0xFEED_0000_0000_0000u64
                .wrapping_add((KERNEL_PML4_BASE + i) as u64);
            assert_eq!(
                after[KERNEL_PML4_BASE + i],
                expected,
                "round-trip mismatch at kernel entry {i}"
            );
        }
    }
}
