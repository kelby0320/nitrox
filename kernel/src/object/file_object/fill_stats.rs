use core::sync::atomic::{AtomicU64, Ordering};

/// Fills that issued a real producer request (a block IRP under Model A).
pub static FILLS: AtomicU64 = AtomicU64::new(0);
/// The pages those fills brought in: one a fill, and under Model A those read ahead.
pub static FILL_PAGES: AtomicU64 = AtomicU64::new(0);
/// Fills that resolved to a hole: counted apart, since averaging them in would flatter the
/// real cost.
pub static HOLE_FILLS: AtomicU64 = AtomicU64::new(0);
/// Total nanoseconds spent inside fills that issued I/O (reserve → page Ready).
pub static FILL_NS: AtomicU64 = AtomicU64::new(0);
/// The slowest single such fill, in nanoseconds.
pub static FILL_MAX_NS: AtomicU64 = AtomicU64::new(0);
/// Faults that found the page already resident (the cache hit path).
pub static HITS: AtomicU64 = AtomicU64::new(0);
/// Faults that found the page mid-fill and `yield_now`-spun waiting for it.
pub static SPINS: AtomicU64 = AtomicU64::new(0);
/// Pages written back to their files' blocks.
pub static WRITEBACK_PAGES: AtomicU64 = AtomicU64::new(0);
/// The write IRPs that carried them: one per run of pages.
pub static WRITEBACK_IRPS: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds those IRPs took, from submission to completion.
pub static WRITEBACK_NS: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the counters.
#[derive(Copy, Clone, Debug, Default)]
pub struct Snapshot {
    pub fills: u64,
    pub fill_pages: u64,
    pub hole_fills: u64,
    pub fill_ns: u64,
    pub fill_max_ns: u64,
    pub hits: u64,
    pub spins: u64,
    pub writeback_pages: u64,
    pub writeback_irps: u64,
    pub writeback_ns: u64,
}

/// Read the counters. Relaxed: they are statistics.
pub fn snapshot() -> Snapshot {
    Snapshot {
        fills: FILLS.load(Ordering::Relaxed),
        fill_pages: FILL_PAGES.load(Ordering::Relaxed),
        hole_fills: HOLE_FILLS.load(Ordering::Relaxed),
        fill_ns: FILL_NS.load(Ordering::Relaxed),
        fill_max_ns: FILL_MAX_NS.load(Ordering::Relaxed),
        hits: HITS.load(Ordering::Relaxed),
        spins: SPINS.load(Ordering::Relaxed),
        writeback_pages: WRITEBACK_PAGES.load(Ordering::Relaxed),
        writeback_irps: WRITEBACK_IRPS.load(Ordering::Relaxed),
        writeback_ns: WRITEBACK_NS.load(Ordering::Relaxed),
    }
}

pub(crate) fn record_fill(ns: u64, pages: u64) {
    FILLS.fetch_add(1, Ordering::Relaxed);
    FILL_PAGES.fetch_add(pages, Ordering::Relaxed);
    FILL_NS.fetch_add(ns, Ordering::Relaxed);
    FILL_MAX_NS.fetch_max(ns, Ordering::Relaxed);
}

pub(crate) fn record_writeback(pages: u64, ns: u64) {
    WRITEBACK_PAGES.fetch_add(pages, Ordering::Relaxed);
    WRITEBACK_IRPS.fetch_add(1, Ordering::Relaxed);
    WRITEBACK_NS.fetch_add(ns, Ordering::Relaxed);
}
