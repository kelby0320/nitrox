//! Keys and clicks sent to the guest, each waited on until it is seen to land.

use crate::*;

/// Position the pointer, click, and **confirm where the press landed**, retrying if it did not. The
/// compositor already reports every press's position, so the retry needs no guest change. **A retry
/// is not inert**: the bar's entries are *toggles*, so a near miss performs the gesture and the
/// retry its inverse. The fix is fewer misses: `Qmp::pointer` remembers a confirmed press, so
/// clicks walk instead of re-pinning with a burst that overruns the guest's input ring.
pub(crate) fn click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<String> {
    const ATTEMPTS: u32 = 3;
    const PER_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(10);
    for attempt in 1..=ATTEMPTS {
        move_pointer_to(qmp, x, y)?;
        qmp.send_button("left", true)?;
        qmp.send_button("left", false)?;
        if session.expect_within(&format!("compositor: press at x={x} y={y}"), PER_ATTEMPT)? {
            qmp.pointer = Some((x, y));
            // Flush, so the release is certain to have been sent: QEMU holds a packet its
            // sixteen-byte queue cannot fit until the next event, and a click's release is the last
            // one.
            qmp.flush()?;
            // The press line's tail ends with the `win=` the compositor routed to. This does not
            // wait for the release: that wait made the drag step below fail deterministically, one
            // serial line later.
            return session.rest_of_line();
        }
        qmp.pointer = None;
        println!("  note: the press did not land at ({x}, {y}) on attempt {attempt}/{ATTEMPTS}");
        session.skip_to_end()?;
    }
    Err(format!(
        "the pointer never reached ({x}, {y}) in {ATTEMPTS} attempts. Injection is relative and \
         unacknowledged, so anything that eats a delta leaves a permanent offset. Since \
         2026-08-26 `input-server` carries the motion of an undeliverable batch forward instead \
         of discarding it, and `burst_holds_its_position` gates that — so a failure here is \
         more likely a mis-aimed click than lost movement. Check that gate's verdict first, \
         then look for `input batch DROPPED (SYN_DROPPED)` in the transcript. Loss on the \
         *host* side leaves no such line at all: QEMU's PS/2 queue holds a packet it cannot fit \
         until the next injected event, which is why the pin is drained and flushed before the \
         walk and why three attempts failing means something other than a held motion"
    )
    .into())
}

/// Inject a burst of motion across a full-screen repaint and check that none of it was lost. The
/// assertion is arithmetic: every delta is known, the start is pinned in a corner, and the
/// compositor reports where a press landed. The burst is sent as fast as QMP accepts it, right
/// after a desktop switch, so it is faster than the guest can drain.
pub(crate) fn burst_holds_its_position(
    qmp: &mut Qmp,
    session: &mut Session,
    chord: &dyn Fn(&mut Qmp, bool, &str) -> R<()>,
) -> R<()> {
    // Pinned **without a click to confirm it**: the clamp makes the corner certain, and the corner
    // is on the bottom bar, where a click opens the overview.
    let screen = qmp.screen.ok_or("burst_holds_its_position: the screen's size is unset")?;
    let corner = screen.corner();
    for _ in 0..screen.pin_motions() {
        qmp.send_motion(100, 100)?;
    }
    qmp.pointer = Some(corner);
    // Let that drain: the chord below is a key, and an overflow may drop a key (`SYN_DROPPED`),
    // where motion is recovered. Then flush as `move_pointer_to` does: QEMU holds what its
    // sixteen-byte queue could not take and would add it to the burst's first motion, which the
    // chord's key (syncing only the keyboard) cannot send.
    std::thread::sleep(std::time::Duration::from_millis(500));
    qmp.flush()?;

    // Up and left, inside the screen all the way: a clamp would hide the loss this looks for.
    const K: i32 = 120;
    const DX: i32 = -6;
    const DY: i32 = -3;
    let want = (corner.0 + K * DX, corner.1 + K * DY);

    // To desktop 1, as switching to the desktop already showing says nothing. **The motion goes in
    // immediately after the chord, not after the acknowledgement**, which comes after the stall the
    // burst must land in; placed there, this gate passed against an `input-server` that discards
    // motion. It is safe for the chord, whose keys are queued first: a full ring refuses the last.
    chord(qmp, false, "1")?;
    for _ in 0..K {
        qmp.send_motion(DX, DY)?;
    }
    session.expect("desktop-shell: switched to ")?;

    // Settle before reading: the click that reads the position is a *button*, droppable like any
    // other.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Not `click_at`, whose retry would correct for the bug under test.
    qmp.send_button("left", true)?;
    qmp.send_button("left", false)?;
    let want_line = format!("compositor: press at x={} y={}", want.0, want.1);
    if session.expect_within(&want_line, std::time::Duration::from_secs(20))? {
        qmp.pointer = Some(want);
        qmp.flush()?;
        println!("  ok: {K} motions across a full-screen repaint arrived to the pixel");
        return Ok(());
    }
    qmp.pointer = None;
    Err(format!(
        "input gate FAILED: {K} injected motions of ({DX}, {DY}) from {corner:?} did not put \
         the cursor at ({}, {}). A relative delta that does not arrive cannot be recovered — \
         `input-server` must carry the motion of an undeliverable batch forward and re-emit it, \
         rather than counting it as a `SYN_DROPPED` gap. Look for `input batch DROPPED` in the \
         transcript: a gap here means the compositor was busy and the deferral did not cover it",
        want.0, want.1
    )
    .into())
}

/// Type a line into a windowed terminal, then Enter. **Paced by a delay, not a receipt**: the
/// terminal's echo goes into its grid, rendered under `test-harness` only, and a dropped PS/2 batch
/// eats a keystroke.
pub(crate) fn type_at_terminal(qmp: &mut Qmp, line: &str) -> R<()> {
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    // **A bare Enter first**: `ESC` is the meta prefix and eats the next byte, and an Escape
    // pressed to dismiss a menu reaches the terminal when no popup is open (`desktop` becomes
    // `esktop`).
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    // **The wait comes before each key, including the first**: the first character after a click
    // that repaints was swallowed.
    for c in line.chars() {
        std::thread::sleep(PER_KEY);
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
    }
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    Ok(())
}

/// **Type an answer at a prompt a program is waiting at, then Enter**, like [`type_at_terminal`]
/// but with no leading Enter, which at a prompt would be an empty answer.
pub(crate) fn type_answer(qmp: &mut Qmp, text: &str) -> R<()> {
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    for c in text.chars() {
        std::thread::sleep(PER_KEY);
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
    }
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    Ok(())
}

pub(crate) fn press(qmp: &mut Qmp, qcode: &str) -> R<()> {
    qmp.send_key(qcode, true)?;
    qmp.send_key(qcode, false)?;
    Ok(())
}

/// **`boot-probe`'s verdict, `PASS`**, waited for up to a minute. Only `test-qemu` exits on it, so
/// a gate that never read it passed while the probe failed. Read from the transcript, not with
/// `expect`, which would consume what the gate waits for next, and as a whole line.
pub(crate) fn expect_probe_pass(s: &Session) -> R<()> {
    const VERDICT: &str = "boot-probe: test-harness verdict ";
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let t = s.transcript();
        if t.lines().any(|l| l.contains("boot-probe: test-harness verdict PASS")) {
            return Ok(());
        }
        if t.lines().any(|l| l.contains("boot-probe: test-harness verdict FAIL")) {
            let failed: Vec<&str> = t.lines().filter(|l| l.contains("boot-probe:") && l.contains("FAIL")).collect();
            return Err(format!("boot-probe failed on this machine: {}", failed.join(" | ")).into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("no `{VERDICT}…` line within a minute of the gate's end").into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// **The PS/2 keyboard's lights since the kernel reset it**, from QEMU's `ps2_set_ledstate` trace,
/// in PS/2's order (Scroll Lock bit 0, Num Lock bit 1, Caps Lock bit 2), starting with the reset's
/// zero. The firmware sets its own lights first, so only what follows the last reset counts.
fn ps2_lights(log: &Path) -> Vec<u8> {
    ps2_lights_in(&fs::read_to_string(log).unwrap_or_default())
}

/// [`ps2_lights`] of the trace's text: **the value is the line's last word**, as the event's name
/// holds `ledstate` too.
pub(crate) fn ps2_lights_in(text: &str) -> Vec<u8> {
    let mut lights = Vec::new();
    for line in text.lines() {
        if line.contains("ps2_reset_keyboard") {
            lights.clear();
        } else if line.contains("ps2_set_ledstate")
            && let Some(v) = line.rsplit(' ').next().and_then(|v| v.parse().ok())
        {
            lights.push(v);
        }
    }
    lights
}

/// Wait until the keyboard's lights since its reset are **exactly** `want`, so a write too many
/// fails here rather than passing as the next step's value.
pub(crate) fn wait_ps2_lights(log: &Path, want: &[u8], what: &str) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let got = ps2_lights(log);
        if got == want {
            println!("  ok: {what}: the keyboard's lights went {want:?}");
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "{what}: the PS/2 keyboard's lights since the kernel reset it are {got:?}, not \
                 {want:?} (PS/2's order: 2 is Num Lock, 4 Caps Lock)"
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// **Press `qcode` at a terminal**, with Shift if `shift`, and wait for its grid to show `line`
/// followed by `shows`. **A row is reported with its trailing blanks trimmed**, so a space is
/// waited for as the row before it.
pub(crate) fn key_echoed(
    qmp: &mut Qmp,
    session: &mut Session,
    line: &mut String,
    qcode: &str,
    shift: bool,
    shows: &str,
) -> R<()> {
    if shift {
        qmp.send_key("shift", true)?;
    }
    press(qmp, qcode)?;
    if shift {
        qmp.send_key("shift", false)?;
    }
    line.push_str(shows);
    session.expect(&format!("nxterm: grid> {}", line.trim_end()))?;
    Ok(())
}

/// The QMP key code for `c`, and whether it needs shift held. A silently dropped character makes a
/// *different command*.
pub(crate) fn qcode_for(c: char) -> R<(String, bool)> {
    let s = |t: &str, shift: bool| Ok((t.to_string(), shift));
    match c {
        'a'..='z' => s(&c.to_string(), false),
        'A'..='Z' => s(&c.to_ascii_lowercase().to_string(), true),
        '0'..='9' => s(&c.to_string(), false),
        ' ' => s("spc", false),
        '-' => s("minus", false),
        '_' => s("minus", true),
        '/' => s("slash", false),
        '.' => s("dot", false),
        ',' => s("comma", false),
        '"' => s("apostrophe", true),
        '\'' => s("apostrophe", false),
        '(' => s("9", true),
        ')' => s("0", true),
        other => Err(format!(
            "no qcode for {other:?} — add one to `qcode_for` rather than letting a command line \
             lose a character"
        )
        .into()),
    }
}

/// Type `text` at the greeter, one character at a time, waiting for each redraw: a word injected at
/// once outruns a client that repaints between keystrokes.
pub(crate) fn type_at_greeter(qmp: &mut Qmp, session: &mut Session, text: &str) -> R<()> {
    for c in text.chars() {
        let qcode = match c {
            'a'..='z' => {
                let mut s = String::new();
                s.push(c);
                s
            }
            '0'..='9' => c.to_string(),
            ' ' => "spc".to_string(),
            '-' => "minus".to_string(),
            other => {
                return Err(format!(
                    "check-login cannot type {other:?}: add its qcode to `type_at_greeter`. \
                     The demo credentials are lowercase, digits, space and hyphen; a new one \
                     outside that set needs a mapping rather than a silent skip"
                )
                .into());
            }
        };
        press(qmp, &qcode)?;
        // The redraw is the receipt. **One receipt is not one keystroke**: the greeter presents
        // once per drained batch, so the loop can run a receipt ahead; this is pacing, not
        // acknowledgement.
        session.expect("desktop-session-mgr: greeter redraw ")?;
    }
    Ok(())
}
