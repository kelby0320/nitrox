//! `check-input`: a key and a click injected, each seen to reach a userspace client.

use crate::*;

/// `cargo xtask check-input` — inject a keystroke and a click, and check they arrive.
///
/// Through the whole path: i8042 -> driver ring -> raw node -> `input-server` -> merge -> consumer
/// channel -> guest, which prints `listening` first: an event before the consumer channel exists
/// has nowhere to go.
pub(crate) fn cmd_check_input(accel: Accel, no_ps2_irq: bool, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    // `--no-ps2-irq` turns the i8042's interrupts off so the `ps2::poll` sweep recovers every byte;
    // on healthy hardware the sweep never fires, so deleting it passes every other gate.
    cmd_image(if no_ps2_irq { BuildMode::TestHarnessNoPs2Irq } else { BuildMode::TestHarness })?;
    let ovmf = locate_ovmf()?;
    let qmp_sock = build_cache().join("check-input.qmp");
    fs::create_dir_all(build_cache())?;
    let _ = fs::remove_file(&qmp_sock);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if usb {
        usb_input_args(&mut cmd, true);
    }

    println!("xtask: input gate — booting and injecting…\n");
    let mut session = Session::spawn(cmd, "check-input")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    qmp.screen = Some(size);

    // The compositor consumes the same merged stream, attached before the client's `listening`:
    // this proves it attached, not that a key reached a window.
    session.expect("compositor: input connected")?;

    session.expect("input-testclient: listening")?;

    // Wait for `ui-testclient` before injecting: its leak probe churns 128 windows, so a key
    // injected meanwhile is routed to a window about to be destroyed.
    session.expect("ui-testclient: PASSED")?;

    // Motion survives a consumer that stops reading: a relative delta is the pointer's position, so
    // an undeliverable batch must be carried forward, or every consumer is permanently offset. The
    // client announces it stopped reading and sleeps, so the ring fills at any speed. First,
    // because a sum cannot be delimited (an earlier flood still trickles in under `--no-ps2-irq`);
    // started by a keystroke (`F2`, unbound), not the client's clock, which would race the `PASSED`
    // wait.
    press(&mut qmp, "f2")?;
    session.expect("input-testclient: input stalled")?;
    // Thirty overruns the sleeping client's sixteen-message ring by fourteen.
    const BURST: i32 = 30;
    const BDX: i32 = 7;
    const BDY: i32 = 3;
    // Paced: the overrun comes from the stalled consumer. Unpaced, the host loses packets: with
    // `--no-ps2-irq` nothing reads the controller until the 10 ms sweep and QEMU's PS/2 queue holds
    // sixteen bytes, so a fast burst overflows it, and QEMU holds the overflow until the next
    // injected event, which a burst's last has none after.
    for _ in 0..BURST {
        qmp.send_motion(BDX, BDY)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    session.expect("input-testclient: motion sum ")?;
    let line = session.rest_of_line()?;
    let field = |name: &str| -> Option<i64> {
        line.split_whitespace()
            .filter_map(|f| f.split_once('='))
            .find(|(k, _)| *k == name)
            .and_then(|(_, v)| v.parse().ok())
    };
    let (dx, dy) = (field("dx"), field("dy"));
    let (announced, widest) = (field("announced"), field("widest"));
    let want = (Some((BURST * BDX) as i64), Some((BURST * BDY) as i64));
    if (dx, dy) != want {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: {BURST} motions of ({BDX}, {BDY}) injected while the consumer \
             was not reading summed to {dx:?}, {dy:?} rather than {:?}, {:?}. A relative delta \
             that does not arrive cannot be recovered — `input-server` must carry the motion of \
             an undeliverable batch forward and re-emit it, not count it as a `SYN_DROPPED` gap. \
             The line was: motion sum {line}",
            want.0, want.1
        )
        .into());
    }
    if announced != Some(0) {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: a motion-only burst announced {announced:?} lost records. \
             Nothing here is unrecoverable, so nothing should be announced: motion sum {line}"
        )
        .into());
    }
    // The precondition, checked: a run where the ring never filled passes the sum testing nothing.
    // A folded group is wider than any single delta; equal means no deferral happened.
    if widest.is_none_or(|w| w <= BDX as i64) {
        let _ = session.child.kill();
        return Err(format!(
            "input gate FAILED: the consumer's widest REL_X was {widest:?}, no more than the \
             {BDX} injected — so no batch was ever deferred and this gate did not exercise the \
             path it exists for. Raise BURST until the {}-slot consumer ring overruns, or check \
             that the client really stopped reading: motion sum {line}",
            "16"
        )
        .into());
    }
    println!("  ok: {BURST} motions across a stalled consumer arrived in full (widest fold {widest:?})");

    // `a` is keycode 30 (set-1 0x1E), in the decoder's identity range: a wrong `E0` or release bit
    // shows as a different code, not as silence.
    qmp.send_key("a", true)?;
    session.expect("input-testclient: ev kind=1 code=30 value=1")?;
    qmp.send_key("a", false)?;
    session.expect("input-testclient: ev kind=1 code=30 value=0")?;

    // `REL_Y` comes back +3, as injected: the PS/2 wire's positive Y is up and the driver negates,
    // a one-line mistake no host test can catch.
    qmp.send_motion(5, 3)?;
    session.expect("input-testclient: ev kind=2 code=0 value=5")?;
    session.expect("input-testclient: ev kind=2 code=1 value=3")?;

    // The wheel, device layer: the only check on the sign and the packet length (the IntelliMouse
    // knock makes four-byte packets; a decoder framing three misreads every later one). `REL_WHEEL`
    // is positive-down. Before the click, whose press ends the client's raw-stream phase.
    qmp.send_wheel(true)?;
    session.expect("input-testclient: ev kind=2 code=8 value=1")?;
    qmp.send_wheel(false)?;
    session.expect("input-testclient: ev kind=2 code=8 value=-1")?;

    qmp.send_button("left", true)?;
    session.expect("input-testclient: ev kind=1 code=272 value=1")?;
    // Released, as the window phase asserts on the button mask.
    qmp.send_button("left", false)?;

    // Through a window, created only now so the two phases cannot contend for its session ring.
    session.expect("input-testclient: window ready")?;

    // A deliberate flood, and a weak one: the client drains between injections, so no send is
    // refused (the phase-3 stall covers coalescing and retry). Kept as the historical regression.
    for _ in 0..12 {
        qmp.send_motion(-120, -120)?;
    }

    // The window was told it has the keyboard, before any input: a compositor announcing focus late
    // would pass an arrival-only check.
    session.expect("input-testclient: first win event=focus")?;
    session.expect("input-testclient: win focus has=1")?;

    // `b` (48) goes to the focused window, not the one under the cursor; `widget key` is `libui`'s
    // router, the one line that says the toolkit is wired to the compositor.
    qmp.send_key("b", true)?;
    session.expect("input-testclient: win key code=48 down=1")?;
    session.expect("input-testclient: widget key code=48 down=1")?;
    // Held, not released: the compositor repeats it; asserted before the release.
    session.expect("input-testclient: win repeat code=48")?;

    qmp.send_key("b", false)?;
    session.expect("input-testclient: win key code=48 down=0")?;

    // Keyboards that come and go, while the window holding the keyboard logs every key; USB only.
    if usb {
        await_registry_probe(&session)?;
        usb_keyboards_come_and_go(&mut qmp, &mut session)?;
    }

    // A registered chord is consumed, here and not after `PASSED`, where the client has stopped
    // logging and a delivered chord would be as silent as a consumed one. Twice, to end as started.
    for want in ["desktop 2", "desktop 1"] {
        qmp.send_key("meta_l", true)?;
        qmp.send_key("f1", true)?;
        // Held past the repeat delay (`REPEAT_DELAY_NS`, 400 ms): repeat arms from the physical
        // transition, so a consumed chord once delivered its key 400 ms later.
        std::thread::sleep(std::time::Duration::from_millis(700));
        qmp.send_key("f1", false)?;
        qmp.send_key("meta_l", false)?;
        // The positive half makes the absence at the end mean something.
        session.expect(&format!("ui-testclient: hotkey fired -> {want}"))?;
    }
    // A chord whose action moves nothing, held past the repeat delay: the two above empty the
    // desktop, so `fire_repeat` cancels itself; `Super+Space` only prints.
    qmp.send_key("meta_l", true)?;
    qmp.send_key("spc", true)?;
    std::thread::sleep(std::time::Duration::from_millis(700));
    qmp.send_key("spc", false)?;
    qmp.send_key("meta_l", false)?;
    session.expect("ui-testclient: quiet chord fired")?;

    // No wait for focus here: the compositor announces it between the two chords, so an expect
    // after them would scan past it.

    // The wheel through a window into a widget. `on_pointer` never receives a wheel, so a wrong
    // delivery would print `widget ptr kind=4`. Before the click.
    qmp.send_wheel(true)?;
    session.expect("input-testclient: win ptr kind=4 btn=0 buttons=0 wheel=1")?;
    session.expect("input-testclient: widget wheel dz=1")?;
    qmp.send_wheel(false)?;
    session.expect("input-testclient: widget wheel dz=-1")?;

    // Kind 1 is `POINTER_BUTTON`, routed by hit-testing, at widget-local coordinates.
    qmp.send_button("left", true)?;
    session.expect("input-testclient: win ptr kind=1 btn=272 buttons=1 wheel=0")?;
    session.expect("input-testclient: widget ptr kind=1")?;
    qmp.send_button("left", false)?;

    // Park-and-retry: the client stops draining and the flood overruns its 16-slot ring; the rest
    // parks in the compositor's outbox. The key must still arrive.
    session.expect("input-testclient: stalling")?;

    // Well past the 16-slot ring, so the outbox really holds messages.
    for _ in 0..40 {
        qmp.send_motion(3, 3)?;
    }
    // `c` arrives only if the compositor parked it and woke itself to re-send.
    qmp.send_key("c", true)?;

    session.expect("input-testclient: late key code=46")?;

    session.expect("input-testclient: PASSED")?;

    // A release QEMU is holding is delivered (the `lost-release` fix's guard). At the natural rate,
    // two runs in a hundred, deleting either flush passes every gate, so this builds the condition:
    // F9 makes the kernel hold the i8042 drain for 300 ms (`ps2-hold-gate`), and inside the hold a
    // three-step walk and a click fill QEMU's sixteen-byte queue so the release waits for the next
    // injected event. The condition is asserted first, then each flush that delivers it,
    // `expect_after_pointer`'s and `click_at`'s; release lines are matched by position, so the
    // clicks that set a round up cannot satisfy them. Each hold starts from an empty queue, which
    // `settle` ensures. Not under `--no-ps2-irq`, where the guest reads only on the tick, nor
    // `--usb`, which has no i8042.
    if no_ps2_irq || usb {
        println!("  note: the held-release guard runs only on the i8042, with its interrupts on");
    } else {
        let hold = |qmp: &mut Qmp| -> R<()> {
            qmp.send_key("f9", true)?;
            qmp.send_key("f9", false)?;
            // The key must reach the driver before the walk does.
            std::thread::sleep(std::time::Duration::from_millis(50));
            Ok(())
        };
        let settle = |qmp: &mut Qmp, session: &mut Session| -> R<()> {
            move_pointer_to(qmp, 200, 200)?;
            qmp.send_button("left", true)?;
            expect_after_pointer(qmp, session, "compositor: press at x=200 y=200")?;
            qmp.send_button("left", false)?;
            expect_after_pointer(qmp, session, "compositor: release at x=200 y=200")?;
            qmp.pointer = Some((200, 200));
            Ok(())
        };
        const HELD: &str = "compositor: release at x=500 y=200";
        // Somewhere known first, through `click_at`'s retry; `settle` then clicks in place, so its
        // release receipt is the last thing queued.
        qmp.pointer = None;
        click_at(&mut qmp, &mut session, 200, 200)?;
        settle(&mut qmp, &mut session)?;
        hold(&mut qmp)?;
        for _ in 0..3 {
            qmp.send_motion(100, 0)?;
        }
        qmp.send_button("left", true)?;
        qmp.send_button("left", false)?;
        session.expect("compositor: press at x=500 y=200")?;
        if session.expect_within(HELD, std::time::Duration::from_secs(1))? {
            return Err("input gate FAILED: with the i8042 drain held, a three-step walk and a click \
                        should fill QEMU's sixteen-byte queue so that the release waits — and it \
                        arrived anyway, so this guard built nothing and proves nothing. Check that \
                        the mouse still answers the wheel knock: a three-byte packet leaves room"
                .into());
        }
        println!("  ok: with the drain held, QEMU kept the release back — the condition is real");
        expect_after_pointer(&mut qmp, &mut session, HELD)?;
        println!("  ok: expect_after_pointer's flush delivered the release QEMU held");
        qmp.pointer = Some((500, 200));

        settle(&mut qmp, &mut session)?;
        hold(&mut qmp)?;
        click_at(&mut qmp, &mut session, 500, 200)?;
        if !session.expect_within(HELD, std::time::Duration::from_secs(2))? {
            return Err("input gate FAILED: `click_at` walked three steps and clicked inside a \
                        held drain, and the release never arrived — its flush after the press \
                        receipt is what sends a release QEMU held for want of room"
                .into());
        }
        println!("  ok: click_at's flush delivered the release QEMU held");
    }

    expect_probe_pass(&session)?;
    println!("  ok: boot-probe's verdict is PASS");
    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);

    // `f1` is keycode 59, `Space` 57: neither press nor release may reach the focused window (`win
    // key` is the compositor's delivery, `widget key` the router's).
    for line in ["win key code=59", "widget key code=59", "win key code=57"] {
        if transcript.contains(line) {
            return Err(format!(
                "input gate FAILED: a registered chord reached the focused window — the \
                 transcript contains \"{line}\". `ui-testclient` registers `Super+F1` (59) and \
                 `Super+Space` (57), so the compositor must consume each chord's press, its \
                 release, and any repeat armed from it; delivering any of them makes every \
                 hotkey also type into whatever has the keyboard"
            )
            .into());
        }
    }
    println!("  ok: Super+F1 fired the manager's chord and reached no window");
    if usb {
        check_departures_quiet(&transcript)?;
    }

    // The USB keyboard took its lights: QEMU traces nothing of them (unlike its PS/2 one, read by
    // `check-terminal`), so the hub thread's `SET_REPORT` acknowledgement is what can be seen.
    if usb {
        if !transcript.lines().any(|l| l.starts_with("usb: port ") && l.contains(": keyboard lights acknowledged")) {
            return Err("input gate FAILED: no USB keyboard acknowledged its lights — the compositor sends \
                        Num Lock's when it connects, so every boot's keyboard is set once"
                .into());
        }
        println!("  ok: the USB keyboard acknowledged its lights");
    }

    println!("\nxtask: input gate PASSED — an injected key and click reached userspace ✓");
    Ok(())
}
