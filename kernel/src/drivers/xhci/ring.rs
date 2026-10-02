//! **TRBs, and the two kinds of ring that carry them** (xHCI 1.2 §4.9, §6.4).
//!
//! A ring is an array of 16-byte Transfer Request Blocks in memory both sides can reach, and a
//! **cycle bit** in each TRB says whose it is: a TRB whose cycle bit matches the reader's cycle
//! state is new. The writer flips its state each time it wraps, so what it wrote last time round
//! stops matching without anything being cleared.
//! - **A [`Producer`]** is a ring software writes and the controller reads: the command ring, a
//!   transfer ring. Its last slot holds a Link TRB back to the first, with *Toggle Cycle* set, so
//!   the controller flips its state where software does.
//! - **A [`Consumer`]** is a ring the controller writes and software reads: the event ring. A
//!   one-segment event ring wraps at its end, with no Link TRB, and the reader flips its state there.
//!
//! Where the TRBs live is [`Slots`]': device memory written volatile in the kernel, a `Vec` in a
//! test. Everything here is the arithmetic, so the host tests are tests of it.

/// One Transfer Request Block: four dwords, the fourth holding the cycle bit and the type.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Trb(pub [u32; 4]);

/// TRB types (xHCI 1.2 Table 6-91), in dword 3's bits 15:10.
pub mod kind {
    /// Link: the next TRB is at the address this one carries.
    pub const LINK: u32 = 6;
    /// No Op Command: completes with Success and does nothing else.
    pub const NO_OP_COMMAND: u32 = 23;
    /// Transfer Event.
    pub const TRANSFER_EVENT: u32 = 32;
    /// Command Completion Event.
    pub const COMMAND_COMPLETION: u32 = 33;
    /// Port Status Change Event.
    pub const PORT_STATUS_CHANGE: u32 = 34;
}

/// Completion codes (xHCI 1.2 Table 6-90), in an event's dword 2 bits 31:24.
pub mod code {
    /// The command or transfer did what it was asked.
    pub const SUCCESS: u8 = 1;
}

/// Dword 3 bit 0: the cycle bit.
const CYCLE: u32 = 1;
/// A Link TRB's dword 3 bit 1: the reader flips its cycle state on following it.
const TOGGLE_CYCLE: u32 = 1 << 1;

impl Trb {
    /// A TRB of type `kind` with nothing else set, its cycle bit clear.
    pub const fn of_kind(kind: u32) -> Trb {
        Trb([0, 0, 0, kind << 10])
    }

    /// A Link TRB to `target`, with *Toggle Cycle*.
    pub const fn link(target: u64) -> Trb {
        Trb([target as u32, (target >> 32) as u32, 0, kind::LINK << 10 | TOGGLE_CYCLE])
    }

    /// Its type.
    pub const fn kind(&self) -> u32 {
        (self.0[3] >> 10) & 0x3F
    }

    /// Its cycle bit.
    pub const fn cycle(&self) -> bool {
        self.0[3] & CYCLE != 0
    }

    /// This TRB with its cycle bit set to `cycle`.
    pub const fn with_cycle(self, cycle: bool) -> Trb {
        let d3 = if cycle { self.0[3] | CYCLE } else { self.0[3] & !CYCLE };
        Trb([self.0[0], self.0[1], self.0[2], d3])
    }

    /// An event's first two dwords as one address: a Command Completion Event's command TRB, a
    /// Transfer Event's transfer TRB.
    pub const fn pointer(&self) -> u64 {
        self.0[0] as u64 | (self.0[1] as u64) << 32
    }

    /// An event's completion code.
    pub const fn completion_code(&self) -> u8 {
        (self.0[2] >> 24) as u8
    }

    /// The slot an event is about, or 0.
    pub const fn slot_id(&self) -> u8 {
        (self.0[3] >> 24) as u8
    }

    /// A Port Status Change Event's port, numbered from 1.
    pub const fn port_id(&self) -> u8 {
        (self.0[0] >> 24) as u8
    }
}

/// The memory a ring's TRBs are in.
pub trait Slots {
    /// How many TRBs it holds.
    fn len(&self) -> usize;
    /// TRB `i`.
    fn read(&self, i: usize) -> Trb;
    /// Write TRB `i`. **Dword 3 last**: it holds the cycle bit that hands the TRB over, so the
    /// other three must be in place before it.
    fn write(&mut self, i: usize, trb: Trb);
}

/// A ring software writes: the command ring, or a transfer ring.
#[derive(Debug)]
pub struct Producer {
    /// The slot the next TRB goes in.
    enqueue: usize,
    /// The cycle state: what the cycle bit of a TRB written now says.
    cycle: bool,
}

impl Producer {
    /// Make `slots` a ring at `base` (its physical address): every TRB cleared, and the last a Link
    /// back to the first. The Link's cycle bit is the opposite of the starting state, so the
    /// controller does not follow it until the producer gets there.
    pub fn new<S: Slots>(slots: &mut S, base: u64) -> Producer {
        let last = slots.len() - 1;
        for i in 0..last {
            slots.write(i, Trb::default());
        }
        slots.write(last, Trb::link(base).with_cycle(false));
        Producer { enqueue: 0, cycle: true }
    }

    /// The cycle state a reader starts with: the Ring Cycle State a command ring's CRCR, or an
    /// endpoint context's dequeue pointer, carries.
    pub fn cycle(&self) -> bool {
        self.cycle
    }

    /// Write `trb` at the enqueue slot with this ring's cycle bit, and return the slot it went in.
    ///
    /// **At the Link the ring hands that over too and wraps**: the Link gets the current cycle bit,
    /// then the state flips and the next TRB goes in slot 0. Done as soon as the enqueue slot
    /// reaches the Link rather than when the next TRB is pushed, so the controller never stops at a
    /// Link it does not yet own with work waiting beyond it.
    pub fn push<S: Slots>(&mut self, slots: &mut S, trb: Trb) -> usize {
        let at = self.enqueue;
        slots.write(at, trb.with_cycle(self.cycle));
        self.enqueue += 1;
        let last = slots.len() - 1;
        if self.enqueue == last {
            let link = slots.read(last).with_cycle(self.cycle);
            slots.write(last, link);
            self.cycle = !self.cycle;
            self.enqueue = 0;
        }
        at
    }
}

/// A ring the controller writes: the event ring, in one segment.
#[derive(Debug)]
pub struct Consumer {
    /// The slot the next event will be in.
    dequeue: usize,
    /// The cycle state: what a new event's cycle bit says.
    cycle: bool,
}

impl Default for Consumer {
    fn default() -> Self {
        Self::new()
    }
}

impl Consumer {
    /// A ring the controller has written nothing to: it writes its first events with cycle 1.
    pub const fn new() -> Consumer {
        Consumer { dequeue: 0, cycle: true }
    }

    /// The slot the next event will be in, which the Event Ring Dequeue Pointer is set to after
    /// a drain.
    pub fn dequeue(&self) -> usize {
        self.dequeue
    }

    /// The next event, if the controller has written one.
    pub fn next<S: Slots>(&mut self, slots: &S) -> Option<Trb> {
        let trb = slots.read(self.dequeue);
        if trb.cycle() != self.cycle {
            return None;
        }
        self.dequeue += 1;
        if self.dequeue == slots.len() {
            self.dequeue = 0;
            self.cycle = !self.cycle;
        }
        Some(trb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mem(Vec<Trb>);

    impl Slots for Mem {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn read(&self, i: usize) -> Trb {
            self.0[i]
        }
        fn write(&mut self, i: usize, trb: Trb) {
            self.0[i] = trb;
        }
    }

    const BASE: u64 = 0x1234_5000;

    /// **The controller's side of a producer ring**, written from the specification rather than
    /// from [`Producer`]: start at slot 0 with the producer's cycle state, take a TRB whose cycle
    /// bit matches, follow a Link and flip on *Toggle Cycle*. A producer whose Link is handed over
    /// with the wrong bit, or not at all, makes this reader stop or read stale slots.
    struct Controller {
        at: usize,
        cycle: bool,
    }

    impl Controller {
        fn take(&mut self, mem: &Mem) -> Option<Trb> {
            loop {
                let trb = mem.0[self.at];
                if trb.cycle() != self.cycle {
                    return None;
                }
                if trb.kind() == kind::LINK {
                    assert_eq!(trb.pointer(), BASE, "the Link points back to the start");
                    if trb.0[3] & TOGGLE_CYCLE != 0 {
                        self.cycle = !self.cycle;
                    }
                    self.at = 0;
                    continue;
                }
                self.at += 1;
                return Some(trb);
            }
        }
    }

    fn numbered(n: u32) -> Trb {
        Trb([n, 0, 0, kind::NO_OP_COMMAND << 10])
    }

    #[test]
    fn a_new_ring_is_cleared_and_its_link_is_not_yet_the_controllers() {
        let mut mem = Mem(vec![numbered(99).with_cycle(true); 8]);
        let ring = Producer::new(&mut mem, BASE);
        assert!(ring.cycle());
        assert!(mem.0[..7].iter().all(|t| *t == Trb::default()), "stale TRBs cleared: {:?}", mem.0);
        assert_eq!(mem.0[7].kind(), kind::LINK);
        assert!(!mem.0[7].cycle(), "a Link the controller could follow before anything is written");
        let mut c = Controller { at: 0, cycle: ring.cycle() };
        assert_eq!(c.take(&mem), None, "nothing for the controller yet");
    }

    /// **Three times round a ring of eight**, the controller taking each TRB as it is written:
    /// every one is seen once, in order, across each wrap.
    #[test]
    fn the_controller_sees_every_trb_once_across_wraps() {
        let mut mem = Mem(vec![Trb::default(); 8]);
        let mut ring = Producer::new(&mut mem, BASE);
        let mut c = Controller { at: 0, cycle: ring.cycle() };
        for n in 0..21 {
            let slot = ring.push(&mut mem, numbered(n));
            assert_eq!(slot, n as usize % 7, "seven usable slots, then the Link");
            assert_eq!(c.take(&mem).map(|t| t.0[0]), Some(n));
            assert_eq!(c.take(&mem), None, "and nothing past it");
        }
    }

    /// **The same, with the controller behind**: seven written before it looks, which is a full
    /// ring, then seven more after it has caught up.
    #[test]
    fn a_full_ring_read_late_reads_in_order() {
        let mut mem = Mem(vec![Trb::default(); 8]);
        let mut ring = Producer::new(&mut mem, BASE);
        let mut c = Controller { at: 0, cycle: ring.cycle() };
        for n in 0..7 {
            ring.push(&mut mem, numbered(n));
        }
        assert!(!ring.cycle(), "the state flipped at the Link");
        let seen: Vec<u32> = core::iter::from_fn(|| c.take(&mem)).map(|t| t.0[0]).collect();
        assert_eq!(seen, (0..7).collect::<Vec<_>>());
        for n in 7..14 {
            ring.push(&mut mem, numbered(n));
        }
        let seen: Vec<u32> = core::iter::from_fn(|| c.take(&mem)).map(|t| t.0[0]).collect();
        assert_eq!(seen, (7..14).collect::<Vec<_>>());
    }

    /// **The event ring, written as the controller writes it**: cycle 1 the first time round, 0
    /// the second, with no Link. The reader takes what is new and stops at what is not.
    #[test]
    fn the_consumer_takes_new_events_and_flips_at_the_end() {
        let mut mem = Mem(vec![Trb::default(); 4]);
        let mut ev = Consumer::new();
        assert_eq!(ev.next(&mem), None, "a zeroed ring holds nothing");
        for i in 0..3 {
            mem.0[i] = numbered(i as u32).with_cycle(true);
        }
        let got: Vec<u32> = core::iter::from_fn(|| ev.next(&mem)).map(|t| t.0[0]).collect();
        assert_eq!(got, vec![0, 1, 2]);
        assert_eq!(ev.dequeue(), 3);
        mem.0[3] = numbered(3).with_cycle(true);
        mem.0[0] = numbered(4).with_cycle(false);
        let got: Vec<u32> = core::iter::from_fn(|| ev.next(&mem)).map(|t| t.0[0]).collect();
        assert_eq!(got, vec![3, 4], "the wrap, and the second time round written with cycle 0");
        assert_eq!(ev.dequeue(), 1);
        assert_eq!(ev.next(&mem), None, "slot 1 still holds last round's event");
    }

    /// A Command Completion Event as the specification lays it out (xHCI 1.2 §6.4.2.2), read field
    /// by field.
    #[test]
    fn an_events_fields_read_from_their_dwords() {
        let ev = Trb([0x1234_5670, 0x0000_0001, (code::SUCCESS as u32) << 24 | 7, 3 << 24 | kind::COMMAND_COMPLETION << 10 | 1]);
        assert_eq!(ev.kind(), kind::COMMAND_COMPLETION);
        assert_eq!(ev.pointer(), 0x1_1234_5670);
        assert_eq!(ev.completion_code(), code::SUCCESS);
        assert_eq!(ev.slot_id(), 3);
        assert!(ev.cycle());
        let port = Trb([5 << 24, 0, (code::SUCCESS as u32) << 24, kind::PORT_STATUS_CHANGE << 10 | 1]);
        assert_eq!((port.kind(), port.port_id()), (kind::PORT_STATUS_CHANGE, 5));
    }
}
