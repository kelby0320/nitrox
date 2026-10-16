use crate::*;

/// `cargo xtask check-input` — inject a keystroke and a click, and check they arrive.
///
/// The counterpart to `check-display`, and the answer to the same class of problem. A
/// display gate exists because the guest staying consistent with itself proves nothing about
/// the device binding; an input gate exists because a driver that reports `armed` proves
/// nothing about whether an interrupt ever produced an event. Both are cases where every
/// self-contained check passes and the thing still does not work.
///
/// Since M3 Part B this runs through the **whole path**: i8042 → driver ring → raw node →
/// `input-server` → merge → consumer channel → guest. The client consumes `/dev/input/new`
/// rather than the raw nodes, which it can no longer open — the driver is single-reader per
/// device and the server holds both, which is the exclusivity the keylogging boundary rests
/// on, demonstrated rather than asserted.
///
/// The guest resolves `/dev/input/new`, prints `listening`, and only then does this inject:
/// an event produced before the consumer channel exists is one the server has nowhere to
/// send, so injecting on a timer would make this flaky in exactly the way a test of a rare
/// path must not be.
pub(crate) fn cmd_check_input(accel: Accel, no_ps2_irq: bool, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    // **`--no-ps2-irq` boots the same image with the i8042's interrupt generation left off**,
    // so every byte has to be recovered by the tick-driven `ps2::poll` sweep rather than
    // arriving on IRQ 1 / 12. The assertions below are unchanged — that is the point. A
    // keystroke and a click still have to reach a userspace client; only the road they take
    // is different.
    //
    // It exists because **no pass count can catch deleting that sweep.** On healthy hardware
    // the interrupt path carries every byte and the sweep never fires, so it is pure
    // redundancy: remove it and every gate in the tree still passes. This flag makes the
    // redundancy load-bearing, which is the only way its absence becomes observable. The
    // sweep itself was added 2026-08-13 for the i8042's one-byte output buffer losing edges —
    // a bug that made every input gate intermittently flaky and was written off as flakiness
    // for weeks.
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

    // The compositor is a consumer of the same merged stream — it resolves `/dev/input/new`
    // during startup, which is why the input server is bound first. Asserted *before* the
    // test client's `listening`, which is the ordering `service-mgr` fixes: the compositor is spawned
    // and answers `Meta::Ready` well before the selftest client runs.
    //
    // This proves the compositor is **attached**, not that a key reached a window: with no
    // client holding a window at injection time there is nothing to route to. That end of
    // the chain is Part D's gate, which brings an echo client that stays alive.
    session.expect("compositor: input connected")?;

    session.expect("input-testclient: listening")?;

    // **Wait for `ui-testclient` to finish before injecting anything.** Its leak probe churns
    // 128 windows, and every one of them is a `Normal` window that becomes, for its brief
    // life, the topmost window that takes focus — so a keystroke injected while it runs is
    // routed to a window that is about to be destroyed. That is the compositor behaving
    // correctly; it is the *gate* that was assuming a quiet stack. Diagnosed from the
    // compositor's own routing log: `key win=9` while the client had announced `id=8`.
    //
    // Safe to wait for here rather than later: `listening` is printed milliseconds after
    // spawn, the churn takes seconds, so this line always follows it.
    session.expect("ui-testclient: PASSED")?;

    // ---- Motion survives a consumer that stops reading ----
    //
    // **The guard on the property PR #246 exists to keep**, and the accelerator cannot change
    // what it means. A relative delta *is* the pointer's position: a batch `input-server` cannot
    // deliver is movement no consumer can re-derive, unlike a key or a button, whose state
    // `SYN_DROPPED` asks it to resynchronise. Discarding one leaves every consumer permanently
    // offset from the device.
    //
    // **The overrun is caused by the client, not by the clock.** The first version of this guard
    // lived in `check-login` and outran the compositor's repaint — which works under TCG and not
    // under KVM, where the repaint is fast enough that the ring never fills, and `--kvm` is the
    // only configuration CI runs (PR #246 review, blocking 1). Here the client announces that it
    // has stopped reading and sleeps; the ring fills at any speed.
    //
    // **First, before anything else is injected**, because a sum cannot be delimited: with
    // `--no-ps2-irq` input arrives on a 10 ms sweep and an earlier phase's flood is still
    // trickling in when this one starts. Bracketing it with a button press and release was the
    // first repair and failed for the right reason — a button in an overflowing ring is exactly
    // what deferral cannot recover, so the delimiter was eaten by the overrun it measured.
    //
    // The assertion is arithmetic: relative deltas are additive, so whatever the batching,
    // coalescing and deferral in between do, the consumer must end up with the total injected.
    // **Started by a keystroke, not by the client's own clock.** This step sits between two
    // independent clients' output and `ui-testclient` finishes on its own schedule, so a client
    // that stalled at boot was racing whatever the harness was waiting for — and this step's
    // expects then consumed the `PASSED` line the wait above needed (CI, 2026-08-27). `F2` is
    // unbound: `ui-testclient` registers `Super+F1`.
    press(&mut qmp, "f2")?;
    session.expect("input-testclient: input stalled")?;
    // **Thirty, not sixty.** The ring is sixteen messages and the client is asleep, so thirty
    // overruns it by fourteen — the property is "it overran", not "by how much".
    const BURST: i32 = 30;
    const BDX: i32 = 7;
    const BDY: i32 = 3;
    // **Paced, and the pacing is not a workaround for the guest.** The overrun under test comes
    // from a consumer that has stopped reading, not from how fast the harness injects — so
    // slowing the injection costs the measurement nothing. What it avoids is *host*-side loss:
    // with the i8042's interrupts off (`--no-ps2-irq`) nothing reads the controller until the
    // 10 ms recovery sweep, and QEMU's own PS/2 queue is sixteen bytes — barely five packets —
    // so a burst injected as fast as QMP accepts it overflows *that*. QEMU holds what does not
    // fit until the next injected event (`expect_after_pointer`), and a burst's last event has
    // none after it. Measured: one to five packets short per run, with the guest announcing no
    // loss at all, because nothing in the guest lost anything.
    for _ in 0..BURST {
        qmp.send_motion(BDX, BDY)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    // The sum, and the evidence that it was worth summing. `REL_Y` comes back with the sign the
    // harness injected — the PS/2 wire reports positive-Y as up and the driver negates, which
    // `check-input`'s earlier motion assertion pins.
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
    // **The precondition, checked rather than assumed.** Recovery is invisible by design — the
    // stream still adds up — so a run where the ring never filled satisfies the sum above while
    // testing nothing. A folded group carries the total of the batches it stands for, so it is
    // wider than any single injected delta; equal to it means no deferral happened and this gate
    // proved nothing. That is not a pass.
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

    // A key down and up. `a` is scancode 0x1E in set 1, so keycode 30 — chosen because it
    // is in the identity range the decoder relies on, and because a wrong `E0` or release
    // bit shows up as a different code rather than as silence.
    qmp.send_key("a", true)?;
    session.expect("input-testclient: ev kind=1 code=30 value=1")?;
    qmp.send_key("a", false)?;
    session.expect("input-testclient: ev kind=1 code=30 value=0")?;

    // Motion, then a click. The mouse reports positive-Y as up and `REL_Y` is positive-down,
    // so a downward injection must come back **negated** — that inversion is a one-line
    // mistake no host test can catch, because the host has no PS/2 wire to be wrong about.
    qmp.send_motion(5, 3)?;
    session.expect("input-testclient: ev kind=2 code=0 value=5")?;
    // `REL_Y` must come back **+3**, matching the injected downward motion. The PS/2 wire
    // reports positive-Y as *up*, so this only holds because the driver negates — and a
    // missing negation is a one-line error no host test can catch, because the host has no
    // PS/2 wire to be wrong about. This assertion is the only thing standing between an
    // inverted mouse and a green build.
    session.expect("input-testclient: ev kind=2 code=1 value=3")?;

    // ---- The wheel (M14 Part I) ----
    //
    // **This is the only check on the sign, and on the packet length.** A mouse that answered
    // the IntelliMouse knock sends *four*-byte packets, and a decoder still framing three
    // reads every packet after the first at an offset — so the motion assertions above would
    // have started failing rather than this one. That the knock happened at all is only
    // observable here: it is a conversation with the emulated device, and no host test has one.
    //
    // `REL_WHEEL` is `0x08`, and it is **positive-down** in this system — the sign convention
    // is the screen's rather than Linux's (`rsproto-input-ops.md`). A turn toward the user
    // must therefore arrive as `value=1`. Getting this backwards is one character in the
    // driver and inverts scrolling everywhere; nothing on the host can catch it, because the
    // host has no PS/2 wire to be wrong about — exactly the argument the `REL_Y` assertion
    // above makes for itself.
    //
    // **Before the click, not after it.** `BTN_LEFT`'s press is the sentinel that ends the
    // client's raw-stream phase, so anything injected after it is never printed.
    qmp.send_wheel(true)?;
    session.expect("input-testclient: ev kind=2 code=8 value=1")?;
    qmp.send_wheel(false)?;
    session.expect("input-testclient: ev kind=2 code=8 value=-1")?;

    qmp.send_button("left", true)?;
    session.expect("input-testclient: ev kind=1 code=272 value=1")?;
    // Release it: phase 2 asserts on the button mask, and a press left held from here would
    // make `buttons` already non-zero before its own click.
    qmp.send_button("left", false)?;

    // ---- Through a window ----
    //
    // Everything above proves the *device* stream reached userspace. This proves the
    // Surface path: the compositor routed it to a window and `libsurface` delivered it into that
    // window's queue. The client creates the window only now, so the two phases cannot
    // contend for its four-message session ring.
    session.expect("input-testclient: window ready")?;

    // **A deliberate flood, and a weak one.** Twelve cursor movements are what broke this
    // gate before M3 Part D3, when each became a `PointerEvent` sent `NOBLOCK` at a
    // four-message ring and the keystroke behind them went on the floor.
    //
    // It proves less than it looks: the client drains in `wait_event` between injections, so
    // thirteen messages never queue against a sixteen-slot ring and **no send is ever
    // refused**. It covers neither coalescing nor retry — the phase-3 stall below is what
    // covers those. Kept because it is still the historical regression, cheap, and the shape
    // a real cursor movement takes.
    for _ in 0..12 {
        qmp.send_motion(-120, -120)?;
    }

    // **The window was told it has the keyboard.** The second half of the two-focus rule:
    // widget focus is the toolkit's, window focus is the compositor's, and a client needs
    // both to know whether a caret should blink. Announced on the change, so this arrives
    // once when the window becomes the topmost focusable one.
    // **Ordering, not just arrival.** Focus is announced on the create itself, so it
    // precedes any input this window could be routed. Asserting only that a focus event
    // *arrived* passes against a compositor that announces it late — verified by
    // reintroducing that bug and watching the gate stay green (PR #184 review, finding 2).
    session.expect("input-testclient: first win event=focus")?;
    session.expect("input-testclient: win focus has=1")?;

    // `b` is keycode 48. It goes to the *focused* window — this client's, being the topmost
    // that takes focus — not to whatever the cursor happens to be over.
    qmp.send_key("b", true)?;
    session.expect("input-testclient: win key code=48 down=1")?;
    // **Through the toolkit**, which is Part B's actual deliverable: the same keystroke came
    // out of `libui`'s router after `element -> layout -> diff -> route`, addressed to a
    // widget rather than a window. Everything in that chain is unit-tested; this is the only
    // thing that says the pieces are wired to each other.
    session.expect("input-testclient: widget key code=48 down=1")?;
    // **Held, not released**: the compositor repeats it. Asserted before the release,
    // because a repeat that only arrived after the key came up would be a bug that a test
    // ordering these the other way round could not see.
    session.expect("input-testclient: win repeat code=48")?;

    qmp.send_key("b", false)?;
    session.expect("input-testclient: win key code=48 down=0")?;

    // ---- Keyboards that come and go (Phase 6 Part C) ----
    //
    // **Here, while the window that holds the keyboard logs every key**, as the chord below has to
    // be. Only on the machine whose keyboards are USB: the i8042's never leave.
    if usb {
        await_registry_probe(&session)?;
        usb_keyboards_come_and_go(&mut qmp, &mut session)?;
    }

    // ---- A registered chord is consumed (M8 Part B) ----
    //
    // **Here, not at the end of the gate.** The first version ran this after
    // `input-testclient: PASSED`, where the client has stopped logging window events — so a
    // compositor that delivered the chord produced the same silence as one that consumed it,
    // and the control (fire *and* deliver) passed. It has to run while the window that holds
    // focus is still reporting what it receives.
    //
    // The compositor's own tests pin consumption over `route`, the function the binary routes
    // with. What only a boot can show is that a chord injected on a real PS/2 wire, into a stack
    // where a real client holds the keyboard, does not also type into it.
    //
    // **Twice, so the screen ends as it started.** `ui-testclient` toggles the desktop on each
    // chord, and everything after this needs its window back on screen.
    for want in ["desktop 2", "desktop 1"] {
        qmp.send_key("meta_l", true)?;
        qmp.send_key("f1", true)?;
        // **Held past the repeat delay, which is the whole of what the first version missed.**
        // Repeat is armed from the *physical* transition, so a consumed chord used to arm one
        // anyway and deliver its key to the focused window 400 ms later, bypassing the router
        // entirely. Injecting down-and-up back to back never reached that timer, so the gate was
        // blind to it — `input-testclient` logs repeats like any other key, so holding the chord
        // is all that was needed (PR #241 review, blocking 1). `REPEAT_DELAY_NS` is 400 ms.
        std::thread::sleep(std::time::Duration::from_millis(700));
        qmp.send_key("f1", false)?;
        qmp.send_key("meta_l", false)?;
        // **The positive half, and it is what makes the absence at the end mean anything.**
        // Without it a chord that was never injected — a wrong qcode, a dropped QMP command —
        // would produce the same silence as one correctly consumed.
        session.expect(&format!("ui-testclient: hotkey fired -> {want}"))?;
    }
    // **And a chord whose action moves nothing, held past the repeat delay.** The two above
    // switch desktops, which empties the current one — so `focus_candidate` becomes `None` and
    // `fire_repeat` cancels itself, making this gate immune to a wrongly-armed repeat by
    // coincidence of what the client does with the chord. `Super+Space` only prints, so focus
    // stays where it is and a repeat that should not exist lands in the focused window's log.
    qmp.send_key("meta_l", true)?;
    qmp.send_key("spc", true)?;
    std::thread::sleep(std::time::Duration::from_millis(700));
    qmp.send_key("spc", false)?;
    qmp.send_key("meta_l", false)?;
    session.expect("ui-testclient: quiet chord fired")?;

    // No wait for focus here: the compositor announces it back **between** the two chords, so
    // an expect placed after them scans forward past a line already emitted and times out. The
    // click assertions below are what depend on the window being back, and they say so.

    // ---- The wheel, through a window and into a widget (M14 Part I) ----
    //
    // The device-layer half above proves the driver frames and signs it. This proves the rest
    // of the path: `libinput` turned the axis into a `Logical::Wheel`, the compositor routed it
    // to the window **under the cursor** (never the focused one — a wheel is a pointer event)
    // and stamped a `POINTER_WHEEL` record, and `libui` dispatched it to `on_wheel`.
    //
    // **The widget line is the one that could not be faked.** `on_pointer` never receives a
    // wheel, so a `widget wheel` line means the toolkit routed it *as* a wheel — a change that
    // delivered wheels to `on_pointer` "so nothing is lost" would print `widget ptr kind=4`
    // here instead and this would fail.
    //
    // Before the click, because the click's press is what ends the client's window phase.
    qmp.send_wheel(true)?;
    session.expect("input-testclient: win ptr kind=4 btn=0 buttons=0 wheel=1")?;
    session.expect("input-testclient: widget wheel dz=1")?;
    qmp.send_wheel(false)?;
    session.expect("input-testclient: widget wheel dz=-1")?;

    // And a click, which is routed by hit-testing instead of focus. `buttons=1` is the mask
    // the record carries on every kind — the field that used to read zero here.
    qmp.send_button("left", true)?;
    session.expect("input-testclient: win ptr kind=1 btn=272 buttons=1 wheel=0")?;
    // Kind 1 is `POINTER_BUTTON`, and the coordinates are **widget-local**: the grid fills
    // the window and sits at its origin, so they match — which is exactly why the host tests
    // place a widget away from the origin as well.
    session.expect("input-testclient: widget ptr kind=1")?;
    qmp.send_button("left", false)?;

    // ---- Park-and-retry ----
    //
    // The client stops draining. The flood then overruns its 16-slot ring: everything after
    // that parks in the compositor's per-session outbox, where motion coalesces to a single
    // record and the key queues behind it. On waking, the key must still arrive.
    //
    // This is the only assertion covering park-and-retry. The earlier flood covers neither
    // it nor coalescing — with the client draining between injections a send is never
    // refused at all, which is finding 3 of the PR #181 review.
    session.expect("input-testclient: stalling")?;

    // Comfortably past the 16-slot ring, so the outbox is genuinely holding messages.
    for _ in 0..40 {
        qmp.send_motion(3, 3)?;
    }
    // `c` is keycode 46. Injected *after* the ring is full, so it can only arrive if the
    // compositor parked it and re-sent it — and only if it wakes itself to do so, since a
    // client draining its own ring signals nothing to the compositor.
    qmp.send_key("c", true)?;

    session.expect("input-testclient: late key code=46")?;

    session.expect("input-testclient: PASSED")?;

    // **A release QEMU is holding is delivered** — the `lost-release` fix's guard (PR #331). At
    // the natural rate, about two runs in a hundred, deleting either flush passes every gate, so
    // this builds the condition on purpose. F9 makes this kernel hold the i8042 drain for 300 ms
    // (`ps2-hold-gate`, which `test-harness` implies), and inside the hold a three-step walk and a
    // click fill QEMU's sixteen-byte queue to the last packet: the press fits and the release
    // does not, so QEMU keeps it until the next injected event.
    //
    // **The condition is asserted first**, so the guard cannot pass for nothing: a raw release
    // must *not* arrive on its own. Then the two flushes that deliver it — `expect_after_pointer`'s
    // and `click_at`'s — each against a hold of its own. Release lines are matched by position,
    // so the clicks that set each round up cannot satisfy them.
    //
    // **Each hold starts from an empty queue**, which `settle` makes certain: a click whose
    // release is the last packet injected, with a receipt for it. A `click_at` would not do — its
    // flush sends one more packet *after* the release's receipt can arrive, and with the i8042's
    // interrupts off (`--no-ps2-irq`) the guest reads only on the 10 ms tick, so that packet was
    // still in QEMU's queue when F9 set the hold. The hold then held the *press* instead of the
    // release — a byte log in the driver showed the flush packet split around the F9 byte, the
    // keyboard having priority at the controller.
    //
    // **Not under `--no-ps2-irq`.** There the guest reads only on the tick, and a click from an
    // unknown position is itself unreliable: the pin's over-drive outruns what a flush can
    // deliver, and the unpaced walk back arrives packed with the press — which is why that
    // variant's own steps pace their injection. The guard is about the gate's flush, not the
    // interrupt path, so the variant that exists for the interrupt path does not run it.
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
        // Somewhere known first, through `click_at` and its retry: from an unknown position the
        // pin and the long walk back can overrun the queue on a guest that reads on the tick,
        // and a press held with walk motion lands short (`expect_after_pointer`). `settle` then
        // clicks where the pointer already is — no walk — so its release receipt is the last
        // thing in the queue.
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

    // **And `boot-probe` passed on this machine** (PR #358 review): on the `--usb` one it failed
    // its registry test, and this gate passed.
    expect_probe_pass(&session)?;
    println!("  ok: boot-probe's verdict is PASS");
    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);

    // `f1` is keycode 59. The window that had the keyboard must have seen neither the press nor
    // its release — `win key` is the compositor's delivery and `widget key` is `libui`'s router
    // one layer above it, so checking both says the chord stopped at the compositor rather than
    // being dropped later by luck.
    // 59 is F1, 57 is Space — the two chords this client registers.
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

    // **The USB keyboard took its lights** (Phase 6 Part B.5): the compositor's Num Lock, through
    // `input-server` to the keyboard's node, sent by the hub thread as a `SET_REPORT` the device
    // acknowledged. QEMU's USB keyboard traces nothing of its lights, unlike its PS/2 one, whose
    // values `check-terminal` reads; so here the acknowledgement is what can be seen, and the
    // request's fields are `the_lights_request_is_an_output_report_to_its_interface`'s.
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
