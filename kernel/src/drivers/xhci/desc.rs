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
    /// An endpoint descriptor.
    pub const ENDPOINT: u8 = 5;
    /// A HID descriptor, between a HID interface and its endpoints.
    pub const HID: u8 = 0x21;
    /// A HID report descriptor, as a HID descriptor names it.
    pub const REPORT: u8 = 0x22;
    /// A SuperSpeed endpoint's companion, after the endpoint.
    pub const SS_COMPANION: u8 = 0x30;
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

/// A configuration's `bConfigurationValue`, what `SET_CONFIGURATION` names it by, or `None` if `b`
/// is not one.
pub fn configuration_value(b: &[u8]) -> Option<u8> {
    configuration_total(b)?;
    Some(b[5])
}

/// **The descriptors inside a configuration**, after the configuration's own, each as its bytes: the
/// same walk as [`interfaces`], by each descriptor's own length within `wTotalLength`, ending at a
/// length that is too short or runs past the bytes.
fn descriptors(b: &[u8]) -> impl Iterator<Item = &[u8]> + '_ {
    let total = configuration_total(b).map_or(0, |t| (t as usize).min(b.len()));
    let mut at = if total > 0 { b[0] as usize } else { total };
    core::iter::from_fn(move || {
        if at + 2 > total {
            return None;
        }
        let len = b[at] as usize;
        if len < 2 || at + len > total {
            at = total;
            return None;
        }
        let here = at;
        at += len;
        Some(&b[here..here + len])
    })
}

/// **An endpoint** (USB 2.0 §9.6.6): its address, its maximum packet and burst, and its interval as
/// the descriptor gives it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// `bEndpointAddress`: the number in bits 3:0, IN in bit 7.
    pub address: u8,
    /// `wMaxPacketSize` bits 10:0.
    pub max_packet: u16,
    /// Further packets per interval: a high-speed periodic endpoint's `wMaxPacketSize` bits 12:11,
    /// or a SuperSpeed one's companion `bMaxBurst`; 0 otherwise.
    pub burst: u8,
    /// `bInterval`, as the descriptor says it; `context::interval` encodes it.
    pub interval: u8,
}

impl Endpoint {
    /// Its Device Context Index: twice its number, plus one for IN.
    pub fn dci(&self) -> u8 {
        (self.address & 0xF) * 2 + (self.address >> 7)
    }
}

/// **A HID interface**: its number and class triple, its first interrupt-IN
/// endpoint if it has one, and the length of the report descriptor its HID descriptor names.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct HidInterface {
    pub number: u8,
    pub class: (u8, u8, u8),
    pub endpoint: Option<Endpoint>,
    /// The report descriptor's `wDescriptorLength`; 0 when there is no HID descriptor.
    pub report_len: u16,
}

/// **The HID interfaces a configuration describes**, alternate setting 0 only, each with what
/// follows it up to the next interface: its HID descriptor, its first interrupt-IN endpoint, and that
/// endpoint's SuperSpeed companion. At most four; a configuration with more has them ignored.
pub fn hid_interfaces(b: &[u8]) -> impl Iterator<Item = HidInterface> {
    let mut found: [Option<HidInterface>; 4] = [None; 4];
    let mut n = 0;
    let mut current: Option<HidInterface> = None;
    // Whether the last descriptor was the current interface's chosen endpoint, so a companion after
    // it is that endpoint's.
    let mut after_endpoint = false;
    for d in descriptors(b) {
        match d[1] {
            kind::INTERFACE if d.len() >= 9 => {
                if let Some(h) = current.take()
                    && n < found.len()
                {
                    found[n] = Some(h);
                    n += 1;
                }
                if d[3] == 0 && d[5] == 0x03 {
                    current =
                        Some(HidInterface { number: d[2], class: (d[5], d[6], d[7]), endpoint: None, report_len: 0 });
                }
                after_endpoint = false;
            }
            kind::HID if d.len() >= 9 => {
                if let Some(h) = current.as_mut()
                    && d[6] == kind::REPORT
                {
                    h.report_len = u16::from_le_bytes([d[7], d[8]]);
                }
                after_endpoint = false;
            }
            kind::ENDPOINT if d.len() >= 7 => {
                after_endpoint = false;
                let Some(h) = current.as_mut() else { continue };
                let interrupt_in = d[3] & 0x3 == 0x3 && d[2] & 0x80 != 0;
                if h.endpoint.is_none() && interrupt_in {
                    let w = u16::from_le_bytes([d[4], d[5]]);
                    let burst = ((w >> 11) & 0x3) as u8;
                    h.endpoint = Some(Endpoint { address: d[2], max_packet: w & 0x7FF, burst, interval: d[6] });
                    after_endpoint = true;
                }
            }
            kind::SS_COMPANION if d.len() >= 6 => {
                if after_endpoint && let Some(e) = current.as_mut().and_then(|h| h.endpoint.as_mut()) {
                    e.burst = d[2];
                }
                after_endpoint = false;
            }
            _ => after_endpoint = false,
        }
    }
    if let Some(h) = current
        && n < found.len()
    {
        found[n] = Some(h);
    }
    found.into_iter().flatten()
}

/// **A bulk-only mass-storage interface**: its number, and its bulk IN and bulk
/// OUT endpoints, each with a SuperSpeed companion's burst.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct StorageInterface {
    pub number: u8,
    pub bulk_in: Endpoint,
    pub bulk_out: Endpoint,
}

/// **The first bulk-only mass-storage interface a configuration describes** (`08/06/50`, alternate
/// setting 0), with the first bulk endpoint of each direction after it and each one's companion.
/// `None` when there is none, or it lacks either endpoint: bulk-only cannot run on one.
pub fn storage_interface(b: &[u8]) -> Option<StorageInterface> {
    let (mut number, mut inside) = (None, false);
    let (mut bulk_in, mut bulk_out): (Option<Endpoint>, Option<Endpoint>) = (None, None);
    // Which endpoint a companion after it would be: `Some(true)` for IN, `Some(false)` for OUT.
    let mut last: Option<bool> = None;
    for d in descriptors(b) {
        match d[1] {
            kind::INTERFACE if d.len() >= 9 => {
                last = None;
                if number.is_some() {
                    // The interface after the chosen one: what follows is not its.
                    inside = false;
                    continue;
                }
                inside = d[3] == 0 && (d[5], d[6], d[7]) == (0x08, 0x06, 0x50);
                if inside {
                    number = Some(d[2]);
                }
            }
            kind::ENDPOINT if d.len() >= 7 => {
                last = None;
                if !inside || d[3] & 0x3 != 0x2 {
                    continue;
                }
                let w = u16::from_le_bytes([d[4], d[5]]);
                let e = Endpoint { address: d[2], max_packet: w & 0x7FF, burst: 0, interval: 0 };
                let is_in = d[2] & 0x80 != 0;
                let slot = if is_in { &mut bulk_in } else { &mut bulk_out };
                if slot.is_none() {
                    *slot = Some(e);
                    last = Some(is_in);
                }
            }
            kind::SS_COMPANION if d.len() >= 6 => {
                match last {
                    Some(true) => bulk_in.iter_mut().for_each(|e| e.burst = d[2]),
                    Some(false) => bulk_out.iter_mut().for_each(|e| e.burst = d[2]),
                    None => {}
                }
                last = None;
            }
            _ => last = None,
        }
    }
    Some(StorageInterface { number: number?, bulk_in: bulk_in?, bulk_out: bulk_out? })
}

/// What this kernel has, or will have, a driver for.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Match {
    /// HID, boot interface, keyboard: `03/01/01`.
    BootKeyboard,
    /// HID, boot interface, mouse: `03/01/02`.
    BootMouse,
    /// Mass storage, SCSI transparent, bulk-only: `08/06/50`.
    BulkOnlyStorage,
    /// A hub, class `09`: external hubs are not driven.
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
/// matches its first; each interface is bound on its own.
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
    if b.len() < 2 || (b[0] as usize) < 2 || b[1] != kind::STRING {
        return None;
    }
    let len = (b[0] as usize).min(b.len());
    let mut n = 0;
    for pair in b[2..len].as_chunks::<2>().0 {
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

/// **`vvvv:pppp`**, a device's name when it gives no string, into `out`, which
/// holds at least nine bytes. The length written.
pub fn ids_into(vendor: u16, product: u16, out: &mut [u8]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, shift) in [12u16, 8, 4, 0].into_iter().enumerate() {
        out[i] = HEX[(vendor >> shift & 0xF) as usize];
        out[5 + i] = HEX[(product >> shift & 0xF) as usize];
    }
    out[4] = b':';
    9
}

#[cfg(test)]
mod tests;
