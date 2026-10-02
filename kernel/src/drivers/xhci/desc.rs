//! **USB descriptors, read** (USB 2.0 §9.6, USB 3.2 §9.6): what a device says it is, parsed from
//! the bytes it sent, and **the class match** against what this kernel has drivers for.
//!
//! Every reader takes bytes from a device, which may send fewer than it was asked for, or say
//! lengths its own bytes do not have. So every length is checked against the bytes in hand, and a
//! descriptor that does not fit is not read. Host-tested with bytes no correct device sends.

/// Descriptor types.
pub mod kind {
    /// A device descriptor.
    pub const DEVICE: u8 = 1;
    /// A configuration descriptor, followed by its interfaces and endpoints.
    pub const CONFIGURATION: u8 = 2;
    /// A string descriptor.
    pub const STRING: u8 = 3;
    /// An interface descriptor.
    pub const INTERFACE: u8 = 4;
}

/// A device descriptor's fields.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// The USB release it claims, as binary-coded decimal: `0x0200`, `0x0300`.
    pub usb: u16,
    /// Class, subclass and protocol: zero when each interface says its own.
    pub class: (u8, u8, u8),
    /// The default endpoint's maximum packet: bytes, or at SuperSpeed an exponent.
    pub max_packet0: u8,
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// The manufacturer's string index; zero for none.
    pub manufacturer: u8,
    /// The product's string index; zero for none.
    pub product_string: u8,
    /// The serial number's string index; zero for none.
    pub serial: u8,
    /// How many configurations it has.
    pub configurations: u8,
}

/// Read a device descriptor, or `None` if `b` is not one: too short for what it says, or another
/// type. The first eight bytes are enough for [`Device::max_packet0`]; [`device_prefix`] reads them.
pub fn device(b: &[u8]) -> Option<Device> {
    if b.len() < 18 || (b[0] as usize) < 18 || b[1] != kind::DEVICE {
        return None;
    }
    let w = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    Some(Device {
        usb: w(2),
        class: (b[4], b[5], b[6]),
        max_packet0: b[7],
        vendor: w(8),
        product: w(10),
        manufacturer: b[14],
        product_string: b[15],
        serial: b[16],
        configurations: b[17],
    })
}

/// `bMaxPacketSize0` from a device descriptor's first eight bytes, or `None` if they are not one.
pub fn device_prefix(b: &[u8]) -> Option<u8> {
    if b.len() < 8 || b[1] != kind::DEVICE || (b[0] as usize) < 8 {
        return None;
    }
    Some(b[7])
}

/// **The default endpoint's maximum packet in bytes**, from `bMaxPacketSize0`: a byte count below
/// SuperSpeed, and **at SuperSpeed an exponent** — 9 means 512. `None` for a value the speed does
/// not allow.
pub fn max_packet0(super_speed: bool, raw: u8) -> Option<u16> {
    if super_speed {
        // USB 3.2 §9.6.1: it shall be 09h.
        (raw == 9).then_some(512)
    } else {
        matches!(raw, 8 | 16 | 32 | 64).then_some(raw as u16)
    }
}

/// An interface descriptor's fields, for the class match.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// `bAlternateSetting`: always 0 from [`interfaces`].
    pub alternate: u8,
    /// How many endpoints it has, the default one aside.
    pub endpoints: u8,
    /// Class, subclass and protocol.
    pub class: (u8, u8, u8),
}

/// A configuration descriptor's total length, from its first nine bytes, or `None` if they are not
/// one.
pub fn configuration_total(b: &[u8]) -> Option<u16> {
    if b.len() < 9 || (b[0] as usize) < 9 || b[1] != kind::CONFIGURATION {
        return None;
    }
    Some(u16::from_le_bytes([b[2], b[3]]))
}

/// The interfaces a configuration describes, **alternate setting 0 only**, in order. The walk is by
/// each descriptor's own length within `b` (cut to the configuration's `wTotalLength`), and ends
/// at a length of zero or one that runs past the bytes: a device that lies about a length costs it
/// its interfaces, not the kernel a read past the buffer.
pub fn interfaces(b: &[u8]) -> impl Iterator<Item = Interface> + '_ {
    let total = configuration_total(b).map_or(0, |t| (t as usize).min(b.len()));
    let mut at = if total > 0 { b[0] as usize } else { total };
    core::iter::from_fn(move || {
        while at + 2 <= total {
            let len = b[at] as usize;
            if len < 2 || at + len > total {
                at = total;
                return None;
            }
            let here = at;
            at += len;
            if b[here + 1] == kind::INTERFACE && len >= 9 && b[here + 3] == 0 {
                return Some(Interface {
                    number: b[here + 2],
                    alternate: b[here + 3],
                    endpoints: b[here + 4],
                    class: (b[here + 5], b[here + 6], b[here + 7]),
                });
            }
        }
        None
    })
}

/// What this kernel has, or will have, a driver for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Match {
    /// HID, boot interface, keyboard: `03/01/01` (Part B).
    BootKeyboard,
    /// HID, boot interface, mouse: `03/01/02` (Part B).
    BootMouse,
    /// Mass storage, SCSI transparent, bulk-only: `08/06/50` (Part D).
    BulkOnlyStorage,
    /// A hub, class `09`: external hubs are outside Phase 6.
    Hub,
    /// Nothing this kernel drives.
    Nothing,
}

impl Match {
    /// What the log says of it.
    pub fn says(self) -> &'static str {
        match self {
            Match::BootKeyboard => "HID boot keyboard",
            Match::BootMouse => "HID boot mouse",
            Match::BulkOnlyStorage => "mass storage, bulk-only",
            Match::Hub => "a hub, not supported",
            Match::Nothing => "nothing this kernel drives",
        }
    }
}

/// The match for one interface's class triple, or a device's.
fn match_triple(t: (u8, u8, u8)) -> Match {
    match t {
        (0x03, 0x01, 0x01) => Match::BootKeyboard,
        (0x03, 0x01, 0x02) => Match::BootMouse,
        (0x08, 0x06, 0x50) => Match::BulkOnlyStorage,
        (0x09, _, _) => Match::Hub,
        _ => Match::Nothing,
    }
}

/// **The device's match**: the first interface that matches anything, or the device's own class
/// when it is not zero and no interface does. A composite device — a keyboard and a mouse in one —
/// matches its first; Part B binds each interface.
pub fn class_match(dev: &Device, config: &[u8]) -> Match {
    if let Some(m) = interfaces(config).map(|i| match_triple(i.class)).find(|m| *m != Match::Nothing) {
        return m;
    }
    match_triple(dev.class)
}

/// The class triple a record carries: the device's, or the first interface's when the device's
/// is zero, as most are.
pub fn record_class(dev: &Device, config: &[u8]) -> (u8, u8, u8) {
    if dev.class.0 != 0 {
        return dev.class;
    }
    interfaces(config).next().map_or(dev.class, |i| i.class)
}

/// The first language a string descriptor 0 lists, or `None` if `b` is not one.
pub fn first_language(b: &[u8]) -> Option<u16> {
    if b.len() < 4 || (b[0] as usize) < 4 || b[1] != kind::STRING {
        return None;
    }
    Some(u16::from_le_bytes([b[2], b[3]]))
}

/// **A string descriptor's text, printable**, into `out`: UTF-16LE, each character outside
/// printable ASCII written as `?`, cut to `out`'s length and to the descriptor's own. The length
/// written, or `None` if `b` is not a string descriptor. Trailing spaces are dropped, since
/// devices pad.
pub fn string_into(b: &[u8], out: &mut [u8]) -> Option<usize> {
    if b.len() < 2 || b[1] != kind::STRING {
        return None;
    }
    let len = (b[0] as usize).min(b.len());
    let mut n = 0;
    for pair in b[2..len].chunks_exact(2) {
        if n == out.len() {
            break;
        }
        let c = u16::from_le_bytes([pair[0], pair[1]]);
        out[n] = if (0x20..0x7F).contains(&c) { c as u8 } else { b'?' };
        n += 1;
    }
    while n > 0 && out[n - 1] == b' ' {
        n -= 1;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A high-speed keyboard's device descriptor, as the USB 2.0 layout gives it: class zero
    /// (each interface says), a 64-byte default endpoint, strings 1, 2 and 3.
    const KEYBOARD: [u8; 18] =
        [18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 2, 3, 1];

    /// Its configuration: the configuration, a HID boot keyboard interface, its HID descriptor and
    /// its interrupt-IN endpoint. 34 bytes.
    fn keyboard_config() -> Vec<u8> {
        let mut c = vec![9, 2, 34, 0, 1, 1, 0, 0xA0, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
        c.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 63, 0]);
        c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0, 10]);
        c
    }

    #[test]
    fn a_device_descriptor_reads_field_by_field() {
        let d = device(&KEYBOARD).unwrap();
        assert_eq!(d.usb, 0x0200);
        assert_eq!(d.class, (0, 0, 0));
        assert_eq!(d.max_packet0, 64);
        assert_eq!((d.vendor, d.product), (0x0627, 0x0001));
        assert_eq!((d.manufacturer, d.product_string, d.serial), (1, 2, 3));
        assert_eq!(device_prefix(&KEYBOARD[..8]), Some(64), "the first eight bytes are enough for that");
    }

    /// **Bytes a correct device does not send**: too few, a length that says less than a device
    /// descriptor is, or another type. None is read.
    #[test]
    fn a_short_or_mislabelled_device_descriptor_is_not_read() {
        assert_eq!(device(&KEYBOARD[..17]), None, "one byte short");
        let mut lies = KEYBOARD;
        lies[0] = 8;
        assert_eq!(device(&lies), None, "bLength says it is shorter than a device descriptor");
        let mut config = KEYBOARD;
        config[1] = kind::CONFIGURATION;
        assert_eq!(device(&config), None);
        assert_eq!(device_prefix(&KEYBOARD[..7]), None);
    }

    /// **At SuperSpeed `bMaxPacketSize0` is an exponent** (PR #353 review): 9 is 512 there, and
    /// below SuperSpeed only the four byte counts USB 2.0 allows are taken.
    #[test]
    fn the_default_endpoints_packet_is_an_exponent_at_super_speed() {
        assert_eq!(max_packet0(true, 9), Some(512));
        assert_eq!(max_packet0(true, 64), None, "a byte count where an exponent belongs");
        assert_eq!(max_packet0(false, 9), None, "an exponent where a byte count belongs");
        for raw in [8, 16, 32, 64] {
            assert_eq!(max_packet0(false, raw), Some(raw as u16));
        }
        assert_eq!(max_packet0(false, 0), None);
    }

    #[test]
    fn a_keyboards_configuration_matches_the_boot_keyboard() {
        let d = device(&KEYBOARD).unwrap();
        let c = keyboard_config();
        assert_eq!(configuration_total(&c), Some(34));
        let ifs: Vec<Interface> = interfaces(&c).collect();
        assert_eq!(ifs, vec![Interface { number: 0, alternate: 0, endpoints: 1, class: (3, 1, 1) }]);
        assert_eq!(class_match(&d, &c), Match::BootKeyboard);
        assert_eq!(record_class(&d, &c), (3, 1, 1), "the device's own class is zero");
    }

    /// **A composite device and a hub.** A keyboard-and-mouse receiver matches its first
    /// interface; an alternate setting is not an interface of its own; a hub matches by the
    /// device's class.
    #[test]
    fn a_composite_device_matches_its_first_and_a_hub_by_its_class() {
        let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0xA0, 50];
        c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 2, 0]); // mouse first
        c.extend_from_slice(&[9, 4, 0, 1, 1, 3, 1, 1, 0]); // an alternate of it: not counted
        c.extend_from_slice(&[9, 4, 1, 0, 1, 3, 1, 1, 0]); // keyboard
        let total = c.len() as u16;
        c[2..4].copy_from_slice(&total.to_le_bytes());
        let d = device(&KEYBOARD).unwrap();
        assert_eq!(interfaces(&c).count(), 2);
        assert_eq!(class_match(&d, &c), Match::BootMouse);
        let mut hub = KEYBOARD;
        hub[4] = 9;
        let hub = device(&hub).unwrap();
        let mut hc = vec![9, 2, 25, 0, 1, 1, 0, 0xE0, 0];
        hc.extend_from_slice(&[9, 4, 0, 0, 1, 9, 0, 0, 0]);
        hc.extend_from_slice(&[7, 5, 0x81, 3, 1, 0, 12]);
        assert_eq!(class_match(&hub, &hc), Match::Hub);
        assert_eq!(record_class(&hub, &hc), (9, 0, 0));
    }

    /// **A configuration that lies about its lengths** is read as far as its bytes go, and no
    /// further: a total past the buffer, a descriptor whose length runs past the total, and a
    /// length of zero, which would otherwise never advance.
    #[test]
    fn a_configuration_that_lies_about_lengths_is_not_read_past_its_bytes() {
        let mut long = keyboard_config();
        long[2..4].copy_from_slice(&4096u16.to_le_bytes());
        assert_eq!(interfaces(&long).count(), 1, "the total is cut to the bytes in hand");
        let mut runs = keyboard_config();
        runs[9] = 200; // the interface says it is 200 bytes long
        assert_eq!(interfaces(&runs).count(), 0);
        let mut zero = keyboard_config();
        zero[9] = 0;
        assert_eq!(interfaces(&zero).count(), 0, "a zero length ends the walk instead of looping");
        assert_eq!(interfaces(&keyboard_config()[..8]).count(), 0, "not even a configuration");
    }

    /// Hex to bytes, for the captures below.
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// **What real devices sent**: QEMU 11.0.2's `usb-kbd` at high speed, `usb-mouse` at full
    /// speed, and `usb-ccid`, captured with each device's `pcap=` during an enumeration by this
    /// driver (2026-10-02). The reader has a 54-byte class descriptor among its interface's, and a
    /// 64-byte default endpoint at full speed — the case that takes Evaluate Context.
    #[test]
    fn qemus_devices_read_as_they_enumerated() {
        let kbd = device(&hex("120100020000004027060100000001040b01")).unwrap();
        let kbd_config = hex(
            "09022200010108a032090400000103010100092111010001223f0007058103080007",
        );
        assert_eq!((kbd.vendor, kbd.product, kbd.max_packet0), (0x0627, 0x0001, 64));
        assert_eq!((kbd.product_string, kbd.serial), (4, 11));
        assert_eq!(class_match(&kbd, &kbd_config), Match::BootKeyboard);
        assert_eq!(max_packet0(false, kbd.max_packet0), Some(64), "high speed's default: no evaluation");

        let mouse = device(&hex("120100020000000827060100000001020901")).unwrap();
        let mouse_config = hex(
            "09022200010106a0320904000001030102000921010000012234000705810304000a",
        );
        assert_eq!(class_match(&mouse, &mouse_config), Match::BootMouse);
        assert_eq!(record_class(&mouse, &mouse_config), (3, 1, 2));

        let ccid = device(&hex("1201100100000040e6083344000001020301")).unwrap();
        let ccid_config = hex(concat!(
            "09025d00010100e03209040000030b00000436211001000701000000a00f000000000100008025000000",
            "c2010000fe0000000000000000000000fe04010012000100ffff000001010705810340",
            "00ff0705820240000007050302400000"
        ));
        assert_eq!(ccid.usb, 0x0110);
        assert_eq!(max_packet0(false, ccid.max_packet0), Some(64), "full speed with 64: evaluated");
        assert_eq!(configuration_total(&ccid_config), Some(93));
        assert_eq!(interfaces(&ccid_config).count(), 1, "its class descriptor is walked past, not taken");
        assert_eq!(class_match(&ccid, &ccid_config), Match::Nothing);
        assert_eq!(record_class(&ccid, &ccid_config), (0x0B, 0, 0));

        // Strings, as a 255-byte request returns them: exactly the descriptor, zeros after.
        let mut product = hex("2403510045004d005500200055005300420020004b006500790062006f00610072006400");
        product.resize(255, 0);
        let mut out = [0u8; 72];
        let n = string_into(&product, &mut out).unwrap();
        assert_eq!(&out[..n], b"QEMU USB Keyboard");
        assert_eq!(first_language(&hex("04030904")), Some(0x0409), "US English");
    }

    /// **The checks a reviewer could delete without a test failing** (PR #355 review): a string
    /// descriptor 0 too short to name a language, *as the driver hands it over* — 255 bytes with
    /// zeros after it, where the zeros would read as language 0 — and a device prefix of the wrong
    /// type or too short a length.
    #[test]
    fn a_short_language_list_and_a_mislabelled_prefix_are_not_read() {
        let mut none = vec![2, kind::STRING];
        none.resize(255, 0);
        assert_eq!(first_language(&none), None, "no language listed, whatever follows it");
        let mut prefix = KEYBOARD[..8].to_vec();
        prefix[1] = kind::CONFIGURATION;
        assert_eq!(device_prefix(&prefix), None, "another type");
        let mut short = KEYBOARD[..8].to_vec();
        short[0] = 7;
        assert_eq!(device_prefix(&short), None, "bLength says it is shorter than the prefix");
    }

    /// **An interface's match before the device's class.** A composite device with an Interface
    /// Association — device class `EF/02/01`, which matches nothing — and a keyboard interface is a
    /// keyboard: reading the device's class first would call it nothing.
    #[test]
    fn a_composite_devices_interface_matches_before_its_device_class() {
        let mut iad = KEYBOARD;
        iad[4..7].copy_from_slice(&[0xEF, 0x02, 0x01]);
        let iad = device(&iad).unwrap();
        assert_eq!(class_match(&iad, &keyboard_config()), Match::BootKeyboard);
        assert_eq!(record_class(&iad, &keyboard_config()), (0xEF, 0x02, 0x01), "the record keeps the device's");
    }

    /// **A string, made printable**: UTF-16LE, anything outside printable ASCII as `?`, padding
    /// dropped, and cut to the space it is written into.
    #[test]
    fn a_string_descriptor_is_made_printable() {
        let mut s = vec![0, kind::STRING];
        for c in "QEMU USB Keyboard  ".encode_utf16() {
            s.extend_from_slice(&c.to_le_bytes());
        }
        s.extend_from_slice(&0x00E9u16.to_le_bytes()); // é
        s[0] = s.len() as u8;
        let mut out = [0u8; 72];
        let n = string_into(&s, &mut out).unwrap();
        assert_eq!(&out[..n], b"QEMU USB Keyboard  ?");
        let mut short = [0u8; 4];
        assert_eq!(string_into(&s, &mut short), Some(4));
        assert_eq!(&short, b"QEMU");
        let mut lies = s.clone();
        lies[0] = 200; // says it is longer than it is
        assert_eq!(string_into(&lies, &mut out), Some(n), "read as far as the bytes go");
        // **As the driver hands it over**: 255 bytes asked for, the string's own length in its
        // first byte, and zeros after. The zeros are not text.
        let mut asked = s.clone();
        asked.resize(255, 0);
        assert_eq!(string_into(&asked, &mut out), Some(n), "read as far as its own length, not the buffer's");
        assert_eq!(&out[..n], b"QEMU USB Keyboard  ?");
        let mut padded = vec![0, kind::STRING];
        for c in "QEMU   ".encode_utf16() {
            padded.extend_from_slice(&c.to_le_bytes());
        }
        padded[0] = padded.len() as u8;
        let n = string_into(&padded, &mut out).unwrap();
        assert_eq!(&out[..n], b"QEMU", "the padding a device sends is dropped");
        assert_eq!(first_language(&[4, 3, 0x09, 0x04]), Some(0x0409));
        assert_eq!(first_language(&[2, 3]), None, "no language listed");
    }
}
