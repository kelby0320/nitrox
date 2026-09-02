//! A window an application opens **beside** its main one — a menu, or a dialog.
//!
//! ## Why this is here at all
//!
//! `widget-toolkit.md` §11 has said since Milestone 4 that "one `App` drives one window", with
//! the trigger written down: *dialogs that are real windows rather than `stack` overlays*. M12
//! Part A is that day. But the shape it names was already in the tree twice over — `nxterm`
//! grew a `Popup` struct in M6 Part C3 holding an id, a [`BufferPool`], a scratch framebuffer,
//! a [`Tree`] and a [`Router`], and an editor's confirmation needs exactly the same six fields
//! and the same four operations on them.
//!
//! Two consumers is this project's rule for when something goes down a layer
//! (`userspace/CLAUDE.md`), so it went down one. Nothing here is new behaviour: it is
//! `nxterm::Popup` with the application-specific parts taken out.
//!
//! ## What a `Child` is, and what it is not
//!
//! A **child** is one of the two parented roles — [`Role::Popup`] and [`Role::Dialog`] — which
//! is not a category this module invented: the compositor's own `parent_of` matches exactly
//! those two, and `rsproto-surface-ops.md` calls both "transient, parented". A child has a size
//! fixed at creation, no `Configure` to honour, no second event source, and a whole widget tree
//! with nothing custom in it. That is what makes it a value an application can hold, open and
//! drop.
//!
//! **A main window is not one of these**, and is deliberately still driven by each application's
//! own loop: it owns the `sys_wait`, it must answer `Configure` by reallocating everything here,
//! and `nxterm`'s also paints a `custom` grid whose damage feeds `libterm`. Converting those is
//! a change to the shape of every `main` in the tree with no new behaviour in it, and it does
//! not belong in the same change as the first dialog. **Trigger: the next part that touches
//! both applications' main loops** — M12 Part D's tabs is the obvious one.
//!
//! ## Not host-tested, for the reason [`libsurface::buffers`] is not
//!
//! Every line below is a call into [`Session`]/[`WindowRef`](libsurface::WindowRef) or into
//! [`paint`]/[`layout`]/[`Tree::update`]/[`Router`], and both halves have host tests of their
//! own — the first against `libsurface`'s mock transport, the second throughout this crate.
//! What is left here is the *order* they are called in, which is what a gate sees:
//! `check-login` drives a real dialog to both its answers.
//!
//! This is the one module in `libui` that is not a function of values, and it is why the crate
//! depends on `libsurface` at all. The rest of the toolkit is unchanged: nothing in
//! [`element`](crate::element), [`layout`](crate::layout), [`diff`](crate::diff),
//! [`paint`](crate::paint), [`route`](crate::route) or [`widget`](crate::widget) can reach a
//! syscall.

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, Transport, WindowEvent};

use crate::diff::Tree;
use crate::element::Element;
use crate::layout::{Constraints, layout, measure};
use crate::paint::{FontMetrics, Theme, paint};
use crate::route::Router;

/// The largest extent [`Child::open`] will measure a tree against.
///
/// **Not `u32::MAX`**, which overflows the moment a layout adds padding to it. A quarter of the
/// range is past any screen this system will see and leaves three quarters of headroom for the
/// arithmetic inside `measure`; it is the bound `nxterm::Popup::open` used before this module
/// existed.
const MEASURE_MAX: u32 = u32::MAX / 4;

/// A window an application opens beside its main one.
///
/// Holds everything that is *per window* rather than per application: the id the compositor
/// knows it by, the pixels on both sides of the wire, and the two pieces of retained state that
/// describe a window rather than a program — the diff [`Tree`] and the [`Router`].
///
/// **The same `App` drives both windows.** A message from a dialog's button updates the same
/// state a message from the main window does; what differs is the tree it was routed through.
/// That is `nxterm`'s arrangement for its menu, unchanged.
pub struct Child {
    /// The compositor's id for this window.
    id: u32,
    /// Its size, fixed at creation.
    size: Size,
    /// The retained tree this window's frames diff against.
    tree: Tree,
    /// Routing state. Focus *within* this window is not focus within its parent.
    router: Router,
    /// Where a frame is composed before it is copied to a shared buffer.
    scratch: MemFramebuffer,
    /// The shared buffers, and the mappings that are unmapped when this value drops.
    pool: BufferPool,
}

impl Child {
    /// Open a child window sized to what `content` measures, at `at` in the role's own
    /// coordinates.
    ///
    /// **Measured rather than given a size.** A window needs its extent before it exists, and a
    /// hardcoded one silently stops matching the tree the first time a row is added to a menu or
    /// a word to a question. `Fill` measures as zero, so a backing layer does not inflate it.
    ///
    /// `at` means what the role means: a [`Role::Popup`]'s offset is from its **parent's**
    /// origin, and a [`Role::Dialog`]'s is a requested screen origin that a manager overrides —
    /// so a dialog should pass `(0, 0)` and let the shell place it. A client does not know where
    /// it is on screen and should not pretend to; `rsproto-surface-ops.md` says the manager can
    /// centre a dialog on its parent from what it already tracks.
    ///
    /// `None` if the compositor refuses, if the tree measures to nothing, or if the memory could
    /// not be had.
    ///
    /// **Everything after the create destroys the window on the way out.** An abandoned child is
    /// worse than none: [`Session::create`] waits for the first `Configure`, so it is
    /// *configured*, and a configured window is a focus candidate — having committed nothing it
    /// is never drawn, so the result is an invisible window silently eating every keystroke
    /// (`nxterm`, PR #223 review, finding 4).
    pub fn open<T: Transport, Msg>(
        session: &mut Session<T>,
        role: Role,
        at: (i32, i32),
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
        buffers: usize,
    ) -> Option<Self> {
        let m = FontMetrics::new(font, theme.font_px);
        let size = measure(content, Constraints::loose(Size::new(MEASURE_MAX, MEASURE_MAX)), &m);
        // **Nothing, and everything, are both refusals.** Zero is a tree that draws nothing.
        // The upper bound is the one worth explaining: `Node::Dock` measures as *whatever it is
        // offered*, deliberately — its whole purpose is to divide a given area — so any tree
        // with a bare `dock` in it measures to `MEASURE_MAX` here and would otherwise be created
        // as a window a thousand screens wide. A caller that wants a dock inside a child window
        // wraps it in a `sized`, which is a size it has chosen rather than one this function
        // invented for it.
        if size.w == 0 || size.h == 0 || size.w >= MEASURE_MAX || size.h >= MEASURE_MAX {
            return None;
        }
        let id = session
            .create(&CreateWindowRequest::at(size.w, size.h, role, at.0, at.1), buffers)
            .ok()?;
        let built = (|| {
            let scratch = compose_buffer(size)?;
            let pool = BufferPool::new(&mut session.window(id)?, size, buffers)?;
            Some((pool, scratch))
        })();
        let Some((pool, scratch)) = built else {
            if let Some(w) = session.window(id) {
                let _ = w.destroy();
            }
            return None;
        };
        Some(Self { id, size, tree: Tree::new(), router: Router::new(), scratch, pool })
    }

    /// The compositor's id for this window — what an event names.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Its size in pixels, which does not change.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The key of the widget under the pointer, for a caller building the next frame.
    ///
    /// **A key, not a tree id.** `Router::inside` reports a diff-tree id and `.key(…)` is the
    /// application's own numbering; comparing one to the other compiles and gives a stable wrong
    /// answer (M11 Part E batch 3).
    pub fn hovered_key(&self) -> Option<u64> {
        self.router.hovered_key(&self.tree)
    }

    /// Paint `content` and put it on screen **if anything changed**. `false` if the window
    /// could not be drawn, which the caller should treat as the window being gone.
    ///
    /// **Gated on the diff, and that is not merely thrift.** With two buffers the third commit
    /// blocks in `acquire` until the compositor releases one, and that block is inside the
    /// render half of a client's loop — so a client with a second source of work stops pumping
    /// it. `nxterm` appeared to hang with its menu open for exactly this reason.
    ///
    /// The whole composed frame is copied into the buffer and only the damaged rectangle is
    /// committed: the scratch framebuffer persists between frames, so what is outside the damage
    /// is last frame's pixels and is still correct.
    pub fn present<T: Transport, Msg>(
        &mut self,
        session: &mut Session<T>,
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
    ) -> bool {
        let bounds = Rect::new(0, 0, self.size.w, self.size.h);
        let l = layout(content, bounds, &FontMetrics::new(font, theme.font_px));
        let damage = match self.tree.update(content, &l) {
            Ok(None) => return true, // nothing changed; what is on screen is still right
            Ok(Some(d)) => d,
            // A malformed tree is a bug in the caller's view, not a runtime condition — but a
            // child window is not worth killing a process over, so it is reported as a window
            // that cannot be drawn.
            Err(_) => return false,
        };
        paint(&mut self.scratch, font, theme, content, &l, damage, &mut |_, _, _, _| {});
        let Some(mut w) = session.window(self.id) else { return false };
        let Ok(b) = self.pool.acquire(&mut w, self.size) else { return false };
        if !self.pool.write(b, self.scratch.bytes()) {
            return false;
        }
        session.window(self.id).is_some_and(|mut w| {
            w.commit(b, (damage.origin.x as u32, damage.origin.y as u32, damage.size.w, damage.size.h))
                .is_ok()
        })
    }

    /// Route one event through *this* window's tree, and return what it produced.
    ///
    /// `content` must be the tree the window was last presented with — routing is against a
    /// layout, and a layout of a different tree would report a widget that is not the one under
    /// the pointer.
    ///
    /// **Three variants and no default action.** A `Configure` is not answered because a child's
    /// size is fixed; a `Dismissed` means "close me", which is the caller's to act on because
    /// only the caller knows what the window was *for*; and no child declares a drop acceptor,
    /// so no `Drop` can arrive here.
    pub fn route<Msg: Clone>(
        &mut self,
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
        event: &WindowEvent,
    ) -> Vec<Msg> {
        match event {
            WindowEvent::Key(k) => {
                self.router.key(&self.tree, content, *k).into_iter().collect()
            }
            WindowEvent::Pointer(p) => {
                let bounds = Rect::new(0, 0, self.size.w, self.size.h);
                let l = layout(content, bounds, &FontMetrics::new(font, theme.font_px));
                self.router.pointer(&self.tree, content, &l, *p).0
            }
            WindowEvent::Focus(f) => {
                self.router.set_window_focused(*f);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Destroy the window and give this side's pixels back.
    ///
    /// **By value, because the mapping is the point.** The compositor drops its own when the
    /// window goes; the mapping *here* is the pool's, released when this value is dropped at the
    /// end of this call — a menu opened and closed a hundred times would otherwise grow the
    /// process by a hundred buffers.
    pub fn close<T: Transport>(self, session: &mut Session<T>) {
        if let Some(w) = session.window(self.id) {
            let _ = w.destroy();
        }
    }
}

/// A private framebuffer of `size` to compose a frame into.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}
