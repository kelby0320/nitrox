//! Interpreting input (`docs/architecture/input-subsystem.md` §4a): device [`InputEvent`]
//! triples → logical events on the compositor side, and keycode + modifiers → text
//! ([`keymap`]) on the client side.
//!
//! The device layer has no notion of "shift is held", so this crate accumulates it once. It
//! does not track the pointer's position, which needs a screen to clamp against: the
//! compositor's `InputRouter` does, and [`Logical::Motion`] carries the delta.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use libkern::abi::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EV_KEY, EV_REL, EV_SYN, InputEvent, KEY_CAPSLOCK, KEY_DELETE, KEY_DOWN, KEY_END,
    KEY_ENTER, KEY_HOME, KEY_INSERT, KEY_KPENTER, KEY_LEFT, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT,
    KEY_NUMLOCK, KEY_PAGEDOWN, KEY_PAGEUP, KEY_RIGHT, KEY_RIGHTALT, KEY_RIGHTCTRL, KEY_RIGHTMETA, KEY_RIGHTSHIFT,
    KEY_UP, LIGHT_CAPS, LIGHT_NUM, REL_WHEEL, REL_X, REL_Y, SYN_DROPPED, SYN_REPORT,
};
use librsproto::surface::{LOCK_CAPS, LOCK_NUM, MOD_ALT, MOD_CTRL, MOD_META, MOD_SHIFT};

pub mod keymap;

/// Transitions one group may hold before further ones are dropped: three buttons, a motion and a
/// loss marker is the largest plausible group, and a device sending more is malfunctioning.
pub const MAX_LOGICAL: usize = 5;

/// How large an `out` slice [`Interpreter::feed`] needs to lose nothing: [`MAX_LOGICAL`] plus
/// the accumulated motion and wheel, which are appended at flush time rather than queued.
pub const MAX_PER_GROUP: usize = MAX_LOGICAL + 2;

/// One interpreted event — what happened, when, and the state that was true then.
///
/// Every variant carries `time_ns`, the kernel's stamp at the interrupt, not the time the event
/// was delivered; a double click is two presses close in time.
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
        /// The locks on **at this transition**, a lock key's own press included: `LOCK_CAPS`,
        /// `LOCK_NUM`.
        locks: u16,
        /// Kernel monotonic time at the interrupt. See the type's own docs.
        time_ns: u64,
    },
    /// Pointer motion, as a **delta**. See the module docs on why it is not a position.
    Motion {
        /// Horizontal delta, positive right.
        dx: i32,
        /// Vertical delta, positive down.
        dy: i32,
        /// Every button held during this motion: a drag is motion plus a held button.
        buttons: u16,
        /// Modifiers held during this motion — what makes a shift-drag expressible.
        modifiers: u16,
        /// The closing `SYN_REPORT`'s stamp: the motion sums several records, so none's own
        /// time is the answer.
        time_ns: u64,
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
        /// Kernel monotonic time at the interrupt — **the press time a double click needs**.
        time_ns: u64,
    },
    /// The wheel turned, as a **delta in detents**.
    ///
    /// Not an axis of [`Motion`](Self::Motion): motion moves a cursor, the wheel scrolls what
    /// is under it.
    Wheel {
        /// Detents, positive **down** (the screen's sign, not Linux's; see `REL_WHEEL`).
        dz: i32,
        /// Buttons held while it turned.
        buttons: u16,
        /// Modifiers held while it turned.
        modifiers: u16,
        /// The closing `SYN_REPORT`'s stamp, as for [`Motion`](Self::Motion).
        time_ns: u64,
    },
    /// Events were lost upstream; **accumulated state has been reset**.
    ///
    /// The consumer must discard what it believed about held keys and buttons, or it carries
    /// a phantom held modifier.
    Dropped {
        /// When the loss was reported; a consumer synthesising events on a drop stamps them
        /// with it.
        time_ns: u64,
    },
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

/// Whether `keycode` is a modifier key — shift, control, alt or meta, either side.
///
/// Modifiers arrive as ordinary [`Logical::Key`] events; a consumer such as key repeat must
/// not treat them as text keys.
pub fn is_modifier(keycode: u16) -> bool {
    modifier_slot(keycode).is_some()
}

/// The keypad's navigation keys, as `(keypad keycode, the key it stands for with Num Lock
/// off)`: 7 Home, 8 Up, 9 Page Up, 4 Left, 6 Right, 1 End, 2 Down, 3 Page Down, 0 Insert,
/// `.` Delete.
const KEYPAD_NAV: [(u16, u16); 10] = [
    (71, KEY_HOME),
    (72, KEY_UP),
    (73, KEY_PAGEUP),
    (75, KEY_LEFT),
    (77, KEY_RIGHT),
    (79, KEY_END),
    (80, KEY_DOWN),
    (81, KEY_PAGEDOWN),
    (82, KEY_INSERT),
    (83, KEY_DELETE),
];

/// Keypad 5, which stands for nothing with Num Lock off and is delivered as nothing.
const KEY_KP5: u16 = 76;

/// The lock keys, and the bit each toggles.
const LOCK_KEYS: [(u16, u16); 2] = [(KEY_CAPSLOCK, LOCK_CAPS), (KEY_NUMLOCK, LOCK_NUM)];

/// Whether `keycode` is a lock key — Caps Lock or Num Lock. Like a modifier, it is not one to
/// repeat.
pub fn is_lock_key(keycode: u16) -> bool {
    LOCK_KEYS.iter().any(|&(k, _)| k == keycode)
}

/// The keyboard lights `locks` call for: the byte a keyboard's node takes. Scroll Lock is not
/// a lock here, so its light stays off.
pub fn lights(locks: u16) -> u8 {
    let mut l = 0;
    if locks & LOCK_NUM != 0 {
        l |= LIGHT_NUM;
    }
    if locks & LOCK_CAPS != 0 {
        l |= LIGHT_CAPS;
    }
    l as u8
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
/// It emits when a group's `SYN_REPORT` arrives; a diagonal move (`REL_X`, `REL_Y`, `SYN`) is
/// one motion.
#[derive(Clone, Copy, Debug)]
pub struct Interpreter {
    /// Which modifier **keys** are down, one bit per entry of [`MOD_KEYS`].
    ///
    /// The mask handed to consumers is derived from this: editing a shared mask bit would
    /// clear `MOD_SHIFT` when one of two held shifts came up.
    held_mods: u8,
    buttons: u16,
    /// Motion accumulated within the current group.
    dx: i32,
    dy: i32,
    /// Wheel detents accumulated within the current group.
    dz: i32,
    /// Key/button transitions seen in the current group, awaiting its `SYN`.
    pending: [Option<Logical>; MAX_LOGICAL],
    pending_n: usize,
    /// The locks on: `LOCK_CAPS`, `LOCK_NUM`. Num Lock is on from the start.
    locks: u16,
    /// Which lock keys are down, one bit per entry of [`LOCK_KEYS`]: a held lock key toggles
    /// once, though a PS/2 keyboard's typematic repeat sends further presses with no release.
    lock_keys_down: u8,
    /// Which keypad keys are down, one bit per entry of [`KEYPAD_NAV`] and bit 10 for keypad 5,
    /// and which of them went down as navigation keys: **a release is its press's keycode**, so a
    /// key held while Num Lock changes releases what it pressed, and its repeats keep the first
    /// press's meaning.
    keypad_down: u16,
    keypad_as_nav: u16,
}

impl Default for Interpreter {
    /// [`Interpreter::new`]: nothing held, and Num Lock on.
    fn default() -> Self {
        Self::new()
    }
}

impl Interpreter {
    /// An interpreter with nothing held, and **Num Lock on**, so the keypad types digits.
    pub const fn new() -> Self {
        Self {
            held_mods: 0,
            buttons: 0,
            dx: 0,
            dy: 0,
            dz: 0,
            pending: [None; MAX_LOGICAL],
            pending_n: 0,
            locks: LOCK_NUM,
            lock_keys_down: 0,
            keypad_down: 0,
            keypad_as_nav: 0,
        }
    }

    /// The locks on: `LOCK_CAPS`, `LOCK_NUM`.
    pub fn locks(&self) -> u16 {
        self.locks
    }

    /// A first press toggles the lock; a release lets it toggle again.
    fn lock_key(&mut self, code: u16, pressed: bool) {
        let Some(i) = LOCK_KEYS.iter().position(|&(k, _)| k == code) else {
            return;
        };
        if pressed {
            if self.lock_keys_down & (1 << i) == 0 {
                self.locks ^= LOCK_KEYS[i].1;
            }
            self.lock_keys_down |= 1 << i;
        } else {
            self.lock_keys_down &= !(1 << i);
        }
    }

    /// The keycode a key is delivered as, or `None`: keypad Enter as Enter always, and with Num
    /// Lock off (as when the key first went down) the keypad's navigation keys as the keys they
    /// stand for and keypad 5 as nothing.
    fn translate(&mut self, code: u16, pressed: bool) -> Option<u16> {
        if code == KEY_KPENTER {
            return Some(KEY_ENTER);
        }
        let i = match KEYPAD_NAV.iter().position(|&(k, _)| k == code) {
            Some(i) => i,
            None if code == KEY_KP5 => KEYPAD_NAV.len(),
            None => return Some(code),
        };
        let bit = 1u16 << i;
        if pressed && self.keypad_down & bit == 0 {
            self.keypad_down |= bit;
            if self.locks & LOCK_NUM == 0 {
                self.keypad_as_nav |= bit;
            } else {
                self.keypad_as_nav &= !bit;
            }
        }
        let as_nav = self.keypad_as_nav & bit != 0;
        if !pressed {
            self.keypad_down &= !bit;
            self.keypad_as_nav &= !bit;
        }
        match (as_nav, KEYPAD_NAV.get(i)) {
            (false, _) => Some(code),
            (true, Some(&(_, nav))) => Some(nav),
            (true, None) => None,
        }
    }

    /// Modifiers currently held, derived from which modifier keys are down.
    ///
    /// Left and right share a bit; the keycodes stay distinct for a consumer that needs the side.
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
                    // Updated before the event is built: the modifiers held at a transition
                    // include the transition itself.
                    if pressed {
                        self.held_mods |= 1 << slot;
                    } else {
                        self.held_mods &= !(1 << slot);
                    }
                }
                let modifiers = self.modifiers();
                self.lock_key(e.code, pressed);
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
                        time_ns: e.time_ns,
                    });
                } else if let Some(keycode) = self.translate(e.code, pressed) {
                    self.push(Logical::Key { keycode, pressed, modifiers, locks: self.locks, time_ns: e.time_ns });
                }
                0
            }
            EV_REL => {
                match e.code {
                    REL_X => self.dx += e.value,
                    REL_Y => self.dy += e.value,
                    REL_WHEEL => self.dz += e.value,
                    _ => {} // future axes — a horizontal wheel, a dial
                }
                0
            }
            EV_SYN if e.code == SYN_DROPPED => {
                // A guess now: reset, and report the loss at once rather than at the next `SYN`.
                self.held_mods = 0;
                self.buttons = 0;
                // Not the locks: a lock is not a held key, and its light goes on showing it.
                self.lock_keys_down = 0;
                self.keypad_down = 0;
                self.keypad_as_nav = 0;
                self.dx = 0;
                self.dy = 0;
                self.dz = 0;
                self.pending_n = 0;
                if out.is_empty() {
                    return 0;
                }
                out[0] = Logical::Dropped { time_ns: e.time_ns };
                1
            }
            EV_SYN if e.code == SYN_REPORT => self.flush(e.time_ns, out),
            _ => 0,
        }
    }

    /// Queue a transition for the current group, dropping it if the group is full.
    fn push(&mut self, l: Logical) {
        if self.pending_n < self.pending.len() {
            self.pending[self.pending_n] = Some(l);
            self.pending_n += 1;
        }
    }

    /// Emit the current group and reset it; transitions keep their own stamps, the accumulated
    /// motion and wheel take `time_ns`.
    fn flush(&mut self, time_ns: u64, out: &mut [Logical]) -> usize {
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
                time_ns,
            };
            n += 1;
        }
        // After the motion: the cursor moves before the wheel's target is decided.
        if self.dz != 0 && n < out.len() {
            out[n] = Logical::Wheel { dz: self.dz, buttons: self.buttons, modifiers: self.modifiers(), time_ns };
            n += 1;
        }
        self.dx = 0;
        self.dy = 0;
        self.dz = 0;
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{KEY_ESC, KEY_PRESS, KEY_RELEASE};

    /// The time every helper stamps; nonzero so an interpreter that dropped `time_ns` fails.
    const T: u64 = 9_000_000_000;

    fn key(code: u16, down: bool) -> InputEvent {
        InputEvent { kind: EV_KEY, code, value: if down { KEY_PRESS } else { KEY_RELEASE }, time_ns: T }
    }
    fn rel(code: u16, v: i32) -> InputEvent {
        InputEvent { kind: EV_REL, code, value: v, time_ns: T }
    }
    fn syn() -> InputEvent {
        InputEvent { kind: EV_SYN, code: SYN_REPORT, value: 0, time_ns: T }
    }
    fn dropped() -> InputEvent {
        InputEvent { kind: EV_SYN, code: SYN_DROPPED, value: 7, time_ns: T }
    }

    /// Feed a whole group and collect what it produced.
    fn group(i: &mut Interpreter, events: &[InputEvent]) -> ([Logical; MAX_PER_GROUP], usize) {
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        let mut n = 0;
        for &e in events {
            n = i.feed(e, &mut out);
        }
        (out, n)
    }

    #[test]
    fn nothing_is_emitted_until_the_group_ends() {
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        assert_eq!(i.feed(key(KEY_ESC, true), &mut out), 0, "the key alone completes nothing");
        assert_eq!(i.feed(syn(), &mut out), 1, "the SYN does");
    }

    #[test]
    fn a_motion_carries_the_buttons_held_during_it() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(BTN_LEFT, true), syn()]);
        let (out, n) = group(&mut i, &[rel(REL_X, 4), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Motion { dx: 4, dy: 0, buttons: 1, modifiers: 0, time_ns: T });

        let (out, _) = group(&mut i, &[key(KEY_LEFTSHIFT, true), rel(REL_X, 2), syn()]);
        let motion = out.iter().find(|l| matches!(l, Logical::Motion { .. })).expect("motion");
        assert_eq!(
            *motion,
            Logical::Motion { dx: 2, dy: 0, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
            "a shift-drag is expressible without the consumer tracking either"
        );
    }

    #[test]
    fn a_diagonal_move_is_one_motion_not_two() {
        let mut i = Interpreter::new();
        let (out, n) = group(&mut i, &[rel(REL_X, 5), rel(REL_Y, -3), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Motion { dx: 5, dy: -3, buttons: 0, modifiers: 0, time_ns: T });
    }

    #[test]
    fn a_key_carries_the_modifiers_held_at_its_transition() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        let (out, n) = group(&mut i, &[key(30, true), syn()]);
        assert_eq!(n, 1);
        assert_eq!(
            out[0],
            Logical::Key { keycode: 30, pressed: true, modifiers: MOD_SHIFT, locks: LOCK_NUM, time_ns: T }
        );
    }

    #[test]
    fn pressing_a_modifier_reports_it_as_held() {
        let mut i = Interpreter::new();
        let (out, _) = group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        assert_eq!(
            out[0],
            Logical::Key { keycode: KEY_LEFTSHIFT, pressed: true, modifiers: MOD_SHIFT, locks: LOCK_NUM, time_ns: T }
        );
    }

    #[test]
    fn releasing_a_modifier_clears_it_for_that_event_and_after() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTCTRL, true), syn()]);
        assert_eq!(i.modifiers(), MOD_CTRL);
        let (out, _) = group(&mut i, &[key(KEY_LEFTCTRL, false), syn()]);
        assert_eq!(
            out[0],
            Logical::Key { keycode: KEY_LEFTCTRL, pressed: false, modifiers: 0, locks: LOCK_NUM, time_ns: T }
        );
        assert_eq!(i.modifiers(), 0);
    }

    #[test]
    fn releasing_one_shift_while_the_other_is_held_keeps_shift_set() {
        // A naive `mask &= !bit` would report no shift while one is still down.
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), syn()]);
        group(&mut i, &[key(KEY_RIGHTSHIFT, true), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT);

        group(&mut i, &[key(KEY_LEFTSHIFT, false), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT, "the right shift is still down");

        let (out, _) = group(&mut i, &[key(30, true), syn()]);
        assert_eq!(
            out[0],
            Logical::Key { keycode: 30, pressed: true, modifiers: MOD_SHIFT, locks: LOCK_NUM, time_ns: T },
            "so a key typed now is still shifted"
        );

        group(&mut i, &[key(KEY_RIGHTSHIFT, false), syn()]);
        assert_eq!(i.modifiers(), 0, "and only when both are up does it clear");
    }

    #[test]
    fn the_sides_are_tracked_separately_across_every_modifier() {
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
            Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
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
        // Buttons share `EV_KEY` with keys on the wire.
        let mut i = Interpreter::new();
        let (out, _) = group(&mut i, &[key(BTN_MIDDLE, true), syn()]);
        assert!(matches!(out[0], Logical::Button { .. }));
    }

    #[test]
    fn a_dropped_marker_resets_state_and_is_reported_immediately() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_LEFTSHIFT, true), key(BTN_LEFT, true), syn()]);
        assert_eq!(i.modifiers(), MOD_SHIFT);
        assert_eq!(i.buttons(), 1);

        let mut out = [Logical::Key { keycode: 0, pressed: false, modifiers: 0, locks: 0, time_ns: T }; MAX_PER_GROUP];
        assert_eq!(i.feed(dropped(), &mut out), 1);
        assert_eq!(out[0], Logical::Dropped { time_ns: T });
        assert_eq!(i.modifiers(), 0, "held keys across a gap are a guess");
        assert_eq!(i.buttons(), 0);
    }

    /// Five transitions, a motion and a wheel: sized short, the accumulated events fall off.
    #[test]
    fn a_full_group_plus_motion_and_a_wheel_needs_the_whole_max_per_group() {
        const GROUP: [fn() -> InputEvent; 7] = [
            || key(BTN_LEFT, true),
            || key(BTN_RIGHT, true),
            || key(BTN_MIDDLE, true),
            || key(KEY_LEFTSHIFT, true),
            || key(KEY_LEFTCTRL, true),
            || rel(REL_X, 7),
            || rel(REL_WHEEL, -2),
        ];
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        for e in GROUP {
            i.feed(e(), &mut out);
        }
        let n = i.feed(syn(), &mut out);
        assert_eq!(n, MAX_PER_GROUP, "five transitions, the motion and the wheel");
        assert_eq!(
            out[MAX_PER_GROUP - 2],
            Logical::Motion { dx: 7, dy: 0, buttons: 7, modifiers: MOD_SHIFT | MOD_CTRL, time_ns: T },
            "and it carries the state the same group established"
        );
        assert_eq!(
            out[MAX_PER_GROUP - 1],
            Logical::Wheel { dz: -2, buttons: 7, modifiers: MOD_SHIFT | MOD_CTRL, time_ns: T },
            "the wheel carries it too — Ctrl-scroll is a gesture, and it is one event"
        );

        let mut small = [Logical::Dropped { time_ns: T }; MAX_LOGICAL];
        for e in GROUP {
            i.feed(e(), &mut small);
        }
        let n = i.feed(syn(), &mut small);
        assert_eq!(n, MAX_LOGICAL, "and two short, the accumulated events fall off the end");
        assert!(!small.iter().any(|l| matches!(l, Logical::Motion { .. } | Logical::Wheel { .. })));
    }

    /// A wheel is its own event, not a third axis on the motion, even in one group.
    #[test]
    fn a_wheel_is_a_separate_event_from_the_motion_in_the_same_group() {
        let mut i = Interpreter::new();
        let (out, n) = group(&mut i, &[rel(REL_X, 2), rel(REL_WHEEL, 1), syn()]);
        assert_eq!(n, 2);
        assert_eq!(out[0], Logical::Motion { dx: 2, dy: 0, buttons: 0, modifiers: 0, time_ns: T });
        assert_eq!(out[1], Logical::Wheel { dz: 1, buttons: 0, modifiers: 0, time_ns: T });
    }

    #[test]
    fn wheel_detents_accumulate_within_a_group_and_a_still_wheel_is_not_an_event() {
        let mut i = Interpreter::new();
        let (out, n) = group(&mut i, &[rel(REL_WHEEL, 1), rel(REL_WHEEL, 2), syn()]);
        assert_eq!(n, 1);
        assert_eq!(out[0], Logical::Wheel { dz: 3, buttons: 0, modifiers: 0, time_ns: T });

        let (_, n) = group(&mut i, &[rel(REL_WHEEL, 0), syn()]);
        assert_eq!(n, 0, "a zero delta is not an event, as for motion");

        let (_, n) = group(&mut i, &[key(30, true), syn()]);
        assert_eq!(n, 1, "the key alone");
    }

    #[test]
    fn a_drop_discards_a_wheel_the_group_had_accumulated() {
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        i.feed(rel(REL_WHEEL, 4), &mut out); // no SYN yet
        i.feed(dropped(), &mut out);
        let (_, n) = group(&mut i, &[syn()]);
        assert_eq!(n, 0, "the interrupted group's detents did not survive the gap");
    }

    #[test]
    fn a_partial_group_interrupted_by_a_drop_does_not_leak_into_the_next() {
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        i.feed(key(30, true), &mut out); // no SYN yet
        i.feed(dropped(), &mut out);
        let (out2, n) = group(&mut i, &[key(31, true), syn()]);
        assert_eq!(n, 1, "only the new group's key");
        assert_eq!(out2[0], Logical::Key { keycode: 31, pressed: true, modifiers: 0, locks: LOCK_NUM, time_ns: T });
    }

    #[test]
    fn a_group_with_no_motion_emits_no_motion_event() {
        let mut i = Interpreter::new();
        let (_, n) = group(&mut i, &[rel(REL_X, 0), rel(REL_Y, 0), syn()]);
        assert_eq!(n, 0, "a zero delta is not an event");
    }

    /// The times differ on purpose: the helpers' one constant cannot tell an interpreter that
    /// stamps everything with the `SYN`'s time from a correct one.
    #[test]
    fn the_interrupts_own_time_reaches_each_event() {
        let at = |mut e: InputEvent, t: u64| {
            e.time_ns = t;
            e
        };
        let mut i = Interpreter::new();
        let mut out = [Logical::Dropped { time_ns: 0 }; MAX_PER_GROUP];
        i.feed(at(key(BTN_LEFT, true), 100), &mut out);
        i.feed(at(rel(REL_X, 3), 200), &mut out);
        let n = i.feed(at(syn(), 300), &mut out);
        assert_eq!(n, 2);
        assert_eq!(
            out[0],
            Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: 0, time_ns: 100 },
            "the button kept the moment it was pressed, not the moment the group closed"
        );
        assert_eq!(
            out[1],
            Logical::Motion { dx: 3, dy: 0, buttons: 1, modifiers: 0, time_ns: 300 },
            "the motion is the group's, so it takes the group's time"
        );

        assert_eq!(i.feed(at(dropped(), 400), &mut out), 1);
        assert_eq!(out[0], Logical::Dropped { time_ns: 400 });
    }

    /// A PS/2 typematic repeat is press, press, release, with no release between.
    #[test]
    fn a_held_lock_key_toggles_its_lock_once() {
        let mut i = Interpreter::new();
        assert_eq!(i.locks(), LOCK_NUM, "Num Lock on from the start");
        let (out, n) = group(&mut i, &[key(KEY_CAPSLOCK, true), syn()]);
        assert_eq!(n, 1);
        assert!(matches!(out[0], Logical::Key { keycode: KEY_CAPSLOCK, locks, .. } if locks == LOCK_NUM | LOCK_CAPS));
        // Checked after each repeat, since two toggles cancel.
        group(&mut i, &[key(KEY_CAPSLOCK, true), syn()]);
        assert_eq!(i.locks(), LOCK_NUM | LOCK_CAPS, "a repeat toggled nothing");
        group(&mut i, &[key(KEY_CAPSLOCK, true), syn()]);
        assert_eq!(i.locks(), LOCK_NUM | LOCK_CAPS, "nor did a second");
        group(&mut i, &[key(KEY_CAPSLOCK, false), syn()]);
        group(&mut i, &[key(KEY_CAPSLOCK, true), syn(), key(KEY_CAPSLOCK, false), syn()]);
        assert_eq!(i.locks(), LOCK_NUM, "pressed again after its release, it toggles back");
        group(&mut i, &[key(KEY_NUMLOCK, true), syn(), key(KEY_NUMLOCK, false), syn()]);
        assert_eq!(i.locks(), 0);
    }

    /// A lock key held across the gap can toggle again on its next press.
    #[test]
    fn a_drop_keeps_the_locks() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_CAPSLOCK, true), syn()]);
        let mut out = [Logical::Dropped { time_ns: T }; MAX_PER_GROUP];
        i.feed(dropped(), &mut out);
        assert_eq!(i.locks(), LOCK_NUM | LOCK_CAPS, "both locks survive");
        group(&mut i, &[key(KEY_CAPSLOCK, true), syn()]);
        assert_eq!(i.locks(), LOCK_NUM, "a press after the gap toggles");
    }

    /// The bits follow HID's LED order, which the kernel's drivers read.
    #[test]
    fn the_locks_light_their_own_lights() {
        assert_eq!(lights(0), 0);
        assert_eq!(lights(LOCK_NUM), 0x01);
        assert_eq!(lights(LOCK_CAPS), 0x02);
        assert_eq!(lights(LOCK_NUM | LOCK_CAPS), 0x03);
        assert_eq!(lights(Interpreter::new().locks()), 0x01, "Num Lock's light is on from the start");
    }

    fn keycodes(i: &mut Interpreter, events: &[InputEvent]) -> Vec<(u16, bool)> {
        let (out, n) = group(i, events);
        out[..n]
            .iter()
            .map(|l| match *l {
                Logical::Key { keycode, pressed, .. } => (keycode, pressed),
                other => panic!("not a key: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_keypad_is_navigation_with_num_lock_off() {
        let mut i = Interpreter::new();
        assert_eq!(keycodes(&mut i, &[key(75, true), syn()]), vec![(75, true)], "keypad 4, Num Lock on");
        assert_eq!(keycodes(&mut i, &[key(75, false), syn()]), vec![(75, false)]);
        group(&mut i, &[key(KEY_NUMLOCK, true), syn(), key(KEY_NUMLOCK, false), syn()]);
        assert_eq!(keycodes(&mut i, &[key(75, true), syn()]), vec![(KEY_LEFT, true)], "keypad 4 is Left");
        assert_eq!(keycodes(&mut i, &[key(75, false), syn()]), vec![(KEY_LEFT, false)]);
        for (kp, nav) in KEYPAD_NAV {
            assert_eq!(keycodes(&mut i, &[key(kp, true), syn()]), vec![(nav, true)], "keypad {kp}");
            keycodes(&mut i, &[key(kp, false), syn()]);
        }
        assert_eq!(group(&mut i, &[key(KEY_KP5, true), syn()]).1, 0, "keypad 5 is nothing");
        assert_eq!(group(&mut i, &[key(KEY_KP5, false), syn()]).1, 0);
        assert_eq!(keycodes(&mut i, &[key(KEY_KPENTER, true), syn()]), vec![(KEY_ENTER, true)]);
        group(&mut i, &[key(KEY_NUMLOCK, true), syn(), key(KEY_NUMLOCK, false), syn()]);
        assert_eq!(keycodes(&mut i, &[key(KEY_KPENTER, false), syn()]), vec![(KEY_ENTER, false)], "either way");
    }

    /// Keypad 4 pressed as Left releases as Left after Num Lock came on in between.
    #[test]
    fn a_keypad_key_keeps_its_first_press_meaning() {
        let mut i = Interpreter::new();
        group(&mut i, &[key(KEY_NUMLOCK, true), syn(), key(KEY_NUMLOCK, false), syn()]);
        assert_eq!(keycodes(&mut i, &[key(75, true), syn()]), vec![(KEY_LEFT, true)]);
        group(&mut i, &[key(KEY_NUMLOCK, true), syn(), key(KEY_NUMLOCK, false), syn()]);
        assert_eq!(i.locks(), LOCK_NUM);
        assert_eq!(keycodes(&mut i, &[key(75, true), syn()]), vec![(KEY_LEFT, true)], "a repeat");
        assert_eq!(keycodes(&mut i, &[key(75, false), syn()]), vec![(KEY_LEFT, false)], "its release");
        assert_eq!(keycodes(&mut i, &[key(75, true), syn()]), vec![(75, true)], "a new press, Num Lock on");
    }
}
