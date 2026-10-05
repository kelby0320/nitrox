//! **The keyboard's lights, set over the i8042** (Phase 6 Part B.5): `0xED`, then the mask, each
//! answered by `0xFA` — the exchange the PS/2 keyboard protocol defines for its LEDs.
//!
//! **The answers arrive through the same byte stream as keys**, and that is the whole difficulty.
//! While an exchange is in flight its answers are taken *ahead of* the scancode decoder: an
//! acknowledgement between an `E0` prefix and its code would otherwise be read as that code, so
//! `E0 FA 48` — Up, with an answer in the middle — would be keypad 8 pressed and never released
//! (PR #357 review, probed on the decoder). Outside an exchange the decoder already turns `0xFA` and
//! `0xFE` into silence, and still does.
//!
//! A pure state machine, so the exchange is host-tested byte by byte; the driver sends what it
//! says to send and completes the write when it says the exchange is done.

use crate::libkern::input::{LIGHT_CAPS, LIGHT_NUM, LIGHT_SCROLL};

/// The keyboard's command: set the LEDs to the byte that follows.
pub const SET_LEDS: u8 = 0xED;
/// The keyboard's acknowledgement.
pub const ACK: u8 = 0xFA;
/// The keyboard's request to send the last byte again.
pub const RESEND: u8 = 0xFE;
/// Resends taken before the exchange fails.
const RESENDS_MAX: u8 = 3;
/// How long the keyboard has to answer each byte before the exchange fails.
pub const ANSWER_NS: u64 = 100_000_000;

/// **The PS/2 order of the lights**: Scroll Lock in bit 0, Num Lock in bit 1, Caps Lock in bit 2 —
/// from HID's, which the node's write carries.
pub fn ps2_mask(lights: u8) -> u8 {
    let has = |l: u16| lights as u16 & l != 0;
    (has(LIGHT_SCROLL) as u8) | (has(LIGHT_NUM) as u8) << 1 | (has(LIGHT_CAPS) as u8) << 2
}

/// Where the exchange is.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Step {
    /// `0xED` sent; its acknowledgement awaited.
    Command,
    /// The mask sent; its acknowledgement awaited.
    Mask,
}

/// What the driver does next.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send this byte to the keyboard.
    Send(u8),
    /// The exchange is over: the keyboard took the lights, or it did not.
    Done(Result<(), Failure>),
}

/// Why an exchange failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// No answer within [`ANSWER_NS`].
    NoAnswer,
    /// Asked to resend more than [`RESENDS_MAX`] times.
    Resends,
}

/// **One exchange in flight.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Exchange {
    mask: u8,
    step: Step,
    resends: u8,
    /// When the byte now awaited was sent.
    sent_ns: u64,
}

impl Exchange {
    /// Begin setting the lights to `lights` (HID's order) at `now`: the exchange, and the byte to send
    /// first — `0xED`.
    pub fn start(lights: u8, now: u64) -> (Exchange, u8) {
        (Exchange { mask: ps2_mask(lights), step: Step::Command, resends: 0, sent_ns: now }, SET_LEDS)
    }

    /// **A byte from the keyboard port**, before the decoder sees it: the exchange's answer, and what
    /// to do, or `None` for a byte that is not one — a scancode, which the decoder takes.
    pub fn on_byte(&mut self, byte: u8, now: u64) -> Option<Action> {
        match byte {
            ACK => Some(match self.step {
                Step::Command => {
                    self.step = Step::Mask;
                    self.sent_ns = now;
                    Action::Send(self.mask)
                }
                Step::Mask => Action::Done(Ok(())),
            }),
            RESEND => {
                self.resends += 1;
                if self.resends > RESENDS_MAX {
                    return Some(Action::Done(Err(Failure::Resends)));
                }
                self.sent_ns = now;
                Some(Action::Send(match self.step {
                    Step::Command => SET_LEDS,
                    Step::Mask => self.mask,
                }))
            }
            _ => None,
        }
    }

    /// **The tick**: the exchange fails if the keyboard has not answered within [`ANSWER_NS`].
    pub fn on_tick(&self, now: u64) -> Option<Action> {
        (now.saturating_sub(self.sent_ns) > ANSWER_NS).then_some(Action::Done(Err(Failure::NoAnswer)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_masks_are_reordered_for_the_ps2_keyboard() {
        assert_eq!(ps2_mask(LIGHT_NUM as u8), 0b010);
        assert_eq!(ps2_mask(LIGHT_CAPS as u8), 0b100);
        assert_eq!(ps2_mask(LIGHT_SCROLL as u8), 0b001);
        assert_eq!(ps2_mask((LIGHT_NUM | LIGHT_CAPS) as u8), 0b110);
        assert_eq!(ps2_mask(0), 0);
    }

    /// **`0xED`, its acknowledgement, the mask, its acknowledgement**, and done.
    #[test]
    fn an_exchange_sends_the_command_then_the_mask() {
        let (mut x, first) = Exchange::start((LIGHT_NUM | LIGHT_CAPS) as u8, 0);
        assert_eq!(first, SET_LEDS);
        assert_eq!(x.on_byte(ACK, 1), Some(Action::Send(0b110)));
        assert_eq!(x.on_byte(ACK, 2), Some(Action::Done(Ok(()))));
    }

    /// **Scancodes are not the exchange's**: they pass to the decoder, whatever the step.
    #[test]
    fn a_scancode_in_the_middle_is_the_decoders() {
        let (mut x, _) = Exchange::start(0, 0);
        assert_eq!(x.on_byte(0xE0, 1), None, "an E0 prefix");
        assert_eq!(x.on_byte(0x48, 1), None, "Up");
        assert_eq!(x.on_byte(0xC8, 1), None, "Up released");
        assert_eq!(x.on_byte(ACK, 1), Some(Action::Send(0)));
    }

    /// **A resend asks for the byte again**, three times at most.
    #[test]
    fn a_resend_sends_the_byte_again_a_bounded_number_of_times() {
        let (mut x, _) = Exchange::start(LIGHT_CAPS as u8, 0);
        assert_eq!(x.on_byte(RESEND, 1), Some(Action::Send(SET_LEDS)));
        assert_eq!(x.on_byte(ACK, 2), Some(Action::Send(0b100)));
        assert_eq!(x.on_byte(RESEND, 3), Some(Action::Send(0b100)), "the mask again");
        assert_eq!(x.on_byte(RESEND, 4), Some(Action::Send(0b100)));
        assert_eq!(x.on_byte(RESEND, 5), Some(Action::Done(Err(Failure::Resends))), "a fourth");
    }

    /// **A resend restarts the answer's clock** (PR #359 review): the byte sent again has the whole
    /// bound, not what was left of the first's.
    #[test]
    fn a_resend_restarts_the_answers_clock() {
        let (mut x, _) = Exchange::start(0, 0);
        let late = ANSWER_NS - 1;
        assert_eq!(x.on_byte(RESEND, late), Some(Action::Send(SET_LEDS)));
        assert_eq!(x.on_tick(late + ANSWER_NS), None, "the byte sent again has its whole bound");
        assert_eq!(x.on_tick(late + ANSWER_NS + 1), Some(Action::Done(Err(Failure::NoAnswer))));
    }

    /// **An answer that never comes** fails the exchange on the tick after its bound, and not before.
    #[test]
    fn no_answer_fails_on_the_tick_after_its_bound() {
        let (mut x, _) = Exchange::start(0, 1_000);
        assert_eq!(x.on_tick(1_000 + ANSWER_NS), None, "not a nanosecond early");
        assert_eq!(x.on_tick(1_000 + ANSWER_NS + 1), Some(Action::Done(Err(Failure::NoAnswer))));
        // An answer restarts the clock for the next byte.
        assert_eq!(x.on_byte(ACK, 5_000), Some(Action::Send(0)));
        assert_eq!(x.on_tick(1_000 + ANSWER_NS + 1), None);
    }
}
