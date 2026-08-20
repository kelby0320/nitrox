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
///
/// Grew from 16 to 24 in M6 C1, when popups gained an offset. Named rather than written out
/// at each call site: every buffer in the tree is sized from this, so the record can grow
/// without a search for stale literals.
pub const CREATE_WINDOW_REQUEST_LEN: usize = 24;
/// Body length of a `CreateWindowReply`.
pub const CREATE_WINDOW_REPLY_LEN: usize = 4;

// **The record's length is a contract**, published in `docs/spec/rsproto-surface-ops.md` and
// read by a second implementation. Nothing pinned it before C1 — `ConfigureEvent` and
// `FocusEvent` both carry asserts and this, the record every client sends first, did not — so
// the spec and the encoder could drift apart silently. The offsets below are what the spec
// table names.
const _: () = assert!(CREATE_WINDOW_REQUEST_LEN == 24);
const _: () = assert!(CREATE_WINDOW_REPLY_LEN == 4);

/// A parsed `CreateWindowRequest`.
#[derive(Copy, Clone, Debug)]
pub struct CreateWindowRequest {
    /// Requested width in pixels.
    pub width: u32,
    /// Requested height in pixels.
    pub height: u32,
    /// The window's role, fixed for its lifetime.
    pub role: Role,
    /// Offset from the parent's origin, for the roles that have a parent.
    ///
    /// **Role-specific, like the aux words**, and zero for `normal` and `panel` — a window with
    /// no parent has nothing to be offset from, and a manager places it. For `popup` and
    /// `dialog` this is the whole of their placement: they are positioned by their *creator*,
    /// which is the only party that knows where the menu item it drops from was drawn.
    ///
    /// **Carried here rather than sent afterwards** so that a popup's position is atomic with
    /// its existence. A separate op between `CreateWindow` and the first `Commit` would put a
    /// second message on the path of every menu open, and would create the window at `(0, 0)`
    /// before moving it — never *seen* there, since nothing is composited before its first
    /// commit, but briefly wrong rather than never wrong, and a spurious `WindowGeometry` for
    /// the manager.
    ///
    /// **Resolved once, at creation.** The compositor stores absolute origins; a popup that
    /// tracked its parent would have to be re-placed whenever the parent moved, which is
    /// placement policy and belongs with the shell — see `TODO(popup-follows-parent)`.
    pub offset_x: i32,
    /// Vertical half of [`offset_x`](Self::offset_x).
    pub offset_y: i32,
}

impl CreateWindowRequest {
    /// A window with no offset — the only meaningful shape for a role that has no parent.
    ///
    /// **Constructors rather than struct literals**, so the record can gain a role-specific
    /// word without touching every caller in the tree. Adding the offset in C1 broke 66 literal
    /// sites; the next field breaks none.
    pub const fn new(width: u32, height: u32, role: Role) -> Self {
        Self { width, height, role, offset_x: 0, offset_y: 0 }
    }

    /// A popup or dialog at `(x, y)` from its parent's origin.
    ///
    /// The offset is ignored — written and read as zero — for a role with no parent, so this
    /// says what it means rather than being the general constructor.
    pub const fn at(width: u32, height: u32, role: Role, x: i32, y: i32) -> Self {
        Self { width, height, role, offset_x: x, offset_y: y }
    }
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
    // Zero for a role with no parent, for the same reason the aux words are: two identical
    // requests must produce identical bytes.
    let (ox, oy) = match req.role {
        Role::Popup { .. } | Role::Dialog { .. } => (req.offset_x, req.offset_y),
        Role::Normal | Role::Panel { .. } => (0, 0),
    };
    put_u32(out, 16, ox as u32);
    put_u32(out, 20, oy as u32);
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
    // Only meaningful for the roles that have a parent; a `normal` or `panel` request carries
    // zero here, and reading it anyway would invent an offset nobody sent.
    let (offset_x, offset_y) = match role {
        Role::Popup { .. } | Role::Dialog { .. } => {
            (get_u32(body, 16) as i32, get_u32(body, 20) as i32)
        }
        Role::Normal | Role::Panel { .. } => (0, 0),
    };
    Some(CreateWindowRequest { width, height, role, offset_x, offset_y })
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
/// `Surface::Configure` — where and how large the compositor would like this window to be.
///
/// **Server → client, unsolicited, no reply.** A *request*, not a command: the compositor cannot
/// resize a client's buffer, because the client allocates it. A client answers by attaching and
/// committing a buffer of that size, or declines by committing whatever it likes — and declining
/// stays legal, because a fixed-size window is an ordinary thing and a protocol that required
/// compliance would make every client implement reflow before it could exist.
///
/// **The first one is different, and it is an ordering rule rather than a stronger request.** A
/// window is not composited until it has been configured, so a client waits for its first
/// `Configure` before its first `Commit`. That is what lets a manager place a window *before* it
/// is ever seen, without putting the manager on the critical path of window creation: the round
/// trip belongs to the client. See `docs/spec/rsproto-surface-ops.md`.
pub const OP_CONFIGURE: u16 = 0x0908;

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

// --- The manager channel ----------------------------------------------------
//
// **A third channel role in this category.** A client holds a *session* and speaks the ops
// above about windows it created; a **manager** holds one channel for the whole compositor and
// speaks the ops below about *any* window. The ops live here rather than in a category of their
// own because they are about windows, and splitting them would put the two halves of one
// subsystem in two files — the same call `Tty` made for its backend ops.
//
// **The capability is the binding, not a check.** None of these verifies ownership; that is the
// point of a manager. `/dev/draw/manage` is what bounds who may hold one, and in Milestone 6
// that binding gates nothing — see `TODO(manage-ungated)` and
// `docs/design/graphical-session.md` §3.
// ---------------------------------------------------------------------------

/// `Manage::Place` — put a window's top-left corner at an absolute screen position.
///
/// Absolute rather than relative: a manager computes positions from the work area and from other
/// windows, so it always knows the answer in screen coordinates. A relative `Move` would serve
/// only an interactive drag, which needs a grab offset the compositor does not keep.
pub const OP_MGR_PLACE: u16 = 0x0910;
/// `Manage::Raise` — to the top of the stack.
pub const OP_MGR_RAISE: u16 = 0x0911;
/// `Manage::Lower` — to the bottom of the stack.
pub const OP_MGR_LOWER: u16 = 0x0912;
/// `Manage::RaiseAbove` — directly above another window, which is what alt-tab needs: a full
/// raise reorders everything in between, and the user sees the rest of the stack shuffle.
pub const OP_MGR_RAISE_ABOVE: u16 = 0x0913;
/// `Manage::SetFocus` — give a window the keyboard.
pub const OP_MGR_SET_FOCUS: u16 = 0x0914;
/// `Manage::Configure` — ask a window to be a given size and position.
///
/// The manager's half of the [`Configure`](OP_CONFIGURE) a client receives. Sent in answer to a
/// [`WindowCreated`](OP_MGR_WINDOW_CREATED) it is what releases the client from the
/// initial-configure handshake; sent later it is an ordinary request the client may decline.
pub const OP_MGR_CONFIGURE: u16 = 0x0915;

/// `Manage::WindowCreated` — server → manager, when a window is created.
///
/// **Carries the role and the requested geometry, not just an id.** A manager cannot place from
/// an id alone: a `panel` is not placed like a `normal`, a `popup` is placed by its own client,
/// and centring needs a size. All of it is already in the create request, so an event that made
/// the manager ask a follow-up question would be a seam with a round trip in it.
///
/// Body: [`MgrWindowCreated`], 20 bytes.
pub const OP_MGR_WINDOW_CREATED: u16 = 0x0918;
/// `Manage::WindowDestroyed` — server → manager, when a window goes away.
///
/// Body: [`MgrWindowRef`], 8 bytes; `other` is unused and sent as 0. Reusing the request shape
/// rather than minting a 4-byte one keeps the manager channel to three body layouts, and the
/// request side already spends `other` the same way on `Raise`/`Lower`/`SetFocus`.
pub const OP_MGR_WINDOW_DESTROYED: u16 = 0x0919;
/// `Manage::WindowGeometry` — server → manager: this window's position or size changed.
///
/// Sent whenever a window's bounds change for any reason, **including the manager's own
/// `Place`** — a manager that had to remember which changes were its own would be keeping a
/// second copy of the stack. A window list that learned about size changes by polling is the
/// thing this exists to avoid.
///
/// **"Any reason" includes a commit, and the size is the committed one.** A window's bounds
/// are its *committed* buffer's rectangle, which is what `/dev/draw/<id>/info` reports and
/// what is actually on screen — not the size the client named at `CreateWindow`. So a client
/// that reflows and commits a different-sized buffer has changed its bounds with no manager
/// involved, and is reported. Sending the requested size here instead would put the manager
/// and a namespace read in disagreement about one window at one instant (PR #217 review,
/// findings 1 and 2).
///
/// Body: [`ConfigureEvent`], 20 bytes — the same layout the client-facing `Configure` uses,
/// because it says the same thing about the same window.
pub const OP_MGR_WINDOW_GEOMETRY: u16 = 0x091A;
/// `Manage::WindowFocus` — server → manager: the keyboard moved.
///
/// Body: [`FocusEvent`], 8 bytes — the same layout the client-facing `FocusEvent` uses.
pub const OP_MGR_WINDOW_FOCUS: u16 = 0x091B;
/// `Manage::WindowTitle` — server → manager: this window's title changed.
///
/// **Reserved: encoding defined, not implemented — `TODO(m6-b3b-titles)`.** It and
/// [`OP_SET_TITLE`] are the title half of Part B3, which needs a client-facing op and a
/// variable-length body; the four lifecycle events above are built. Declared so the title
/// work does not renumber.
pub const OP_MGR_WINDOW_TITLE: u16 = 0x091C;

/// `Surface::SetTitle` — a client naming its own window. **Reserved: not implemented.** No
/// client sends it and the compositor does not answer it. Would be client → server, silent on
/// success.
///
/// Body: the window id, then UTF-8 bytes, up to [`MAX_TITLE`]. A title is the one piece of a
/// window a *manager* needs and only the *client* knows.
pub const OP_SET_TITLE: u16 = 0x0909;

/// The longest window title accepted, in bytes. **Reserved** with [`OP_SET_TITLE`].
///
/// Bounded at the protocol edge for the reason [`MAX_STRUT_RESERVE`] is: it arrives off the wire
/// from a client, it is stored per window for the compositor's life, and a manager forwarding it
/// has to fit it in a message. Long enough for any sentence anyone puts in a title bar.
pub const MAX_TITLE: usize = 256;

/// A manager request naming one window and a point — [`Place`](OP_MGR_PLACE).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrPlace {
    /// Which window.
    pub window: u32,
    /// Top-left corner in screen coordinates.
    pub x: i32,
    /// Top-left corner in screen coordinates.
    pub y: i32,
}

impl MgrPlace {
    /// Serialise into exactly 12 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 12 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..8].copy_from_slice(&self.x.to_le_bytes());
        out[8..12].copy_from_slice(&self.y.to_le_bytes());
        Some(12)
    }

    /// Parse from exactly 12 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 12 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            x: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            y: i32::from_le_bytes([b[8], b[9], b[10], b[11]]),
        })
    }
}

/// A manager request naming one window, and optionally a second — `Raise`, `Lower`, `SetFocus`,
/// `RaiseAbove`.
///
/// One body for four ops because they differ only in what the compositor does, not in what they
/// carry: `other` is the reference window for [`RaiseAbove`](OP_MGR_RAISE_ABOVE) and **zero** for
/// the rest, which is not a valid window id (ids start at 1).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrWindowRef {
    /// The window the request is about.
    pub window: u32,
    /// The reference window for `RaiseAbove`; zero otherwise.
    pub other: u32,
}

const _: () = assert!(core::mem::size_of::<MgrWindowRef>() == 8);

impl MgrWindowRef {
    /// Serialise into exactly 8 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..8].copy_from_slice(&self.other.to_le_bytes());
        Some(8)
    }

    /// Parse from exactly 8 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            other: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}

impl MgrWindowCreated {
    /// Build the event for a window of `role` with the geometry it asked for.
    ///
    /// The `role`/`aux16`/`aux32` split is the **same encoding `CreateWindowRequest` uses** —
    /// tag, then the role's extra field: a panel's dock edge and strut reserve, a popup's or
    /// dialog's parent. Sharing it rather than inventing a second one means a manager decoding
    /// this reads the role exactly as the compositor did.
    pub fn for_role(window: u32, role: Role, width: u32, height: u32) -> Self {
        let (aux16, aux32) = match role {
            Role::Normal => (0, 0),
            Role::Panel { dock, reserve } => (dock.tag(), reserve),
            Role::Popup { parent } | Role::Dialog { parent } => (0, parent),
        };
        Self { window, role: role.tag(), aux16, aux32, width, height }
    }
}

/// The body of a [`WindowCreated`](OP_MGR_WINDOW_CREATED).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrWindowCreated {
    /// The new window.
    pub window: u32,
    /// Its role tag — see [`ROLE_NORMAL`] and friends.
    pub role: u16,
    /// Role aux16: a panel's dock edge; zero otherwise.
    pub aux16: u16,
    /// Role aux32: a panel's reserve, or a popup/dialog's parent; zero otherwise.
    pub aux32: u32,
    /// The width the client asked for.
    pub width: u32,
    /// The height the client asked for.
    pub height: u32,
}

const _: () = assert!(core::mem::size_of::<MgrWindowCreated>() == 20);

impl MgrWindowCreated {
    /// Serialise into exactly 20 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 20 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..6].copy_from_slice(&self.role.to_le_bytes());
        out[6..8].copy_from_slice(&self.aux16.to_le_bytes());
        out[8..12].copy_from_slice(&self.aux32.to_le_bytes());
        out[12..16].copy_from_slice(&self.width.to_le_bytes());
        out[16..20].copy_from_slice(&self.height.to_le_bytes());
        Some(20)
    }

    /// Parse from exactly 20 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 20 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            role: u16::from_le_bytes([b[4], b[5]]),
            aux16: u16::from_le_bytes([b[6], b[7]]),
            aux32: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            width: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            height: u32::from_le_bytes([b[16], b[17], b[18], b[19]]),
        })
    }
}

/// The body of a [`Surface::Configure`](OP_CONFIGURE): where and how large.
///
/// Carries an **origin as well as a size** because the manager's answer to "where does this go"
/// is a placement, and a client that had to learn its position through some other message would
/// be reconciling two mechanisms that can disagree.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ConfigureEvent {
    /// Which window this is about — one session can hold several.
    pub window: u32,
    /// Suggested width in pixels.
    pub width: u32,
    /// Suggested height in pixels.
    pub height: u32,
    /// Top-left corner in screen coordinates.
    pub x: i32,
    /// Top-left corner in screen coordinates.
    pub y: i32,
}

const _: () = assert!(core::mem::size_of::<ConfigureEvent>() == 20);

impl ConfigureEvent {
    /// Serialise into exactly 20 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 20 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..8].copy_from_slice(&self.width.to_le_bytes());
        out[8..12].copy_from_slice(&self.height.to_le_bytes());
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
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            width: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            height: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            x: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            y: i32::from_le_bytes([b[16], b[17], b[18], b[19]]),
        })
    }
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

    /// `for_role` puts the role in the same two aux words `CreateWindowRequest` does — for
    /// **every** role, not just the one a gate happens to create.
    ///
    /// The mirror is the whole contract: a manager reads `WindowCreated` and must arrive at
    /// the role the client asked for. The two encoders are separate code with the same match
    /// arms, so nothing but a test holds them together — and every gate in the tree creates
    /// `Role::Normal`, where all three branches agree on `(0, 0)` and any mix-up is invisible.
    /// Swapping `(dock.tag(), reserve)` to `(reserve as u16, dock as u32)` leaves the suite
    /// green without this (PR #217 review, finding 6).
    #[test]
    fn for_role_encodes_every_role_the_way_a_create_request_does() {
        for role in [
            Role::Normal,
            Role::Panel { dock: Edge::Top, reserve: 24 },
            Role::Panel { dock: Edge::Left, reserve: 0 },
            Role::Popup { parent: 7 },
            Role::Dialog { parent: 9 },
        ] {
            let mut req = [0u8; CREATE_WINDOW_REQUEST_LEN];
            build_create_window_request(&mut req, &CreateWindowRequest::new(640, 480, role))
            .expect("request encodes");

            let ev = MgrWindowCreated::for_role(3, role, 640, 480);
            assert_eq!(ev.role, get_u16(&req, 8), "role tag for {role:?}");
            assert_eq!(ev.aux16, get_u16(&req, 10), "aux16 for {role:?}");
            assert_eq!(ev.aux32, get_u32(&req, 12), "aux32 for {role:?}");

            // And the whole way round: what a manager decodes is the role that was asked for.
            let mut body = [0u8; 20];
            let n = ev.write(&mut body).expect("event encodes");
            let back = MgrWindowCreated::read(&body[..n]).expect("event decodes");
            let parsed = parse_create_window_request(&req).expect("request parses");
            assert_eq!(parsed.role, role, "the request round-trips to the same role");
            assert_eq!((back.role, back.aux16, back.aux32), (ev.role, ev.aux16, ev.aux32));
            assert_eq!((back.width, back.height), (640, 480));
        }
    }

    /// `MgrWindowCreated`'s 20-byte layout.
    ///
    /// Written when this struct had no sender and no receiver — B3 was open, so nothing
    /// exercised the encoding end to end, and an event carrying role and geometry so a manager
    /// need not ask a follow-up question is exactly the kind of struct a byte-offset slip goes
    /// unnoticed in. `MgrPlace` and `MgrWindowRef` were at least driven through
    /// `manager::dispatch`; this had nothing (PR #216 review, finding 5). Both ends exist as of
    /// PR #217 — the compositor sends it and `ui-testclient` decodes it — so this now pins a
    /// layout that is on the wire rather than one that is only declared.
    #[test]
    fn mgr_window_created_round_trips_including_the_role_aux_fields() {
        let cases = [
            MgrWindowCreated { window: 1, role: ROLE_NORMAL, aux16: 0, aux32: 0, width: 64, height: 32 },
            // A panel carries its dock edge and strut reserve in the aux fields.
            MgrWindowCreated { window: 4096, role: ROLE_PANEL, aux16: 3, aux32: 28, width: 1280, height: 28 },
            // A popup carries its parent in `aux32`.
            MgrWindowCreated { window: 7, role: ROLE_POPUP, aux16: 0, aux32: 12, width: 180, height: 96 },
            // Extremes, to catch a field written at the wrong offset or the wrong width.
            MgrWindowCreated {
                window: u32::MAX,
                role: u16::MAX,
                aux16: u16::MAX,
                aux32: u32::MAX,
                width: u32::MAX,
                height: u32::MAX,
            },
        ];
        for want in cases {
            let mut buf = [0u8; 20];
            assert_eq!(want.write(&mut buf), Some(20), "must serialise into exactly 20 bytes");
            let got = MgrWindowCreated::read(&buf).expect("20 bytes must parse");
            assert_eq!(got.window, want.window);
            assert_eq!(got.role, want.role);
            assert_eq!(got.aux16, want.aux16);
            assert_eq!(got.aux32, want.aux32);
            assert_eq!(got.width, want.width);
            assert_eq!(got.height, want.height);
        }

        // Short buffers are refused rather than truncated, both directions.
        let mut short = [0u8; 19];
        assert_eq!(cases[0].write(&mut short), None, "19 bytes must not serialise");
        assert!(MgrWindowCreated::read(&short).is_none(), "19 bytes must not parse");
    }

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
            let req = CreateWindowRequest::new(800, 600, role);
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
        let req = CreateWindowRequest::new(4, 4, Role::Normal);
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
        let req = CreateWindowRequest::new(1, 1, Role::Normal);
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
        let req = CreateWindowRequest::new(1, 1, Role::Normal);
        assert!(build_create_window_request(&mut small, &req).is_none());
    }
}
