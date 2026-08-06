//! The input event ABI — one record, from the driver to the compositor unchanged.
//!
//! `docs/design/input-subsystem.md` §3. A single `#[repr(C)]` record travels from a Tier 1
//! driver, through the raw char node, through the userspace `input-server`, to the
//! compositor. There is no translation step and no second format.
//!
//! **Extensibility lives in the enums, not the layout.** A touchscreen is [`EV_ABS`] codes
//! that do not exist yet; a dial is an [`EV_REL`] code; a device class nobody has thought of
//! is a `kind` value. [`InputEvent`] itself never changes, so no future device breaks the
//! wire — which is exactly the corner a "fat" record with named `x`/`y`/`modifiers` fields
//! paints you into at the *second* device class.
//!
//! The shape and the numbering are deliberately Linux's `evdev`. Twenty-five years across
//! mice, tablets, touchscreens and accelerometers without a layout break is the strongest
//! evidence available, and matching the numbering means existing keycode knowledge
//! transfers.
//!
//! ## Grouping, and loss
//!
//! A logical event is a **group terminated by [`EV_SYN`]/[`SYN_REPORT`]**: a diagonal mouse
//! move is `REL_X`, `REL_Y`, `SYN`. Consumers accumulate until the `SYN` rather than acting
//! on each record.
//!
//! When events are lost, [`SYN_DROPPED`] is delivered and the consumer must **discard its
//! accumulated state and resynchronise** from the next `SYN_REPORT`. This is not optional
//! politeness: after a loss the consumer's idea of which keys are held and where the pointer
//! is are both stale, and silently carrying on is how a phantom held modifier survives for
//! the rest of a session.

use core::mem::{align_of, offset_of, size_of};

/// One input event: what happened, on what, when.
///
/// `#[repr(C)]`, 16 bytes, and mirrored byte-for-byte in `userspace/libkern/src/abi.rs`.
/// Both sides carry the layout asserts below.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct InputEvent {
    /// Event class: [`EV_SYN`], [`EV_KEY`], [`EV_REL`], [`EV_ABS`].
    pub kind: u16,
    /// Code within the class — a keycode, [`REL_X`], [`BTN_LEFT`].
    pub code: u16,
    /// [`KEY_RELEASE`]/[`KEY_PRESS`]/[`KEY_REPEAT`] for [`EV_KEY`]; a signed delta for
    /// [`EV_REL`]; an absolute position for [`EV_ABS`].
    pub value: i32,
    /// Kernel monotonic time **at the interrupt**, not at delivery.
    ///
    /// Load-bearing for merging, not a convenience for double-click. The `input-server`
    /// reads several device nodes over *independent* `sys_io_submit` round trips, so without
    /// an interrupt-time stamp the only order available to it is the order its own reads
    /// happened to complete in — scheduling noise, which would put a keystroke before or
    /// after a click depending on when the server was descheduled. Stamping here is the only
    /// place the true order still exists.
    pub time_ns: u64,
}

const _: () = assert!(size_of::<InputEvent>() == 16);
const _: () = assert!(align_of::<InputEvent>() == 8);
const _: () = assert!(offset_of!(InputEvent, kind) == 0);
const _: () = assert!(offset_of!(InputEvent, code) == 2);
const _: () = assert!(offset_of!(InputEvent, value) == 4);
const _: () = assert!(offset_of!(InputEvent, time_ns) == 8);

/// Bytes one [`InputEvent`] occupies on the wire. Reads of a raw input node are a whole
/// number of these — see [`crate::drivers::ps2`]'s ring, which drops whole records.
pub const INPUT_EVENT_LEN: usize = size_of::<InputEvent>();

// ---------------------------------------------------------------------------
// Event classes (`kind`). evdev numbering.
// ---------------------------------------------------------------------------

/// Group separator; see [`SYN_REPORT`] and [`SYN_DROPPED`].
pub const EV_SYN: u16 = 0x00;
/// A key or button changed state. `code` is a keycode or a `BTN_*`.
pub const EV_KEY: u16 = 0x01;
/// A relative axis moved. `code` is a `REL_*`; `value` is a signed delta.
pub const EV_REL: u16 = 0x02;
/// An absolute axis was reported. `code` is an `ABS_*`; `value` is device-space.
///
/// Nothing emits this yet — no absolute device exists. Reserved so a touchscreen needs no
/// wire change (`deferred-decisions.md`, input).
pub const EV_ABS: u16 = 0x03;

// ---------------------------------------------------------------------------
// `EV_SYN` codes.
// ---------------------------------------------------------------------------

/// End of a logical event group. Everything since the previous `SYN_REPORT` happened
/// together.
pub const SYN_REPORT: u16 = 0;
/// **Events were lost.** Discard accumulated state and resynchronise from the next
/// [`SYN_REPORT`]: held-key and pointer state are both stale across a drop.
pub const SYN_DROPPED: u16 = 3;

// ---------------------------------------------------------------------------
// `EV_KEY` values.
// ---------------------------------------------------------------------------

/// The key came up.
pub const KEY_RELEASE: i32 = 0;
/// The key went down.
pub const KEY_PRESS: i32 = 1;
/// Autorepeat while held. Nothing emits this yet — key repeat is deferred to `libinput`
/// (`deferred-decisions.md`, input) — but the value is reserved so it needs no wire change.
pub const KEY_REPEAT: i32 = 2;

// ---------------------------------------------------------------------------
// `EV_REL` codes.
// ---------------------------------------------------------------------------

/// Horizontal motion; positive is right.
pub const REL_X: u16 = 0x00;
/// Vertical motion; positive is **down**, matching screen coordinates rather than the
/// PS/2 wire, which reports positive as up. The driver negates.
pub const REL_Y: u16 = 0x01;
/// Wheel detents; positive is away from the user.
pub const REL_WHEEL: u16 = 0x08;

// ---------------------------------------------------------------------------
// Keycodes — `EV_KEY` codes for keyboard keys.
//
// These are Linux keycodes, and for `0x01..=0x53` they are numerically identical to AT
// scancode set 1, because Linux derived them from it. `scancode.rs` relies on that: the base
// table is the identity, and only the `E0`-prefixed keys need a mapping.
// ---------------------------------------------------------------------------

/// Escape. Also the lowest valid keycode: `0` is "no key".
pub const KEY_ESC: u16 = 1;
/// Backspace.
pub const KEY_BACKSPACE: u16 = 14;
/// Tab.
pub const KEY_TAB: u16 = 15;
/// Return/Enter on the main block.
pub const KEY_ENTER: u16 = 28;
/// Left Control.
pub const KEY_LEFTCTRL: u16 = 29;
/// Left Shift.
pub const KEY_LEFTSHIFT: u16 = 42;
/// Right Shift.
pub const KEY_RIGHTSHIFT: u16 = 54;
/// Left Alt.
pub const KEY_LEFTALT: u16 = 56;
/// Space.
pub const KEY_SPACE: u16 = 57;
/// Caps Lock.
pub const KEY_CAPSLOCK: u16 = 58;
/// F1.
pub const KEY_F1: u16 = 59;
/// Num Lock.
pub const KEY_NUMLOCK: u16 = 69;
/// Scroll Lock.
pub const KEY_SCROLLLOCK: u16 = 70;
/// Keypad `.`/Del — the highest key reachable without an `E0` prefix.
pub const KEY_KPDOT: u16 = 83;

/// Keypad Enter (`E0 1C`).
pub const KEY_KPENTER: u16 = 96;
/// Right Control (`E0 1D`).
pub const KEY_RIGHTCTRL: u16 = 97;
/// Keypad `/` (`E0 35`).
pub const KEY_KPSLASH: u16 = 98;
/// Right Alt / AltGr (`E0 38`).
pub const KEY_RIGHTALT: u16 = 100;
/// Home (`E0 47`).
pub const KEY_HOME: u16 = 102;
/// Cursor up (`E0 48`).
pub const KEY_UP: u16 = 103;
/// Page Up (`E0 49`).
pub const KEY_PAGEUP: u16 = 104;
/// Cursor left (`E0 4B`).
pub const KEY_LEFT: u16 = 105;
/// Cursor right (`E0 4D`).
pub const KEY_RIGHT: u16 = 106;
/// End (`E0 4F`).
pub const KEY_END: u16 = 107;
/// Cursor down (`E0 50`).
pub const KEY_DOWN: u16 = 108;
/// Page Down (`E0 51`).
pub const KEY_PAGEDOWN: u16 = 109;
/// Insert (`E0 52`).
pub const KEY_INSERT: u16 = 110;
/// Delete (`E0 53`).
pub const KEY_DELETE: u16 = 111;
/// Left Meta / "Super" (`E0 5B`) — the desktop shell's motivating hotkey
/// (`display-substrate.md` §5a).
pub const KEY_LEFTMETA: u16 = 125;
/// Right Meta (`E0 5C`).
pub const KEY_RIGHTMETA: u16 = 126;
/// Menu / Compose (`E0 5D`).
pub const KEY_COMPOSE: u16 = 127;

// ---------------------------------------------------------------------------
// Buttons — also `EV_KEY` codes, in evdev's `0x110` block. Buttons are keys: it is the
// same state machine (down, up, held), so giving them a separate class would duplicate it.
// ---------------------------------------------------------------------------

/// Primary button.
pub const BTN_LEFT: u16 = 0x110;
/// Secondary button.
pub const BTN_RIGHT: u16 = 0x111;
/// Wheel click.
pub const BTN_MIDDLE: u16 = 0x112;

impl InputEvent {
    /// A key or button transition.
    pub const fn key(code: u16, value: i32, time_ns: u64) -> Self {
        Self { kind: EV_KEY, code, value, time_ns }
    }

    /// A relative-axis motion.
    pub const fn rel(code: u16, delta: i32, time_ns: u64) -> Self {
        Self { kind: EV_REL, code, value: delta, time_ns }
    }

    /// The end of a logical group.
    pub const fn syn(time_ns: u64) -> Self {
        Self { kind: EV_SYN, code: SYN_REPORT, value: 0, time_ns }
    }

    /// The loss marker. `value` carries how many whole records were discarded, which is
    /// diagnostic only — a consumer resynchronises identically whatever the count.
    pub const fn dropped(lost: i32, time_ns: u64) -> Self {
        Self { kind: EV_SYN, code: SYN_DROPPED, value: lost, time_ns }
    }

    /// Serialise into exactly [`INPUT_EVENT_LEN`] little-endian bytes.
    pub fn write(&self, out: &mut [u8; INPUT_EVENT_LEN]) {
        out[0..2].copy_from_slice(&self.kind.to_le_bytes());
        out[2..4].copy_from_slice(&self.code.to_le_bytes());
        out[4..8].copy_from_slice(&self.value.to_le_bytes());
        out[8..16].copy_from_slice(&self.time_ns.to_le_bytes());
    }

    /// Parse from exactly [`INPUT_EVENT_LEN`] little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < INPUT_EVENT_LEN {
            return None;
        }
        Some(Self {
            kind: u16::from_le_bytes([b[0], b[1]]),
            code: u16::from_le_bytes([b[2], b[3]]),
            value: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            time_ns: u64::from_le_bytes([b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_sits_at_the_offsets_the_spec_publishes() {
        // A round trip cannot see a swapped pair — `write` and `read` agree with each other
        // whatever they agree on. This is a kernel↔userspace ABI with a published byte
        // table, so the bytes are pinned, not merely their symmetry. (The same gap cost a
        // finding on `WindowInfo` in PR #175.)
        let e = InputEvent { kind: 0x1112, code: 0x2122, value: -2, time_ns: 0x8182_8384_8586_8788 };
        let mut b = [0u8; INPUT_EVENT_LEN];
        e.write(&mut b);
        assert_eq!(&b[0..2], &0x1112u16.to_le_bytes(), "kind @0");
        assert_eq!(&b[2..4], &0x2122u16.to_le_bytes(), "code @2");
        assert_eq!(&b[4..8], &(-2i32).to_le_bytes(), "value @4, signed");
        assert_eq!(&b[8..16], &0x8182_8384_8586_8788u64.to_le_bytes(), "time_ns @8");
    }

    #[test]
    fn an_event_round_trips() {
        let e = InputEvent::rel(REL_Y, -7, 12_345);
        let mut b = [0u8; INPUT_EVENT_LEN];
        e.write(&mut b);
        assert_eq!(InputEvent::read(&b), Some(e));
    }

    #[test]
    fn a_truncated_event_is_refused_rather_than_read_short() {
        let b = [0u8; INPUT_EVENT_LEN - 1];
        assert_eq!(InputEvent::read(&b), None);
    }

    #[test]
    fn the_constructors_agree_with_the_classes() {
        assert_eq!(InputEvent::key(KEY_ESC, KEY_PRESS, 1).kind, EV_KEY);
        assert_eq!(InputEvent::rel(REL_X, 3, 1).kind, EV_REL);
        assert_eq!(InputEvent::syn(1).kind, EV_SYN);
        assert_eq!(InputEvent::syn(1).code, SYN_REPORT);
        assert_eq!(InputEvent::dropped(4, 1).code, SYN_DROPPED);
        assert_eq!(InputEvent::dropped(4, 1).value, 4, "the count is diagnostic but carried");
    }

    #[test]
    fn buttons_are_keys_not_a_separate_class() {
        // Deliberate: a button has the same down/up/held state machine as a key, so it
        // shares `EV_KEY` and a consumer needs one code path, not two.
        assert_eq!(InputEvent::key(BTN_LEFT, KEY_PRESS, 1).kind, EV_KEY);
        assert!(BTN_LEFT > KEY_COMPOSE, "button codes must not collide with keycodes");
    }
}
