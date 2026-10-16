use crate::*;

/// How many events [`Qmp::connect`] reads past before it must see QEMU's greeting.
pub(crate) const MAX_EVENTS_BEFORE_GREETING: usize = 8;

/// A minimal QMP client over a Unix socket.
///
/// QEMU's machine protocol is line-delimited JSON. This speaks just enough of it for the
/// display gate — a handshake and one command — without a JSON library: the messages
/// sent are fixed strings, and the only thing read back is whether a reply line carries
/// `"return"` or `"error"`.
///
/// QMP rather than the human monitor because it is the supported interface and because
/// Milestone 3 needs `input-send-event` from the same channel; adding HMP now would mean
/// replacing it then.
pub(crate) struct Qmp {
    stream: std::os::unix::net::UnixStream,
    buf: Vec<u8>,
    /// Where the guest's pointer is, when a press receipt has confirmed it.
    ///
    /// **The pointer is relative and unacknowledged, so this is the only way to know.** Without
    /// it every click re-pins to a corner with twenty over-driven motions before walking, and
    /// that burst is what overruns the guest's input ring: `input batch DROPPED (SYN_DROPPED)`,
    /// then a press tens of pixels from where it was aimed. A confirmed press *is* a position
    /// report, so consecutive clicks cost two motions instead of thirty-four
    /// (PR #243 review, finding 3). Cleared whenever an attempt does not land, because then it
    /// is not known any more.
    pub(crate) pointer: Option<(i32, i32)>,
    /// The size of the screen the guest was booted with, which is where the pointer's clamp is.
    /// A gate that moves the pointer sets it; [`move_pointer_to`] refuses to pin without it
    /// rather than assume a corner (Phase 5 Part E: it assumed `(1279, 799)`).
    pub(crate) screen: Option<DisplaySize>,
}

impl Qmp {
    /// Connect and complete the capabilities handshake.
    ///
    /// QEMU creates the socket as it starts, so this retries briefly rather than
    /// requiring the caller to guess how long that takes.
    pub(crate) fn connect(path: &Path) -> R<Qmp> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let stream = loop {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(s) => break s,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(format!("connect QMP socket {}: {e}", path.display()).into()),
            }
        };
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(30)))
            .map_err(|e| format!("qmp read timeout: {e}"))?;
        let mut q = Qmp { stream, buf: Vec::new(), pointer: None, screen: None };
        // The greeting arrives unsolicited; then capabilities must be negotiated before
        // any other command is accepted.
        //
        // **An event can arrive before it.** CI's Display gate failed on 2026-09-23 with a
        // `RESUME` event as the first line on the socket: this connects the moment the socket
        // exists, and QEMU started the guest before it had greeted the connection. An event is
        // not an answer to anything, so it is skipped here as `execute` skips one — but only a
        // few, so a socket that never greets still fails rather than reading forever.
        let mut greeting = q.read_line()?;
        for _ in 0..MAX_EVENTS_BEFORE_GREETING {
            if !greeting.contains("\"event\"") {
                break;
            }
            greeting = q.read_line()?;
        }
        if !greeting.contains("QMP") {
            return Err(format!("unexpected QMP greeting: {greeting}").into());
        }
        q.execute(r#"{"execute":"qmp_capabilities"}"#)?;
        Ok(q)
    }

    /// Read one newline-delimited message.
    fn read_line(&mut self) -> R<String> {
        use std::io::Read as _;
        loop {
            if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let line = String::from_utf8_lossy(&self.buf[..i]).into_owned();
                self.buf.drain(..=i);
                return Ok(line);
            }
            let mut chunk = [0u8; 4096];
            let n = self.stream.read(&mut chunk).map_err(|e| format!("qmp read: {e}"))?;
            if n == 0 {
                return Err("qmp socket closed".into());
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send one command and wait for its reply, skipping asynchronous events.
    ///
    /// QEMU interleaves `{"event": ...}` messages with replies, so a reader that took the
    /// next line as the answer would eventually read an event and misreport success.
    pub(crate) fn execute(&mut self, json: &str) -> R<String> {
        use std::io::Write as _;
        self.stream.write_all(format!("{json}\n").as_bytes()).map_err(|e| format!("qmp write: {e}"))?;
        self.stream.flush().map_err(|e| format!("qmp flush: {e}"))?;
        loop {
            let line = self.read_line()?;
            if line.contains("\"event\"") {
                continue; // asynchronous event, not our reply
            }
            if line.contains("\"error\"") {
                return Err(format!("qmp command failed: {json} -> {line}").into());
            }
            if line.contains("\"return\"") {
                return Ok(line);
            }
            // Anything else is unexpected; surface it rather than looping forever.
            return Err(format!("unexpected qmp reply: {line}").into());
        }
    }

    /// Run a **human monitor** command through QMP and return its text output.
    ///
    /// The monitor is where the interesting diagnostics live — `info registers`, `info
    /// cpus` — and QMP has no structured equivalent for most of them.
    pub(crate) fn hmp(&mut self, cmd: &str) -> R<String> {
        let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
        let reply = self
            .execute(&format!(r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{escaped}"}}}}"#))?;
        Ok(unescape_json_return(&reply))
    }

    /// Inject a key press or release by QEMU `qcode` name (`"a"`, `"shift"`, `"esc"`).
    ///
    /// Injection is what makes an input driver testable at all: without it nothing can type
    /// at the guest, so the ISR, the event ring and the parked read are exercised only by a
    /// human (`display-substrate.md` §8d).
    pub(crate) fn send_key(&mut self, qcode: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"key",
               "data":{{"down":{down},"key":{{"type":"qcode","data":"{qcode}"}}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Inject a pointer button press or release (`"left"`, `"right"`, `"middle"`).
    pub(crate) fn send_button(&mut self, button: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"btn",
               "data":{{"down":{down},"button":"{button}"}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Turn the wheel one detent, up or down.
    ///
    /// **A button, not an axis**, which is QEMU's model rather than this one's: `wheel-up` and
    /// `wheel-down` are `InputButton` values, and the PS/2 backend turns a press of one into a
    /// signed `Z` in the packet it sends. The release is sent for symmetry and produces
    /// nothing — QEMU decrements or increments its accumulator on the *down* edge only.
    pub(crate) fn send_wheel(&mut self, down: bool) -> R<()> {
        let button = if down { "wheel-down" } else { "wheel-up" };
        self.send_button(button, true)?;
        self.send_button(button, false)
    }

    /// Inject relative pointer motion.
    pub(crate) fn send_motion(&mut self, dx: i32, dy: i32) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[
               {{"type":"rel","data":{{"axis":"x","value":{dx}}}}},
               {{"type":"rel","data":{{"axis":"y","value":{dy}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Make QEMU send what mouse state it is holding: an event that moves nothing, which syncs
    /// the device. **It cannot add input, only deliver it** — the packets it can produce carry
    /// buttons and motion that were already injected, and as many as the queue has room for:
    /// four at most, 508 px an axis. See [`expect_after_pointer`].
    pub(crate) fn flush(&mut self) -> R<()> {
        self.send_motion(0, 0)
    }

    /// Capture the guest's display to `path` as a binary PPM.
    pub(crate) fn screendump(&mut self, path: &Path) -> R<()> {
        let p = path.to_str().ok_or("screendump path is not UTF-8")?;
        self.execute(&format!(r#"{{"execute":"screendump","arguments":{{"filename":"{p}"}}}}"#))?;
        Ok(())
    }
}

/// Pull the string value of a QMP `"return"` field out of a reply line and undo the JSON
/// escaping. Hand-rolled because xtask carries no JSON dependency, and the shape here is
/// fixed: `{"return": "…"}`.
pub(crate) fn unescape_json_return(line: &str) -> String {
    let Some(start) = line.find(r#""return""#) else { return String::new() };
    let rest = &line[start + 8..];
    let Some(open) = rest.find('"') else { return String::new() };
    let body = &rest[open + 1..];
    let mut out = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('u') => {
                    // \uXXXX — consume the four digits; the monitor emits none in practice.
                    for _ in 0..4 {
                        let _ = chars.next();
                    }
                    out.push('?');
                }
                Some(other) => out.push(other),
                None => break,
            },
            _ => out.push(c),
        }
    }
    out
}
