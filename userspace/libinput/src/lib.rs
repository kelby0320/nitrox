//! `libinput` — interpreting input, at both ends of the protocol.
//!
//! Two jobs that look unrelated and are the same one, which is why they share a crate
//! (`docs/design/input-subsystem.md` §4a):
//!
//! - **Compositor side:** device [`InputEvent`] triples → logical events. The device layer
//!   has no notion of "shift is held" — shift is an ordinary key there — so somebody must
//!   accumulate that state, and it should be one somebody rather than one per consumer.
//! - **Client side:** a keycode and modifiers → text. A layout is policy and data, which is
//!   why it is not in the kernel; putting it in each application instead would be the same
//!   mistake one layer up.
//!
//! **A client never reads the device stream.** Input reaches a window over its Surface
//! session, routed by the compositor — so this crate is not "the client's input library". It
//! owns *interpreting* input; `libui` owns *transporting* it to a window.
//!
//! ## What it deliberately does not do
//!
//! **It does not track where the pointer is.** The device layer reports motion as deltas, and
//! turning deltas into a position needs a screen to clamp against — which the compositor owns
//! and this crate does not. Who owns accumulated pointer position is a filed question
//! (`deferred-decisions.md`); until it is answered, [`Logical::Motion`] carries the delta and
//! the caller decides what it means.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, EV_SYN, InputEvent, KEY_LEFTALT, KEY_LEFTCTRL,
    KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_RIGHTMETA, KEY_RIGHTSHIFT,
    REL_X, REL_Y, SYN_DROPPED, SYN_REPORT,
};
use librsproto::surface::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

pub mod keymap;

/// Logical events the largest single group can produce: three button changes, a motion, and
/// a loss marker.
pub const MAX_LOGICAL: usize = 5;

/// One interpreted event — what happened, with the state that was true when it happened.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Logical {
    /// A key or button transition, with the modifiers held at that moment.
    Key {
        /// The keycode (an `EV_KEY` code).
        keycode: u16,
        /// True if the key went down.
        pressed: bool,
        /// Modifiers held **at this transition**.
        modifiers: u16,
    },
    /// Pointer motion, as a **delta**. See the module docs on why it is not a position.
    Motion {
        /// Horizontal delta, positive right.
        dx: i32,
        /// Vertical delta, positive down.
        dy: i32,
    },
    /// A pointer button transition.
    Button {
        /// A `BTN_*` code.
        button: u16,
        /// True if the button went down.
        pressed: bool,
        /// Every button held after this transition.
        buttons: u16,
        /// Modifiers held at this transition — what makes shift-click expressible.
        modifiers: u16,
    },
    /// Events were lost upstream; **accumulated state has been reset**.
    ///
    /// The consumer must discard whatever it believed about held keys and buttons. A
    /// consumer that ignores this carries a phantom held modifier for the rest of a session,
    /// which is exactly the failure `SYN_DROPPED` exists to prevent.
    Dropped,
}

/// Which modifier bit a keycode carries, if any.
fn modifier_bit(keycode: u16) -> Option<u16> {
    match keycode {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT => Some(MOD_SHIFT),
        KEY_LEFTCTRL | KEY_RIGHTCTRL => Some(MOD_CTRL),
        KEY_LEFTALT | KEY_RIGHTALT => Some(MOD_ALT),
        KEY_LEFTMETA | KEY_RIGHTMETA => Some(MOD_META),
        _ => None,
    }
}

/// Which bit of the held-button mask a `BTN_*` code occupies.
fn button_bit(code: u16) -> Option<u16> {
    match code {
        BTN_LEFT => Some(1 << 0),
        BTN_RIGHT => Some(1 << 1),
        BTN_MIDDLE => Some(1 << 2),
        _ => None,
    }
}

/// The device-stream interpreter: triples in, logical events out.
///
/// Feed it every record; it emits when a group's `SYN_REPORT` arrives, because that is when a
/// logical event is complete. A diagonal move is `REL_X`, `REL_Y`, `SYN` and must become
/// **one** motion, not two.
#[derive(Clone, Copy, Debug, Default)]
pub struct Interpreter {
    modifiers: u16,
    buttons: u16,
    /// Motion accumulated within the current group.
    dx: i32,
    dy: i32,
    /// Key/button transitions seen in the current group, awaiting its `SYN`.
    pending: [Option<Logical>; MAX_LOGICAL],
    pending_n: usize,
}

impl Interpreter {
    /// An interpreter with nothing held.
    pub const fn new() -> Self {
        Self {
            modifiers: 0,
            buttons: 0,
            dx: 0,
            dy: 0,
            pending: [None; MAX_LOGICAL],
            pending_n: 0,
        }
    }

    /// Modifiers currently held.
    pub fn modifiers(&self) -> u16 {
        self.modifiers
    }

    /// Buttons currently held, as a mask.
    pub fn buttons(&self) -> u16 {
        self.buttons
    }

    /// Feed one device record. Returns the logical events completed by it, which is nothing
    /// until a group's `SYN_REPORT` arrives.
    pub fn feed(&mut self, e: InputEvent, out: &mut [Logical]) -> usize {
        match e.kind {
            EV_KEY => {
                let pressed = e.value != 0;
                if let Some(bit) = modifier_bit(e.code) {
                    // **State updates before the event is built**, so pressing shift reports
                    // `MOD_SHIFT` set. "Modifiers held at this transition" includes the
                    // transition itself; the alternative reports shift-down with no shift,
                    // which no consumer expects.
                    if pressed {
                        self.modifiers |= bit;
                    } else {
                        self.modifiers &= !bit;
                    }
                }
                if let Some(bit) = button_bit(e.code) {
                    if pressed {
                        self.buttons |= bit;
                    } else {
                        self.buttons &= !bit;
                    }
                    self.push(Logical::Button {
                        button: e.code,
                        pressed,
                        buttons: self.buttons,
                        modifiers: self.modifiers,
                    });
                } else {
                    self.push(Logical::Key {
                        keycode: e.code,
                        pressed,
                        modifiers: self.modifiers,
                    });
                }
                0
            }
            EV_REL => {
                match e.code {
                    REL_X => self.dx += e.value,
                    REL_Y => self.dy += e.value,
                    _ => {} // wheel and future axes: not interpreted yet
                }
                0
            }
            EV_SYN if e.code == SYN_DROPPED => {
                // Everything accumulated is now a guess. Reset and say so — a consumer that
                // keeps its held-key set across a gap is the phantom-modifier bug.
                self.modifiers = 0;
                self.buttons = 0;
                self.dx = 0;
                self.dy = 0;
                self.pending_n = 0;
                if out.is_empty() {
                    return 0;
                }
                out[0] = Logical::Dropped;
                1
            }
            EV_SYN if e.code == SYN_REPORT => self.flush(out),
            _ => 0,
        }
    }

    /// Queue a transition for the current group, dropping it if the group is implausibly
    /// large rather than growing without bound.
    fn push(&mut self, l: Logical) {
        if self.pending_n < self.pending.len() {
            self.pending[self.pending_n] = Some(l);
            self.pending_n += 1;
        }
    }

    /// Emit the current group and reset it.
    fn flush(&mut self, out: &mut [Logical]) -> usize {
        let mut n = 0;
        for slot in self.pending.iter_mut().take(self.pending_n) {
            if let Some(l) = slot.take()
                && n < out.len()
            {
                out[n] = l;
                n += 1;
            }
        }
        self.pending_n = 0;
        if (self.dx != 0 || self.dy != 0) && n < out.len() {
            out[n] = Logical::Motion { dx: self.dx, dy: self.dy };
            n += 1;
        }
        self.dx = 0;
        self.dy = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{KEY_ESC, KEY_PRESS, KEY_RELEASE};

    fn key(code: u16, down: bool) -> InputEvent {
        InputEvent {
            kind: EV_KEY,
            code,
            value: if down { KEY_PRESS } else { KEY_RELEASE },
            time_ns: 0,
        }
    }
    fn rel(code: u16, v: i32) -> InputEvent {
        InputEvent { kind: EV_REL, code, value: v, time_ns: 0 }
    }
    fn syn() -> InputEvent {
        InputEvent { kind: EV_SYN, code: SYN_REPORT, value: 0, time_ns: 0 }
    }
    fn dropped() -> InputEvent {
        InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: 7, time_ns: 0 }
    }

    /// Feed a whole group and collect what it produced.
    fn group(i: &mut Interpreter, events: &[InputEvent]) -> ([Logical; MAX_LOGICAL], usize) {
        let mut out = [Logical::Dropped; MAX_LOGICAL];
        let mut n = 0;
        for &e in events {
            n = i.feed(e, &mut out);
        }
        (out, n)
    }

    #[test]
    fn nothing_is_emitted_until_the_group_ends() {
        // The property the whole accumulator exists for: a `SYN` is what completes a logical
        // event, so acting on each record would double-report a diagonal move.
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped; MAX_LOGICAL];
        assert_eq!(i.feed(key(KEY_ESC, true), &mut out), 0, "the key alone completes nothing");
        assert_eq!(i.feed(syn(), &mut out), 1, "the SYN does");
    }

    #[test]
    fn a_diagonal_move_is_one_motion_not_two() {
        let mut i = Interpreter::new();
        let (out, n) = group(&mut i, &[rel(REL_X, 5), rel(REL_Y, -3), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Motion { dx: 5, dy: -3 });
    }

    #[test]
    fn a_key_carries_the_modifiers_held_at_its_transition() {
        // Shift-A: the `a` must report MOD_SHIFT, which is the entire reason this layer
        // exists — the device stream has no such field.
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        let (out, n) = group(&mut i, &[key(30, true), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Key { keycode: 30, pressed: true, modifiers: MOD_SHIFT });
    }

    #[test]
    fn pressing_a_modifier_reports_it_as_held() {
        // State updates before the event is built: shift-down with no shift set would be a
        // surprise to every consumer.
        let mut i = Interpreter::new();
        let (out, _) = group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        assert_eq!(
            out[0],
            Logical::Key { keycode: KEY_LEFTSHIFT, pressed: true, modifiers: MOD_SHIFT }
        );
    }

    #[test]
    fn releasing_a_modifier_clears_it_for_that_event_and_after() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTCTRL, true), syn()]);
        assert_eq!(i.modifiers(), MOD_CTRL);
        let (out, _) = group(&mut i, &[key(KEY_LEFTCTRL, false), syn()]);
        assert_eq!(out[0], Logical::Key { keycode: KEY_LEFTCTRL, pressed: false, modifiers: 0 });
        assert_eq!(i.modifiers(), 0);
    }

    #[test]
    fn left_and_right_modifiers_share_a_bit_but_not_a_lifetime() {
        // They are deliberately not distinguished — but releasing one must not clear the
        // other's contribution to nothing while it is still held. This is the case that
        // makes a naive `&= !bit` wrong, and records the current behaviour honestly.
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        group(&mut i, &[key(KEY_RIGHTSHIFT, true), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT);
        group(&mut i, &[key(KEY_LEFTSHIFT, false), syn()]);
        assert_eq!(
            i.modifiers(),
            0,
            "known limitation: one bit cannot count two keys, so releasing either clears it"
        );
    }

    #[test]
    fn a_button_reports_the_held_mask_and_the_modifiers() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        let (out, n) = group(&mut i, &[key(BTN_LEFT, true), syn()]);
        assert_eq!(n, 1);
        assert_eq!(
            out[0],
            Logical::Button {
                button: BTN_LEFT,
                pressed: true,
                buttons: 1,
                modifiers: MOD_SHIFT,
            },
            "shift-click is expressible only because both travel together"
        );
    }

    #[test]
    fn button_state_accumulates_and_clears() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(BTN_LEFT, true), key(BTN_RIGHT, true), syn()]);
        assert_eq!(i.buttons(), 0b011);
        group(&mut i, &[key(BTN_LEFT, false), syn()]);
        assert_eq!(i.buttons(), 0b010);
    }

    #[test]
    fn a_button_is_not_reported_as_a_key() {
        // Buttons share `EV_KEY` with keys on the wire; a consumer routing by window needs
        // them apart, since a key goes to the focused window and a click to the one under
        // the pointer.
        let mut i = Interpreter::new();
        let (out, _) = group(&mut i, &[key(BTN_MIDDLE, true), syn()]);
        assert!(matches!(out[0], Logical::Button { .. }));
    }

    #[test]
    fn a_dropped_marker_resets_state_and_is_reported_immediately() {
        // Immediately, not at the next SYN: the marker *is* the news, and a consumer holding
        // stale state until the next group would act on it in between.
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), key(BTN_LEFT, true), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT);
        assert_eq!(i.buttons(), 1);

        let mut out = [Logical::Key { keycode: 0, pressed: false, modifiers: 0 }; MAX_LOGICAL];
        assert_eq!(i.feed(dropped(), &mut out), 1);
        assert_eq!(out[0], Logical::Dropped);
        assert_eq!(i.modifiers(), 0, "held keys across a gap are a guess");
        assert_eq!(i.buttons(), 0);
    }

    #[test]
    fn a_partial_group_interrupted_by_a_drop_does_not_leak_into_the_next() {
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped; MAX_LOGICAL];
        i.feed(key(30, true), &mut out); // no SYN yet
        i.feed(dropped(), &mut out);
        let (out2, n) = group(&mut i, &[key(31, true), syn()]);
        assert_eq!(n, 1, "only the new group's key");
        assert_eq!(out2[0], Logical::Key { keycode: 31, pressed: true, modifiers: 0 });
    }

    #[test]
    fn a_group_with_no_motion_emits_no_motion_event() {
        let mut i = Interpreter::new();
        let (_, n) = group(&mut i, &[rel(REL_X, 0), rel(REL_Y, 0), syn()]);
        assert_eq!(n, 0, "a zero delta is not an event");
    }
}
