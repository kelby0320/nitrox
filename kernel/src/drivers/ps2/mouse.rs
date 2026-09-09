//! PS/2 mouse packet framing → [`InputEvent`]s.
//!
//! The aux port delivers a **three-byte packet** with no framing byte:
//!
//! ```text
//!   byte 0:  YO XO YS XS  1  M  R  L      bit 3 is always 1
//!   byte 1:  X delta, sign in byte 0's XS
//!   byte 2:  Y delta, sign in byte 0's YS
//!   byte 3:  Z delta, signed — IntelliMouse only, see below
//! ```
//!
//! ## Four bytes if the mouse has a wheel
//!
//! A mouse that answered [`arch::ps2`](crate::arch::ps2)'s IntelliMouse knock sends **four**
//! bytes per packet, the fourth a signed wheel delta. The length is not discoverable from the
//! stream — there is no flag in byte 0 and no framing byte to count from — so the decoder is
//! *told*, once, by whoever performed the knock. Telling it wrongly in either direction
//! desynchronises every packet after the first, which is the failure the whole resynchronisation
//! machinery below exists for; that is why [`Decoder::enable_wheel`] is a deliberate call made
//! at bring-up rather than something guessed at per packet.
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
//! ## Y is negated, and Z is not
//!
//! The wire reports positive-Y as *up*; [`REL_Y`] is positive-*down* to match screen
//! coordinates. Doing it here means exactly one place knows, rather than every consumer.
//!
//! The wheel's Z is already positive-**down** on the wire — rolling toward the user increases
//! it — and [`REL_WHEEL`] is positive-down for the same reason [`REL_Y`] is, so it passes
//! through untouched. **This is where the codebase parts company with Linux**, which negates Z
//! so that its `REL_WHEEL` is positive-up. The codes here are Linux's; this sign is the
//! screen's, so that a consumer adding a wheel delta to a scroll offset needs no sign of its
//! own. Stated in `docs/spec/rsproto-input-ops.md` rather than left to be discovered by
//! somebody whose scrolling comes out inverted.

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
    /// Wheel detents, positive **down** (straight off the wire — see the module docs).
    ///
    /// Always zero from a three-byte packet, which is a mouse with no wheel rather than a
    /// wheel that did not move.
    pub dz: i32,
    /// Left button held.
    pub left: bool,
    /// Right button held.
    pub right: bool,
    /// Middle button held.
    pub middle: bool,
}

/// The packet framing state machine, plus the button state needed to emit *changes*.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decoder {
    buf: [u8; 4],
    have: usize,
    /// How many bytes a packet is: three, or four once [`enable_wheel`](Self::enable_wheel)
    /// has been called.
    len: usize,
    /// Button state as of the last packet, so a press is emitted once rather than every
    /// packet while it is held.
    last: Packet,
    /// True once a packet has been seen, so the first packet's buttons are compared against
    /// "nothing held" rather than against uninitialised state.
    started: bool,
}

impl Decoder {
    /// A decoder awaiting a first byte, for a mouse with no wheel.
    ///
    /// **Three bytes until told otherwise**, because that is what an unconfigured PS/2 mouse
    /// sends and what a mouse that fails the knock goes on sending.
    pub const fn new() -> Self {
        Self {
            buf: [0; 4],
            have: 0,
            len: 3,
            last: Packet { dx: 0, dy: 0, dz: 0, left: false, right: false, middle: false },
            started: false,
        }
    }

    /// Read four-byte packets from now on: the mouse answered the IntelliMouse knock.
    ///
    /// **Call once, at bring-up, and only when the device id really was `0x03`.** Nothing in
    /// the byte stream distinguishes the two packet lengths, so this is the only thing that
    /// keeps the decoder framed — and a wrong answer either way reads every packet at an
    /// offset, turning deltas into button states.
    ///
    /// Resets the partial packet, since a length change mid-packet has no sound interpretation.
    pub fn enable_wheel(&mut self) {
        self.len = 4;
        self.have = 0;
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
        if self.have < self.len {
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
            // **Not negated**, and not overflow-checked either: the wheel byte carries no
            // overflow flag, and a signed byte cannot say more than it says.
            dz: if self.len == 4 { self.buf[3] as i8 as i32 } else { 0 },
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
        if p.dz != 0 {
            push(InputEvent::rel(REL_WHEEL, p.dz, time_ns), &mut n);
        }
        if n > 0 {
            push(InputEvent::syn(time_ns), &mut n);
        }
        n
    }

    /// Largest number of events one packet can produce: three button changes, **three** axes,
    /// and the `SYN`. Callers size their scratch buffer with this.
    ///
    /// Six until the wheel arrived. Left at the worst case rather than made conditional on
    /// `len`: a buffer sized from a runtime answer is a buffer that is right until somebody
    /// enables the wheel after allocating it, and one event is 16 bytes.
    pub const MAX_EVENTS: usize = 7;
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

    /// Feed a whole three-byte packet, returning the decoded result.
    fn packet(d: &mut Decoder, flags: u8, x: u8, y: u8) -> Option<Packet> {
        assert_eq!(d.feed(flags), None);
        assert_eq!(d.feed(x), None);
        d.feed(y)
    }

    /// Feed a whole four-byte packet — what a mouse that answered the knock sends.
    fn wheel_packet(d: &mut Decoder, flags: u8, x: u8, y: u8, z: u8) -> Option<Packet> {
        assert_eq!(d.feed(flags), None);
        assert_eq!(d.feed(x), None);
        assert_eq!(d.feed(y), None, "the third byte no longer completes a packet");
        d.feed(z)
    }

    /// A decoder for a mouse with a wheel.
    fn wheel_decoder() -> Decoder {
        let mut d = Decoder::new();
        d.enable_wheel();
        d
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
        // Three buttons changing at once plus all three axes plus the SYN. If `MAX_EVENTS` is
        // ever too small the events are silently truncated, so this pins it.
        //
        // **The wheel is part of the worst case**, which is why this uses a four-byte decoder:
        // against a three-byte one it passed at six for as long as `MAX_EVENTS` was six, and
        // would have gone on passing while a wheel event fell off the end of every packet that
        // moved and clicked at the same time.
        let mut d = wheel_decoder();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let p = wheel_packet(&mut d, SYNC_BIT | BTN_L | BTN_R | BTN_M, 2, 2, 1).unwrap();
        let n = d.events(p, 1, &mut out);
        assert_eq!(n, Decoder::MAX_EVENTS, "worst case must fit exactly");
        assert_eq!(out[n - 1], InputEvent::syn(1), "and the group is still terminated");
    }

    /// A wheel mouse's packets are four bytes, and the fourth is the wheel.
    #[test]
    fn a_wheel_packet_takes_four_bytes_and_the_last_is_the_detent() {
        let mut d = wheel_decoder();
        let p = wheel_packet(&mut d, SYNC_BIT, 1, 0, 1).expect("the fourth byte completes it");
        assert_eq!(p.dx, 1);
        assert_eq!(p.dz, 1);
    }

    /// The wheel is signed, and **not** negated the way Y is.
    ///
    /// **The assertion this module is most likely to be wrong about**, and the one no amount
    /// of host testing can settle on its own: the sign is a claim about the wire, and the wire
    /// is in the guest. `cargo xtask check-input` injects a real wheel over QMP and asserts the
    /// same direction, which is what makes this pair meaningful — this pins the intent, that
    /// pins the hardware. Linux negates here; this does not, because `REL_WHEEL` is
    /// positive-down in this system, as `REL_Y` is.
    #[test]
    fn the_wheel_is_signed_and_passes_through_with_the_wires_sign() {
        let mut d = wheel_decoder();
        let down = wheel_packet(&mut d, SYNC_BIT, 0, 0, 1).unwrap();
        assert_eq!(down.dz, 1, "the wire's positive Z is a turn toward the user, and stays positive");

        let up = wheel_packet(&mut d, SYNC_BIT, 0, 0, 0xFF).unwrap();
        assert_eq!(up.dz, -1, "and 0xFF is -1, not 255");

        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let n = d.events(up, 5, &mut out);
        assert_eq!(n, 2, "the wheel and its SYN");
        assert_eq!(out[0], InputEvent::rel(REL_WHEEL, -1, 5));
    }

    /// A mouse with no wheel reports none.
    ///
    /// **What this can and cannot see** (PR #288 review, 5). It pins the *behaviour* — a
    /// three-byte decoder emits no `REL_WHEEL` — and that behaviour holds for two independent
    /// reasons: `events` reads `dz` only when it is non-zero, and `feed` never writes `buf[3]`
    /// at this length so `dz` cannot be anything else. Deleting the `len == 4` check in `feed`
    /// therefore leaves this green. The check stays as a statement of intent rather than as a
    /// guard this test defends, and there is no state to reach it from: nothing turns a
    /// four-byte decoder back into a three-byte one.
    #[test]
    fn a_three_byte_mouse_never_reports_a_wheel() {
        let mut d = Decoder::new();
        let p = packet(&mut d, SYNC_BIT, 3, 0).unwrap();
        assert_eq!(p.dz, 0);
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let n = d.events(p, 1, &mut out);
        assert!(
            !out[..n].iter().any(|e| e.kind == EV_REL && e.code == REL_WHEEL),
            "a decoder framing three bytes has no wheel byte to read"
        );
    }

    /// Four-byte packets resynchronise too, and at the four-byte stride.
    ///
    /// **Not covered by the three-byte version of this test.** A decoder that dropped bytes
    /// until a sync bit but then counted three would re-frame onto the *wrong* boundary and
    /// stay one byte out for ever — plausible enough to be worth its own case, since the
    /// length is the one thing about this decoder that is now configurable.
    #[test]
    fn a_wheel_decoder_reframes_at_four_bytes() {
        let mut d = wheel_decoder();
        d.feed(SYNC_BIT);
        d.feed(9);
        // The rest of the packet never arrives; the next real packet's bytes land where this
        // one expected its Y and Z, completing one garbage packet.
        assert!(d.feed(SYNC_BIT).is_none());
        assert!(d.feed(0).is_some(), "the decoder cannot know this was garbage");
        let p = wheel_packet(&mut d, SYNC_BIT, 4, 0, 2).expect("re-framed");
        assert_eq!((p.dx, p.dz), (4, 2), "and at the four-byte stride, not the three-byte one");
    }

    /// Enabling the wheel abandons a half-read packet rather than finishing it at the new
    /// length, which would splice two packets' bytes into one.
    #[test]
    fn enabling_the_wheel_drops_a_packet_in_progress() {
        let mut d = Decoder::new();
        d.feed(SYNC_BIT);
        d.feed(7);
        d.enable_wheel();
        let p = wheel_packet(&mut d, SYNC_BIT, 1, 0, 0).expect("a whole packet after the switch");
        assert_eq!(p.dx, 1, "and it is the new packet's, not two packets spliced");
    }
}
