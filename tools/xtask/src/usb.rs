//! QEMU's USB devices: the controller every gate uses, and `test-qemu`'s hot-plug checks.

use crate::*;

/// The line the hub thread ends its first round with, after which a device is plugged in.
const USB_FIRST_ROUND: &str = "usb: first round: ";

/// The hot-plugged keyboard's arrival: a high-speed boot keyboard, after the first round.
const HOT_KEYBOARD: &[&str] = &["usb: port ", ": 0627:0001 class 03/01/01, high-speed, \"QEMU USB Keyboard"];

/// The mouse swapped in for it on the same port: a high-speed boot mouse.
const HOT_MOUSE: &[&str] = &["usb: port ", ": 0627:0001 class 03/01/02, high-speed, \"QEMU USB Mouse"];

/// **USB devices plugged in after boot, swapped, and pulled out**: once the guest logs its first
/// round a keyboard is added over QMP; once it logs the keyboard **bound**, **the machine is
/// paused**, the keyboard removed and a mouse added on the same port, so the guest finds a connect
/// change on a port that still has a device and must take it as a departure and an arrival; once it
/// logs the mouse bound, the mouse is removed and the QMP connection closed (QEMU serves one
/// client, and the deadline's dump needs it). Bound, not only enumerated: a device removed during
/// binding is rightly not bound. Asserted by [`check_hot_plug`]; the device lands on a root port
/// only because `test-qemu`'s controller has eight connectors.
#[derive(Default)]
pub(crate) struct HotPlug {
    stage: u8,
    from: usize,
    pub(crate) qmp: Option<Qmp>,
    /// Why the hot-plug stopped early, reported after the run rather than ending it unexplained.
    failed: Option<String>,
}

impl HotPlug {
    /// Act on what the guest has said so far. A failure is kept for [`check_hot_plug`], and the
    /// connection closed.
    pub(crate) fn step(&mut self, transcript: &[u8], qmp_sock: &Path) {
        if self.stage >= 3 {
            return;
        }
        if let Err(e) = self.advance(transcript, qmp_sock) {
            self.failed = Some(e.to_string());
            self.stage = 3;
            self.qmp = None;
        }
    }

    fn advance(&mut self, transcript: &[u8], qmp_sock: &Path) -> R<()> {
        let text = String::from_utf8_lossy(transcript);
        let bound = |from: usize, fragments: &[&str], what: &str| {
            let rest = &text[from..];
            let Some(line) = rest.lines().find(|l| fragments.iter().all(|f| l.contains(f))) else {
                return false;
            };
            let port = line.split("usb: port ").nth(1).and_then(|r| r.split(':').next()).unwrap_or("");
            let at = rest.find(line).unwrap_or(0) + line.len();
            rest[at..].contains(&format!("usb: port {port}: {what} at /dev/input/raw/"))
        };
        match self.stage {
            0 => {
                let Some(at) = text.find(USB_FIRST_ROUND) else { return Ok(()) };
                let qmp = self.qmp.insert(Qmp::connect(qmp_sock)?);
                qmp.execute(
                    r#"{"execute":"device_add","arguments":{"driver":"usb-kbd","id":"hotkbd","bus":"xhci.0"}}"#,
                )?;
                self.from = at;
                self.stage = 1;
            }
            1 if bound(self.from, HOT_KEYBOARD, "keyboard") => {
                let qmp = self.qmp.as_mut().ok_or("no QMP connection")?;
                let usb = qmp.hmp("info usb")?;
                let port = usb
                    .lines()
                    .find(|l| l.contains("ID: hotkbd"))
                    .and_then(|l| l.split("Port ").nth(1))
                    .and_then(|r| r.split(',').next())
                    .ok_or_else(|| format!("QEMU's `info usb` does not place hotkbd: {usb:?}"))?
                    .to_string();
                qmp.execute(r#"{"execute":"stop"}"#)?;
                qmp.execute(r#"{"execute":"device_del","arguments":{"id":"hotkbd"}}"#)?;
                qmp.execute(&format!(
                    r#"{{"execute":"device_add","arguments":{{"driver":"usb-mouse","id":"hotmouse","bus":"xhci.0","port":"{port}"}}}}"#
                ))?;
                qmp.execute(r#"{"execute":"cont"}"#)?;
                self.from = text.len();
                self.stage = 2;
            }
            2 if bound(self.from, HOT_MOUSE, "mouse") => {
                let qmp = self.qmp.as_mut().ok_or("no QMP connection")?;
                qmp.execute(r#"{"execute":"device_del","arguments":{"id":"hotmouse"}}"#)?;
                self.qmp = None;
                self.stage = 3;
            }
            _ => {}
        }
        Ok(())
    }
}

/// **The hot-plug's lines**, in order after the first round, all on one port: the keyboard's
/// arrival and binding, its departure when the mouse is swapped in, the mouse's arrival and
/// binding, its departure; each departure with its slot disabled. **And the first round before
/// userspace**, which the facts alone cannot show.
pub(crate) fn check_hot_plug(transcript: &[u8], hot: &HotPlug) -> R<()> {
    if let Some(why) = &hot.failed {
        return Err(format!("the hot-plug could not be driven over QMP: {why}").into());
    }
    let text = String::from_utf8_lossy(transcript);
    let Some(at) = text.find(USB_FIRST_ROUND) else {
        return Err("the USB hub thread never finished its first round".into());
    };
    match text.find("init: spawned init (pid 1)") {
        Some(init) if init > at => {}
        Some(_) => {
            return Err("userspace started before the USB hub thread's first round ended: the boot did \
                        not wait for it, so a device present at boot can miss the registry's replay"
                .into());
        }
        None => return Err("the kernel never said it spawned init".into()),
    }
    let after = &text[at..];
    let Some(line) = after.lines().find(|l| HOT_KEYBOARD.iter().all(|f| l.contains(f))) else {
        return Err("a keyboard plugged in after the first round never arrived: no line enumerating it".into());
    };
    let port = line
        .split("usb: port ")
        .nth(1)
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {line:?}"))?;
    let gone = format!("usb: port {port}: disconnected; slot ");
    let mouse = format!("usb: port {port}: 0627:0001 class 03/01/02, high-speed, \"QEMU USB Mouse");
    let kbd_bound = format!("usb: port {port}: keyboard at /dev/input/raw/");
    let mouse_bound = format!("usb: port {port}: mouse at /dev/input/raw/");
    let mut rest = &after[after.find(line).unwrap_or(0) + line.len()..];
    for (what, want) in [
        ("the keyboard bound", kbd_bound.as_str()),
        ("the keyboard's departure when the mouse was swapped in", gone.as_str()),
        ("the mouse's arrival on the same port", mouse.as_str()),
        ("the mouse bound", mouse_bound.as_str()),
        ("the mouse's departure", gone.as_str()),
    ] {
        let Some(at) = rest.find(want) else {
            let why = if want.contains("/dev/input/raw/") {
                "a HID device enumerated after boot that was not bound"
            } else {
                "a replug the hub thread does not take as a departure and an arrival keeps the old device"
            };
            return Err(
                format!("after the keyboard's arrival on port {port}, no line for {what} ({want:?}): {why}").into()
            );
        };
        let found = rest[at..].lines().next().unwrap_or("");
        if want == gone && !found.contains("disabled") {
            return Err(format!("port {port}'s departure did not disable its slot: {found:?}").into());
        }
        rest = &rest[at + want.len()..];
    }
    println!(
        "xtask: a keyboard plugged in after boot, swapped for a mouse while the machine was paused, \
         and pulled out — each arrival, binding and departure on port {port}, each slot disabled ✓"
    );
    Ok(())
}

const DEVICES_USB_LISTED: &str = " of them usb-<id>, and ";

/// **`device-mgr` lists every device the first round found.** `boot-probe` can hold the manager
/// only to the registry's first records, as the hot-plug may land on either side of its read; the
/// first round ended before `init`, so the count of `usb-<id>.tsm` files must be at least the
/// round's.
pub(crate) fn check_usb_listed(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    let round = text
        .lines()
        .find_map(|l| l.split(USB_FIRST_ROUND).nth(1)?.split(' ').next()?.parse::<usize>().ok())
        .ok_or("no first round's count of USB devices")?;
    let listed = text
        .lines()
        .filter(|l| l.contains("boot-probe: devices: "))
        .find_map(|l| l.split(DEVICES_USB_LISTED).next()?.rsplit(' ').next()?.parse::<usize>().ok())
        .ok_or("no boot-probe devices line counting the USB devices the manager lists")?;
    if listed < round {
        return Err(format!(
            "device-mgr lists {listed} USB device(s) as usb-<id>.tsm, but the first round found {round} \
             before init: the manager read the registry after them, so it dropped some"
        )
        .into());
    }
    println!("xtask: device-mgr lists {listed} USB device(s) as usb-<id>.tsm, the first round's {round} among them ✓");
    // `input-server` is handed the i8042's two and every input device the first round bound, or
    // more: a keyboard hot-plugged and bound before the manager's read is handed over too.
    let at_round = text.find(USB_FIRST_ROUND).unwrap_or(0);
    let bound =
        text[..at_round].lines().filter(|l| l.starts_with("usb: port ") && l.contains(" at /dev/input/raw/")).count();
    let handed = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("input-server: ")?.split(" device(s) from the device manager").next()?.parse::<usize>().ok()
        })
        .ok_or("no input-server line counting the devices it was handed")?;
    if handed < 2 + bound {
        return Err(format!(
            "input-server was handed {handed} device(s), fewer than the i8042's two and the {bound} the \
             first round bound"
        )
        .into());
    }
    println!(
        "xtask: input-server was handed {handed} input device(s), the i8042's two and the first round's {bound} \
         among them ✓"
    );
    Ok(())
}

/// **QEMU's xHCI, configured as the laptop's is: MSI, and no MSI-X**, the one mechanism its Sunrise
/// Point-LP controller has, and so the driver's. **`nec-usb-xhci`, not `qemu-xhci`**: `qemu-xhci`
/// offers MSI-X alone on q35, and only QEMU 11 lets that be changed; CI's QEMU 8.2 has no `msi`
/// property. The NEC model takes `msi` and `msix` in both, and numbers its SuperSpeed ports first.
pub(crate) const XHCI_DEVICE: &str = "nec-usb-xhci,id=xhci,msi=on,msix=off";

/// **A machine whose only keyboard and mouse are USB**: q35 with its i8042 off (the FADT then says
/// the 8042 is absent) and a `usb-kbd` and `usb-mouse` on the gates' controller, added here unless
/// the gate has one. A second `-machine` merges with `qemu_base_args`'s in QEMU 8.2 and 11.
pub(crate) fn usb_input_args(cmd: &mut Command, add_controller: bool) {
    cmd.arg("-machine").arg("i8042=off");
    if add_controller {
        cmd.arg("-device").arg(XHCI_DEVICE);
    }
    cmd.arg("-device").arg("usb-kbd,bus=xhci.0,id=usbkbd").arg("-device").arg("usb-mouse,bus=xhci.0");
}

/// **`boot-probe`'s registry test has reported**, waited for before `check-input --usb`'s keyboards
/// come and go, as an unplug in the middle of it fails it. Not the whole verdict, which comes long
/// after the test client gives up. Read from the transcript, not with `expect`.
pub(crate) fn await_registry_probe(s: &Session) -> R<()> {
    let start = std::time::Instant::now();
    let reported = |l: &str| {
        l.starts_with("boot-probe: registry: ")
            && (l.ends_with("keyboard and mouse at their raw indices ok") || l.ends_with(" FAIL"))
    };
    while !s.transcript().lines().any(|l| reported(l.trim_end())) {
        if start.elapsed() > std::time::Duration::from_secs(90) {
            return Err("boot-probe's registry test did not report within 90 s".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("  ok: boot-probe's registry test reported, {} ms after the gate asked", start.elapsed().as_millis());
    Ok(())
}

/// **`check-input --usb`'s keyboards coming and going** while the test client's window holds the
/// keyboard: the boot keyboard unplugged; one plugged in, then the only one; **a key held down on
/// it, then it is unplugged, and the window sees the release** (QEMU sends nothing for a key held
/// when its device goes, `hw/input/hid.c`, so the release is the guest's); one plugged in for the
/// rest of the gate. Each line is matched from the action that causes it on, whichever process
/// prints first: the input server's, the manager's and the kernel's lines answer one unplug.
pub(crate) fn usb_keyboards_come_and_go(qmp: &mut Qmp, session: &mut Session) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    let first_word = |rest: String| rest.split(' ').next().unwrap_or("").to_string();
    let boot_id = session
        .transcript()
        .lines()
        .find_map(|l| l.strip_prefix("input-server: reading keyboard ")?.split(' ').next().map(str::to_string))
        .ok_or("the input server read no keyboard at boot")?;

    let from = session.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"usbkbd"}}"#)?;
    session.line_since(from, &format!("input-server: keyboard {boot_id} left"), any, secs(30))?;
    session.line_since(from, "departed, sent to input", any, secs(30))?;
    session.line_since(from, &format!("input-server: device {boot_id} departed from slot "), any, secs(30))?;
    println!("  ok: the boot keyboard unplugged: it left, the manager said so, and its slot was retired");

    let from = session.transcript().len();
    qmp.execute(r#"{"execute":"device_add","arguments":{"driver":"usb-kbd","id":"hotkbd","bus":"xhci.0"}}"#)?;
    let raw = session.line_since(from, ": keyboard at /dev/input/raw/", any, secs(30))?;
    let index = raw.split(',').next().unwrap_or("").to_string();
    session.line_since(from, &format!("device-mgr: input-{index} arrived, sent to input"), any, secs(30))?;
    let hot_id = first_word(session.line_since(from, "input-server: reading keyboard ", any, secs(30))?);
    session.line_since(from, ": keyboard lights acknowledged", any, secs(30))?;
    qmp.send_key("b", true)?;
    session.expect("input-testclient: win key code=48 down=1")?;
    qmp.send_key("b", false)?;
    session.expect("input-testclient: win key code=48 down=0")?;
    println!("  ok: a keyboard plugged in was handed over, read, given its lights, and typed on");

    qmp.send_key("x", true)?;
    session.expect("input-testclient: win key code=45 down=1")?;
    let from = session.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"hotkbd"}}"#)?;
    session.line_since(from, "input-testclient: win key code=45 down=0", any, secs(30))?;
    session.line_since(from, &format!("input-server: keyboard {hot_id} left"), any, secs(30))?;
    session.line_since(from, &format!("input-server: device {hot_id} departed from slot "), any, secs(30))?;
    println!("  ok: a key held when its keyboard was unplugged was released, by the guest");

    let from = session.transcript().len();
    qmp.execute(r#"{"execute":"device_add","arguments":{"driver":"usb-kbd","id":"lastkbd","bus":"xhci.0"}}"#)?;
    session.line_since(from, "input-server: reading keyboard ", any, secs(30))?;
    Ok(())
}

/// **What `check-input --usb`'s departures leave in the transcript**: one `left` line per unplugged
/// keyboard, and no read of a node after its device has gone: neither `read completed with an
/// error` (a read completing `PeerClosed`) nor `read submit FAILED` (one refused).
pub(crate) fn check_departures_quiet(transcript: &str) -> R<()> {
    let left = transcript.lines().filter(|l| l.starts_with("input-server: keyboard ") && l.ends_with(" left")).count();
    if left != 2 {
        return Err(format!("input gate FAILED: {left} `left` line(s) for the two keyboards unplugged").into());
    }
    for bad in ["input-server: read completed with an error", "input-server: read submit FAILED"] {
        if transcript.contains(bad) {
            return Err(format!(
                "input gate FAILED: \"{bad}\" — the input server went on reading a node whose device had \
                 gone, where `PeerClosed` should have ended it"
            )
            .into());
        }
    }
    println!("  ok: each departure said `left` once, and no node was read after its device had gone");
    Ok(())
}

/// What `check-report --usb`'s report says beside [`EMULATED_MACHINE_FACTS`] and [`REPORT_FACTS`]:
/// the FADT's 8042 absent; the keyboard and mouse bound at `/dev/input/raw/0` and `/1`; a first
/// round of three devices.
pub(crate) const USB_INPUT_FACTS: &[&[&str]] = &[
    &["fadt: boot arch 0x0; 8042 absent"],
    &["ps2: no i8042 devices answered"],
    &["usb: port 6: keyboard at /dev/input/raw/0"],
    &["usb: port 7: mouse at /dev/input/raw/1"],
    &["usb: first round: 3 device(s) in "],
];

/// **The USB devices `test-qemu` boots with**: an xHCI controller with a device at each of three
/// speeds (`usb-kbd` high, `usb-mouse` with `usb_version=1` full, `usb-storage` SuperSpeed; no QEMU
/// device takes low speed), a hub, and **`usb-ccid`**, which nothing matches and whose default
/// endpoint is not its speed's default (full speed, 64-byte packets), so its enumeration takes
/// Evaluate Context. **`p2=8,p3=8` gives eight connectors**; the default four fill, and a later
/// device would land behind the hub. The stick is an MBR naming one FAT partition. **A drive past 2
/// TiB** ([`PAST_TWO_TIB_IMAGE`], sparse, last so every other device keeps its port): `boot-probe`
/// writes a sector past 2 TiB and [`check_past_two_tib`] finds it, where a block number cut to 32
/// bits would be 2 TiB lower.
pub(crate) fn test_qemu_usb_args(cmd: &mut Command) -> R<()> {
    let stick = build_cache().join("test-qemu-usb-stick.img");
    mbr_fat_stick(&stick, 16, "NXSTICK")?;
    let big = build_cache().join(PAST_TWO_TIB_IMAGE);
    let _ = fs::remove_file(&big);
    fs::File::create(&big)
        .and_then(|f| f.set_len(3 << 40))
        .map_err(|e| format!("a 3 TiB sparse image at {}: {e}", big.display()))?;
    cmd.arg("-device")
        .arg(format!("{XHCI_DEVICE},p2=8,p3=8"))
        .arg("-device")
        .arg("usb-kbd,bus=xhci.0")
        .arg("-device")
        .arg("usb-mouse,bus=xhci.0,usb_version=1")
        .arg("-drive")
        .arg(format!("if=none,id=usbstick,format=raw,file={}", stick.display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=usbstick")
        .arg("-device")
        .arg("usb-hub,bus=xhci.0")
        .arg("-device")
        .arg("usb-ccid,bus=xhci.0")
        .arg("-drive")
        .arg(format!("if=none,id=usbbig,format=raw,file={}", big.display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=usbbig");
    Ok(())
}
