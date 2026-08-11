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
//! owns *interpreting* input; `libsurface` owns *transporting* it to a window.
//!
//! ## What it deliberately does not do
//!
//! **It does not track where the pointer is.** The device layer reports motion as deltas, and
//! turning deltas into a position needs a screen to clamp against — which the compositor owns
//! and this crate does not. That is now **decided rather than deferred**
//! (`deferred-decisions.md`, 2026-08-10): the compositor's `InputRouter` accumulates and
//! clamps, and [`Logical::Motion`] carries the delta plus the state held during it.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, EV_SYN, InputEvent, KEY_LEFTALT, KEY_LEFTCTRL,
    KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_RIGHTMETA, KEY_RIGHTSHIFT,
    REL_X, REL_Y, SYN_DROPPED, SYN_REPORT,
};
use librsproto::surface::{MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

pub mod keymap;

/// Transitions one group may hold before further ones are dropped.
///
/// Three buttons, a motion and a loss marker is the largest plausible group; a device
/// sending more than this in one `SYN` is malfunctioning, and dropping the surplus beats
/// growing a buffer on its say-so.
pub const MAX_LOGICAL: usize = 5;

/// How large an `out` slice [`Interpreter::feed`] needs to lose nothing: **one more than
/// [`MAX_LOGICAL`]**.
///
/// The accumulated motion is appended at flush time rather than queued, so a full group of
/// transitions plus a motion is `MAX_LOGICAL + 1` events. A caller sizing its buffer to
/// `MAX_LOGICAL` — as this crate's own tests did — silently loses the motion in exactly
/// that case, which reads as a cursor that stops while keys are held.
pub const MAX_PER_GROUP: usize = MAX_LOGICAL + 1;

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
        /// Every button held during this motion.
        ///
        /// Carried rather than left to the consumer because **a drag is motion plus a held
        /// button**, and the alternative is every consumer re-accumulating button state
        /// from the transitions — the per-application duplication this layer exists to
        /// prevent. A first version omitted it, and the resulting `PointerEvent` reported
        /// `buttons == 0` for every motion of every drag (PR #180 review, finding 2).
        buttons: u16,
        /// Modifiers held during this motion — what makes a shift-drag expressible.
        modifiers: u16,
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

/// Every modifier key, and the mask bit it contributes to. Order fixes each key's slot in
/// [`Interpreter::held_mods`].
const MOD_KEYS: [(u16, u16); 8] = [
    (KEY_LEFTSHIFT, MOD_SHIFT),
    (KEY_RIGHTSHIFT, MOD_SHIFT),
    (KEY_LEFTCTRL, MOD_CTRL),
    (KEY_RIGHTCTRL, MOD_CTRL),
    (KEY_LEFTALT, MOD_ALT),
    (KEY_RIGHTALT, MOD_ALT),
    (KEY_LEFTMETA, MOD_META),
    (KEY_RIGHTMETA, MOD_META),
];

/// Which slot in the held-keys set a keycode occupies, if it is a modifier.
fn modifier_slot(keycode: u16) -> Option<usize> {
    MOD_KEYS.iter().position(|&(k, _)| k == keycode)
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
    /// Which modifier **keys** are down, one bit per entry of [`MOD_KEYS`].
    ///
    /// The mask handed to consumers is *derived* from this rather than edited in place. The
    /// first version tracked the mask directly and cleared `MOD_SHIFT` when either shift
    /// came up — so holding both and releasing one reported no shift while a key was still
    /// down. Tracking the keys and deriving the mask is how X11 and Wayland avoid the same
    /// bug with a single `ShiftMask` bit: **the shared bit was never the defect**.
    held_mods: u8,
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
            held_mods: 0,
            buttons: 0,
            dx: 0,
            dy: 0,
            pending: [None; MAX_LOGICAL],
            pending_n: 0,
        }
    }

    /// Modifiers currently held, derived from which modifier keys are down.
    ///
    /// Left and right share a bit deliberately — a client asking "was shift held" should not
    /// have to ask twice, which is what X11's `ShiftMask` and Wayland's xkb mask also do.
    /// The *keycodes* stay distinct, so a consumer that genuinely needs the side reads them;
    /// adding `MOD_*_R` bits later is additive.
    pub fn modifiers(&self) -> u16 {
        let mut m = 0;
        for (i, &(_, bit)) in MOD_KEYS.iter().enumerate() {
            if self.held_mods & (1 << i) != 0 {
                m |= bit;
            }
        }
        m
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
                if let Some(slot) = modifier_slot(e.code) {
                    // **State updates before the event is built**, so pressing shift reports
                    // `MOD_SHIFT` set. "Modifiers held at this transition" includes the
                    // transition itself; the alternative reports shift-down with no shift,
                    // which no consumer expects.
                    if pressed {
                        self.held_mods |= 1 << slot;
                    } else {
                        self.held_mods &= !(1 << slot);
                    }
                }
                let modifiers = self.modifiers();
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
                        modifiers,
                    });
                } else {
                    self.push(Logical::Key { keycode: e.code, pressed, modifiers });
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
                self.held_mods = 0;
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
            out[n] = Logical::Motion {
                dx: self.dx,
                dy: self.dy,
                buttons: self.buttons,
                modifiers: self.modifiers(),
            };
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
    fn group(i: &mut Interpreter, events: &[InputEvent]) -> ([Logical; MAX_PER_GROUP], usize) {
        let mut out = [Logical::Dropped; MAX_PER_GROUP];
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
        let mut out = [Logical::Dropped; MAX_PER_GROUP];
        assert_eq!(i.feed(key(KEY_ESC, true), &mut out), 0, "the key alone completes nothing");
        assert_eq!(i.feed(syn(), &mut out), 1, "the SYN does");
    }

    #[test]
    fn a_motion_carries_the_buttons_held_during_it() {
        // A drag is motion plus a held button. Without this a consumer implementing the
        // standard "on motion, if a button is down, move the object" gets `buttons == 0`
        // for every motion of every drag, and nothing ever drags (PR #180 review).
        let mut i = Interpreter::new();
        group(&mut i, &[key(BTN_LEFT, true), syn()]);
        let (out, n) = group(&mut i, &[rel(REL_X, 4), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Motion { dx: 4, dy: 0, buttons: 1, modifiers: 0 });

        let (out, _) = group(&mut i, &[key(KEY_LEFTSHIFT, true), rel(REL_X, 2), syn()]);
        let motion = out.iter().find(|l| matches!(l, Logical::Motion { .. })).expect("motion");
        assert_eq!(
            *motion,
            Logical::Motion { dx: 2, dy: 0, buttons: 1, modifiers: MOD_SHIFT },
            "a shift-drag is expressible without the consumer tracking either"
        );
    }

    #[test]
    fn a_diagonal_move_is_one_motion_not_two() {
        let mut i = Interpreter::new();
        let (out, n) = group(&mut i, &[rel(REL_X, 5), rel(REL_Y, -3), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Motion { dx: 5, dy: -3, buttons: 0, modifiers: 0 });
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
    fn releasing_one_shift_while_the_other_is_held_keeps_shift_set() {
        // The bug a naive `mask &= !bit` has, and the reason the mask is *derived* from held
        // keys rather than edited: with both shifts down, releasing one must not report that
        // no shift is held while a key is still physically down. X11 and Wayland avoid this
        // with a single `ShiftMask` too — the shared bit was never the defect.
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        group(&mut i, &[key(KEY_RIGHTSHIFT, true), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT);

        group(&mut i, &[key(KEY_LEFTSHIFT, false), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT, "the right shift is still down");

        let (out, _) = group(&mut i, &[key(30, true), syn()]);
        assert_eq!(
            out[0],
            Logical::Key { keycode: 30, pressed: true, modifiers: MOD_SHIFT },
            "so a key typed now is still shifted"
        );

        group(&mut i, &[key(KEY_RIGHTSHIFT, false), syn()]);
        assert_eq!(i.modifiers(), 0, "and only when both are up does it clear");
    }

    #[test]
    fn the_sides_are_tracked_separately_across_every_modifier() {
        // The same asymmetry for ctrl/alt/meta, since one shared slot per pair would pass
        // the shift test above by luck.
        for (l, r, bit) in [
            (KEY_LEFTCTRL, KEY_RIGHTCTRL, MOD_CTRL),
            (KEY_LEFTALT, KEY_RIGHTALT, MOD_ALT),
            (KEY_LEFTMETA, KEY_RIGHTMETA, MOD_META),
        ] {
            let mut i = Interpreter::new();
            group(&mut i, &[key(l, true), syn()]);
            group(&mut i, &[key(r, true), syn()]);
            group(&mut i, &[key(l, false), syn()]);
            assert_eq!(i.modifiers(), bit, "{l} released, {r} still held");
            group(&mut i, &[key(r, false), syn()]);
            assert_eq!(i.modifiers(), 0);
        }
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

        let mut out =
            [Logical::Key { keycode: 0, pressed: false, modifiers: 0 }; MAX_PER_GROUP];
        assert_eq!(i.feed(dropped(), &mut out), 1);
        assert_eq!(out[0], Logical::Dropped);
        assert_eq!(i.modifiers(), 0, "held keys across a gap are a guess");
        assert_eq!(i.buttons(), 0);
    }

    #[test]
    fn a_full_group_plus_motion_needs_the_whole_max_per_group() {
        // Motion is appended at flush rather than queued, so it does not compete with the
        // transitions for `MAX_LOGICAL` slots — it needs the extra one. Sized at
        // `MAX_LOGICAL` this loses the motion, and a cursor that freezes while three
        // buttons are held is a maddening bug to find from the symptom.
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped; MAX_PER_GROUP];
        for e in [
            key(BTN_LEFT, true),
            key(BTN_RIGHT, true),
            key(BTN_MIDDLE, true),
            key(KEY_LEFTSHIFT, true),
            key(KEY_LEFTCTRL, true),
            rel(REL_X, 7),
        ] {
            i.feed(e, &mut out);
        }
        let n = i.feed(syn(), &mut out);
        assert_eq!(n, MAX_PER_GROUP, "five transitions and the motion");
        assert_eq!(
            out[MAX_PER_GROUP - 1],
            Logical::Motion { dx: 7, dy: 0, buttons: 7, modifiers: MOD_SHIFT | MOD_CTRL },
            "and it carries the state the same group established"
        );

        let mut small = [Logical::Dropped; MAX_LOGICAL];
        for e in [
            key(BTN_LEFT, true),
            key(BTN_RIGHT, true),
            key(BTN_MIDDLE, true),
            key(KEY_LEFTSHIFT, true),
            key(KEY_LEFTCTRL, true),
            rel(REL_X, 7),
        ] {
            i.feed(e, &mut small);
        }
        let n = i.feed(syn(), &mut small);
        assert_eq!(n, MAX_LOGICAL, "and one short, the motion is what falls off the end");
        assert!(!small.iter().any(|l| matches!(l, Logical::Motion { .. })));
    }

    #[test]
    fn a_partial_group_interrupted_by_a_drop_does_not_leak_into_the_next() {
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped; MAX_PER_GROUP];
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
