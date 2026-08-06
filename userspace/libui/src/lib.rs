//! `libui` — the client side of the Surface protocol.
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
    OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_RELEASE, Role, SURFACE_FORMAT_XRGB8888,
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

/// A window and its buffers.
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
}

/// A window and its buffers.
pub struct Window<T: Transport> {
    transport: T,
    id: u32,
    buffers: Vec<ClientBuffer>,
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
        Ok(Self { transport, id, buffers: Vec::new() })
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
    fn apply_event(&mut self, op: u16, body: &[u8]) {
        if op == OP_RELEASE
            && let Some((window, buffer)) = parse_release_event(body)
            && window == self.id
            && let Some(b) = self.buffers.iter_mut().find(|b| b.id == buffer)
        {
            b.busy = false;
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
}
