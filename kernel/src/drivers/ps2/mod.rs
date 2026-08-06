//! The **i8042** controller driver (Tier 1) — PS/2 keyboard and mouse.
//!
//! `docs/design/input-subsystem.md`. One driver, because the keyboard and the mouse are two
//! *devices behind one controller*: they share data port `0x60`, are configured through
//! command port `0x64`, and enabling the mouse is a read-modify-write of the same config
//! byte that carries the keyboard's IRQ-1 enable. Two drivers initialising independently
//! race on that byte and produce a machine that intermittently boots with a dead keyboard.
//!
//! It publishes **two** char `DeviceNode`s — `/dev/input/raw/0` (keyboard) and
//! `/dev/input/raw/1` (mouse) — each delivering
//! [`InputEvent`](crate::libkern::input::InputEvent) records.
//!
//! ## What is here and what is in `arch`
//!
//! Port I/O and interrupt arming are x86-only and stay behind `crate::arch::ps2`, exactly as
//! the serial console keeps COM1's registers and IRQ behind `crate::arch::serial`. The
//! decision is older than this driver: `arch/mod.rs` refuses to re-export `install_isa_irq`
//! neutrally because "ISA" is x86 jargon, and "a fixed legacy platform device wires its own
//! interrupt inside the arch layer". The i8042 is exactly that class of device.
//!
//! This module owns what is portable: the scancode table, the mouse packet framing, and the
//! event rings.

pub mod mouse;
pub mod ring;
pub mod scancode;
