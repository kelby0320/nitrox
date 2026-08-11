//! The `Surface` category (`op = 0x09xx`) bodies — windows and their pixels. See
//! `docs/spec/rsproto-surface-ops.md`.
//!
//! The compositor is a resource server bound at `/dev/draw` with a subtree base, the
//! same binding kind `/home` uses, so window paths are forwarded resolves and nobody
//! calls `sys_ns_bind` when a window opens
//! (`docs/design/ui-composition-model.md` §2a).
//!
//! **This category adds no kernel surface.** A client creates a `MemoryObject`, maps it,
//! draws, and transfers the handle **once** — not per frame. Thereafter it sends
//! [`Commit`](build_commit_request) naming a buffer and a damage rectangle, and the
//! compositor sends [`Release`](build_release_event) back when it is done reading. Every
//! syscall that needs is already there: `sys_memory_create`/`_map`, handle transfer on an
//! IPC message, and notifications (`display-substrate.md` §4).
//!
//! Bodies are little-endian and byte-serialised into a caller buffer, like every other
//! category.

use crate::{get_u16, get_u32, put_u16, put_u32};

// --- Operation numbers ------------------------------------------------------

/// `CreateWindow` — mint a window; the reply carries its id.
pub const OP_CREATE_WINDOW: u16 = 0x0900;
/// `AttachBuffer` — register a transferred `MemoryObject` as one of the window's buffers.
pub const OP_ATTACH_BUFFER: u16 = 0x0901;
/// `Commit` — this buffer, this damage, is ready to composite.
pub const OP_COMMIT: u16 = 0x0902;
/// `Release` — server → client: that buffer is free to draw into again.
pub const OP_RELEASE: u16 = 0x0903;
/// `DestroyWindow` — drop the window and everything attached to it.
pub const OP_DESTROY_WINDOW: u16 = 0x0904;

// --- Window roles -----------------------------------------------------------

/// `normal` — an ordinary application window.
pub const ROLE_NORMAL: u16 = 0;
/// `panel` — a bar. Docks to a screen edge, is visible on every desktop, and **never
/// takes keyboard focus**, so clicking the clock does not steal input from the terminal.
pub const ROLE_PANEL: u16 = 1;
/// `popup` — a menu or modal. Transient, parented, and may extend beyond its parent's
/// bounds (a menu clipped to its window is not a menu).
pub const ROLE_POPUP: u16 = 2;
/// `dialog` — parented, on its parent's desktop, listed but not offered as a wirable node
/// on the composition canvas.
pub const ROLE_DIALOG: u16 = 3;

/// Wire tag for the top edge.
pub const EDGE_TOP: u16 = 0;
/// Wire tag for the bottom edge.
pub const EDGE_BOTTOM: u16 = 1;
/// Wire tag for the left edge.
pub const EDGE_LEFT: u16 = 2;
/// Wire tag for the right edge.
pub const EDGE_RIGHT: u16 = 3;

/// A screen edge a panel can dock to.
///
/// An enum rather than a raw `u16` so an edge that does not exist is **unrepresentable**
/// rather than merely rejected by the parser. `Role`'s fields are public: with a raw tag,
/// `Role::Panel { dock: 9, .. }` is constructible in Rust, and then a compositor's `match`
/// needs a catch-all arm that silently reserves nothing while `strut()` still reports a
/// reservation — two components disagreeing about the same window. `libdraw` pushed the
/// depth check into `Geometry::with_pitch` for exactly this reason.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Top of the screen.
    Top,
    /// Bottom of the screen.
    Bottom,
    /// Left of the screen.
    Left,
    /// Right of the screen.
    Right,
}

impl Edge {
    /// The wire tag for this edge.
    pub const fn tag(self) -> u16 {
        match self {
            Edge::Top => EDGE_TOP,
            Edge::Bottom => EDGE_BOTTOM,
            Edge::Left => EDGE_LEFT,
            Edge::Right => EDGE_RIGHT,
        }
    }

    /// The edge a wire tag names, or `None` if it names none.
    pub const fn from_wire(tag: u16) -> Option<Edge> {
        match tag {
            EDGE_TOP => Some(Edge::Top),
            EDGE_BOTTOM => Some(Edge::Bottom),
            EDGE_LEFT => Some(Edge::Left),
            EDGE_RIGHT => Some(Edge::Right),
            _ => None,
        }
    }
}

/// Largest strut a panel may reserve, in pixels.
///
/// Bounded **at the protocol edge**, where an unbounded `u32` arrives straight off the
/// wire from a client. Two panels each reserving `0x8000_0000` overflow a `u32`
/// accumulator: in a debug build that is a client-triggered compositor panic, and in
/// release (how `xtask` builds userspace) it wraps to zero and silently returns the *full*
/// screen as the work area — defeating the clamp this protocol promises. The compositor
/// also saturates, but a value no display could ever need has no business being accepted.
pub const MAX_STRUT_RESERVE: u32 = 1 << 16;

/// A window's role, and the extra facts a role carries.
///
/// **Immutable after creation.** A role change would force the compositor to redo struts,
/// focus policy and stacking mid-flight; a client that wants a different role creates a
/// different window — which is what a menu or a dialog already is.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Role {
    /// An ordinary application window.
    Normal,
    /// A bar docked to `dock`, reserving `reserve` pixels along that edge.
    ///
    /// **The reservation is declared, not derived from geometry.** They differ in a case
    /// that matters: a fullscreen window covers a panel's pixels while the panel still
    /// reserves that space for *maximised* windows. Deriving would also make a
    /// partial-width bar, or one that reserves less than it occupies, inexpressible.
    Panel {
        /// Which screen edge the panel docks to.
        dock: Edge,
        /// Pixels reserved along that edge, subtracted from the area offered to
        /// `normal` windows.
        reserve: u32,
    },
    /// A menu or modal, parented to `parent`.
    Popup {
        /// Window id this popup belongs to.
        parent: u32,
    },
    /// A dialog, parented to `parent`.
    Dialog {
        /// Window id this dialog belongs to.
        parent: u32,
    },
}

impl Role {
    /// The wire tag for this role.
    pub const fn tag(&self) -> u16 {
        match self {
            Role::Normal => ROLE_NORMAL,
            Role::Panel { .. } => ROLE_PANEL,
            Role::Popup { .. } => ROLE_POPUP,
            Role::Dialog { .. } => ROLE_DIALOG,
        }
    }

    /// Whether a window in this role may take keyboard focus.
    ///
    /// A panel must not: clicking a clock or a window-list entry would otherwise steal
    /// input from whatever the user was typing into.
    pub const fn takes_focus(&self) -> bool {
        !matches!(self, Role::Panel { .. })
    }

    /// The edge and size this role reserves, if any.
    pub const fn strut(&self) -> Option<(Edge, u32)> {
        match self {
            Role::Panel { dock, reserve } => Some((*dock, *reserve)),
            _ => None,
        }
    }
}

// --- Window info ------------------------------------------------------------

/// What `/dev/draw/<N>/info` reports about a window.
///
/// Served as the bytes of a small read-only `MemoryObject`, the same shape
/// `/dev/framebuffer/info` uses — a resolve answers with an object the caller maps, not
/// with a message.
///
/// **Readable by anyone holding `/dev/draw`, deliberately.** A resolve arrives on the
/// forwarding endpoint with no connection identity attached, so the compositor could not
/// scope this per client even if it wanted to. That is the right answer rather than a
/// concession: the composition model expects a canvas or desktop shell to enumerate windows
/// and read their metadata, and `display-substrate.md` §4b gates *pixels* — not titles,
/// roles or geometry — as the thing that actually leaks.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WindowInfo {
    /// Compositor-assigned window id.
    pub id: u32,
    /// Committed width, or the requested width if nothing is committed yet.
    pub width: u32,
    /// Committed height, or the requested height.
    pub height: u32,
    /// Top-left corner in screen coordinates. Signed: a window may sit off an edge.
    pub x: i32,
    /// Top-left corner, y.
    pub y: i32,
    /// Role tag — see [`ROLE_NORMAL`] and friends.
    pub role: u16,
    /// A panel's dock edge; zero otherwise.
    pub dock: u16,
    /// A panel's reservation; zero otherwise.
    pub reserve: u32,
    /// A popup or dialog's parent window; zero otherwise.
    pub parent: u32,
}

const _: () = assert!(core::mem::size_of::<WindowInfo>() == 32);

impl WindowInfo {
    /// Build the info for a window with `role` at `(x, y)`, sized `width × height`.
    pub fn new(id: u32, role: Role, x: i32, y: i32, width: u32, height: u32) -> Self {
        let (dock, reserve, parent) = match role {
            Role::Normal => (0, 0, 0),
            Role::Panel { dock, reserve } => (dock.tag(), reserve, 0),
            Role::Popup { parent } | Role::Dialog { parent } => (0, 0, parent),
        };
        Self { id, width, height, x, y, role: role.tag(), dock, reserve, parent }
    }

    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 32 {
            return None;
        }
        put_u32(out, 0, self.id);
        put_u32(out, 4, self.width);
        put_u32(out, 8, self.height);
        put_u32(out, 12, self.x as u32);
        put_u32(out, 16, self.y as u32);
        put_u16(out, 20, self.role);
        put_u16(out, 22, self.dock);
        put_u32(out, 24, self.reserve);
        put_u32(out, 28, self.parent);
        Some(32)
    }

    /// Parse from the first 32 bytes of a mapped `info` object.
    ///
    /// Returns `None` if the slice is short: a truncated read would otherwise produce a
    /// plausible window with zeroed geometry.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 32 {
            return None;
        }
        Some(Self {
            id: get_u32(b, 0),
            width: get_u32(b, 4),
            height: get_u32(b, 8),
            x: get_u32(b, 12) as i32,
            y: get_u32(b, 16) as i32,
            role: get_u16(b, 20),
            dock: get_u16(b, 22),
            reserve: get_u32(b, 24),
            parent: get_u32(b, 28),
        })
    }
}

// --- CreateWindow -----------------------------------------------------------

/// Body length of a `CreateWindowRequest`.
pub const CREATE_WINDOW_REQUEST_LEN: usize = 16;
/// Body length of a `CreateWindowReply`.
pub const CREATE_WINDOW_REPLY_LEN: usize = 4;

/// A parsed `CreateWindowRequest`.
#[derive(Copy, Clone, Debug)]
pub struct CreateWindowRequest {
    /// Requested width in pixels.
    pub width: u32,
    /// Requested height in pixels.
    pub height: u32,
    /// The window's role, fixed for its lifetime.
    pub role: Role,
}

/// Write a `CreateWindowRequest` body; returns its length.
pub fn build_create_window_request(out: &mut [u8], req: &CreateWindowRequest) -> Option<usize> {
    if out.len() < CREATE_WINDOW_REQUEST_LEN {
        return None;
    }
    put_u32(out, 0, req.width);
    put_u32(out, 4, req.height);
    put_u16(out, 8, req.role.tag());
    // Role-specific words. Unused ones are written zero rather than left alone, so the
    // body never carries whatever the caller's buffer happened to hold.
    let (aux16, aux32) = match req.role {
        Role::Normal => (0, 0),
        Role::Panel { dock, reserve } => (dock.tag(), reserve),
        Role::Popup { parent } | Role::Dialog { parent } => (0, parent),
    };
    put_u16(out, 10, aux16);
    put_u32(out, 12, aux32);
    Some(CREATE_WINDOW_REQUEST_LEN)
}

/// Parse a `CreateWindowRequest` body.
pub fn parse_create_window_request(body: &[u8]) -> Option<CreateWindowRequest> {
    if body.len() < CREATE_WINDOW_REQUEST_LEN {
        return None;
    }
    let width = get_u32(body, 0);
    let height = get_u32(body, 4);
    let aux16 = get_u16(body, 10);
    let aux32 = get_u32(body, 12);
    let role = match get_u16(body, 8) {
        ROLE_NORMAL => Role::Normal,
        ROLE_PANEL => {
            let Some(dock) = Edge::from_wire(aux16) else { return None };
            if aux32 > MAX_STRUT_RESERVE {
                return None;
            }
            Role::Panel { dock, reserve: aux32 }
        }
        ROLE_POPUP => Role::Popup { parent: aux32 },
        ROLE_DIALOG => Role::Dialog { parent: aux32 },
        _ => return None,
    };
    Some(CreateWindowRequest { width, height, role })
}

/// Write a `CreateWindowReply` body (the new window's id).
pub fn build_create_window_reply(out: &mut [u8], window: u32) -> Option<usize> {
    if out.len() < CREATE_WINDOW_REPLY_LEN {
        return None;
    }
    put_u32(out, 0, window);
    Some(CREATE_WINDOW_REPLY_LEN)
}

/// Parse a `CreateWindowReply` body.
pub fn parse_create_window_reply(body: &[u8]) -> Option<u32> {
    if body.len() < CREATE_WINDOW_REPLY_LEN {
        return None;
    }
    Some(get_u32(body, 0))
}

// --- AttachBuffer -----------------------------------------------------------

/// Body length of an `AttachBufferRequest`.
pub const ATTACH_BUFFER_REQUEST_LEN: usize = 24;

/// A parsed `AttachBufferRequest`.
///
/// The `MemoryObject` handle itself rides on the message's handle-transfer slot, not in
/// the body — this describes how to interpret the memory it names.
#[derive(Copy, Clone, Debug)]
pub struct AttachBufferRequest {
    /// Window the buffer belongs to.
    pub window: u32,
    /// Client-chosen buffer id, unique within the window. The client picks it so it can
    /// name the buffer in a later `Commit` without waiting for a reply.
    pub buffer: u32,
    /// Buffer width in pixels.
    pub width: u32,
    /// Buffer height in pixels.
    pub height: u32,
    /// Bytes per row. Not `width * 4`: the client may pad rows.
    pub pitch: u32,
    /// Pixel format tag; see `docs/spec/rsproto-surface-ops.md`. Only `0` (XRGB8888) is
    /// accepted today, and an unknown value is rejected rather than assumed.
    pub format: u32,
}

/// Pixel format tag for `0x00RRGGBB`.
pub const SURFACE_FORMAT_XRGB8888: u32 = 0;

/// Write an `AttachBufferRequest` body.
pub fn build_attach_buffer_request(out: &mut [u8], req: &AttachBufferRequest) -> Option<usize> {
    if out.len() < ATTACH_BUFFER_REQUEST_LEN {
        return None;
    }
    put_u32(out, 0, req.window);
    put_u32(out, 4, req.buffer);
    put_u32(out, 8, req.width);
    put_u32(out, 12, req.height);
    put_u32(out, 16, req.pitch);
    put_u32(out, 20, req.format);
    Some(ATTACH_BUFFER_REQUEST_LEN)
}

/// Parse an `AttachBufferRequest` body.
///
/// Rejects a pitch too small to hold a row: accepting it would alias rows onto each other
/// in a buffer the *client* owns, which the compositor cannot detect any other way.
pub fn parse_attach_buffer_request(body: &[u8]) -> Option<AttachBufferRequest> {
    if body.len() < ATTACH_BUFFER_REQUEST_LEN {
        return None;
    }
    let req = AttachBufferRequest {
        window: get_u32(body, 0),
        buffer: get_u32(body, 4),
        width: get_u32(body, 8),
        height: get_u32(body, 12),
        pitch: get_u32(body, 16),
        format: get_u32(body, 20),
    };
    if req.format != SURFACE_FORMAT_XRGB8888 {
        return None;
    }
    if (req.pitch as u64) < req.width as u64 * 4 {
        return None;
    }
    Some(req)
}

// --- Commit / Release -------------------------------------------------------

/// Body length of a `CommitRequest`.
pub const COMMIT_REQUEST_LEN: usize = 24;
/// Body length of a `ReleaseEvent`.
pub const RELEASE_EVENT_LEN: usize = 8;

/// A parsed `CommitRequest`.
#[derive(Copy, Clone, Debug)]
pub struct CommitRequest {
    /// Window being committed.
    pub window: u32,
    /// Which attached buffer holds the new content.
    pub buffer: u32,
    /// Damage rectangle, in buffer coordinates.
    pub damage_x: u32,
    /// Damage rectangle origin, y.
    pub damage_y: u32,
    /// Damage width. Zero means "no damage" and is a valid no-op commit.
    pub damage_w: u32,
    /// Damage height.
    pub damage_h: u32,
}

/// Write a `CommitRequest` body.
pub fn build_commit_request(out: &mut [u8], req: &CommitRequest) -> Option<usize> {
    if out.len() < COMMIT_REQUEST_LEN {
        return None;
    }
    put_u32(out, 0, req.window);
    put_u32(out, 4, req.buffer);
    put_u32(out, 8, req.damage_x);
    put_u32(out, 12, req.damage_y);
    put_u32(out, 16, req.damage_w);
    put_u32(out, 20, req.damage_h);
    Some(COMMIT_REQUEST_LEN)
}

/// Parse a `CommitRequest` body.
pub fn parse_commit_request(body: &[u8]) -> Option<CommitRequest> {
    if body.len() < COMMIT_REQUEST_LEN {
        return None;
    }
    Some(CommitRequest {
        window: get_u32(body, 0),
        buffer: get_u32(body, 4),
        damage_x: get_u32(body, 8),
        damage_y: get_u32(body, 12),
        damage_w: get_u32(body, 16),
        damage_h: get_u32(body, 20),
    })
}

/// Write a `ReleaseEvent` body — server → client, "you may draw into this buffer again".
pub fn build_release_event(out: &mut [u8], window: u32, buffer: u32) -> Option<usize> {
    if out.len() < RELEASE_EVENT_LEN {
        return None;
    }
    put_u32(out, 0, window);
    put_u32(out, 4, buffer);
    Some(RELEASE_EVENT_LEN)
}

/// Parse a `ReleaseEvent` body.
pub fn parse_release_event(body: &[u8]) -> Option<(u32, u32)> {
    if body.len() < RELEASE_EVENT_LEN {
        return None;
    }
    Some((get_u32(body, 0), get_u32(body, 4)))
}

// --- DestroyWindow ----------------------------------------------------------

/// Body length of a `DestroyWindowRequest`.
pub const DESTROY_WINDOW_REQUEST_LEN: usize = 4;

/// Write a `DestroyWindowRequest` body.
pub fn build_destroy_window_request(out: &mut [u8], window: u32) -> Option<usize> {
    if out.len() < DESTROY_WINDOW_REQUEST_LEN {
        return None;
    }
    put_u32(out, 0, window);
    Some(DESTROY_WINDOW_REQUEST_LEN)
}

/// Parse a `DestroyWindowRequest` body.
pub fn parse_destroy_window_request(body: &[u8]) -> Option<u32> {
    if body.len() < DESTROY_WINDOW_REQUEST_LEN {
        return None;
    }
    Some(get_u32(body, 0))
}


// ---------------------------------------------------------------------------
// Input delivered to a window (M3 Part C).
//
// **Surface-layer events, not device records.** The device layer carries `InputEvent`
// triples with a `SYN` state machine; a client should never see one. `libinput` runs that
// machine on the compositor's side and hands a window something already usable — which is
// the same split `display-substrate.md` §5 makes for the keyboard, and the reason its
// `KeyEvent` carries modifiers the device layer has no field for.
// ---------------------------------------------------------------------------

/// `Surface::KeyEvent` — a key transition delivered to the focused window.
pub const OP_KEY_EVENT: u16 = 0x0905;
/// `Surface::PointerEvent` — pointer motion, a button, or a crossing.
pub const OP_POINTER_EVENT: u16 = 0x0906;
/// `Surface::FocusEvent` — this window gained or lost the keyboard.
pub const OP_FOCUS_EVENT: u16 = 0x0907;

/// The key was released.
pub const KEY_UP: u16 = 0;
/// The key was pressed.
pub const KEY_DOWN: u16 = 1;
/// The key is still held and the repeat interval elapsed.
///
/// Distinct from [`KEY_DOWN`] so that a consumer counting presses does not count a held key
/// forever — a shell's history stepping wants repeats, and a "press any key" prompt does not.
pub const KEY_REPEAT: u16 = 2;

/// A key transition, as a window sees it.
///
/// The shape `display-substrate.md` §5 fixed, and the reason the boundary is key events
/// rather than characters: **modifiers travel with the key**. A byte stream cannot express
/// Shift-Enter — `\n` is `\n` whatever was held — and a terminal that wants it needs the
/// modifier state at the instant the key went down, not whatever it is by the time a
/// character arrives.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KeyEvent {
    /// The keycode (an `EV_KEY` code from the device layer, unchanged).
    pub keycode: u16,
    /// [`KEY_UP`], [`KEY_DOWN`] or [`KEY_REPEAT`].
    ///
    /// **Non-zero means "the key is down"**, which is what a client that ignores repeat
    /// reads it as — so treating this as a boolean gives the behaviour most callers want
    /// without them knowing repeat exists. One that cares tells a repeat from a fresh press
    /// by the exact value, the same way the device layer's `InputEvent::value` does
    /// (`input-subsystem.md` §3, which reserved `2` for this).
    pub pressed: u16,
    /// Modifiers held **at this transition** — see the `MOD_*` constants.
    pub modifiers: u16,
    /// Reserved; zero.
    pub _pad: u16,
}

const _: () = assert!(core::mem::size_of::<KeyEvent>() == 8);

/// Modifier bits.
///
/// Carried by both [`KeyEvent::modifiers`] and [`PointerEvent::modifiers`].
///
/// Left and right share a bit, as X11's `ShiftMask` and Wayland's xkb mask also do: a client
/// asking "was shift held" should not have to ask twice. What stays distinct is the
/// **keycode** — `KEY_LEFTSHIFT` and `KEY_RIGHTSHIFT` arrive unchanged — so a consumer that
/// genuinely needs the side reads that, and adding `MOD_*_R` bits later is additive.
///
/// A sender must derive these from *which modifier keys are down*, not by clearing the bit on
/// each release: with both shifts held, releasing one has to leave `MOD_SHIFT` set. That is a
/// tracking obligation, not a layout one — see `libinput`'s `Interpreter::held_mods`.
pub const MOD_SHIFT: u16 = 1 << 0;
/// Either control key.
pub const MOD_CTRL: u16 = 1 << 1;
/// Either alt key.
pub const MOD_ALT: u16 = 1 << 2;
/// Either meta/"super" key.
pub const MOD_META: u16 = 1 << 3;

/// What a [`PointerEvent`] reports.
pub const POINTER_MOTION: u16 = 0;
/// A button went down or came up; `button` and `flags` say which and which way.
pub const POINTER_BUTTON: u16 = 1;
/// The pointer entered this window.
pub const POINTER_ENTER: u16 = 2;
/// The pointer left this window.
pub const POINTER_LEAVE: u16 = 3;

/// `POINTER_BUTTON` flag: the button went down. Absent means it came up.
pub const POINTER_PRESSED: u16 = 1 << 0;

/// A pointer event, as a window sees it.
///
/// **Coordinates are window-local**, so a client can use them without knowing where it sits
/// on screen, and they stay correct when the window moves — which a client is not told about
/// and should not have to be.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PointerEvent {
    /// `POINTER_MOTION`, `POINTER_BUTTON`, `POINTER_ENTER` or `POINTER_LEAVE`.
    pub kind: u16,
    /// The button for `POINTER_BUTTON` (a `BTN_*` code), otherwise zero.
    pub button: u16,
    /// Every button currently held, as `BTN_LEFT`→bit 0, `BTN_RIGHT`→bit 1, `BTN_MIDDLE`→bit 2.
    ///
    /// Meaningful on **every** kind, not only `POINTER_BUTTON` — a drag is motion with a
    /// button held, and a client that has to re-derive this from the button events is doing
    /// the accumulation this layer exists to do once.
    pub buttons: u16,
    /// `POINTER_PRESSED` on a press; zero on a release.
    pub flags: u16,
    /// Modifiers held — `MOD_SHIFT` and friends — on every kind.
    ///
    /// Here for the same reason they ride [`KeyEvent`]: shift-click and shift-drag are not
    /// expressible otherwise. A client that instead tracked shift from `KeyEvent`s would get
    /// it right only while it also held keyboard focus, so shift-clicking an unfocused
    /// window would silently behave as a plain click (PR #180 review, finding 3).
    pub modifiers: u16,
    /// Reserved; zero.
    pub _pad: u16,
    /// Window-local x.
    pub x: i32,
    /// Window-local y.
    pub y: i32,
}

const _: () = assert!(core::mem::size_of::<PointerEvent>() == 20);

/// This window gained or lost the keyboard.
///
/// **A toolkit needs this and cannot derive it.** A caret blinks only when *both* the widget
/// has focus within its window and the window has the keyboard; those are two facts from two
/// sources, and a client with only the first would keep blinking behind another window.
///
/// Sent when the answer changes, not on every event that could have changed it — the
/// compositor compares against what it last told each window, so a raise that does not move
/// focus sends nothing.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FocusEvent {
    /// Non-zero if this window now has the keyboard.
    pub focused: u16,
    /// Reserved; zero.
    pub _pad: u16,
    /// Which window this is about.
    ///
    /// **Focus is per-window state, and one session can hold several windows** — a menu
    /// popup is created on its parent's connection and takes focus from it, so both halves
    /// of that change arrive on one channel. Without an id a client cannot attribute them,
    /// and per-window focus state is exactly what a toolkit keeps.
    ///
    /// `KeyEvent` and `PointerEvent` have the same shortcoming and are not fixed here: they
    /// are shipped, and widening them is a wire break. This record was one commit old when
    /// the gap was found, so it cost two bytes that were already reserved
    /// (PR #184 re-review, finding 3). The others are filed.
    pub window: u32,
}

const _: () = assert!(core::mem::size_of::<FocusEvent>() == 8);

impl FocusEvent {
    /// Serialise into exactly 8 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        out[0..2].copy_from_slice(&self.focused.to_le_bytes());
        out[2..4].copy_from_slice(&0u16.to_le_bytes());
        out[4..8].copy_from_slice(&self.window.to_le_bytes());
        Some(8)
    }

    /// Parse from exactly 8 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            focused: u16::from_le_bytes([b[0], b[1]]),
            _pad: 0,
            window: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}

impl KeyEvent {
    /// Serialise into exactly 8 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        out[0..2].copy_from_slice(&self.keycode.to_le_bytes());
        out[2..4].copy_from_slice(&self.pressed.to_le_bytes());
        out[4..6].copy_from_slice(&self.modifiers.to_le_bytes());
        out[6..8].copy_from_slice(&0u16.to_le_bytes());
        Some(8)
    }

    /// Parse from exactly 8 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            keycode: u16::from_le_bytes([b[0], b[1]]),
            pressed: u16::from_le_bytes([b[2], b[3]]),
            modifiers: u16::from_le_bytes([b[4], b[5]]),
            _pad: 0,
        })
    }
}

impl PointerEvent {
    /// Serialise into exactly 20 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 20 {
            return None;
        }
        out[0..2].copy_from_slice(&self.kind.to_le_bytes());
        out[2..4].copy_from_slice(&self.button.to_le_bytes());
        out[4..6].copy_from_slice(&self.buttons.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..10].copy_from_slice(&self.modifiers.to_le_bytes());
        out[10..12].copy_from_slice(&0u16.to_le_bytes());
        out[12..16].copy_from_slice(&self.x.to_le_bytes());
        out[16..20].copy_from_slice(&self.y.to_le_bytes());
        Some(20)
    }

    /// Parse from exactly 20 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 20 {
            return None;
        }
        Some(Self {
            kind: u16::from_le_bytes([b[0], b[1]]),
            button: u16::from_le_bytes([b[2], b[3]]),
            buttons: u16::from_le_bytes([b[4], b[5]]),
            flags: u16::from_le_bytes([b[6], b[7]]),
            modifiers: u16::from_le_bytes([b[8], b[9]]),
            _pad: 0,
            x: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            y: i32::from_le_bytes([b[16], b[17], b[18], b[19]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_round_trips_with_its_extra_fields() {
        let roles = [
            Role::Normal,
            Role::Panel { dock: Edge::Top, reserve: 32 },
            Role::Panel { dock: Edge::Bottom, reserve: 28 },
            Role::Popup { parent: 7 },
            Role::Dialog { parent: 12 },
        ];
        let mut buf = [0u8; 64];
        for role in roles {
            let req = CreateWindowRequest { width: 800, height: 600, role };
            let n = build_create_window_request(&mut buf, &req).unwrap();
            assert_eq!(n, CREATE_WINDOW_REQUEST_LEN);
            let got = parse_create_window_request(&buf[..n]).unwrap();
            assert_eq!(got.width, 800);
            assert_eq!(got.height, 600);
            assert_eq!(got.role, role, "role did not survive the round trip");
        }
    }

    #[test]
    fn a_panel_reports_its_strut_and_refuses_focus() {
        let p = Role::Panel { dock: Edge::Top, reserve: 32 };
        assert_eq!(p.strut(), Some((Edge::Top, 32)));
        assert!(!p.takes_focus(), "a panel must never take keyboard focus");
        // Everything else focuses and reserves nothing.
        for r in [Role::Normal, Role::Popup { parent: 1 }, Role::Dialog { parent: 1 }] {
            assert_eq!(r.strut(), None);
            assert!(r.takes_focus());
        }
    }

    #[test]
    fn an_unknown_role_tag_is_rejected_rather_than_defaulted() {
        let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
        put_u32(&mut buf, 0, 100);
        put_u32(&mut buf, 4, 100);
        put_u16(&mut buf, 8, 99); // not a role
        assert!(parse_create_window_request(&buf).is_none());
    }

    #[test]
    fn a_panel_docked_to_a_nonexistent_edge_is_rejected() {
        let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
        put_u32(&mut buf, 0, 100);
        put_u32(&mut buf, 4, 32);
        put_u16(&mut buf, 8, ROLE_PANEL);
        put_u16(&mut buf, 10, 9); // no such edge
        put_u32(&mut buf, 12, 32);
        assert!(parse_create_window_request(&buf).is_none());
    }

    #[test]
    fn a_strut_larger_than_any_display_is_rejected() {
        // Unbounded on the wire, this overflows the compositor's accumulator: a panic in
        // debug, and in release a wrap to zero that returns the *full* screen as the work
        // area — silently defeating the clamp the spec promises.
        let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
        put_u32(&mut buf, 0, 100);
        put_u32(&mut buf, 4, 32);
        put_u16(&mut buf, 8, ROLE_PANEL);
        put_u16(&mut buf, 10, EDGE_TOP);
        put_u32(&mut buf, 12, 0x8000_0000);
        assert!(parse_create_window_request(&buf).is_none(), "an absurd reserve must be refused");

        put_u32(&mut buf, 12, MAX_STRUT_RESERVE);
        assert!(parse_create_window_request(&buf).is_some(), "the bound itself is allowed");
        put_u32(&mut buf, 12, MAX_STRUT_RESERVE + 1);
        assert!(parse_create_window_request(&buf).is_none());
    }

    #[test]
    fn edge_tags_round_trip_and_unknown_tags_have_no_edge() {
        for e in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
            assert_eq!(Edge::from_wire(e.tag()), Some(e));
        }
        assert_eq!(Edge::from_wire(4), None);
        assert_eq!(Edge::from_wire(u16::MAX), None);
    }

    #[test]
    fn unused_role_words_are_written_zero() {
        // A `Normal` window must not leak whatever the caller's buffer held, or two
        // otherwise-identical requests would differ on the wire.
        let mut buf = [0xAAu8; CREATE_WINDOW_REQUEST_LEN];
        let req = CreateWindowRequest { width: 4, height: 4, role: Role::Normal };
        build_create_window_request(&mut buf, &req).unwrap();
        assert_eq!(&buf[10..16], &[0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn attach_rejects_a_pitch_that_would_alias_rows() {
        let mut buf = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        let mut req = AttachBufferRequest {
            window: 1,
            buffer: 0,
            width: 64,
            height: 32,
            pitch: 64 * 4,
            format: SURFACE_FORMAT_XRGB8888,
        };
        build_attach_buffer_request(&mut buf, &req).unwrap();
        assert!(parse_attach_buffer_request(&buf).is_some(), "an exact pitch is fine");

        req.pitch = 64 * 4 - 1;
        build_attach_buffer_request(&mut buf, &req).unwrap();
        assert!(parse_attach_buffer_request(&buf).is_none(), "a short pitch must be refused");

        req.pitch = 64 * 4 + 16;
        build_attach_buffer_request(&mut buf, &req).unwrap();
        assert!(parse_attach_buffer_request(&buf).is_some(), "padding is legitimate");
    }

    #[test]
    fn attach_rejects_an_unknown_pixel_format() {
        let mut buf = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        let req = AttachBufferRequest {
            window: 1,
            buffer: 0,
            width: 8,
            height: 8,
            pitch: 32,
            format: 7,
        };
        build_attach_buffer_request(&mut buf, &req).unwrap();
        assert!(parse_attach_buffer_request(&buf).is_none());
    }


    #[test]
    fn a_key_event_sits_at_the_offsets_the_spec_publishes() {
        let e = KeyEvent { keycode: 0x1112, pressed: 1, modifiers: 0x3132, _pad: 0 };
        let mut b = [0u8; 8];
        e.write(&mut b).unwrap();
        assert_eq!(&b[0..2], &0x1112u16.to_le_bytes(), "keycode @0");
        assert_eq!(&b[2..4], &1u16.to_le_bytes(), "pressed @2");
        assert_eq!(&b[4..6], &0x3132u16.to_le_bytes(), "modifiers @4");
        assert_eq!(KeyEvent::read(&b), Some(e));
    }

    #[test]
    fn a_pointer_event_sits_at_the_offsets_the_spec_publishes() {
        // Signed coordinates: a pointer can be dragged past a window's left or top edge,
        // and a client that read them unsigned would see it teleport.
        let e = PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x1112,
            buttons: 0x2122,
            flags: POINTER_PRESSED,
            modifiers: MOD_SHIFT | MOD_CTRL,
            _pad: 0,
            x: -3,
            y: -4,
        };
        let mut b = [0u8; 20];
        e.write(&mut b).unwrap();
        assert_eq!(&b[0..2], &POINTER_BUTTON.to_le_bytes(), "kind @0");
        assert_eq!(&b[2..4], &0x1112u16.to_le_bytes(), "button @2");
        assert_eq!(&b[4..6], &0x2122u16.to_le_bytes(), "buttons @4");
        assert_eq!(&b[6..8], &POINTER_PRESSED.to_le_bytes(), "flags @6");
        assert_eq!(&b[8..10], &(MOD_SHIFT | MOD_CTRL).to_le_bytes(), "modifiers @8");
        assert_eq!(&b[10..12], &0u16.to_le_bytes(), "reserved @10, zero");
        assert_eq!(&b[12..16], &(-3i32).to_le_bytes(), "x @12, signed");
        assert_eq!(&b[16..20], &(-4i32).to_le_bytes(), "y @16, signed");
        assert_eq!(PointerEvent::read(&b), Some(e));
    }

    #[test]
    fn a_focus_event_round_trips_and_refuses_a_short_buffer() {
        let e = FocusEvent { focused: 1, _pad: 0, window: 0x0102_0304 };
        let mut b = [0u8; 8];
        assert_eq!(e.write(&mut b), Some(8));
        assert_eq!(&b[0..2], &1u16.to_le_bytes(), "focused @0");
        assert_eq!(&b[2..4], &0u16.to_le_bytes(), "reserved @2, zero");
        assert_eq!(&b[4..8], &0x0102_0304u32.to_le_bytes(), "window @4");
        assert_eq!(FocusEvent::read(&b), Some(e));

        assert_eq!(FocusEvent::read(&[0u8; 7]), None);
        assert_eq!(FocusEvent::default().write(&mut [0u8; 7]), None);

        // A non-zero `focused` other than 1 still reads as focused: the field is a boolean
        // on the wire, and a sender writing 2 must not read back as "not focused".
        let e = FocusEvent { focused: 2, _pad: 0, window: 9 };
        let mut b = [0u8; 8];
        e.write(&mut b).unwrap();
        assert_ne!(FocusEvent::read(&b).unwrap().focused, 0);
    }

    #[test]
    fn a_truncated_input_event_is_refused_rather_than_read_short() {
        assert_eq!(KeyEvent::read(&[0u8; 7]), None);
        assert_eq!(PointerEvent::read(&[0u8; 19]), None);
        assert_eq!(KeyEvent::default().write(&mut [0u8; 7]), None);
        assert_eq!(PointerEvent::default().write(&mut [0u8; 19]), None);
    }

    #[test]
    fn the_new_ops_do_not_collide_with_the_existing_surface_ops() {
        for op in [OP_CREATE_WINDOW, OP_ATTACH_BUFFER, OP_COMMIT, OP_RELEASE, OP_DESTROY_WINDOW] {
            assert_ne!(op, OP_KEY_EVENT);
            assert_ne!(op, OP_POINTER_EVENT);
            assert_ne!(op, OP_FOCUS_EVENT);
        }
        assert_ne!(OP_KEY_EVENT, OP_POINTER_EVENT);
        assert_ne!(OP_KEY_EVENT, OP_FOCUS_EVENT);
        assert_ne!(OP_POINTER_EVENT, OP_FOCUS_EVENT);
    }

    #[test]
    fn window_info_sits_at_the_offsets_the_spec_publishes() {
        // A round trip cannot see a swapped pair — `write` and `read` agree with each other
        // whatever they agree on. `docs/spec/rsproto-surface-ops.md` publishes an exact
        // byte table as a contract for other implementations, so the bytes are pinned here
        // rather than only their symmetry (PR #175 review, finding 7).
        let info = WindowInfo {
            id: 0x1112_1314,
            width: 0x2122_2324,
            height: 0x3132_3334,
            x: -2,
            y: -3,
            role: 0x5152,
            dock: 0x6162,
            reserve: 0x7172_7374,
            parent: 0x8182_8384,
        };
        let mut b = [0u8; 32];
        info.write(&mut b).unwrap();
        assert_eq!(&b[0..4], &0x1112_1314u32.to_le_bytes(), "id @0");
        assert_eq!(&b[4..8], &0x2122_2324u32.to_le_bytes(), "width @4");
        assert_eq!(&b[8..12], &0x3132_3334u32.to_le_bytes(), "height @8");
        assert_eq!(&b[12..16], &(-2i32).to_le_bytes(), "x @12, signed");
        assert_eq!(&b[16..20], &(-3i32).to_le_bytes(), "y @16, signed");
        assert_eq!(&b[20..22], &0x5152u16.to_le_bytes(), "role @20");
        assert_eq!(&b[22..24], &0x6162u16.to_le_bytes(), "dock @22");
        assert_eq!(&b[24..28], &0x7172_7374u32.to_le_bytes(), "reserve @24");
        assert_eq!(&b[28..32], &0x8182_8384u32.to_le_bytes(), "parent @28");
    }

    #[test]
    fn window_info_round_trips_every_role() {
        let cases = [
            (Role::Normal, 0u16, 0u32, 0u32),
            (Role::Panel { dock: Edge::Bottom, reserve: 28 }, EDGE_BOTTOM, 28, 0),
            (Role::Popup { parent: 4 }, 0, 0, 4),
            (Role::Dialog { parent: 9 }, 0, 0, 9),
        ];
        let mut buf = [0u8; 48];
        for (role, dock, reserve, parent) in cases {
            let info = WindowInfo::new(7, role, -3, 12, 640, 480);
            let n = info.write(&mut buf).unwrap();
            assert_eq!(n, 32);
            let got = WindowInfo::read(&buf[..n]).unwrap();
            assert_eq!(got, info);
            assert_eq!(got.id, 7);
            assert_eq!((got.x, got.y), (-3, 12), "a negative origin must survive");
            assert_eq!((got.width, got.height), (640, 480));
            assert_eq!(got.role, role.tag());
            assert_eq!((got.dock, got.reserve, got.parent), (dock, reserve, parent));
        }
    }

    #[test]
    fn a_truncated_info_is_refused_rather_than_read_short() {
        let mut buf = [0u8; 32];
        WindowInfo::new(1, Role::Normal, 0, 0, 8, 8).write(&mut buf).unwrap();
        for short in 0..32 {
            assert!(WindowInfo::read(&buf[..short]).is_none(), "len {short}");
        }
        assert!(WindowInfo::read(&buf).is_some());
    }

    #[test]
    fn commit_and_release_round_trip() {
        let mut buf = [0u8; 32];
        let c = CommitRequest {
            window: 3,
            buffer: 1,
            damage_x: 4,
            damage_y: 5,
            damage_w: 16,
            damage_h: 9,
        };
        let n = build_commit_request(&mut buf, &c).unwrap();
        let got = parse_commit_request(&buf[..n]).unwrap();
        assert_eq!((got.window, got.buffer), (3, 1));
        assert_eq!((got.damage_x, got.damage_y, got.damage_w, got.damage_h), (4, 5, 16, 9));

        let n = build_release_event(&mut buf, 3, 1).unwrap();
        assert_eq!(parse_release_event(&buf[..n]).unwrap(), (3, 1));
    }

    #[test]
    fn a_truncated_body_is_rejected_rather_than_read_short() {
        let mut buf = [0u8; 64];
        let req = CreateWindowRequest { width: 1, height: 1, role: Role::Normal };
        let n = build_create_window_request(&mut buf, &req).unwrap();
        for short in 0..n {
            assert!(parse_create_window_request(&buf[..short]).is_none(), "len {short}");
        }
        let c = CommitRequest {
            window: 1,
            buffer: 0,
            damage_x: 0,
            damage_y: 0,
            damage_w: 1,
            damage_h: 1,
        };
        let n = build_commit_request(&mut buf, &c).unwrap();
        for short in 0..n {
            assert!(parse_commit_request(&buf[..short]).is_none(), "len {short}");
        }
    }

    #[test]
    fn building_into_a_short_buffer_fails_rather_than_truncating() {
        let mut small = [0u8; 4];
        let req = CreateWindowRequest { width: 1, height: 1, role: Role::Normal };
        assert!(build_create_window_request(&mut small, &req).is_none());
    }
}
