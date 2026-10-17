//! **The ring `Log::Read` answers from**: the records kept, bounded by bytes and cut to what a
//! `Read` reply holds whole, and the replies built from them. Here so that it is host-tested.

use alloc::collections::VecDeque;
use alloc::string::String;

use librsproto::log::{ReadRecord, ReadReplyWriter};

/// **What the ring may hold, in bytes**, counting what each record's strings have allocated and
/// [`RECORD_OVERHEAD`]. Bytes rather than a count, because a message may be a kilobyte.
/// **Allocated, not used**: a message cut from 4 KB keeps its 4 KB until [`cut`] gives it back.
/// The heap's size classes round a string of under 2 KiB up to a power of two, which this does
/// not count, so what the ring occupies can reach twice this.
pub const RING_BUDGET: usize = 1024 * 1024;
/// What a record costs the ring beyond its strings: its fixed fields and the queue's slot.
pub const RECORD_OVERHEAD: usize = 96;
/// **The longest message the ring keeps**, in bytes; a longer one is cut there on a character
/// boundary and ends in `…`. The serial sink prints it whole. Every record fits a `Read` reply:
/// `RECORD_HEADER_LEN` + two [`NAME_MAX`] strings + this.
pub const MESSAGE_KEPT: usize = 1024;
/// The longest principal or source a resolve may name, refused past it; and the longest source a
/// record's own claim is kept to.
pub const NAME_MAX: usize = 64;

/// A stamped log record. The trusted fields (`principal`/`tier`/`timestamp`/`time`/`sequence`)
/// are supplied by the server; the rest are the emitter's claims.
pub struct Record {
    /// The channel's principal.
    pub principal: String,
    /// The channel's tier.
    pub tier: u8,
    /// The monotonic clock at ingest.
    pub timestamp: u64,
    /// The wall clock at ingest, or `None` while it is not set. It can step, so `sequence` is
    /// what orders records.
    pub time: Option<u64>,
    /// Rises by one per record: an `oldest()` past a reader's last plus one says it missed some.
    pub sequence: u64,
    /// The emitter's severity.
    pub level: u8,
    /// The emitter's text, cut to [`MESSAGE_KEPT`].
    pub message: String,
    /// The emitter's claim, or the channel's label; never present and empty once pushed.
    pub source: Option<String>,
}

/// **The most recent records, for `Read`**, oldest first and bounded by [`RING_BUDGET`].
/// Sequences rise, so the records after one are found by a binary search.
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

    /// What `rec` counts against [`RING_BUDGET`]: what its strings have allocated.
    pub fn cost(rec: &Record) -> usize {
        let source = rec.source.as_ref().map_or(0, String::capacity);
        RECORD_OVERHEAD + rec.principal.capacity() + source + rec.message.capacity()
    }

    /// Keep `rec` as a reply can carry it, and drop the oldest until the ring is in budget.
    ///
    /// **Everything kept is servable**: a message and a source are cut to what a reply holds, and
    /// a present, empty source (an empty source means none in a reply) is kept as none. A record
    /// the reply writer refused would stop every `Read` at it, which `log` takes for the end of
    /// the ring. The principal is the service's, from a resolve path naming 1 to [`NAME_MAX`]
    /// bytes.
    pub fn push(&mut self, mut rec: Record) {
        cut(&mut rec.message, MESSAGE_KEPT);
        if rec.source.as_ref().is_some_and(String::is_empty) {
            rec.source = None;
        }
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

    /// **A `Read` reply in `out`**: the records after `after`, oldest first, as many as fit and
    /// no more than `max` (`0` for no limit). Returns the reply's length, or `None` if `out` cannot
    /// hold even its header.
    ///
    /// A record the writer refuses ends the reply (it is full) **unless the reply is empty**:
    /// then that record could never be written, and stopping at it would stop every reader there.
    /// [`Ring::push`] keeps nothing a reply cannot carry, but this passes over one anyway.
    pub fn fill_reply(&self, after: u64, max: u32, out: &mut [u8]) -> Option<usize> {
        let mut w = ReadReplyWriter::new(out, self.oldest())?;
        for r in self.after(after) {
            if max != 0 && w.count() >= max {
                break;
            }
            if !w.push(&read_record(r)) && w.count() != 0 {
                break;
            }
        }
        Some(w.finish())
    }
}

/// **The source a record is kept under**: its own claim when it makes one, and otherwise the
/// channel's named-source label. **An empty claim is no claim**, so it falls back to the label
/// as none does; a claim that is not UTF-8 is `?`.
pub fn claimed_source(claim: Option<&[u8]>, channel: &Option<String>) -> Option<String> {
    match claim.filter(|c| !c.is_empty()) {
        Some(c) => Some(String::from(core::str::from_utf8(c).unwrap_or("?"))),
        None => channel.clone(),
    }
}

/// `rec` as a reply carries it.
pub fn read_record(rec: &Record) -> ReadRecord<'_> {
    ReadRecord {
        sequence: rec.sequence,
        time: rec.time,
        timestamp: rec.timestamp,
        tier: rec.tier,
        level: rec.level,
        principal: rec.principal.as_bytes(),
        source: rec.source.as_deref().map(str::as_bytes),
        message: rec.message.as_bytes(),
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
    // `truncate` keeps the allocation, which the ring's budget counts.
    s.shrink_to_fit();
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

    /// `bytes` stays the sum of what the ring holds, not a counter that drifted.
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
        assert!(r.bytes + Ring::cost(&rec(0, &long)) > RING_BUDGET);
        assert_eq!(r.after(0).last().map(|r| r.sequence), Some(3000));
        let kept = r.after(0).count() as u64;
        assert_eq!(r.oldest(), 3000 - kept + 1);
    }

    fn read_all(r: &Ring) -> alloc::vec::Vec<u64> {
        let (mut after, mut seen) = (0, alloc::vec::Vec::new());
        let mut out = alloc::vec![0u8; 3980];
        loop {
            let len = r.fill_reply(after, 0, &mut out).unwrap();
            let reply = librsproto::log::parse_read_reply(&out[..len]).unwrap();
            if reply.count == 0 {
                return seen;
            }
            for rec in reply.records() {
                after = rec.sequence;
                seen.push(rec.sequence);
            }
        }
    }

    /// Built the way the service builds it: an append encoded, parsed, its source claimed, kept.
    #[test]
    fn an_empty_source_does_not_stop_a_reader() {
        use librsproto::log::{LEVEL_INFO, encode_append, parse_append};
        let mut r = Ring::new();
        let label = Some(String::from("worker"));
        for seq in 1..=50u64 {
            let source: Option<&[u8]> = match seq {
                2 => Some(b""),
                3 => Some(b"own"),
                _ => None,
            };
            let mut body = [0u8; 128];
            let n = encode_append(&mut body, LEVEL_INFO, b"a record", None, None, source).unwrap();
            let la = parse_append(&body[..n]).unwrap();
            let mut rec = rec(seq, core::str::from_utf8(la.message).unwrap());
            rec.source = claimed_source(la.source, &label);
            r.push(rec);
        }
        assert_eq!(read_all(&r), (1..=50).collect::<alloc::vec::Vec<u64>>());
        let sources: alloc::vec::Vec<_> = r.after(0).take(3).map(|x| x.source.clone()).collect();
        assert_eq!(sources, [label.clone(), label.clone(), Some(String::from("own"))]);
        let mut empty = rec(51, "m");
        empty.source = Some(String::new());
        r.push(empty);
        assert_eq!(r.after(50).next().map(|x| x.source.clone()), Some(None));
        assert_eq!(read_all(&r).last(), Some(&51));
        // Kept around `push`, a reply that cannot write it passes it over.
        let mut unservable = rec(52, "m");
        unservable.source = Some(String::new());
        r.buf.push_back(unservable);
        r.push(rec(53, "m"));
        let seen = read_all(&r);
        assert!(!seen.contains(&52) && seen.last() == Some(&53), "{seen:?}");
    }

    #[test]
    fn a_cut_message_gives_back_what_was_cut() {
        let mut s = String::with_capacity(3900);
        s.push_str(&"w".repeat(3900));
        cut(&mut s, MESSAGE_KEPT);
        assert!(s.len() <= MESSAGE_KEPT && s.capacity() <= MESSAGE_KEPT, "len {} capacity {}", s.len(), s.capacity());
        let mut r = Ring::new();
        let mut big = rec(1, "");
        big.message = String::from("v".repeat(3900).as_str());
        r.push(big);
        assert!(r.bytes <= RECORD_OVERHEAD + "view-broker".len() + MESSAGE_KEPT, "{}", r.bytes);
    }

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

    #[test]
    fn what_is_kept_is_cut_on_a_character_boundary() {
        let mut r = Ring::new();
        let exact = "y".repeat(MESSAGE_KEPT);
        r.push(rec(1, &exact));
        // A two-byte character straddles the cut.
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
