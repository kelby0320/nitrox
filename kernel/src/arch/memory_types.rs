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

/// What the platform says about caching physical memory.
pub trait ArchMemoryTypes {
    /// The type the firmware's configuration gives `phys`, or `None` where the CPU offers no
    /// such configuration (then every range is whatever the page tables say).
    ///
    /// # Safety
    /// Ring 0. Reads architecture configuration registers; touches no memory.
    unsafe fn at(phys: u64) -> Option<MemoryType>;

    /// Log the platform's cache-policy configuration, in that architecture's own terms — the
    /// one place the spelling is allowed to show, because a reader comparing this against the
    /// vendor's manual needs the vendor's names.
    ///
    /// # Safety
    /// Ring 0, during boot, once the console exists.
    unsafe fn log_configuration();
}
