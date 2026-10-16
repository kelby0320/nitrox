//! Keys and clicks sent to the guest, each waited on until it is seen to land.

use crate::*;

/// Position the pointer, click, and **confirm where the press landed** — retrying if it did
/// not land there.
///
/// **The press is the receipt, and it already existed.** `move_pointer_to` fires ~33
/// unacknowledged relative motions, and a dropped one leaves a permanent offset; the
/// compositor already reports every press's position, so the retry needs no guest change at
/// all. PR #232 tried reporting position per *motion* instead and it cost too much in the
/// compositor's input path — this is the cheap half of that idea, using a receipt the protocol
/// was already emitting.
///
/// Observed on the first full run of this gate: `input batch DROPPED (SYN_DROPPED)` and a
/// press at (321, 312) with `win=none`.
///
/// **A retry is not inert, and since M8 Part C it can undo the thing it is retrying.** The
/// bar's entries are *toggles* — a press that misses the aimed pixel by less than an entry
/// width lands on the same entry and performs the gesture, and the retry then performs its
/// inverse, so the gate waits forever for a transition that happened twice. That is not fixed
/// here; what is fixed is the cause of the misses. Every click used to re-pin the pointer to a
/// corner with twenty over-driven motions before walking, and that burst is what overran the
/// guest's input ring. A confirmed press is a position report, so `Qmp::pointer` remembers it
/// and consecutive clicks walk from there — two motions rather than thirty-four
/// (PR #243 review, finding 3).
pub(crate) fn click_at(qmp: &mut Qmp, session: &mut Session, x: i32, y: i32) -> R<String> {
    const ATTEMPTS: u32 = 3;
    const PER_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(10);
    for attempt in 1..=ATTEMPTS {
        // Unknown position means pin first; known means walk the difference.
        move_pointer_to(qmp, x, y)?;
        qmp.send_button("left", true)?;
        qmp.send_button("left", false)?;
        if session.expect_within(&format!("compositor: press at x={x} y={y}"), PER_ATTEMPT)? {
            qmp.pointer = Some((x, y));
            // **Then flush, so the release is certain to have been sent.** QEMU holds a packet
            // its sixteen-byte queue cannot fit until the next injected event, and a click's
            // release is the last one — so after a walk that filled the queue, the press went
            // and the release waited for ever (the `lost-release` deferral, three times in CI). The
            // receipt proves the guest drained the queue through the press, so this event's
            // sync has room, and it carries the held release if there is one. It moves nothing
            // and waits for nothing, so it is not the release-wait the drag step rejected below.
            qmp.flush()?;
            // The press line's tail, which ends with the `win=` the compositor routed to. One
            // caller needs it, and an `expect` for the whole line would scan past its own
            // evidence.
            //
            // **This deliberately does not wait for the release**, though the release is now
            // logged beside the press and waiting for it was tried. A click is both halves, and
            // confirming only the press is why a click whose effect never arrives looks like a
            // client that ignored it — which is how `check-login` failed in CI on PR #280 with
            // nothing in the transcript to say which. But adding the wait made the *drag* step
            // below fail deterministically, three runs out of three, with every release present
            // and no retry ever firing: the only difference is that the gate proceeds one serial
            // line later. That is a timing sensitivity in the guest's input path, not something
            // the wait fixed or caused, and trading one red gate for another to chase it is not
            // a bargain. The logging stays, because it makes the next occurrence *legible*;
            // the wait does not, because it makes a different step fail.
            //
            // **It is not lost input, and that took a change to establish.** A click is both
            // halves and only the press used to be logged, so a dropped release and a client
            // ignoring the click were indistinguishable. With both logged the answer was that
            // both arrived — the loss was in `libui`, which dropped the gesture when a repaint
            // between the halves re-identified the widget under the pointer. Fixed 2026-09-04;
            // the logging stays, because it is what turned a guess into an answer.
            return session.rest_of_line();
        }
        // It did not land where it was aimed, so where it *is* is no longer known.
        qmp.pointer = None;
        println!("  note: the press did not land at ({x}, {y}) on attempt {attempt}/{ATTEMPTS}");
        // The abandoned attempt's press must not satisfy the next one's wait.
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

/// Inject a burst of motion across a full-screen repaint and check that none of it was lost.
///
/// **The assertion is arithmetic, not a screenshot.** Every injected delta is known, the start
/// is pinned by over-driving into a corner, and the compositor reports where a press landed — so
/// the expected position is exact and any difference is movement that did not arrive. There is
/// no rounding, no scaling and no acknowledgement anywhere on this path: a PS/2 delta is applied
/// as an integer and clamped only at the screen edge, which this route stays well inside.
///
/// **The burst must be faster than the guest can drain**, or it proves nothing: paced injection
/// is what every other step here does, and a consumer that is never overrun is a consumer whose
/// loss path is untested. It is sent as fast as QMP accepts it, immediately after a desktop
/// switch, which is the compositor's one legitimately whole-screen recompose.
pub(crate) fn burst_holds_its_position(
    qmp: &mut Qmp,
    session: &mut Session,
    chord: &dyn Fn(&mut Qmp, bool, &str) -> R<()>,
) -> R<()> {
    // Start pinned, and **without a click to confirm it**: over-driving into a corner is the one
    // position injection can establish with no acknowledgement, because the clamp is what makes
    // it certain — 2000 px of travel into an edge lands on the edge whether or not some of it
    // arrives. A confirming click would be worse than redundant here: the bottom-right corner is
    // on the bottom bar, a few pixels past the desktop's name — which opens the overview, which
    // would then take the chord below instead of the shell — and a click that confirmed a
    // position by what it did there would depend on those few pixels.
    let screen = qmp.screen.ok_or("burst_holds_its_position: the screen's size is unset")?;
    let corner = screen.corner();
    for _ in 0..screen.pin_motions() {
        qmp.send_motion(100, 100)?;
    }
    qmp.pointer = Some(corner);
    // Let that drain before anything else is injected. **The chord below is a key, and a key is
    // exactly what this fix does not recover**: `SYN_DROPPED` asks a consumer to resynchronise
    // state it can re-derive, and motion is the half it cannot. Flooding the ring and then
    // relying on a keystroke to survive it would be a gate that failed for the one reason the
    // system is entitled to.
    //
    // **Then flush, as `move_pointer_to` does after its own pin, and for its reason**: QEMU holds
    // what its sixteen-byte queue could not take, and would add it to the burst's first motion.
    // The chord below cannot send it — a key syncs only the keyboard — so without this the burst
    // would start short by whatever the pin left, and the failure would blame `input-server` for
    // a delta QEMU was holding (PR #331 review, finding 1).
    std::thread::sleep(std::time::Duration::from_millis(500));
    qmp.flush()?;

    // Up and to the left, staying inside the screen the whole way: a burst that ran into a
    // clamp would hide exactly the loss this is looking for.
    const K: i32 = 120;
    const DX: i32 = -6;
    const DY: i32 = -3;
    let want = (corner.0 + K * DX, corner.1 + K * DY);

    // **To desktop 1, because the session is on desktop 2 by now** and switching to the desktop
    // already showing is a no-op the shell correctly says nothing about — the first version of
    // this waited 45 s for an acknowledgement that was never coming.
    //
    // **The motion goes in immediately after the chord, not after the acknowledgement.** The
    // shell logs the switch once the compositor has answered, and the compositor answers when
    // the recompose is done — so waiting for that line puts the burst entirely *after* the stall
    // it is supposed to land in. Verified the expensive way: with the burst placed after the
    // acknowledgement, this gate passed against an `input-server` that discards motion, which is
    // the bug it exists to catch.
    //
    // Injecting behind the chord is safe for the chord: its keys are already queued ahead of
    // this motion, and an overflowing ring refuses what arrives last.
    chord(qmp, false, "1")?;
    for _ in 0..K {
        qmp.send_motion(DX, DY)?;
    }
    session.expect("desktop-shell: switched to ")?;

    // Settle before reading: deferred motion is re-sent on the next wakeup or within a few
    // milliseconds, and the click that reads the position is a *button* — droppable like any
    // other, so it is injected once the guest is idle rather than into the same stall.
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Where it actually ended up. Not `click_at`, which re-pins and retries — the retry exists
    // to survive precisely the loss under test, and using it here would report a pass after
    // correcting for the bug.
    qmp.send_button("left", true)?;
    qmp.send_button("left", false)?;
    let want_line = format!("compositor: press at x={} y={}", want.0, want.1);
    if session.expect_within(&want_line, std::time::Duration::from_secs(20))? {
        qmp.pointer = Some(want);
        // The release, if QEMU held it — see `expect_after_pointer`.
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

/// Type a line into a windowed terminal, then Enter.
///
/// **Paced by a delay rather than by a receipt, which is the exception here.** Every other
/// typing loop in these gates waits for something the guest says — the greeter's redraw, the
/// shell's `name so far`, a prompt on serial. A terminal's echo goes into its *grid*, and the
/// grid renders under `test-harness` only, so a gate booting a release image has nothing to
/// wait on per character. Injection is relative and unacknowledged, and a dropped PS/2 batch
/// eats a keystroke — which here means the command line reads `desktp` and nothing runs.
///
/// The delay is the smallest thing that removes the drops rather than a guess at a safe number:
/// the whole line is a handful of characters and the guest's input ring is drained per event.
pub(crate) fn type_at_terminal(qmp: &mut Qmp, line: &str) -> R<()> {
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    // **A bare Enter first, because this gate has pressed Escape at a terminal.** `ESC` is the
    // **meta prefix**: the discipline consumes the byte after a bare one, exactly as readline
    // does — `ESC d` is M-d, and an unbound pair is discarded rather than inserted. This gate
    // presses Escape to dismiss the menu and the overview, and when no popup is open that
    // Escape reaches whatever holds the keyboard, which by then is the terminal. So the next
    // character typed is eaten: `desktop` becomes `esktop`.
    //
    // Measured rather than assumed, after it was briefly taken for a bug in `nxsh`: typing one
    // command three times in a session loses one character, and injecting an Escape between two
    // of them loses a second — the loss follows the Escape. An empty line is the cheapest thing
    // to feed a prefix that is going to take one.
    std::thread::sleep(PER_KEY);
    press(qmp, "ret")?;
    // **The wait comes *before* each key, including the first**, and that is not cosmetic.
    // Typing here follows a click that raises the window, focuses a widget and repaints; the
    // first character injected straight after it was swallowed, and the shell read `esktop`,
    // which resolves to nothing. Sleeping only *between* keys leaves exactly that one gap.
    for c in line.chars() {
        std::thread::sleep(PER_KEY);
        // **One character map, in `qcode_for`.** This loop handled lowercase and space,
        // which is all `check-terminal` ever typed; `check-install` types a disk's identity
        // back — uppercase, quotes, brackets — and a second map would be a second thing to
        // keep correct. A character neither knows is an error there rather than a silent
        // skip, because a dropped character makes a *different command line*.
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

/// **Type an answer at a prompt a program is waiting at, then Enter** — a password at `with`'s, a
/// name or a password at `nxinstall`'s. Paced like [`type_at_terminal`], before every key, but with
/// **no leading Enter**: at a prompt it would be an empty answer.
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

/// Inject one key by qcode, press and release.
pub(crate) fn press(qmp: &mut Qmp, qcode: &str) -> R<()> {
    qmp.send_key(qcode, true)?;
    qmp.send_key(qcode, false)?;
    Ok(())
}

/// **`boot-probe`'s verdict, `PASS`**, waited for up to a minute: what a gate booting the test
/// image owes the machine it boots (PR #358 review). Only `test-qemu` exits on the verdict, so a
/// gate that never read it passed while the probe failed — `check-input --usb` did, on the one
/// machine with no i8042. Read from the transcript rather than with `expect`, which would consume
/// whatever the gate waits for next, and as a whole line: the verdict's word can arrive after its
/// prefix.
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

/// **The PS/2 keyboard's lights since the kernel reset it** (Phase 6 Part B.5), read from QEMU's
/// `ps2_set_ledstate` trace: each value the keyboard was set to, in PS/2's order — Scroll Lock in
/// bit 0, Num Lock in bit 1, Caps Lock in bit 2 — starting with the reset's own zero.
///
/// **Only what follows the last reset is the system's**: the firmware resets the keyboard and sets
/// its lights before the kernel does, and the kernel's reset is the last.
fn ps2_lights(log: &Path) -> Vec<u8> {
    ps2_lights_in(&fs::read_to_string(log).unwrap_or_default())
}

/// [`ps2_lights`] of the trace's text. **The value is the line's last word**: the event's own name
/// holds `ledstate` too — `ps2_set_ledstate 0x… ledstate 2` — so splitting at the word finds the
/// device's address first.
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

/// Wait until the keyboard's lights since its reset are **exactly** `want` — not merely ending
/// with it, so a write too many fails here rather than passing as the next step's value.
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
/// followed by `shows` — which `line` then is. The echo is the pacing, as in `check-terminal`'s
/// first typing. **A row is reported with its trailing blanks trimmed**, so a space is waited for
/// as the row before it, reported again.
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

/// The QMP key code for `c`, and whether it needs shift held.
///
/// Refuses what it does not know rather than skipping it: a character silently dropped from a
/// command line produces a *different command*, and here that is the difference between naming
/// the disk correctly and naming a different disk.
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

/// Type `text` at the greeter, one character at a time, waiting for each redraw.
///
/// **One at a time and waited for**, for the reason `check-terminal`'s typing loop gives: a
/// word injected as fast as QMP can send it outruns a client that repaints between keystrokes,
/// and the tail lands on a client that is not looking. A human types slower than this loop; a
/// harness does not.
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
        // The redraw is the receipt, and its number is not asserted — only that one happened.
        //
        // **One receipt is not one keystroke.** The greeter drains every queued event and
        // presents once, so two keys that arrive in the same pump produce one redraw and this
        // loop runs a receipt ahead until the next key supplies it. Measured on a passing
        // run: 20 keystrokes produced 20 redraws in one phase and 34 produced 33 in another.
        // That is fine and self-correcting — nothing is lost, and the drift stays far under
        // the outbox's depth — but it is pacing, not a per-character acknowledgement, and the
        // comment said otherwise (PR #236 review, finding 5).
        session.expect("desktop-session-mgr: greeter redraw ")?;
    }
    Ok(())
}
