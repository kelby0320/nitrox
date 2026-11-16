//! A transport that answers as a compositor with no manager would, for host tests.

use alloc::vec::Vec;

use librsproto::surface::{
    ConfigureEvent, KeyEvent, OP_CONFIGURE, OP_CREATE_WINDOW, OP_KEY_EVENT, OP_POINTER_EVENT, OP_RELEASE, PointerEvent,
};

use crate::{Transport, UiError};

/// A [`Transport`] that records what the client sends and plays back queued events.
#[derive(Default)]
pub struct MockTransport {
    /// Every request sent: its op, its body and its handle.
    pub sent: Vec<(u16, Vec<u8>, Option<u64>)>,
    /// The id of the last window created; the next is one more.
    pub next_window: u32,
    /// Events the client will read, taken from the back.
    pub events: Vec<(u16, Vec<u8>)>,
    /// Events the compositor "sends" during a blocking wait, one per wait.
    pub deferred: Vec<(u16, Vec<u8>)>,
    /// How many times the client blocked in `wait_event` with nothing queued.
    pub waits: usize,
    /// Fail every request.
    pub fail: bool,
    /// Answer `CreateWindow`, then fail the wait for its `Configure`.
    pub fail_after_create: bool,
    /// An event was dropped for want of room, reported once by `took_loss`.
    pub lost: bool,
}

impl Transport for MockTransport {
    fn request(
        &mut self,
        op: u16,
        body: &[u8],
        handle: Option<u64>,
        reply: &mut [u8],
    ) -> Result<Option<usize>, UiError> {
        if self.fail {
            return Err(UiError::Transport);
        }
        self.sent.push((op, body.to_vec(), handle));
        if op == OP_CREATE_WINDOW {
            self.next_window += 1;
            let n =
                librsproto::surface::build_create_window_reply(reply, self.next_window).ok_or(UiError::Malformed)?;
            // `create` waits for a `Configure`; echo the request, as with no manager.
            let req = librsproto::surface::parse_create_window_request(body).ok_or(UiError::Malformed)?;
            let mut cfg = [0u8; 20];
            ConfigureEvent { window: self.next_window, width: req.width, height: req.height, x: 0, y: 0 }
                .write(&mut cfg)
                .ok_or(UiError::Malformed)?;
            self.events.insert(0, (OP_CONFIGURE, cfg.to_vec()));
            return Ok(Some(n));
        }
        Ok(None)
    }

    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        let Some((op, body)) = self.events.pop() else { return Ok(None) };
        // A body that does not fit is a loss, not a shorter body, as in `ChannelTransport`.
        if body.len() > buf.len() {
            self.lost = true;
            return Ok(None);
        }
        buf[..body.len()].copy_from_slice(&body);
        Ok(Some((op, body.len())))
    }

    fn took_loss(&mut self) -> bool {
        core::mem::take(&mut self.lost)
    }

    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        // `deferred` is what the compositor "sends" during a block. An event already queued
        // is not a wait, as in the real transport, or the create handshake would count.
        if self.fail_after_create {
            return Err(UiError::Transport);
        }
        if !self.events.is_empty() {
            return match self.poll_event(buf)? {
                Some(ev) => Ok(ev),
                None => Err(UiError::Transport),
            };
        }
        if let Some(ev) = self.deferred.pop() {
            self.events.push(ev);
        }
        self.waits += 1;
        match self.poll_event(buf)? {
            Some(ev) => Ok(ev),
            None => Err(UiError::Transport), // would block forever
        }
    }
}

impl MockTransport {
    /// Queue a key event for the last created window.
    pub fn queue_key(&mut self, keycode: u16, pressed: bool, modifiers: u16) {
        let e = KeyEvent::new(self.next_window, keycode, u16::from(pressed), modifiers);
        let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
        let n = e.write(&mut b).unwrap();
        self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
    }

    /// Queue a key event for `window`.
    pub fn queue_key_for(&mut self, window: u32, keycode: u16, pressed: bool, modifiers: u16) {
        let e = KeyEvent::new(window, keycode, u16::from(pressed), modifiers);
        let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
        let n = e.write(&mut b).unwrap();
        self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
    }

    /// Queue a pointer event for `window`.
    pub fn queue_pointer_for(&mut self, window: u32, kind: u16, x: i32, y: i32) {
        let e = PointerEvent { window, kind, x, y, ..Default::default() };
        let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
        let n = e.write(&mut b).unwrap();
        self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
    }

    /// Queue a pointer event for the last created window.
    pub fn queue_pointer(&mut self, kind: u16, x: i32, y: i32) {
        let e = PointerEvent { window: self.next_window, kind, x, y, ..Default::default() };
        let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
        let n = e.write(&mut b).unwrap();
        self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
    }

    /// Queue a file drop on `window`, naming its acceptor, path and file name.
    pub fn queue_drop(&mut self, window: u32, acceptor: &str, path: &str, name: &str) {
        use librsproto::surface::{DROP_KIND_FILE, DroppedEvent, MAX_EVENT_BODY, OP_DROPPED};
        let mut b = alloc::vec![0u8; MAX_EVENT_BODY];
        let ev = DroppedEvent { window, kind: DROP_KIND_FILE, x: 3, y: 4, acceptor_len: 0, path_len: 0 };
        let n = ev
            .write(&mut b, acceptor.as_bytes(), path.as_bytes(), name.as_bytes())
            .expect("within the protocol's caps");
        self.events.insert(0, (OP_DROPPED, b[..n].to_vec()));
    }

    /// Queue the release of `buffer` on `window`, ready to read now.
    pub fn queue_release(&mut self, window: u32, buffer: u32) {
        let mut b = [0u8; 8];
        let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
        self.events.push((OP_RELEASE, b[..n].to_vec()));
    }

    /// Queue the release of `buffer` on `window` to arrive during the next blocking wait.
    pub fn defer_release(&mut self, window: u32, buffer: u32) {
        let mut b = [0u8; 8];
        let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
        self.deferred.push((OP_RELEASE, b[..n].to_vec()));
    }
}
