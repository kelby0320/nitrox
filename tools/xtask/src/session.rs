//! The serial session a gate drives: lines typed at the guest, and its output waited on.

use crate::*;

/// A driven QEMU serial session: write lines, wait for text.
pub(crate) struct Session {
    pub(crate) child: std::process::Child,
    /// Which gate this is, so a failing run's transcript does not overwrite another gate's.
    /// A batch of runs is usually what you are comparing, and a single fixed filename means
    /// every failure destroys the evidence from the one before it — which happened to a
    /// reviewer mid-review (PR #197).
    gate: &'static str,
    /// Everything the guest has printed, accumulated by a reader thread.
    out: std::sync::Arc<std::sync::Mutex<String>>,
    /// When each chunk of `out` arrived: `(end offset, instant)`, in order. For a gate that times
    /// the guest, which the moments `expect` returns cannot do — two lines that arrive in one
    /// burst are both buffered by the time the first `expect` sees either.
    arrivals: std::sync::Arc<std::sync::Mutex<Vec<(usize, std::time::Instant)>>>,
    /// How far `expect` has already matched, so each step scans only new output and a
    /// pattern cannot be satisfied by an earlier occurrence of itself.
    cursor: usize,
    /// Every line typed at the guest since the last successful [`expect`](Self::expect), so
    /// that call can refuse a pattern the guest's own echo would satisfy. Empty for gates
    /// that type over QMP rather than the serial line, which is every gate but
    /// `test-interactive`.
    ///
    /// **A list, not the most recent line.** The unconsumed-echo window is not one send: two
    /// sends before an `expect` leave *both* echoes ahead of the cursor, and a guard that
    /// remembered only the second would wave the first one's text straight through — the one
    /// shape that walks past a guard whose whole purpose is preventing recurrence. Cleared on
    /// a successful match because the cursor has then advanced past those echoes, so they can
    /// no longer satisfy anything.
    sent_since_match: Vec<String>,
}

impl Drop for Session {
    /// Kill the guest when the session goes out of scope.
    ///
    /// Every `?` between `spawn` and an explicit `kill` used to leak a QEMU, and a leaked
    /// QEMU holds the write lock on `tools/build-cache/nitrox.hdd` — so the *next* run boots
    /// a guest that cannot open its disk and fails with a bare timeout and no serial output,
    /// which looks nothing like the original error. A reviewer lost a pass to a 20-hour-old
    /// orphan this way (PR #175 review). Killing an already-dead child is harmless, so the
    /// explicit kills elsewhere stay valid.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // **Kept on request, pass or fail**, for `check-resolutions`, which runs each gate as a
        // child and wants every boot's transcript beside its result. A gate run by hand keeps its
        // transcript only on a timeout, as it always has — so a passing rerun does not overwrite
        // the evidence of the failure before it.
        if let Ok(path) = env::var("NITROX_KEEP_TRANSCRIPT")
            && let Ok(g) = self.out.lock()
        {
            let _ = fs::write(path, g.as_str());
        }
    }
}

/// Take the text in `pending` that is whole, leaving a character a read stopped in the middle of
/// for the next one.
///
/// **A read's boundary is not a character's.** Decoding each chunk on its own turned a multi-byte
/// character split across two reads into two replacement characters, so an `expect` for text
/// containing it could never match: `test-interactive` timed out waiting for `sleep — exited` with
/// `sleep �� exited` sitting in the transcript (administration Part A.6). Bytes that are simply
/// invalid still decode lossily, as before.
pub(crate) fn take_utf8(pending: &mut Vec<u8>) -> String {
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let text = text.to_string();
            pending.clear();
            text
        }
        // An incomplete sequence at the end: keep it, and take everything before it.
        Err(e) if e.error_len().is_none() => {
            let cut = e.valid_up_to();
            let text = String::from_utf8_lossy(&pending[..cut]).into_owned();
            pending.drain(..cut);
            text
        }
        Err(_) => {
            let text = String::from_utf8_lossy(pending).into_owned();
            pending.clear();
            text
        }
    }
}

impl Session {
    pub(crate) fn spawn(mut cmd: Command, gate: &'static str) -> R<Session> {
        let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;
        let stdout = child.stdout.take().ok_or("qemu stdout")?;
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let arrivals = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = out.clone();
        let clock = arrivals.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut r = stdout;
            let mut buf = [0u8; 1024];
            // Bytes of a character a read ended in the middle of — see `take_utf8`.
            let mut pending: Vec<u8> = Vec::new();
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let now = std::time::Instant::now();
                pending.extend_from_slice(&buf[..n]);
                if let Ok(mut g) = sink.lock() {
                    g.push_str(&take_utf8(&mut pending));
                    if let Ok(mut a) = clock.lock() {
                        a.push((g.len(), now));
                    }
                }
            }
        });
        Ok(Session { child, out, arrivals, cursor: 0, gate, sent_since_match: Vec::new() })
    }

    /// The rest of the line the last [`expect`](Self::expect) matched on.
    ///
    /// For the few assertions that need a *value* the guest computed rather than a string it
    /// was always going to print — a window's id and geometry, say, which the host cannot know
    /// and must not guess. Read after `expect` has moved the cursor past the pattern.
    pub(crate) fn rest_of_line(&self) -> R<String> {
        // **Wait for the newline.** `expect` returns the instant the *prefix* appears, and the
        // guest's line reaches the host serial in whatever chunks `read` happens to return — so
        // taking what is there can take half a line. Reading `"… popup 138 at 0,"` and failing
        // to parse it would be a rare, confusing gate failure with nothing wrong in the guest
        // (PR #223 review, finding 6).
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let tail = &g[self.cursor..];
                if let Some(end) = tail.find('\n') {
                    return Ok(tail[..end].trim().to_string());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err("the guest never finished the line after the matched prefix".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The **whole** line the last [`expect`](Self::expect) matched on, once it has ended — for a
    /// line whose values sit on both sides of the pattern, such as a device's `blk-<n>` before it
    /// and its mount after.
    ///
    /// **Waited for, as [`rest_of_line`](Self::rest_of_line) is**, and for the same reason. Two
    /// gates took the line straight from the transcript the moment its prefix matched, and on KVM
    /// `check-storage` read `… mounted at /storage/nitrox-r` and failed with nothing wrong in the
    /// guest (PR #352's CI).
    pub(crate) fn matched_line(&self) -> R<String> {
        self.rest_of_line()?;
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        let start = g[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = g[self.cursor..].find('\n').map_or(g.len(), |i| self.cursor + i);
        Ok(g[start..end].trim().to_string())
    }

    /// The first whole line from transcript offset `from` on that holds `pat` and whose rest
    /// satisfies `want` — **whether or not an `expect` has already scanned past it** — and the rest
    /// of it after `pat`. Moves the cursor past it when it is ahead of the cursor.
    ///
    /// **For a line another process prints in answer to the same event as the line just
    /// expected.** `expect` consumes what it scans past, so waiting for one process's receipt and
    /// then for the other's line fails whenever the other spoke first, and `expect_all` cannot
    /// tell the answer from an older line of the same shape that is still unread. Take `from`
    /// before the action, and only what came after it counts (PR #353's CI: `nxedit` destroys a
    /// window and then says so, and the shell's list of what is left can land in between).
    pub(crate) fn line_since(
        &mut self,
        from: usize,
        pat: &str,
        want: impl Fn(&str) -> bool,
        timeout: std::time::Duration,
    ) -> R<String> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let mut at = from.min(g.len());
                // Whole lines only: a line still arriving can satisfy `want` with half its text.
                while let Some(nl) = g[at..].find('\n') {
                    let line = &g[at..at + nl];
                    if let Some(i) = line.find(pat) {
                        let rest = line[i + pat.len()..].trim();
                        if want(rest) {
                            let rest = rest.to_string();
                            self.cursor = self.cursor.max(at + nl + 1);
                            self.sent_since_match.clear();
                            return Ok(rest);
                        }
                    }
                    at += nl + 1;
                }
            }
            if std::time::Instant::now() > deadline {
                let seen: Vec<String> = {
                    let g = self.out.lock().map_err(|_| "transcript lock")?;
                    g[from.min(g.len())..].lines().filter(|l| l.contains(pat)).map(str::to_string).collect()
                };
                return Err(format!(
                    "no line holding {pat:?} since the action was the one wanted within {timeout:?}; \
                     the ones that came: {seen:?}"
                )
                .into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Wait up to `timeout` for `pat`, answering **whether it arrived** rather than failing.
    ///
    /// For the one case where absence is retryable rather than a verdict: a click whose
    /// positioning a dropped PS/2 packet left short. [`expect`](Self::expect) is right
    /// everywhere else — a gate that treats a missing guest line as "try again" is a gate that
    /// can pass while the guest is broken.
    pub(crate) fn expect_within(&mut self, pat: &str, timeout: std::time::Duration) -> R<bool> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                if let Some(i) = g[self.cursor..].find(pat) {
                    self.cursor += i + pat.len();
                    self.sent_since_match.clear();
                    println!("  ok: saw {pat:?}");
                    return Ok(true);
                }
            }
            if std::time::Instant::now() > deadline {
                return Ok(false);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The **last** line starting `prefix` once such lines have stopped arriving for `quiet` —
    /// the rest of it after `prefix` — or `None` if none arrived before `limit`. Consumes
    /// everything read.
    ///
    /// **For a receipt that the path to a position can also produce.** An injected move is a run
    /// of relative packets, and the guest reports every row the pointer crosses on the way, not
    /// only the one it stops on. `expect_within` takes the *first* matching line, so a walk that
    /// polled for "hover over row N" accepted a row the pointer merely passed through, and clicked
    /// where it had stopped instead — which is what `check-terminal` did once the desktop
    /// refresh's Part B moved a menu popup four pixels, putting the approach's first 100-pixel
    /// step on the very row the walk was looking for. The row the pointer is *over* is the last
    /// one it reported, and only once it has stopped reporting.
    pub(crate) fn settled_line(
        &mut self,
        prefix: &str,
        quiet: std::time::Duration,
        limit: std::time::Duration,
    ) -> R<Option<String>> {
        let start = std::time::Instant::now();
        let mut last: Option<String> = None;
        let mut changed = start;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                // **Whole lines only**: the reader appends as bytes arrive, and a half-written
                // `hover 10` is not row 10. The cursor stops at the last complete line, so the
                // rest is read on the next poll.
                let (receipt, whole) = last_receipt(&g[self.cursor..], prefix);
                if let Some(r) = receipt {
                    last = Some(r);
                    // Only a receipt restarts the wait: other output — a heartbeat — says nothing
                    // about whether the pointer is still moving.
                    changed = std::time::Instant::now();
                }
                self.cursor += whole;
            }
            let now = std::time::Instant::now();
            if last.is_some() && now.duration_since(changed) >= quiet {
                return Ok(last);
            }
            if now.duration_since(start) >= limit {
                return Ok(last);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Drop everything said so far without matching it — for discarding an abandoned attempt's
    /// output, so a retry cannot match a line the previous try emitted.
    pub(crate) fn skip_to_end(&mut self) -> R<()> {
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        self.cursor = g.len();
        Ok(())
    }

    /// Wait for `pat` in output not yet consumed. The guest paces the test.
    pub(crate) fn expect(&mut self, pat: &str) -> R<()> {
        // **An `expect` the guest's echo can satisfy is not an assertion.** The terminal
        // echoes what the harness types and `expect` takes the first match, which is always
        // the echo — emitted as the line is sent, long before anything evaluates it. Such a
        // step passes against a guest with the feature removed entirely.
        //
        // The convention is to wrap the answer so the expected text cannot appear in the
        // command (`format("n={}", n)` → `n=3`), and it is stated twice in
        // `run_interactive_scenarios`, at steps 12 and 13, along with the history of how it
        // was learned. It was still violated twice in that same function — a comment cannot
        // fail a build, so this does.
        //
        // **What this does not cover**, so nobody reads it as more than it is: it knows only
        // what the harness typed. A line the guest *re-emits* from its own history (Up-arrow,
        // Ctrl-R — steps 8-11) was never sent by us and is invisible here, which is why those
        // steps assert on command output instead. Neither does it cover the other half of the
        // problem — a pattern satisfied by a background service's logging rather than by the
        // shell — because nothing can tell the harness what a service might print. See the
        // 2026-08-18 decision-log entry.
        if let Some(echoed) = echo_source(&self.sent_since_match, pat) {
            return Err(format!(
                "expect({:?}) is satisfied by the guest's echo of the text just typed \
                 ({:?}), which it emits as the line is sent — so this step would pass \
                 against a guest that never evaluated the command. Wrap the answer so \
                 the pattern cannot occur in what was typed: \
                 `format(\"label={{}}\", …)` then expect(\"label=…\").",
                pat, echoed
            )
            .into());
        }
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                if let Some(i) = g[self.cursor..].find(pat) {
                    self.cursor += i + pat.len();
                    // The cursor is now past those echoes; they cannot satisfy anything else.
                    self.sent_since_match.clear();
                    println!("  ok: saw {pat:?}");
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(self.timeout_report(&format!("{pat:?}"), TIMEOUT)?.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Wait for **all** of `pats`, in whatever order the guest emits them.
    ///
    /// **For lines whose order is not the guest's to guarantee.** [`expect`](Self::expect)
    /// consumes forward — a match moves the cursor past it — so a chain of them asserts a
    /// *sequence*, and that is the right shape when one line causes the next. It is the wrong
    /// shape when two processes react to the same event: they are scheduled independently on
    /// four vCPUs and the order they reach the serial console in is not a property of the
    /// system. Asserted as a sequence, the pair passes until the day the other one wins, and
    /// then fails by scanning *past* the line it will ask for next — so the timeout names a
    /// line that is sitting in the transcript, which reads like a lost message and is not one.
    ///
    /// Observed on 2026-08-31: maximising the terminal makes `desktop-shell` log the window's
    /// new geometry and `nxterm` log the size it took, both downstream of one compositor apply
    /// and neither downstream of the other.
    ///
    /// **This is not a licence to unorder an assertion that is merely inconvenient.** A pair
    /// belongs here only when nothing in the system orders it — where one line's producer sends
    /// the message that causes the other's, the sequence is the assertion. `configure_window`
    /// logging before it sends is what makes every `… window N to …` → `nxterm: resized …` pair
    /// in this file a genuine chain.
    pub(crate) fn expect_all(&mut self, pats: &[&str]) -> R<()> {
        for pat in pats {
            if let Some(echoed) = echo_source(&self.sent_since_match, pat) {
                return Err(format!(
                    "expect_all({pat:?}) is satisfied by the guest's echo of the text just \
                     typed ({echoed:?}) — see `expect`, which explains the whole trap."
                )
                .into());
            }
        }
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let found: Vec<(usize, &str)> =
                    pats.iter().filter_map(|p| g[self.cursor..].find(p).map(|i| (i + p.len(), *p))).collect();
                if found.len() == pats.len() {
                    // **Past the last of them, not the first.** The cursor is what stops a
                    // later step matching a line this one already accounted for, and the whole
                    // point here is that which of these is last is not known in advance.
                    let mut found = found;
                    found.sort_by_key(|(end, _)| *end);
                    for (_, p) in &found {
                        println!("  ok: saw {p:?}");
                    }
                    self.cursor += found.last().map_or(0, |(end, _)| *end);
                    self.sent_since_match.clear();
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                let missing: Vec<&str> = pats.iter().copied().filter(|p| !g[self.cursor..].contains(p)).collect();
                drop(g);
                // Naming the ones that *did* arrive is the point: an unordered wait that times
                // out with one of two lines present is a different fault from one with neither.
                let what = format!("all of {pats:?}, still missing {missing:?}");
                return Err(self.timeout_report(&what, TIMEOUT)?.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// What a timed-out wait reports, and the transcript it saves on the way out.
    ///
    /// Shared so that every wait fails the same way, and **distinguishes "the guest said
    /// nothing" from "the guest stopped early".** They are the same line in the log and
    /// completely different faults: no output at all usually means QEMU never got as far as the
    /// guest — a disk held open by an orphaned instance, a missing image, bad firmware — while
    /// partial output is a real hang, and the tail says where.
    ///
    /// **The whole transcript goes to a file, not just the tail.** The tail is what you read
    /// first and it is almost never enough: these gates fail *between* two things the guest
    /// said, so the interesting part is the hundred lines before the end. The transcript was
    /// already accumulated in full and only ever printed truncated, which cost three separate
    /// investigations in M5 and M6 — each one reduced to guessing at a guest whose log existed
    /// and was thrown away.
    pub(crate) fn timeout_report(&self, waiting_for: &str, timeout: std::time::Duration) -> R<String> {
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        if g.trim().is_empty() {
            return Ok(format!(
                "timed out after {timeout:?} waiting for {waiting_for}, and the guest \
                 produced NO output at all — this is usually QEMU failing to start \
                 rather than a hang (a stale qemu-system-x86_64 holding \
                 build-cache/nitrox.hdd will do it; check with `pgrep -a qemu`)"
            ));
        }
        let tail: String = g.chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        let path = build_cache().join(format!("guest-transcript-{}.log", self.gate));
        let saved = fs::write(&path, g.as_str()).is_ok();
        let where_ = if saved { format!("\n\nthe full transcript is at {}", path.display()) } else { String::new() };
        Ok(format!("timed out after {timeout:?} waiting for {waiting_for}; last output was:\n{tail}{where_}"))
    }

    /// Send bytes with **no trailing newline** — for keys that are not a line.
    ///
    /// `Ctrl-C` is the case that needs it: appending `\n` would submit the line as well as
    /// interrupt, and the two behaviours are exactly what the step is trying to tell apart.
    pub(crate) fn send_raw(&mut self, bytes: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(bytes.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(bytes.as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// Type a line, Enter included.
    pub(crate) fn send(&mut self, line: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(line.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(format!("{line}\n").as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// When the text just matched by the last [`expect`](Self::expect) reached the host — the
    /// arrival of the chunk holding its final byte.
    ///
    /// **Resolution is a chunk**: the reader's `read` sizes set the boundaries, so two lines in one
    /// chunk read as simultaneous. Enough for `check-live`'s 1.5 s bound against a 3 s failure; not
    /// a precise measurement.
    pub(crate) fn matched_at(&self) -> std::time::Instant {
        let end = self.cursor.saturating_sub(1);
        self.arrivals
            .lock()
            .ok()
            .and_then(|a| a.iter().find(|(chunk_end, _)| *chunk_end > end).map(|(_, t)| *t))
            .unwrap_or_else(std::time::Instant::now)
    }

    /// Everything the guest has said so far, without consuming any of it.
    ///
    /// For a failure message that has to say *what did* happen rather than only what did not —
    /// [`finish`](Self::finish) takes the session, which a `return Err` in the middle of a gate
    /// cannot do.
    pub(crate) fn transcript(&self) -> String {
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Kill the guest and hand back the transcript.
    pub(crate) fn finish(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }
}
