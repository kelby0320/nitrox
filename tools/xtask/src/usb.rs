//! QEMU's USB devices: the controller every gate uses, and `test-qemu`'s hot-plug checks.

use crate::*;

/// The line the hub thread ends its first round with, after which a device is plugged in.
const USB_FIRST_ROUND: &str = "usb: first round: ";

/// The hot-plugged keyboard's arrival: a high-speed boot keyboard, after the first round.
const HOT_KEYBOARD: &[&str] = &["usb: port ", ": 0627:0001 class 03/01/01, high-speed, \"QEMU USB Keyboard"];

/// The mouse swapped in for it on the same port: a high-speed boot mouse.
const HOT_MOUSE: &[&str] = &["usb: port ", ": 0627:0001 class 03/01/02, high-speed, \"QEMU USB Mouse"];

/// **USB devices plugged in after boot, swapped, and pulled out** (Phase 6 Part A.2): coldplug is
/// the first round of hot-plug, and these are the rounds after it.
/// 1. Once the guest logs its first round, a keyboard is added over QMP.
/// 2. Once it logs the keyboard's arrival **and its binding**, **the machine is paused**, the
///    keyboard removed and a
///    mouse added on the same QEMU port, and the machine resumed. The guest then finds a connect
///    change on a port that still has a device — both changes landed before it looked — which is
///    a replug it must take as a departure and an arrival. Paused, so the two always land together:
///    a hub thread that keeps the old device fails it every time (PR #355 review).
/// 3. Once it logs the mouse's arrival and its binding, the mouse is removed, and the QMP
///    connection closed — QEMU serves one QMP client at a time, and the deadline's dump of the
///    guest needs it.
///
/// **Bound, not only enumerated, before each is taken away** (PR #358's gate run). Binding asks the
/// device for its protocol and idle rate after the arrival line, and a device taken away meanwhile
/// leaves that request unanswered: the default endpoint is in doubt, and the device is rightly not
/// bound. Acting on the arrival line made that a race with the guest's speed, which a binding that
/// ignored a failed `SET_PROTOCOL` used to hide.
///
/// The lines are asserted after the run by [`check_hot_plug`]. It lands on a root port only because
/// `test-qemu`'s controller has eight connectors: with QEMU's four, the boot's devices fill them and
/// the keyboard goes behind the hub, where the driver does not look (PR #353 review).
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
        // Whether the device `fragments` names has arrived after `from`, and been bound as `what`.
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
/// arrival; its departure when the mouse is swapped in; the mouse's arrival; the mouse's departure.
/// Each departure with its slot disabled. And **the first round before userspace**: the boot waits
/// for it, which the facts alone cannot show, since its lines would be there either way, later.
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
    // **Each bound as it arrives** (Part B.2), and unbound when it leaves, which the departure's
    // slot being disabled follows.
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

/// The devices line's count of USB devices `device-mgr` lists, the part before it.
const DEVICES_USB_LISTED: &str = " of them usb-<id>, and ";

/// **`device-mgr` lists every device the first round found** (Phase 6 Part A.3; PR #356 review).
/// `boot-probe` can hold the manager only to the registry's first records, since the hot-plug may
/// land on either side of the manager's one read — and a manager that dropped every USB record
/// looks, to it, like one that read before any registered. The host knows better: the first round
/// ended before `init` ([`check_hot_plug`] asserts it), so the manager read at least that round's
/// devices, and its count of `usb-<id>.tsm` files must be at least the round's.
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
    // **And `input-server` was handed the keyboards and mice the first round bound** (Part B.2),
    // beside the i8042's two, which `test-qemu`'s machine has. At least: a hot-plugged keyboard bound
    // before the manager's read is handed over too.
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

/// **QEMU's xHCI, configured as the laptop's is: MSI, and no MSI-X** (Phase 6 Part A). The laptop's
/// Sunrise Point-LP controller has plain MSI with eight vectors and no MSI-X, and the driver takes
/// the one interrupt mechanism that machine has, so every gate's controller is given it.
///
/// **`nec-usb-xhci`, not `qemu-xhci`**: the same xHCI core with the NEC µPD720200's identity
/// (`1033:0194`). `qemu-xhci` offers MSI-X alone on q35, and only QEMU 11 lets that be changed —
/// QEMU 8.2, which CI runs, hard-codes it, with no `msi` property, so the first version of this
/// constant failed every CI boot that attached it (PR #354). The NEC model takes `msi` and `msix`
/// in both, and numbers its SuperSpeed ports first, as `qemu-xhci` does.
pub(crate) const XHCI_DEVICE: &str = "nec-usb-xhci,id=xhci,msi=on,msix=off";

/// **A machine whose only keyboard and mouse are USB** (Phase 6 Part B): q35 with its i8042 off —
/// the FADT then says the 8042 is absent, and the PS/2 driver publishes nothing — and a `usb-kbd`
/// and a `usb-mouse` on the gates' controller, which is added here unless the gate has one
/// already (`check-report`'s stick is on it). QMP's input reaches them because nothing else takes
/// it. A second `-machine` merges with `qemu_base_args`'s, as QEMU 8.2 and 11 both do.
pub(crate) fn usb_input_args(cmd: &mut Command, add_controller: bool) {
    cmd.arg("-machine").arg("i8042=off");
    if add_controller {
        cmd.arg("-device").arg(XHCI_DEVICE);
    }
    // `id=usbkbd`, so `check-input --usb` can unplug it (Phase 6 Part C).
    cmd.arg("-device").arg("usb-kbd,bus=xhci.0,id=usbkbd").arg("-device").arg("usb-mouse,bus=xhci.0");
}

/// **`boot-probe`'s registry test has reported**, waited for before `check-input --usb`'s keyboards
/// come and go (PR #363's gate run). The test reads the device table once and looks up every present
/// record's paths, and an unplug in the middle of it fails it: a kept `--kvm` transcript had it
/// reading the table between the gate's unplug of one keyboard and its plug of the next, and finding
/// none. It landed among the swaps in every run kept, either side of the last plug.
///
/// **Not `boot-probe`'s whole verdict**: the test client gives up after twenty seconds without an
/// event (`IDLE_LIMIT_NS` in `inputclient.rs`), and the verdict comes long after the registry test.
/// Read from the transcript, not with `expect`, which would consume what the gate waits for next.
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

/// **`check-input --usb`'s keyboards coming and going** (Phase 6 Part C), while the test client's
/// window holds the keyboard and logs every key it receives:
/// 1. **the boot keyboard unplugged**: the input server sees it leave, the manager says it departed,
///    and its slot is retired;
/// 2. **a keyboard plugged in**, then the only one, so every key after is its: the manager hands it
///    over, the input server reads it and writes its lights, and a key typed on it reaches the window;
/// 3. **a key held down on it, then the keyboard unplugged: the window sees the release.** QEMU
///    sends nothing for a key held when its device goes (`hw/input/hid.c`, `hid_free`), so the
///    release is the guest's — the driver letting go of what its last report held;
/// 4. a keyboard plugged in for the rest of the gate.
///
/// Each line is matched from the action that causes it on, whichever process prints first: the
/// input server's "left", the manager's departure and the kernel's lines answer one unplug.
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

    // `x` is keycode 45.
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

/// **What `check-input --usb`'s departures leave in the transcript** (Phase 6 Part C): one `left`
/// line for each of its two unplugged keyboards, and neither line an input server prints for a node
/// it goes on reading after its device has gone — `read completed with an error` for a read that
/// completes `PeerClosed`, and `read submit FAILED` at each pass for one refused. Asserting on the
/// failed submit alone could not see the first (PR #360 review).
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

/// What `check-report --usb`'s report says beside [`EMULATED_MACHINE_FACTS`] and [`REPORT_FACTS`]
/// (Phase 6 Part B): the FADT's 8042 absent, where the first has it present; the keyboard and mouse
/// bound at `/dev/input/raw/0` and `/1`, since no i8042 took them, on the USB 2 ports after the
/// stick's (read off the first run); and a first round of three devices, where the second has the
/// stick's one.
pub(crate) const USB_INPUT_FACTS: &[&[&str]] = &[
    &["fadt: boot arch 0x0; 8042 absent"],
    &["ps2: no i8042 devices answered"],
    &["usb: port 6: keyboard at /dev/input/raw/0"],
    &["usb: port 7: mouse at /dev/input/raw/1"],
    &["usb: first round: 3 device(s) in "],
];

/// **The USB devices `test-qemu` boots with** (Phase 6 Part A): an xHCI controller with a device
/// at each of three speeds, a hub, and one nothing matches.
/// - `usb-kbd` attaches at high speed, `usb-mouse` with `usb_version=1` at full speed, and
///   `usb-storage` at SuperSpeed (QEMU 11.0.2's `info usb`). No QEMU USB device takes a low-speed
///   setting.
/// - `usb-hub` matches as a hub, which is listed and left alone: external hubs are outside Phase 6.
/// - **`usb-ccid`** is the device nothing matches, a smart-card reader, and **the one whose default
///   endpoint is not its speed's default**: full speed with 64-byte packets, so its enumeration
///   takes Evaluate Context. The other four keep the default, and QEMU's controller ignores the
///   packet size anyway, so without it nothing would run that path (Part A.2, read off a boot).
/// - **`p2=8,p3=8` gives eight connectors.** The defaults give four, which these four fill, and a
///   device plugged in later would land behind the hub, where the driver does not look (PR #353
///   review).
/// - **The stick is a stick from a shop** (Phase 6 Part D): an MBR naming one FAT partition, which
///   the first round reads and the storage service reports and leaves unmounted, until Part E.
/// - **And a drive past 2 TiB** (Phase 6 Part G): [`PAST_TWO_TIB_IMAGE`], 3 TiB, blank and sparse,
///   last, so every other device keeps its port. The kernel reads it with the sixteen-byte
///   commands, `boot-probe` writes a sector past 2 TiB through its node, and
///   [`check_past_two_tib`] finds that sector in the image, where a block number cut to 32 bits
///   would have put it 2 TiB lower.
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
