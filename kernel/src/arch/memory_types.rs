//! How the hardware caches a physical range, and what the firmware set it to.
//!
//! **Neutral, because the question is.** Every architecture has *some* answer to "what does a
//! write to this physical address cost, and when does the device see it" — x86 spells it with
//! range registers and a page-attribute table, aarch64 with MAIR and stage-1 attributes — and
//! the kernel outside `arch/` only needs the answer, plus a way to say it in a log line. The
//! spelling stays in the architecture's private submodule, as
//! `docs/conventions/arch-boundary.md` requires.
//!
//! **Why this exists at all** (Phase 5 Part G's measurement): the laptop's first boot drew the
//! desktop slowly in proportion to the area repainted, and the suspected reason is that the
//! framebuffer's range is uncacheable while every mapping this kernel makes is write-back. That
//! is a claim about the machine, so the machine is asked rather than assumed.

/// How the hardware treats reads and writes to a range of physical memory.
///
/// The names are the architecture-neutral behaviours, not any architecture's encoding.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MemoryType {
    /// Every access goes to the bus, in order, uncombined. Correct for device registers and
    /// ruinous for a framebuffer: a full screen is millions of individual writes.
    Uncacheable,
    /// Writes are gathered into bursts and may be reordered; reads are uncached. What a
    /// framebuffer wants.
    WriteCombining,
    /// Reads cache; writes go through to memory as well as into the cache.
    WriteThrough,
    /// Reads cache; writes go to the bus and invalidate.
    WriteProtected,
    /// Ordinary cached memory.
    WriteBack,
    /// Uncacheable, but weakly: a *page* may name this and a range register may still raise it
    /// to write-combining. x86 spells it `UC-`; it exists so a page table can ask for "no cache
    /// unless the platform says otherwise".
    UncacheableWeak,
    /// A value this kernel has no name for.
    Other(u8),
}

impl MemoryType {
    /// A short name for a log line.
    pub fn name(self) -> &'static str {
        match self {
            MemoryType::Uncacheable => "uncacheable",
            MemoryType::WriteCombining => "write-combining",
            MemoryType::WriteThrough => "write-through",
            MemoryType::WriteProtected => "write-protected",
            MemoryType::WriteBack => "write-back",
            MemoryType::UncacheableWeak => "uncacheable (overridable)",
            MemoryType::Other(_) => "an unknown type",
        }
    }
}

/// What [`install_policy`](ArchMemoryTypes::install_policy) found and did.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Policy {
    /// The platform's table was already the one this kernel programs.
    Unchanged,
    /// It was something else, and now it is the kernel's. Mappings made before this chose their
    /// entries out of the old table.
    Replaced,
    /// This CPU has no such table at all, so there is nothing to install and no attribute to
    /// select: every mapping is whatever the range registers say. No x86-64 part ships like this,
    /// and the case is handled rather than assumed away because the check for it is one bit.
    NoTable,
}

/// What the platform says about caching physical memory.
pub trait ArchMemoryTypes {
    /// The type the firmware's configuration gives `phys`, or `None` where the CPU offers no
    /// such configuration (then every range is whatever the page tables say).
    ///
    /// # Safety
    /// Ring 0. Reads architecture configuration registers; touches no memory.
    unsafe fn at(phys: u64) -> Option<MemoryType>;

    /// What the *mapping* of `virt` asks for, or `None` if it is not mapped.
    ///
    /// Distinct from [`at`](ArchMemoryTypes::at), which answers for the physical memory: the two
    /// disagreeing is the whole of Phase 5 Part G. What the hardware then does is the stronger of
    /// the two, except that a mapping asking for write-combining raises an uncacheable range —
    /// the one case where a page table wins, and the one the fix uses.
    ///
    /// # Safety
    /// Ring 0. Walks the active page tables.
    unsafe fn of_mapping(virt: u64) -> Option<MemoryType>;

    /// Install the kernel's own cache-policy table on **this** CPU, and say what happened.
    ///
    /// Every CPU needs its own call: the table is per-CPU state, and a CPU whose table disagreed
    /// with its neighbours' would give the same page different meanings depending on which core
    /// touched it.
    ///
    /// **[`Policy::Replaced`] is not a failure** — the table is the kernel's either way. It means
    /// the platform handed over something else, which matters because mappings made *before* this
    /// call (the bootloader's, including the one the console draws through) chose their entries
    /// out of the old table.
    ///
    /// # Safety
    /// Ring 0, during this CPU's bring-up, before it makes any mapping that names an entry.
    unsafe fn install_policy() -> Policy;

    /// Log the platform's cache-policy configuration, in that architecture's own terms — the
    /// one place the spelling is allowed to show, because a reader comparing this against the
    /// vendor's manual needs the vendor's names.
    ///
    /// # Safety
    /// Ring 0, during boot, once the console exists.
    unsafe fn log_configuration();
}
