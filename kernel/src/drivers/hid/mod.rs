//! **HID, whatever bus carries it** (Phase 6 Part B): what a HID keyboard's or mouse's report
//! means, as [`InputEvent`](crate::libkern::input::InputEvent)s.
//!
//! The USB binding that fetches reports is `drivers::xhci::hid`; this module knows nothing about
//! where a report came from, so the trackpad's native I²C-HID interface, should it be built, reads
//! its reports with the same code.
//!
//! - [`keyboard`]: a boot keyboard's report, and the Keyboard/Keypad page's usages as keycodes.
//! - [`mouse`]: a mouse's report by its layout — the boot protocol's, or one a report descriptor
//!   gives.
//! - [`descriptor`]: a report descriptor read for a mouse's layout (Phase 6 Part B.3).

pub mod descriptor;
pub mod keyboard;
pub mod mouse;
