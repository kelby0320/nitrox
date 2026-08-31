//! The `Surface` category (`op = 0x09xx`) bodies — windows and their pixels. See
//! `docs/spec/rsproto-surface-ops.md`.
//!
//! The compositor is a resource server bound at `/dev/draw` with a subtree base, the
//! same binding kind `/home` uses, so window paths are forwarded resolves and nobody
//! calls `sys_ns_bind` when a window opens
//! (`docs/architecture/ui-composition-model.md` §2a).
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
/// `dialog` — parented, on its parent's desktop, and **listed**.
///
/// The parent carries this window's desktop membership and its lifetime — destroy is
/// transitive — but **not its position**: a manager places a dialog as it places any other
/// listed window, and it is held for one like any other. Only a `popup` is placed by its
/// creator. (An earlier definition also said "not offered as a wirable node on the composition
/// canvas"; that canvas was cut, and the rest stands without it.)
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
    /// Which desktop the window is on. **[`STICKY_DESKTOP`] (`0`) means every desktop.**
    pub desktop: u32,
    /// Window state bits — see [`WINDOW_FLAG_MINIMIZED`]. All other bits are reserved and zero.
    ///
    /// **A bitfield rather than a `minimized` boolean**, so the next window state (`maximized`,
    /// which M9 needs) costs a bit instead of another growth of this struct, and so a reader
    /// that does not know a bit degrades to "not set" rather than to a length mismatch.
    pub flags: u32,
}

/// The `desktop` value meaning **every** desktop — a sticky window.
///
/// Reserved rather than discovered: `0` is what an uninitialised field holds, and a window
/// whose desktop was never set showing up everywhere is a far more legible failure than one
/// that vanishes.
pub const STICKY_DESKTOP: u32 = 0;

/// [`WindowInfo::flags`] bit 0 — the window is minimized.
pub const WINDOW_FLAG_MINIMIZED: u32 = 1;

const _: () = assert!(core::mem::size_of::<WindowInfo>() == 40);

/// Bytes in a serialised [`WindowInfo`] — the exact size of a `/dev/draw/<N>/info` object.
pub const WINDOW_INFO_LEN: usize = 40;

impl WindowInfo {
    /// Build the info for a window with `role` at `(x, y)`, sized `width × height`.
    pub fn new(id: u32, role: Role, x: i32, y: i32, width: u32, height: u32) -> Self {
        let (dock, reserve, parent) = match role {
            Role::Normal => (0, 0, 0),
            Role::Panel { dock, reserve } => (dock.tag(), reserve, 0),
            Role::Popup { parent } | Role::Dialog { parent } => (0, 0, parent),
        };
        // `desktop` and `flags` are set by the caller after construction: this constructor
        // takes what `CreateWindow` fixes, and both of those are mutable state the compositor
        // owns rather than creation parameters.
        Self {
            id,
            width,
            height,
            x,
            y,
            role: role.tag(),
            dock,
            reserve,
            parent,
            desktop: STICKY_DESKTOP,
            flags: 0,
        }
    }

    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < WINDOW_INFO_LEN {
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
        put_u32(out, 32, self.desktop);
        put_u32(out, 36, self.flags);
        Some(WINDOW_INFO_LEN)
    }

    /// Parse from the first 40 bytes of a mapped `info` object.
    ///
    /// Returns `None` if the slice is short: a truncated read would otherwise produce a
    /// plausible window with zeroed geometry.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < WINDOW_INFO_LEN {
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
            desktop: get_u32(b, 32),
            flags: get_u32(b, 36),
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
    /// Offset from the parent's origin. **`popup` only.**
    ///
    /// Role-specific like the aux words, and written and read as zero for every other role —
    /// including `dialog`, which names a parent but is not placed relative to it.
    ///
    /// For a `popup` this is the whole of its placement: a menu is positioned by its *creator*,
    /// the only party that knows where the item it drops from was drawn. A `dialog` is an
    /// ordinary listed window that happens to name a parent — the parent carries its desktop
    /// membership and its lifetime, not its position (`display-substrate.md` §4a,
    /// `ui-composition-model.md` §6) — so a manager places it, and a manager needs nothing from
    /// the client to do so: `MgrWindowCreated` already carries the parent id and the requested
    /// size, which is what centring on a parent takes.
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

    /// A popup at `(x, y)` from its parent's origin.
    ///
    /// The offset is ignored — written and read as zero — for every other role, `dialog`
    /// included, so this says what it means rather than being the general constructor.
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
    // Zero for every role but `popup` — `dialog` included, which has a parent but is placed by
    // a manager — for the same reason the aux words are zeroed: two identical requests must
    // produce identical bytes.
    let (ox, oy) = match req.role {
        Role::Popup { .. } => (req.offset_x, req.offset_y),
        Role::Normal | Role::Panel { .. } | Role::Dialog { .. } => (0, 0),
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
    // **Only a `popup`.** Having a parent is not the test: a `dialog` has one and is still placed
    // by a manager, so reading these words for it would invent an offset the client is not
    // entitled to send — and the spec says they are read as zero, not merely written as zero.
    let (offset_x, offset_y) = match role {
        Role::Popup { .. } => (get_u32(body, 16) as i32, get_u32(body, 20) as i32),
        Role::Normal | Role::Panel { .. } | Role::Dialog { .. } => (0, 0),
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
    /// Which window this is about.
    ///
    /// **A session can hold several windows** — a popup is created on its parent's connection —
    /// so without this a client with a menu open cannot tell a keystroke meant for the menu
    /// from one meant for the window under it. `Release`, `FocusEvent` and `Configure` all
    /// carry one and `libsurface` filters on it; these two were shipped before the gap was
    /// found, so closing it is a wire break rather than two spare bytes (M6 C3; filed at the
    /// PR #184 re-review, whose stated trigger was "the first client with two windows").
    pub window: u32,
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

const _: () = assert!(core::mem::size_of::<KeyEvent>() == 12);

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
    /// Which window this is about — see [`KeyEvent::window`].
    ///
    /// Pointer records need it more than keys do, not less: a key goes to the focused window,
    /// which a client can track from `FocusEvent`, but a pointer record goes to the window
    /// *under the cursor* or to the grab holder. There is nothing to infer it from.
    pub window: u32,
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

const _: () = assert!(core::mem::size_of::<PointerEvent>() == 24);

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
// point of a manager. `/dev/draw/manage` is what bounds who may hold one, and as of M7 Part E
// that binding **does** gate: `desktop-shell` binds `/dev/draw/new` alone into every
// application namespace it builds, with subtree base `/new`, so `manage` is not a
// component-boundary prefix match against it and resolves to nothing. The shell's own session
// namespace binds the `/dev/draw` subtree unscoped and gets both. See
// `docs/architecture/graphical-session.md` §3.
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
/// `Manage::SetWindowDesktop` — move a window to a desktop; [`STICKY_DESKTOP`] for all of them.
///
/// The compositor stores the attribute and filters on it, and knows nothing about *which*
/// desktops exist — that is the desktop shell's, and this way the two cannot disagree.
pub const OP_MGR_SET_WINDOW_DESKTOP: u16 = 0x0916;
/// `Manage::SetMinimized` — hide a window without moving it off its desktop.
///
/// **A second attribute rather than a reserved `desktop` value**: a minimized window is still
/// on its desktop, restores there, and belongs in that desktop's window list, so folding the
/// two would make restoring a guess.
pub const OP_MGR_SET_MINIMIZED: u16 = 0x0917;
/// `Manage::Capture` — scale a window's current contents into a buffer the manager allocated.
///
/// The mirror of [`AttachBuffer`](OP_ATTACH_BUFFER): there a client allocates and the compositor
/// reads, here the manager allocates and the compositor writes. Capability-gated by being a
/// manager request — handing a client another window's pixels is the leak per-application
/// namespaces exist to prevent.
pub const OP_MGR_CAPTURE: u16 = 0x0920;

/// `Manage::QueryLayout` — the screen, and the work area every panel's strut leaves.
///
/// Empty request body; the reply is a [`MgrLayout`]. **The first thing a manager needs that it
/// cannot compute**: struts are declared by every `panel`, and only the compositor sees them all.
/// `Place`'s own note has always said "a manager computes positions from the work area", which
/// was not something it could do until this existed (M9 Part B).
pub const OP_MGR_QUERY_LAYOUT: u16 = 0x0921;

/// `Manage::LayoutChanged` — a manager event: the work area is not what it was.
///
/// Body: [`MgrLayout`]. Sent when the work area **differs from the one last announced**, not on
/// any particular cause. A manager that maximised a window against the old numbers would be
/// leaving it under a panel, and nothing else would report it.
///
/// Today only a panel appearing or going away can change the answer — a window's role, and so
/// its strut, is fixed at creation — but the comparison is deliberately blind to that: it stays
/// correct if a strut ever becomes settable, which is the kind of change that otherwise quietly
/// stops an event firing.
pub const OP_MGR_LAYOUT_CHANGED: u16 = 0x0922;

/// `Manage::RequestClose` — ask a window's client to close it.
///
/// Request: [`MgrWindowRef`] (`other` unused). The compositor forwards
/// [`CloseRequested`](OP_CLOSE_REQUESTED) to that window's client and answers when it has;
/// nothing about the window changes. `NotFound` if no such window, or if no session owns it.
///
/// **This is the polite half and it is the one a shell reaches for first.** A window holds a
/// process's work, and a taskbar that destroyed it would take the decision away from the only
/// participant that knows whether that matters.
pub const OP_MGR_REQUEST_CLOSE: u16 = 0x0924;

/// `Manage::Close` — destroy a window whose client will not.
///
/// Request: [`MgrWindowRef`] (`other` unused). The window is removed from the stack exactly as
/// [`DestroyWindow`](OP_DESTROY_WINDOW) removes one, descendants and all, and
/// [`WindowDestroyed`](OP_MGR_WINDOW_DESTROYED) is sent. `NotFound` if no such window.
///
/// **Distinct from `DestroyWindow`, which is a client's request on its own session.** This one
/// names a window the caller does not own, which is what the manager channel is for — and it is
/// the only answer available to a desktop whose applications draw their own chrome: a close
/// button the client paints cannot close a client that has stopped answering.
///
/// The client is not told, because there is nothing it could do with the information that it
/// could not have done with [`RequestClose`](OP_MGR_REQUEST_CLOSE). Its next request naming that
/// window is answered `NotFound`, as for any window that no longer exists.
pub const OP_MGR_CLOSE: u16 = 0x0925;

/// `Manage::WindowStateRequest` — a manager event: a client asked to be minimised or maximised.
///
/// Body: [`WindowState`]. The client's [`RequestState`](OP_REQUEST_STATE), forwarded. **The
/// manager decides**; the compositor neither applies it nor remembers it, because a window being
/// maximised is a rectangle the manager restores from and a second copy here could disagree.
pub const OP_MGR_WINDOW_STATE_REQUEST: u16 = 0x0923;

/// `Manage::DragEnded` — **server → manager. Unsolicited, no reply.**
///
/// Body: a [`ConfigureEvent`], carrying the window and the rectangle the gesture asks for.
///
/// **One event for a whole gesture**, sent when the button comes up. Two gestures produce it and
/// they mean the same thing to a manager: an interactive **resize** ([`StartResize`](OP_START_RESIZE))
/// asks for the rectangle the user let go at, and an interactive **move** released inside a
/// registered [snap zone](OP_MGR_REGISTER_SNAP_ZONE) asks for that zone's target. The name says
/// *drag* rather than *resize* because a manager's answer does not depend on which it was — it
/// was `ResizeEnded` for one part, before the second gesture that produces it existed (M9 Part F).
///
/// Nothing is sent per motion: the manager's queue does not coalesce and evicts its oldest when
/// full, so a five-second drag at 100 Hz would push a `WindowCreated` off the front and leave the
/// shell with a window it will never place and never hear about again. The outline the user sees
/// while dragging — the resize's rectangle, or the zone's target previewed under the pointer — is
/// the compositor's own drawing and crosses no wire at all.
///
/// **It carries a `ConfigureEvent` because that is what the manager sends back.** The shell's
/// whole answer is `Manage::Configure` with these five numbers — the compositor deliberately
/// does not apply them itself, so there is one path to a window's geometry rather than two that
/// can disagree, and a shell that decided the window may not have that rectangle is behaving
/// correctly.
pub const OP_MGR_DRAG_ENDED: u16 = 0x0926;
/// `Manage::RegisterHotkey` — route a key chord to the manager instead of the focused window.
///
/// A manager request rather than a client one because any application able to register `Super`
/// could impersonate the launcher. The capability is holding `/dev/draw/manage`.
pub const OP_MGR_REGISTER_HOTKEY: u16 = 0x091E;
/// `Manage::Hotkey` — a registered chord was pressed. Compositor → manager.
pub const OP_MGR_HOTKEY: u16 = 0x091F;
/// How many chords one manager may register.
///
/// Bounded because everything else here is. Sixteen covers a launcher chord plus switching and
/// moving across the single-digit desktops, which is what M8's shell binds.
pub const MAX_HOTKEYS: usize = 16;

/// `Manage::RegisterSnapZone` — a region of the screen, and the rectangle a window dropped in it
/// takes.
///
/// **A table the compositor matches against, exactly as [`RegisterHotkey`](OP_MGR_REGISTER_HOTKEY)
/// gave it chords it does not understand.** During an interactive move the compositor tests the
/// pointer against this table, shows the matching zone's *target* as the outline, and — if the
/// button comes up inside one — hands the manager that rectangle. The policy is entirely in the
/// numbers: which region means which rect, and how close counts, are the manager's to compute and
/// re-register. The compositor evaluates a lookup and knows nothing about halves or corners
/// (M9 Part F).
///
/// **Registering an existing id replaces it**, which is where this differs from a chord — and the
/// difference is what the two tables are. A chord table is a set of *distinct* chords, so a
/// duplicate id is a manager confusing itself and is refused. A zone table is a **layout**: it is
/// recomputed wholesale whenever the work area changes, and a manager re-registering the same
/// eight ids with new rectangles is doing the ordinary thing rather than a mistake.
pub const OP_MGR_REGISTER_SNAP_ZONE: u16 = 0x0927;

/// How many snap zones the compositor holds at once.
///
/// Bounded for the reason [`MAX_HOTKEYS`] is: it arrives off the wire and is held for the
/// manager's life. Eight is the shape a desktop uses — four edges and four corners — and this
/// leaves room for a shell that wants a few more without being a table a manager can grow
/// without limit.
pub const MAX_SNAP_ZONES: usize = 16;
/// `Manage::SetCurrentDesktop` — switch which desktop is composited.
///
/// **Numbered outside the `0x0910`–`0x0917` request block on purpose**: every other manager
/// request names a window in its first four bytes and this one names none, because it is a
/// property of the screen. `0` is refused — see [`MgrDesktop`].
pub const OP_MGR_SET_CURRENT_DESKTOP: u16 = 0x091D;
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
/// Body: [`title`] — a window id then UTF-8 bytes. Sent when a client names its window with
/// [`OP_SET_TITLE`], which is the only way a title changes. Built in M7 Part A, closing the
/// `m6-b3b-titles` deferral; the four lifecycle events above shipped in M6 B3.
pub const OP_MGR_WINDOW_TITLE: u16 = 0x091C;

/// `Surface::SetTitle` — a client naming its own window. Client → server, **silent on
/// success**; a malformed body gets the usual error reply.
///
/// Body: [`title`] — the window id, then UTF-8 bytes. Longer than [`MAX_TITLE`] is truncated
/// on a character boundary rather than refused, for the reason
/// [`title::truncate_title`] gives. A title is the one piece of a window a *manager* needs
/// and only the *client* knows.
pub const OP_SET_TITLE: u16 = 0x0909;

/// `Surface::StartMove` — hand the compositor an interactive move of this window.
///
/// **A client request, and the only kind of geometry change a client may originate.** Everything
/// else about where a window sits is the manager's: `Place` and `Configure` are manager ops, and
/// a client asking to be moved *to a position* would be asking to place itself. This asks for
/// something different — that the window follow the pointer the user is already holding down on
/// it — which is not a position the client knows or could compute (M9 Part A).
///
/// Refused unless the caller holds the pointer grab on that window: the grab is what makes
/// "the user is dragging me" true, and without the check a client could move its window while
/// nobody was touching it.
pub const OP_START_MOVE: u16 = 0x090A;

/// `Surface::RequestState` — ask to be minimised, maximised, or returned to normal.
///
/// **A client cannot do any of these itself, and must not be able to.** Minimising is
/// `Manage::SetMinimized` and maximising is `Manage::Configure` to a rectangle computed from the
/// work area; both are manager operations, and a client holding either could put another
/// client's window away or place itself. So this asks, the compositor forwards it to the manager
/// as [`WindowStateRequest`](OP_MGR_WINDOW_STATE_REQUEST), and the manager answers with the
/// request it would have sent anyway — the same shape as a resource server asking a supervisor
/// to bind it (M9 Part B).
///
/// The reply says the compositor accepted and forwarded it, **not** that anything happened: what
/// a manager does with it is the manager's, and a shell that decided a window may not be
/// maximised is behaving correctly.
pub const OP_REQUEST_STATE: u16 = 0x090B;

/// [`RequestState`](OP_REQUEST_STATE): the window is neither minimised nor maximised.
pub const WINDOW_STATE_NORMAL: u32 = 0;
/// [`RequestState`](OP_REQUEST_STATE): put the window away, off screen but in the window list.
pub const WINDOW_STATE_MINIMIZED: u32 = 1;
/// [`RequestState`](OP_REQUEST_STATE): fill the work area.
pub const WINDOW_STATE_MAXIMIZED: u32 = 2;

/// `Surface::CloseRequested` — **server → client. Unsolicited, `request_id` 0, no reply.**
///
/// Body, 4 bytes: `window` (u32). Somebody with the manager channel is asking this window to
/// close — the taskbar's close, or anything else a shell offers. **It is a request and there is
/// no way to refuse it**, which is deliberate: a client that wants to ask "save first?" opens a
/// dialog and closes when that resolves, and a client that ignores this simply stays open.
///
/// What a client does *not* have to do is guess. The alternative — a manager destroying the
/// window outright — takes the decision away from the process that owns the work in it, which is
/// why the shell asks first and only insists when nothing happens (M9 Part C).
pub const OP_CLOSE_REQUESTED: u16 = 0x090C;

/// `Surface::StartResize` — hand the compositor an interactive resize of this window.
///
/// **The same shape as [`StartMove`](OP_START_MOVE), and refused the same way**: unless the
/// caller holds the pointer grab on that window, which is what makes "the user is dragging my
/// edge" true. What differs is what the compositor does with it — a move changes the window's
/// origin as the pointer travels, and a resize changes *nothing*. It tracks a rectangle, draws
/// an outline over the composed stack, and when the button comes up it hands the manager the
/// rectangle the user let go at ([`DragEnded`](OP_MGR_DRAG_ENDED)).
///
/// **The compositor never resizes a client**, which is why the gesture ends in a manager event
/// rather than in a `Configure` from here: there is one path to a window's geometry — the
/// manager's — rather than two that can disagree. That the outline moves per motion and the
/// `Configure` goes out once is decision 3 of Milestone 9: a `Configure` per motion is a client
/// cost (new buffers, mapped, re-laid-out, repainted) and not a protocol one (M9 Part E).
pub const OP_START_RESIZE: u16 = 0x090D;

/// [`StartResize`](OP_START_RESIZE): the pointer is dragging the window's left edge.
pub const RESIZE_LEFT: u32 = 1 << 0;
/// [`StartResize`](OP_START_RESIZE): the right edge.
pub const RESIZE_RIGHT: u32 = 1 << 1;
/// [`StartResize`](OP_START_RESIZE): the top edge.
pub const RESIZE_TOP: u32 = 1 << 2;
/// [`StartResize`](OP_START_RESIZE): the bottom edge.
pub const RESIZE_BOTTOM: u32 = 1 << 3;
/// Every edge [`StartResize`](OP_START_RESIZE) understands; anything else is `InvalidArgument`.
///
/// **A corner is two bits**, which is the whole reason this is a mask rather than an enum of
/// eight directions: the bottom-right grip is `RESIZE_RIGHT | RESIZE_BOTTOM`, and the arithmetic
/// that moves an edge is written once per axis rather than once per direction. Opposite edges
/// together (`LEFT | RIGHT`) is refused — there is no gesture that drags both — as is naming no
/// edge at all, which would be a drag that changes nothing while holding a grab.
pub const RESIZE_EDGES: u32 = RESIZE_LEFT | RESIZE_RIGHT | RESIZE_TOP | RESIZE_BOTTOM;

/// `Surface::DeclareAcceptor` — this window takes drops of these kinds, under this name.
///
/// Body: `window` (u32), `kinds` (u32), then the acceptor's name as UTF-8 — at most
/// [`MAX_ACCEPTOR_NAME`] bytes, with the body's own length giving it, as
/// [`SetTitle`](OP_SET_TITLE) does.
///
/// **A name rather than a rectangle, and that is M10 decision 2.** An acceptor is a *port in
/// waiting*: today it is addressed by a drag ending over the window, and when ports arrive the
/// same name is what a command line addresses. A protocol that described a *region* instead
/// would have to be re-specified to be addressed any other way — and regions are the client's
/// anyway (decision 3), routed from the pointer position the drop carries, exactly as a press is.
///
/// **Declared once, not queried per drag.** The compositor holds the table and matches against
/// it while the pointer moves; asking the client mid-gesture would put a round trip per motion
/// on the path that has to stay cheap. It supersedes the composition model's `QueryCaps`.
///
/// Bounded per window like every other table the compositor holds ([`MAX_ACCEPTORS`]), and
/// **cleared with the window** — an acceptor is a property of a window, not of a session, so a
/// client that destroys and recreates a window declares again.
///
/// Re-declaring an existing name **replaces** it, for the reason a snap zone does: a set of
/// acceptors is a description of what this window is currently able to take, and a client whose
/// panel changed would otherwise have to remove one to change it.
///
/// `InvalidArgument` for an empty name, a name over the cap, one that is not UTF-8, or `kinds`
/// naming nothing this protocol knows. `NoSuchWindow` for another session's window.
/// `Unsupported` when the window's table is full.
pub const OP_DECLARE_ACCEPTOR: u16 = 0x090E;

/// `Surface::StartDrag` — the user is dragging this payload out of this window.
///
/// Body: `window` (u32), `kind` (u32), `path_len` (u32), then `path_len` bytes of path followed
/// by the display name, both UTF-8.
///
/// **The same shape as [`StartMove`](OP_START_MOVE) and [`StartResize`](OP_START_RESIZE), and
/// refused the same way**: unless the caller holds the pointer grab on that window. The grab is
/// what makes "the user is dragging this" true — without the check a client could start a drag
/// with nobody touching it, and a drag is an offer of a payload to whatever window it ends over.
///
/// **The payload is a path, which is M10 decision 1.** A handle would have to belong to somebody
/// while the gesture is in flight, and a refused transfer has no clean owner; a path is a name
/// the receiving program opens for itself, reporting its own errors in its own window. The
/// consequence is that both ends must be able to resolve it, which is true here because
/// `desktop-shell` binds `/home` identically into every application namespace it builds.
///
/// The compositor runs the gesture: it highlights windows whose acceptors take `kind`, and on
/// release inside one sends [`Dropped`](OP_DROPPED) to that window. Released anywhere else
/// **nothing is sent** and the drag is simply over — a drop on nothing is not an error, it is
/// the ordinary way to change your mind.
///
/// `InvalidArgument` for an empty path, one over [`MAX_DROP_PATH`], a name over
/// [`MAX_DROP_NAME`], bytes that are not UTF-8, or a `kind` that is not exactly one known kind.
/// `NoSuchWindow` when the caller does not hold the grab, or a move or resize is already running.
pub const OP_START_DRAG: u16 = 0x090F;

/// `Surface::Dropped` — a drag ended over this window, on this acceptor.
///
/// Body: `window` (u32), `kind` (u32), `x` (i32), `y` (i32), `acceptor_len` (u32),
/// `path_len` (u32), then the acceptor's name, the path, and the display name in that order.
///
/// **`x` and `y` are window-local, like a [`PointerEvent`]'s**, because that is what makes a
/// client-side drop *region* possible without the protocol knowing about one: `libui` routes
/// this to the widget under the point exactly as it routes a press, so a window can accept a
/// drop in one panel and not another (decision 3).
///
/// **0x0930 rather than 0x0910**: the client block 0x0900–0x090F is full, and 0x0910–0x092F is
/// the manager's. This begins the second client block rather than borrowing a number from a
/// range that means something else.
pub const OP_DROPPED: u16 = 0x0930;

/// A drop payload that is one file.
pub const DROP_KIND_FILE: u32 = 1 << 0;
/// A drop payload that is one directory.
pub const DROP_KIND_DIR: u32 = 1 << 1;
/// Every kind this protocol knows; anything else is `InvalidArgument`.
///
/// **A mask, because an *acceptor* takes a set** — "files but not folders" is the distinction
/// M10's details pass named as the thing a window must be able to draw. A *drag* carries exactly
/// one bit: a payload is one thing, and a drag offering "file or directory" would be asking the
/// receiver to guess.
pub const DROP_KINDS_KNOWN: u32 = DROP_KIND_FILE | DROP_KIND_DIR;

/// The longest acceptor name accepted, in bytes.
pub const MAX_ACCEPTOR_NAME: usize = 32;

/// How many acceptors one window may declare.
///
/// Small on purpose: an acceptor is a *sink* a window offers, not a widget. A window with four
/// distinct kinds of drop target is already a design worth questioning, and the bound is what
/// stops a client from making the compositor's per-window state unbounded.
pub const MAX_ACCEPTORS: usize = 4;

/// The longest path a drag may carry, in bytes.
pub const MAX_DROP_PATH: usize = 512;

/// The longest display name a drag may carry, in bytes.
pub const MAX_DROP_NAME: usize = 64;

/// The longest window title accepted, in bytes.
///
/// Bounded at the protocol edge for the reason [`MAX_STRUT_RESERVE`] is: it arrives off the wire
/// from a client, it is stored per window for the compositor's life, and a manager forwarding it
/// has to fit it in a message. Long enough for any sentence anyone puts in a title bar.
pub const MAX_TITLE: usize = 256;

/// A manager request naming one window and one `u32` — [`SetWindowDesktop`](OP_MGR_SET_WINDOW_DESKTOP)
/// and [`SetMinimized`](OP_MGR_SET_MINIMIZED).
///
/// Shared because the two have the same shape and the same failure (`NotFound`); the field is
/// named `value` rather than `desktop` so neither op reads as the other's special case.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrWindowValue {
    /// Window the request names.
    pub window: u32,
    /// The desktop id, or the minimized flag — see the op.
    pub value: u32,
}

impl MgrWindowValue {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.value);
        Some(8)
    }

    /// Parse from the first 8 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self { window: get_u32(b, 0), value: get_u32(b, 4) })
    }
}

/// A capture request: which window, and the geometry of the buffer travelling with it.
///
/// The handle is sent alongside rather than named here, the way every other handle on this
/// protocol travels — a body cannot carry one.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrCapture {
    /// The window to scale.
    pub window: u32,
    /// Destination width, never larger than the window's.
    pub width: u32,
    /// Destination height, never larger than the window's.
    pub height: u32,
    /// Destination bytes per row; the object must hold `pitch * height`.
    pub pitch: u32,
}

impl MgrCapture {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 16 {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.width);
        put_u32(out, 8, self.height);
        put_u32(out, 12, self.pitch);
        Some(16)
    }

    /// Parse from the first 16 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 16 {
            return None;
        }
        Some(Self {
            window: get_u32(b, 0),
            width: get_u32(b, 4),
            height: get_u32(b, 8),
            pitch: get_u32(b, 12),
        })
    }
}

/// A key chord — the body of both [`RegisterHotkey`](OP_MGR_REGISTER_HOTKEY) and the
/// [`Hotkey`](OP_MGR_HOTKEY) event it produces.
///
/// **One type for the request and the event**, because the event's job is to say *which* chord
/// fired and the manager already knows the chord by the id it chose. Echoing `mods` and `code`
/// costs four bytes and means a manager that lost track can still tell them apart.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrHotkey {
    /// Manager-chosen identity, the way a client chooses a buffer id. **Never zero** — that is
    /// reserved so a zeroed body cannot register anything.
    pub id: u32,
    /// Modifiers that must be held, matched **exactly**: `Super+Shift+2` is not `Super+2`.
    pub mods: u16,
    /// The keycode, in the table `libkern::abi` mirrors.
    pub code: u16,
}

/// A snap zone: the region that triggers it, and the rectangle a window dropped there takes.
///
/// Both in screen coordinates. See [`RegisterSnapZone`](OP_MGR_REGISTER_SNAP_ZONE).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrSnapZone {
    /// Manager-chosen identity. **Never zero** — reserved so a zeroed body registers nothing.
    pub id: u32,
    /// Where the pointer has to be, in screen coordinates.
    pub trigger_x: i32,
    /// See [`trigger_x`](Self::trigger_x).
    pub trigger_y: i32,
    /// See [`trigger_x`](Self::trigger_x).
    pub trigger_w: u32,
    /// See [`trigger_x`](Self::trigger_x).
    pub trigger_h: u32,
    /// What the window becomes, in screen coordinates.
    pub target_x: i32,
    /// See [`target_x`](Self::target_x).
    pub target_y: i32,
    /// See [`target_x`](Self::target_x).
    pub target_w: u32,
    /// See [`target_x`](Self::target_x).
    pub target_h: u32,
}

impl MgrSnapZone {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 36 {
            return None;
        }
        put_u32(out, 0, self.id);
        put_u32(out, 4, self.trigger_x as u32);
        put_u32(out, 8, self.trigger_y as u32);
        put_u32(out, 12, self.trigger_w);
        put_u32(out, 16, self.trigger_h);
        put_u32(out, 20, self.target_x as u32);
        put_u32(out, 24, self.target_y as u32);
        put_u32(out, 28, self.target_w);
        put_u32(out, 32, self.target_h);
        Some(36)
    }

    /// Parse from the first 36 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 36 {
            return None;
        }
        Some(Self {
            id: get_u32(b, 0),
            trigger_x: get_u32(b, 4) as i32,
            trigger_y: get_u32(b, 8) as i32,
            trigger_w: get_u32(b, 12),
            trigger_h: get_u32(b, 16),
            target_x: get_u32(b, 20) as i32,
            target_y: get_u32(b, 24) as i32,
            target_w: get_u32(b, 28),
            target_h: get_u32(b, 32),
        })
    }
}

impl MgrHotkey {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        put_u32(out, 0, self.id);
        put_u16(out, 4, self.mods);
        put_u16(out, 6, self.code);
        Some(8)
    }

    /// Parse from the first 8 bytes of a body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self { id: get_u32(b, 0), mods: get_u16(b, 4), code: get_u16(b, 6) })
    }
}

/// A manager request naming a desktop and no window —
/// [`SetCurrentDesktop`](OP_MGR_SET_CURRENT_DESKTOP).
///
/// **`0` is not a legal current desktop**, and it is the one value this request validates.
/// `0` means sticky, so a current of `0` would blank every non-sticky window *and* — by the
/// rule that a window is created onto the current desktop — make everything created afterwards
/// silently sticky.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrDesktop {
    /// The desktop to switch to. Never [`STICKY_DESKTOP`].
    pub desktop: u32,
}

impl MgrDesktop {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 4 {
            return None;
        }
        put_u32(out, 0, self.desktop);
        Some(4)
    }

    /// Parse from the first 4 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 4 {
            return None;
        }
        Some(Self { desktop: get_u32(b, 0) })
    }
}

/// One window, by id — [`CloseRequested`](OP_CLOSE_REQUESTED).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WindowRef {
    /// Which window.
    pub window: u32,
}

impl WindowRef {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 4 {
            return None;
        }
        put_u32(out, 0, self.window);
        Some(4)
    }

    /// Parse from the first 4 bytes of a body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 4 {
            return None;
        }
        Some(Self { window: get_u32(b, 0) })
    }
}

/// A window and a state it is asked to be in — [`RequestState`](OP_REQUEST_STATE), and the
/// [`WindowStateRequest`](OP_MGR_WINDOW_STATE_REQUEST) event that forwards it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct WindowState {
    /// Which window.
    pub window: u32,
    /// One of [`WINDOW_STATE_NORMAL`], [`WINDOW_STATE_MINIMIZED`], [`WINDOW_STATE_MAXIMIZED`].
    pub state: u32,
}

impl WindowState {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.state);
        Some(8)
    }

    /// Parse from the first 8 bytes of a body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self { window: get_u32(b, 0), state: get_u32(b, 4) })
    }
}

/// The screen, and the part of it a maximised window may have — [`QueryLayout`](OP_MGR_QUERY_LAYOUT).
///
/// **The work area is not derivable by the manager.** Every `panel` reserves an edge, and the
/// compositor is what knows all of them: a shell that subtracted only its *own* bars would put a
/// maximised window under any other panel-role client, with nothing able to notice.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrLayout {
    /// Screen width in pixels.
    pub screen_w: u32,
    /// Screen height in pixels.
    pub screen_h: u32,
    /// Work area origin — left.
    pub work_x: i32,
    /// Work area origin — top.
    pub work_y: i32,
    /// Work area width.
    pub work_w: u32,
    /// Work area height.
    pub work_h: u32,
}

impl MgrLayout {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 24 {
            return None;
        }
        put_u32(out, 0, self.screen_w);
        put_u32(out, 4, self.screen_h);
        put_u32(out, 8, self.work_x as u32);
        put_u32(out, 12, self.work_y as u32);
        put_u32(out, 16, self.work_w);
        put_u32(out, 20, self.work_h);
        Some(24)
    }

    /// Parse from the first 24 bytes of a body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 24 {
            return None;
        }
        Some(Self {
            screen_w: get_u32(b, 0),
            screen_h: get_u32(b, 4),
            work_x: get_u32(b, 8) as i32,
            work_y: get_u32(b, 12) as i32,
            work_w: get_u32(b, 16),
            work_h: get_u32(b, 20),
        })
    }
}

/// A client request naming one window — [`StartMove`](OP_START_MOVE).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StartMove {
    /// The window to move. Must be one the caller owns and holds the pointer grab on.
    pub window: u32,
}

impl StartMove {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 4 {
            return None;
        }
        put_u32(out, 0, self.window);
        Some(4)
    }

    /// Parse from the first 4 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 4 {
            return None;
        }
        Some(Self { window: get_u32(b, 0) })
    }
}

/// A client request naming a window and the edges being dragged — [`StartResize`](OP_START_RESIZE).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StartResize {
    /// The window to resize. Must be one the caller owns and holds the pointer grab on.
    pub window: u32,
    /// Which edges the pointer is dragging — a mask of [`RESIZE_LEFT`] and friends.
    pub edges: u32,
}

impl StartResize {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 8 {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.edges);
        Some(8)
    }

    /// Parse from the first 8 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 8 {
            return None;
        }
        Some(Self { window: get_u32(b, 0), edges: get_u32(b, 4) })
    }

    /// Whether `edges` names a gesture that exists: at least one edge, and not both of a pair.
    pub fn edges_are_a_gesture(edges: u32) -> bool {
        edges != 0
            && edges & !RESIZE_EDGES == 0
            && edges & (RESIZE_LEFT | RESIZE_RIGHT) != (RESIZE_LEFT | RESIZE_RIGHT)
            && edges & (RESIZE_TOP | RESIZE_BOTTOM) != (RESIZE_TOP | RESIZE_BOTTOM)
    }
}

/// A [`DeclareAcceptor`](OP_DECLARE_ACCEPTOR) request: the fixed head, with the name following.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DeclareAcceptor {
    /// The window declaring it. Must be one the caller owns.
    pub window: u32,
    /// The kinds this acceptor takes — a mask of [`DROP_KIND_FILE`] and friends.
    pub kinds: u32,
}

impl DeclareAcceptor {
    /// The fixed part's length; the name follows it.
    pub const HEAD: usize = 8;

    /// Serialise the head and `name` into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8], name: &[u8]) -> Option<usize> {
        if name.len() > MAX_ACCEPTOR_NAME || out.len() < Self::HEAD + name.len() {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.kinds);
        out[Self::HEAD..Self::HEAD + name.len()].copy_from_slice(name);
        Some(Self::HEAD + name.len())
    }

    /// Parse a request body into the head and the name it carries.
    ///
    /// **The name is validated here rather than by each reader**, because there is exactly one
    /// rule and two would eventually differ: non-empty, within the cap, and UTF-8.
    pub fn read(b: &[u8]) -> Option<(Self, &str)> {
        if b.len() < Self::HEAD {
            return None;
        }
        let name = core::str::from_utf8(&b[Self::HEAD..]).ok()?;
        if name.is_empty() || name.len() > MAX_ACCEPTOR_NAME {
            return None;
        }
        let head = Self { window: get_u32(b, 0), kinds: get_u32(b, 4) };
        // A declaration naming no kind this protocol knows accepts nothing, which is a window
        // asking to be highlighted for drags it cannot take.
        if head.kinds == 0 || head.kinds & !DROP_KINDS_KNOWN != 0 {
            return None;
        }
        Some((head, name))
    }
}

/// A [`StartDrag`](OP_START_DRAG) request: the fixed head, with the path and name following.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StartDrag {
    /// The window the drag comes out of. Must hold the pointer grab.
    pub window: u32,
    /// What the payload is — exactly one of [`DROP_KIND_FILE`] and friends.
    pub kind: u32,
    /// How many bytes of the tail are the path; the rest is the display name.
    pub path_len: u32,
}

impl StartDrag {
    /// The fixed part's length; the path and name follow it.
    pub const HEAD: usize = 12;

    /// Serialise the head, `path` and `name` into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8], path: &[u8], name: &[u8]) -> Option<usize> {
        if path.is_empty() || path.len() > MAX_DROP_PATH || name.len() > MAX_DROP_NAME {
            return None;
        }
        let n = Self::HEAD + path.len() + name.len();
        if out.len() < n {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.kind);
        put_u32(out, 8, path.len() as u32);
        out[Self::HEAD..Self::HEAD + path.len()].copy_from_slice(path);
        out[Self::HEAD + path.len()..n].copy_from_slice(name);
        Some(n)
    }

    /// Parse a request body into the head, the path and the display name.
    ///
    /// **`kind` must be exactly one bit**: a drag carries one payload, and one offering "file or
    /// directory" would be asking whatever it lands on to guess.
    pub fn read(b: &[u8]) -> Option<(Self, &str, &str)> {
        if b.len() < Self::HEAD {
            return None;
        }
        let head =
            Self { window: get_u32(b, 0), kind: get_u32(b, 4), path_len: get_u32(b, 8) };
        if head.kind == 0
            || head.kind & !DROP_KINDS_KNOWN != 0
            || !head.kind.is_power_of_two()
        {
            return None;
        }
        let len = head.path_len as usize;
        // **The declared length must fit what arrived**, or a short body would slice past the
        // end — the reader's half of the bound the writer already checks.
        if len == 0 || len > MAX_DROP_PATH || Self::HEAD + len > b.len() {
            return None;
        }
        let path = core::str::from_utf8(&b[Self::HEAD..Self::HEAD + len]).ok()?;
        let name = core::str::from_utf8(&b[Self::HEAD + len..]).ok()?;
        if name.len() > MAX_DROP_NAME {
            return None;
        }
        Some((head, path, name))
    }
}

/// A [`Dropped`](OP_DROPPED) event: the fixed head, with three names following.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct DroppedEvent {
    /// The window the drop landed on.
    pub window: u32,
    /// What the payload is — one of [`DROP_KIND_FILE`] and friends.
    pub kind: u32,
    /// Where the pointer was, **window-local** like a [`PointerEvent`]'s.
    pub x: i32,
    /// See [`x`](Self::x).
    pub y: i32,
    /// How many bytes of the tail are the acceptor's name.
    pub acceptor_len: u32,
    /// How many of the rest are the path; what follows that is the display name.
    pub path_len: u32,
}

impl DroppedEvent {
    /// The fixed part's length; the three names follow it.
    pub const HEAD: usize = 24;

    /// Serialise the head and the three names into `out`; returns the length written.
    pub fn write(
        &self,
        out: &mut [u8],
        acceptor: &[u8],
        path: &[u8],
        name: &[u8],
    ) -> Option<usize> {
        if acceptor.is_empty()
            || acceptor.len() > MAX_ACCEPTOR_NAME
            || path.is_empty()
            || path.len() > MAX_DROP_PATH
            || name.len() > MAX_DROP_NAME
        {
            return None;
        }
        let n = Self::HEAD + acceptor.len() + path.len() + name.len();
        if out.len() < n {
            return None;
        }
        put_u32(out, 0, self.window);
        put_u32(out, 4, self.kind);
        out[8..12].copy_from_slice(&self.x.to_le_bytes());
        out[12..16].copy_from_slice(&self.y.to_le_bytes());
        put_u32(out, 16, acceptor.len() as u32);
        put_u32(out, 20, path.len() as u32);
        let a = Self::HEAD;
        let p = a + acceptor.len();
        let m = p + path.len();
        out[a..p].copy_from_slice(acceptor);
        out[p..m].copy_from_slice(path);
        out[m..n].copy_from_slice(name);
        Some(n)
    }

    /// Parse an event body into the head, the acceptor, the path and the display name.
    pub fn read(b: &[u8]) -> Option<(Self, &str, &str, &str)> {
        if b.len() < Self::HEAD {
            return None;
        }
        let head = Self {
            window: get_u32(b, 0),
            kind: get_u32(b, 4),
            x: i32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            y: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            acceptor_len: get_u32(b, 16),
            path_len: get_u32(b, 20),
        };
        let a = head.acceptor_len as usize;
        let p = head.path_len as usize;
        // **Both lengths checked against what arrived, and their sum checked for overflow.**
        // Two fields that each fit and together do not is exactly the shape a length-prefixed
        // record gets wrong.
        let end = Self::HEAD.checked_add(a)?.checked_add(p)?;
        if a == 0 || a > MAX_ACCEPTOR_NAME || p == 0 || p > MAX_DROP_PATH || end > b.len() {
            return None;
        }
        let acceptor = core::str::from_utf8(&b[Self::HEAD..Self::HEAD + a]).ok()?;
        let path = core::str::from_utf8(&b[Self::HEAD + a..end]).ok()?;
        let name = core::str::from_utf8(&b[end..]).ok()?;
        if name.len() > MAX_DROP_NAME {
            return None;
        }
        Some((head, acceptor, path, name))
    }
}

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

/// The **first variable-length Surface record**, shared by [`OP_SET_TITLE`] (client → server)
/// and [`OP_MGR_WINDOW_TITLE`] (server → manager).
///
/// Layout: a 4-byte little-endian window id, then the title's UTF-8 bytes, **and nothing
/// else**. There is no length field: the body's own length gives it, because a Surface body
/// arrives inside a message that already carries one. Adding a second length would create two
/// sources of truth for the same number and a way for them to disagree.
///
/// This is the wire-format question the `m6-b3b-titles` deferral split off from M6 B3 — "a
/// length convention, a cap, and a decision about what a client sending 64 KiB of title gets
/// back".
/// The convention is above, the cap is [`MAX_TITLE`], and the answer to the third is
/// [`truncate_title`].
pub mod title {
    use super::MAX_TITLE;

    /// Parse a title record into `(window, title)`.
    ///
    /// `None` on a body too short to hold a window id, or one whose title is not UTF-8 — a
    /// title is displayed, so a client that sends bytes nobody can render is malformed rather
    /// than merely unlucky. The returned title is **not** truncated; that is
    /// [`truncate_title`]'s job at the point of storage, so a parser stays a parser.
    pub fn read(b: &[u8]) -> Option<(u32, &str)> {
        if b.len() < 4 {
            return None;
        }
        let window = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        let title = core::str::from_utf8(&b[4..]).ok()?;
        Some((window, title))
    }

    /// Serialise `(window, title)`. `None` if `out` cannot hold it.
    ///
    /// The caller is expected to have passed `title` through [`truncate_title`] already; this
    /// does not silently shorten, because a writer that truncates hides the one place the
    /// decision should be visible.
    pub fn write(window: u32, title: &str, out: &mut [u8]) -> Option<usize> {
        let n = 4 + title.len();
        if out.len() < n {
            return None;
        }
        out[0..4].copy_from_slice(&window.to_le_bytes());
        out[4..n].copy_from_slice(title.as_bytes());
        Some(n)
    }

    /// The longest prefix of `title` that fits in [`MAX_TITLE`] bytes, **cut on a character
    /// boundary**.
    ///
    /// **Truncate rather than reject**, decided 2026-08-25. [`OP_SET_TITLE`] is silent on
    /// success and has no reply a client reads, so rejecting would need an error path built
    /// for the one op that was specified not to have one — and a dropped tail on a title is
    /// benign in a way a dropped message is not. Every windowing system in use does the same.
    ///
    /// **On a character boundary**, which is the part that is easy to get wrong: slicing at
    /// `MAX_TITLE` bytes can land inside a multi-byte character, and the result is not UTF-8
    /// at all — so a cap meant to bound memory would instead corrupt the string, and a
    /// manager decoding it would see a malformed record rather than a shortened title.
    pub fn truncate_title(title: &str) -> &str {
        if title.len() <= MAX_TITLE {
            return title;
        }
        let mut end = MAX_TITLE;
        while end > 0 && !title.is_char_boundary(end) {
            end -= 1;
        }
        &title[..end]
    }
}

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
    /// A key record for `window`.
    ///
    /// **A constructor, so the next field costs no call sites.** Widening this record for the
    /// window id broke every literal in the tree; the one after it will not.
    pub const fn new(window: u32, keycode: u16, pressed: u16, modifiers: u16) -> Self {
        Self { window, keycode, pressed, modifiers, _pad: 0 }
    }

    /// Serialise into exactly 12 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 12 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..6].copy_from_slice(&self.keycode.to_le_bytes());
        out[6..8].copy_from_slice(&self.pressed.to_le_bytes());
        out[8..10].copy_from_slice(&self.modifiers.to_le_bytes());
        out[10..12].copy_from_slice(&0u16.to_le_bytes());
        Some(12)
    }

    /// Parse from exactly 12 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 12 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            keycode: u16::from_le_bytes([b[4], b[5]]),
            pressed: u16::from_le_bytes([b[6], b[7]]),
            modifiers: u16::from_le_bytes([b[8], b[9]]),
            _pad: 0,
        })
    }
}

impl PointerEvent {
    /// A pointer record for `window` at window-local `(x, y)`.
    ///
    /// A constructor for the same reason [`KeyEvent::new`] is one.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        window: u32,
        kind: u16,
        button: u16,
        buttons: u16,
        flags: u16,
        modifiers: u16,
        x: i32,
        y: i32,
    ) -> Self {
        Self { window, kind, button, buttons, flags, modifiers, _pad: 0, x, y }
    }

    /// Serialise into exactly 24 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 24 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..6].copy_from_slice(&self.kind.to_le_bytes());
        out[6..8].copy_from_slice(&self.button.to_le_bytes());
        out[8..10].copy_from_slice(&self.buttons.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..14].copy_from_slice(&self.modifiers.to_le_bytes());
        out[14..16].copy_from_slice(&0u16.to_le_bytes());
        out[16..20].copy_from_slice(&self.x.to_le_bytes());
        out[20..24].copy_from_slice(&self.y.to_le_bytes());
        Some(24)
    }

    /// Parse from exactly 24 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 24 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            kind: u16::from_le_bytes([b[4], b[5]]),
            button: u16::from_le_bytes([b[6], b[7]]),
            buttons: u16::from_le_bytes([b[8], b[9]]),
            flags: u16::from_le_bytes([b[10], b[11]]),
            modifiers: u16::from_le_bytes([b[12], b[13]]),
            _pad: 0,
            x: i32::from_le_bytes([b[16], b[17], b[18], b[19]]),
            y: i32::from_le_bytes([b[20], b[21], b[22], b[23]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Only a `popup` carries an offset.** Every other role, `dialog` included, sends zero.
    ///
    /// A `dialog` names a parent, but the parent carries its desktop membership and its
    /// lifetime — not its position. It is an ordinary listed window and a manager places it, so
    /// a client-supplied offset would be redundant with what `MgrWindowCreated` already tells
    /// the manager, and would compete with the placement the manager chose. Nothing asserted
    /// this either way before, so the encoder could have started carrying it and no test would
    /// have noticed.
    #[test]
    fn only_a_popup_carries_an_offset_on_the_wire() {
        for role in [
            Role::Normal,
            Role::Panel { dock: Edge::Top, reserve: 4 },
            Role::Dialog { parent: 9 },
        ] {
            let mut buf = [0xAAu8; CREATE_WINDOW_REQUEST_LEN];
            build_create_window_request(&mut buf, &CreateWindowRequest::at(8, 8, role, 77, 88))
                .expect("encodes");
            assert_eq!(&buf[16..24], &[0u8; 8], "{role:?} must not put an offset on the wire");

            // **Then put an offset there by hand.** Parsing the buffer the correct encoder just
            // zeroed asserts nothing: it holds for a parser that reads those words too, because
            // there is nothing in them to read. The reader is a separate arm and needs a body
            // that would betray it (PR #220 review, finding 1).
            put_u32(&mut buf, 16, 77);
            put_u32(&mut buf, 20, 88);
            let back = parse_create_window_request(&buf).expect("parses");
            assert_eq!(
                (back.offset_x, back.offset_y),
                (0, 0),
                "{role:?} must discard an offset even when one is on the wire"
            );
        }

        // And the one role that does.
        let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
        let req = CreateWindowRequest::at(8, 8, Role::Popup { parent: 3 }, 77, -88);
        build_create_window_request(&mut buf, &req).expect("encodes");
        let back = parse_create_window_request(&buf).expect("parses");
        assert_eq!((back.offset_x, back.offset_y), (77, -88), "a popup's offset survives");
    }

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
        // **10..24, not 10..16.** The record grew two words in C1 and the invariant covers them:
        // the offset is meaningless without a parent, so a `normal` request that left those
        // bytes alone would put the caller's stack contents on the wire and two identical
        // requests would differ (PR #219 review, finding 4).
        assert_eq!(&buf[10..24], &[0u8; 14]);
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
        let e = KeyEvent::new(0x4142_4344, 0x1112, 1, 0x3132);
        let mut b = [0u8; 12];
        e.write(&mut b).unwrap();
        assert_eq!(&b[0..4], &0x4142_4344u32.to_le_bytes(), "window @0");
        assert_eq!(&b[4..6], &0x1112u16.to_le_bytes(), "keycode @4");
        assert_eq!(&b[6..8], &1u16.to_le_bytes(), "pressed @6");
        assert_eq!(&b[8..10], &0x3132u16.to_le_bytes(), "modifiers @8");
        assert_eq!(&b[10..12], &0u16.to_le_bytes(), "reserved @10, zero");
        assert_eq!(KeyEvent::read(&b), Some(e));
        // A short body is refused rather than read from whatever follows it.
        assert!(KeyEvent::read(&b[..11]).is_none(), "11 bytes is not a key record");
    }

    #[test]
    fn a_pointer_event_sits_at_the_offsets_the_spec_publishes() {
        // Signed coordinates: a pointer can be dragged past a window's left or top edge,
        // and a client that read them unsigned would see it teleport.
        let e = PointerEvent::new(
            0x4142_4344,
            POINTER_BUTTON,
            0x1112,
            0x2122,
            POINTER_PRESSED,
            MOD_SHIFT | MOD_CTRL,
            -3,
            -4,
        );
        let mut b = [0u8; 24];
        e.write(&mut b).unwrap();
        assert_eq!(&b[0..4], &0x4142_4344u32.to_le_bytes(), "window @0");
        assert_eq!(&b[4..6], &POINTER_BUTTON.to_le_bytes(), "kind @4");
        assert_eq!(&b[6..8], &0x1112u16.to_le_bytes(), "button @6");
        assert_eq!(&b[8..10], &0x2122u16.to_le_bytes(), "buttons @8");
        assert_eq!(&b[10..12], &POINTER_PRESSED.to_le_bytes(), "flags @10");
        assert_eq!(&b[12..14], &(MOD_SHIFT | MOD_CTRL).to_le_bytes(), "modifiers @12");
        assert_eq!(&b[14..16], &0u16.to_le_bytes(), "reserved @14, zero");
        assert_eq!(&b[16..20], &(-3i32).to_le_bytes(), "x @16, signed");
        assert_eq!(&b[20..24], &(-4i32).to_le_bytes(), "y @20, signed");
        assert_eq!(PointerEvent::read(&b), Some(e));
        assert!(PointerEvent::read(&b[..23]).is_none(), "23 bytes is not a pointer record");
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
            desktop: 0x9192_9394,
            flags: 0xA1A2_A3A4,
        };
        let mut b = [0u8; WINDOW_INFO_LEN];
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
        assert_eq!(&b[32..36], &0x9192_9394u32.to_le_bytes(), "desktop @32");
        assert_eq!(&b[36..40], &0xA1A2_A3A4u32.to_le_bytes(), "flags @36");
    }

    #[test]
    fn a_short_info_buffer_is_refused_rather_than_written_partially() {
        // **The growth from 32 to 40 bytes is exactly where a short write becomes plausible.**
        // Every existing caller sized its buffer at 32, and a `write` that filled what it could
        // would hand a reader a window whose desktop and flags are whatever the memory held —
        // for a sticky, minimized window in the worst case, which is invisible *and* everywhere.
        let info = WindowInfo::new(1, Role::Normal, 0, 0, 8, 8);
        let mut small = [0u8; 32];
        assert_eq!(info.write(&mut small), None, "a 32-byte buffer must be refused, not filled");
        let mut exact = [0u8; WINDOW_INFO_LEN];
        assert_eq!(info.write(&mut exact), Some(WINDOW_INFO_LEN));
    }

    #[test]
    fn a_new_window_info_is_not_accidentally_sticky_or_minimized() {
        // `STICKY_DESKTOP` is 0, which is also what an uninitialised field holds — so the
        // constructor's defaults are the one place that reserved value can be shipped by
        // accident. The compositor sets `desktop` from its current one straight after; this
        // pins what the struct itself claims.
        let info = WindowInfo::new(7, Role::Normal, 1, 2, 8, 8);
        assert_eq!(info.desktop, STICKY_DESKTOP);
        assert_eq!(info.flags & WINDOW_FLAG_MINIMIZED, 0);
    }

    #[test]
    fn the_two_manager_request_bodies_round_trip_and_refuse_short_input() {
        let mut b = [0u8; 8];
        MgrWindowValue { window: 0x1112_1314, value: 0x2122_2324 }.write(&mut b).unwrap();
        assert_eq!(&b[0..4], &0x1112_1314u32.to_le_bytes(), "window @0");
        assert_eq!(&b[4..8], &0x2122_2324u32.to_le_bytes(), "value @4");
        assert_eq!(MgrWindowValue::read(&b).unwrap().value, 0x2122_2324);
        assert_eq!(MgrWindowValue::read(&b[..7]), None, "7 bytes must not parse");

        let mut d = [0u8; 4];
        MgrDesktop { desktop: 0x3132_3334 }.write(&mut d).unwrap();
        assert_eq!(&d[0..4], &0x3132_3334u32.to_le_bytes(), "desktop @0");
        assert_eq!(MgrDesktop::read(&d).unwrap().desktop, 0x3132_3334);
        assert_eq!(MgrDesktop::read(&d[..3]), None, "3 bytes must not parse");
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
            assert_eq!(n, WINDOW_INFO_LEN);
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
        let mut buf = [0u8; WINDOW_INFO_LEN];
        WindowInfo::new(1, Role::Normal, 0, 0, 8, 8).write(&mut buf).unwrap();
        // Up to and including 32 — the size this struct was before M8 Part A grew it, and so
        // the length every existing caller's buffer happens to be.
        for short in 0..WINDOW_INFO_LEN {
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

    // ---- drag and drop (M10 Part E) ----

    #[test]
    fn the_three_drop_records_round_trip() {
        let mut b = [0u8; 256];

        let n = DeclareAcceptor { window: 7, kinds: DROP_KIND_FILE }.write(&mut b, b"editor")
            .unwrap();
        let (head, name) = DeclareAcceptor::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kinds, name), (7, DROP_KIND_FILE, "editor"));

        let n = StartDrag { window: 3, kind: DROP_KIND_DIR, path_len: 0 }
            .write(&mut b, b"/home/papers", b"papers")
            .unwrap();
        let (head, path, label) = StartDrag::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kind), (3, DROP_KIND_DIR));
        assert_eq!((path, label), ("/home/papers", "papers"));

        let ev = DroppedEvent {
            window: 9,
            kind: DROP_KIND_FILE,
            x: -4,
            y: 11,
            acceptor_len: 0,
            path_len: 0,
        };
        let n = ev.write(&mut b, b"editor", b"/home/a.txt", b"a.txt").unwrap();
        let (head, acceptor, path, label) = DroppedEvent::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kind, head.x, head.y), (9, DROP_KIND_FILE, -4, 11));
        assert_eq!((acceptor, path, label), ("editor", "/home/a.txt", "a.txt"));
    }

    #[test]
    fn a_drag_carries_exactly_one_kind() {
        // **A payload is one thing.** A drag offering "file or directory" asks whatever it lands
        // on to guess, and an acceptor's mask is the only place a *set* belongs.
        let mut b = [0u8; 64];
        let head = StartDrag { window: 1, kind: DROP_KIND_FILE, path_len: 0 };
        let n = head.write(&mut b, b"/a", b"a").unwrap();
        assert!(StartDrag::read(&b[..n]).is_some());

        for bad in [0u32, DROP_KIND_FILE | DROP_KIND_DIR, 1 << 7] {
            put_u32(&mut b, 4, bad);
            assert!(StartDrag::read(&b[..n]).is_none(), "kind {bad} was accepted");
        }
    }

    #[test]
    fn a_reader_refuses_lengths_a_writer_would_never_produce() {
        // **Hand-built bytes, because a round trip only ever tests the writer.** Every case here
        // is one a correct sender cannot emit and a hostile or broken one can.
        let mut b = [0u8; 64];
        let n = StartDrag { window: 1, kind: DROP_KIND_FILE, path_len: 0 }
            .write(&mut b, b"/a", b"a")
            .unwrap();

        put_u32(&mut b, 8, 0); // a path of nothing
        assert!(StartDrag::read(&b[..n]).is_none());
        put_u32(&mut b, 8, 99); // longer than the body that arrived
        assert!(StartDrag::read(&b[..n]).is_none());
        put_u32(&mut b, 8, u32::MAX); // and the overflow shape of the same mistake
        assert!(StartDrag::read(&b[..n]).is_none());

        let mut d = [0u8; 128];
        let ev = DroppedEvent {
            window: 1,
            kind: DROP_KIND_FILE,
            x: 0,
            y: 0,
            acceptor_len: 0,
            path_len: 0,
        };
        let n = ev.write(&mut d, b"e", b"/a", b"a").unwrap();
        // Each length fits on its own; together they run past the end.
        put_u32(&mut d, 16, 20);
        put_u32(&mut d, 20, 20);
        assert!(DroppedEvent::read(&d[..n]).is_none(), "two lengths that only overflow together");
        // And the sum that overflows the addition itself rather than the buffer.
        put_u32(&mut d, 16, u32::MAX);
        put_u32(&mut d, 20, u32::MAX);
        assert!(DroppedEvent::read(&d[..n]).is_none());
    }

    #[test]
    fn an_acceptor_must_name_itself_and_a_kind_that_exists() {
        let mut b = [0u8; 64];
        let n = DeclareAcceptor { window: 1, kinds: DROP_KINDS_KNOWN }.write(&mut b, b"e").unwrap();
        assert!(DeclareAcceptor::read(&b[..n]).is_some(), "both known kinds is a fine acceptor");

        // No name: nothing for a `Dropped` to say it landed on, and nothing for a port to be.
        assert!(DeclareAcceptor::read(&b[..DeclareAcceptor::HEAD]).is_none());
        // Kinds nobody defines — an acceptor that would be highlighted for drags it cannot take.
        for bad in [0u32, 1 << 9, DROP_KIND_FILE | (1 << 9)] {
            put_u32(&mut b, 4, bad);
            assert!(DeclareAcceptor::read(&b[..n]).is_none(), "kinds {bad} was accepted");
        }
        // A name over the cap is refused rather than truncated: unlike a title, this one is
        // matched against and answered with, so a shortened one names a different acceptor.
        let long = [b'x'; MAX_ACCEPTOR_NAME + 1];
        let mut wide = [0u8; 128];
        assert_eq!(
            DeclareAcceptor { window: 1, kinds: DROP_KIND_FILE }.write(&mut wide, &long),
            None
        );
    }
}

#[cfg(test)]
mod title_tests {
    use super::title::{read, truncate_title, write};
    use super::MAX_TITLE;

    /// The body's own length carries the title's, so a round trip must not need a length field.
    #[test]
    fn a_title_round_trips_without_a_length_field() {
        let mut buf = [0u8; 4 + MAX_TITLE];
        let n = write(7, "nxterm", &mut buf).expect("fits");
        assert_eq!(n, 4 + 6);
        assert_eq!(read(&buf[..n]), Some((7, "nxterm")));
    }

    /// An empty title is a title — a client clearing its name, not a malformed record.
    #[test]
    fn an_empty_title_is_valid() {
        let mut buf = [0u8; 8];
        let n = write(3, "", &mut buf).expect("fits");
        assert_eq!(n, 4);
        assert_eq!(read(&buf[..n]), Some((3, "")));
    }

    /// Fewer than four bytes cannot name a window.
    #[test]
    fn a_body_too_short_for_a_window_id_is_refused() {
        for len in 0..4 {
            assert_eq!(read(&[0u8; 3][..len]), None, "a {len}-byte body parsed");
        }
    }

    /// A title is displayed, so bytes nobody can render are malformed rather than unlucky.
    #[test]
    fn a_title_that_is_not_utf8_is_refused() {
        let body = [1u8, 0, 0, 0, 0xFF, 0xFE];
        assert_eq!(read(&body), None);
    }

    /// The whole point of the cap: slicing at `MAX_TITLE` bytes can land inside a character,
    /// and the result would not be UTF-8 at all — a cap meant to bound memory corrupting the
    /// string it bounds.
    #[test]
    fn truncation_cuts_on_a_character_boundary() {
        // **The fixture is offset by one byte on purpose.** 'é' is two bytes (0xC3 0xA9), so a
        // string of nothing but 'é' has a boundary at byte 256 and a naive `&s[..MAX_TITLE]`
        // would pass this test — which is what a first version of it did, caught by running
        // the control. One leading ASCII byte puts every boundary at an odd offset, so the
        // cap lands *inside* a character and the walk back is the only thing that can work.
        let mut raw = [0u8; 259];
        raw[0] = b'a';
        for pair in raw[1..].chunks_exact_mut(2) {
            pair.copy_from_slice(&[0xC3, 0xA9]);
        }
        let s = core::str::from_utf8(&raw).expect("the fixture is valid UTF-8");
        assert_eq!(s.len(), 259);
        assert!(!s.is_char_boundary(MAX_TITLE), "the fixture does not straddle the cap");
        let kept = truncate_title(s);
        assert!(kept.len() <= MAX_TITLE, "the cap was exceeded");
        assert_eq!(kept.len(), 255, "the straddling character was not dropped whole");
        // It is still a string: the bug this guards would make this line fail to even exist.
        assert!(core::str::from_utf8(kept.as_bytes()).is_ok());
    }

    /// Where the cap falls mid-character, the character goes rather than being split.
    #[test]
    fn a_character_straddling_the_cap_is_dropped_whole() {
        // 255 ASCII bytes, then a two-byte character straddling byte 256.
        let mut raw = [b'a'; 257];
        raw[255] = 0xC3;
        raw[256] = 0xA9;
        let s = core::str::from_utf8(&raw).expect("the fixture is valid UTF-8");
        assert_eq!(s.len(), 257);
        let kept = truncate_title(s);
        assert_eq!(kept.len(), 255, "the straddling character was split at the cap");
        assert!(kept.ends_with('a'));
    }

    /// A title that fits is returned untouched — the negative control for the two above.
    #[test]
    fn a_title_within_the_cap_is_not_touched() {
        let s = "a normal window title";
        assert_eq!(truncate_title(s), s);
        let raw = [b'a'; MAX_TITLE];
        let exact = core::str::from_utf8(&raw).expect("ASCII");
        assert_eq!(truncate_title(exact).len(), MAX_TITLE, "an exactly-sized title was cut");
    }

    /// `write` refuses a buffer it would overrun rather than shortening silently.
    #[test]
    fn write_refuses_a_buffer_it_would_overrun() {
        let mut small = [0u8; 8];
        assert_eq!(write(1, "abcdef", &mut small), None);
        assert_eq!(write(1, "abcd", &mut small), Some(8));
    }
}
