//! x86_64 4-level page-table management: the [`ArchPaging`] implementation, the
//! page-table-entry format, and the table walk.
//!
//! A 48-bit canonical address is translated through PML4 → PDPT → PD → PT, each a 4 KiB frame
//! of 512 8-byte entries indexed by a 9-bit slice; the low 12 bits are the page offset.
//! [`map_page`](X86Paging::map_page) maps only 4 KiB leaves and never sets `PS`, but
//! [`translate`] understands huge pages because it also walks Limine's live tables, which may
//! use 2 MiB or 1 GiB pages.
//!
//! Physical frames are reached through the higher-half direct map (HHDM): a table at physical
//! address `p` is at `p + hhdm_offset()`. 5-level paging is a documented non-goal — see
//! `docs/rationale/deferred-decisions.md`.

use crate::arch::paging::{ArchPaging, MapError, PageFlags, UnmapError};
use crate::arch::x86_64::regs;
use crate::libkern::SpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::frame::Frame;
use crate::mm::{PAGE_SIZE, PhysAddr, VirtAddr};
use core::sync::atomic::{AtomicU64, Ordering};

const PAGE_TABLE_ENTRIES: usize = 512;

// One page table is exactly one page frame, which the HHDM walk and `new_table` rely on.
const _: () = assert!(size_of::<[Pte; PAGE_TABLE_ENTRIES]>() == PAGE_SIZE);
const _: () = assert!(size_of::<[AtomicU64; PAGE_TABLE_ENTRIES]>() == PAGE_SIZE);

// --- Page-table-entry bits ---

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
/// Page size: at a non-leaf level the entry maps a huge page directly. Never set here;
/// [`translate`] reads it.
const PTE_HUGE: u64 = 1 << 7;
/// A 4 KiB leaf's `PAT` bit — the high bit of its attribute-table index.
const PTE_PAT: u64 = 1 << 7;
/// A 2 MiB or 1 GiB leaf's `PAT` bit, moved because bit 7 means "huge" there.
const PTE_HUGE_PAT: u64 = 1 << 12;
/// Global — the translation survives a CR3 reload.
const PTE_GLOBAL: u64 = 1 << 8;
/// No-execute. Faults unless `EFER.NXE` is set; see [`ensure_nxe`].
const PTE_NX: u64 = 1 << 63;

/// Physical-address field of an entry: bits 51:12.
const PTE_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

// --- Page-table entry ---------------------------------------------------

/// A single x86_64 page-table entry, used at every level. `#[repr(transparent)]` over `u64`
/// so `[Pte; 512]` is exactly one frame.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq)]
struct Pte(u64);

impl Pte {
    const fn empty() -> Pte {
        Pte(0)
    }

    const fn is_present(self) -> bool {
        self.0 & PTE_PRESENT != 0
    }

    const fn is_huge(self) -> bool {
        self.0 & PTE_HUGE != 0
    }

    const fn phys(self) -> PhysAddr {
        PhysAddr::new(self.0 & PTE_ADDR_MASK)
    }

    /// A non-leaf entry pointing at child table `child`.
    /// Intermediate entries are writable and user-accessible: the *leaf* entry's flags decide
    /// a mapping's effective permission, so one sub-tree can hold kernel and user leaves.
    fn new_table(child: PhysAddr) -> Pte {
        Pte((child.as_u64() & PTE_ADDR_MASK) | PTE_PRESENT | PTE_WRITABLE | PTE_USER)
    }

    fn new_leaf(phys: PhysAddr, flags: PageFlags) -> Pte {
        Pte((phys.as_u64() & PTE_ADDR_MASK) | PTE_PRESENT | flags_to_pte_bits(flags))
    }
}

/// Translate architecture-neutral [`PageFlags`] into x86_64 entry bits, permission and caching
/// only; the present bit is added by [`Pte::new_leaf`].
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
    // Entry 5 of the table this kernel programs (`KERNEL_PAT`): `PAT` and `PWT`, no `PCD`.
    if flags.contains(PageFlags::WRITE_COMBINING) {
        bits |= PTE_PAT | PTE_PWT;
    }
    bits
}

// --- Virtual-address index split (9-9-9-9-12) ---------------------------

const fn pml4_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 39) & 0x1FF) as usize
}

const fn pdpt_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 30) & 0x1FF) as usize
}

const fn pd_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 21) & 0x1FF) as usize
}

const fn pt_index(v: VirtAddr) -> usize {
    ((v.as_u64() >> 12) & 0x1FF) as usize
}

/// The offset of `v` in the page a leaf at `level` maps: 4 KiB, 2 MiB or 1 GiB.
const fn leaf_offset(level: u8, v: VirtAddr) -> u64 {
    v.as_u64() & ((1 << (12 + 9 * (level as u32 - 1))) - 1)
}

// --- Leaf entries, whose bits move with the page size ---
//
// A leaf's `PAT` bit is bit 7 in a 4 KiB entry and bit 12 in a 2 MiB or 1 GiB one (bit 7 says
// huge there). The one huge mapping this kernel reads selects the same entry whichever bit is
// read, so no boot can tell a wrong bit from a right one; the host tests do.

/// The physical frame a leaf entry names.
///
/// **Bit 12 is part of the frame on a 4 KiB entry and the `PAT` bit on a huge one**, so a huge
/// entry's base must drop it: a write-combining 2 MiB page whose `PAT` bit is set otherwise
/// translates 4 KiB too high.
const fn frame_base(raw: u64, huge: bool) -> u64 {
    let mask = if huge { PTE_ADDR_MASK & !PTE_HUGE_PAT } else { PTE_ADDR_MASK };
    raw & mask
}

/// The attribute-table entry a leaf selects: `PAT:PCD:PWT`, one of eight.
const fn attribute_of(raw: u64, huge: bool) -> u8 {
    let pat = if huge { (raw & PTE_HUGE_PAT) != 0 } else { (raw & PTE_PAT) != 0 };
    (((pat as u64) << 2) | (raw >> 4 & 1) << 1 | (raw >> 3 & 1)) as u8
}

/// The page-attribute index the mapping of `virt` selects (`PAT:PCD:PWT`), or `None` if `virt`
/// is not mapped. The bit arithmetic is [`attribute_of`]'s.
///
/// # Safety
/// As [`TableRef::root`].
pub(crate) unsafe fn attribute_index(root: PhysAddr, virt: VirtAddr) -> Option<u8> {
    // SAFETY: forwarded.
    unsafe { walk(root, virt) }.map(|(leaf, level)| attribute_of(leaf.0, level > 1))
}

// --- Table access -------------------------------------------------------

/// Pointer to the 512-entry page table at physical address `table`, via the HHDM. Dereferencing
/// is sound only while `table` holds a live page-table frame.
fn table_ptr(table: PhysAddr) -> *mut Pte {
    crate::mm::frame::hhdm(table)
}

/// A page table at `level` (4 a PML4, 1 a table of leaves), each entry atomic: the kernel half is
/// shared and edited under no lock. **Invariant**: a present entry above level 1 that is not huge
/// names a live table one level down.
#[derive(Clone, Copy)]
struct TableRef<'a> {
    entries: &'a [AtomicU64; PAGE_TABLE_ENTRIES],
    level: u8,
}

impl<'a> TableRef<'a> {
    /// The top-level table at `root`.
    ///
    /// # Safety
    /// `root` is a live PML4 reachable through the HHDM for `'a`, and each present entry above
    /// level 1 that is not huge, anywhere in its tree, names a live table one level down.
    unsafe fn root(root: PhysAddr) -> Self {
        // SAFETY: forwarded.
        unsafe { Self::at(root, 4) }
    }

    /// The table at `table`, at `level` of a tree as [`root`](Self::root)'s.
    ///
    /// # Safety
    /// As [`root`](Self::root), for the table at `table`.
    unsafe fn at(table: PhysAddr, level: u8) -> Self {
        // SAFETY: by the contract, a live table: one page of 512 aligned `u64`s, the layout of
        // `[AtomicU64; 512]`, written by others only atomically.
        Self { entries: unsafe { &*table_ptr(table).cast::<[AtomicU64; PAGE_TABLE_ENTRIES]>() }, level }
    }

    fn entry(self, index: usize) -> Pte {
        Pte(self.entries[index].load(Ordering::Acquire))
    }

    fn set(self, index: usize, pte: Pte) {
        self.entries[index].store(pte.0, Ordering::Release);
    }

    /// Store `new` at `index` if it still holds `old`, as read empty; `false` if another CPU won.
    fn link(self, index: usize, old: Pte, new: Pte) -> bool {
        self.entries[index].compare_exchange(old.0, new.0, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// What slot `index` holds, from one read: a second could see a table linked since.
    fn slot(self, index: usize) -> Slot<'a> {
        let e = self.entry(index);
        match (e.is_present(), self.level > 1 && !e.is_huge()) {
            (false, _) => Slot::Empty(e),
            // SAFETY: the type's invariant: such an entry names a live table one level down.
            (true, true) => Slot::Table(unsafe { Self::at(e.phys(), self.level - 1) }),
            (true, false) => Slot::Leaf(e),
        }
    }

    /// The slot of `virt` in this table.
    fn index_of(self, virt: VirtAddr) -> usize {
        match self.level {
            4 => pml4_index(virt),
            3 => pdpt_index(virt),
            2 => pd_index(virt),
            _ => pt_index(virt),
        }
    }
}

enum Slot<'a> {
    Empty(Pte),
    Table(TableRef<'a>),
    Leaf(Pte),
}

/// The leaf entry mapping `virt` under `root`, and its level (1 a 4 KiB page, 2 a 2 MiB one, 3 a
/// 1 GiB one), huge pages included, as on the bootloader's tables; `None` if `virt` is unmapped.
///
/// # Safety
/// As [`TableRef::root`].
unsafe fn walk(root: PhysAddr, virt: VirtAddr) -> Option<(Pte, u8)> {
    if !virt.is_canonical() {
        return None;
    }
    // SAFETY: forwarded.
    let mut table = unsafe { TableRef::root(root) };
    loop {
        match table.slot(table.index_of(virt)) {
            Slot::Table(next) => table = next,
            Slot::Leaf(leaf) => return (table.level < 4).then_some((leaf, table.level)),
            Slot::Empty(_) => return None,
        }
    }
}

/// A zeroed frame to use as a page table, or `None` if the frame allocator is exhausted.
fn new_table() -> Option<Frame> {
    #[cfg(test)]
    if TABLES_LEFT.with(|left| left.replace(left.get().map(|n| n.saturating_sub(1))) == Some(0)) {
        return None;
    }
    // Zeroed, every entry reads as not present before the frame is linked into a tree.
    Frame::alloc_zeroed()
}

/// The child table slot `index` of `table` names, linking a fresh one if it is empty.
fn ensure_table(table: TableRef<'_>, index: usize) -> Result<TableRef<'_>, MapError> {
    // The kernel half's tables are shared and edited under no lock: two CPUs making kernel
    // stacks in one fresh region can find a slot empty together. The link is a compare-and-swap,
    // and the CPU that loses gives its table back and takes the winner's.
    debug_assert!(table.level > 1, "a page table's entries are leaves");
    let mut child = None;
    loop {
        let empty = match table.slot(index) {
            Slot::Empty(e) => e,
            Slot::Table(t) => return Ok(t),
            Slot::Leaf(_) => return Err(MapError::AlreadyMapped),
        };
        let new = match child.take() {
            Some(c) => c,
            None => new_table().ok_or(MapError::OutOfMemory)?,
        };
        #[cfg(test)]
        if let Some(other_cpu) = BEFORE_LINK.take() {
            other_cpu(&table.entries[index]);
        }
        if table.link(index, empty, Pte::new_table(new.phys())) {
            // SAFETY: a fresh zeroed table, which holds no entries, now linked one level down.
            return Ok(unsafe { TableRef::at(new.into_pte(), table.level - 1) });
        }
        child = Some(new);
    }
}

/// The kernel half's table at `index` of `pml4`, which every root shares, so a map never links one.
fn shared_table(pml4: TableRef<'_>, index: usize) -> Result<TableRef<'_>, MapError> {
    match pml4.slot(index) {
        Slot::Table(t) => Ok(t),
        Slot::Empty(_) => Err(MapError::Unshared),
        Slot::Leaf(_) => Err(MapError::AlreadyMapped),
    }
}

#[cfg(test)]
std::thread_local! {
    static TABLES_LEFT: core::cell::Cell<Option<usize>> = const { core::cell::Cell::new(None) };
}

/// Let this thread allocate `n` more page tables, failing every one after, or any with `None`.
#[cfg(test)]
pub fn fail_page_tables_after(n: Option<usize>) {
    TABLES_LEFT.with(|left| left.set(n));
}

/// Whether `virt` is a 4 KiB page under `root` mapped with `flags` and nothing else.
///
/// # Safety
/// As [`TableRef::root`].
#[cfg(test)]
pub unsafe fn mapped_with(root: PhysAddr, virt: VirtAddr, flags: PageFlags) -> bool {
    // SAFETY: forwarded.
    let leaf = unsafe { walk_to_pt(root, virt) }.map(|pt| pt.entry(pt_index(virt)));
    leaf.is_some_and(|pte| pte.0 & !PTE_ADDR_MASK == PTE_PRESENT | flags_to_pte_bits(flags))
}

#[cfg(test)]
std::thread_local! {
    /// A test's stand-in for another CPU, run once between reading a slot empty and linking it.
    static BEFORE_LINK: core::cell::Cell<Option<fn(&AtomicU64)>> = const { core::cell::Cell::new(None) };
}

/// The page table holding `virt`'s leaf under `root`, or `None` if none does.
///
/// # Safety
/// As [`TableRef::root`].
unsafe fn walk_to_pt<'a>(root: PhysAddr, virt: VirtAddr) -> Option<TableRef<'a>> {
    // SAFETY: forwarded.
    let mut table = unsafe { TableRef::root(root) };
    while table.level > 1 {
        let Slot::Table(next) = table.slot(table.index_of(virt)) else { return None };
        table = next;
    }
    Some(table)
}

// --- Kernel-half PML4 template ---
//
// The kernel half is PML4 entries 256..512 (`0xFFFF_8000_0000_0000` and up). Every address
// space must share the boot space's kernel-half mappings or switching CR3 would unmap the
// running kernel, stack and HHDM, so they are snapshotted at boot and copied into every new
// top-level table. They point at shared PDPTs, so leaf-level changes propagate everywhere.

const KERNEL_PML4_BASE: usize = 256;
const KERNEL_PML4_COUNT: usize = PAGE_TABLE_ENTRIES - KERNEL_PML4_BASE;

/// Captured kernel-half PML4 entries for [`X86Paging::inherit_kernel_mappings`]; `None` until
/// [`init_kernel_template`] runs at boot. Raw `u64`s: inert until copied into a PML4 frame
/// that is loaded into `CR3`.
static KERNEL_TEMPLATE: SpinLock<Option<[u64; KERNEL_PML4_COUNT]>> = SpinLock::new(LockRank::KernelPmlTemplate, None);

/// Read entries `256..512` from the PML4 at `root` into a fresh array.
///
/// # Safety
/// As [`TableRef::root`].
unsafe fn read_kernel_half_entries(root: PhysAddr) -> [u64; KERNEL_PML4_COUNT] {
    // SAFETY: forwarded.
    let pml4 = unsafe { TableRef::root(root) };
    core::array::from_fn(|i| pml4.entry(KERNEL_PML4_BASE + i).0)
}

/// Write `entries` into the kernel-half slots of the PML4 at `root`.
/// Entries `0..256` (the user half) are left untouched.
///
/// # Safety
/// As [`TableRef::root`], for a PML4 the caller owns, and `entries` are a live PML4's.
unsafe fn write_kernel_half_entries(root: PhysAddr, entries: &[u64; KERNEL_PML4_COUNT]) {
    // SAFETY: forwarded.
    let pml4 = unsafe { TableRef::root(root) };
    for (i, &e) in entries.iter().enumerate() {
        pml4.set(KERNEL_PML4_BASE + i, Pte(e));
    }
}

// --- ArchPaging implementation ------------------------------------------

/// The x86_64 implementation of [`ArchPaging`].
///
/// A zero-sized type: the root is an explicit argument to every operation. Re-exported as
/// `crate::arch::Paging`.
pub struct X86Paging;

impl ArchPaging for X86Paging {
    unsafe fn map_page(root: PhysAddr, virt: VirtAddr, phys: PhysAddr, flags: PageFlags) -> Result<(), MapError> {
        if !virt.is_canonical() || !virt.is_page_aligned() || !phys.is_page_aligned() {
            return Err(MapError::Misaligned);
        }
        // SAFETY: the caller guarantees `root` is a valid, caller-owned top-level table.
        let pml4 = unsafe { TableRef::root(root) };
        let top = pml4_index(virt);
        let pdpt = if top < KERNEL_PML4_BASE { ensure_table(pml4, top)? } else { shared_table(pml4, top)? };
        let pt = ensure_table(ensure_table(pdpt, pdpt_index(virt))?, pd_index(virt))?;
        let i = pt_index(virt);
        match pt.slot(i) {
            Slot::Empty(old) if pt.link(i, old, Pte::new_leaf(phys, flags)) => Ok(()),
            _ => Err(MapError::AlreadyMapped),
        }
    }

    unsafe fn unmap_page(root: PhysAddr, virt: VirtAddr) -> Result<PhysAddr, UnmapError> {
        if !virt.is_canonical() || !virt.is_page_aligned() {
            return Err(UnmapError::Misaligned);
        }
        // SAFETY: the caller guarantees `root` is a valid, caller-owned top-level table.
        let pt = unsafe { walk_to_pt(root, virt) }.ok_or(UnmapError::NotMapped)?;
        let entry = pt.entry(pt_index(virt));
        if !entry.is_present() {
            return Err(UnmapError::NotMapped);
        }
        pt.set(pt_index(virt), Pte::empty());
        // TODO: reclaim intermediate tables that are now empty —
        // deferred (see docs/rationale/deferred-decisions.md). Reclaiming one breaks `translate`'s
        // promise for a loaded root, which safe code walks: `of_mapping`, the fault diagnostics.
        Ok(entry.phys())
    }

    #[cfg(not(test))]
    fn flush_tlb_page(virt: VirtAddr) {
        regs::invlpg(virt.as_u64());
    }

    // Host tests run in ring 3, where `invlpg` `#GP`s; they exercise the page-table memory
    // edits, and there is no TLB to flush under `cargo test`.
    #[cfg(test)]
    fn flush_tlb_page(_virt: VirtAddr) {}

    #[cfg(not(test))]
    fn flush_tlb_all() {
        // SAFETY: CR3 is written with the value just read, which is this thread's root however
        // the thread moves between the two: a thread's root never changes, and no PCID bits are
        // used. The active address space is unchanged; every non-global TLB entry is dropped.
        unsafe {
            regs::write_cr3(regs::read_cr3());
        }
    }

    // Host-test stub: `mov cr3` is privileged (see `flush_tlb_page`).
    #[cfg(test)]
    fn flush_tlb_all() {}

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
        let entries = template.as_ref().expect("inherit_kernel_mappings called before init_kernel_template");
        // SAFETY: forwarded from `ArchPaging::inherit_kernel_mappings` —
        // `root` is a writable PML4 owned by the caller, reachable via
        // HHDM. We touch only entries 256..512 (the kernel half);
        // entries 0..256 (the user half) are preserved.
        unsafe {
            write_kernel_half_entries(root, entries);
        }
    }

    unsafe fn ensure_kernel_intermediate(root: PhysAddr, virt: VirtAddr) -> Result<(), MapError> {
        // SAFETY: forwarded from `ArchPaging::ensure_kernel_intermediate` —
        // `root` is a valid PML4 reachable through HHDM.
        ensure_table(unsafe { TableRef::root(root) }, pml4_index(virt)).map(|_| ())
    }

    unsafe fn translate(root: PhysAddr, virt: VirtAddr) -> Option<PhysAddr> {
        // SAFETY: forwarded from the trait's contract.
        let (leaf, level) = unsafe { walk(root, virt) }?;
        Some(PhysAddr::new(frame_base(leaf.0, level > 1) | leaf_offset(level, virt)))
    }

    unsafe fn attribute_index(root: PhysAddr, virt: VirtAddr) -> Option<u8> {
        // SAFETY: forwarded from the trait's contract.
        unsafe { attribute_index(root, virt) }
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
mod tests;
