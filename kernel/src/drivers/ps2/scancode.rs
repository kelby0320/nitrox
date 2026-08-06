//! AT scancode **set 1** → keycode.
//!
//! `display-substrate.md` §5 puts this in the kernel: "one small table that every consumer
//! would otherwise duplicate, and getting it wrong is not a policy disagreement, it is a
//! bug". Keycode→*character* is the opposite case and lives in userspace, where a layout is
//! data rather than a rebuild.
//!
//! ## Why the base table is the identity
//!
//! Linux derived its keycodes from set 1, so for `0x01..=0x53` **keycode == scancode**. That
//! is not a coincidence worth hiding behind a 83-entry array: an array would be 83 chances to
//! typo a value that is already correct by construction, and every one of those typos is a
//! key that silently does the wrong thing. Only the `E0`-prefixed keys need a real mapping,
//! and there are seventeen of them.
//!
//! ## The three sequences that are not one byte
//!
//! - **`E0` prefix** — the extended set: right-hand modifiers, the arrow island, the Meta
//!   keys, keypad Enter and slash.
//! - **`E0 2A` / `E0 B7`** — *fake shifts*. The controller brackets Print Screen (and the
//!   arrow keys, when Num Lock is on) with synthetic shift press/release so that DOS-era
//!   software saw a consistent shift state. Emitting them produces a phantom held Shift, so
//!   they are swallowed.
//! - **`E1 1D 45 E1 9D C5`** — Pause/Break, the only six-byte sequence, and the only key that
//!   sends no release. Consumed and dropped: nothing needs it yet, and half-decoding it
//!   would leave the decoder mid-sequence on the next key.

use crate::libkern::input::*;

/// What one scancode byte produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// Consumed, nothing to emit: a prefix byte, a fake shift, or part of the Pause
    /// sequence.
    Consumed,
    /// A key transition.
    Key {
        /// The keycode (an `EV_KEY` code).
        code: u16,
        /// True for a press, false for a release.
        pressed: bool,
    },
}

/// The `E0`-prefixed keys, as `(scancode after E0, keycode)`.
///
/// Everything not listed is unknown and produces [`Decoded::Consumed`] — an unmapped key is
/// silence, never a wrong keycode.
const E0_MAP: &[(u8, u16)] = &[
    (0x1C, KEY_KPENTER),
    (0x1D, KEY_RIGHTCTRL),
    (0x35, KEY_KPSLASH),
    (0x38, KEY_RIGHTALT),
    (0x47, KEY_HOME),
    (0x48, KEY_UP),
    (0x49, KEY_PAGEUP),
    (0x4B, KEY_LEFT),
    (0x4D, KEY_RIGHT),
    (0x4F, KEY_END),
    (0x50, KEY_DOWN),
    (0x51, KEY_PAGEDOWN),
    (0x52, KEY_INSERT),
    (0x53, KEY_DELETE),
    (0x5B, KEY_LEFTMETA),
    (0x5C, KEY_RIGHTMETA),
    (0x5D, KEY_COMPOSE),
];

/// Highest scancode in the identity range (keypad `.`).
const BASE_MAX: u8 = 0x53;
/// Set-1 marks a release by setting the high bit of the make code.
const RELEASE_BIT: u8 = 0x80;

/// Where the decoder is in a multi-byte sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Between sequences.
    Base,
    /// `E0` seen; the next byte is an extended code.
    Extended,
    /// Inside `E1 …` (Pause). Counts down the bytes still to swallow.
    Pause(u8),
}

/// The set-1 decoder. One per keyboard port; feed it every byte from the data port.
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    state: State,
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    /// A decoder between sequences.
    pub const fn new() -> Self {
        Self { state: State::Base }
    }

    /// Feed one byte from the keyboard's data port.
    pub fn feed(&mut self, byte: u8) -> Decoded {
        match self.state {
            State::Pause(remaining) => {
                // Pause sends `E1 1D 45 E1 9D C5` and no release. Swallow the tail whole:
                // decoding half of it would leave the *next* real key interpreted as a
                // continuation.
                self.state = if remaining <= 1 { State::Base } else { State::Pause(remaining - 1) };
                Decoded::Consumed
            }
            State::Extended => {
                self.state = State::Base;
                let pressed = byte & RELEASE_BIT == 0;
                let make = byte & !RELEASE_BIT;
                // Fake shifts bracketing Print Screen and the Num-Lock arrow island.
                // Emitting them leaves a Shift that userspace believes is held.
                //
                // **Currently redundant, deliberately kept.** Neither code is in `E0_MAP`,
                // so the unmapped-code fallthrough below would swallow them anyway — a
                // break-test proved this check is doing no work today. It stays because the
                // *reason* they must not be emitted is invisible from the map's absence: the
                // obvious "fix" for a missing keycode is to add an entry, and `E0 2A` looks
                // exactly like a shift that someone forgot. `fake_shift_codes_are_absent_
                // from_the_map` is the test with teeth; this is the belt.
                if make == 0x2A || make == 0x36 {
                    return Decoded::Consumed;
                }
                match E0_MAP.iter().find(|(sc, _)| *sc == make) {
                    Some(&(_, code)) => Decoded::Key { code, pressed },
                    None => Decoded::Consumed,
                }
            }
            State::Base => match byte {
                0xE0 => {
                    self.state = State::Extended;
                    Decoded::Consumed
                }
                0xE1 => {
                    self.state = State::Pause(5);
                    Decoded::Consumed
                }
                // **Controller chatter is not special-cased here, deliberately.** `0xFA`
                // (ack), `0xFE` (resend) and `0xFF` fall outside the base range and become
                // silence below — but `0xAA` does *not*: it is `0x2A | 0x80`, an ordinary
                // Left Shift **release**, and it is also the byte the controller sends to
                // report a passed self-test. The two are indistinguishable in the stream.
                //
                // The disambiguation is temporal, not syntactic: the driver drains
                // controller responses synchronously during bring-up, before arming the
                // IRQ, so by the time a byte reaches this decoder every byte *is* a
                // scancode. Treating `0xAA` as chatter here would instead swallow every
                // Left Shift release and leave userspace holding a Shift forever.
                _ => {
                    let pressed = byte & RELEASE_BIT == 0;
                    let make = byte & !RELEASE_BIT;
                    if (KEY_ESC as u8..=BASE_MAX).contains(&make) {
                        Decoded::Key { code: make as u16, pressed }
                    } else {
                        Decoded::Consumed
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(d: &mut Decoder, b: u8) -> Decoded {
        d.feed(b)
    }

    #[test]
    fn the_base_range_is_the_identity() {
        // The whole justification for not writing an 83-entry table. If this ever fails,
        // the table has to come back.
        let mut d = Decoder::new();
        for sc in 0x01u8..=0x53 {
            assert_eq!(
                d.feed(sc),
                Decoded::Key { code: sc as u16, pressed: true },
                "scancode {sc:#04x} must map to keycode {sc}"
            );
        }
    }

    #[test]
    fn named_keys_land_where_the_abi_says() {
        // Spot-checks against the constants, so a wrong constant fails here and not in a
        // guest three parts later.
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x01), Decoded::Key { code: KEY_ESC, pressed: true });
        assert_eq!(d.feed(0x1C), Decoded::Key { code: KEY_ENTER, pressed: true });
        assert_eq!(d.feed(0x2A), Decoded::Key { code: KEY_LEFTSHIFT, pressed: true });
        assert_eq!(d.feed(0x39), Decoded::Key { code: KEY_SPACE, pressed: true });
        assert_eq!(d.feed(0x3B), Decoded::Key { code: KEY_F1, pressed: true });
        assert_eq!(d.feed(0x53), Decoded::Key { code: KEY_KPDOT, pressed: true });
    }

    #[test]
    fn the_high_bit_is_release() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x1E), Decoded::Key { code: 0x1E, pressed: true });
        assert_eq!(d.feed(0x9E), Decoded::Key { code: 0x1E, pressed: false });
    }

    #[test]
    fn e0_maps_the_extended_keys_and_releases_them() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0xE0), Decoded::Consumed, "the prefix emits nothing by itself");
        assert_eq!(d.feed(0x48), Decoded::Key { code: KEY_UP, pressed: true });
        assert_eq!(d.feed(0xE0), Decoded::Consumed);
        assert_eq!(d.feed(0xC8), Decoded::Key { code: KEY_UP, pressed: false });
    }

    #[test]
    fn e0_1d_is_right_ctrl_not_left() {
        // `0x1D` bare is Left Ctrl and `E0 1D` is Right Ctrl. Losing the prefix state
        // between the two bytes silently merges the modifiers.
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x1D), Decoded::Key { code: KEY_LEFTCTRL, pressed: true });
        d.feed(0xE0);
        assert_eq!(d.feed(0x1D), Decoded::Key { code: KEY_RIGHTCTRL, pressed: true });
    }

    #[test]
    fn fake_shift_codes_are_absent_from_the_map() {
        // The invariant with teeth. `fake_shifts_are_swallowed` below asserts the behaviour
        // but is over-determined: removing the explicit swallow leaves it passing, because
        // an unmapped extended code is `Consumed` regardless. What actually breaks the
        // behaviour is somebody *adding* `E0 2A` to the map — which looks like fixing a
        // missing Shift and is the real hazard — and only this test catches that.
        for &(sc, _) in E0_MAP {
            assert!(
                sc != 0x2A && sc != 0x36,
                "{sc:#04x} is a synthetic shift the controller brackets Print Screen with; \
                 mapping it emits a Shift the user never pressed"
            );
        }
    }

    #[test]
    fn fake_shifts_are_swallowed() {
        // Print Screen arrives as `E0 2A E0 37`. Emitting the bracketing shift leaves
        // userspace believing Shift is held for the rest of the session.
        let mut d = Decoder::new();
        d.feed(0xE0);
        assert_eq!(d.feed(0x2A), Decoded::Consumed, "fake shift press");
        d.feed(0xE0);
        assert_eq!(d.feed(0xB7), Decoded::Consumed, "fake shift release (0x36 | 0x80)");
    }

    #[test]
    fn a_real_shift_is_not_swallowed_by_the_fake_shift_rule() {
        // The rule keys on the `E0` prefix. Bare `0x2A` must still be Left Shift.
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x2A), Decoded::Key { code: KEY_LEFTSHIFT, pressed: true });
        assert_eq!(d.feed(0x36), Decoded::Key { code: KEY_RIGHTSHIFT, pressed: true });
    }

    #[test]
    fn pause_is_consumed_whole_and_leaves_the_decoder_usable() {
        // The failure this guards is not "Pause does nothing" — it is the *next* key being
        // eaten or misread because the decoder was left mid-sequence.
        let mut d = Decoder::new();
        for b in [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5] {
            assert_eq!(d.feed(b), Decoded::Consumed, "byte {b:#04x} of the Pause sequence");
        }
        assert_eq!(d.feed(0x1E), Decoded::Key { code: 0x1E, pressed: true }, "decoder recovered");
    }

    #[test]
    fn aa_is_a_shift_release_not_a_self_test_result() {
        // The one genuine collision in set 1: `0xAA` is both "self-test passed" and
        // `0x2A | 0x80`, Left Shift up. Swallowing it as chatter would leave userspace
        // believing Shift is held for the rest of the session — and this test was
        // originally written asserting the wrong side of it.
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x2A), Decoded::Key { code: KEY_LEFTSHIFT, pressed: true });
        assert_eq!(
            d.feed(0xAA),
            Decoded::Key { code: KEY_LEFTSHIFT, pressed: false },
            "the driver drains controller responses before arming the IRQ, so by here \
             every byte is a scancode"
        );
    }

    #[test]
    fn controller_chatter_and_unknown_codes_are_silence_not_wrong_keys() {
        let mut d = Decoder::new();
        for b in [0x00u8, 0xFA, 0xFE, 0xFF] {
            assert_eq!(d.feed(b), Decoded::Consumed, "{b:#04x} is not a key");
        }
        // An unmapped extended code: silence, never a guess.
        d.feed(0xE0);
        assert_eq!(d.feed(0x7F), Decoded::Consumed);
        assert_eq!(press(&mut d, 0x1E), Decoded::Key { code: 0x1E, pressed: true });
    }

    #[test]
    fn every_e0_entry_is_reachable_and_distinct() {
        // Guards a copy-paste duplicate in the table, which would make one key unreachable
        // and another fire twice.
        for (i, &(sc, code)) in E0_MAP.iter().enumerate() {
            assert!(code != 0, "keycode 0 means 'no key'");
            for &(sc2, code2) in &E0_MAP[i + 1..] {
                assert_ne!(sc, sc2, "duplicate scancode {sc:#04x}");
                assert_ne!(code, code2, "duplicate keycode {code}");
            }
            let mut d = Decoder::new();
            d.feed(0xE0);
            assert_eq!(d.feed(sc), Decoded::Key { code, pressed: true });
        }
    }
}
