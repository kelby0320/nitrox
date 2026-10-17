//! The serial session a gate drives: lines typed at the guest, and its output waited on.

use crate::*;

pub(crate) struct Session {
    pub(crate) child: std::process::Child,
    /// Which gate this is, so one gate's failing transcript does not overwrite another's.
    gate: &'static str,
    out: std::sync::Arc<std::sync::Mutex<String>>,
    /// When each chunk of `out` arrived: `(end offset, instant)`. Two lines arriving in one burst
    /// are both buffered by the time `expect` sees either, so only this can time the guest.
    arrivals: std::sync::Arc<std::sync::Mutex<Vec<(usize, std::time::Instant)>>>,
    /// How far `expect` has matched, so a pattern cannot match an earlier occurrence of itself.
    cursor: usize,
    /// Every line typed since the last successful [`expect`](Self::expect), so it can refuse a
    /// pattern the guest's own echo would satisfy. Empty for gates that type over QMP.
    ///
    /// **A list, not the latest line**: two sends before an `expect` leave both echoes ahead of the
    /// cursor.
    sent_since_match: Vec<String>,
}

impl Drop for Session {
    /// Kill the guest when the session goes out of scope: a leaked QEMU holds the write lock on
    /// `tools/build-cache/nitrox.hdd`, and the next run fails with a bare timeout.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // **Kept on request, pass or fail**, for `check-resolutions`; otherwise only a timeout
        // keeps it.
        if let Ok(path) = env::var("NITROX_KEEP_TRANSCRIPT")
            && let Ok(g) = self.out.lock()
        {
            let _ = fs::write(path, g.as_str());
        }
    }
}

/// Take the text in `pending` that is whole, leaving a character a read stopped in the middle of
/// for the next one. **A read's boundary is not a character's**: decoding each chunk alone turned a
/// split multi-byte character into two replacement characters, so an `expect` for text containing
/// it never matched.
pub(crate) fn take_utf8(pending: &mut Vec<u8>) -> String {
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let text = text.to_string();
            pending.clear();
            text
        }
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

    /// The rest of the line the last [`expect`](Self::expect) matched on, for a *value* the guest
    /// computed.
    pub(crate) fn rest_of_line(&self) -> R<String> {
        // **Wait for the newline**: `expect` returns the instant the *prefix* appears, and the line
        // arrives in whatever chunks `read` returns.
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

    /// The **whole** line the last [`expect`](Self::expect) matched on, once it has ended. **Waited
    /// for, as [`rest_of_line`](Self::rest_of_line) is**: on KVM `check-storage` read half a line.
    pub(crate) fn matched_line(&self) -> R<String> {
        self.rest_of_line()?;
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        let start = g[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let end = g[self.cursor..].find('\n').map_or(g.len(), |i| self.cursor + i);
        Ok(g[start..end].trim().to_string())
    }

    /// The first whole line from transcript offset `from` on that holds `pat` and whose rest
    /// satisfies `want`, **whether or not an `expect` has already scanned past it**, and the rest
    /// after `pat`. **For a line another process prints in answer to the same event as the one just
    /// expected**: `expect` consumes what it scans past, so waiting for one receipt and then the
    /// other's fails whenever the other spoke first. Take `from` before the action.
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

    /// Wait up to `timeout` for `pat`, answering **whether it arrived**: for the one retryable
    /// case, a click a dropped PS/2 packet left short. Anywhere else, a gate treating a missing
    /// line as "try again" passes while the guest is broken.
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

    /// The **last** line starting `prefix` once such lines have stopped arriving for `quiet`, the
    /// rest of it after `prefix`, or `None` if none arrived before `limit`. Consumes everything
    /// read. **For a receipt the path to a position also produces**: the guest reports every row
    /// the pointer crosses, and `expect_within` takes the *first*, so a walk accepted a row it
    /// merely passed through.
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
                // **Whole lines only**: a half-written `hover 10` is not row 10.
                let (receipt, whole) = last_receipt(&g[self.cursor..], prefix);
                if let Some(r) = receipt {
                    last = Some(r);
                    // Only a receipt restarts the wait.
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

    /// Drop everything said so far without matching it, so a retry cannot match the previous try's
    /// output.
    pub(crate) fn skip_to_end(&mut self) -> R<()> {
        let g = self.out.lock().map_err(|_| "transcript lock")?;
        self.cursor = g.len();
        Ok(())
    }

    pub(crate) fn expect(&mut self, pat: &str) -> R<()> {
        // **An `expect` the guest's echo can satisfy is not an assertion**: it takes the first
        // match, always the echo. Wrap the answer so the expected text cannot appear in the command
        // (`format("n={}", n)` → `n=3`). This does not cover a line the guest re-emits from its
        // history, or a background service's logging.
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

    /// Wait for **all** of `pats`, in whatever order the guest emits them. [`expect`](Self::expect)
    /// consumes forward, so a chain asserts a *sequence*; that is wrong when two processes react to
    /// the same event, as asserted so the pair fails the day the other wins, *past* the line it
    /// asks for next, and the timeout names a line sitting in the transcript. **Not a licence to
    /// unorder an assertion that is merely inconvenient**: where one line's producer causes the
    /// other's, the sequence is the assertion.
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
                    // **Past the last of them, not the first**: which is last is not known in
                    // advance.
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
                let what = format!("all of {pats:?}, still missing {missing:?}");
                return Err(self.timeout_report(&what, TIMEOUT)?.into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// What a timed-out wait reports, and the transcript it saves. It **distinguishes "the guest
    /// said nothing" from "the guest stopped early"**: no output usually means QEMU never got as
    /// far as the guest (a disk held by an orphaned instance, a missing image), while partial
    /// output is a real hang. **The whole transcript goes to a file**: these gates fail *between*
    /// two things the guest said.
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

    /// Send bytes with **no trailing newline**, for keys that are not a line (`Ctrl-C`).
    pub(crate) fn send_raw(&mut self, bytes: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(bytes.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(bytes.as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    pub(crate) fn send(&mut self, line: &str) -> R<()> {
        use std::io::Write as _;
        self.sent_since_match.push(line.to_string());
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(format!("{line}\n").as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// When the text just matched by the last [`expect`](Self::expect) reached the host: the
    /// arrival of the chunk holding its final byte. **Resolution is a chunk**, so two lines in one
    /// chunk read as simultaneous: enough for `check-live`'s 1.5 s bound against a 3 s failure.
    pub(crate) fn matched_at(&self) -> std::time::Instant {
        let end = self.cursor.saturating_sub(1);
        self.arrivals
            .lock()
            .ok()
            .and_then(|a| a.iter().find(|(chunk_end, _)| *chunk_end > end).map(|(_, t)| *t))
            .unwrap_or_else(std::time::Instant::now)
    }

    /// Everything the guest has said so far, without consuming any of it.
    pub(crate) fn transcript(&self) -> String {
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }

    pub(crate) fn finish(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }
}
