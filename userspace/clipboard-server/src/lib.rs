//! The kill ring itself — the half of `clipboard-server` that is a function of values.
//!
//! Fixed storage, no allocator: [`CLIP_RING`] slots of [`MAX_CLIP_BYTES`] each, in `.bss`, about
//! 63 KiB. A clipboard that could grow is one a program can push into until something dies.
//!
//! **Never log bytes** — counts, serials, lengths and kinds only. A clipboard eventually holds a
//! password, and the serial console is a log file.

#![cfg_attr(not(test), no_std)]

use librsproto::clipboard::{CLIP_RING, ClipInfo, MAX_CLIP_BYTES};

/// Why a read did not produce an entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RingError {
    /// No entry at that index: the ring is empty, or a cycle walked off the end of it.
    NoEntry,
    /// The caller's `expect` is not the ring's serial — something was copied under it.
    Stale,
    /// The payload is over [`MAX_CLIP_BYTES`].
    TooLarge,
}

/// One slot; its bytes are inline so the whole ring is one `.bss` object.
struct Slot {
    kind: u16,
    len: usize,
    bytes: [u8; MAX_CLIP_BYTES],
}

impl Slot {
    const fn new() -> Slot {
        Slot { kind: 0, len: 0, bytes: [0; MAX_CLIP_BYTES] }
    }
}

/// The ring: the last [`CLIP_RING`] entries, newest first, and the serial that says whether it
/// has moved.
pub struct Ring {
    slots: [Slot; CLIP_RING],
    /// Index of the newest entry. Meaningless while `len == 0`.
    head: usize,
    /// How many slots hold an entry — climbs to [`CLIP_RING`] and stays.
    len: usize,
    /// Pushes since boot. **Never reset or decremented**: a value that could repeat would make
    /// a stale cycle look fresh.
    serial: u64,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    /// An empty ring; `const` so the server holds it in `.bss` with no runtime initialiser.
    pub const fn new() -> Ring {
        // `[Slot::new(); CLIP_RING]` needs `Copy`, which a 4 KiB slot should not have.
        Ring { slots: [const { Slot::new() }; CLIP_RING], head: 0, len: 0, serial: 0 }
    }

    /// The ring's serial; `CLIP_ANY_SERIAL` exactly while nothing has ever been copied.
    pub fn serial(&self) -> u64 {
        self.serial
    }

    /// How many entries the ring holds.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the ring holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Push an entry; returns the new serial, or [`RingError::TooLarge`].
    ///
    /// **Duplicates are kept**: what counts as the same entry has no obviously right answer.
    pub fn push(&mut self, kind: u16, bytes: &[u8]) -> Result<u64, RingError> {
        if bytes.len() > MAX_CLIP_BYTES {
            return Err(RingError::TooLarge);
        }
        // The newest goes before the old head, so index 0 is the newest and reads need no
        // reversal.
        self.head = if self.len == 0 { 0 } else { (self.head + CLIP_RING - 1) % CLIP_RING };
        let s = &mut self.slots[self.head];
        s.kind = kind;
        s.len = bytes.len();
        s.bytes[..bytes.len()].copy_from_slice(bytes);
        if self.len < CLIP_RING {
            self.len += 1;
        }
        self.serial += 1;
        Ok(self.serial)
    }

    /// Read entry `index` (0 is the newest), refusing if `expect` is a serial the ring has left
    /// behind.
    ///
    /// **The staleness check comes first.** Checked after the bound, a cycle that also ran off
    /// the end would be told `NoEntry` (stop cycling) when the ring moved (start again from the
    /// newest).
    pub fn get(&self, index: usize, expect: u64) -> Result<(u16, &[u8]), RingError> {
        if expect != librsproto::clipboard::CLIP_ANY_SERIAL && expect != self.serial {
            return Err(RingError::Stale);
        }
        if index >= self.len {
            return Err(RingError::NoEntry);
        }
        let s = &self.slots[(self.head + index) % CLIP_RING];
        Ok((s.kind, &s.bytes[..s.len]))
    }

    /// Describe every entry, newest first, into `out`; returns how many rows were written.
    pub fn list(&self, out: &mut [ClipInfo]) -> usize {
        let n = self.len.min(out.len());
        for (i, row) in out[..n].iter_mut().enumerate() {
            let s = &self.slots[(self.head + i) % CLIP_RING];
            *row = ClipInfo { kind: s.kind, len: s.len as u32 };
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::clipboard::{CLIP_ANY_SERIAL, CLIP_KIND_TEXT};

    fn ring() -> alloc::boxed::Box<Ring> {
        // Boxed: 63 KiB is more than a test thread's stack wants.
        alloc::boxed::Box::new(Ring::new())
    }

    extern crate alloc;

    #[test]
    fn an_empty_ring_has_nothing_at_index_zero() {
        let r = ring();
        assert_eq!(r.serial(), CLIP_ANY_SERIAL);
        assert_eq!(r.get(0, CLIP_ANY_SERIAL), Err(RingError::NoEntry));
    }

    #[test]
    fn index_zero_is_the_newest_and_the_serial_counts_pushes() {
        let mut r = ring();
        assert_eq!(r.push(CLIP_KIND_TEXT, b"first").unwrap(), 1);
        assert_eq!(r.push(CLIP_KIND_TEXT, b"second").unwrap(), 2);
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1, b"second");
        assert_eq!(r.get(1, CLIP_ANY_SERIAL).unwrap().1, b"first");
        assert_eq!(r.get(2, CLIP_ANY_SERIAL), Err(RingError::NoEntry));
    }

    #[test]
    fn the_ring_wraps_and_the_oldest_falls_off() {
        let mut r = ring();
        for i in 0..(CLIP_RING + 3) {
            r.push(CLIP_KIND_TEXT, &[b'a' + i as u8]).unwrap();
        }
        assert_eq!(r.len(), CLIP_RING);
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1, &[b'a' + (CLIP_RING + 2) as u8]);
        assert_eq!(r.get(CLIP_RING - 1, CLIP_ANY_SERIAL).unwrap().1, &[b'a' + 3]);
        assert_eq!(r.get(CLIP_RING, CLIP_ANY_SERIAL), Err(RingError::NoEntry));
        assert_eq!(r.serial(), (CLIP_RING + 3) as u64);
    }

    #[test]
    fn a_cycle_is_refused_once_the_ring_has_moved() {
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"one").unwrap();
        let serial = r.push(CLIP_KIND_TEXT, b"two").unwrap();
        assert_eq!(r.get(1, serial).unwrap().1, b"one");
        r.push(CLIP_KIND_TEXT, b"three").unwrap();
        assert_eq!(r.get(1, serial), Err(RingError::Stale));
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1, b"three");
    }

    #[test]
    fn a_stale_cycle_past_the_end_says_stale_rather_than_no_entry() {
        // Both conditions hold at once; the order of the two checks is what is tested.
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"one").unwrap();
        let serial = r.serial();
        r.push(CLIP_KIND_TEXT, b"two").unwrap();
        assert_eq!(r.get(50, serial), Err(RingError::Stale));
        assert_eq!(r.get(50, CLIP_ANY_SERIAL), Err(RingError::NoEntry));
    }

    #[test]
    fn an_entry_over_the_cap_is_refused_and_changes_nothing() {
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"kept").unwrap();
        let before = r.serial();
        let big = [b'x'; MAX_CLIP_BYTES + 1];
        assert_eq!(r.push(CLIP_KIND_TEXT, &big), Err(RingError::TooLarge));
        // A serial that moved would tell cycling clients the ring changed for an entry never
        // stored.
        assert_eq!(r.serial(), before);
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1, b"kept");
    }

    #[test]
    fn an_entry_at_exactly_the_cap_is_accepted() {
        let mut r = ring();
        let big = [b'x'; MAX_CLIP_BYTES];
        assert_eq!(r.push(CLIP_KIND_TEXT, &big).unwrap(), 1);
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1.len(), MAX_CLIP_BYTES);
    }

    #[test]
    fn a_shorter_entry_does_not_show_the_last_one_through() {
        // Slots are never cleared, so the length bounds a read; the whole slot would leak the
        // tail of an earlier copy.
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"a long secret value").unwrap();
        for _ in 0..CLIP_RING {
            r.push(CLIP_KIND_TEXT, b"x").unwrap();
        }
        for i in 0..r.len() {
            assert_eq!(r.get(i, CLIP_ANY_SERIAL).unwrap().1, b"x");
        }
    }

    #[test]
    fn an_empty_entry_round_trips() {
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"").unwrap();
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().1, b"");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn a_list_describes_the_entries_newest_first_and_fits_what_it_is_given() {
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"one").unwrap();
        r.push(CLIP_KIND_TEXT, b"twenty").unwrap();
        let mut rows = [ClipInfo::default(); CLIP_RING];
        assert_eq!(r.list(&mut rows), 2);
        assert_eq!(rows[0], ClipInfo { kind: CLIP_KIND_TEXT, len: 6 });
        assert_eq!(rows[1], ClipInfo { kind: CLIP_KIND_TEXT, len: 3 });
        let mut one = [ClipInfo::default(); 1];
        assert_eq!(r.list(&mut one), 1);
        assert_eq!(one[0].len, 6);
    }

    #[test]
    fn the_kind_travels_with_the_entry() {
        let mut r = ring();
        r.push(CLIP_KIND_TEXT, b"text").unwrap();
        r.push(7, b"something else").unwrap();
        assert_eq!(r.get(0, CLIP_ANY_SERIAL).unwrap().0, 7);
        assert_eq!(r.get(1, CLIP_ANY_SERIAL).unwrap().0, CLIP_KIND_TEXT);
    }
}
