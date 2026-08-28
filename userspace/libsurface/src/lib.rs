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
//! needing a running compositor. See [`WindowRef::next_free`].
//!
//! ## Single buffering cannot work, and the library says so
//!
//! A buffer is busy from the moment it is committed until the compositor releases it, and
//! the compositor releases the buffer that *left* the screen — never the one on it. With
//! one buffer there is never anything to release, so [`WindowRef::next_free`] returns `None`
//! forever after the first commit. That is not a bug to work around: drawing into the
//! buffer the compositor is reading is exactly the tearing the protocol exists to prevent.
//! [`Session::create`] refuses fewer than two.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::Geometry;
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, ConfigureEvent, CreateWindowRequest, OP_ATTACH_BUFFER,
    OP_COMMIT, OP_CONFIGURE,
    FocusEvent, KeyEvent, OP_CREATE_WINDOW, OP_DESTROY_WINDOW, OP_FOCUS_EVENT, OP_KEY_EVENT,
    OP_POINTER_EVENT, OP_RELEASE, PointerEvent, Role, SURFACE_FORMAT_XRGB8888,
    build_attach_buffer_request, build_commit_request, build_create_window_request,
    build_destroy_window_request, parse_create_window_reply, parse_release_event,
};

pub mod buffers;
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

    /// The handle a caller may `sys_wait` on to learn an event is pending, or `0` if this
    /// transport has none.
    ///
    /// **For a client with a second source of work.** `wait_event` blocks on this transport
    /// alone, which is right for a client whose only input is the compositor — and wrong for
    /// one that also holds, say, a terminal backend: it would render the shell's output only
    /// after the next keystroke, so a prompt would appear one keypress late. Such a client
    /// waits on this handle *and* its own, then drains with
    /// [`poll_event`](Self::poll_event).
    ///
    /// `0` for a transport that cannot be waited on — the test mock — so a caller that gets
    /// one falls back to `wait_event` rather than spinning on a handle the kernel will reject.
    fn wait_handle(&self) -> u64 {
        0
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

    fn wait_handle(&self) -> u64 {
        (**self).wait_handle()
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
    /// Somebody with the manager channel is asking this window to close.
    ///
    /// **A request with no way to refuse it, and none needed.** A client that wants to ask
    /// "save first?" opens a dialog and closes when that resolves; a client that ignores this
    /// stays open, and a shell that cares will eventually insist. What this buys is that the
    /// decision reaches the process holding the work rather than being taken from it.
    CloseRequested,
    /// The compositor would like this window at this position and size.
    ///
    /// **A request, not a command.** The compositor cannot resize a client's buffer — the
    /// client allocates it — so a client answers by committing a buffer of that size, or
    /// declines by committing whatever it likes. Declining is legal and stays legal: a
    /// fixed-size window is ordinary.
    ///
    /// The **first** one is not delivered here at all: [`Session::create`] waits for it and returns
    /// once it has arrived, because a window is not composited until it has been configured.
    /// What reaches a client through this variant is therefore always a *later* opinion — a
    /// manager moving or resizing a window that is already on screen.
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
    /// The queue overflowed and events were discarded.
    ///
    /// **Accumulated state must be discarded.** A client tracking which keys or buttons are
    /// down has to assume it missed a release, exactly as `libinput` requires of a
    /// `SYN_DROPPED`. A client that ignores this carries a phantom held modifier for the
    /// rest of its life, which is the failure the marker exists to prevent.
    Dropped,
}

/// Per-window state on a [`Session`].
///
/// Split out from the session itself because a session owns the *connection* and a window owns
/// what is true of one window: its buffers, its queued input, and whether it has been
/// configured. The transport is shared; none of this is.
struct WindowState {
    id: u32,
    /// The parent this window was created against, for the roles that have one.
    ///
    /// Kept so [`WindowRef::destroy`] can drop the descendants the *compositor* drops: destroy
    /// is transitive there, so a client that forgot a menu's submenu would hold state for a
    /// window that no longer exists and could never be told about again.
    parent: Option<u32>,
    buffers: Vec<ClientBuffer>,
    /// Input delivered but not yet drained by the client.
    events: alloc::collections::VecDeque<WindowEvent>,
    /// Whether the next drain owes the client a [`WindowEvent::Dropped`].
    ///
    /// A flag rather than a count: the client's obligation is the same whether it missed one
    /// event or forty — discard what you believed — so a number would be information nobody
    /// can act on differently.
    lost: bool,
    /// The most recent `Configure`, or `None` before the first has arrived.
    configured: Option<ConfigureEvent>,
    /// Whether the handshake configure has been seen.
    ///
    /// Separates "the compositor has answered, you may commit" from "the compositor has changed
    /// its mind". Without it the first configure would reach the client as an event about a
    /// window it has not drawn to yet — an opinion about a size nothing has been committed at.
    mapped: bool,
}

/// One Surface connection and every window on it.
///
/// **A session, not a window, is what a client holds.** The protocol has always allowed several
/// windows on one connection — a popup may only name a parent *its own connection owns* — but
/// the API did not: the old `Window` owned its `Transport`, so holding a window meant holding
/// the only handle to the channel, and a client could drive exactly one. That made a menu
/// impossible to build, which is what this type exists for (M6 C3).
///
/// Operate on a window through [`window`](Self::window), which lends one for the length of a
/// call. Events arrive for the session and are routed to the window they name — see
/// [`next_event`](Self::next_event).
///
/// **Hold this behind a `Box` if `T` is a `ChannelTransport`**: it is ~9 KiB of message buffers
/// against a 32 KiB user stack, and a client that moves one by value in a loop overflows it and
/// dies in its prologue.
pub struct Session<T: Transport> {
    transport: T,
    windows: Vec<WindowState>,
}

impl<T: Transport> Session<T> {
    /// Wrap a connected transport. No window exists yet.
    pub fn new(transport: T) -> Self {
        Self { transport, windows: Vec::new() }
    }

    /// Create a window with `buffers` buffers, returning its id.
    ///
    /// Refuses fewer than two: with one buffer a client can never redraw without drawing into
    /// pixels the compositor is reading. [`WindowRef::attach`] is called for each, and the
    /// caller supplies the shared memory each buffer names.
    ///
    /// **Blocks until the window's first `Configure` arrives**, which is the client obligation
    /// the compositor's ordering rests on: a window is not composited until it has been
    /// configured, and the round trip is deliberately the client's to wait out. The alternative
    /// — the compositor asking a manager before replying — would put a userspace process on the
    /// critical path of every window creation, where a wedged shell stops clients starting at
    /// all. With no manager attached the answer is immediate and this does not block.
    ///
    /// **Records that arrive for other windows while it blocks are routed to them**, not lost.
    /// The old one-window-per-connection API could not do that, and it is the reason a client
    /// may now open a menu without stalling the window underneath it.
    pub fn create(
        &mut self,
        req: &CreateWindowRequest,
        buffers: usize,
    ) -> Result<u32, UiError> {
        if buffers < 2 {
            return Err(UiError::TooFewBuffers);
        }
        let mut body = [0u8; 32];
        let n = build_create_window_request(&mut body, req).ok_or(UiError::Malformed)?;
        let mut reply = [0u8; 32];
        let len = self
            .transport
            .request(OP_CREATE_WINDOW, &body[..n], None, &mut reply)?
            .ok_or(UiError::BadReply)?;
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
            let mut buf = [0u8; 64];
            let (op, n) = match self.transport.wait_event(&mut buf) {
                Ok(ev) => ev,
                // **Take the half-made window back out.** Leaving it would break
                // `WindowRef::configured`'s promise that it is never `None`, and hand the
                // caller — who has an `Err` in hand and no id — a window it does not know it
                // owns (PR #222 review, finding 6).
                Err(e) => {
                    self.windows.retain(|w| w.id != id);
                    return Err(e);
                }
            };
            self.apply_event(op, &buf[..n]);
        }
        Ok(id)
    }

    /// Lend `id` for the length of one call chain. `None` if this session has no such window.
    ///
    /// One window is borrowed at a time, which is all any caller needs: operations are
    /// sequential, and the alternative — handing out several live handles — would mean several
    /// paths to the one transport.
    pub fn window(&mut self, id: u32) -> Option<WindowRef<'_, T>> {
        let i = self.idx(id)?;
        Some(WindowRef { session: self, index: i })
    }

    /// Every window this session still holds, in creation order.
    pub fn window_ids(&self) -> Vec<u32> {
        self.windows.iter().map(|w| w.id).collect()
    }

    /// Take the next queued input event and the window it belongs to, if any.
    ///
    /// Does not talk to the compositor; call [`pump`](Self::pump) first to collect what has
    /// arrived. **Windows are scanned in creation order**, so a client that drains until this
    /// returns `None` — which is the intended shape — sees everything, while one that takes a
    /// single event per iteration favours its oldest window.
    pub fn next_event(&mut self) -> Option<(u32, WindowEvent)> {
        for i in 0..self.windows.len() {
            let w = &mut self.windows[i];
            if w.lost {
                // Announced before the surviving events, so a client resets its state and
                // *then* applies what it still has, rather than the other way round.
                w.lost = false;
                return Some((w.id, WindowEvent::Dropped));
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
    ///
    /// The companion of [`wait_handle`](Self::wait_handle): a client waiting on several sources
    /// blocks itself and then drains this until it returns `None`.
    pub fn poll_event(&mut self) -> Result<Option<(u32, WindowEvent)>, UiError> {
        self.pump()?;
        loop {
            if let Some(e) = self.next_event() {
                return Ok(Some(e));
            }
            let mut buf = [0u8; 64];
            match self.transport.poll_event(&mut buf)? {
                Some((op, len)) => self.apply_event(op, &buf[..len]),
                None => return Ok(None),
            }
        }
    }

    /// Block until an input event is available, then take it.
    ///
    /// Buffer releases arriving meanwhile are applied rather than discarded — they are the same
    /// channel, and dropping one strands a buffer forever.
    pub fn wait_event(&mut self) -> Result<(u32, WindowEvent), UiError> {
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

    /// Drain pending server events, freeing released buffers. Returns how many arrived.
    ///
    /// Non-blocking: a client with something to draw calls this, then [`WindowRef::next_free`],
    /// and only waits if there is still nothing free.
    pub fn pump(&mut self) -> Result<usize, UiError> {
        let mut seen = 0usize;
        // Loss below this layer is loss to the client. It is not attributable to one window —
        // the transport dropped a message without decoding it — so every window is told, which
        // is the safe direction: a client that discards accumulated state it did not have to is
        // correct, one that keeps state it should have discarded is not.
        if self.transport.took_loss() {
            for w in &mut self.windows {
                w.lost = true;
            }
        }
        let mut buf = [0u8; 64];
        while let Some((op, len)) = self.transport.poll_event(&mut buf)? {
            self.apply_event(op, &buf[..len]);
            seen += 1;
        }
        Ok(seen)
    }

    /// Give the transport back, dropping every window's local state.
    ///
    /// The windows themselves are **not** destroyed: the compositor keeps them until this
    /// session's channel closes or they are destroyed explicitly.
    pub fn into_transport(self) -> T {
        self.transport
    }

    fn idx(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    /// Apply one server event, routing it to the window it names.
    ///
    /// Input is **queued here rather than returned**, so a client blocked in `acquire` waiting
    /// for a buffer does not lose the keystrokes that arrive while it waits. That is the
    /// ordinary case, not a corner: a client renders, commits, and blocks — which is precisely
    /// when a user is looking at the result and typing.
    ///
    /// A record naming a window this session does not hold is **dropped**, not queued anywhere:
    /// the compositor may still have had one in flight for a window just destroyed.
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
            OP_KEY_EVENT => {
                if let Some(e) = KeyEvent::read(body)
                    && let Some(i) = self.idx(e.window)
                {
                    self.enqueue(i, WindowEvent::Key(e));
                }
            }
            OP_POINTER_EVENT => {
                if let Some(e) = PointerEvent::read(body)
                    && let Some(i) = self.idx(e.window)
                {
                    self.enqueue(i, WindowEvent::Pointer(e));
                }
            }
            OP_FOCUS_EVENT => {
                if let Some(e) = FocusEvent::read(body)
                    && let Some(i) = self.idx(e.window)
                {
                    self.enqueue(i, WindowEvent::Focus(e.focused != 0));
                }
            }
            librsproto::surface::OP_CLOSE_REQUESTED => {
                if let Some(r) = librsproto::surface::WindowRef::read(body)
                    && let Some(i) = self.idx(r.window)
                {
                    self.enqueue(i, WindowEvent::CloseRequested);
                }
            }
            OP_CONFIGURE => {
                if let Some(e) = ConfigureEvent::read(body)
                    && let Some(i) = self.idx(e.window)
                {
                    self.windows[i].configured = Some(e);
                    // The first one is the handshake `create` is waiting on and is not an
                    // application event; only later ones are. A client that saw the first as an
                    // event would act on a size it has not yet committed anything at.
                    if self.windows[i].mapped {
                        self.enqueue(
                            i,
                            WindowEvent::Configure {
                                width: e.width,
                                height: e.height,
                                x: e.x,
                                y: e.y,
                            },
                        );
                    }
                    self.windows[i].mapped = true;
                }
            }
            _ => {}
        }
    }

    /// Queue an event on window `i`, discarding the oldest if its queue is full.
    ///
    /// **Oldest, not newest.** The newest event is the one describing the world as it is now —
    /// dropping it would leave a client acting on a stale button state forever, where dropping
    /// the oldest only costs it history it is already behind on.
    ///
    /// **Per window, not per session**, so a noisy window cannot evict a quiet one's events.
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

    /// The compositor's most recent opinion of this window's position and size.
    ///
    /// Never `None`: waiting for the first is what [`Session::create`] does last.
    pub fn configured(&self) -> Option<ConfigureEvent> {
        self.state().configured
    }

    /// The buffers attached so far, in attach order.
    pub fn buffers(&self) -> &[ClientBuffer] {
        &self.state().buffers
    }

    /// Attach shared memory as buffer `buffer_id`, transferring `handle`.
    ///
    /// The handle crosses **once per attach**; thereafter the buffer is named by id. `pitch` is
    /// the client's own row stride and need not be `width * 4`.
    ///
    /// **Re-attaching an id replaces it**, which is how a client resizes (M9 Part D): a window
    /// whose buffers are the wrong size hands the compositor new memory under the same ids
    /// rather than inventing new ones the compositor would then hold for the window's life.
    /// The compositor refuses this for the buffer it is currently displaying, so a caller
    /// replaces a **free** buffer — [`BufferPool`](crate::buffers::BufferPool) is the piece
    /// that gets that sequence right.
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
                window: self.id(),
                buffer: buffer_id,
                width,
                height,
                pitch,
                format: SURFACE_FORMAT_XRGB8888,
            },
        )
        .ok_or(UiError::Malformed)?;
        self.session.transport.request(OP_ATTACH_BUFFER, &body[..n], Some(handle), &mut [])?;
        // Replaced in place when the id is already known, so the record count tracks the
        // compositor's rather than growing by one per resize.
        match self.state_mut().buffers.iter_mut().find(|b| b.id == buffer_id) {
            Some(b) => b.geometry = geometry,
            None => {
                self.state_mut().buffers.push(ClientBuffer { id: buffer_id, geometry, busy: false })
            }
        }
        Ok(())
    }

    /// The geometry attached under `buffer_id`, if this window has such a buffer.
    ///
    /// What tells a caller its buffers are the wrong size after a `Configure`.
    pub fn buffer_geometry(&self, buffer_id: u32) -> Option<Geometry> {
        self.state().buffers.iter().find(|b| b.id == buffer_id).map(|b| b.geometry)
    }

    /// A buffer the client may draw into, or `None` if the compositor holds them all.
    ///
    /// Callers should [`Session::pump`] first: a release may already be waiting.
    pub fn next_free(&self) -> Option<u32> {
        self.state().buffers.iter().find(|b| !b.busy).map(|b| b.id)
    }

    /// A buffer to draw into, **waiting** if the compositor holds them all.
    ///
    /// This is the call a render loop wants, and the reason `next_free` alone is not enough:
    /// after committing more frames than it has buffers, a client's next buffer only becomes
    /// available when a `Release` arrives, and that may not have happened yet when it asks.
    /// Polling once and failing is how the first real client stalled at its third frame.
    ///
    /// Drains pending events first, so a release already waiting costs no block at all — and
    /// those events go to whichever window they name, so blocking here does not strand a
    /// sibling's input.
    pub fn acquire(&mut self) -> Result<u32, UiError> {
        self.session.pump()?;
        loop {
            if let Some(b) = self.next_free() {
                return Ok(b);
            }
            if self.state().buffers.is_empty() {
                return Err(UiError::NoSuchBuffer);
            }
            // Nothing free: block until the compositor says something, then re-check.
            let mut buf = [0u8; 64];
            let (op, len) = self.session.transport.wait_event(&mut buf)?;
            self.session.apply_event(op, &buf[..len]);
        }
    }

    /// Commit `buffer_id` with a damage rectangle, marking it busy.
    pub fn commit(
        &mut self,
        buffer_id: u32,
        damage: (u32, u32, u32, u32),
    ) -> Result<(), UiError> {
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
        // **Marked busy only after the send succeeds.** Setting it first strands the buffer if
        // the send fails — the compositor never saw the commit and will never release it, so a
        // double-buffered client stalls forever after two such failures.
        self.session.transport.request(OP_COMMIT, &body[..n], None, &mut [])?;
        if let Some(b) = self.state_mut().buffers.iter_mut().find(|b| b.id == buffer_id) {
            b.busy = true;
        }
        Ok(())
    }

    /// Tell the compositor what to call this window.
    ///
    /// **The only client-facing use of `SetTitle`**, which shipped in M7 Part A with a
    /// compositor that stores titles and a manager event that reports them — and no way for a
    /// client to set one. So every title was empty, `WindowTitle` was never emitted, and M8
    /// Part C's window list showed `window 6` for everything (PR #242 review, optional 7).
    ///
    /// Longer titles are truncated by the compositor at a character boundary, not refused —
    /// see `docs/spec/rsproto-surface-ops.md`.
    pub fn set_title(&mut self, title: &str) -> Result<(), UiError> {
        let mut body = [0u8; 4 + librsproto::surface::MAX_TITLE];
        let n = librsproto::surface::title::write(self.id(), title, &mut body)
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(
            librsproto::surface::OP_SET_TITLE,
            &body[..n],
            None,
            &mut [],
        )?;
        Ok(())
    }

    /// Ask the compositor to move this window with the pointer — `Surface::StartMove`.
    ///
    /// **Call it from a press handler, and only from one.** The compositor refuses unless this
    /// window holds the implicit pointer grab, which is what makes "the user is dragging me"
    /// true; a client cannot move itself at an arbitrary moment, because that would be placing
    /// itself and `Place` is a manager op.
    ///
    /// The move ends when the button comes up. There is nothing to end it from here, and
    /// deliberately: the compositor is the one that knows when the button was released.
    pub fn start_move(&mut self) -> Result<(), UiError> {
        let mut body = [0u8; 4];
        let n = librsproto::surface::StartMove { window: self.id() }
            .write(&mut body)
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(
            librsproto::surface::OP_START_MOVE,
            &body[..n],
            None,
            &mut [],
        )?;
        Ok(())
    }

    /// Hand the compositor an interactive resize of this window — `Surface::StartResize`.
    ///
    /// **The same authority as [`start_move`](Self::start_move)**: refused unless this window
    /// holds the pointer grab, which is what makes "the user is dragging my edge" true. Sent
    /// from the press, not the click, for the reason a move is — the gesture *begins* at the
    /// press, and by the time a button comes up the drag is over.
    ///
    /// **Nothing about the window changes while it runs.** The compositor moves an outline and
    /// tells the manager where the user let go; the manager sends the `Configure`. So a client
    /// calling this must expect its size to change through the ordinary `Configure` path, or
    /// not at all — a shell may decide otherwise, and a fixed-size window declines it anyway.
    ///
    /// `edges` is a mask of `RESIZE_LEFT` and friends; a corner is two bits. Naming none, or
    /// both of an opposite pair, is `InvalidArgument`.
    pub fn start_resize(&mut self, edges: u32) -> Result<(), UiError> {
        let mut body = [0u8; 8];
        let n = librsproto::surface::StartResize { window: self.id(), edges }
            .write(&mut body)
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(
            librsproto::surface::OP_START_RESIZE,
            &body[..n],
            None,
            &mut [],
        )?;
        Ok(())
    }

    /// Ask the manager to minimise, maximise or restore this window — `Surface::RequestState`.
    ///
    /// **An ask, not a change.** A client cannot minimise or maximise itself: both are manager
    /// operations, and one a client could reach would let it put another window away or place
    /// itself. The compositor forwards this to whoever holds the manager channel, and what
    /// happens next is that manager's decision — a shell may refuse, and a window that is not
    /// resizable will decline the `Configure` that arrives anyway.
    ///
    /// Repeating the state last asked for produces no manager event, so a button held down or a
    /// view rebuilt every frame costs nothing.
    pub fn request_state(&mut self, state: u32) -> Result<(), UiError> {
        let mut body = [0u8; 8];
        let n = librsproto::surface::WindowState { window: self.id(), state }
            .write(&mut body)
            .ok_or(UiError::Malformed)?;
        self.session.transport.request(
            librsproto::surface::OP_REQUEST_STATE,
            &body[..n],
            None,
            &mut [],
        )?;
        Ok(())
    }

    /// Destroy the window, and forget every descendant the compositor destroys with it.
    ///
    /// **Transitively**, because that is what the compositor does: a popup goes with its parent
    /// and a submenu with that popup. A client that kept local state for those would be holding
    /// buffers and queued events for windows that no longer exist, and would never hear about
    /// them again.
    ///
    /// **Consumes the ref**, because after this there is no window for it to refer to. It
    /// caches an index into the session's list and `retain` below shifts everything after the
    /// removed entry down — so a ref that outlived its window would silently re-aim at
    /// whichever window took its place, and `attach` through it would land a buffer on someone
    /// else's window. Taking `self` by value makes that unrepresentable rather than merely
    /// unwise (PR #222 review, finding 2).
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
        /// Answer `CreateWindow`, then fail the wait for its `Configure`.
        ///
        /// Models the one failure that can strand a half-made window: the compositor took the
        /// request and the channel died before the handshake finished.
        fail_after_create: bool,
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
                // **The mock owes the handshake, because the compositor does.** `Window::new`
                // waits for the first `Configure` before returning, so a mock that replied with
                // an id and nothing else would hang every test — which is the contract being
                // modelled rather than an accommodation of it. The geometry echoes the request,
                // as a compositor with no manager attached does.
                let req = librsproto::surface::parse_create_window_request(body)
                    .ok_or(UiError::Malformed)?;
                let mut cfg = [0u8; 20];
                ConfigureEvent {
                    window: self.next_window,
                    width: req.width,
                    height: req.height,
                    x: 0,
                    y: 0,
                }
                .write(&mut cfg)
                .ok_or(UiError::Malformed)?;
                self.events.insert(0, (OP_CONFIGURE, cfg.to_vec()));
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
            // **An event already queued is not a wait**, which the real transport gets right by
            // polling before it blocks. Counting one here made the create handshake — which
            // reads a configure the mock queued during the create — look like a block, and the
            // two `acquire` tests that assert on the count are about *buffer* waits.
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
        /// Queue a key event as the compositor would send it.
        fn queue_key(&mut self, keycode: u16, pressed: bool, modifiers: u16) {
            let e = KeyEvent::new(self.next_window, keycode, u16::from(pressed), modifiers);
            let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
        }

        /// Queue a key addressed to `window` — for the multi-window filter test.
        fn queue_key_for(&mut self, window: u32, keycode: u16, pressed: bool, modifiers: u16) {
            let e = KeyEvent::new(window, keycode, u16::from(pressed), modifiers);
            let mut b = [0u8; core::mem::size_of::<KeyEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_KEY_EVENT, b[..n].to_vec()));
        }

        /// Queue a pointer record addressed to `window`.
        fn queue_pointer_for(&mut self, window: u32, kind: u16, x: i32, y: i32) {
            let e = PointerEvent { window, kind, x, y, ..Default::default() };
            let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
            let n = e.write(&mut b).unwrap();
            self.events.insert(0, (OP_POINTER_EVENT, b[..n].to_vec()));
        }

        /// Queue a pointer event as the compositor would send it.
        fn queue_pointer(&mut self, kind: u16, x: i32, y: i32) {
            let e = PointerEvent { window: self.next_window, kind, x, y, ..Default::default() };
            let mut b = [0u8; core::mem::size_of::<PointerEvent>()];
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

    // ---- Multi-window: what `Window` could not do -------------------------------------

    /// Two windows on one session, and input reaches the one it names.
    ///
    /// The whole point of the type. A popup may only name a parent **its own connection owns**,
    /// so a menu is necessarily a second window on the parent's session — and until the records
    /// carried a window id and this type existed, a client could not have both.
    #[test]
    fn two_windows_on_one_session_each_get_their_own_input() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();
        let menu = s
            .create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 4, 24), 2)
            .unwrap();
        assert_ne!(parent, menu, "two distinct windows on one connection");

        s.transport.queue_key_for(menu, 30, true, 0);
        s.transport.queue_key_for(parent, 31, true, 0);
        assert_eq!(s.pump().expect("pump"), 2);

        // Scanned in creation order, so the parent's comes first — and each carries its window.
        assert_eq!(
            s.next_event(),
            Some((parent, WindowEvent::Key(KeyEvent::new(parent, 31, 1, 0)))),
        );
        assert_eq!(
            s.next_event(),
            Some((menu, WindowEvent::Key(KeyEvent::new(menu, 30, 1, 0)))),
        );
        assert_eq!(s.next_event(), None);
    }

    /// Destroying a parent drops the descendants the compositor drops with it.
    ///
    /// Destroy is transitive on the compositor's side — a popup goes with its parent and a
    /// submenu with that popup. A session that kept local state for those would hold buffers
    /// and queued events for windows that no longer exist and can never be mentioned again.
    #[test]
    fn destroying_a_parent_forgets_the_windows_that_went_with_it() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();
        let menu = s
            .create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 0, 0), 2)
            .unwrap();
        let submenu = s
            .create(&CreateWindowRequest::at(20, 20, Role::Popup { parent: menu }, 0, 0), 2)
            .unwrap();
        let other = s.create(&CreateWindowRequest::new(10, 10, Role::Normal), 2).unwrap();
        assert_eq!(s.window_ids().len(), 4);

        s.window(parent).unwrap().destroy().unwrap();

        assert_eq!(s.window_ids(), alloc::vec![other], "the subtree went, and only it");
        assert!(s.window(menu).is_none(), "a popup goes with its parent");
        assert!(s.window(submenu).is_none(), "and a popup of that popup");
    }

    /// A sibling's input consumed by another window's handshake is routed, not dropped.
    ///
    /// `Session::create` reads from the channel until its window's first `Configure` arrives,
    /// so it necessarily consumes whatever else is queued ahead of it. Those records belong to
    /// other windows, and with one window per connection there was nowhere to put them — a
    /// client could not open a menu without losing what was typed at the window underneath.
    #[test]
    fn input_arriving_while_another_window_is_created_is_kept() {
        let mut s = Session::new(MockTransport::default());
        let parent = s.create(&CreateWindowRequest::new(200, 100, Role::Normal), 2).unwrap();

        // On the wire before the menu's handshake, so `create`'s wait loop is what consumes it.
        s.transport.queue_key_for(parent, 42, true, 0);

        let menu = s
            .create(&CreateWindowRequest::at(40, 60, Role::Popup { parent }, 0, 0), 2)
            .unwrap();

        assert_eq!(
            s.next_event(),
            Some((parent, WindowEvent::Key(KeyEvent::new(parent, 42, 1, 0)))),
            "the parent's keystroke survived the sibling's handshake"
        );
        assert!(s.window(menu).unwrap().configured().is_some(), "and the menu was configured");
    }

    /// A `create` that fails mid-handshake leaves nothing behind.
    ///
    /// The window exists on the compositor by then — the reply arrived — but the caller has an
    /// `Err` and no id, so a session that kept the entry would hold a window nobody can name,
    /// and one whose `configured` is `None`, which [`WindowRef::configured`] promises never
    /// happens (PR #222 review, finding 6).
    #[test]
    fn a_create_that_fails_during_the_handshake_leaves_no_window_behind() {
        let mut t = MockTransport::default();
        // The create request is answered; the wait for the configure is not.
        t.fail_after_create = true;
        let mut s = Session::new(t);
        assert!(
            s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).is_err(),
            "the handshake failed"
        );
        assert!(s.window_ids().is_empty(), "and the session kept no half-made window");
    }

    /// Transport-level loss is announced to **every** window, not just one.
    ///
    /// A message the transport dropped before decoding is not attributable to any window — it
    /// was never decoded, so nothing says which one it named. Telling only one window leaves
    /// the others carrying accumulated state they should have discarded: a client with a menu
    /// open keeps a phantom button-down on the menu for the rest of its life, which is exactly
    /// what `Dropped` exists to prevent.
    ///
    /// The existing loss test runs through the single-window fixture and structurally cannot
    /// see the difference — narrowing the fan-out to one window leaves it passing
    /// (PR #222 review, finding 3).
    #[test]
    fn transport_loss_is_announced_to_every_window() {
        let mut s = Session::new(MockTransport::default());
        let a = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        let b = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();

        s.transport.lost = true;
        s.pump().expect("pump");

        assert_eq!(s.next_event(), Some((a, WindowEvent::Dropped)), "the first window is told");
        assert_eq!(s.next_event(), Some((b, WindowEvent::Dropped)), "and so is the second");
        assert_eq!(s.next_event(), None, "once each");
    }

    /// One window overflowing its queue does not evict another window's events.
    ///
    /// The bound is per window precisely so a noisy window cannot cost a quiet one anything.
    /// A session-wide budget would let a drag over the terminal evict the menu's queued click
    /// and mark the *menu* `Dropped` — a window losing events because of traffic it had no part
    /// in (PR #222 review, finding 4).
    #[test]
    fn one_windows_overflow_does_not_evict_anothers_events() {
        let mut s = Session::new(MockTransport::default());
        let noisy = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        let quiet = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();

        // One event for the quiet window, then far more than the bound for the noisy one.
        s.transport.queue_key_for(quiet, 99, true, 0);
        for i in 0..(EVENT_QUEUE_MAX + 8) {
            s.transport.queue_key_for(noisy, i as u16, true, 0);
        }
        s.pump().expect("pump");

        // The noisy window overflowed and is told so; the quiet one is untouched.
        assert_eq!(
            s.next_event(),
            Some((noisy, WindowEvent::Dropped)),
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
                assert_ne!(e, WindowEvent::Dropped, "only the overflowing window is marked");
                from_noisy += 1;
            }
        }
        assert!(quiet_key.is_some(), "the quiet window's event survived the flood");
        assert_eq!(from_noisy, EVENT_QUEUE_MAX, "the noisy window kept exactly its bound");
    }

    /// A record naming a window this session does not hold is dropped, not misattributed.
    #[test]
    fn a_record_for_an_unknown_window_is_dropped() {
        let mut s = Session::new(MockTransport::default());
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).unwrap();
        s.transport.queue_key_for(w + 99, 30, true, 0);
        assert_eq!(s.pump().expect("pump"), 1, "it was read off the wire");
        assert_eq!(s.next_event(), None, "and went nowhere");
    }

    /// A session holding exactly one window, with `count` buffers attached.
    ///
    /// **A fixture for the single-window shape**, which is what every test below this line was
    /// written against and what most clients still are. It forwards to the session for the
    /// calls that are now session-wide and to the window for the rest, so those tests keep
    /// saying what they always said. The multi-window behaviour that replaced `Window` is
    /// tested through `Session` directly — see the tests at the end of this module.
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
        fn poll_event(&mut self) -> Result<Option<WindowEvent>, UiError> {
            Ok(self.s.poll_event()?.map(|(_, e)| e))
        }
        fn wait_event(&mut self) -> Result<WindowEvent, UiError> {
            Ok(self.s.wait_event()?.1)
        }
        fn configured(&mut self) -> Option<ConfigureEvent> {
            self.w().configured()
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
        fn attach(
            &mut self,
            b: u32,
            width: u32,
            height: u32,
            pitch: u32,
            handle: u64,
        ) -> Result<(), UiError> {
            self.w().attach(b, width, height, pitch, handle)
        }
        fn commit(&mut self, b: u32, d: (u32, u32, u32, u32)) -> Result<(), UiError> {
            self.w().commit(b, d)
        }
    }

    fn window(count: usize) -> One {
        let mut s = Session::new(MockTransport::default());
        let id = s
            .create(&CreateWindowRequest::new(64, 32, Role::Normal), count)
            .expect("created");
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
        // Not a limitation to work around: with one buffer, the only thing to draw into is
        // the buffer the compositor is reading.
        for count in [0, 1] {
            assert_eq!(
                Session::new(MockTransport::default()).create(&CreateWindowRequest::new(8, 8, Role::Normal), count).err(),
                Some(UiError::TooFewBuffers)
            );
        }
        assert!(Session::new(MockTransport::default()).create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).is_ok());
    }

    #[test]
    fn re_attaching_an_id_replaces_its_record_rather_than_adding_one() {
        // **The client half of how a resize works** (M9 Part D). The compositor replaces the
        // memory behind an id rather than refusing it, so this side must replace the record —
        // a pushed duplicate would make `buffer_geometry` and `next_free` answer from
        // whichever copy came first, and a client would go on believing its buffers were the
        // old size after resizing them.
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
        // A commit makes a buffer busy; a client must not be able to launder that by
        // re-attaching. It cannot reach this — the compositor refuses re-attaching the
        // displayed buffer — but the record here is what `next_free` reads, and a replace that
        // cleared `busy` would hand the caller a buffer the compositor is still reading back.
        let mut w = window(2);
        w.commit(0, (0, 0, 64, 32)).unwrap();
        w.attach(0, 128, 64, 128 * 4, 201).unwrap();
        w.commit(1, (0, 0, 64, 32)).unwrap();
        assert_eq!(
            w.next_free(),
            None,
            "both buffers are committed, so nothing is free: {:?}",
            w.buffers()
        );
    }

    #[test]
    fn the_handle_is_transferred_once_at_attach_and_never_again() {
        // The whole point of the shared-memory design: the handle crosses on attach, and
        // commits thereafter name the buffer by id.
        let mut w = window(2);
        w.commit(0, (0, 0, 64, 32)).unwrap();
        w.commit(1, (0, 0, 64, 32)).unwrap();

        let attaches: Vec<_> =
            w.s.transport.sent.iter().filter(|(op, _, _)| *op == OP_ATTACH_BUFFER).collect();
        assert_eq!(attaches.len(), 2);
        assert!(attaches.iter().all(|(_, _, h)| h.is_some()), "attach must carry a handle");

        let commits: Vec<_> =
            w.s.transport.sent.iter().filter(|(op, _, _)| *op == OP_COMMIT).collect();
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
        w.s.transport.queue_release(1, 0);
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
                w.s.transport.queue_release(w.id(), p);
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
        // **The handshake.** A window is not composited until it has been configured, so
        // `Window::new` does not hand back a window a client could commit on before the
        // compositor has said where it goes. That is what lets a manager place a window before
        // it is ever seen, with the round trip on the client's side rather than the
        // compositor's.
        let mut t = MockTransport::default();
        t.next_window = 6;
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(320, 200, Role::Normal), 2).expect("created");
        let cfg = s.window(id).unwrap().configured().expect("configured before create returned");
        assert_eq!(cfg.window, id);
        assert_eq!((cfg.width, cfg.height), (320, 200), "echoed, with no manager to disagree");
        assert_eq!((cfg.x, cfg.y), (0, 0));
    }

    #[test]
    fn the_handshake_configure_is_not_delivered_as_an_event() {
        // A client acting on the first configure would be acting on a size it has committed
        // nothing at — it is the permission to draw, not an opinion about a drawing. Only a
        // *later* configure is news.
        let mut t = MockTransport::default();
        t.next_window = 1;
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(64, 32, Role::Normal), 2).expect("created");
        assert!(s.poll_event().expect("ok").is_none(), "the handshake leaked into the queue");

        // A second one is a manager changing its mind, and that *is* an event.
        let mut cfg = [0u8; 20];
        ConfigureEvent { window: id, width: 100, height: 50, x: 7, y: 9 }
            .write(&mut cfg)
            .unwrap();
        s.transport.events.push((OP_CONFIGURE, cfg.to_vec()));
        assert_eq!(
            s.poll_event().expect("ok"),
            Some((id, WindowEvent::Configure { width: 100, height: 50, x: 7, y: 9 })),
        );
        assert_eq!(s.window(id).unwrap().configured().unwrap().x, 7, "and it updates what the window knows");
    }

    #[test]
    fn a_configure_for_another_window_is_not_this_windows_business() {
        // One session can hold several windows and they share a channel, so the id is what
        // makes a configure attributable — the same reason `FocusEvent` carries one.
        let mut t = MockTransport::default();
        t.next_window = 3;
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).expect("created");
        let before = s.window(id).unwrap().configured().unwrap();

        let mut cfg = [0u8; 20];
        ConfigureEvent { window: id + 1, width: 999, height: 999, x: 1, y: 1 }
            .write(&mut cfg)
            .unwrap();
        s.transport.events.push((OP_CONFIGURE, cfg.to_vec()));
        assert!(s.poll_event().expect("ok").is_none(), "somebody else's configure was queued");
        assert_eq!(s.window(id).unwrap().configured().unwrap(), before, "and it did not overwrite ours");
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
        // The end-to-end shape: commit, then acquire the next buffer, for more frames than
        // there are buffers. This is what `ui-testclient` does in the guest.
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
        // If `busy` were set before the send, a failed commit would strand the buffer: the
        // compositor never saw it and will never release it. Two such failures on a
        // double-buffered window stall the client forever.
        let mut w = window(2);
        w.s.transport.fail = true;
        assert_eq!(w.commit(0, (0, 0, 1, 1)), Err(UiError::Transport));
        assert_eq!(w.next_free(), Some(0), "buffer 0 must still be drawable");
        assert!(!w.buffers().iter().find(|b| b.id == 0).unwrap().busy);

        // And a subsequent successful commit does mark it.
        w.s.transport.fail = false;
        w.commit(0, (0, 0, 1, 1)).unwrap();
        assert!(w.buffers().iter().find(|b| b.id == 0).unwrap().busy);
    }

    #[test]
    fn a_transport_failure_surfaces_rather_than_being_swallowed() {
        let mut t = MockTransport::default();
        t.fail = true;
        assert_eq!(
            Session::new(t).create(&CreateWindowRequest::new(8, 8, Role::Normal), 2).err(),
            Some(UiError::Transport)
        );
    }

    #[test]
    fn the_damage_rectangle_reaches_the_wire_unchanged() {
        let mut w = window(2);
        w.commit(0, (3, 5, 17, 9)).unwrap();
        let (_, body, _) =
            w.s.transport.sent.iter().rev().find(|(op, _, _)| *op == OP_COMMIT).unwrap();
        let req = librsproto::surface::parse_commit_request(body).unwrap();
        assert_eq!((req.damage_x, req.damage_y, req.damage_w, req.damage_h), (3, 5, 17, 9));
    }

    #[test]
    fn transport_level_loss_surfaces_as_a_dropped_marker() {
        // A transport that discarded a parked event must say so, or a client tracking held
        // keys carries a phantom modifier for the rest of its life. Folded into the same
        // flag the local queue sets, so both surface as one `Dropped`.
        let mut w = window(2);
        w.s.transport.lost = true;
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
            w.s.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
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
        w.s.transport.events.insert(0, (OP_FOCUS_EVENT, b[..n].to_vec()));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None, "not addressed to this window");
    }

    #[test]
    fn a_truncated_focus_event_is_ignored_rather_than_read_as_unfocused() {
        // Reading a short record as `focused: false` would dim a window for a malformed
        // message — the failure mode is invisible, because a dim window looks like a window
        // that legitimately lost focus.
        let mut w = window(2);
        w.s.transport.events.insert(0, (OP_FOCUS_EVENT, vec![0u8; 7]));
        w.pump().expect("pump");
        assert_eq!(w.next_event(), None);
    }

    /// Input for **another** window on the same connection is not delivered to this one.
    ///
    /// This is the whole reason `KeyEvent` and `PointerEvent` gained a window id (M6 C3). A
    /// session can hold several windows — a popup is created on its parent's connection — and
    /// until now these two records carried nothing to tell them apart, so every window on a
    /// connection received every keystroke and every click that arrived on it. With one window
    /// per connection that was invisible; with a menu open it means a click on the menu is also
    /// handed to the window underneath, and both act on it.
    #[test]
    fn input_for_another_window_on_the_same_session_is_not_delivered_here() {
        let mut w = window(2);
        let mine = w.id();
        let other = mine + 1;

        // Two records for a sibling window, then one for this one.
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

        assert_eq!(
            w.next_event(),
            Some(WindowEvent::Key(KeyEvent::new(w.id(), 30, 1, MOD_SHIFT)))
        );
        // Signed coordinates survive the round trip — an unsigned read here is the
        // "pointer teleports on a leftward drag" bug, one layer further out.
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
        // The ordinary case, not a corner: a client renders, commits, and blocks — which is
        // exactly when the user is looking at the result and typing into it.
        let mut w = window(2);
        let b = w.acquire().expect("a free buffer");
        w.commit(b, (0, 0, 64, 32)).expect("commit");
        let b2 = w.acquire().expect("the other buffer");
        w.commit(b2, (0, 0, 64, 32)).expect("commit");

        // Nothing free. The compositor sends a keystroke *and* the release during the block.
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
        assert!(
            !w.buffers().iter().find(|x| x.id == b).unwrap().busy,
            "the release was applied, not thrown away"
        );
    }

    #[test]
    fn an_overflowing_queue_drops_the_oldest_and_says_so() {
        let mut w = window(2);
        for i in 0..(EVENT_QUEUE_MAX as u16 + 3) {
            w.s.transport.queue_key(i, true, 0);
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
