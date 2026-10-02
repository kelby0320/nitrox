//! **The xHCI extended capabilities** (xHCI 1.2 §7): a chain of dword-aligned entries in the
//! controller's register window, starting at the offset `HCCPARAMS1` gives. Two matter here:
//! - **USB Legacy Support** (ID 1), through which the firmware hands the controller over;
//! - **Supported Protocol** (ID 2), one per USB major version, saying which ports speak it. A
//!   connector with a USB 2 and a USB 3 side has a port number in each range.
//!
//! The walk reads through a closure, so the host tests hand it register images.

/// Capability ID: USB Legacy Support.
pub const ID_LEGACY: u8 = 1;
/// Capability ID: Supported Protocol.
pub const ID_PROTOCOL: u8 = 2;

/// A Supported Protocol capability's name string, `"USB "`, as a little-endian dword.
const NAME_USB: u32 = u32::from_le_bytes(*b"USB ");

/// How many entries a walk reads before giving up. A next pointer only moves forward, so the window
/// bounds a walk anyway; this bounds what a chain of many short steps costs a probe.
const MAX_ENTRIES: usize = 64;

/// How many Supported Protocol entries are kept. A controller has one per USB major version it
/// speaks; four covers USB 1 to USB 4.
pub const MAX_PROTOCOLS: usize = 4;

/// One Supported Protocol capability: a USB version and the ports that speak it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Protocol {
    /// The USB major version: 2 or 3.
    pub major: u8,
    /// The minor version, as the capability gives it (`0x10` for USB 3.1).
    pub minor: u8,
    /// The first port that speaks it, numbered from 1.
    pub first: u8,
    /// How many consecutive ports do.
    pub count: u8,
    /// Its Protocol Speed ID count: zero when the default speed IDs apply.
    pub psic: u8,
}

impl Protocol {
    /// Whether `port` speaks this protocol.
    pub fn has(&self, port: u8) -> bool {
        port >= self.first && (port as u16) < self.first as u16 + self.count as u16
    }
}

/// What the walk found.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtCaps {
    /// The USB Legacy Support capability's offset in the register window.
    pub legacy: Option<u32>,
    /// The Supported Protocol capabilities, in chain order.
    pub protocols: [Option<Protocol>; MAX_PROTOCOLS],
    /// How many entries of other kinds the chain held.
    pub other: u32,
}

impl ExtCaps {
    /// The USB major version port `port` speaks, if any capability names it.
    pub fn major_of(&self, port: u8) -> Option<u8> {
        self.protocols.iter().flatten().find(|p| p.has(port)).map(|p| p.major)
    }
}

/// Walk the chain that starts at dword offset `xecp` (`HCCPARAMS1` bits 31:16), reading the
/// register window through `read` (a byte offset in, a dword out), within `window` bytes.
///
/// It ends at a next pointer of zero, at an entry past the window, or after [`MAX_ENTRIES`]. A
/// Supported Protocol entry whose name is not `"USB "` is counted among the others.
pub fn walk(read: impl Fn(u32) -> u32, xecp: u32, window: u32) -> ExtCaps {
    let mut found = ExtCaps::default();
    let mut kept = 0;
    let mut off = xecp * 4;
    for _ in 0..MAX_ENTRIES {
        if off == 0 || off.saturating_add(12) > window {
            break;
        }
        let hdr = read(off);
        let id = (hdr & 0xFF) as u8;
        if id == ID_LEGACY && found.legacy.is_none() {
            found.legacy = Some(off);
        } else if id == ID_PROTOCOL && read(off + 4) == NAME_USB && kept < MAX_PROTOCOLS {
            let ports = read(off + 8);
            found.protocols[kept] = Some(Protocol {
                major: (hdr >> 24) as u8,
                minor: (hdr >> 16) as u8,
                first: ports as u8,
                count: (ports >> 8) as u8,
                psic: (ports >> 28) as u8,
            });
            kept += 1;
        } else {
            found.other += 1;
        }
        let next = (hdr >> 8) & 0xFF;
        if next == 0 {
            break;
        }
        off += next * 4;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A register window as dwords at byte offsets; anything not set reads zero.
    fn reader(w: &HashMap<u32, u32>) -> impl Fn(u32) -> u32 + '_ {
        |off| *w.get(&off).unwrap_or(&0)
    }

    fn protocol(at: u32, major: u8, minor: u8, first: u8, count: u8, next: u32, w: &mut HashMap<u32, u32>) {
        w.insert(at, (major as u32) << 24 | (minor as u32) << 16 | next << 8 | ID_PROTOCOL as u32);
        w.insert(at + 4, NAME_USB);
        w.insert(at + 8, (count as u32) << 8 | first as u32);
    }

    /// **A controller the shape of the laptop's**: Legacy Support first, then USB 2 and USB 3, then
    /// a vendor-defined entry, as Intel's lay them out.
    #[test]
    fn legacy_and_two_protocols_are_found_and_the_rest_counted() {
        let mut w = HashMap::new();
        w.insert(0x8000, 0x0000_0401); // legacy, next +4 dwords
        protocol(0x8010, 2, 0x00, 1, 12, 4, &mut w);
        protocol(0x8020, 3, 0x00, 13, 4, 4, &mut w);
        w.insert(0x8030, 0x0000_00C0); // vendor-defined, last
        let caps = walk(reader(&w), 0x8000 / 4, 0x1_0000);
        assert_eq!(caps.legacy, Some(0x8000));
        assert_eq!(caps.protocols[0], Some(Protocol { major: 2, minor: 0, first: 1, count: 12, psic: 0 }));
        assert_eq!(caps.protocols[1], Some(Protocol { major: 3, minor: 0, first: 13, count: 4, psic: 0 }));
        assert_eq!(caps.other, 1);
        assert_eq!(caps.major_of(1), Some(2));
        assert_eq!(caps.major_of(12), Some(2));
        assert_eq!(caps.major_of(13), Some(3), "the range's edge, from both sides");
        assert_eq!(caps.major_of(16), Some(3));
        assert_eq!(caps.major_of(17), None);
        assert_eq!(caps.major_of(0), None, "ports are numbered from 1");
    }

    /// **A runaway chain ends at the bound.** A next pointer only moves forward, so a chain cannot
    /// loop; but one of nothing but one-dword steps across a large window would be read to its end.
    /// It is read [`MAX_ENTRIES`] times instead. And a next of zero ends a chain at once.
    #[test]
    fn a_runaway_chain_ends_at_the_bound() {
        let reads = core::cell::Cell::new(0);
        let caps = walk(
            |_| {
                reads.set(reads.get() + 1);
                0x0000_010A // ID 10 (debug), next: one dword on
            },
            0x10,
            0x10_0000,
        );
        assert_eq!(caps.other as usize, MAX_ENTRIES);
        assert_eq!(reads.get(), MAX_ENTRIES);
        assert_eq!(walk(|_| 0x0000_000A, 0x10, 0x1000).other, 1, "next = 0 ends after one");
    }

    /// An entry past the end of the window is not read, and neither is a chain that runs off it.
    #[test]
    fn the_window_bounds_the_walk() {
        let mut w = HashMap::new();
        protocol(0x40, 2, 0, 1, 4, 0x40, &mut w); // next: +0x100 bytes, past a 0x100 window
        protocol(0x140, 3, 0, 5, 4, 0, &mut w);
        let caps = walk(reader(&w), 0x10, 0x100);
        assert_eq!(caps.protocols[0].map(|p| p.major), Some(2));
        assert_eq!(caps.protocols[1], None, "the USB 3 entry is outside the window");
        assert_eq!(walk(|_| 0, 0, 0x1000), ExtCaps::default(), "no chain at all");
    }

    /// A Supported Protocol entry with some other name is not a USB one.
    #[test]
    fn a_protocol_entry_not_named_usb_is_not_taken() {
        let mut w = HashMap::new();
        protocol(0x40, 2, 0, 1, 4, 0, &mut w);
        w.insert(0x44, u32::from_le_bytes(*b"XYZ "));
        let caps = walk(reader(&w), 0x10, 0x1000);
        assert_eq!(caps.protocols[0], None);
        assert_eq!(caps.other, 1);
    }
}
