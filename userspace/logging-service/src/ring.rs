//! **The ring `Log::Read` answers from** (administration Part E.6): the records the logging
//! service keeps, bounded by bytes, and cut to what a `Read` reply holds whole. Here rather than in
//! the binary so that its arithmetic is host-tested.

use alloc::collections::VecDeque;
use alloc::string::String;

/// **What the ring may hold, in bytes** (administration Part E.6), counting each record's strings
/// and [`RECORD_OVERHEAD`]: a megabyte, several thousand records. It was 256 records until E.6;
/// a boot writes 50 to 100, so what outgrew it was a machine running for days, whose audit is
/// here too. Bytes rather than a count, because a record's message may be a kilobyte.
pub const RING_BUDGET: usize = 1024 * 1024;
/// What a record costs the ring beyond its strings: its fixed fields, and its allocations' slack.
pub const RECORD_OVERHEAD: usize = 96;
/// **The longest message the ring keeps**, in bytes; a longer one is cut there on a character
/// boundary and ends in `…`. The serial sink prints it whole. So every record fits a `Read`
/// reply with room to spare: `RECORD_HEADER_LEN` + two [`NAME_MAX`] strings + this.
pub const MESSAGE_KEPT: usize = 1024;
/// The longest principal or source a resolve may name, refused past it; and the longest source a
/// record's own claim is kept to.
pub const NAME_MAX: usize = 64;

/// A stamped log record. The trusted fields (`principal`/`tier`/`timestamp`/`time`/`sequence`)
/// are supplied by the server; the rest are the emitter's claims.
pub struct Record {
    pub principal: String,
    pub tier: u8,
    /// The monotonic clock at ingest.
    pub timestamp: u64,
    /// The wall clock at ingest (administration Part E.6), or `None` while it is not set. It can
    /// step (Part E.5), so `sequence` is what orders records.
    pub time: Option<u64>,
    pub sequence: u64,
    pub level: u8,
    pub message: String,
    pub source: Option<String>,
}

/// **The most recent records, for `Read`** (administration Part E.6), oldest first and bounded by
/// [`RING_BUDGET`]. Sequences rise, so the records after one are found by a binary search.
pub struct Ring {
    buf: VecDeque<Record>,
    /// What the records hold, by [`Ring::cost`].
    bytes: usize,
}

impl Ring {
    /// An empty ring.
    pub const fn new() -> Ring {
        Ring { buf: VecDeque::new(), bytes: 0 }
    }

    /// What `rec` counts against [`RING_BUDGET`].
    pub fn cost(rec: &Record) -> usize {
        RECORD_OVERHEAD + rec.principal.len() + rec.source.as_ref().map_or(0, String::len) + rec.message.len()
    }

    /// Keep `rec`, cut to what the ring keeps, and drop the oldest until the ring is in budget.
    pub fn push(&mut self, mut rec: Record) {
        cut(&mut rec.message, MESSAGE_KEPT);
        if let Some(source) = rec.source.as_mut() {
            cut(source, NAME_MAX);
        }
        self.bytes += Ring::cost(&rec);
        self.buf.push_back(rec);
        while self.bytes > RING_BUDGET {
            match self.buf.pop_front() {
                Some(old) => self.bytes -= Ring::cost(&old),
                None => break,
            }
        }
    }

    /// The oldest sequence still held, `0` for none.
    pub fn oldest(&self) -> u64 {
        self.buf.front().map_or(0, |r| r.sequence)
    }

    /// The records after sequence `after`, oldest first.
    pub fn after(&self, after: u64) -> impl Iterator<Item = &Record> {
        let start = self.buf.partition_point(|r| r.sequence <= after);
        self.buf.range(start..)
    }
}

impl Default for Ring {
    fn default() -> Ring {
        Ring::new()
    }
}

/// Cut `s` to at most `max` bytes on a character boundary, ending in `…` when anything was cut.
pub fn cut(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max - '…'.len_utf8();
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push('…');
}


#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    fn rec(sequence: u64, message: &str) -> Record {
        Record {
            principal: String::from("view-broker"),
            tier: 1,
            timestamp: sequence * 1000,
            time: None,
            sequence,
            level: 2,
            message: String::from(message),
            source: None,
        }
    }

    /// **Over budget, the oldest go, and the count stays exact**: after thousands of kilobyte
    /// records the ring holds what fits, newest last, and `bytes` is the sum of what it holds —
    /// not a counter that drifted.
    #[test]
    fn the_oldest_go_when_the_budget_is_spent() {
        let mut r = Ring::new();
        let long = "x".repeat(MESSAGE_KEPT);
        for seq in 1..=3000 {
            r.push(rec(seq, &long));
        }
        let held: usize = r.after(0).map(Ring::cost).sum();
        assert_eq!(r.bytes, held);
        assert!(r.bytes <= RING_BUDGET);
        // One more would not have fitted: it is full, not merely under.
        assert!(r.bytes + Ring::cost(&rec(0, &long)) > RING_BUDGET);
        assert_eq!(r.after(0).last().map(|r| r.sequence), Some(3000));
        let kept = r.after(0).count() as u64;
        assert_eq!(r.oldest(), 3000 - kept + 1);
    }

    /// **`after` is the records past a sequence**, at each edge: before the oldest, at it, inside,
    /// at the newest, and past it.
    #[test]
    fn after_finds_the_records_past_a_sequence() {
        let mut r = Ring::new();
        assert_eq!((r.oldest(), r.after(0).count()), (0, 0));
        for seq in [3, 4, 7, 9] {
            r.push(rec(seq, "m"));
        }
        let after = |n| r.after(n).map(|x| x.sequence).collect::<alloc::vec::Vec<_>>();
        assert_eq!(after(0), [3, 4, 7, 9]);
        assert_eq!(after(3), [4, 7, 9]);
        assert_eq!(after(5), [7, 9]);
        assert_eq!(after(9), [] as [u64; 0]);
        assert_eq!(after(u64::MAX), [] as [u64; 0]);
        assert_eq!(r.oldest(), 3);
    }

    /// **A long message is cut on a character boundary and says so**; one at the limit is not.
    /// The source is cut the same way, at [`NAME_MAX`].
    #[test]
    fn what_is_kept_is_cut_on_a_character_boundary() {
        let mut r = Ring::new();
        let exact = "y".repeat(MESSAGE_KEPT);
        r.push(rec(1, &exact));
        // A two-byte character straddling the cut: the cut steps back rather than splitting it.
        let straddling = format!("{}é{}", "z".repeat(MESSAGE_KEPT - 4), "z".repeat(10));
        let mut long_source = rec(2, &straddling);
        long_source.source = Some("s".repeat(NAME_MAX + 1));
        r.push(long_source);
        let kept: alloc::vec::Vec<_> = r.after(0).collect();
        assert_eq!(kept[0].message, exact);
        assert!(kept[1].message.len() <= MESSAGE_KEPT && kept[1].message.ends_with('…'));
        assert!(kept[1].message.starts_with(&"z".repeat(MESSAGE_KEPT - 4)));
        assert!(!kept[1].message.contains('é'));
        let source = kept[1].source.as_deref().unwrap();
        assert!(source.len() <= NAME_MAX && source.ends_with('…'));
    }
}
