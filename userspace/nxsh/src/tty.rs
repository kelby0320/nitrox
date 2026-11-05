//! The session's terminal: a channel to the tty server, spoken in `librsproto` requests.

use crate::notices::{Notices, stop};
use alloc::rc::Rc;
use libkern::abi::KIND_TERMINATE_REQUESTED;
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, Ref, WaitSet};

/// A reply: whether it is an error, how much of its body was copied out, and the handles it brought.
type Reply = (bool, usize, Carried);

pub(crate) struct Tty {
    chan: Handle<Channel, Only>,
    notices: Rc<Notices>,
}

impl Tty {
    pub(crate) fn new(chan: Handle<Channel, Only>, notices: Rc<Notices>) -> Self {
        Tty { chan, notices }
    }

    pub(crate) fn channel(&self) -> Ref<'_, Channel, Only> {
        self.chan.as_ref()
    }

    /// Send one tty request and wait for its reply, copying the body into `out`; `None` if the
    /// exchange failed.
    fn request(&self, op: u16, body: &[u8], out: &mut [u8]) -> Option<Reply> {
        self.exchange(op, body, out, false)
    }

    /// [`request`](Self::request), and when `watch`, the notification channel watched for a
    /// terminate request.
    fn exchange(&self, op: u16, body: &[u8], out: &mut [u8], watch: bool) -> Option<Reply> {
        let mut msg = Msg::new();
        let len = librsproto::encode(msg.payload_mut(), op, 1, 0, body, 0)?;
        msg.set_len(len).ok()?;
        self.chan.send(&msg, &mut Carried::new()).ok()?;
        self.await_reply(out, watch)
    }

    /// Wait for the reply to the request just sent, stepping over any interrupt event.
    ///
    /// Re-entered because the interrupt is the one unsolicited message on this channel (§11h) and
    /// the reply is still coming behind it. When `watch`, the notification channel is watched too,
    /// so a terminate request finds the shell at the prompt's read; only the prompt watches, since
    /// elsewhere a stage's `ChildExited` could be taken from the reap.
    fn await_reply(&self, out: &mut [u8], watch: bool) -> Option<Reply> {
        let mut msg = Msg::new();
        loop {
            self.wait_for_reply(watch)?;
            let sent = self.chan.recv(&mut msg).ok()?;
            let m = librsproto::decode(msg.payload()).ok()?;
            // The interrupt is unsolicited, so it can land where a reply was expected: record it and
            // keep waiting, or the real reply stays queued for whoever asked next.
            if m.op == librsproto::OP_TTY_INTERRUPT {
                self.notices.interrupt();
                continue;
            }
            let n = m.body.len().min(out.len());
            out[..n].copy_from_slice(&m.body[..n]);
            return Some((m.is_error(), n, sent));
        }
    }

    fn wait_for_reply(&self, watch: bool) -> Option<()> {
        let notify = self.notices.notify().filter(|_| watch);
        loop {
            let mut set = WaitSet::<2>::new();
            let _ = set.add(&self.chan);
            if let Some(notify) = notify {
                let _ = set.add(notify);
            }
            let found = set.wait(Deadline::NEVER).ok().filter(|s| !s.is_empty())?;
            let Some(notify) = notify else { return Some(()) };
            if found.contains(1) {
                // Whatever else is queued is dropped here: only a terminate request is ours.
                while let Ok(Some(n)) = notify.recv() {
                    if n.kind == KIND_TERMINATE_REQUESTED {
                        stop();
                    }
                }
            }
            if found.contains(0) {
                return Some(());
            }
        }
    }

    /// Take whatever keystrokes are available. `None` if the terminal is unusable.
    pub(crate) fn read(&self, out: &mut [u8]) -> Option<usize> {
        match self.exchange(librsproto::OP_TTY_READ, &[], out, true) {
            Some((false, n, _)) => Some(n),
            _ => None,
        }
    }

    /// Turn the terminal's own echo on or off; the shell holds the line buffer, so server-side echo
    /// would fight it.
    pub(crate) fn set_echo(&self, on: bool) {
        let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
        let mut scratch = [0u8; 1];
        let _ = self.request(librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
    }

    pub(crate) fn write(&self, text: &[u8]) {
        let mut scratch = [0u8; 1];
        // In parts: a body over `RS_MAX_BODY` is refused whole, and the screen and the line part.
        for part in text.chunks(librsproto::RS_MAX_BODY) {
            let _ = self.request(librsproto::OP_TTY_WRITE, part, &mut scratch);
        }
    }

    pub(crate) fn write_crlf(&self, text: &str) {
        for chunk in text.split('\n') {
            if !chunk.is_empty() {
                self.write(chunk.as_bytes());
            }
            self.write(b"\r\n");
        }
    }

    /// Ask the tty server for another terminal on this one's backend (`Tty::OpenSibling`), for a
    /// stage to prompt on. `None` if it refused or the exchange failed; the stage then runs without
    /// one.
    pub(crate) fn open_sibling(&self) -> Option<Handle<Channel, Only>> {
        let mut scratch = [0u8; 1];
        match self.request(librsproto::OP_TTY_OPEN_SIBLING, &[], &mut scratch) {
            Some((false, _, mut sent)) => sent.take(0),
            _ => None,
        }
    }

    /// Take every pending message from the terminal without blocking, recording a `Ctrl-C` for the
    /// evaluator's checkpoint too; whether one came.
    pub(crate) fn poll_interrupt(&self) -> bool {
        let mut hit = false;
        let mut msg = Msg::new();
        // WouldBlock (nothing queued) is the common case.
        while self.chan.recv(&mut msg).is_ok() {
            if let Ok(m) = librsproto::decode(msg.payload())
                && m.op == librsproto::OP_TTY_INTERRUPT
            {
                self.notices.interrupt();
                hit = true;
            }
        }
        hit
    }
}
