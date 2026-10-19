//! The `Surface` category (`op = 0x09xx`) bodies — windows and their pixels. See
//! `docs/spec/rsproto-surface-ops.md`.
//!
//! The compositor is bound at `/dev/draw`; a client transfers its buffer handle **once** and
//! then sends [`Commit`](build_commit_request) per frame, the compositor answering
//! [`Release`](build_release_event) (`rsproto-surface-ops.md` § The buffer lifecycle).

use crate::{get_u16, get_u32, put_u16, put_u32};

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

/// `normal` — an ordinary application window.
pub const ROLE_NORMAL: u16 = 0;
/// `panel` — a bar that **never takes keyboard focus** (`rsproto-surface-ops.md` § Window roles).
pub const ROLE_PANEL: u16 = 1;
/// `popup` — a menu or modal, parented and placed by its creator (§ Window roles).
pub const ROLE_POPUP: u16 = 2;
/// `dialog` — parented, **listed**, and placed by a manager (`rsproto-surface-ops.md` § Roles).
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
/// An enum rather than a raw `u16`, so an edge that does not exist is **unrepresentable**.
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
/// Bounded at the protocol edge: two panels each reserving `0x8000_0000` would overflow the
/// compositor's `u32` accumulator (`rsproto-surface-ops.md` § Struts).
pub const MAX_STRUT_RESERVE: u32 = 1 << 16;

/// A window's role, and the extra facts a role carries.
///
/// **Immutable after creation** (`rsproto-surface-ops.md` § Window roles).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Role {
    /// An ordinary application window.
    Normal,
    /// A bar docked to `dock`, reserving `reserve` pixels along that edge.
    ///
    /// **Declared, not derived from geometry** (`rsproto-surface-ops.md` § Struts).
    Panel {
        /// Which screen edge the panel docks to.
        dock: Edge,
        /// Pixels reserved along that edge, subtracted from the area offered to `normal` windows.
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
    pub const fn takes_focus(&self) -> bool {
        !matches!(self, Role::Panel { .. })
    }

    /// Whether this is a popup — a window a press elsewhere dismisses.
    pub const fn is_popup(&self) -> bool {
        matches!(self, Role::Popup { .. })
    }

    /// The edge and size this role reserves, if any.
    pub const fn strut(&self) -> Option<(Edge, u32)> {
        match self {
            Role::Panel { dock, reserve } => Some((*dock, *reserve)),
            _ => None,
        }
    }
}

/// What `/dev/draw/<N>/info` reports about a window.
///
/// Readable by any holder of `/dev/draw`, deliberately (`rsproto-surface-ops.md` § metadata).
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
    pub flags: u32,
}

/// The `desktop` value meaning **every** desktop — a sticky window.
///
/// `0` is what an uninitialised field holds, so a forgotten desktop shows everywhere.
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
        // `desktop` and `flags` are mutable compositor state, not `CreateWindow` parameters.
        Self { id, width, height, x, y, role: role.tag(), dock, reserve, parent, desktop: STICKY_DESKTOP, flags: 0 }
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

/// What `/dev/draw/screen` reports: the size of the screen the compositor draws on.
///
/// Readable by any holder of `/dev/draw` (`rsproto-surface-ops.md` § Reading the screen's size).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScreenInfo {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
}

/// Bytes in a serialised [`ScreenInfo`] — the exact size of a `/dev/draw/screen` object. The last
/// eight are reserved, written as zero and ignored on read, so a later screen fact costs no growth.
pub const SCREEN_INFO_LEN: usize = 16;

impl ScreenInfo {
    /// Serialise into `out`, reserved bytes zeroed; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < SCREEN_INFO_LEN {
            return None;
        }
        put_u32(out, 0, self.width);
        put_u32(out, 4, self.height);
        out[8..SCREEN_INFO_LEN].fill(0);
        Some(SCREEN_INFO_LEN)
    }

    /// Parse from a mapped `screen` object; `None` for a slice shorter than [`SCREEN_INFO_LEN`].
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < SCREEN_INFO_LEN {
            return None;
        }
        Some(Self { width: get_u32(b, 0), height: get_u32(b, 4) })
    }
}

/// Body length of a `CreateWindowRequest`.
pub const CREATE_WINDOW_REQUEST_LEN: usize = 24;
/// Body length of a `CreateWindowReply`.
pub const CREATE_WINDOW_REPLY_LEN: usize = 4;

// **The record's length is a contract**, published in `docs/spec/rsproto-surface-ops.md` and
// read by a second implementation; the offsets below are what the spec table names.
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
    /// Where the window wants to be; zero for a `panel` (`rsproto-surface-ops.md` §
    /// `CreateWindow`). Carried here, not sent afterwards, so a popup's position is atomic with
    /// its existence. **Resolved once, at creation**: tracking the parent is placement policy —
    /// see `TODO(popup-follows-parent)`.
    pub offset_x: i32,
    /// Vertical half of [`offset_x`](Self::offset_x).
    pub offset_y: i32,
}

impl CreateWindowRequest {
    /// A window with no offset — the only meaningful shape for a role that has no parent.
    pub const fn new(width: u32, height: u32, role: Role) -> Self {
        Self { width, height, role, offset_x: 0, offset_y: 0 }
    }

    /// A window at `(x, y)` — from its parent's origin for a popup, from the screen's otherwise.
    ///
    /// A `normal` or `dialog` offset is a preference; a `panel` ignores it.
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
    // Zero for a `panel` only; identical requests must produce identical bytes.
    let (ox, oy) = match req.role {
        Role::Popup { .. } | Role::Normal | Role::Dialog { .. } => (req.offset_x, req.offset_y),
        Role::Panel { .. } => (0, 0),
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
            let dock = Edge::from_wire(aux16)?;
            if aux32 > MAX_STRUT_RESERVE {
                return None;
            }
            Role::Panel { dock, reserve: aux32 }
        }
        ROLE_POPUP => Role::Popup { parent: aux32 },
        ROLE_DIALOG => Role::Dialog { parent: aux32 },
        _ => return None,
    };
    // Read for every role but `panel`, matching the writer: an ignored word is a discarded request.
    let (offset_x, offset_y) = match role {
        Role::Popup { .. } | Role::Normal | Role::Dialog { .. } => (get_u32(body, 16) as i32, get_u32(body, 20) as i32),
        Role::Panel { .. } => (0, 0),
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
    /// Client-chosen buffer id, unique within the window.
    pub buffer: u32,
    /// Buffer width in pixels.
    pub width: u32,
    /// Buffer height in pixels.
    pub height: u32,
    /// Bytes per row. Not `width * 4`: the client may pad rows.
    pub pitch: u32,
    /// Pixel format tag: `0` XRGB8888 or `1` ARGB8888; anything else is rejected.
    pub format: u32,
}

/// Pixel format tag for `0x00RRGGBB`.
pub const SURFACE_FORMAT_XRGB8888: u32 = 0;

/// Pixel format tag for `0xAARRGGBB` — a surface that carries its own opacity.
/// Unknown `format` values were always rejected, so adding this cost no wire change.
pub const SURFACE_FORMAT_ARGB8888: u32 = 1;

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
/// Rejects a pitch too small to hold a row (`rsproto-surface-ops.md` § `AttachBuffer`).
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
    if req.format != SURFACE_FORMAT_XRGB8888 && req.format != SURFACE_FORMAT_ARGB8888 {
        return None;
    }
    if (req.pitch as u64) < req.width as u64 * 4 {
        return None;
    }
    Some(req)
}

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

// Input delivered to a window. **Surface-layer events, not device records**: `libinput` runs
// the device layer's `SYN` state machine on the compositor's side (`display-substrate.md` §5).

/// `Surface::KeyEvent` — a key transition delivered to the focused window.
pub const OP_KEY_EVENT: u16 = 0x0905;
/// `Surface::PointerEvent` — pointer motion, a button, or a crossing.
pub const OP_POINTER_EVENT: u16 = 0x0906;
/// `Surface::FocusEvent` — this window gained or lost the keyboard.
pub const OP_FOCUS_EVENT: u16 = 0x0907;
/// `Surface::Configure` — where and how large the compositor would like this window to be.
///
/// **Server → client, unsolicited, no reply.** A *request*, not a command. The first one is an
/// ordering rule: a client waits for it before its first `Commit` (`rsproto-surface-ops.md`
/// § Configure).
pub const OP_CONFIGURE: u16 = 0x0908;

/// The key was released.
pub const KEY_UP: u16 = 0;
/// The key was pressed.
pub const KEY_DOWN: u16 = 1;
/// The key is still held and the repeat interval elapsed.
pub const KEY_REPEAT: u16 = 2;

/// A key transition, as a window sees it.
///
/// **Modifiers travel with the key** (`rsproto-surface-ops.md` § KeyEvent).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct KeyEvent {
    /// Which window this is about.
    pub window: u32,
    /// The keycode (an `EV_KEY` code from the device layer, unchanged).
    pub keycode: u16,
    /// [`KEY_UP`], [`KEY_DOWN`] or [`KEY_REPEAT`].
    pub pressed: u16,
    /// Modifiers held **at this transition** — see the `MOD_*` constants.
    pub modifiers: u16,
    /// **The locks on at this transition** — [`LOCK_CAPS`], [`LOCK_NUM`]; what
    /// `libinput::keymap::text` reads to give a letter its case and the keypad its digits.
    ///
    /// Not modifier bits: a lock there would break every `modifiers == hotkey` chord match.
    pub locks: u16,
}

const _: () = assert!(core::mem::size_of::<KeyEvent>() == 12);

/// Modifier bits, carried by [`KeyEvent::modifiers`] and [`PointerEvent::modifiers`].
///
/// Left and right share a bit; the **keycode** (`KEY_LEFTSHIFT`, `KEY_RIGHTSHIFT`) stays
/// distinct. A sender must derive these from *which modifier keys are down*, not by clearing
/// a bit on release: with both shifts held, releasing one must leave `MOD_SHIFT` set.
pub const MOD_SHIFT: u16 = 1 << 0;
/// Either control key.
pub const MOD_CTRL: u16 = 1 << 1;
/// Either alt key.
pub const MOD_ALT: u16 = 1 << 2;
/// Either meta/"super" key.
pub const MOD_META: u16 = 1 << 3;

/// Lock bits, carried by [`KeyEvent::locks`]: Caps Lock gives a letter its other case.
pub const LOCK_CAPS: u16 = 1 << 0;
/// Num Lock: the keypad's digits and point are text. On from boot.
pub const LOCK_NUM: u16 = 1 << 1;

/// What a [`PointerEvent`] reports.
pub const POINTER_MOTION: u16 = 0;
/// A button went down or came up; `button` and `flags` say which and which way.
pub const POINTER_BUTTON: u16 = 1;
/// The pointer entered this window.
pub const POINTER_ENTER: u16 = 2;
/// The pointer left this window.
pub const POINTER_LEAVE: u16 = 3;
/// The wheel turned; `wheel` says by how much and which way.
pub const POINTER_WHEEL: u16 = 4;

/// `POINTER_BUTTON` flag: the button went down. Absent means it came up.
pub const POINTER_PRESSED: u16 = 1 << 0;

/// A pointer event, as a window sees it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PointerEvent {
    /// Which window this is about — see [`KeyEvent::window`].
    pub window: u32,
    /// `POINTER_MOTION`, `POINTER_BUTTON`, `POINTER_ENTER` or `POINTER_LEAVE`.
    pub kind: u16,
    /// The button for `POINTER_BUTTON` (a `BTN_*` code), otherwise zero.
    pub button: u16,
    /// Every button currently held, as `BTN_LEFT`→bit 0, `BTN_RIGHT`→bit 1, `BTN_MIDDLE`→bit 2.
    pub buttons: u16,
    /// `POINTER_PRESSED` on a press; zero on a release.
    pub flags: u16,
    /// Modifiers held — `MOD_SHIFT` and friends — on every kind.
    pub modifiers: u16,
    /// Wheel detents on `POINTER_WHEEL`, otherwise zero; **positive is down**, a count.
    pub wheel: i16,
    /// Window-local x.
    pub x: i32,
    /// Window-local y.
    pub y: i32,
    /// Monotonic milliseconds **at the interrupt** that produced this event.
    ///
    /// At the interrupt, **not at delivery**; 64 bits so it never wraps.
    pub time_ms: u64,
}

const _: () = assert!(core::mem::size_of::<PointerEvent>() == 32);

/// This window gained or lost the keyboard.
/// Sent when the answer changes, not on every event that could have changed it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FocusEvent {
    /// Non-zero if this window now has the keyboard.
    pub focused: u16,
    /// Reserved; zero.
    pub _pad: u16,
    /// Which window this is about.
    pub window: u32,
}

//
// A **manager** holds one channel for the whole compositor and speaks the ops below about
// *any* window.
//
// **The capability is the binding, not a check**: `/dev/draw/manage` bounds who may hold one.
// `desktop-shell` binds `/dev/draw/new` alone into every application namespace it builds, so
// `manage` resolves to nothing there (`docs/architecture/graphical-session.md` §3).

/// `Manage::Place` — put a window's top-left corner at an absolute screen position.
pub const OP_MGR_PLACE: u16 = 0x0910;
/// `Manage::Raise` — to the top of the stack.
pub const OP_MGR_RAISE: u16 = 0x0911;
/// `Manage::Lower` — to the bottom of the stack.
pub const OP_MGR_LOWER: u16 = 0x0912;
/// `Manage::RaiseAbove` — directly above another window (alt-tab), not a full raise.
pub const OP_MGR_RAISE_ABOVE: u16 = 0x0913;
/// `Manage::SetFocus` — give a window the keyboard.
pub const OP_MGR_SET_FOCUS: u16 = 0x0914;
/// `Manage::SetWindowDesktop` — move a window to a desktop; [`STICKY_DESKTOP`] for all of them.
pub const OP_MGR_SET_WINDOW_DESKTOP: u16 = 0x0916;
/// `Manage::SetMinimized` — hide a window without moving it off its desktop.
pub const OP_MGR_SET_MINIMIZED: u16 = 0x0917;
/// `Manage::Capture` — scale a window's current contents into a buffer the manager allocated.
pub const OP_MGR_CAPTURE: u16 = 0x0920;

/// `Manage::QueryLayout` — the screen, and the work area every panel's strut leaves.
pub const OP_MGR_QUERY_LAYOUT: u16 = 0x0921;

/// `Manage::LayoutChanged` — a manager event: the work area is not what it was.
///
/// Body: [`MgrLayout`]. Sent when the work area differs from the one last announced.
pub const OP_MGR_LAYOUT_CHANGED: u16 = 0x0922;

/// `Manage::RequestClose` — ask a window's client to close it.
///
/// Request: [`MgrWindowRef`] (`other` unused); `NotFound` if no such window or no session owns
/// it. The polite half (`rsproto-surface-ops.md` § `RequestClose`).
pub const OP_MGR_REQUEST_CLOSE: u16 = 0x0924;

/// `Manage::Close` — destroy a window whose client will not.
///
/// Request: [`MgrWindowRef`] (`other` unused). Removes the window exactly as
/// [`DestroyWindow`](OP_DESTROY_WINDOW) does; the client is not told and its next request
/// naming it is `NotFound` (`rsproto-surface-ops.md` § `Close`).
pub const OP_MGR_CLOSE: u16 = 0x0925;

/// `Manage::WindowStateRequest` — a manager event: a client asked to be minimised or maximised.
///
/// Body: [`WindowState`]. **The manager decides**; the compositor neither applies nor remembers.
pub const OP_MGR_WINDOW_STATE_REQUEST: u16 = 0x0923;

/// `Manage::DragEnded` — **server → manager. Unsolicited, no reply.**
///
/// Body: [`MgrDragEnded`], what a finished gesture asks for and where the window began. One event
/// per gesture, none per motion (`rsproto-surface-ops.md` § `DragEnded`).
pub const OP_MGR_DRAG_ENDED: u16 = 0x0926;
/// `Manage::RegisterHotkey` — route a key chord to the manager instead of the focused window.
///
/// A manager request rather than a client one because any application able to register `Super`
/// could impersonate the launcher. The capability is holding `/dev/draw/manage`.
pub const OP_MGR_REGISTER_HOTKEY: u16 = 0x091E;
/// `Manage::Hotkey` — a registered chord was pressed. Compositor → manager.
pub const OP_MGR_HOTKEY: u16 = 0x091F;
/// How many chords one manager may register.
pub const MAX_HOTKEYS: usize = 16;

/// `Manage::RegisterSnapZone` — a region of the screen, and the rectangle a window dropped in it
///
/// A table the compositor matches during an interactive move; **registering an existing id
/// replaces it** (`rsproto-surface-ops.md` § `RegisterSnapZone`).
pub const OP_MGR_REGISTER_SNAP_ZONE: u16 = 0x0927;

/// How many snap zones the compositor holds at once.
pub const MAX_SNAP_ZONES: usize = 16;
/// `Manage::SetCurrentDesktop` — switch which desktop is composited.
///
/// Numbered outside the `0x0910`–`0x0917` block because it names no window; `0` is refused.
pub const OP_MGR_SET_CURRENT_DESKTOP: u16 = 0x091D;
/// `Manage::SetScheme` — which built-in scheme the compositor's own drawing follows.
///
/// Names no window. See [`MgrScheme`] and `rsproto-surface-ops.md` § `SetScheme`.
pub const OP_MGR_SET_SCHEME: u16 = 0x0928;
/// `Manage::SetWindowState` — tell a window's client the state the manager put it in.
///
/// Body: [`WindowState`], forwarded as [`Surface::WindowState`](OP_WINDOW_STATE).
pub const OP_MGR_SET_WINDOW_STATE: u16 = 0x0929;
/// `Manage::Configure` — ask a window to be a given size and position.
///
/// The manager's half of the [`Configure`](OP_CONFIGURE) a client receives; sent in answer to
/// a [`WindowCreated`](OP_MGR_WINDOW_CREATED) it ends the initial-configure handshake.
pub const OP_MGR_CONFIGURE: u16 = 0x0915;

/// `Manage::WindowCreated` — server → manager, when a window is created.
///
/// Body: [`MgrWindowCreated`], 20 bytes; carries the role and requested geometry.
pub const OP_MGR_WINDOW_CREATED: u16 = 0x0918;
/// `Manage::WindowDestroyed` — server → manager, when a window goes away.
///
/// Body: [`MgrWindowRef`], 8 bytes; `other` is unused and sent as 0.
pub const OP_MGR_WINDOW_DESTROYED: u16 = 0x0919;
/// `Manage::WindowGeometry` — server → manager: this window's position or size changed.
///
/// Sent whenever a window's bounds change, with the *committed* size. Body: [`ConfigureEvent`],
/// 20 bytes.
pub const OP_MGR_WINDOW_GEOMETRY: u16 = 0x091A;
/// `Manage::WindowFocus` — server → manager: the keyboard moved.
///
/// Body: [`FocusEvent`], 8 bytes — the same layout the client-facing `FocusEvent` uses.
pub const OP_MGR_WINDOW_FOCUS: u16 = 0x091B;
/// `Manage::WindowTitle` — server → manager: this window's title changed.
///
/// Body: [`title`]; sent when a client names its window with [`OP_SET_TITLE`].
pub const OP_MGR_WINDOW_TITLE: u16 = 0x091C;

/// `Surface::SetTitle` — a client naming its own window. Client → server, **silent on
/// success**; a malformed body gets the usual error reply.
///
/// Body: [`title`] — the window id, then UTF-8 bytes. Longer than [`MAX_TITLE`] is truncated
/// on a character boundary rather than refused (see [`title::truncate_title`]).
pub const OP_SET_TITLE: u16 = 0x0909;

/// `Surface::StartMove` — hand the compositor an interactive move of this window.
///
/// The only geometry change a client may originate; refused unless the caller holds the pointer
/// grab on that window (`rsproto-surface-ops.md` § `StartMove`).
pub const OP_START_MOVE: u16 = 0x090A;

/// `Surface::RequestState` — ask to be minimised, maximised, or returned to normal.
///
/// The compositor forwards this as [`WindowStateRequest`](OP_MGR_WINDOW_STATE_REQUEST); the
/// reply says it was accepted, **not** that anything happened.
pub const OP_REQUEST_STATE: u16 = 0x090B;

/// [`RequestState`](OP_REQUEST_STATE): the window is neither minimised nor maximised.
pub const WINDOW_STATE_NORMAL: u32 = 0;
/// [`RequestState`](OP_REQUEST_STATE): put the window away, off screen but in the window list.
pub const WINDOW_STATE_MINIMIZED: u32 = 1;
/// [`RequestState`](OP_REQUEST_STATE): fill the work area.
pub const WINDOW_STATE_MAXIMIZED: u32 = 2;

/// `Surface::CloseRequested` — **server → client. Unsolicited, `request_id` 0, no reply.**
///
/// Body, 4 bytes: `window` (u32). **A request that cannot be refused**: a client that ignores
/// it simply stays open (`rsproto-surface-ops.md` § `CloseRequested`).
pub const OP_CLOSE_REQUESTED: u16 = 0x090C;

/// `Surface::StartResize` — hand the compositor an interactive resize of this window.
///
/// **The same shape as [`StartMove`](OP_START_MOVE), refused the same way**; the manager gets
/// the final rectangle ([`DragEnded`](OP_MGR_DRAG_ENDED)) and the compositor never resizes a
/// client (`rsproto-surface-ops.md` § `StartResize`).
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
/// A corner is two bits; opposite edges together, or no edge, are refused.
pub const RESIZE_EDGES: u32 = RESIZE_LEFT | RESIZE_RIGHT | RESIZE_TOP | RESIZE_BOTTOM;

/// `Surface::DeclareAcceptor` — this window takes drops of these kinds, under this name.
///
/// Body: `window` (u32), `kinds` (u32), then the acceptor's name as UTF-8, at most
/// [`MAX_ACCEPTOR_NAME`] bytes (the body's own length gives it). Declared once; bounded per
/// window ([`MAX_ACCEPTORS`]), cleared with the window; re-declaring a name replaces it
/// (`rsproto-surface-ops.md` § `DeclareAcceptor`).
///
/// `InvalidArgument` for an empty name, a name over the cap, one that is not UTF-8, or `kinds`
/// naming nothing this protocol knows. `NotFound` for another session's window.
/// `Unsupported` when the window's table is full.
pub const OP_DECLARE_ACCEPTOR: u16 = 0x090E;

/// `Surface::StartDrag` — the user is dragging this payload out of this window.
///
/// Body: `window` (u32), `kind` (u32), `path_len` (u32), then `path_len` bytes of path followed
/// by the display name, both UTF-8.
///
/// Refused unless the caller holds the pointer grab, as [`StartMove`](OP_START_MOVE) is. **The
/// payload is a path**, not a handle; on release over an acceptor the compositor sends
/// [`Dropped`](OP_DROPPED), elsewhere **nothing** (`rsproto-surface-ops.md` § `StartDrag`).
///
/// `InvalidArgument` for an empty path, one over [`MAX_DROP_PATH`], a name over
/// [`MAX_DROP_NAME`], bytes that are not UTF-8, or a `kind` that is not exactly one known kind.
/// `NotFound` when the caller does not hold the grab, or a move or resize is already running.
pub const OP_START_DRAG: u16 = 0x090F;

/// `Surface::Dropped` — a drag ended over this window, on this acceptor.
///
/// Body: `window` (u32), `kind` (u32), `x` (i32), `y` (i32), `acceptor_len` (u32),
/// `path_len` (u32), then the acceptor's name, the path, and the display name in that order.
///
/// **`x` and `y` are window-local.** **0x0930 rather than 0x0910**: the client block
/// 0x0900–0x090F is full and 0x0910–0x092F is the manager's.
pub const OP_DROPPED: u16 = 0x0930;

/// `Surface::Dismissed` — **server → client. Unsolicited, `request_id` 0, no reply.**
///
/// Body, 4 bytes: `window` (u32). A press landed somewhere that is not this popup. **A
/// request, refusable like [`CloseRequested`](OP_CLOSE_REQUESTED)**: one that ignores it
/// simply stays open.
///
/// Sent for a press anywhere that is not the popup itself, its parent included; the press that
/// *opens* a popup cannot dismiss it; only the topmost popup is told
/// (`rsproto-surface-ops.md` § `Dismissed`).
pub const OP_DISMISSED: u16 = 0x0931;

/// `Surface::WindowState` — **server → client. Unsolicited, `request_id` 0, no reply.**
///
/// Body: [`WindowState`]; a maximise button shows this, not what its click asked for.
pub const OP_WINDOW_STATE: u16 = 0x0932;

/// A drop payload that is one file.
pub const DROP_KIND_FILE: u32 = 1 << 0;
/// A drop payload that is one directory.
pub const DROP_KIND_DIR: u32 = 1 << 1;
/// Every kind this protocol knows; anything else is `InvalidArgument`.
pub const DROP_KINDS_KNOWN: u32 = DROP_KIND_FILE | DROP_KIND_DIR;

/// The longest acceptor name accepted, in bytes.
pub const MAX_ACCEPTOR_NAME: usize = 32;

/// How many acceptors one window may declare.
pub const MAX_ACCEPTORS: usize = 4;

/// The longest path a drag may carry, in bytes.
pub const MAX_DROP_PATH: usize = 512;

/// The longest display name a drag may carry, in bytes.
pub const MAX_DROP_NAME: usize = 64;

/// The largest Surface **event** body, in bytes — what a client must be able to receive.
///
/// Both ends spell the size as this expression, so `Dropped` (the first event over 64 bytes)
/// is not truncated.
pub const MAX_EVENT_BODY: usize = DroppedEvent::HEAD + MAX_ACCEPTOR_NAME + MAX_DROP_PATH + MAX_DROP_NAME;

/// The longest window title accepted, in bytes.
///
/// Bounded at the protocol edge, like [`MAX_STRUT_RESERVE`].
pub const MAX_TITLE: usize = 256;

/// A manager request naming one window and one `u32` —
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
        Some(Self { window: get_u32(b, 0), width: get_u32(b, 4), height: get_u32(b, 8), pitch: get_u32(b, 12) })
    }
}

/// A key chord — the body of both [`RegisterHotkey`](OP_MGR_REGISTER_HOTKEY) and the
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrHotkey {
    /// Manager-chosen identity. **Never zero**: reserved so a zeroed body registers nothing.
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
/// **`0` is not a legal current desktop**: it means sticky, so a current of `0` would blank
/// every non-sticky window and make everything created afterwards silently sticky.
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

/// A manager request naming a scheme — [`SetScheme`](OP_MGR_SET_SCHEME).
///
/// Only the two below are legal; anything else is `Malformed` (`rsproto-surface-ops.md`
/// § `SetScheme`).
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrScheme {
    /// [`SCHEME_LIGHT`] or [`SCHEME_DARK`].
    pub scheme: u32,
}

/// The light scheme, in a [`MgrScheme`].
pub const SCHEME_LIGHT: u32 = 0;
/// The dark scheme, in a [`MgrScheme`].
pub const SCHEME_DARK: u32 = 1;

impl MgrScheme {
    /// Serialise into `out`; returns the length written.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 4 {
            return None;
        }
        put_u32(out, 0, self.scheme);
        Some(4)
    }

    /// Parse from the first 4 bytes of a request body.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 4 {
            return None;
        }
        Some(Self { scheme: get_u32(b, 0) })
    }
}

/// One window, by id — [`CloseRequested`](OP_CLOSE_REQUESTED) and [`Dismissed`](OP_DISMISSED).
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

/// A window and a state: asked for by [`RequestState`](OP_REQUEST_STATE), or told by
/// [`SetWindowState`](OP_MGR_SET_WINDOW_STATE).
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

/// The screen, and the part a maximised window may have ([`QueryLayout`](OP_MGR_QUERY_LAYOUT)).
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
    /// The name is validated here, once: non-empty, within the cap, and UTF-8.
    pub fn read(b: &[u8]) -> Option<(Self, &str)> {
        if b.len() < Self::HEAD {
            return None;
        }
        let name = core::str::from_utf8(&b[Self::HEAD..]).ok()?;
        if name.is_empty() || name.len() > MAX_ACCEPTOR_NAME {
            return None;
        }
        let head = Self { window: get_u32(b, 0), kinds: get_u32(b, 4) };
        // A declaration naming no known kind accepts nothing.
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
    /// **`kind` must be exactly one bit**: a drag carries one payload.
    pub fn read(b: &[u8]) -> Option<(Self, &str, &str)> {
        if b.len() < Self::HEAD {
            return None;
        }
        let head = Self { window: get_u32(b, 0), kind: get_u32(b, 4), path_len: get_u32(b, 8) };
        if head.kind == 0 || head.kind & !DROP_KINDS_KNOWN != 0 || !head.kind.is_power_of_two() {
            return None;
        }
        let len = head.path_len as usize;
        // The declared length must fit what arrived: the reader's half of the writer's bound.
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
    pub fn write(&self, out: &mut [u8], acceptor: &[u8], path: &[u8], name: &[u8]) -> Option<usize> {
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
        // Both lengths checked against what arrived, and their sum for overflow.
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

/// A manager request naming one window and optionally a second: `other` is zero (not a valid
/// id) for `Raise`, `Lower` and `SetFocus`.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrWindowRef {
    /// The window the request is about.
    pub window: u32,
    /// The reference window for `RaiseAbove`; zero otherwise.
    pub other: u32,
}

const _: () = assert!(core::mem::size_of::<MgrWindowRef>() == 8);

/// The **first variable-length Surface record**, shared by [`OP_SET_TITLE`] and
/// [`OP_MGR_WINDOW_TITLE`]: a 4-byte window id then UTF-8, with no length field (the body's
/// own length gives it). The cap is [`MAX_TITLE`].
pub mod title {
    use super::MAX_TITLE;

    /// Parse a title record into `(window, title)`.
    ///
    /// `None` on a body too short to hold a window id, or one whose title is not UTF-8. The
    /// title is **not** truncated; that is [`truncate_title`]'s job at storage.
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
    /// The caller passes `title` through [`truncate_title`] first; this does not shorten.
    pub fn write(window: u32, title: &str, out: &mut [u8]) -> Option<usize> {
        let n = 4 + title.len();
        if out.len() < n {
            return None;
        }
        out[0..4].copy_from_slice(&window.to_le_bytes());
        out[4..n].copy_from_slice(title.as_bytes());
        Some(n)
    }

    /// The longest prefix of `title` that fits in [`MAX_TITLE`] bytes, cut on a character
    /// boundary. **Truncate rather than reject**: [`OP_SET_TITLE`] has no reply a client reads,
    /// and a dropped tail is benign.
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

/// The body of [`DragEnded`](OP_MGR_DRAG_ENDED), 40 bytes: two rectangles of one window.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct MgrDragEnded {
    /// What the gesture asks for, as the [`Configure`](OP_MGR_CONFIGURE) that would grant it.
    pub asked: ConfigureEvent,
    /// The window's rectangle when the gesture began, which a manager cannot keep itself.
    pub from: ConfigureEvent,
}

impl MgrDragEnded {
    /// Serialise into exactly 40 bytes: `asked`, then `from`.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        let n = self.asked.write(out.get_mut(..20)?)?;
        Some(n + self.from.write(out.get_mut(20..40)?)?)
    }

    /// Parse from 40 bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        Some(Self { asked: ConfigureEvent::read(b.get(..20)?)?, from: ConfigureEvent::read(b.get(20..40)?)? })
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
    pub const fn new(window: u32, keycode: u16, pressed: u16, modifiers: u16) -> Self {
        Self { window, keycode, pressed, modifiers, locks: 0 }
    }

    /// This record with `locks` on.
    pub const fn with_locks(self, locks: u16) -> Self {
        Self { locks, ..self }
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
        out[10..12].copy_from_slice(&self.locks.to_le_bytes());
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
            locks: u16::from_le_bytes([b[10], b[11]]),
        })
    }
}

impl PointerEvent {
    /// A pointer record for `window` at window-local `(x, y)`, `time_ms` into the clock.
    #[allow(clippy::too_many_arguments, reason = "a pointer event's fields, in wire order")]
    pub const fn new(
        window: u32,
        kind: u16,
        button: u16,
        buttons: u16,
        flags: u16,
        modifiers: u16,
        x: i32,
        y: i32,
        time_ms: u64,
    ) -> Self {
        Self { window, kind, button, buttons, flags, modifiers, wheel: 0, x, y, time_ms }
    }

    /// The same record, carrying `wheel` detents.
    pub const fn with_wheel(self, wheel: i16) -> Self {
        Self { wheel, ..self }
    }

    /// Serialise into exactly 32 little-endian bytes.
    pub fn write(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 32 {
            return None;
        }
        out[0..4].copy_from_slice(&self.window.to_le_bytes());
        out[4..6].copy_from_slice(&self.kind.to_le_bytes());
        out[6..8].copy_from_slice(&self.button.to_le_bytes());
        out[8..10].copy_from_slice(&self.buttons.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..14].copy_from_slice(&self.modifiers.to_le_bytes());
        out[14..16].copy_from_slice(&self.wheel.to_le_bytes());
        out[16..20].copy_from_slice(&self.x.to_le_bytes());
        out[20..24].copy_from_slice(&self.y.to_le_bytes());
        out[24..32].copy_from_slice(&self.time_ms.to_le_bytes());
        Some(32)
    }

    /// Parse from exactly 32 little-endian bytes.
    pub fn read(b: &[u8]) -> Option<Self> {
        if b.len() < 32 {
            return None;
        }
        Some(Self {
            window: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            kind: u16::from_le_bytes([b[4], b[5]]),
            button: u16::from_le_bytes([b[6], b[7]]),
            buttons: u16::from_le_bytes([b[8], b[9]]),
            flags: u16::from_le_bytes([b[10], b[11]]),
            modifiers: u16::from_le_bytes([b[12], b[13]]),
            wheel: i16::from_le_bytes([b[14], b[15]]),
            x: i32::from_le_bytes([b[16], b[17], b[18], b[19]]),
            y: i32::from_le_bytes([b[20], b[21], b[22], b[23]]),
            time_ms: u64::from_le_bytes([b[24], b[25], b[26], b[27], b[28], b[29], b[30], b[31]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_ended_reads_the_asked_rectangle_then_the_one_it_came_from() {
        // By hand rather than through `write`: a round trip would pass a reader that swapped them.
        let mut b = [0u8; 40];
        for (i, v) in [7u32, 1360, 708, 0, 30, 7, 596, 505, 24, 78].iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        let d = MgrDragEnded::read(&b).expect("40 bytes");
        assert_eq!(d.asked, ConfigureEvent { window: 7, width: 1360, height: 708, x: 0, y: 30 });
        assert_eq!(d.from, ConfigureEvent { window: 7, width: 596, height: 505, x: 24, y: 78 });
        assert_eq!(MgrDragEnded::read(&b[..39]), None, "the old 20-byte body, or a short one");
        let mut out = [0u8; 40];
        assert_eq!(d.write(&mut out), Some(40));
        assert_eq!(out, b);
        assert_eq!(d.write(&mut out[..39]), None);
    }

    #[test]
    fn a_panel_discards_an_offset_and_the_other_roles_carry_it() {
        let mut buf = [0xAAu8; CREATE_WINDOW_REQUEST_LEN];
        let panel = Role::Panel { dock: Edge::Top, reserve: 4 };
        build_create_window_request(&mut buf, &CreateWindowRequest::at(8, 8, panel, 77, 88)).expect("encodes");
        assert_eq!(&buf[16..24], &[0u8; 8], "a panel put an offset on the wire");

        // Then put an offset there by hand: parsing the buffer the correct encoder just zeroed
        // asserts nothing, so the reader needs a body that would betray it.
        put_u32(&mut buf, 16, 77);
        put_u32(&mut buf, 20, 88);
        let back = parse_create_window_request(&buf).expect("parses");
        assert_eq!((back.offset_x, back.offset_y), (0, 0), "a panel took an offset that was on the wire");

        for role in [Role::Normal, Role::Dialog { parent: 9 }] {
            let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
            build_create_window_request(&mut buf, &CreateWindowRequest::at(8, 8, role, 77, -88)).expect("encodes");
            let back = parse_create_window_request(&buf).expect("parses");
            assert_eq!((back.offset_x, back.offset_y), (77, -88), "{role:?} lost the origin it asked for");
        }

        let mut buf = [0u8; CREATE_WINDOW_REQUEST_LEN];
        let req = CreateWindowRequest::at(8, 8, Role::Popup { parent: 3 }, 77, -88);
        build_create_window_request(&mut buf, &req).expect("encodes");
        let back = parse_create_window_request(&buf).expect("parses");
        assert_eq!((back.offset_x, back.offset_y), (77, -88), "a popup's offset survives");
    }

    /// `for_role` puts the role in the same two aux words `CreateWindowRequest` does, for
    /// **every** role: the two encoders are separate code with the same match arms, and every
    /// gate creates `Role::Normal`, where all branches agree on `(0, 0)`.
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
            build_create_window_request(&mut req, &CreateWindowRequest::new(640, 480, role)).expect("request encodes");

            let ev = MgrWindowCreated::for_role(3, role, 640, 480);
            assert_eq!(ev.role, get_u16(&req, 8), "role tag for {role:?}");
            assert_eq!(ev.aux16, get_u16(&req, 10), "aux16 for {role:?}");
            assert_eq!(ev.aux32, get_u32(&req, 12), "aux32 for {role:?}");

            let mut body = [0u8; 20];
            let n = ev.write(&mut body).expect("event encodes");
            let back = MgrWindowCreated::read(&body[..n]).expect("event decodes");
            let parsed = parse_create_window_request(&req).expect("request parses");
            assert_eq!(parsed.role, role, "the request round-trips to the same role");
            assert_eq!((back.role, back.aux16, back.aux32), (ev.role, ev.aux16, ev.aux32));
            assert_eq!((back.width, back.height), (640, 480));
        }
    }

    #[test]
    fn mgr_window_created_round_trips_including_the_role_aux_fields() {
        let cases = [
            MgrWindowCreated { window: 1, role: ROLE_NORMAL, aux16: 0, aux32: 0, width: 64, height: 32 },
            MgrWindowCreated { window: 4096, role: ROLE_PANEL, aux16: 3, aux32: 28, width: 1280, height: 28 },
            MgrWindowCreated { window: 7, role: ROLE_POPUP, aux16: 0, aux32: 12, width: 180, height: 96 },
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
        let mut buf = [0xAAu8; CREATE_WINDOW_REQUEST_LEN];
        let req = CreateWindowRequest::new(4, 4, Role::Normal);
        build_create_window_request(&mut buf, &req).unwrap();
        // **10..24, not 10..16**: the offset is meaningless without a parent, so a `normal`
        // request that left those bytes alone would put the caller's stack contents on the wire.
        assert_eq!(&buf[10..24], &[0u8; 14]);
    }

    #[test]
    fn attach_accepts_the_two_known_formats_and_refuses_the_rest() {
        // **The bytes are edited directly, not built**: a builder only emits tags it knows, so
        // round-tripping one would leave the decoder's rejection unexercised. Byte 20 is the
        // format word.
        let mut buf = [0u8; ATTACH_BUFFER_REQUEST_LEN];
        build_attach_buffer_request(
            &mut buf,
            &AttachBufferRequest {
                window: 1,
                buffer: 0,
                width: 64,
                height: 32,
                pitch: 64 * 4,
                format: SURFACE_FORMAT_XRGB8888,
            },
        )
        .unwrap();

        for tag in [SURFACE_FORMAT_XRGB8888, SURFACE_FORMAT_ARGB8888] {
            buf[20..24].copy_from_slice(&tag.to_le_bytes());
            let got = parse_attach_buffer_request(&buf).expect("a known format is accepted");
            assert_eq!(got.format, tag, "the tag must survive the parse, not be normalised");
        }
        for tag in [2u32, 3, 0xFFFF_FFFF, 0x8000_0000] {
            buf[20..24].copy_from_slice(&tag.to_le_bytes());
            assert!(parse_attach_buffer_request(&buf).is_none(), "format {tag} was accepted");
        }
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
        let req = AttachBufferRequest { window: 1, buffer: 0, width: 8, height: 8, pitch: 32, format: 7 };
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
        assert_eq!(&b[10..12], &0u16.to_le_bytes(), "locks @10, none");
        assert_eq!(KeyEvent::read(&b), Some(e));
        assert!(KeyEvent::read(&b[..11]).is_none(), "11 bytes is not a key record");
        let locked = e.with_locks(LOCK_CAPS | LOCK_NUM);
        locked.write(&mut b).unwrap();
        assert_eq!(&b[10..12], &3u16.to_le_bytes(), "locks @10");
        assert_eq!(KeyEvent::read(&b), Some(locked));
        assert_eq!(&b[8..10], &0x3132u16.to_le_bytes(), "the modifiers untouched by them");
    }

    #[test]
    fn a_pointer_event_sits_at_the_offsets_the_spec_publishes() {
        let e = PointerEvent::new(
            0x4142_4344,
            POINTER_BUTTON,
            0x1112,
            0x2122,
            POINTER_PRESSED,
            MOD_SHIFT | MOD_CTRL,
            -3,
            -4,
            0x0102_0304_0506_0708,
        )
        .with_wheel(-2);
        let mut b = [0u8; 32];
        e.write(&mut b).unwrap();
        assert_eq!(&b[0..4], &0x4142_4344u32.to_le_bytes(), "window @0");
        assert_eq!(&b[4..6], &POINTER_BUTTON.to_le_bytes(), "kind @4");
        assert_eq!(&b[6..8], &0x1112u16.to_le_bytes(), "button @6");
        assert_eq!(&b[8..10], &0x2122u16.to_le_bytes(), "buttons @8");
        assert_eq!(&b[10..12], &POINTER_PRESSED.to_le_bytes(), "flags @10");
        assert_eq!(&b[12..14], &(MOD_SHIFT | MOD_CTRL).to_le_bytes(), "modifiers @12");
        assert_eq!(&b[14..16], &(-2i16).to_le_bytes(), "wheel @14, signed");
        assert_eq!(&b[16..20], &(-3i32).to_le_bytes(), "x @16, signed");
        assert_eq!(&b[20..24], &(-4i32).to_le_bytes(), "y @20, signed");
        assert_eq!(&b[24..32], &0x0102_0304_0506_0708u64.to_le_bytes(), "time_ms @24");
        assert_eq!(PointerEvent::read(&b), Some(e));
        assert!(PointerEvent::read(&b[..31]).is_none(), "31 bytes is not a pointer record");
    }

    #[test]
    fn a_pointer_record_reads_its_wheel_and_its_clock_from_the_wire() {
        let mut b = [0u8; 32];
        b[4..6].copy_from_slice(&POINTER_WHEEL.to_le_bytes());
        b[14..16].copy_from_slice(&(-1i16).to_le_bytes());
        b[24..32].copy_from_slice(&12_345u64.to_le_bytes());
        let e = PointerEvent::read(&b).expect("32 bytes is a pointer record");
        assert_eq!(e.kind, POINTER_WHEEL);
        assert_eq!(e.wheel, -1, "the wheel is signed on the way in as well as out");
        assert_eq!(e.time_ms, 12_345);
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
        let info = WindowInfo::new(1, Role::Normal, 0, 0, 8, 8);
        let mut small = [0u8; 32];
        assert_eq!(info.write(&mut small), None, "a 32-byte buffer must be refused, not filled");
        let mut exact = [0u8; WINDOW_INFO_LEN];
        assert_eq!(info.write(&mut exact), Some(WINDOW_INFO_LEN));
    }

    #[test]
    fn a_new_window_info_is_not_accidentally_sticky_or_minimized() {
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

        let mut sc = [0u8; 4];
        MgrScheme { scheme: SCHEME_DARK }.write(&mut sc).unwrap();
        assert_eq!(&sc, &1u32.to_le_bytes(), "scheme @0");
        assert_eq!(MgrScheme::read(&sc).unwrap().scheme, SCHEME_DARK);
        assert_eq!(MgrScheme::read(&sc[..3]), None, "3 bytes must not parse");
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
        for short in 0..WINDOW_INFO_LEN {
            assert!(WindowInfo::read(&buf[..short]).is_none(), "len {short}");
        }
        assert!(WindowInfo::read(&buf).is_some());
    }

    #[test]
    fn screen_info_sits_at_the_offsets_the_spec_publishes_and_zeroes_the_rest() {
        let mut buf = [0xAAu8; SCREEN_INFO_LEN];
        assert_eq!(ScreenInfo { width: 1360, height: 768 }.write(&mut buf), Some(SCREEN_INFO_LEN));
        assert_eq!(&buf[0..4], &1360u32.to_le_bytes());
        assert_eq!(&buf[4..8], &768u32.to_le_bytes());
        assert_eq!(&buf[8..], &[0u8; 8], "the reserved bytes are written, not left as they were");
        assert_eq!(ScreenInfo::default().write(&mut [0u8; SCREEN_INFO_LEN - 1]), None);
    }

    #[test]
    fn a_screen_info_reader_ignores_the_reserved_bytes_and_refuses_a_short_object() {
        let mut bytes = [0xFFu8; SCREEN_INFO_LEN];
        bytes[0..4].copy_from_slice(&1024u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&768u32.to_le_bytes());
        assert_eq!(ScreenInfo::read(&bytes), Some(ScreenInfo { width: 1024, height: 768 }));
        for short in 0..SCREEN_INFO_LEN {
            assert_eq!(ScreenInfo::read(&bytes[..short]), None, "len {short}");
        }
    }

    #[test]
    fn commit_and_release_round_trip() {
        let mut buf = [0u8; 32];
        let c = CommitRequest { window: 3, buffer: 1, damage_x: 4, damage_y: 5, damage_w: 16, damage_h: 9 };
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
        let c = CommitRequest { window: 1, buffer: 0, damage_x: 0, damage_y: 0, damage_w: 1, damage_h: 1 };
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

    #[test]
    fn the_three_drop_records_round_trip() {
        let mut b = [0u8; 256];

        let n = DeclareAcceptor { window: 7, kinds: DROP_KIND_FILE }.write(&mut b, b"editor").unwrap();
        let (head, name) = DeclareAcceptor::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kinds, name), (7, DROP_KIND_FILE, "editor"));

        let n = StartDrag { window: 3, kind: DROP_KIND_DIR, path_len: 0 }
            .write(&mut b, b"/home/papers", b"papers")
            .unwrap();
        let (head, path, label) = StartDrag::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kind), (3, DROP_KIND_DIR));
        assert_eq!((path, label), ("/home/papers", "papers"));

        let ev = DroppedEvent { window: 9, kind: DROP_KIND_FILE, x: -4, y: 11, acceptor_len: 0, path_len: 0 };
        let n = ev.write(&mut b, b"editor", b"/home/a.txt", b"a.txt").unwrap();
        let (head, acceptor, path, label) = DroppedEvent::read(&b[..n]).unwrap();
        assert_eq!((head.window, head.kind, head.x, head.y), (9, DROP_KIND_FILE, -4, 11));
        assert_eq!((acceptor, path, label), ("editor", "/home/a.txt", "a.txt"));
    }

    #[test]
    fn a_drag_carries_exactly_one_kind() {
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
        let mut b = [0u8; 64];
        let n = StartDrag { window: 1, kind: DROP_KIND_FILE, path_len: 0 }.write(&mut b, b"/a", b"a").unwrap();

        put_u32(&mut b, 8, 0); // a path of nothing
        assert!(StartDrag::read(&b[..n]).is_none());
        put_u32(&mut b, 8, 99); // longer than the body that arrived
        assert!(StartDrag::read(&b[..n]).is_none());
        put_u32(&mut b, 8, u32::MAX); // and the overflow shape of the same mistake
        assert!(StartDrag::read(&b[..n]).is_none());

        let mut d = [0u8; 128];
        let ev = DroppedEvent { window: 1, kind: DROP_KIND_FILE, x: 0, y: 0, acceptor_len: 0, path_len: 0 };
        let n = ev.write(&mut d, b"e", b"/a", b"a").unwrap();
        put_u32(&mut d, 16, 20);
        put_u32(&mut d, 20, 20);
        assert!(DroppedEvent::read(&d[..n]).is_none(), "two lengths that only overflow together");
        put_u32(&mut d, 16, u32::MAX);
        put_u32(&mut d, 20, u32::MAX);
        assert!(DroppedEvent::read(&d[..n]).is_none());
    }

    #[test]
    fn an_acceptor_must_name_itself_and_a_kind_that_exists() {
        let mut b = [0u8; 64];
        let n = DeclareAcceptor { window: 1, kinds: DROP_KINDS_KNOWN }.write(&mut b, b"e").unwrap();
        assert!(DeclareAcceptor::read(&b[..n]).is_some(), "both known kinds is a fine acceptor");

        assert!(DeclareAcceptor::read(&b[..DeclareAcceptor::HEAD]).is_none());
        for bad in [0u32, 1 << 9, DROP_KIND_FILE | (1 << 9)] {
            put_u32(&mut b, 4, bad);
            assert!(DeclareAcceptor::read(&b[..n]).is_none(), "kinds {bad} was accepted");
        }
        // A name over the cap is refused rather than truncated: unlike a title, this one is
        // matched against and answered with, so a shortened one names a different acceptor.
        let long = [b'x'; MAX_ACCEPTOR_NAME + 1];
        let mut wide = [0u8; 128];
        assert_eq!(DeclareAcceptor { window: 1, kinds: DROP_KIND_FILE }.write(&mut wide, &long), None);
    }
}

#[cfg(test)]
mod title_tests {
    use super::MAX_TITLE;
    use super::title::{read, truncate_title, write};

    #[test]
    fn a_title_round_trips_without_a_length_field() {
        let mut buf = [0u8; 4 + MAX_TITLE];
        let n = write(7, "nxterm", &mut buf).expect("fits");
        assert_eq!(n, 4 + 6);
        assert_eq!(read(&buf[..n]), Some((7, "nxterm")));
    }

    #[test]
    fn an_empty_title_is_valid() {
        let mut buf = [0u8; 8];
        let n = write(3, "", &mut buf).expect("fits");
        assert_eq!(n, 4);
        assert_eq!(read(&buf[..n]), Some((3, "")));
    }

    #[test]
    fn a_body_too_short_for_a_window_id_is_refused() {
        for len in 0..4 {
            assert_eq!(read(&[0u8; 3][..len]), None, "a {len}-byte body parsed");
        }
    }

    #[test]
    fn a_title_that_is_not_utf8_is_refused() {
        let body = [1u8, 0, 0, 0, 0xFF, 0xFE];
        assert_eq!(read(&body), None);
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary() {
        // The fixture is offset by one byte: 'é' is two bytes, so a string of only 'é' has a
        // boundary at byte 256 and a naive `&s[..MAX_TITLE]` would pass.
        let mut raw = [0u8; 259];
        raw[0] = b'a';
        for pair in raw[1..].as_chunks_mut::<2>().0 {
            pair.copy_from_slice(&[0xC3, 0xA9]);
        }
        let s = core::str::from_utf8(&raw).expect("the fixture is valid UTF-8");
        assert_eq!(s.len(), 259);
        assert!(!s.is_char_boundary(MAX_TITLE), "the fixture does not straddle the cap");
        let kept = truncate_title(s);
        assert!(kept.len() <= MAX_TITLE, "the cap was exceeded");
        assert_eq!(kept.len(), 255, "the straddling character was not dropped whole");
        assert!(core::str::from_utf8(kept.as_bytes()).is_ok());
    }

    #[test]
    fn a_character_straddling_the_cap_is_dropped_whole() {
        let mut raw = [b'a'; 257];
        raw[255] = 0xC3;
        raw[256] = 0xA9;
        let s = core::str::from_utf8(&raw).expect("the fixture is valid UTF-8");
        assert_eq!(s.len(), 257);
        let kept = truncate_title(s);
        assert_eq!(kept.len(), 255, "the straddling character was split at the cap");
        assert!(kept.ends_with('a'));
    }

    #[test]
    fn a_title_within_the_cap_is_not_touched() {
        let s = "a normal window title";
        assert_eq!(truncate_title(s), s);
        let raw = [b'a'; MAX_TITLE];
        let exact = core::str::from_utf8(&raw).expect("ASCII");
        assert_eq!(truncate_title(exact).len(), MAX_TITLE, "an exactly-sized title was cut");
    }

    #[test]
    fn write_refuses_a_buffer_it_would_overrun() {
        let mut small = [0u8; 8];
        assert_eq!(write(1, "abcdef", &mut small), None);
        assert_eq!(write(1, "abcd", &mut small), Some(8));
    }
}
