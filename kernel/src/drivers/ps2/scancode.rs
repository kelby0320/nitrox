//! AT scancode **set 1** → keycode.
//!
//! `display-substrate.md` §5 puts this table in the kernel; keycode→character lives in userspace.
//!
//! For `0x01..=0x53` **keycode == scancode** (Linux derived its keycodes from set 1), so the base
//! table is the identity, not 83 chances to typo a value correct by construction. Only the
//! seventeen `E0` keys need a mapping. The sequences that are not one byte:
//!
//! - **`E0` prefix** — the extended set: right-hand modifiers, arrows, Meta keys, keypad Enter and
//!   slash.
//! - **`E0 2A` / `E0 B7`** — *fake shifts*, with which the controller brackets Print Screen (and
//!   the arrows, with Num Lock on); emitting them leaves a phantom held Shift, so they are
//!   swallowed.
//! - **`E1 1D 45 E1 9D C5`** — Pause/Break, six bytes and no release; consumed whole.

use crate::libkern::input::*;

/// What one scancode byte produced.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decoded {
    /// Consumed, nothing to emit: a prefix byte, a fake shift, or part of the Pause sequence.
    Consumed,
    /// A key transition.
    Key {
        /// The keycode (an `EV_KEY` code).
        code: u16,
        /// True for a press, false for a release.
        pressed: bool,
    },
}

/// The `E0`-prefixed keys, as `(scancode after E0, keycode)`; an unlisted one is silence.
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

const BASE_MAX: u8 = 0x53;
const RELEASE_BIT: u8 = 0x80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Base,
    Extended,
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
                // Pause sends `E1 1D 45 E1 9D C5` and no release; swallow the tail whole.
                self.state = if remaining <= 1 { State::Base } else { State::Pause(remaining - 1) };
                Decoded::Consumed
            }
            State::Extended => {
                self.state = State::Base;
                let pressed = byte & RELEASE_BIT == 0;
                let make = byte & !RELEASE_BIT;
                // Fake shifts bracket Print Screen and the Num-Lock arrow island; emitting them
                // leaves a Shift held. **Redundant today, deliberately kept**: neither code is in
                // `E0_MAP`, so the fallthrough would swallow them anyway, but the obvious "fix" for
                // a missing keycode is to add an entry; `fake_shift_codes_are_absent_from_the_map`
                // has the teeth.
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
                // **Controller chatter is not special-cased here, deliberately.** `0xFA` (ack),
                // `0xFE` (resend) and `0xFF` fall outside the base range and become silence below,
                // but `0xAA` is `0x2A | 0x80`, an ordinary Left Shift **release**, and also the
                // controller's self-test-passed byte; the two are indistinguishable. The driver
                // drains controller responses during bring-up, before arming the IRQ, so every byte
                // reaching this decoder *is* a scancode; treating `0xAA` as chatter would swallow
                // every Left Shift release.
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
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x1D), Decoded::Key { code: KEY_LEFTCTRL, pressed: true });
        d.feed(0xE0);
        assert_eq!(d.feed(0x1D), Decoded::Key { code: KEY_RIGHTCTRL, pressed: true });
    }

    #[test]
    fn fake_shift_codes_are_absent_from_the_map() {
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
        let mut d = Decoder::new();
        d.feed(0xE0);
        assert_eq!(d.feed(0x2A), Decoded::Consumed, "fake shift press");
        d.feed(0xE0);
        assert_eq!(d.feed(0xB7), Decoded::Consumed, "fake shift release (0x36 | 0x80)");
    }

    #[test]
    fn a_real_shift_is_not_swallowed_by_the_fake_shift_rule() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(0x2A), Decoded::Key { code: KEY_LEFTSHIFT, pressed: true });
        assert_eq!(d.feed(0x36), Decoded::Key { code: KEY_RIGHTSHIFT, pressed: true });
    }

    #[test]
    fn pause_is_consumed_whole_and_leaves_the_decoder_usable() {
        let mut d = Decoder::new();
        for b in [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5] {
            assert_eq!(d.feed(b), Decoded::Consumed, "byte {b:#04x} of the Pause sequence");
        }
        assert_eq!(d.feed(0x1E), Decoded::Key { code: 0x1E, pressed: true }, "decoder recovered");
    }

    #[test]
    fn aa_is_a_shift_release_not_a_self_test_result() {
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
        d.feed(0xE0);
        assert_eq!(d.feed(0x7F), Decoded::Consumed);
        assert_eq!(press(&mut d, 0x1E), Decoded::Key { code: 0x1E, pressed: true });
    }

    #[test]
    fn every_e0_entry_is_reachable_and_distinct() {
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
