//! PS/2 mouse packet framing → [`InputEvent`]s.
//!
//! The aux port delivers a **three-byte packet** with no framing byte:
//!
//! ```text
//!   byte 0:  YO XO YS XS  1  M  R  L      bit 3 is always 1
//!   byte 1:  X delta, sign in byte 0's XS
//!   byte 2:  Y delta, sign in byte 0's YS
//! ```
//!
//! ## Resynchronisation is the whole problem
//!
//! There is no framing byte, only bit 3 of the first byte being always-set. Drop or gain a
//! single byte — a full controller buffer, a missed interrupt — and every subsequent packet
//! is read one byte out of phase: deltas become button states, and the pointer wanders while
//! buttons fire on their own. It does not self-correct, because two of the three offsets
//! still look plausible.
//!
//! So the decoder **validates the sync bit on every first byte** and, when it fails, drops
//! bytes until one has it set. That heuristic is not perfect — a delta byte can have bit 3
//! set — but it converges within a packet or two, and the alternative is a permanently
//! scrambled pointer.
//!
//! ## Y is negated
//!
//! The wire reports positive-Y as *up*; [`REL_Y`] is positive-*down* to match screen
//! coordinates. Doing it here means exactly one place knows, rather than every consumer.

use crate::libkern::input::*;

/// Bit 3 of the first packet byte, always set by the hardware. The only framing signal.
const SYNC_BIT: u8 = 0x08;
/// Button bits in the first byte.
const BTN_L: u8 = 0x01;
const BTN_R: u8 = 0x02;
const BTN_M: u8 = 0x04;
/// Sign bits for the X and Y deltas.
const X_SIGN: u8 = 0x10;
const Y_SIGN: u8 = 0x20;
/// Overflow flags — the hardware saw more motion than a byte can carry.
const X_OVERFLOW: u8 = 0x40;
const Y_OVERFLOW: u8 = 0x80;

/// A decoded movement/button report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Packet {
    /// Horizontal delta, positive right.
    pub dx: i32,
    /// Vertical delta, positive **down** (already negated from the wire).
    pub dy: i32,
    /// Left button held.
    pub left: bool,
    /// Right button held.
    pub right: bool,
    /// Middle button held.
    pub middle: bool,
}

/// The three-byte framing state machine, plus the button state needed to emit *changes*.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decoder {
    buf: [u8; 3],
    have: usize,
    /// Button state as of the last packet, so a press is emitted once rather than every
    /// packet while it is held.
    last: Packet,
    /// True once a packet has been seen, so the first packet's buttons are compared against
    /// "nothing held" rather than against uninitialised state.
    started: bool,
}

impl Decoder {
    /// A decoder awaiting a first byte.
    pub const fn new() -> Self {
        Self { buf: [0; 3], have: 0, last: Packet { dx: 0, dy: 0, left: false, right: false, middle: false }, started: false }
    }

    /// Feed one byte from the aux port. Returns a [`Packet`] on the third byte of a
    /// well-framed group, and `None` otherwise — including when a byte is discarded to
    /// resynchronise.
    pub fn feed(&mut self, byte: u8) -> Option<Packet> {
        if self.have == 0 && byte & SYNC_BIT == 0 {
            // Not a first byte. Discarding is the resync: accepting it would frame every
            // following packet one byte out of phase.
            return None;
        }
        self.buf[self.have] = byte;
        self.have += 1;
        if self.have < 3 {
            return None;
        }
        self.have = 0;

        let flags = self.buf[0];
        // An overflowed axis carries a meaningless magnitude. Report no motion on that axis
        // rather than a wrong one — a wrong large delta throws the pointer across the screen.
        let dx = if flags & X_OVERFLOW != 0 { 0 } else { sign_extend(self.buf[1], flags & X_SIGN != 0) };
        let dy = if flags & Y_OVERFLOW != 0 { 0 } else { sign_extend(self.buf[2], flags & Y_SIGN != 0) };

        Some(Packet {
            dx,
            // Negated: the wire is positive-up, `REL_Y` is positive-down.
            dy: -dy,
            left: flags & BTN_L != 0,
            right: flags & BTN_R != 0,
            middle: flags & BTN_M != 0,
        })
    }

    /// Turn a packet into the events it implies, appending to `out`.
    ///
    /// Emits only **changes**: a button that was already held produces nothing, and a packet
    /// with no motion and no button change produces nothing at all — not even a `SYN`, since
    /// an empty group is noise that every consumer would have to filter.
    ///
    /// Returns how many events were appended.
    pub fn events(&mut self, p: Packet, time_ns: u64, out: &mut [InputEvent]) -> usize {
        let prev = if self.started { self.last } else { Packet::default() };
        self.last = p;
        self.started = true;

        let mut n = 0;
        let mut push = |e: InputEvent, n: &mut usize| {
            if *n < out.len() {
                out[*n] = e;
                *n += 1;
            }
        };

        for (now, was, code) in [
            (p.left, prev.left, BTN_LEFT),
            (p.right, prev.right, BTN_RIGHT),
            (p.middle, prev.middle, BTN_MIDDLE),
        ] {
            if now != was {
                push(InputEvent::key(code, if now { KEY_PRESS } else { KEY_RELEASE }, time_ns), &mut n);
            }
        }
        if p.dx != 0 {
            push(InputEvent::rel(REL_X, p.dx, time_ns), &mut n);
        }
        if p.dy != 0 {
            push(InputEvent::rel(REL_Y, p.dy, time_ns), &mut n);
        }
        if n > 0 {
            push(InputEvent::syn(time_ns), &mut n);
        }
        n
    }

    /// Largest number of events one packet can produce: three button changes, two axes, and
    /// the `SYN`. Callers size their scratch buffer with this.
    pub const MAX_EVENTS: usize = 6;
}

/// Sign-extend a delta byte using the sign bit carried in the flags byte.
fn sign_extend(magnitude: u8, negative: bool) -> i32 {
    if negative {
        // The value is 9-bit two's complement split across the flags byte; the byte holds
        // the low 8 bits.
        magnitude as i32 - 0x100
    } else {
        magnitude as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a whole packet, returning the decoded result.
    fn packet(d: &mut Decoder, flags: u8, x: u8, y: u8) -> Option<Packet> {
        assert_eq!(d.feed(flags), None);
        assert_eq!(d.feed(x), None);
        d.feed(y)
    }

    #[test]
    fn a_simple_move_decodes() {
        let mut d = Decoder::new();
        let p = packet(&mut d, SYNC_BIT, 5, 3).expect("third byte completes the packet");
        assert_eq!(p.dx, 5);
        assert_eq!(p.dy, -3, "wire positive-up becomes screen positive-down");
        assert!(!p.left && !p.right && !p.middle);
    }

    #[test]
    fn negative_deltas_sign_extend() {
        let mut d = Decoder::new();
        // dx = -1 (0xFF with X_SIGN), dy = -1 → screen +1.
        let p = packet(&mut d, SYNC_BIT | X_SIGN | Y_SIGN, 0xFF, 0xFF).unwrap();
        assert_eq!(p.dx, -1);
        assert_eq!(p.dy, 1);
    }

    #[test]
    fn an_overflowed_axis_reports_no_motion_rather_than_a_wrong_one() {
        let mut d = Decoder::new();
        let p = packet(&mut d, SYNC_BIT | X_OVERFLOW, 0x7F, 4).unwrap();
        assert_eq!(p.dx, 0, "an overflowed magnitude is meaningless");
        assert_eq!(p.dy, -4, "the other axis is unaffected");
    }

    #[test]
    fn a_first_byte_without_the_sync_bit_is_discarded() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x00), None, "no sync bit — not a first byte");
        // The stream is still framed correctly afterwards.
        let p = packet(&mut d, SYNC_BIT, 1, 1).unwrap();
        assert_eq!(p.dx, 1);
    }

    #[test]
    fn the_decoder_resynchronises_after_a_lost_byte() {
        // The failure this exists for: lose one byte mid-packet and every later packet is
        // read one byte out of phase, so deltas become buttons. This asserts recovery, not
        // merely that nothing panics.
        let mut d = Decoder::new();
        d.feed(SYNC_BIT); // first byte of a packet…
        d.feed(9); // …its X…
        // …and its Y byte never arrives. The next real packet's first byte lands where the
        // decoder expects a Y, so it completes a garbage packet:
        let garbage = d.feed(SYNC_BIT | BTN_L);
        assert!(garbage.is_some(), "the decoder cannot know this was garbage");
        // What matters is that it is framed again by the following packet.
        let p = packet(&mut d, SYNC_BIT, 4, 0).expect("re-framed");
        assert_eq!(p.dx, 4);
        assert!(!p.left, "and the stale button bit did not stick");
    }

    #[test]
    fn a_button_press_is_emitted_once_not_every_packet() {
        let mut d = Decoder::new();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];

        let p = packet(&mut d, SYNC_BIT | BTN_L, 0, 0).unwrap();
        let n = d.events(p, 100, &mut out);
        assert_eq!(n, 2, "the press and its SYN");
        assert_eq!(out[0], InputEvent::key(BTN_LEFT, KEY_PRESS, 100));
        assert_eq!(out[1], InputEvent::syn(100));

        // Held, with no motion: nothing at all, not even an empty group.
        let p = packet(&mut d, SYNC_BIT | BTN_L, 0, 0).unwrap();
        assert_eq!(d.events(p, 200, &mut out), 0, "still held is not an event");

        let p = packet(&mut d, SYNC_BIT, 0, 0).unwrap();
        let n = d.events(p, 300, &mut out);
        assert_eq!(n, 2);
        assert_eq!(out[0], InputEvent::key(BTN_LEFT, KEY_RELEASE, 300));
    }

    #[test]
    fn motion_emits_only_the_axes_that_moved_then_a_syn() {
        let mut d = Decoder::new();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let p = packet(&mut d, SYNC_BIT, 3, 0).unwrap();
        let n = d.events(p, 7, &mut out);
        assert_eq!(n, 2, "REL_X and SYN — no REL_Y for a zero delta");
        assert_eq!(out[0], InputEvent::rel(REL_X, 3, 7));
        assert_eq!(out[1], InputEvent::syn(7));
    }

    #[test]
    fn a_packet_with_nothing_in_it_produces_no_group() {
        let mut d = Decoder::new();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let p = packet(&mut d, SYNC_BIT, 0, 0).unwrap();
        assert_eq!(d.events(p, 1, &mut out), 0, "an empty SYN group is noise");
    }

    #[test]
    fn max_events_is_large_enough_for_the_worst_packet() {
        // Three buttons changing at once plus both axes plus the SYN. If `MAX_EVENTS` is
        // ever too small the events are silently truncated, so this pins it.
        let mut d = Decoder::new();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let p = packet(&mut d, SYNC_BIT | BTN_L | BTN_R | BTN_M, 2, 2).unwrap();
        let n = d.events(p, 1, &mut out);
        assert_eq!(n, Decoder::MAX_EVENTS, "worst case must fit exactly");
        assert_eq!(out[n - 1], InputEvent::syn(1), "and the group is still terminated");
    }
}
