//! PS/2 mouse packet framing → [`InputEvent`]s.
//!
//! The aux port delivers a **three-byte packet** with no framing byte:
//!
//! ```text
//!   byte 0:  YO XO YS XS  1  M  R  L      bit 3 is always 1
//!   byte 1:  X delta, sign in byte 0's XS
//!   byte 2:  Y delta, sign in byte 0's YS
//!   byte 3:  Z delta, signed — IntelliMouse only
//! ```
//!
//! A mouse that answered [`arch::ps2`](crate::arch::ps2)'s IntelliMouse knock sends **four** bytes
//! per packet; the length is not in the stream, so the decoder is *told*
//! ([`Decoder::enable_wheel`]).
//!
//! **Resynchronisation is the whole problem**: drop or gain one byte and every later packet is read
//! out of phase with no self-correction, so the decoder **validates bit 3 of every first byte** and
//! drops bytes until it is set — imperfect, but it converges within a packet or two.
//!
//! **Y is negated, Z is not**: [`REL_Y`] is positive-*down*, the wire's *up*; the wheel's Z is
//! already positive-down, as [`REL_WHEEL`] is — **unlike Linux**. See
//! `docs/spec/rsproto-input-ops.md`.

use crate::libkern::input::*;

/// Bit 3 of the first packet byte, always set by the hardware. The only framing signal.
const SYNC_BIT: u8 = 0x08;
const BTN_L: u8 = 0x01;
const BTN_R: u8 = 0x02;
const BTN_M: u8 = 0x04;
const X_SIGN: u8 = 0x10;
const Y_SIGN: u8 = 0x20;
const X_OVERFLOW: u8 = 0x40;
const Y_OVERFLOW: u8 = 0x80;

/// A decoded movement/button report.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Packet {
    /// Horizontal delta, positive right.
    pub dx: i32,
    /// Vertical delta, positive **down** (already negated from the wire).
    pub dy: i32,
    /// Wheel detents, positive **down** (off the wire); always zero from a three-byte packet.
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
    len: usize,
    last: Packet,
    started: bool,
}

impl Decoder {
    /// A decoder for a mouse with no wheel: three bytes until told otherwise.
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
    /// **Call once, at bring-up, and only when the device id really was `0x03`**: a wrong answer
    /// either way reads every packet at an offset. Resets the partial packet.
    pub fn enable_wheel(&mut self) {
        self.len = 4;
        self.have = 0;
    }

    /// Feed one byte from the aux port: a [`Packet`] when a well-framed group completes.
    pub fn feed(&mut self, byte: u8) -> Option<Packet> {
        if self.have == 0 && byte & SYNC_BIT == 0 {
            // Not a first byte: discarding is the resync.
            return None;
        }
        self.buf[self.have] = byte;
        self.have += 1;
        if self.have < self.len {
            return None;
        }
        self.have = 0;

        let flags = self.buf[0];
        // An overflowed axis has a meaningless magnitude: report no motion.
        let dx = if flags & X_OVERFLOW != 0 { 0 } else { sign_extend(self.buf[1], flags & X_SIGN != 0) };
        let dy = if flags & Y_OVERFLOW != 0 { 0 } else { sign_extend(self.buf[2], flags & Y_SIGN != 0) };

        Some(Packet {
            dx,
            dy: -dy,
            // **Not negated**, and not overflow-checked: the wheel byte carries no overflow flag.
            dz: if self.len == 4 { self.buf[3] as i8 as i32 } else { 0 },
            left: flags & BTN_L != 0,
            right: flags & BTN_R != 0,
            middle: flags & BTN_M != 0,
        })
    }

    /// Turn a packet into the events it implies, appending to `out` and returning how many; only
    /// **changes** are emitted, and no `SYN` for a packet with none.
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

        for (now, was, code) in
            [(p.left, prev.left, BTN_LEFT), (p.right, prev.right, BTN_RIGHT), (p.middle, prev.middle, BTN_MIDDLE)]
        {
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

    /// Largest number of events one packet can produce (three button changes, **three** axes, the
    /// `SYN`), for sizing scratch buffers: fixed at the worst case rather than by packet length, so
    /// a buffer sized before the wheel is enabled still fits.
    pub const MAX_EVENTS: usize = 7;
}

fn sign_extend(magnitude: u8, negative: bool) -> i32 {
    if negative {
        // 9-bit two's complement split across the flags byte; the byte holds the low 8 bits.
        magnitude as i32 - 0x100
    } else {
        magnitude as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(d: &mut Decoder, flags: u8, x: u8, y: u8) -> Option<Packet> {
        assert_eq!(d.feed(flags), None);
        assert_eq!(d.feed(x), None);
        d.feed(y)
    }

    fn wheel_packet(d: &mut Decoder, flags: u8, x: u8, y: u8, z: u8) -> Option<Packet> {
        assert_eq!(d.feed(flags), None);
        assert_eq!(d.feed(x), None);
        assert_eq!(d.feed(y), None, "the third byte no longer completes a packet");
        d.feed(z)
    }

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
        let p = packet(&mut d, SYNC_BIT, 1, 1).unwrap();
        assert_eq!(p.dx, 1);
    }

    #[test]
    fn the_decoder_resynchronises_after_a_lost_byte() {
        let mut d = Decoder::new();
        d.feed(SYNC_BIT); // first byte of a packet…
        d.feed(9); // …its X…
        let garbage = d.feed(SYNC_BIT | BTN_L);
        assert!(garbage.is_some(), "the decoder cannot know this was garbage");
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
        // All three buttons, all three axes and the SYN. **The wheel is part of the worst case**: a
        // three-byte decoder would pass.
        let mut d = wheel_decoder();
        let mut out = [InputEvent::default(); Decoder::MAX_EVENTS];
        let p = wheel_packet(&mut d, SYNC_BIT | BTN_L | BTN_R | BTN_M, 2, 2, 1).unwrap();
        let n = d.events(p, 1, &mut out);
        assert_eq!(n, Decoder::MAX_EVENTS, "worst case must fit exactly");
        assert_eq!(out[n - 1], InputEvent::syn(1), "and the group is still terminated");
    }

    #[test]
    fn a_wheel_packet_takes_four_bytes_and_the_last_is_the_detent() {
        let mut d = wheel_decoder();
        let p = wheel_packet(&mut d, SYNC_BIT, 1, 0, 1).expect("the fourth byte completes it");
        assert_eq!(p.dx, 1);
        assert_eq!(p.dz, 1);
    }

    /// Not negated the way Y is; only `cargo xtask check-input` can settle the wire's sign.
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

    /// A mouse with no wheel reports none; the `len == 4` check in `feed` is intent only.
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

    #[test]
    fn a_wheel_decoder_reframes_at_four_bytes() {
        let mut d = wheel_decoder();
        d.feed(SYNC_BIT);
        d.feed(9);
        assert!(d.feed(SYNC_BIT).is_none());
        assert!(d.feed(0).is_some(), "the decoder cannot know this was garbage");
        let p = wheel_packet(&mut d, SYNC_BIT, 4, 0, 2).expect("re-framed");
        assert_eq!((p.dx, p.dz), (4, 2), "and at the four-byte stride, not the three-byte one");
    }

    /// Enabling the wheel abandons a half-read packet rather than splicing two packets' bytes.
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
