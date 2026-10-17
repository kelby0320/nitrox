//! **HID, whatever bus carries it**: what a HID keyboard's or mouse's report means, as
//! [`InputEvent`](crate::libkern::input::InputEvent)s.
//!
//! The USB binding that fetches reports is `drivers::xhci::hid`; this module knows nothing about
//! where a report came from, so the trackpad's native I²C-HID interface, should it be built, reads
//! its reports with the same code.
//!
//! - [`keyboard`]: a keyboard's report by its layout, and the Keyboard/Keypad page's usages as
//!   keycodes.
//! - [`mouse`]: a mouse's report by its layout.
//! - [`descriptor`]: a report descriptor read for a mouse's or keyboard's layout, by one walk of
//!   its items.

pub mod descriptor;
pub mod keyboard;
pub mod mouse;
