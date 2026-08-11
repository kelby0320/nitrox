//! `libsurface` — the client side of the Surface protocol.
//!
//! The role `librsproto` plays for the RS protocol: **the protocol gets a library, and
//! clients use it**. The plan is explicit about why — if the first application hand-rolls
//! this, the surface protocol immediately has two implementations and the second one lives
//! in an application.
//!
//! ## The buffer lifecycle is the interesting part
//!
//! Sending the messages is mechanical; `librsproto::surface` already encodes them and is
//! already tested. What a client actually needs from a library is the answer to *"which
//! buffer may I draw into right now?"* — and getting that wrong produces tearing that is
//! invisible in testing and obvious in use (`display-substrate.md` §4).
//!
//! So that logic sits behind [`Transport`] and host-tests against a mock, rather than
//! needing a running compositor. See [`Window::next_free`].
//!
//! ## Single buffering cannot work, and the library says so
//!
//! A buffer is busy from the moment it is committed until the compositor releases it, and
//! the compositor releases the buffer that *left* the screen — never the one on it. With
//! one buffer there is never anything to release, so [`Window::next_free`] returns `None`
//! forever after the first commit. That is not a bug to work around: drawing into the
//! buffer the compositor is reading is exactly the tearing the protocol exists to prevent.
//! [`Window::new`] refuses fewer than two.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::Geometry;
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, OP_ATTACH_BUFFER, OP_COMMIT,
    FocusEvent, KeyEvent, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_FOCUS_EVENT, OP_KEY_EVENT,
    OP_POINTER_EVENT, OP_RELEASE, PointerEvent, Role, SURFACE_FORMAT_XRGB8888,
    build_attach_buffer_request, build_commit_request, build_create_window_request,
    build_destroy_window_request, parse_create_window_reply, parse_release_event,
};

pub mod ipc;

/// What went wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UiError {
    /// The transport failed.
    Transport,
    /// The server's reply was not the one this request expects.
    BadReply,
    /// A message body did not fit the buffer, or did not parse.
    Malformed,
    /// Fewer than two buffers were requested — see the module docs.
    TooFewBuffers,
    /// No buffer with that id.
    NoSuchBuffer,
    /// The compositor answered this request with an error.
    ///
    /// Distinct from [`UiError::BadReply`]: the reply is well-formed and *is* for this
    /// request — it just says no. Before this existed, `request` matched on
    /// `RS_FLAG_REPLY` alone, so an error reply came back as a **successful** one and the
    /// caller parsed the error body as a result. `Window::new` would have read a window id
    /// out of an error code.
    Server,
}

/// How a client talks to the compositor.
///
/// The seam that keeps the buffer lifecycle testable. The real implementation is
/// [`ipc::ChannelTransport`]; tests use a mock that records what was sent.
pub trait Transport {
    /// Send a Surface request, optionally transferring a handle, and read the reply body
    /// into `reply`. Returns the reply length, or `None` for ops with no reply.
    fn request(
        &mut self,
        op: u16,
        body: &[u8],
        handle: Option<u64>,
        reply: &mut [u8],
    ) -> Result<Option<usize>, UiError>;

    /// Poll for a server-initiated event without blocking. `None` when there is none.
    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError>;

    /// Whether the transport discarded an event since this was last called, clearing the
    /// flag.
    ///
    /// Defaulted to `false` so a transport that cannot lose anything — a test mock, or one
    /// with an unbounded queue — says nothing. A transport that *can* must report it, or a
    /// client tracking held keys carries a phantom modifier for the rest of its life.
    fn took_loss(&mut self) -> bool {
        false
    }

    /// Block until a server-initiated event arrives, then return it.
    ///
    /// The counterpart `poll_event` cannot provide. A client that has drawn everything it
    /// can and holds no free buffer must **wait**, not spin or give up: the compositor may
    /// simply not have processed the last commit yet. Without this a client stalls the
    /// moment it commits more frames than it has buffers, which is every client.
    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError>;
}

/// A buffer the client owns, and whether the compositor is currently reading it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClientBuffer {
    /// Client-chosen id.
    pub id: u32,
    /// Shape of the pixels.
    pub geometry: Geometry,
    /// True from `commit` until the compositor releases it.
    ///
    /// **Busy means the compositor may be reading these pixels right now.** Drawing into a
    /// busy buffer is the tearing case; the whole point of tracking this is to make that
    /// unrepresentable rather than merely discouraged.
    pub busy: bool,
}

/// A boxed transport is a transport.
///
/// Not a convenience: [`ipc::ChannelTransport`] carries two 4 KiB message buffers plus the
/// parked-event queue, so it is ~9 KiB, and a userspace thread starts with a **32 KiB**
/// stack. Holding one by value — and moving it in and out of a `Window` — is enough to run
/// a client off the end of its stack, which presents as a process that dies in its
/// prologue and prints nothing at all, before its own first line. Box it and the `Window`
/// holds a pointer.
impl<T: Transport + ?Sized> Transport for alloc::boxed::Box<T> {
    fn request(
        &mut self,
        op: u16,
        body: &[u8],
        handle: Option<u64>,
        reply: &mut [u8],
    ) -> Result<Option<usize>, UiError> {
        (**self).request(op, body, handle, reply)
    }

    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
        (**self).wait_event(buf)
    }

    fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
        (**self).poll_event(buf)
    }

    fn took_loss(&mut self) -> bool {
        (**self).took_loss()
    }
}

/// How many input events a window queues before the oldest are discarded.
///
/// A bound, because a client that stops draining must not grow this without limit — and
/// input is *continuous*, so "stops draining" is the ordinary state of a client rendering a
/// frame rather than a misbehaviour. Sized for a burst of typing plus a drag, not for a
/// client that has stopped reading.
pub const EVENT_QUEUE_MAX: usize = 64;

/// One input event, as a window receives it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WindowEvent {
    /// A key transition, with the modifiers held at that moment.
    Key(KeyEvent),
    /// Pointer motion, a button, or a crossing.
    Pointer(PointerEvent),
    /// This window gained or lost the keyboard.
    ///
    /// **Not the same as widget focus**, which is the toolkit's and which this must not
    /// disturb: a window returning to the foreground has to put the caret back where it was.
    ///
    /// Filtered to *this* window: one session can hold several, and a popup taking focus
    /// from its parent sends both halves down the one channel.
    Focus(bool),
    /// The queue overflowed and events were discarded.
    ///
    /// **Accumulated state must be discarded.** A client tracking which keys or buttons are
    /// down has to assume it missed a release, exactly as `libinput` requires of a
    /// `SYN_DROPPED`. A client that ignores this carries a phantom held modifier for the
    /// rest of its life, which is the failure the marker exists to prevent.
    Dropped,
}

/// A window and its buffers.
pub struct Window<T: Transport> {
    transport: T,
    id: u32,
    buffers: Vec<ClientBuffer>,
    /// Input delivered but not yet drained by the client.
    events: alloc::collections::VecDeque<WindowEvent>,
    /// Whether the next drain owes the client a [`WindowEvent::Dropped`].
    ///
    /// A flag rather than a count: the client's obligation is the same whether it missed one
    /// event or forty — discard what you believed — so a number would be information nobody
    /// can act on differently.
    lost: bool,
}

impl<T: Transport> Window<T> {
    /// Create a window with `count` buffers of `width × height`.
    ///
    /// Refuses fewer than two: with one buffer a client can never redraw without drawing
    /// into pixels the compositor is reading. `attach` is called for each, and the caller
    /// supplies the shared memory each buffer names.
    pub fn new(
        mut transport: T,
        width: u32,
        height: u32,
        role: Role,
        count: usize,
    ) -> Result<Self, UiError> {
        if count < 2 {
            return Err(UiError::TooFewBuffers);
        }
        let mut body = [0u8; 32];
        let n = build_create_window_request(
            &mut body,
            &CreateWindowRequest { width, height, role },
        )
        .ok_or(UiError::Malformed)?;
        let mut reply = [0u8; 32];
        let len = transport
            .request(OP_CREATE_WINDOW, &body[..n], None, &mut reply)?
            .ok_or(UiError::BadReply)?;
        let id = parse_create_window_reply(&reply[..len]).ok_or(UiError::BadReply)?;
        Ok(Self {
            transport,
            id,
            buffers: Vec::new(),
            events: alloc::collections::VecDeque::new(),
            lost: false,
        })
    }

    /// Consume the window and hand back its connection.
    ///
    /// A connection outlives any one window — closing a window and opening another is
    /// ordinary application behaviour and must not require reconnecting, which would cost a
    /// compositor session slot each time. Pair with [`Window::destroy`]: destroy the window,
    /// then take the transport back for the next one.
    pub fn into_transport(self) -> T {
        self.transport
    }

    /// This window's id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The buffers attached so far.
    pub fn buffers(&self) -> &[ClientBuffer] {
        &self.buffers
    }

    /// Attach shared memory as buffer `buffer_id`, transferring `handle`.
    ///
    /// The handle crosses **once**; thereafter the buffer is named by id. `pitch` is the
    /// client's own row stride and need not be `width * 4`.
    pub fn attach(
        &mut self,
        buffer_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        handle: u64,
    ) -> Result<(), UiError> {
        let geometry = Geometry::with_pitch(width, height, pitch as usize, PixelFormat::XRGB8888)
            .ok_or(UiError::Malformed)?;
        let mut body = [0u8; 32];
        let n = build_attach_buffer_request(
            &mut body,
            &AttachBufferRequest {
                window: self.id,
                buffer: buffer_id,
                width,
                height,
                pitch,
                format: SURFACE_FORMAT_XRGB8888,
            },
        )
        .ok_or(UiError::Malformed)?;
        self.transport.request(OP_ATTACH_BUFFER, &body[..n], Some(handle), &mut [])?;
        self.buffers.push(ClientBuffer { id: buffer_id, geometry, busy: false });
        Ok(())
    }

    /// A buffer the client may draw into, or `None` if the compositor holds them all.
    ///
    /// Callers should [`pump`](Self::pump) first: a release may already be waiting.
    pub fn next_free(&self) -> Option<u32> {
        self.buffers.iter().find(|b| !b.busy).map(|b| b.id)
    }

    /// A buffer to draw into, **waiting** if the compositor holds them all.
    ///
    /// This is the call a render loop wants, and the reason `next_free` alone is not
    /// enough: after committing more frames than it has buffers, a client's next buffer
    /// only becomes available when a `Release` arrives, and that may not have happened yet
    /// when it asks. Polling once and failing is how the first real client stalled at its
    /// third frame.
    ///
    /// Drains pending events first, so a release already waiting costs no block at all.
    pub fn acquire(&mut self) -> Result<u32, UiError> {
        self.pump()?;
        loop {
            if let Some(b) = self.next_free() {
                return Ok(b);
            }
            if self.buffers.is_empty() {
                return Err(UiError::NoSuchBuffer);
            }
            // Nothing free: block until the compositor says something, then re-check.
            let mut buf = [0u8; 64];
            let (op, len) = self.transport.wait_event(&mut buf)?;
            self.apply_event(op, &buf[..len]);
        }
    }

    /// Apply one server event. Shared by [`pump`](Self::pump) and [`acquire`](Self::acquire).
    ///
    /// Input is **queued here rather than returned**, so a client blocked in `acquire`
    /// waiting for a buffer does not lose the keystrokes that arrive while it waits. That is
    /// the ordinary case, not a corner: a client renders, commits, and blocks — which is
    /// precisely when a user is looking at the result and typing.
    fn apply_event(&mut self, op: u16, body: &[u8]) {
        match op {
            OP_RELEASE => {
                if let Some((window, buffer)) = parse_release_event(body)
                    && window == self.id
                    && let Some(b) = self.buffers.iter_mut().find(|b| b.id == buffer)
                {
                    b.busy = false;
                }
            }
            OP_KEY_EVENT => {
                if let Some(e) = KeyEvent::read(body) {
                    self.enqueue(WindowEvent::Key(e));
                }
            }
            OP_POINTER_EVENT => {
                if let Some(e) = PointerEvent::read(body) {
                    self.enqueue(WindowEvent::Pointer(e));
                }
            }
            OP_FOCUS_EVENT => {
                if let Some(e) = FocusEvent::read(body)
                    && e.window == self.id
                {
                    self.enqueue(WindowEvent::Focus(e.focused != 0));
                }
            }
            _ => {}
        }
    }

    /// Queue an event, discarding the oldest if the queue is full.
    ///
    /// **Oldest, not newest.** The newest event is the one describing the world as it is now
    /// — dropping it would leave a client acting on a stale button state forever, where
    /// dropping the oldest only costs it history it is already behind on.
    fn enqueue(&mut self, e: WindowEvent) {
        if self.events.len() >= EVENT_QUEUE_MAX {
            self.events.pop_front();
            self.lost = true;
        }
        self.events.push_back(e);
    }

    /// Take the next queued input event, if any. Does not talk to the compositor.
    ///
    /// Call [`pump`](Self::pump) first to collect what has arrived.
    pub fn next_event(&mut self) -> Option<WindowEvent> {
        if self.lost {
            // Announced before the surviving events, so a client resets its state and *then*
            // applies what it still has, rather than the other way round.
            self.lost = false;
            return Some(WindowEvent::Dropped);
        }
        self.events.pop_front()
    }

    /// How many input events are queued.
    pub fn events_pending(&self) -> usize {
        self.events.len()
    }

    /// Block until an input event is available, then take it.
    ///
    /// Buffer releases arriving meanwhile are applied rather than discarded — they are the
    /// same channel, and dropping one strands a buffer forever.
    pub fn wait_event(&mut self) -> Result<WindowEvent, UiError> {
        self.pump()?;
        loop {
            if let Some(e) = self.next_event() {
                return Ok(e);
            }
            let mut buf = [0u8; 64];
            let (op, len) = self.transport.wait_event(&mut buf)?;
            self.apply_event(op, &buf[..len]);
        }
    }

    /// Commit `buffer_id` with a damage rectangle, marking it busy.
    pub fn commit(
        &mut self,
        buffer_id: u32,
        damage: (u32, u32, u32, u32),
    ) -> Result<(), UiError> {
        if !self.buffers.iter().any(|b| b.id == buffer_id) {
            return Err(UiError::NoSuchBuffer);
        }
        let mut body = [0u8; 32];
        let n = build_commit_request(
            &mut body,
            &CommitRequest {
                window: self.id,
                buffer: buffer_id,
                damage_x: damage.0,
                damage_y: damage.1,
                damage_w: damage.2,
                damage_h: damage.3,
            },
        )
        .ok_or(UiError::Malformed)?;
        // **Marked busy only after the send succeeds.** Setting it first strands the buffer
        // if the send fails — the compositor never saw the commit and will never release
        // it, so a double-buffered client stalls forever after two such failures.
        self.transport.request(OP_COMMIT, &body[..n], None, &mut [])?;
        if let Some(b) = self.buffers.iter_mut().find(|b| b.id == buffer_id) {
            b.busy = true;
        }
        Ok(())
    }

    /// Drain pending server events, freeing released buffers. Returns how many arrived.
    ///
    /// Non-blocking: a client with something to draw calls this, then [`next_free`], and
    /// only waits if there is still nothing free.
    ///
    /// [`next_free`]: Self::next_free
    pub fn pump(&mut self) -> Result<usize, UiError> {
        let mut seen = 0usize;
        // Loss below this layer is loss to the client: fold it into the same flag the local
        // queue overflowing sets, so both surface as one `WindowEvent::Dropped`.
        if self.transport.took_loss() {
            self.lost = true;
        }
        let mut buf = [0u8; 64];
        while let Some((op, len)) = self.transport.poll_event(&mut buf)? {
            self.apply_event(op, &buf[..len]);
            seen += 1;
        }
        Ok(seen)
    }

    /// Destroy the window.
    pub fn destroy(&mut self) -> Result<(), UiError> {
        let mut body = [0u8; 8];
        let n = build_destroy_window_request(&mut body, self.id).ok_or(UiError::Malformed)?;
        self.transport.request(OP_DESTROY_WINDOW, &body[..n], None, &mut [])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use librsproto::surface::{MOD_SHIFT, POINTER_MOTION};

    /// Records what was sent and lets a test hand back replies and events.
    #[derive(Default)]
    struct MockTransport {
        sent: Vec<(u16, Vec<u8>, Option<u64>)>,
        next_window: u32,
        events: Vec<(u16, Vec<u8>)>,
        /// Events that only appear once the client actually blocks.
        deferred: Vec<(u16, Vec<u8>)>,
        /// How many times the client had to block.
        waits: usize,
        fail: bool,
        /// Set by a test to model the real transport discarding a parked event.
        lost: bool,
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
                let n = librsproto::surface::build_create_window_reply(reply, self.next_window)
                    .ok_or(UiError::Malformed)?;
                return Ok(Some(n));
            }
            Ok(None)
        }

        fn poll_event(&mut self, buf: &mut [u8]) -> Result<Option<(u16, usize)>, UiError> {
            let Some((op, body)) = self.events.pop() else { return Ok(None) };
            buf[..body.len()].copy_from_slice(&body);
            Ok(Some((op, body.len())))
        }

        fn took_loss(&mut self) -> bool {
            core::mem::take(&mut self.lost)
        }

        fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError> {
            // Models the real timing: nothing is queued *yet*, and the release only
            // materialises because we waited. `deferred` is what a test arranges to have
            // the compositor "send" during the block.
            if self.events.is_empty()
                && let Some(ev) = self.deferred.pop()
            {
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
        /// Queue a key event as the compositor would send it.
        fn queue_key(&mut self, keycode: u16, pressed: bool, modifiers: u16) {
            let e = KeyEvent { keycode, pressed: u16::from(pressed), modifiers, _pad: 0 };
            let mut b = [0u8; 8];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
        }

        /// Queue a pointer event as the compositor would send it.
        fn queue_pointer(&mut self, kind: u16, x: i32, y: i32) {
            let e = PointerEvent { kind, x, y, ..Default::default() };
            let mut b = [0u8; 20];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
        }

        fn queue_release(&mut self, window: u32, buffer: u32) {
            let mut b = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
            self.events.push((OP_RELEASE, b[..n].to_vec()));
        }

        /// A release the compositor only sends once the client blocks for it.
        fn defer_release(&mut self, window: u32, buffer: u32) {
            let mut b = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
            self.deferred.push((OP_RELEASE, b[..n].to_vec()));
        }
    }

    fn window(count: usize) -> Window<MockTransport> {
        let mut w =
            Window::new(MockTransport::default(), 64, 32, Role::Normal, count).unwrap();
        for i in 0..count {
            w.attach(i as u32, 64, 32, 64 * 4, 100 + i as u64).unwrap();
        }
        w
    }

    #[test]
    fn a_window_gets_its_id_from_the_server_reply() {
        let w = window(2);
        assert_eq!(w.id(), 1);
        assert_eq!(w.buffers().len(), 2);
    }

    #[test]
    fn single_buffering_is_refused_at_construction() {
        // Not a limitation to work around: with one buffer, the only thing to draw into is
        // the buffer the compositor is reading.
        for count in [0, 1] {
            assert_eq!(
                Window::new(MockTransport::default(), 8, 8, Role::Normal, count).err(),
                Some(UiError::TooFewBuffers)
            );
        }
        assert!(Window::new(MockTransport::default(), 8, 8, Role::Normal, 2).is_ok());
    }

    #[test]
    fn the_handle_is_transferred_once_at_attach_and_never_again() {
        // The whole point of the shared-memory design: the handle crosses on attach, and
        // commits thereafter name the buffer by id.
        let mut w = window(2);
        w.commit(0, (0, 0, 64, 32)).unwrap();
        w.commit(1, (0, 0, 64, 32)).unwrap();

        let attaches: Vec<_> =
            w.transport.sent.iter().filter(|(op, _, _)| *op == OP_ATTACH_BUFFER).collect();
        assert_eq!(attaches.len(), 2);
        assert!(attaches.iter().all(|(_, _, h)| h.is_some()), "attach must carry a handle");

        let commits: Vec<_> =
            w.transport.sent.iter().filter(|(op, _, _)| *op == OP_COMMIT).collect();
        assert_eq!(commits.len(), 2);
        assert!(commits.iter().all(|(_, _, h)| h.is_none()), "a commit transfers nothing");
    }

    #[test]
    fn committing_marks_a_buffer_busy_and_a_release_frees_it() {
        let mut w = window(2);
        assert_eq!(w.next_free(), Some(0));
        w.commit(0, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), Some(1), "the other buffer is still free");

        w.commit(1, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), None, "the compositor holds both");

        // The compositor releases the one that left the screen.
        w.transport.queue_release(1, 0);
        assert_eq!(w.pump().unwrap(), 1);
        assert_eq!(w.next_free(), Some(0));
    }

    #[test]
    fn double_buffering_can_draw_forever_without_ever_touching_a_busy_buffer() {
        // The property the whole design exists for. Ten frames, alternating, never
        // drawing into a buffer the compositor holds.
        let mut w = window(2);
        let mut previous: Option<u32> = None;
        for frame in 0..10 {
            w.pump().unwrap();
            let b = w.next_free().unwrap_or_else(|| panic!("stalled at frame {frame}"));
            assert!(!w.buffers().iter().find(|x| x.id == b).unwrap().busy);
            w.commit(b, (0, 0, 64, 32)).unwrap();
            // The compositor releases whatever left the screen.
            if let Some(p) = previous {
                w.transport.queue_release(w.id(), p);
            }
            previous = Some(b);
        }
    }

    #[test]
    fn a_release_for_another_window_is_ignored() {
        // Events arrive on a shared channel; a client with two windows must not free a
        // buffer because the *other* window's buffer was released.
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.transport.queue_release(w.id() + 99, 0);
        w.pump().unwrap();
        assert_eq!(w.next_free(), Some(1), "buffer 0 must still be busy");
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn a_release_naming_an_unknown_buffer_is_ignored() {
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.transport.queue_release(w.id(), 42);
        w.pump().unwrap();
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn committing_an_unattached_buffer_is_refused_before_it_reaches_the_wire() {
        let mut w = window(2);
        let before = w.transport.sent.len();
        assert_eq!(w.commit(7, (0, 0, 1, 1)), Err(UiError::NoSuchBuffer));
        assert_eq!(w.transport.sent.len(), before, "nothing was sent");
    }

    #[test]
    fn a_pitch_too_small_for_a_row_is_refused_at_attach() {
        let mut w = window(2);
        assert_eq!(w.attach(9, 64, 32, 64 * 4 - 1, 1), Err(UiError::Malformed));
    }

    #[test]
    fn acquire_waits_when_the_compositor_holds_every_buffer() {
        // The bug the first real client hit: it polled once, found nothing free, and gave
        // up at frame 3. A release that has not arrived *yet* is not a release that will
        // never arrive — the compositor may simply not have processed the commit.
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.commit(1, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), None, "both are held");

        // The compositor sends the release only once the client blocks.
        w.transport.defer_release(1, 0);
        assert_eq!(w.acquire().unwrap(), 0);
        assert_eq!(w.transport.waits, 1, "it had to block exactly once");
    }

    #[test]
    fn acquire_does_not_block_when_something_is_already_free() {
        let mut w = window(2);
        assert_eq!(w.acquire().unwrap(), 0);
        assert_eq!(w.transport.waits, 0, "no reason to wait");
    }

    #[test]
    fn a_render_loop_of_more_frames_than_buffers_never_stalls() {
        // The end-to-end shape: commit, then acquire the next buffer, for more frames than
        // there are buffers. This is what `ui-testclient` does in the guest.
        let mut w = window(2);
        let mut previous: Option<u32> = None;
        for frame in 0..8 {
            let b = w.acquire().unwrap_or_else(|e| panic!("stalled at frame {frame}: {e:?}"));
            w.commit(b, (0, 0, 1, 1)).unwrap();
            if let Some(p) = previous {
                w.transport.defer_release(w.id(), p);
            }
            previous = Some(b);
        }
    }

    #[test]
    fn a_failed_commit_leaves_the_buffer_drawable() {
        // If `busy` were set before the send, a failed commit would strand the buffer: the
        // compositor never saw it and will never release it. Two such failures on a
        // double-buffered window stall the client forever.
        let mut w = window(2);
        w.transport.fail = true;
        assert_eq!(w.commit(0, (0, 0, 1, 1)), Err(UiError::Transport));
        assert_eq!(w.next_free(), Some(0), "buffer 0 must still be drawable");
        assert!(!w.buffers().iter().find(|b| b.id == 0).unwrap().busy);

        // And a subsequent successful commit does mark it.
        w.transport.fail = false;
        w.commit(0, (0, 0, 1, 1)).unwrap();
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn a_transport_failure_surfaces_rather_than_being_swallowed() {
        let mut t = MockTransport::default();
        t.fail = true;
        assert_eq!(
            Window::new(t, 8, 8, Role::Normal, 2).err(),
            Some(UiError::Transport)
        );
    }

    #[test]
    fn the_damage_rectangle_reaches_the_wire_unchanged() {
        let mut w = window(2);
        w.commit(0, (3, 5, 17, 9)).unwrap();
        let (_, body, _) =
            w.transport.sent.iter().rev().find(|(op, _, _)| *op == OP_COMMIT).unwrap();
        let req = librsproto::surface::parse_commit_request(body).unwrap();
        assert_eq!((req.damage_x, req.damage_y, req.damage_w, req.damage_h), (3, 5, 17, 9));
    }

    #[test]
    fn transport_level_loss_surfaces_as_a_dropped_marker() {
        // A transport that discarded a parked event must say so, or a client tracking held
        // keys carries a phantom modifier for the rest of its life. Folded into the same
        // flag the local queue sets, so both surface as one `Dropped`.
        let mut w = window(2);
        w.transport.lost = true;
        w.pump().expect("pump");
        assert_eq!(w.next_event(), Some(WindowEvent::Dropped));
        assert_eq!(w.next_event(), None, "announced once, not on every drain");
    }

    #[test]
    fn a_focus_event_reaches_the_queue_as_a_bool() {
        let mut w = window(2);
        for focused in [true, false] {
            let e = FocusEvent { focused: u16::from(focused), _pad: 0, window: w.id() };
            let mut b = [0u8; 8];
            let n = e.write(&mut b).unwrap();
            w.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
        }
        w.pump().expect("pump");
        assert_eq!(w.next_event(), Some(WindowEvent::Focus(true)));
        assert_eq!(w.next_event(), Some(WindowEvent::Focus(false)));
    }

    #[test]
    fn a_focus_event_for_another_window_is_not_this_windows_business() {
        // One session can hold several windows — a popup is created on its parent's
        // connection — so both halves of a focus change arrive on the one channel. Without
        // the filter a parent would read its own loss as a gain (PR #184 re-review).
        let mut w = window(2);
        let other = w.id() + 1;
        let e = FocusEvent { focused: 1, _pad: 0, window: other };
        let mut b = [0u8; 8];
        let n = e.write(&mut b).unwrap();
        w.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None, "not addressed to this window");
    }

    #[test]
    fn a_truncated_focus_event_is_ignored_rather_than_read_as_unfocused() {
        // Reading a short record as `focused: false` would dim a window for a malformed
        // message — the failure mode is invisible, because a dim window looks like a window
        // that legitimately lost focus.
        let mut w = window(2);
        w.transport.events.insert(0, (OP_FOCUS_EVENT, vec![0u8; 7]));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None);
    }

    #[test]
    fn key_and_pointer_events_reach_the_queue_intact() {
        let mut w = window(2);
        w.transport.queue_key(30, true, MOD_SHIFT);
        w.transport.queue_pointer(POINTER_MOTION, -7, 12);
        assert_eq!(w.pump().expect("pump"), 2);

        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Key(KeyEvent {
                keycode: 30,
                pressed: 1,
                modifiers: MOD_SHIFT,
                _pad: 0
            }))
        );
        // Signed coordinates survive the round trip — an unsigned read here is the
        // "pointer teleports on a leftward drag" bug, one layer further out.
        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Pointer(PointerEvent {
                kind: POINTER_MOTION,
                x: -7,
                y: 12,
                ..Default::default()
            }))
        );
        assert_eq!(w.next_event(), None);
    }

    #[test]
    fn input_arriving_while_blocked_for_a_buffer_is_queued_not_lost() {
        // The ordinary case, not a corner: a client renders, commits, and blocks — which is
        // exactly when the user is looking at the result and typing into it.
        let mut w = window(2);
        let b = w.acquire().expect("a free buffer");
        w.commit(b, (0, 0, 64, 32)).expect("commit");
        let b2 = w.acquire().expect("the other buffer");
        w.commit(b2, (0, 0, 64, 32)).expect("commit");

        // Nothing free. The compositor sends a keystroke *and* the release during the block.
        w.transport.queue_key(31, true, 0);
        w.transport.deferred.push({
            let mut bb = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut bb, w.id(), b).unwrap();
            (OP_RELEASE, bb[..n].to_vec())
        });

        assert_eq!(w.acquire().expect("released"), b);
        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Key(KeyEvent { keycode: 31, pressed: 1, modifiers: 0, _pad: 0 })),
            "the keystroke that arrived while blocked survived"
        );
    }

    #[test]
    fn a_release_arriving_while_waiting_for_input_is_not_discarded() {
        // The mirror of the case above, and the one a naive `wait_event` gets wrong by
        // ignoring anything that is not input — which strands the buffer forever.
        let mut w = window(2);
        let b = w.acquire().expect("free");
        w.commit(b, (0, 0, 64, 32)).expect("commit");
        assert!(w.buffers().iter().find(|x| x.id == b).unwrap().busy);

        // Both arrive **during the block**, release first. Queuing the release up front
        // instead would have `pump` consume it before `wait_event` ever blocks — which is
        // how a first version of this test passed against a `wait_event` that discarded
        // releases outright.
        w.transport.deferred.push({
            let e = KeyEvent { keycode: 1, pressed: 1, modifiers: 0, _pad: 0 };
            let mut bb = [0u8; 8];
            let n = e.write(&mut bb).unwrap();
            (OP_KEY_EVENT, bb[..n].to_vec())
        });
        w.transport.deferred.push({
            let mut bb = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut bb, w.id(), b).unwrap();
            (OP_RELEASE, bb[..n].to_vec())
        });

        assert!(matches!(w.wait_event(), Ok(WindowEvent::Key(_))));
        assert!(
            !w.buffers().iter().find(|x| x.id == b).unwrap().busy,
            "the release was applied, not thrown away"
        );
    }

    #[test]
    fn an_overflowing_queue_drops_the_oldest_and_says_so() {
        let mut w = window(2);
        for i in 0..(EVENT_QUEUE_MAX as u16 + 3) {
            w.transport.queue_key(i, true, 0);
        }
        w.pump().expect("pump");
        assert_eq!(w.events_pending(), EVENT_QUEUE_MAX);

        // The marker comes first, so a client resets what it believed *before* applying
        // what survived.
        assert_eq!(w.next_event(), Some(WindowEvent::Dropped));
        let Some(WindowEvent::Key(k)) = w.next_event() else { panic!("a key") };
        assert_eq!(k.keycode, 3, "the three oldest went, not the three newest");

        // And it is announced once, not on every subsequent drain.
        while let Some(e) = w.next_event() {
            assert_ne!(e, WindowEvent::Dropped);
        }
    }

    #[test]
    fn the_newest_event_is_the_one_kept() {
        // Dropping the newest would leave a client acting on a stale button state forever;
        // dropping the oldest only costs it history it is already behind on.
        let mut w = window(2);
        for i in 0..(EVENT_QUEUE_MAX as u16 + 1) {
            w.transport.queue_key(i, true, 0);
        }
        w.pump().expect("pump");
        let last = core::iter::from_fn(|| w.next_event())
            .filter_map(|e| match e {
                WindowEvent::Key(k) => Some(k.keycode),
                _ => None,
            })
            .last();
        assert_eq!(last, Some(EVENT_QUEUE_MAX as u16), "the most recent survived");
    }

    #[test]
    fn a_malformed_input_record_is_ignored_rather_than_queued_as_garbage() {
        let mut w = window(2);
        w.transport.events.insert(0, (OP_POINTER_EVENT, vec![0u8; 19]));
        w.transport.queue_key(42, true, 0);
        w.pump().expect("pump");
        assert_eq!(w.events_pending(), 1, "the short pointer record was dropped");
        assert!(matches!(w.next_event(), Some(WindowEvent::Key(_))));
    }

}
