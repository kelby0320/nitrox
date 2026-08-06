//! The compositor's window model — the stack, roles and struts, commits, and compositing.
//!
//! **No syscalls in here.** This is the same split `fs-server-ext4` uses for its ext4
//! parser and `nxsh` for its evaluator: the logic that decides *what the screen should
//! look like* is a pure function of the window stack, and the parts that touch the OS
//! (IPC, mapped buffers, `/dev/draw`) live in the server bin. Compositing is asserted
//! pixel-exactly on the host in milliseconds rather than through a 90-second boot.
//!
//! Pixels come in through [`BufferSource`] rather than being owned here. In the server
//! those bytes are a `MemoryObject` the client drew into and shared **once**; owning them
//! would mean a copy per frame and would defeat the shared-memory design the substrate
//! doc chose (`docs/design/display-substrate.md` §4).
//!
//! ## Scope
//!
//! Plan Milestone 2 Part A. Window creation, buffer attachment, commit, release, destroy,
//! and stacking — plus roles and struts, which land here rather than later because
//! "retrofitting a role into a shipped protocol touches every client".

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod server;

use alloc::vec::Vec;

use libdraw::compose::{SurfaceRef, compose};
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry};
use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Role,
};

/// Where a window's pixels are, for a given (window, buffer) pair.
///
/// The seam that keeps this crate free of syscalls. Tests back it with owned vectors; the
/// server backs it with mapped shared memory.
pub trait BufferSource {
    /// The bytes of `buffer` on `window`, or `None` if it is not mapped here.
    fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]>;
}

/// A buffer a client has attached to a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachedBuffer {
    /// Client-chosen id, unique within the window.
    pub id: u32,
    /// Shape of the buffer's pixels.
    pub geometry: Geometry,
}

/// A window: an id, a fixed role, a position, and the buffer currently on screen.
#[derive(Clone, Debug)]
pub struct Window {
    /// Compositor-assigned id, unique for the life of the compositor.
    pub id: u32,
    /// Fixed at creation — see [`Role`].
    pub role: Role,
    /// Top-left corner in screen coordinates.
    pub origin: Point,
    /// Requested size. The committed buffer's geometry is what actually gets drawn.
    pub size: (u32, u32),
    /// Buffers the client has attached.
    pub buffers: Vec<AttachedBuffer>,
    /// The buffer last committed, if any — what compositing reads.
    pub committed: Option<u32>,
}

impl Window {
    /// The window's bounds in screen space, from its committed buffer if there is one.
    pub fn bounds(&self) -> Rect {
        let (w, h) = self
            .committed
            .and_then(|id| self.buffers.iter().find(|b| b.id == id))
            .map(|b| (b.geometry.width, b.geometry.height))
            .unwrap_or(self.size);
        Rect::new(self.origin.x, self.origin.y, w, h)
    }
}

/// What went wrong applying a client request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackError {
    /// No window with that id.
    NoSuchWindow,
    /// No buffer with that id on that window.
    NoSuchBuffer,
    /// A popup or dialog named a parent that does not exist.
    NoSuchParent,
    /// The buffer's geometry is not one this compositor can read.
    BadGeometry,
    /// A buffer id already attached to this window.
    DuplicateBuffer,
}

/// The compositor's window stack: bottom-first order, plus the id allocator.
pub struct WindowStack {
    windows: Vec<Window>,
    next_id: u32,
}

impl Default for WindowStack {
    /// Delegates to [`WindowStack::new`].
    ///
    /// **Not `#[derive(Default)]`.** That would start `next_id` at 0 while `new()` starts
    /// at 1, so the two constructors would hand out different id spaces — and a client
    /// using 0 as a "no parent" sentinel would get `Popup { parent: 0 }` *accepted* on a
    /// derived stack where `new()` returns `NoSuchParent`.
    fn default() -> Self {
        Self::new()
    }
}

impl WindowStack {
    /// An empty stack.
    pub fn new() -> Self {
        Self { windows: Vec::new(), next_id: 1 }
    }

    /// Windows, bottom-first.
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// The window with `id`, if it exists.
    pub fn window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// Create a window and return its id.
    ///
    /// A popup or dialog must name a parent that exists — otherwise the compositor would
    /// be holding a transient window with nothing to be transient to, and stacking it
    /// would be undefined.
    pub fn create(&mut self, req: &CreateWindowRequest) -> Result<u32, StackError> {
        if let Role::Popup { parent } | Role::Dialog { parent } = req.role
            && self.window(parent).is_none()
        {
            return Err(StackError::NoSuchParent);
        }
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window {
            id,
            role: req.role,
            origin: Point::new(0, 0),
            size: (req.width, req.height),
            buffers: Vec::new(),
            committed: None,
        });
        Ok(id)
    }

    /// Move a window's top-left corner.
    pub fn set_origin(&mut self, id: u32, origin: Point) -> Result<(), StackError> {
        let w = self.windows.iter_mut().find(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        w.origin = origin;
        Ok(())
    }

    /// Attach a buffer to a window.
    pub fn attach(&mut self, req: &AttachBufferRequest) -> Result<(), StackError> {
        let geometry =
            Geometry::with_pitch(req.width, req.height, req.pitch as usize, PixelFormat::XRGB8888)
                .ok_or(StackError::BadGeometry)?;
        let w = self
            .windows
            .iter_mut()
            .find(|w| w.id == req.window)
            .ok_or(StackError::NoSuchWindow)?;
        if w.buffers.iter().any(|b| b.id == req.buffer) {
            return Err(StackError::DuplicateBuffer);
        }
        w.buffers.push(AttachedBuffer { id: req.buffer, geometry });
        Ok(())
    }

    /// Apply a commit: the named buffer becomes what compositing reads.
    ///
    /// Returns the buffer that was on screen before, if any — **that is the one to
    /// release**. Releasing the newly committed buffer instead would hand the client back
    /// the memory the compositor is about to read, which is the tearing this protocol
    /// exists to prevent.
    pub fn commit(&mut self, req: &CommitRequest) -> Result<Option<u32>, StackError> {
        let w = self
            .windows
            .iter_mut()
            .find(|w| w.id == req.window)
            .ok_or(StackError::NoSuchWindow)?;
        if !w.buffers.iter().any(|b| b.id == req.buffer) {
            return Err(StackError::NoSuchBuffer);
        }
        let previous = w.committed;
        w.committed = Some(req.buffer);
        // Re-committing the same buffer releases nothing: the client already knows it owns
        // no other buffer, and reporting a release here would let it draw into the buffer
        // now on screen.
        Ok(previous.filter(|&p| p != req.buffer))
    }

    /// Destroy a window and everything attached to it.
    pub fn destroy(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        self.windows.remove(i);
        // Descendants cannot outlive an ancestor: a popup or dialog with no parent has no
        // defined desktop or stacking position, and `create` refuses to produce one.
        //
        // **Transitively.** A single pass over direct children leaves a submenu — a popup
        // parented to a popup, which is the canonical nested case — alive with a dead
        // parent, still compositing and still eligible for focus after the menu chain it
        // belonged to was closed. Repeat until nothing more is orphaned.
        loop {
            let live: Vec<u32> = self.windows.iter().map(|w| w.id).collect();
            let before = self.windows.len();
            self.windows.retain(|w| match w.role {
                Role::Popup { parent } | Role::Dialog { parent } => live.contains(&parent),
                _ => true,
            });
            if self.windows.len() == before {
                break;
            }
        }
        Ok(())
    }

    /// Raise a window to the top of the stack.
    pub fn raise(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        let w = self.windows.remove(i);
        self.windows.push(w);
        Ok(())
    }

    /// The area left for `normal` windows after every panel's reservation.
    ///
    /// This is what "a maximised window must not cover the bars" means concretely
    /// (`display-substrate.md` §4a). Reservations are **declared**, not derived from a
    /// panel's geometry: the two differ when a window is fullscreen (it covers the
    /// panel's pixels while the panel still reserves that space for maximised windows).
    ///
    /// Over-reservation is clamped rather than allowed to invert the rectangle: panels
    /// claiming more than the screen leave an empty work area, not a negative one.
    pub fn work_area(&self, screen: Rect) -> Rect {
        let (mut top, mut bottom, mut left, mut right) = (0u32, 0u32, 0u32, 0u32);
        for w in &self.windows {
            if let Some((edge, reserve)) = w.role.strut() {
                // Saturating, not `+=`. `reserve` is bounded at the protocol edge, but the
                // *sum* over many panels is not, and a wrap here is worse than a panic: in
                // release it returns the full screen as the work area, silently defeating
                // the clamp below. No catch-all arm — `Edge` is an enum, so a new edge is a
                // compile error rather than a silently ignored reservation.
                let slot = match edge {
                    Edge::Top => &mut top,
                    Edge::Bottom => &mut bottom,
                    Edge::Left => &mut left,
                    Edge::Right => &mut right,
                };
                *slot = slot.saturating_add(reserve);
            }
        }
        let horizontal = left.saturating_add(right);
        let vertical = top.saturating_add(bottom);
        let w = screen.size.w.saturating_sub(horizontal);
        let h = screen.size.h.saturating_sub(vertical);
        Rect::new(
            screen.origin.x + left.min(screen.size.w) as i32,
            screen.origin.y + top.min(screen.size.h) as i32,
            w,
            h,
        )
    }

    /// The `info` a window reports at `/dev/draw/<id>/info`.
    ///
    /// Reports the **committed** buffer's size when there is one, because that is what is
    /// actually on screen; the requested size is only a hint until a client commits.
    pub fn info(&self, id: u32) -> Option<librsproto::surface::WindowInfo> {
        let w = self.window(id)?;
        let bounds = w.bounds();
        Some(librsproto::surface::WindowInfo::new(
            w.id,
            w.role,
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.w,
            bounds.size.h,
        ))
    }

    /// The topmost window that may take keyboard focus, if any.
    ///
    /// Panels are skipped: clicking the clock must not steal input from the terminal.
    pub fn focus_candidate(&self) -> Option<u32> {
        self.windows.iter().rev().find(|w| w.role.takes_focus()).map(|w| w.id)
    }

    /// Composite the stack into `fb`, repainting `damage`.
    ///
    /// Windows with no committed buffer, or whose buffer the source cannot resolve, are
    /// skipped rather than drawn as garbage — a client that has created a window but not
    /// yet drawn into it should show background, not whatever the memory held.
    pub fn compose_into<F, S>(&self, fb: &mut F, background: Rgb, source: &S, damage: &[Rect])
    where
        F: Framebuffer + ?Sized,
        S: BufferSource + ?Sized,
    {
        let mut surfaces: Vec<SurfaceRef<'_>> = Vec::new();
        for w in &self.windows {
            let Some(buffer_id) = w.committed else { continue };
            let Some(b) = w.buffers.iter().find(|b| b.id == buffer_id) else { continue };
            let Some(px) = source.pixels(w.id, buffer_id) else { continue };
            if px.len() < b.geometry.byte_len() {
                continue;
            }
            surfaces.push(SurfaceRef::new(b.geometry, w.origin, px));
        }
        compose(fb, background, &surfaces, damage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use libdraw::framebuffer::MemFramebuffer;
    use librsproto::surface::SURFACE_FORMAT_XRGB8888;

    /// A `BufferSource` backed by owned vectors.
    #[derive(Default)]
    struct MapSource(BTreeMap<(u32, u32), Vec<u8>>);

    impl MapSource {
        fn put(&mut self, window: u32, buffer: u32, g: Geometry, colour: Rgb) {
            let mut px = vec![0u8; g.byte_len()];
            let word = g.format.encode(colour).to_le_bytes();
            for y in 0..g.height {
                for x in 0..g.width {
                    let off = g.offset_of(x, y).unwrap();
                    px[off..off + 4].copy_from_slice(&word);
                }
            }
            self.0.insert((window, buffer), px);
        }
    }

    impl BufferSource for MapSource {
        fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]> {
            self.0.get(&(window, buffer)).map(|v| v.as_slice())
        }
    }

    fn geom(w: u32, h: u32) -> Geometry {
        Geometry::packed(w, h, PixelFormat::XRGB8888)
    }

    fn attach(window: u32, buffer: u32, w: u32, h: u32) -> AttachBufferRequest {
        AttachBufferRequest {
            window,
            buffer,
            width: w,
            height: h,
            pitch: w * 4,
            format: SURFACE_FORMAT_XRGB8888,
        }
    }

    fn commit(window: u32, buffer: u32) -> CommitRequest {
        CommitRequest {
            window,
            buffer,
            damage_x: 0,
            damage_y: 0,
            damage_w: 0,
            damage_h: 0,
        }
    }

    fn screen() -> MemFramebuffer {
        MemFramebuffer::new(Geometry::with_pitch(32, 16, 140, PixelFormat::XRGB8888).unwrap())
    }

    #[test]
    fn a_created_window_gets_a_fresh_id_and_keeps_its_role() {
        let mut s = WindowStack::new();
        let a = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        let b = s
            .create(&CreateWindowRequest {
                width: 32,
                height: 4,
                role: Role::Panel { dock: Edge::Top, reserve: 4 },
            })
            .unwrap();
        assert_ne!(a, b, "ids must be unique");
        assert_eq!(s.window(a).unwrap().role, Role::Normal);
        assert_eq!(s.window(b).unwrap().role, Role::Panel { dock: Edge::Top, reserve: 4 });
    }

    #[test]
    fn a_popup_must_name_a_parent_that_exists() {
        let mut s = WindowStack::new();
        assert_eq!(
            s.create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: 99 } }),
            Err(StackError::NoSuchParent)
        );
        let p = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        assert!(
            s.create(&CreateWindowRequest {
                width: 4,
                height: 4,
                role: Role::Popup { parent: p }
            })
            .is_ok()
        );
    }

    #[test]
    fn destroying_a_parent_takes_its_popups_with_it() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        let menu = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: p } })
            .unwrap();
        let other = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        s.destroy(p).unwrap();
        assert!(s.window(menu).is_none(), "an orphaned popup has no defined stacking position");
        assert!(s.window(other).is_some(), "unrelated windows are untouched");
    }

    #[test]
    fn commit_returns_the_previous_buffer_to_release_not_the_new_one() {
        // Releasing the newly committed buffer would hand the client back memory the
        // compositor is about to read — the tearing this protocol exists to prevent.
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
            .unwrap();
        s.attach(&attach(w, 0, 4, 4)).unwrap();
        s.attach(&attach(w, 1, 4, 4)).unwrap();

        assert_eq!(s.commit(&commit(w, 0)).unwrap(), None, "nothing was on screen before");
        assert_eq!(s.commit(&commit(w, 1)).unwrap(), Some(0), "buffer 0 is now free");
        assert_eq!(s.commit(&commit(w, 0)).unwrap(), Some(1));
        // Re-committing the same buffer frees nothing.
        assert_eq!(s.commit(&commit(w, 0)).unwrap(), None);
    }

    #[test]
    fn committing_an_unattached_buffer_is_refused() {
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
            .unwrap();
        assert_eq!(s.commit(&commit(w, 7)), Err(StackError::NoSuchBuffer));
        assert_eq!(s.commit(&commit(99, 0)), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn attaching_the_same_buffer_id_twice_is_refused() {
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
            .unwrap();
        s.attach(&attach(w, 0, 4, 4)).unwrap();
        assert_eq!(s.attach(&attach(w, 0, 4, 4)), Err(StackError::DuplicateBuffer));
    }

    #[test]
    fn panels_reserve_space_and_normal_windows_do_not() {
        let mut s = WindowStack::new();
        let screen = Rect::new(0, 0, 100, 50);
        assert_eq!(s.work_area(screen), screen, "no panels, no reservation");

        s.create(&CreateWindowRequest {
            width: 100,
            height: 8,
            role: Role::Panel { dock: Edge::Top, reserve: 8 },
        })
        .unwrap();
        s.create(&CreateWindowRequest {
            width: 100,
            height: 6,
            role: Role::Panel { dock: Edge::Bottom, reserve: 6 },
        })
        .unwrap();
        s.create(&CreateWindowRequest { width: 40, height: 40, role: Role::Normal }).unwrap();

        assert_eq!(s.work_area(screen), Rect::new(0, 8, 100, 36));
    }

    #[test]
    fn every_edge_reserves_on_its_own_axis() {
        let mut s = WindowStack::new();
        for (edge, reserve) in
            [(Edge::Left, 3u32), (Edge::Right, 5), (Edge::Top, 2), (Edge::Bottom, 4)]
        {
            s.create(&CreateWindowRequest {
                width: 1,
                height: 1,
                role: Role::Panel { dock: edge, reserve },
            })
            .unwrap();
        }
        assert_eq!(s.work_area(Rect::new(0, 0, 100, 50)), Rect::new(3, 2, 92, 44));
    }

    #[test]
    fn over_reservation_empties_the_work_area_rather_than_inverting_it() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest {
            width: 1,
            height: 1,
            role: Role::Panel { dock: Edge::Top, reserve: 90 },
        })
        .unwrap();
        let wa = s.work_area(Rect::new(0, 0, 100, 50));
        assert_eq!(wa.size.h, 0, "a negative height would be a clipping catastrophe");
        assert!(wa.is_empty());
    }

    #[test]
    fn focus_skips_panels_and_follows_the_top_of_the_stack() {
        let mut s = WindowStack::new();
        let a = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let b = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.create(&CreateWindowRequest {
            width: 32,
            height: 4,
            role: Role::Panel { dock: Edge::Top, reserve: 4 },
        })
        .unwrap();
        // The panel is topmost, but must not take focus.
        assert_eq!(s.focus_candidate(), Some(b));
        s.raise(a).unwrap();
        assert_eq!(s.focus_candidate(), Some(a));
    }

    #[test]
    fn a_stack_with_only_panels_has_nothing_to_focus() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest {
            width: 32,
            height: 4,
            role: Role::Panel { dock: Edge::Top, reserve: 4 },
        })
        .unwrap();
        assert_eq!(s.focus_candidate(), None);
    }

    #[test]
    fn compositing_draws_committed_windows_bottom_first() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);

        let a = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(a, 0, 8, 8)).unwrap();
        src.put(a, 0, geom(8, 8), red);
        s.commit(&commit(a, 0)).unwrap();

        let b = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(b, 0, 8, 8)).unwrap();
        src.put(b, 0, geom(8, 8), blue);
        s.commit(&commit(b, 0)).unwrap();
        s.set_origin(b, Point::new(4, 4)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);

        assert_eq!(fb.get_pixel(0, 0), Some(red), "the lower window");
        assert_eq!(fb.get_pixel(6, 6), Some(blue), "the upper window wins the overlap");
        assert_eq!(fb.get_pixel(10, 10), Some(blue));
        assert_eq!(fb.get_pixel(20, 2), Some(Rgb::BLACK), "background elsewhere");
    }

    #[test]
    fn a_window_with_no_committed_buffer_shows_background_not_garbage() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        // **The pixels must exist**, or the source-resolution guard catches the window and
        // this test says nothing about the `committed` guard it is named for. In the server
        // an attached buffer is real mapped memory holding whatever the client last put
        // there; showing it before a commit is the bug.
        src.put(w, 0, geom(8, 8), Rgb::new(0xC0, 0x10, 0x10));
        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(1, 2, 3), &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(1, 2, 3)), "uncommitted pixels leaked");
    }

    #[test]
    fn destroying_an_ancestor_reaches_grandchildren() {
        // A submenu is a popup parented to a popup. One pass over direct children leaves it
        // alive with a dead parent — still compositing, still eligible for focus.
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let menu =
            s.create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: w } })
                .unwrap();
        let sub = s
            .create(&CreateWindowRequest { width: 2, height: 2, role: Role::Popup { parent: menu } })
            .unwrap();
        let dlg = s
            .create(&CreateWindowRequest { width: 2, height: 2, role: Role::Dialog { parent: sub } })
            .unwrap();
        let other =
            s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();

        s.destroy(w).unwrap();
        for gone in [menu, sub, dlg] {
            assert!(s.window(gone).is_none(), "window {gone} outlived its ancestry");
        }
        assert!(s.window(other).is_some(), "unrelated windows are untouched");
        assert_eq!(s.focus_candidate(), Some(other));
    }

    #[test]
    fn strut_accumulation_saturates_rather_than_wrapping() {
        // `reserve` is bounded at the protocol edge, but `Role`'s fields are **public**, so
        // the library must not rely on that: anything constructing a `Role` in Rust bypasses
        // the parser entirely. A wrap here is worse than a panic — in release it returns the
        // *full* screen as the work area, silently defeating the clamp.
        //
        // Sized to actually overflow: two reserves near `u32::MAX` do, where a hundred
        // legitimate ones do not. An earlier version of this test used 128 panels at the
        // protocol bound (8.4M total) and could not tell `wrapping_add` from
        // `saturating_add` at all.
        let mut s = WindowStack::new();
        for _ in 0..2 {
            s.create(&CreateWindowRequest {
                width: 1,
                height: 1,
                // Exactly 2^31 each: the sum is 2^32, which wraps to **zero** — the case
                // that silently returns the full screen. `u32::MAX - 1` twice wraps to a
                // still-huge number the clamp handles anyway, so it proves nothing; that
                // was this test's second wrong version.
                role: Role::Panel { dock: Edge::Top, reserve: 0x8000_0000 },
            })
            .unwrap();
        }
        let wa = s.work_area(Rect::new(0, 0, 100, 50));
        assert_eq!(wa.size.h, 0, "the work area must clamp to empty, not wrap open");
        assert!(wa.is_empty());
    }

    #[test]
    fn a_realistic_pair_of_bars_still_leaves_a_work_area() {
        // The saturation guard must not swallow ordinary values.
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest {
            width: 100,
            height: 32,
            role: Role::Panel { dock: Edge::Top, reserve: 32 },
        })
        .unwrap();
        s.create(&CreateWindowRequest {
            width: 100,
            height: 28,
            role: Role::Panel { dock: Edge::Bottom, reserve: 28 },
        })
        .unwrap();
        assert_eq!(s.work_area(Rect::new(0, 0, 200, 100)), Rect::new(0, 32, 200, 40));
    }

    #[test]
    fn default_and_new_share_one_id_space() {
        let mut d = WindowStack::default();
        let mut n = WindowStack::new();
        let req = CreateWindowRequest { width: 1, height: 1, role: Role::Normal };
        assert_eq!(d.create(&req).unwrap(), n.create(&req).unwrap());
    }

    #[test]
    fn a_buffer_the_source_cannot_resolve_is_skipped_not_drawn() {
        // The server maps buffers; a client can commit one whose mapping has gone. Drawing
        // from an unresolvable buffer would read whatever memory happened to be there.
        let mut s = WindowStack::new();
        let src = MapSource::default(); // deliberately empty
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        s.commit(&commit(w, 0)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(9, 9, 9), &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(9, 9, 9)));
    }

    #[test]
    fn a_short_buffer_is_skipped_rather_than_read_past_its_end() {
        struct Short;
        impl BufferSource for Short {
            fn pixels(&self, _w: u32, _b: u32) -> Option<&[u8]> {
                Some(&[0u8; 4]) // far shorter than an 8x8 buffer
            }
        }
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        s.commit(&commit(w, 0)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(4, 5, 6), &Short, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(4, 5, 6)));
    }

    #[test]
    fn raising_a_window_changes_what_wins_the_overlap() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);
        for (id, colour) in [(1u32, red), (2u32, blue)] {
            let w = s
                .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
                .unwrap();
            assert_eq!(w, id);
            s.attach(&attach(w, 0, 8, 8)).unwrap();
            src.put(w, 0, geom(8, 8), colour);
            s.commit(&commit(w, 0)).unwrap();
        }
        let full = screen().geometry().bounds();

        let mut fb = screen();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(blue), "window 2 is on top");

        s.raise(1).unwrap();
        let mut fb = screen();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(red), "window 1 now is");
    }

    #[test]
    fn the_guest_configuration_composites_a_client_surface() {
        // The exact shape `ui-testclient` produces in the guest: a 1280x800 screen at
        // pitch 5120, a 64x32 client surface at pitch 256, window origin (0,0). Written
        // after the guest showed pure background in the window region with every guard in
        // `compose_into` passing and the client's pixels verified correct.
        let mut fb = MemFramebuffer::new(
            Geometry::with_pitch(1280, 800, 5120, PixelFormat::XRGB8888).unwrap(),
        );
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest { width: 64, height: 32, role: Role::Normal }).unwrap();
        s.attach(&AttachBufferRequest {
            window: w,
            buffer: 0,
            width: 64,
            height: 32,
            pitch: 256,
            format: SURFACE_FORMAT_XRGB8888,
        })
        .unwrap();
        let g = Geometry::with_pitch(64, 32, 256, PixelFormat::XRGB8888).unwrap();
        let marker = Rgb::new(33, 33, 33);
        src.put(w, 0, g, marker);
        s.commit(&commit(w, 0)).unwrap();

        let bounds = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(0x0E, 0x14, 0x1B), &src, &[bounds]);

        assert_eq!(fb.get_pixel(6, 5), Some(marker), "the client's surface must reach the screen");
        assert_eq!(fb.get_pixel(63, 31), Some(marker), "including its last pixel");
        assert_eq!(
            fb.get_pixel(64, 0),
            Some(Rgb::new(0x0E, 0x14, 0x1B)),
            "and stop at its edge"
        );
    }

    #[test]
    fn info_reports_the_committed_size_not_the_requested_one() {
        // A client may create a window at one size and commit a buffer of another; what is
        // on screen is the buffer, so that is what `info` must report.
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest { width: 100, height: 50, role: Role::Normal }).unwrap();
        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (100, 50), "requested size before any commit");

        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::BLACK);
        s.commit(&commit(w, 0)).unwrap();
        s.set_origin(w, Point::new(-3, 12)).unwrap();

        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (8, 8), "committed size once there is one");
        assert_eq!((info.x, info.y), (-3, 12));
        assert_eq!(info.id, w);
    }

    #[test]
    fn info_carries_the_role_and_its_extra_fields() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest {
                width: 100,
                height: 24,
                role: Role::Panel { dock: Edge::Bottom, reserve: 24 },
            })
            .unwrap();
        let info = s.info(p).unwrap();
        assert_eq!(info.role, librsproto::surface::ROLE_PANEL);
        assert_eq!(info.reserve, 24);

        let child = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: p } })
            .unwrap();
        assert_eq!(s.info(child).unwrap().parent, p);
        assert!(s.info(9999).is_none(), "a window that does not exist has no info");
    }

    #[test]
    fn a_bad_pitch_is_refused_at_attach() {
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let mut req = attach(w, 0, 8, 8);
        req.pitch = 8 * 4 - 1; // cannot hold a row
        assert_eq!(s.attach(&req), Err(StackError::BadGeometry));
    }
}
