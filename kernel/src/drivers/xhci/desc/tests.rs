use super::*;

/// A high-speed keyboard's device descriptor, as the USB 2.0 layout gives it: class zero
/// (each interface says), a 64-byte default endpoint, strings 1, 2 and 3.
const KEYBOARD: [u8; 18] = [18, 1, 0x00, 0x02, 0, 0, 0, 64, 0x27, 0x06, 0x01, 0x00, 0x00, 0x00, 1, 2, 3, 1];

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

/// **At SuperSpeed `bMaxPacketSize0` is an exponent**: 9 is 512 there, and
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
    assert_eq!(ending(move || interfaces(&zero).count()), 0, "a zero length ends the walk instead of looping");
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
    let kbd_config = hex("09022200010108a032090400000103010100092111010001223f0007058103080007");
    assert_eq!((kbd.vendor, kbd.product, kbd.max_packet0), (0x0627, 0x0001, 64));
    assert_eq!((kbd.product_string, kbd.serial), (4, 11));
    assert_eq!(class_match(&kbd, &kbd_config), Match::BootKeyboard);
    assert_eq!(max_packet0(false, kbd.max_packet0), Some(64), "high speed's default: no evaluation");

    let mouse = device(&hex("120100020000000827060100000001020901")).unwrap();
    let mouse_config = hex("09022200010106a0320904000001030102000921010000012234000705810304000a");
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

/// **The checks a reviewer could delete without a test failing**: a string
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

/// **QEMU's keyboard and mouse, as HID interfaces**, from the bytes they sent:
/// each one interface with its interrupt-IN endpoint 1 and its report descriptor's length, and
/// the reader none. Endpoint 1 IN is Device Context Index 3.
#[test]
fn qemus_hid_interfaces_read_with_their_endpoints() {
    let kbd = hex("09022200010108a032090400000103010100092111010001223f0007058103080007");
    let found: Vec<HidInterface> = hid_interfaces(&kbd).collect();
    let ep = Endpoint { address: 0x81, max_packet: 8, burst: 0, interval: 7 };
    assert_eq!(found, vec![HidInterface { number: 0, class: (3, 1, 1), endpoint: Some(ep), report_len: 63 }]);
    assert_eq!(ep.dci(), 3);
    assert_eq!(configuration_value(&kbd), Some(1));
    let mouse = hex("09022200010106a0320904000001030102000921010000012234000705810304000a");
    let m: Vec<HidInterface> = hid_interfaces(&mouse).collect();
    assert_eq!(m[0].endpoint, Some(Endpoint { address: 0x81, max_packet: 4, burst: 0, interval: 10 }));
    assert_eq!((m[0].class, m[0].report_len), ((3, 1, 2), 52));
    let ccid = hex(concat!(
        "09025d00010100e03209040000030b00000436211001000701000000a00f000000000100008025000000",
        "c2010000fe0000000000000000000000fe04010012000100ffff000001010705810340",
        "00ff0705820240000007050302400000"
    ));
    assert_eq!(hid_interfaces(&ccid).count(), 0, "a smart-card reader is not HID");
}

/// **A receiver with a keyboard, a mouse and a vendor interface** gives two HID interfaces, each
/// with its own endpoint; an OUT endpoint and an alternate setting's are passed over; a
/// high-speed endpoint's extra transactions and a SuperSpeed companion's burst are read.
#[test]
fn each_hid_interface_takes_its_own_interrupt_in_endpoint() {
    let mut c = vec![9, 2, 0, 0, 3, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 2, 3, 1, 1, 0]); // keyboard, alt 0
    c.extend_from_slice(&[9, 0x21, 0x11, 0x01, 0, 1, 0x22, 65, 0]);
    c.extend_from_slice(&[7, 5, 0x01, 3, 8, 0, 10]); // an OUT interrupt endpoint: not this one
    c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0x10, 4]); // IN, 8 bytes, bits 12:11 = 2
    c.extend_from_slice(&[9, 4, 0, 1, 1, 3, 1, 1, 0]); // the keyboard's alternate setting
    c.extend_from_slice(&[7, 5, 0x83, 3, 64, 0, 1]); // its endpoint: not alt 0's
    c.extend_from_slice(&[9, 4, 1, 0, 1, 3, 1, 2, 0]); // mouse
    c.extend_from_slice(&[7, 5, 0x82, 3, 4, 0, 10]);
    c.extend_from_slice(&[6, 0x30, 3, 0, 4, 0]); // SuperSpeed companion: burst 3
    c.extend_from_slice(&[9, 4, 2, 0, 1, 0xFF, 0, 0, 0]); // vendor
    c.extend_from_slice(&[7, 5, 0x84, 3, 64, 0, 1]);
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    let found: Vec<HidInterface> = hid_interfaces(&c).collect();
    assert_eq!(found.len(), 2, "{found:?}");
    assert_eq!(found[0].endpoint, Some(Endpoint { address: 0x81, max_packet: 8, burst: 2, interval: 4 }));
    assert_eq!(found[0].report_len, 65);
    assert_eq!(found[1].number, 1);
    assert_eq!(found[1].endpoint, Some(Endpoint { address: 0x82, max_packet: 4, burst: 3, interval: 10 }));
    assert_eq!(found[1].endpoint.unwrap().dci(), 5);
    assert_eq!(found[1].report_len, 0, "no HID descriptor");
}

/// **A companion is its own endpoint's**: one after an endpoint that is not the
/// chosen one — here an OUT endpoint after the chosen IN — leaves the chosen one's burst alone.
#[test]
fn a_companion_after_another_endpoint_is_not_the_chosen_ones() {
    let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 2, 3, 1, 2, 0]); // a mouse
    c.extend_from_slice(&[7, 5, 0x81, 3, 4, 0, 10]); // IN: the chosen one
    c.extend_from_slice(&[6, 0x30, 1, 0, 4, 0]); // its companion: burst 1
    c.extend_from_slice(&[7, 5, 0x02, 3, 4, 0, 10]); // OUT
    c.extend_from_slice(&[6, 0x30, 7, 0, 4, 0]); // the OUT endpoint's: burst 7
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    let found: Vec<HidInterface> = hid_interfaces(&c).collect();
    assert_eq!(found[0].endpoint.map(|e| e.burst), Some(1));
}

/// **An endpoint that runs past the configuration is not read**: the interface is still there,
/// with no endpoint to bind.
#[test]
fn a_hid_endpoint_past_the_configuration_is_not_read() {
    let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 1, 3, 1, 1, 0]);
    c.extend_from_slice(&[7, 5, 0x81, 3, 8, 0]); // one byte short
    let total = c.len() as u16 + 1; // says one more byte than there is
    c[2..4].copy_from_slice(&total.to_le_bytes());
    let found: Vec<HidInterface> = hid_interfaces(&c).collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].endpoint, None);
    assert_eq!(configuration_value(&[9, 2]), None);
}

/// **A stick's bulk-only interface**, as QEMU's SuperSpeed one describes it: its two bulk
/// endpoints, each with its own companion's burst, an interrupt endpoint and another interface's
/// bulk endpoints passed over.
#[test]
fn a_storage_interface_takes_its_bulk_endpoints_and_their_bursts() {
    let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 3, 8, 6, 0x50, 0]); // bulk-only
    c.extend_from_slice(&[7, 5, 0x83, 3, 8, 0, 4]); // interrupt: not bulk
    c.extend_from_slice(&[7, 5, 0x81, 2, 0, 4, 0]); // bulk IN, 1024 bytes
    c.extend_from_slice(&[6, 0x30, 15, 0, 0, 0]); // its companion: burst 15
    c.extend_from_slice(&[7, 5, 0x02, 2, 0, 4, 0]); // bulk OUT, 1024 bytes
    c.extend_from_slice(&[6, 0x30, 3, 0, 0, 0]); // its companion: burst 3
    c.extend_from_slice(&[9, 4, 1, 0, 2, 0xFF, 0, 0, 0]); // vendor
    c.extend_from_slice(&[7, 5, 0x84, 2, 0, 2, 0]);
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    let s = storage_interface(&c).expect("a bulk-only interface");
    assert_eq!(s.number, 0);
    assert_eq!(s.bulk_in, Endpoint { address: 0x81, max_packet: 1024, burst: 15, interval: 0 });
    assert_eq!(s.bulk_out, Endpoint { address: 0x02, max_packet: 1024, burst: 3, interval: 0 });
    assert_eq!((s.bulk_in.dci(), s.bulk_out.dci()), (3, 4));
}

/// **Bulk-only needs both directions**: an interface with a bulk IN alone is none, and a SCSI
/// interface of another protocol (UAS, `0x62`) is not this one.
#[test]
fn a_storage_interface_without_both_bulk_endpoints_is_none() {
    let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 1, 8, 6, 0x50, 0]);
    c.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    assert_eq!(storage_interface(&c), None);
    c[9 + 7] = 0x62;
    c.extend_from_slice(&[7, 5, 0x02, 2, 64, 0, 0]);
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    assert_eq!(storage_interface(&c), None, "UAS is not bulk-only");
    c[9 + 7] = 0x50;
    assert!(storage_interface(&c).is_some());
}

/// **An interface's endpoints are its own**: a bulk-only interface with a bulk IN
/// alone does not take the next interface's bulk OUT to make up the pair.
#[test]
fn a_storage_interface_takes_no_endpoint_of_the_next_interface() {
    let mut c = vec![9, 2, 0, 0, 2, 1, 0, 0xA0, 50];
    c.extend_from_slice(&[9, 4, 0, 0, 1, 8, 6, 0x50, 0]); // bulk-only, its bulk IN alone
    c.extend_from_slice(&[7, 5, 0x81, 2, 64, 0, 0]);
    c.extend_from_slice(&[9, 4, 1, 0, 1, 0xFF, 0, 0, 0]); // vendor, with a bulk OUT
    c.extend_from_slice(&[7, 5, 0x02, 2, 64, 0, 0]);
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    assert_eq!(storage_interface(&c), None);
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

/// **A string descriptor shorter than its own header is no string**: a length byte of 0 or 1
/// names no text, and the bytes after it are the driver's buffer, not the descriptor's.
#[test]
fn a_string_descriptor_shorter_than_its_header_is_refused() {
    let mut out = [0u8; 72];
    for length in [0u8, 1] {
        let mut b = vec![0u8; 255];
        b[0] = length;
        b[1] = kind::STRING;
        assert_eq!(string_into(&b, &mut out), None, "a length byte of {length}");
    }
    assert_eq!(string_into(&[2, kind::STRING], &mut out), Some(0), "an empty string is a string");
}

/// **A device with no strings is named by its IDs**, four hex digits each, lower case as the
/// log prints them, with the leading zeros a vendor like `0627` needs.
#[test]
fn a_nameless_device_is_named_by_its_ids() {
    let mut out = [0xAAu8; 12];
    assert_eq!(ids_into(0x0627, 0x0001, &mut out), 9);
    assert_eq!(&out[..9], b"0627:0001");
    assert_eq!(ids_into(0xABCD, 0xF00D, &mut out), 9);
    assert_eq!(&out[..9], b"abcd:f00d");
    assert_eq!(out[9], 0xAA, "nothing past its nine bytes");
}

/// `walk`'s answer, or a failure if it has not ended within a few seconds: a walk that stops
/// advancing loops rather than fails.
fn ending<R: Send + 'static>(walk: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(walk()));
    rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the walk did not end")
}

/// A configuration of `descs` after its own nine bytes, its total their length.
fn config(descs: &[&[u8]]) -> Vec<u8> {
    let mut c = vec![9, 2, 0, 0, 1, 1, 0, 0xA0, 50];
    descs.iter().for_each(|d| c.extend_from_slice(d));
    let total = c.len() as u16;
    c[2..4].copy_from_slice(&total.to_le_bytes());
    c
}

/// **A descriptor shorter than its kind is passed over by every reader**: each would read its
/// fields past its end, a panic from a device's bytes. Each is last, so its slice ends with it.
#[test]
fn a_descriptor_shorter_than_its_kind_is_passed_over() {
    const HID_IF: [u8; 9] = [9, 4, 0, 0, 1, 3, 1, 1, 0];
    const STORAGE_IF: [u8; 9] = [9, 4, 0, 0, 2, 8, 6, 0x50, 0];
    const BULK_IN: [u8; 7] = [7, 5, 0x81, 2, 0, 2, 0];
    let short_if: &[u8] = &[4, 4, 0, 0];
    assert_eq!(interfaces(&config(&[short_if])).count(), 0, "an interface for `interfaces`");
    assert_eq!(hid_interfaces(&config(&[short_if])).count(), 0, "an interface for HID");
    let hid = hid_interfaces(&config(&[&HID_IF, &[6, 0x21, 0x11, 0x01, 0, 1]])).next().unwrap();
    assert_eq!(hid.report_len, 0, "a HID descriptor");
    let hid = hid_interfaces(&config(&[&HID_IF, &[4, 5, 0x81, 3]])).next().unwrap();
    assert_eq!(hid.endpoint, None, "an endpoint for HID");
    let hid = hid_interfaces(&config(&[&HID_IF, &[7, 5, 0x81, 3, 8, 0, 10], &[2, 0x30]])).next().unwrap();
    assert_eq!(hid.endpoint.unwrap().burst, 0, "a companion for HID");
    assert_eq!(storage_interface(&config(&[short_if])), None, "an interface for storage");
    assert_eq!(storage_interface(&config(&[&STORAGE_IF, &[4, 5, 0x81, 2]])), None, "an endpoint for storage");
    assert_eq!(storage_interface(&config(&[&STORAGE_IF, &BULK_IN, &[2, 0x30]])), None, "a companion for storage");
}

/// **A descriptor of no length, or one byte, ends the walk**: read as one, its kind is past its end.
#[test]
fn a_descriptor_of_no_length_or_one_byte_ends_the_walk() {
    for len in [0u8, 1] {
        let c = config(&[&[9, 4, 0, 0, 1, 3, 1, 1, 0], &[len, 5, 0x81, 3, 8, 0, 10]]);
        let found = ending(move || hid_interfaces(&c).map(|h| h.endpoint).collect::<Vec<_>>());
        assert_eq!(found, [None], "a descriptor of {len} bytes");
    }
}

/// **A configuration descriptor shorter than its nine bytes is none**, whatever its total says.
#[test]
fn a_configuration_shorter_than_its_nine_bytes_is_none() {
    let mut c = keyboard_config();
    c[0] = 8;
    assert_eq!((configuration_total(&c), configuration_value(&c)), (None, None));
    c[0] = 9;
    assert_eq!((configuration_total(&c), configuration_value(&c)), (Some(34), Some(1)));
}

/// **An interface's first interrupt-IN endpoint is its own**: a second does not replace it.
#[test]
fn an_interfaces_first_interrupt_in_endpoint_is_the_one_taken() {
    let c = config(&[&[9, 4, 0, 0, 2, 3, 1, 1, 0], &[7, 5, 0x81, 3, 8, 0, 10], &[7, 5, 0x82, 3, 4, 0, 1]]);
    let first = Endpoint { address: 0x81, max_packet: 8, burst: 0, interval: 10 };
    assert_eq!(hid_interfaces(&c).next().unwrap().endpoint, Some(first));
}
