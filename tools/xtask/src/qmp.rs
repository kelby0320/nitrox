//! The QMP connection: keys, buttons, the wheel and pointer motion injected, and screendumps.

use crate::*;

/// How many events [`Qmp::connect`] reads past before it must see QEMU's greeting.
pub(crate) const MAX_EVENTS_BEFORE_GREETING: usize = 8;

/// A minimal QMP client over a Unix socket, without a JSON library: it reads back only `"return"`
/// or `"error"`.
pub(crate) struct Qmp {
    stream: std::os::unix::net::UnixStream,
    buf: Vec<u8>,
    /// Where the guest's pointer is, when a press receipt has confirmed it: **the pointer is
    /// relative and unacknowledged, so this is the only way to know**. Cleared whenever an attempt
    /// does not land.
    pub(crate) pointer: Option<(i32, i32)>,
    /// The size of the screen the guest was booted with, which is where the pointer's clamp is;
    /// [`move_pointer_to`] refuses to pin without it rather than assume a corner.
    pub(crate) screen: Option<DisplaySize>,
}

impl Qmp {
    /// Connect and complete the capabilities handshake, retrying briefly: QEMU creates the socket
    /// as it starts.
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
        // An event can arrive before the greeting (QEMU starts the guest before greeting): skip a
        // few, so a socket that never greets still fails.
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

    /// Send one command and wait for its reply, skipping the asynchronous events QEMU interleaves.
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
            return Err(format!("unexpected qmp reply: {line}").into());
        }
    }

    /// Run a **human monitor** command through QMP and return its text output.
    pub(crate) fn hmp(&mut self, cmd: &str) -> R<String> {
        let escaped = cmd.replace('\\', "\\\\").replace('"', "\\\"");
        let reply = self
            .execute(&format!(r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{escaped}"}}}}"#))?;
        Ok(unescape_json_return(&reply))
    }

    pub(crate) fn send_key(&mut self, qcode: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"key",
               "data":{{"down":{down},"key":{{"type":"qcode","data":"{qcode}"}}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    pub(crate) fn send_button(&mut self, button: &str, down: bool) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[{{"type":"btn",
               "data":{{"down":{down},"button":"{button}"}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Turn the wheel one detent: **a button, not an axis**, in QEMU's model, and only the *down*
    /// edge counts.
    pub(crate) fn send_wheel(&mut self, down: bool) -> R<()> {
        let button = if down { "wheel-down" } else { "wheel-up" };
        self.send_button(button, true)?;
        self.send_button(button, false)
    }

    pub(crate) fn send_motion(&mut self, dx: i32, dy: i32) -> R<()> {
        self.execute(&format!(
            r#"{{"execute":"input-send-event","arguments":{{"events":[
               {{"type":"rel","data":{{"axis":"x","value":{dx}}}}},
               {{"type":"rel","data":{{"axis":"y","value":{dy}}}}}]}}}}"#
        ))?;
        Ok(())
    }

    /// Make QEMU send what mouse state it is holding: an event that moves nothing, which syncs the
    /// device. **It cannot add input, only deliver it**, and no more than four packets, 508 px an
    /// axis. See [`expect_after_pointer`].
    pub(crate) fn flush(&mut self) -> R<()> {
        self.send_motion(0, 0)
    }

    pub(crate) fn screendump(&mut self, path: &Path) -> R<()> {
        let p = path.to_str().ok_or("screendump path is not UTF-8")?;
        self.execute(&format!(r#"{{"execute":"screendump","arguments":{{"filename":"{p}"}}}}"#))?;
        Ok(())
    }
}

/// Pull the string value of a QMP `"return"` field out of a reply line and undo the JSON escaping.
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
