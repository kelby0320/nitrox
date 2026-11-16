//! Creating a window: its role, where it asks to be, and the reply that names it.

use crate::{Reader, put_u16, put_u32};

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
        let mut r = Reader::new(b);
        Some(Self {
            id: r.u32()?,
            width: r.u32()?,
            height: r.u32()?,
            x: r.u32()? as i32,
            y: r.u32()? as i32,
            role: r.u16()?,
            dock: r.u16()?,
            reserve: r.u32()?,
            parent: r.u32()?,
            desktop: r.u32()?,
            flags: r.u32()?,
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
        let mut r = Reader::new(b);
        let (width, height) = (r.u32()?, r.u32()?);
        r.skip(SCREEN_INFO_LEN - 8)?;
        Some(Self { width, height })
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

/// A popup's placement, in its aux16 word: at its offset, as every popup was placed before.
pub const PLACE_AS_ASKED: u16 = 0;
/// A popup's placement: moved inward the least it takes to lie inside the work area.
pub const PLACE_INSIDE: u16 = 1;
/// A popup's placement: flipped about its anchor on each axis it would overhang, then moved inward.
pub const PLACE_AT_POINT: u16 = 2;

/// How the compositor places a popup against the **work area**, the screen less the panels'
/// reservations, which a client does not know (`rsproto-surface-ops.md` § `CreateWindow`).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Placement {
    /// At its offset, clipped and nothing more.
    #[default]
    AsAsked,
    /// Moved inward the least it takes to lie inside: a drop-down.
    Inside,
    /// Flipped about its anchor on each axis it would overhang, then moved inward: a context menu.
    AtPoint,
}

impl Placement {
    /// The wire word.
    pub const fn tag(self) -> u16 {
        match self {
            Placement::AsAsked => PLACE_AS_ASKED,
            Placement::Inside => PLACE_INSIDE,
            Placement::AtPoint => PLACE_AT_POINT,
        }
    }

    /// The placement a wire word names; `None` for one unknown, which a decoder refuses.
    pub const fn from_wire(tag: u16) -> Option<Self> {
        match tag {
            PLACE_AS_ASKED => Some(Placement::AsAsked),
            PLACE_INSIDE => Some(Placement::Inside),
            PLACE_AT_POINT => Some(Placement::AtPoint),
            _ => None,
        }
    }
}

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
    /// How a popup is placed from its offset; only a popup carries one, written zero for the rest.
    pub place: Placement,
}

impl CreateWindowRequest {
    /// A window with no offset — the only meaningful shape for a role that has no parent.
    pub const fn new(width: u32, height: u32, role: Role) -> Self {
        Self { width, height, role, offset_x: 0, offset_y: 0, place: Placement::AsAsked }
    }

    /// A window at `(x, y)` — from its parent's origin for a popup, from the screen's otherwise.
    ///
    /// A `normal` or `dialog` offset is a preference; a `panel` ignores it.
    pub const fn at(width: u32, height: u32, role: Role, x: i32, y: i32) -> Self {
        Self { width, height, role, offset_x: x, offset_y: y, place: Placement::AsAsked }
    }

    /// The same request, its popup placed `how` against the work area.
    pub const fn placed(mut self, how: Placement) -> Self {
        self.place = how;
        self
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
        Role::Popup { parent } => (req.place.tag(), parent),
        Role::Dialog { parent } => (0, parent),
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
    let mut r = Reader::new(body);
    let width = r.u32()?;
    let height = r.u32()?;
    let role_tag = r.u16()?;
    let aux16 = r.u16()?;
    let aux32 = r.u32()?;
    let (offset_x, offset_y) = (r.u32()? as i32, r.u32()? as i32);
    let role = match role_tag {
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
        Role::Popup { .. } | Role::Normal | Role::Dialog { .. } => (offset_x, offset_y),
        Role::Panel { .. } => (0, 0),
    };
    let place = match role {
        Role::Popup { .. } => Placement::from_wire(aux16)?,
        _ => Placement::AsAsked,
    };
    Some(CreateWindowRequest { width, height, role, offset_x, offset_y, place })
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
    Reader::new(body).u32()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(req: &CreateWindowRequest) -> [u8; CREATE_WINDOW_REQUEST_LEN] {
        let mut b = [0xAAu8; CREATE_WINDOW_REQUEST_LEN];
        build_create_window_request(&mut b, req).expect("encodes");
        b
    }

    #[test]
    fn a_popups_placement_rides_in_its_aux16_and_round_trips() {
        for how in [Placement::AsAsked, Placement::Inside, Placement::AtPoint] {
            let req = CreateWindowRequest::at(80, 60, Role::Popup { parent: 7 }, 12, -4).placed(how);
            let b = encoded(&req);
            assert_eq!(u16::from_le_bytes([b[10], b[11]]), how.tag(), "{how:?} is the aux16 word");
            let back = parse_create_window_request(&b).expect("parses");
            assert_eq!((back.place, back.role, back.offset_x, back.offset_y), (how, req.role, 12, -4));
        }
    }

    /// Bytes no encoder here writes: read as "as asked", a menu would leave the screen silently.
    #[test]
    fn a_placement_the_decoder_does_not_know_is_refused() {
        for tag in [3u16, 0x0100, u16::MAX] {
            let mut b = encoded(&CreateWindowRequest::new(80, 60, Role::Popup { parent: 7 }));
            b[10..12].copy_from_slice(&tag.to_le_bytes());
            assert!(parse_create_window_request(&b).is_none(), "placement {tag:#x} was accepted");
        }
    }

    /// A `dialog`'s aux16 stays ignored on read, room for a field of its own.
    #[test]
    fn only_a_popup_carries_a_placement() {
        let inside = |role| encoded(&CreateWindowRequest::new(80, 60, role).placed(Placement::Inside));
        assert_eq!(inside(Role::Normal)[10..12], [0, 0]);
        assert_eq!(inside(Role::Dialog { parent: 7 })[10..12], [0, 0]);
        let mut b = encoded(&CreateWindowRequest::new(80, 60, Role::Dialog { parent: 7 }));
        b[10..12].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(parse_create_window_request(&b).map(|r| r.place), Some(Placement::AsAsked));
    }

    /// Against literal bytes: a client that follows the spec rather than this crate sends these.
    #[test]
    fn the_placements_wire_values_are_the_specs() {
        let popup = || CreateWindowRequest::new(80, 60, Role::Popup { parent: 7 });
        for (word, how) in [(0u16, Placement::AsAsked), (1, Placement::Inside), (2, Placement::AtPoint)] {
            assert_eq!(encoded(&popup().placed(how))[10..12], word.to_le_bytes(), "{how:?} is written {word}");
            let mut b = encoded(&popup());
            b[10..12].copy_from_slice(&word.to_le_bytes());
            assert_eq!(parse_create_window_request(&b).map(|r| r.place), Some(how), "{word} is read as {how:?}");
        }
    }

    #[test]
    fn a_placed_popups_window_created_carries_no_placement() {
        let role = Role::Popup { parent: 7 };
        for how in [Placement::Inside, Placement::AtPoint] {
            assert_ne!(encoded(&CreateWindowRequest::new(80, 60, role).placed(how))[10..12], [0, 0], "precondition");
            assert_eq!(super::super::MgrWindowCreated::for_role(3, role, 80, 60).aux16, 0, "{how:?}");
        }
    }
}
