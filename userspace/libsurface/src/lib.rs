//! `libsurface` — the client side of the Surface protocol.
//!
//! A buffer is busy from commit until the compositor releases it, and the compositor releases
//! the buffer that left the screen, never the one on it (`display-substrate.md` §4). With one
//! buffer there is nothing to release, so [`WindowRef::next_free`] returns `None` forever after
//! the first commit, and drawing into the buffer the compositor is reading tears.
//! [`Session::create`] refuses fewer than two.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::Geometry;
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, ConfigureEvent, CreateWindowRequest, KeyEvent, OP_ATTACH_BUFFER, OP_COMMIT,
    OP_CONFIGURE, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_RELEASE, PointerEvent, Role, SURFACE_FORMAT_ARGB8888,
    SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request, build_create_window_request,
    build_destroy_window_request, parse_create_window_reply, parse_release_event,
};

pub mod buffers;
mod events;
pub mod ipc;
pub mod screen;

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
    /// A pixel format with no wire tag, refused rather than sent as `XRGB8888` with red and
    /// blue swapped; see [`attach_with_format`](WindowRef::attach_with_format).
    UnsupportedFormat,
    /// The compositor answered this request with an error: well-formed, unlike
    /// [`UiError::BadReply`], but a refusal.
    Server,
}

/// How a client talks to the compositor; the real one is [`ipc::ChannelTransport`].
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

    /// Whether the transport discarded an event since this was last called. One that can must
    /// report it, or a client tracking held keys carries a phantom modifier.
    fn took_loss(&mut self) -> bool {
        false
    }

    /// The handle to `sys_wait` on for a pending event, or `0` if none can be waited on (then fall
    /// back to `wait_event` rather than spin). For a client with a second source of work, since
    /// `wait_event` blocks on this transport alone.
    fn wait_handle(&self) -> u64 {
        0
    }

    /// Block until a server-initiated event arrives, then return it.
    fn wait_event(&mut self, buf: &mut [u8]) -> Result<(u16, usize), UiError>;
}

/// A buffer the client owns, and whether the compositor is currently reading it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ClientBuffer {
    /// Client-chosen id.
    pub id: u32,
    /// Shape of the pixels.
    pub geometry: Geometry,
    /// True from `commit` until the compositor releases it: it may be reading these pixels
    /// right now, and drawing into a busy buffer tears.
    pub busy: bool,
}

/// A boxed transport is a transport: [`ipc::ChannelTransport`] is ~9 KiB against a 32 KiB user
/// stack, and moving one by value kills the process in its prologue, printing nothing.
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

    fn wait_handle(&self) -> u64 {
        (**self).wait_handle()
    }
}

/// How many input events a window queues before the oldest are discarded: a burst of typing
/// plus a drag.
pub const EVENT_QUEUE_MAX: usize = 64;

/// One input event, as a window receives it. Not `Copy`: a [`Drop`](Self::Drop) owns text.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum WindowEvent {
    /// A key transition, with the modifiers held at that moment.
    Key(KeyEvent),
    /// Pointer motion, a button, or a crossing.
    Pointer(PointerEvent),
    /// This window gained or lost the keyboard; not widget focus, which is the toolkit's.
    Focus(bool),
    /// Somebody with the manager channel is asking this window to close; ignoring it stays open.
    CloseRequested,
    /// The compositor would like this window at this position and size: a request, answered by
    /// committing a buffer of that size. The first is consumed by [`Session::create`].
    Configure {
        /// Suggested width in pixels.
        width: u32,
        /// Suggested height in pixels.
        height: u32,
        /// Top-left corner in screen coordinates.
        x: i32,
        /// Top-left corner in screen coordinates.
        y: i32,
    },
    /// Events were discarded: a client tracking held keys or buttons must assume it missed a
    /// release. Not on the wire; derived from a per-window `lost` flag.
    InputLost,
    /// A press landed outside this popup — `Surface::Dismissed`. The window is still up; a
    /// menu destroys itself on it. Distinct from [`CloseRequested`](Self::CloseRequested).
    Dismissed,
    /// A payload was dropped here — `Surface::Dropped`; only for a window whose declared
    /// acceptor ([`WindowRef::declare_acceptor`]) takes its kind. `x` and `y` are window-local.
    Drop {
        /// The acceptor this landed on, by the name this window declared.
        acceptor: alloc::string::String,
        /// What the payload is — a mask bit, one of `DROP_KIND_FILE` and friends.
        kind: u32,
        /// The payload: a path the receiver opens for itself.
        path: alloc::string::String,
        /// What to call it on screen.
        name: alloc::string::String,
        /// Window-local x of the pointer when the button came up.
        x: i32,
        /// Window-local y.
        y: i32,
    },
    /// The manager put this window in this state, one of the `WINDOW_STATE_*` values —
    /// `Surface::WindowState`. A maximise button shows this, not what its own click asked for.
    State(u32),
}

/// Per-window state on a [`Session`].
struct WindowState {
    id: u32,
    /// The parent this window was created against, so [`WindowRef::destroy`] can drop the
    /// descendants the compositor drops.
    parent: Option<u32>,
    buffers: Vec<ClientBuffer>,
    /// Input delivered but not yet drained by the client.
    events: alloc::collections::VecDeque<WindowEvent>,
    /// Whether the next drain owes the client a [`WindowEvent::InputLost`].
    lost: bool,
    /// The most recent `Configure`, or `None` before the first has arrived.
    configured: Option<ConfigureEvent>,
    /// Whether the handshake configure has been seen, so the first is not delivered as an event.
    mapped: bool,
}

/// One Surface connection and every window on it (a popup may only name a parent its own
/// connection owns). Events are routed to the window they name. Box it if `T` is a
/// `ChannelTransport`.
pub struct Session<T: Transport> {
    transport: T,
    windows: Vec<WindowState>,
}

impl<T: Transport> Session<T> {
    /// Wrap a connected transport. No window exists yet.
    pub fn new(transport: T) -> Self {
        Self { transport, windows: Vec::new() }
    }

    /// Create a window with `buffers` (at least two) buffers, returning its id.
    ///
    /// Blocks until the window's first `Configure` arrives: a window is not composited until
    /// configured, and asking a manager before replying would put a userspace process on the
    /// critical path of every window creation. Events for other windows arriving meanwhile are
    /// routed to them.
    pub fn create(&mut self, req: &CreateWindowRequest, buffers: usize) -> Result<u32, UiError> {
        if buffers < 2 {
            return Err(UiError::TooFewBuffers);
        }
        let mut body = [0u8; 32];
        let n = build_create_window_request(&mut body, req).ok_or(UiError::Malformed)?;
        let mut reply = [0u8; 32];
        let len = self.transport.request(OP_CREATE_WINDOW, &body[..n], None, &mut reply)?.ok_or(UiError::BadReply)?;
        let id = parse_create_window_reply(&reply[..len]).ok_or(UiError::BadReply)?;
        self.windows.push(WindowState {
            id,
            parent: parent_of(req.role),
            buffers: Vec::new(),
            events: alloc::collections::VecDeque::new(),
            lost: false,
            configured: None,
            mapped: false,
        });
        while self.idx(id).is_some_and(|i| self.windows[i].configured.is_none()) {
            let mut buf = [0u8; librsproto::surface::MAX_EVENT_BODY];
            let (op, n) = match self.transport.wait_event(&mut buf) {
                Ok(ev) => ev,
                Err(e) => {
                    // Take the half-made window back out: the caller has an `Err` and no id.
                    self.windows.retain(|w| w.id != id);
                    return Err(e);
                }
            };
            self.apply_event(op, &buf[..n]);
        }
        Ok(id)
    }

    /// Lend `id` for one call chain. `None` if this session has no such window.
    pub fn window(&mut self, id: u32) -> Option<WindowRef<'_, T>> {
        let i = self.idx(id)?;
        Some(WindowRef { session: self, index: i })
    }

    /// Every window this session still holds, in creation order.
    pub fn window_ids(&self) -> Vec<u32> {
        self.windows.iter().map(|w| w.id).collect()
    }

    /// Take the next queued input event and its window, if any, in window creation order.
    /// Does not talk to the compositor; [`pump`](Self::pump) first.
    pub fn next_event(&mut self) -> Option<(u32, WindowEvent)> {
        for i in 0..self.windows.len() {
            let w = &mut self.windows[i];
            // The loss first, so a client resets what it believed before applying what survived.
            if w.lost {
                w.lost = false;
                return Some((w.id, WindowEvent::InputLost));
            }
            if let Some(e) = w.events.pop_front() {
                return Some((w.id, e));
            }
        }
        None
    }

    /// How many input events are queued across every window.
    pub fn events_pending(&self) -> usize {
        self.windows.iter().map(|w| w.events.len()).sum()
    }

    /// The handle to `sys_wait` on alongside a client's own — see [`Transport::wait_handle`].
    /// `0` if this transport cannot be waited on.
    pub fn wait_handle(&self) -> u64 {
        self.transport.wait_handle()
    }

    /// Take one event if one is already here, without blocking.
    pub fn poll_event(&mut self) -> Result<Option<(u32, WindowEvent)>, UiError> {
        self.pump()?;
        loop {
            if let Some(e) = self.next_event() {
                return Ok(Some(e));
            }
            let mut buf = [0u8; librsproto::surface::MAX_EVENT_BODY];
            match self.transport.poll_event(&mut buf)? {
                Some((op, len)) => self.apply_event(op, &buf[..len]),
                None => return Ok(None),
            }
        }
    }

    /// Block until an input event is available, then take it.
    /// Releases arriving meanwhile are applied; dropping one strands a buffer forever.
    pub fn wait_event(&mut self) -> Result<(u32, WindowEvent), UiError> {
        self.pump()?;
        loop {
            if let Some(e) = self.next_event() {
                return Ok(e);
            }
            let mut buf = [0u8; librsproto::surface::MAX_EVENT_BODY];
            let (op, len) = self.transport.wait_event(&mut buf)?;
            self.apply_event(op, &buf[..len]);
        }
    }

    /// Drain pending server events, freeing released buffers. Returns how many arrived.
    pub fn pump(&mut self) -> Result<usize, UiError> {
        let mut seen = 0usize;
        // A loss is not attributable to one window, so every window is told.
        if self.transport.took_loss() {
            for w in &mut self.windows {
                w.lost = true;
            }
        }
        let mut buf = [0u8; librsproto::surface::MAX_EVENT_BODY];
        while let Some((op, len)) = self.transport.poll_event(&mut buf)? {
            self.apply_event(op, &buf[..len]);
            seen += 1;
        }
        Ok(seen)
    }

    /// Give the transport back, dropping every window's local state. The windows are not
    /// destroyed: the compositor keeps them until the channel closes.
    pub fn into_transport(self) -> T {
        self.transport
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// Apply one server event. Input is queued, so a client blocked in `acquire` keeps its
    /// keystrokes; a record for an unknown window (one just destroyed) is dropped.
    fn apply_event(&mut self, op: u16, body: &[u8]) {
        match op {
            OP_RELEASE => {
                if let Some((window, buffer)) = parse_release_event(body)
                    && let Some(i) = self.idx(window)
                    && let Some(b) = self.windows[i].buffers.iter_mut().find(|b| b.id == buffer)
                {
                    b.busy = false;
                }
            }
            OP_CONFIGURE => {
                if let Some(e) = ConfigureEvent::read(body)
                    && let Some(i) = self.idx(e.window)
                {
                    self.windows[i].configured = Some(e);
                    // The first is the handshake `create` waits on, not an application event.
                    if self.windows[i].mapped {
                        self.enqueue(i, WindowEvent::Configure { width: e.width, height: e.height, x: e.x, y: e.y });
                    }
                    self.windows[i].mapped = true;
                }
            }
            _ => {
                if let Some((window, event)) = events::window_event(op, body)
                    && let Some(i) = self.idx(window)
                {
                    self.enqueue(i, event);
                }
            }
        }
    }

    /// Queue an event on window `i`, discarding the oldest if full: the newest describes the
    /// world as it is now. Per window, so a noisy one cannot evict a quiet one's events.
    fn enqueue(&mut self, i: usize, e: WindowEvent) {
        let w = &mut self.windows[i];
        if w.events.len() >= EVENT_QUEUE_MAX {
            w.events.pop_front();
            w.lost = true;
        }
        w.events.push_back(e);
    }
}

/// One window on a [`Session`], borrowed for the length of a call chain.
pub struct WindowRef<'a, T: Transport> {
    session: &'a mut Session<T>,
    index: usize,
}

impl<T: Transport> WindowRef<'_, T> {
    fn state(&self) -> &WindowState {
        &self.session.windows[self.index]
    }

    fn state_mut(&mut self) -> &mut WindowState {
        &mut self.session.windows[self.index]
    }

    /// This window's id.
    pub fn id(&self) -> u32 {
        self.state().id
    }

    /// The compositor's most recent opinion of this window's position and size. Never `None`.
    pub fn configured(&self) -> Option<ConfigureEvent> {
        self.state().configured
    }

    /// The buffers attached so far, in attach order.
    pub fn buffers(&self) -> &[ClientBuffer] {
        &self.state().buffers
    }

    /// Attach shared memory as buffer `buffer_id`, transferring `handle`.
    ///
    /// The handle crosses once per attach. Re-attaching an id replaces its memory, which is how
    /// a client resizes; the compositor refuses that for the displayed buffer, so replace a
    /// free one ([`BufferPool`](crate::buffers::BufferPool) does).
    pub fn attach(&mut self, buffer_id: u32, width: u32, height: u32, pitch: u32, handle: u64) -> Result<(), UiError> {
        self.attach_with_format(buffer_id, width, height, pitch, handle, PixelFormat::XRGB8888)
    }

    /// [`attach`](Self::attach), naming the buffer's pixel format.
    ///
    /// `PixelFormat::ARGB8888` makes a window translucent (opacity is read from the pixels, via
    /// [`PixelFormat::encode_alpha`]); it composites a pixel at a time, so ordinary windows stay
    /// opaque. Other formats are [`UiError::UnsupportedFormat`], not sent under the nearest tag.
    pub fn attach_with_format(
        &mut self,
        buffer_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        handle: u64,
        format: PixelFormat,
    ) -> Result<(), UiError> {
        let tag = if format == PixelFormat::ARGB8888 {
            SURFACE_FORMAT_ARGB8888
        } else if format == PixelFormat::XRGB8888 {
            SURFACE_FORMAT_XRGB8888
        } else {
            return Err(UiError::UnsupportedFormat);
        };
        let geometry = Geometry::with_pitch(width, height, pitch as usize, format).ok_or(UiError::Malformed)?;
        let mut body = [0u8; 32];
        let n = build_attach_buffer_request(
            &mut body,
            &AttachBufferRequest { window: self.id(), buffer: buffer_id, width, height, pitch, format: tag },
        )
        .ok_or(UiError::Malformed)?;
        self.session.transport.request(OP_ATTACH_BUFFER, &body[..n], Some(handle), &mut [])?;
        // Replaced in place when the id is known, so the count tracks the compositor's.
        match self.state_mut().buffers.iter_mut().find(|b| b.id == buffer_id) {
            Some(b) => b.geometry = geometry,
            None => self.state_mut().buffers.push(ClientBuffer { id: buffer_id, geometry, busy: false }),
        }
        Ok(())
    }

    /// The geometry attached under `buffer_id`, if this window has such a buffer.
    pub fn buffer_geometry(&self, buffer_id: u32) -> Option<Geometry> {
        self.state().buffers.iter().find(|b| b.id == buffer_id).map(|b| b.geometry)
    }

    /// A buffer the client may draw into, or `None` if the compositor holds them all.
    /// [`Session::pump`] first: a release may be waiting.
    pub fn next_free(&self) -> Option<u32> {
        self.state().buffers.iter().find(|b| !b.busy).map(|b| b.id)
    }

    /// A buffer to draw into, waiting if the compositor holds them all. Drains pending events
    /// first, to whichever window they name, so blocking does not strand a sibling's input.
    pub fn acquire(&mut self) -> Result<u32, UiError> {
        self.session.pump()?;
        loop {
            if let Some(b) = self.next_free() {
                return Ok(b);
            }
            if self.state().buffers.is_empty() {
                return Err(UiError::NoSuchBuffer);
            }
            let mut buf = [0u8; librsproto::surface::MAX_EVENT_BODY];
            let (op, len) = self.session.transport.wait_event(&mut buf)?;
            self.session.apply_event(op, &buf[..len]);
        }
    }

    /// Commit `buffer_id` with a damage rectangle, marking it busy.
    pub fn commit(&mut self, buffer_id: u32, damage: (u32, u32, u32, u32)) -> Result<(), UiError> {
        if !self.state().buffers.iter().any(|b| b.id == buffer_id) {
            return Err(UiError::NoSuchBuffer);
        }
        let mut body = [0u8; 32];
        let n = build_commit_request(
            &mut body,
            &CommitRequest {
                window: self.id(),
                buffer: buffer_id,
                damage_x: damage.0,
                damage_y: damage.1,
                damage_w: damage.2,
                damage_h: damage.3,
            },
        )
        .ok_or(UiError::Malformed)?;
        // Busy only after the send succeeds: the compositor never releases a commit it never saw.
        self.session.transport.request(OP_COMMIT, &body[..n], None, &mut [])?;
        if let Some(b) = self.state_mut().buffers.iter_mut().find(|b| b.id == buffer_id) {
            b.busy = true;
        }
        Ok(())
    }

    /// Tell the compositor what to call this window; a long title is truncated, not refused.
    pub fn set_title(&mut self, title: &str) -> Result<(), UiError> {
        let mut body = [0u8; 4 + librsproto::surface::MAX_TITLE];
        let n = librsproto::surface::title::write(self.id(), title, &mut body).ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_SET_TITLE, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Ask the compositor to move this window with the pointer — `Surface::StartMove`.
    ///
    /// From a press handler only: the compositor refuses unless this window holds the implicit
    /// pointer grab, since moving at an arbitrary moment would be placing itself.
    pub fn start_move(&mut self) -> Result<(), UiError> {
        let mut body = [0u8; 4];
        let n = librsproto::surface::StartMove { window: self.id() }.write(&mut body).ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_START_MOVE, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Hand the compositor an interactive resize of this window — `Surface::StartResize`.
    ///
    /// Refused unless this window holds the pointer grab. The manager sends the `Configure`,
    /// or not. `edges` is a mask of `RESIZE_LEFT` and friends; naming none, or both of an
    /// opposite pair, is `InvalidArgument`.
    pub fn start_resize(&mut self, edges: u32) -> Result<(), UiError> {
        let mut body = [0u8; 8];
        let n =
            librsproto::surface::StartResize { window: self.id(), edges }.write(&mut body).ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_START_RESIZE, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Say what this window takes when something is dropped on it — `Surface::DeclareAcceptor`.
    ///
    /// The compositor matches while the pointer moves, so a drag costs no round trip.
    /// `name` is what a [`WindowEvent::Drop`] says it landed on; `kinds` a mask of
    /// `DROP_KIND_FILE` and friends. Re-declaring a name replaces it.
    pub fn declare_acceptor(&mut self, name: &str, kinds: u32) -> Result<(), UiError> {
        let mut body = [0u8; librsproto::surface::DeclareAcceptor::HEAD + librsproto::surface::MAX_ACCEPTOR_NAME];
        let n = librsproto::surface::DeclareAcceptor { window: self.id(), kinds }
            .write(&mut body, name.as_bytes())
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_DECLARE_ACCEPTOR, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Offer a payload to whatever window the user drags it onto — `Surface::StartDrag`.
    ///
    /// Refused unless this window holds the pointer grab, or a client could push a path into
    /// another window untouched. One [`WindowEvent::Drop`] is delivered if it ends over a
    /// window taking `kind`. `path` is the payload and `name` only a label: a path is opened by
    /// the receiver, which reports its own errors.
    pub fn start_drag(&mut self, kind: u32, path: &str, name: &str) -> Result<(), UiError> {
        use librsproto::surface::{MAX_DROP_NAME, MAX_DROP_PATH, StartDrag};
        let mut body = [0u8; StartDrag::HEAD + MAX_DROP_PATH + MAX_DROP_NAME];
        let n = StartDrag { window: self.id(), kind, path_len: 0 }
            .write(&mut body, path.as_bytes(), name.as_bytes())
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_START_DRAG, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Ask the manager to minimise, maximise or restore this window — `Surface::RequestState`.
    ///
    /// An ask, not a change: the compositor forwards it to the manager, which may refuse.
    /// Repeating the state last asked for produces no manager event.
    pub fn request_state(&mut self, state: u32) -> Result<(), UiError> {
        let mut body = [0u8; 8];
        let n =
            librsproto::surface::WindowState { window: self.id(), state }.write(&mut body).ok_or(UiError::Malformed)?;
        self.session.transport.request(librsproto::surface::OP_REQUEST_STATE, &body[..n], None, &mut [])?;
        Ok(())
    }

    /// Destroy the window, and forget every descendant the compositor destroys with it.
    ///
    /// Transitively, as the compositor does. Consumes the ref: it caches an index that `retain`
    /// below shifts, so a ref outliving its window would re-aim at another window.
    pub fn destroy(self) -> Result<(), UiError> {
        let id = self.id();
        let mut body = [0u8; 8];
        let n = build_destroy_window_request(&mut body, id).ok_or(UiError::Malformed)?;
        self.session.transport.request(OP_DESTROY_WINDOW, &body[..n], None, &mut [])?;
        let mut gone = alloc::vec![id];
        loop {
            let before = gone.len();
            for w in &self.session.windows {
                if let Some(p) = w.parent
                    && gone.contains(&p)
                    && !gone.contains(&w.id)
                {
                    gone.push(w.id);
                }
            }
            if gone.len() == before {
                break;
            }
        }
        self.session.windows.retain(|w| !gone.contains(&w.id));
        Ok(())
    }
}

/// The parent a role names, if it names one.
fn parent_of(role: Role) -> Option<u32> {
    match role {
        Role::Popup { parent } | Role::Dialog { parent } => Some(parent),
        Role::Normal | Role::Panel { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use librsproto::surface::{
        FocusEvent, MOD_SHIFT, OP_FOCUS_EVENT, OP_KEY_EVENT, OP_POINTER_EVENT, OP_WINDOW_STATE, POINTER_MOTION,
    };

    #[derive(Default)]
    struct MockTransport {
        sent: Vec<(u16, Vec<u8>, Option<u64>)>,
        next_window: u32,
        events: Vec<(u16, Vec<u8>)>,
        deferred: Vec<(u16, Vec<u8>)>,
        waits: usize,
        fail: bool,
        /// Answer `CreateWindow`, then fail the wait for its `Configure`.
        fail_after_create: bool,
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
        fn queue_key(&mut self, keycode: u16, pressed: bool, modifiers: u16) {
            let e = KeyEvent::new(self.next_window, keycode, u16::from(pressed), modifiers);
            let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
        }

        fn queue_key_for(&mut self, window: u32, keycode: u16, pressed: bool, modifiers: u16) {
            let e = KeyEvent::new(window, keycode, u16::from(pressed), modifiers);
            let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
        }

        fn queue_pointer_for(&mut self, window: u32, kind: u16, x: i32, y: i32) {
            let e = PointerEvent { window, kind, x, y, ..Default::default() };
            let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
        }

        fn queue_pointer(&mut self, kind: u16, x: i32, y: i32) {
            let e = PointerEvent { window: self.next_window, kind, x, y, ..Default::default() };
            let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
        }

        fn queue_drop(&mut self, window: u32, acceptor: &str, path: &str, name: &str) {
            use librsproto::surface::{DROP_KIND_FILE, DroppedEvent, MAX_EVENT_BODY, OP_DROPPED};
            let mut b = alloc::vec![0u8; MAX_EVENT_BODY];
            let ev = DroppedEvent { window, kind: DROP_KIND_FILE, x: 3, y: 4, acceptor_len: 0, path_len: 0 };
            let n = ev
                .write(&mut b, acceptor.as_bytes(), path.as_bytes(), name.as_bytes())
                .expect("within the protocol's caps");
            self.events.insert(0, (OP_DROPPED, b[..n].to_vec()));
        }

        fn queue_release(&mut self, window: u32, buffer: u32) {
            let mut b = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
            self.events.push((OP_RELEASE, b[..n].to_vec()));
        }

        fn defer_release(&mut self, window: u32, buffer: u32) {
            let mut b = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut b, window, buffer).unwrap();
            self.deferred.push((OP_RELEASE, b[..n].to_vec()));
        }
    }

    // ---- Multi-window ----

    #[test]
    fn two_windows_on_one_session_each_get_their_own_input() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();
        let menu = s.create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 4, 24), 2).unwrap();
        assert_ne!(parent, menu, "two distinct windows on one connection");

        s.transport.queue_key_for(menu, 30, true, 0);
        s.transport.queue_key_for(parent, 31, true, 0);
        assert_eq!(s.pump().expect("pump"), 2);

        assert_eq!(s.next_event(), Some((parent, WindowEvent::Key(KeyEvent::new(parent, 31, 1, 0)))),);
        assert_eq!(s.next_event(), Some((menu, WindowEvent::Key(KeyEvent::new(menu, 30, 1, 0)))),);
        assert_eq!(s.next_event(), None);
    }

    #[test]
    fn destroying_a_parent_forgets_the_windows_that_went_with_it() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();
        let menu = s.create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 0, 0), 2).unwrap();
        let submenu = s.create(&CreateWindowRequest::at(20, 20, Role::Popup { parent: menu }, 0, 0), 2).unwrap();
        let other = s.create(&CreateWindowRequest::new(10, 10, Role::Normal), 2).unwrap();
        assert_eq!(s.window_ids().len(), 4);

        s.window(parent).unwrap().destroy().unwrap();

        assert_eq!(s.window_ids(), alloc::vec![other], "the subtree went, and only it");
        assert!(s.window(menu).is_none(), "a popup goes with its parent");
        assert!(s.window(submenu).is_none(), "and a popup of that popup");
    }

    #[test]
    fn input_arriving_while_another_window_is_created_is_kept() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();

        s.transport.queue_key_for(parent, 42, true, 0);

        let menu = s.create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 0, 0), 2).unwrap();

        assert_eq!(
            s.next_event(),
            Some((parent, WindowEvent::Key(KeyEvent::new(parent, 42, 1, 0)))),
            "the parent's keystroke survived the sibling's handshake"
        );
        assert!(s.window(menu).unwrap().configured().is_some(), "and the menu was configured");
    }

    #[test]
    fn a_create_that_fails_during_the_handshake_leaves_no_window_behind() {
        let t = MockTransport { fail_after_create: true, ..Default::default() };
        let mut s = Session::new(t);
        assert!(s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).is_err(), "the handshake failed");
        assert!(s.window_ids().is_empty(), "and the session kept no half-made window");
    }

    /// A buffer smaller than the protocol's maximum record loses the event silently or cuts the
    /// display name, so pinning the size needs the maximum.
    #[test]
    fn a_drop_at_the_protocol_maximum_reaches_the_client_whole() {
        use librsproto::surface::{DROP_KIND_FILE, MAX_ACCEPTOR_NAME, MAX_DROP_NAME, MAX_DROP_PATH};
        let mut s = Session::new(MockTransport::default());
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();

        let acceptor = "a".repeat(MAX_ACCEPTOR_NAME);
        let path = alloc::format!("/{}", "p".repeat(MAX_DROP_PATH - 1));
        let name = "n".repeat(MAX_DROP_NAME);
        s.transport.queue_drop(w, &acceptor, &path, &name);
        s.pump().expect("pump");

        assert_eq!(
            s.next_event(),
            Some((
                w,
                WindowEvent::Drop {
                    acceptor: acceptor.clone(),
                    kind: DROP_KIND_FILE,
                    path: path.clone(),
                    name: name.clone(),
                    x: 3,
                    y: 4,
                }
            )),
            "a maximal drop arrives with nothing shortened"
        );

        s.transport.queue_drop(w, "document", "/home/papers/quarterly-report.txt", "quarterly-report.txt");
        s.pump().expect("pump");
        let Some((_, WindowEvent::Drop { path, name, .. })) = s.next_event() else {
            panic!("an ordinary drop did not arrive");
        };
        assert_eq!(path, "/home/papers/quarterly-report.txt");
        assert_eq!(name, "quarterly-report.txt");
    }

    /// A message dropped before decoding is not attributable to a window.
    #[test]
    fn transport_loss_is_announced_to_every_window() {
        let mut s = Session::new(MockTransport::default());
        let a = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        let b = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();

        s.transport.lost = true;
        s.pump().expect("pump");

        assert_eq!(s.next_event(), Some((a, WindowEvent::InputLost)), "the first window is told");
        assert_eq!(s.next_event(), Some((b, WindowEvent::InputLost)), "and so is the second");
        assert_eq!(s.next_event(), None, "once each");
    }

    #[test]
    fn one_windows_overflow_does_not_evict_anothers_events() {
        let mut s = Session::new(MockTransport::default());
        let noisy = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        let quiet = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();

        s.transport.queue_key_for(quiet, 99, true, 0);
        for i in 0..(EVENT_QUEUE_MAX + 8) {
            s.transport.queue_key_for(noisy, i as u16, true, 0);
        }
        s.pump().expect("pump");

        assert_eq!(
            s.next_event(),
            Some((noisy, WindowEvent::InputLost)),
            "the window that overflowed is the window that is told"
        );
        let mut from_noisy = 0;
        let mut quiet_key = None;
        while let Some((w, e)) = s.next_event() {
            if w == quiet {
                assert!(quiet_key.is_none(), "the quiet window had exactly one event");
                assert_eq!(e, WindowEvent::Key(KeyEvent::new(quiet, 99, 1, 0)));
                quiet_key = Some(e);
            } else {
                assert_ne!(e, WindowEvent::InputLost, "only the overflowing window is marked");
                from_noisy += 1;
            }
        }
        assert!(quiet_key.is_some(), "the quiet window's event survived the flood");
        assert_eq!(from_noisy, EVENT_QUEUE_MAX, "the noisy window kept exactly its bound");
    }

    #[test]
    fn a_record_for_an_unknown_window_is_dropped() {
        let mut s = Session::new(MockTransport::default());
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        s.transport.queue_key_for(w + 99, 30, true, 0);
        assert_eq!(s.pump().expect("pump"), 1, "it was read off the wire");
        assert_eq!(s.next_event(), None, "and went nowhere");
    }

    /// A session holding one window; forwards session-wide calls to the session.
    struct One {
        s: Session<MockTransport>,
        id: u32,
    }

    impl One {
        fn w(&mut self) -> WindowRef<'_, MockTransport> {
            self.s.window(self.id).expect("the fixture's window")
        }
        fn id(&self) -> u32 {
            self.id
        }
        fn pump(&mut self) -> Result<usize, UiError> {
            self.s.pump()
        }
        fn next_event(&mut self) -> Option<WindowEvent> {
            self.s.next_event().map(|(_, e)| e)
        }
        fn events_pending(&self) -> usize {
            self.s.events_pending()
        }
        fn wait_event(&mut self) -> Result<WindowEvent, UiError> {
            Ok(self.s.wait_event()?.1)
        }
        fn buffers(&mut self) -> Vec<ClientBuffer> {
            self.w().buffers().to_vec()
        }
        fn next_free(&mut self) -> Option<u32> {
            self.w().next_free()
        }
        fn acquire(&mut self) -> Result<u32, UiError> {
            self.w().acquire()
        }
        fn attach(&mut self, b: u32, width: u32, height: u32, pitch: u32, handle: u64) -> Result<(), UiError> {
            self.w().attach(b, width, height, pitch, handle)
        }
        fn commit(&mut self, b: u32, d: (u32, u32, u32, u32)) -> Result<(), UiError> {
            self.w().commit(b, d)
        }
    }

    fn window(count: usize) -> One {
        let mut s = Session::new(MockTransport::default());
        let id = s.create(&CreateWindowRequest::new(64, 32, Role::Normal), count).expect("created");
        let mut one = One { s, id };
        for i in 0..count {
            one.attach(i as u32, 64, 32, 64 * 4, 100 + i as u64).unwrap();
        }
        one
    }

    #[test]
    fn a_window_gets_its_id_from_the_server_reply() {
        let mut w = window(2);
        assert_eq!(w.id(), 1);
        assert_eq!(w.buffers().len(), 2);
    }

    #[test]
    fn single_buffering_is_refused_at_construction() {
        for count in [0, 1] {
            assert_eq!(
                Session::new(MockTransport::default())
                    .create(&CreateWindowRequest::new(8, 8, Role::Normal), count)
                    .err(),
                Some(UiError::TooFewBuffers)
            );
        }
        assert!(
            Session::new(MockTransport::default()).create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).is_ok()
        );
    }

    #[test]
    fn re_attaching_an_id_replaces_its_record_rather_than_adding_one() {
        let mut w = window(2);
        assert_eq!(w.buffers().len(), 2);

        w.attach(0, 128, 64, 128 * 4, 200).unwrap();
        assert_eq!(w.buffers().len(), 2, "replaced, not added");
        let g = w.w().buffer_geometry(0).expect("still attached");
        assert_eq!((g.width, g.height, g.pitch), (128, 64, 128 * 4));
        let other = w.w().buffer_geometry(1).expect("untouched");
        assert_eq!((other.width, other.height), (64, 32), "the other buffer is not affected");
    }

    #[test]
    fn a_replaced_buffer_keeps_the_busy_state_the_compositor_has_for_it() {
        let mut w = window(2);
        w.commit(0, (0, 0, 64, 32)).unwrap();
        w.attach(0, 128, 64, 128 * 4, 201).unwrap();
        w.commit(1, (0, 0, 64, 32)).unwrap();
        assert_eq!(w.next_free(), None, "both buffers are committed, so nothing is free: {:?}", w.buffers());
    }

    #[test]
    fn the_handle_is_transferred_once_at_attach_and_never_again() {
        let mut w = window(2);
        w.commit(0, (0, 0, 64, 32)).unwrap();
        w.commit(1, (0, 0, 64, 32)).unwrap();

        let attaches: Vec<_> = w.s.transport.sent.iter().filter(|(op, _, _)| *op == OP_ATTACH_BUFFER).collect();
        assert_eq!(attaches.len(), 2);
        assert!(attaches.iter().all(|(_, _, h)| h.is_some()), "attach must carry a handle");

        let commits: Vec<_> = w.s.transport.sent.iter().filter(|(op, _, _)| *op == OP_COMMIT).collect();
        assert_eq!(commits.len(), 2);
        assert!(commits.iter().all(|(_, _, h)| h.is_none()), "a commit transfers nothing");
    }

    #[test]
    fn the_format_a_client_asks_for_is_the_tag_that_goes_on_the_wire() {
        // Byte 20 of the attach body is the format word.
        fn tag_of(w: &One) -> u32 {
            let (_, body, _) = w.s.transport.sent.iter().rev().find(|(op, _, _)| *op == OP_ATTACH_BUFFER).unwrap();
            u32::from_le_bytes(body[20..24].try_into().unwrap())
        }

        let mut w = window(2);
        assert_eq!(tag_of(&w), SURFACE_FORMAT_XRGB8888, "the plain attach is opaque");

        w.w().attach_with_format(0, 64, 32, 64 * 4, 300, PixelFormat::ARGB8888).unwrap();
        assert_eq!(tag_of(&w), SURFACE_FORMAT_ARGB8888);
        assert!(w.w().buffer_geometry(0).unwrap().format.has_alpha());

        w.w().attach_with_format(0, 64, 32, 64 * 4, 301, PixelFormat::XRGB8888).unwrap();
        assert_eq!(tag_of(&w), SURFACE_FORMAT_XRGB8888);
        assert!(!w.w().buffer_geometry(0).unwrap().format.has_alpha());
    }

    #[test]
    fn a_format_with_no_wire_tag_is_refused_rather_than_approximated() {
        let mut w = window(2);
        let before = w.s.transport.sent.len();
        let e = w.w().attach_with_format(0, 64, 32, 64 * 4, 302, PixelFormat::XBGR8888);
        assert_eq!(e, Err(UiError::UnsupportedFormat));
        assert_eq!(w.s.transport.sent.len(), before, "a refused attach must send nothing");
        assert_eq!(w.w().buffer_geometry(0).unwrap().format, PixelFormat::XRGB8888);
    }

    #[test]
    fn committing_marks_a_buffer_busy_and_a_release_frees_it() {
        let mut w = window(2);
        assert_eq!(w.next_free(), Some(0));
        w.commit(0, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), Some(1), "the other buffer is still free");

        w.commit(1, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), None, "the compositor holds both");

        w.s.transport.queue_release(1, 0);
        assert_eq!(w.pump().unwrap(), 1);
        assert_eq!(w.next_free(), Some(0));
    }

    #[test]
    fn double_buffering_can_draw_forever_without_ever_touching_a_busy_buffer() {
        let mut w = window(2);
        let mut previous: Option<u32> = None;
        for frame in 0..10 {
            w.pump().unwrap();
            let b = w.next_free().unwrap_or_else(|| panic!("stalled at frame {frame}"));
            assert!(!w.buffers().iter().find(|x| x.id == b).unwrap().busy);
            w.commit(b, (0, 0, 64, 32)).unwrap();
            if let Some(p) = previous {
                w.s.transport.queue_release(w.id(), p);
            }
            previous = Some(b);
        }
    }

    #[test]
    fn a_release_for_another_window_is_ignored() {
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.s.transport.queue_release(w.id() + 99, 0);
        w.pump().unwrap();
        assert_eq!(w.next_free(), Some(1), "buffer 0 must still be busy");
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn a_release_naming_an_unknown_buffer_is_ignored() {
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.s.transport.queue_release(w.id(), 42);
        w.pump().unwrap();
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn committing_an_unattached_buffer_is_refused_before_it_reaches_the_wire() {
        let mut w = window(2);
        let before = w.s.transport.sent.len();
        assert_eq!(w.commit(7, (0, 0, 1, 1)), Err(UiError::NoSuchBuffer));
        assert_eq!(w.s.transport.sent.len(), before, "nothing was sent");
    }

    #[test]
    fn a_pitch_too_small_for_a_row_is_refused_at_attach() {
        let mut w = window(2);
        assert_eq!(w.attach(9, 64, 32, 64 * 4 - 1, 1), Err(UiError::Malformed));
    }

    #[test]
    fn a_new_window_has_been_configured_before_it_returns() {
        let t = MockTransport { next_window: 6, ..Default::default() };
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(320, 200, Role::Normal), 2).expect("created");
        let cfg = s.window(id).unwrap().configured().expect("configured before create returned");
        assert_eq!(cfg.window, id);
        assert_eq!((cfg.width, cfg.height), (320, 200), "echoed, with no manager to disagree");
        assert_eq!((cfg.x, cfg.y), (0, 0));
    }

    #[test]
    fn the_handshake_configure_is_not_delivered_as_an_event() {
        let t = MockTransport { next_window: 1, ..Default::default() };
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(64, 32, Role::Normal), 2).expect("created");
        assert!(s.poll_event().expect("ok").is_none(), "the handshake leaked into the queue");

        let mut cfg = [0u8; 20];
        ConfigureEvent { window: id, width: 100, height: 50, x: 7, y: 9 }.write(&mut cfg).unwrap();
        s.transport.events.push((OP_CONFIGURE, cfg.to_vec()));
        assert_eq!(
            s.poll_event().expect("ok"),
            Some((id, WindowEvent::Configure { width: 100, height: 50, x: 7, y: 9 })),
        );
        assert_eq!(s.window(id).unwrap().configured().unwrap().x, 7, "and it updates what the window knows");
    }

    #[test]
    fn a_configure_for_another_window_is_not_this_windows_business() {
        let t = MockTransport { next_window: 3, ..Default::default() };
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).expect("created");
        let before = s.window(id).unwrap().configured().unwrap();

        let mut cfg = [0u8; 20];
        ConfigureEvent { window: id + 1, width: 999, height: 999, x: 1, y: 1 }.write(&mut cfg).unwrap();
        s.transport.events.push((OP_CONFIGURE, cfg.to_vec()));
        assert!(s.poll_event().expect("ok").is_none(), "somebody else's configure was queued");
        assert_eq!(s.window(id).unwrap().configured().unwrap(), before, "and it did not overwrite ours");
    }

    #[test]
    fn acquire_waits_when_the_compositor_holds_every_buffer() {
        let mut w = window(2);
        w.commit(0, (0, 0, 1, 1)).unwrap();
        w.commit(1, (0, 0, 1, 1)).unwrap();
        assert_eq!(w.next_free(), None, "both are held");

        w.s.transport.defer_release(1, 0);
        assert_eq!(w.acquire().unwrap(), 0);
        assert_eq!(w.s.transport.waits, 1, "it had to block exactly once");
    }

    #[test]
    fn acquire_does_not_block_when_something_is_already_free() {
        let mut w = window(2);
        assert_eq!(w.acquire().unwrap(), 0);
        assert_eq!(w.s.transport.waits, 0, "no reason to wait");
    }

    #[test]
    fn a_render_loop_of_more_frames_than_buffers_never_stalls() {
        let mut w = window(2);
        let mut previous: Option<u32> = None;
        for frame in 0..8 {
            let b = w.acquire().unwrap_or_else(|e| panic!("stalled at frame {frame}: {e:?}"));
            w.commit(b, (0, 0, 1, 1)).unwrap();
            if let Some(p) = previous {
                w.s.transport.defer_release(w.id(), p);
            }
            previous = Some(b);
        }
    }

    #[test]
    fn a_failed_commit_leaves_the_buffer_drawable() {
        let mut w = window(2);
        w.s.transport.fail = true;
        assert_eq!(w.commit(0, (0, 0, 1, 1)), Err(UiError::Transport));
        assert_eq!(w.next_free(), Some(0), "buffer 0 must still be drawable");
        assert!(!w.buffers().iter().find(|b| b.id == 0).unwrap().busy);

        w.s.transport.fail = false;
        w.commit(0, (0, 0, 1, 1)).unwrap();
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn a_transport_failure_surfaces_rather_than_being_swallowed() {
        let t = MockTransport { fail: true, ..Default::default() };
        assert_eq!(
            Session::new(t).create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).err(),
            Some(UiError::Transport)
        );
    }

    #[test]
    fn the_damage_rectangle_reaches_the_wire_unchanged() {
        let mut w = window(2);
        w.commit(0, (3, 5, 17, 9)).unwrap();
        let (_, body, _) = w.s.transport.sent.iter().rev().find(|(op, _, _)| *op == OP_COMMIT).unwrap();
        let req = librsproto::surface::parse_commit_request(body).unwrap();
        assert_eq!((req.damage_x, req.damage_y, req.damage_w, req.damage_h), (3, 5, 17, 9));
    }

    #[test]
    fn transport_level_loss_surfaces_as_a_dropped_marker() {
        let mut w = window(2);
        w.s.transport.lost = true;
        w.pump().expect("pump");
        assert_eq!(w.next_event(), Some(WindowEvent::InputLost));
        assert_eq!(w.next_event(), None, "announced once, not on every drain");
    }

    #[test]
    fn a_focus_event_reaches_the_queue_as_a_bool() {
        let mut w = window(2);
        for focused in [true, false] {
            let e = FocusEvent { focused: u16::from(focused), _pad: 0, window: w.id() };
            let mut b = [0u8; 8];
            let n = e.write(&mut b).unwrap();
            w.s.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
        }
        w.pump().expect("pump");
        assert_eq!(w.next_event(), Some(WindowEvent::Focus(true)));
        assert_eq!(w.next_event(), Some(WindowEvent::Focus(false)));
    }

    #[test]
    fn the_managers_word_on_a_windows_state_reaches_that_windows_queue() {
        let mut w = window(2);
        for (window, state) in [(w.id(), 2u32), (w.id() + 1, 0)] {
            let mut b = [0u8; 8];
            librsproto::surface::WindowState { window, state }.write(&mut b).unwrap();
            w.s.transport.events.insert(0, (OP_WINDOW_STATE, b.to_vec()));
        }
        w.pump().expect("pump");
        assert_eq!(w.next_event(), Some(WindowEvent::State(2)));
        assert_eq!(w.next_event(), None, "another window's state is not this one's");
    }

    #[test]
    fn a_focus_event_for_another_window_is_not_this_windows_business() {
        let mut w = window(2);
        let other = w.id() + 1;
        let e = FocusEvent { focused: 1, _pad: 0, window: other };
        let mut b = [0u8; 8];
        let n = e.write(&mut b).unwrap();
        w.s.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None, "not addressed to this window");
    }

    #[test]
    fn a_truncated_focus_event_is_ignored_rather_than_read_as_unfocused() {
        let mut w = window(2);
        w.s.transport.events.insert(0, (OP_FOCUS_EVENT, vec![0u8; 7]));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None);
    }

    #[test]
    fn input_for_another_window_on_the_same_session_is_not_delivered_here() {
        let mut w = window(2);
        let mine = w.id();
        let other = mine + 1;

        w.s.transport.queue_key_for(other, 30, true, 0);
        w.s.transport.queue_pointer_for(other, POINTER_MOTION, 1, 2);
        w.s.transport.queue_key_for(mine, 31, true, 0);

        assert_eq!(w.pump().expect("pump"), 3, "all three were read off the wire");
        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Key(KeyEvent::new(mine, 31, 1, 0))),
            "only the record naming this window is queued"
        );
        assert_eq!(w.next_event(), None, "and the sibling's two were dropped, not queued");
    }

    #[test]
    fn key_and_pointer_events_reach_the_queue_intact() {
        let mut w = window(2);
        w.s.transport.queue_key(30, true, MOD_SHIFT);
        w.s.transport.queue_pointer(POINTER_MOTION, -7, 12);
        assert_eq!(w.pump().expect("pump"), 2);

        assert_eq!(w.next_event(), Some(WindowEvent::Key(KeyEvent::new(w.id(), 30, 1, MOD_SHIFT))));
        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Pointer(PointerEvent {
                window: w.id(),
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
        let mut w = window(2);
        let b = w.acquire().expect("a free buffer");
        w.commit(b, (0, 0, 64, 32)).expect("commit");
        let b2 = w.acquire().expect("the other buffer");
        w.commit(b2, (0, 0, 64, 32)).expect("commit");

        w.s.transport.queue_key(31, true, 0);
        w.s.transport.deferred.push({
            let mut bb = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut bb, w.id(), b).unwrap();
            (OP_RELEASE, bb[..n].to_vec())
        });

        assert_eq!(w.acquire().expect("released"), b);
        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Key(KeyEvent::new(w.id(), 31, 1, 0))),
            "the keystroke that arrived while blocked survived"
        );
    }

    #[test]
    fn a_release_arriving_while_waiting_for_input_is_not_discarded() {
        let mut w = window(2);
        let b = w.acquire().expect("free");
        w.commit(b, (0, 0, 64, 32)).expect("commit");
        assert!(w.buffers().iter().find(|x| x.id == b).unwrap().busy);

        // Queued up front, `pump` would consume the release before `wait_event` blocks.
        let wid = w.id();
        w.s.transport.deferred.push({
            let e = KeyEvent::new(wid, 1, 1, 0);
            let mut bb = [0u8; core::mem::size_of::<KeyEvent>()];
            let n = e.write(&mut bb).unwrap();
            (OP_KEY_EVENT, bb[..n].to_vec())
        });
        w.s.transport.deferred.push({
            let mut bb = [0u8; 8];
            let n = librsproto::surface::build_release_event(&mut bb, w.id(), b).unwrap();
            (OP_RELEASE, bb[..n].to_vec())
        });

        assert!(matches!(w.wait_event(), Ok(WindowEvent::Key(_))));
        assert!(!w.buffers().iter().find(|x| x.id == b).unwrap().busy, "the release was applied, not thrown away");
    }

    #[test]
    fn an_overflowing_queue_drops_the_oldest_and_says_so() {
        let mut w = window(2);
        for i in 0..(EVENT_QUEUE_MAX as u16 + 3) {
            w.s.transport.queue_key(i, true, 0);
        }
        w.pump().expect("pump");
        assert_eq!(w.events_pending(), EVENT_QUEUE_MAX);

        assert_eq!(w.next_event(), Some(WindowEvent::InputLost));
        let Some(WindowEvent::Key(k)) = w.next_event() else { panic!("a key") };
        assert_eq!(k.keycode, 3, "the three oldest went, not the three newest");

        while let Some(e) = w.next_event() {
            assert_ne!(e, WindowEvent::InputLost);
        }
    }

    #[test]
    fn the_newest_event_is_the_one_kept() {
        let mut w = window(2);
        for i in 0..(EVENT_QUEUE_MAX as u16 + 1) {
            w.s.transport.queue_key(i, true, 0);
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
        w.s.transport.events.insert(0, (OP_POINTER_EVENT, vec![0u8; 19]));
        w.s.transport.queue_key(42, true, 0);
        w.pump().expect("pump");
        assert_eq!(w.events_pending(), 1, "the short pointer record was dropped");
        assert!(matches!(w.next_event(), Some(WindowEvent::Key(_))));
    }
}
