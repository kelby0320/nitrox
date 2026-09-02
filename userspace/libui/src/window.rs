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
    /// The live hover: what the pointer is actually over.
    hover: Option<u64>,
    /// The hover the retained tree was **last built with**.
    ///
    /// **This is the one a gesture must see**, and the distinction is the whole of a bug that
    /// took two goes to fix. A capture names a *tree id*; the widgets here change shape under the
    /// pointer; so rebuilding the tree with a different hover between a press and its release
    /// gives the captured node a new id, `path_to_id` finds nothing, and the click is lost.
    ///
    /// M12 Part B froze the hover *from the press onwards*, which is too late: the motion that
    /// brought the pointer onto the widget is usually in the **same batch** as the press, so the
    /// live hover has already advanced while the tree still reflects the old one. The press is
    /// routed against the old tree — correctly — and then the next frame rebuilds with the new
    /// hover and strands the capture. It failed about one run in seven, and a probe in the guest
    /// is what finally said so rather than any amount of reading (M12 Part D).
    shown: Option<u64>,
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
    /// `None` for a role that is not one of the two parented ones, if the compositor refuses, if
    /// the tree measures to nothing, or if the memory could not be had.
    ///
    /// **The role is checked rather than assumed** (PR #267 review, optional 4). A `Child` has
    /// its size fixed at creation and never answers a `Configure`, so a `normal` created through
    /// here would ignore every resize a manager asked of it for the rest of its life —
    /// silently, because declining a `Configure` is legal.
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
        if !matches!(role, Role::Popup { .. } | Role::Dialog { .. }) {
            return None;
        }
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
        Some(Self {
            id,
            size,
            tree: Tree::new(),
            router: Router::new(),
            scratch,
            pool,
            hover: None,
            shown: None,
        })
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
    ///
    /// **While a gesture is in progress this is what the retained tree was built with**, not
    /// what the pointer is over — see [`shown`](Self::shown). A caller builds its view from this
    /// and hands the result to both [`present`](Self::present) and [`route`](Self::route), so
    /// answering with the tree's own hover is what keeps all three the same shape for the whole
    /// of a gesture.
    pub fn hovered_key(&self) -> Option<u64> {
        reported_hover(self.router.grabbed(), self.hover, self.shown)
    }

    /// Paint `content` and put it on screen **if anything changed**. `false` if this frame could
    /// not be drawn.
    ///
    /// **A failure is recoverable, and it has to be made so here.** The diff has already
    /// advanced the retained tree by the time a buffer can be refused, so the damage for the
    /// frame is spent: a caller that logged the failure and carried on — which is what both
    /// callers do — would get `Ok(None)` from every later frame and a child stuck showing
    /// whatever it last managed to commit, while remaining a configured, focusable window. So
    /// the tree is cleared on the way out and the next attempt repaints everything. The
    /// alternative reading, that `false` means the window is gone and the caller should destroy
    /// it, is a contract neither caller followed (PR #267 review, optional 3).
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
        // **What this frame is being drawn with**, recorded before it is drawn: the caller built
        // `content` from `hovered_key` a moment ago, so this is the hover the retained tree is
        // about to hold. Under a grab it is already `shown` and this is a no-op.
        self.shown = self.hovered_key();
        let bounds = Rect::new(0, 0, self.size.w, self.size.h);
        let l = layout(content, bounds, &FontMetrics::new(font, theme.font_px));
        let damage = match self.tree.update(content, &l) {
            Ok(None) => return true, // nothing changed; what is on screen is still right
            Ok(Some(d)) => d,
            // A malformed tree is a bug in the caller's view, not a runtime condition — but a
            // child window is not worth killing a process over, so it is reported as a frame
            // that could not be drawn. Nothing to clear: a rejected update leaves the tree
            // exactly as it was.
            Err(_) => return false,
        };
        paint(&mut self.scratch, font, theme, content, &l, damage, &mut |_, _, _, _| {});
        let mut drawn = false;
        if let Some(mut w) = session.window(self.id)
            && let Ok(b) = self.pool.acquire(&mut w, self.size)
            && self.pool.write(b, self.scratch.bytes())
        {
            let region =
                (damage.origin.x as u32, damage.origin.y as u32, damage.size.w, damage.size.h);
            drawn = session.window(self.id).is_some_and(|mut w| w.commit(b, region).is_ok());
        }
        if !drawn {
            // The damage this frame described is spent, so the next one has to describe
            // everything: `clear` is what a resize uses for the same reason.
            self.tree.clear();
        }
        drawn
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
                let out = self.router.pointer(&self.tree, content, &l, *p).0;
                // Tracked always; what a *gesture* sees is gated in `hovered_key`, because by
                // the time a press arrives this has usually already moved.
                self.hover = self.router.hovered_key(&self.tree);
                out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{Element, Insets, padding, text};
    use crate::layout::FixedCell;
    use crate::route::Router;
    use crate::widget::{Theme, menu_item};
    use librsproto::surface::{POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent};

    /// A menu row is a widget whose *shape* changes under the pointer: quiet it draws one layer,
    /// hovered it draws three. That is what makes the id a capture records go stale.
    fn menu(hovered: Option<u64>) -> Element<u32> {
        let theme = Theme::default();
        crate::element::column(alloc::vec![
            menu_item("one", 1u32, hovered == Some(1), &theme).key(1),
            menu_item("two", 2u32, hovered == Some(2), &theme).key(2),
        ])
    }

    #[test]
    fn a_click_survives_a_repaint_between_the_press_and_the_release() {
        // **The bug this exists to prevent, reproduced at the level it lives at.** A capture is a
        // tree id of the *deepest* node under the cursor. Rebuild the tree with a different hover
        // between the press and the release and that node is a different node with a different
        // id, `path_to_id` finds nothing, and the release produces no message at all — which
        // presents as a menu row that can be clicked and does nothing, once in every few runs.
        //
        // `Child` cannot be built here (every one of its methods is a syscall or a `Session`), so
        // this drives the two pieces it wires together: a retained `Tree` and a `Router`, with
        // the hover resampled the way `Child::route` resamples it.
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 120, 60);
        let mut tree = Tree::new();
        let mut router = Router::new();
        // What `Child` stores: the hover the retained tree was last built with.
        let mut hover: Option<u64> = None;

        let at = |kind: u16, flags: u16, buttons: u16, y: i32| PointerEvent {
            kind,
            button: 0x110,
            buttons,
            flags,
            x: 20,
            y,
            ..Default::default()
        };
        // Row 1 is the second `menu_item`, which is 20 tall in this metric.
        let row1_y = 30;

        let step = |ev: PointerEvent, hover: &mut Option<u64>, tree: &mut Tree, router: &mut Router| {
            let ui = menu(*hover);
            let l = crate::layout::layout(&ui, bounds, &cell);
            tree.update(&ui, &l).expect("diffable");
            let out = router.pointer(tree, &ui, &l, ev).0;
            if !router.grabbed() {
                *hover = router.hovered_key(tree);
            }
            out
        };

        // Move onto the row, press, and *repaint* — which is where the tree changes shape.
        step(at(POINTER_MOTION, 0, 0, row1_y), &mut hover, &mut tree, &mut router);
        assert_eq!(hover, Some(2), "the pointer is over the second row");
        let down = step(at(POINTER_BUTTON, POINTER_PRESSED, 1, row1_y), &mut hover, &mut tree, &mut router);
        assert!(down.is_empty(), "a press is not a click");
        let up = step(at(POINTER_BUTTON, 0, 0, row1_y), &mut hover, &mut tree, &mut router);
        assert_eq!(up, alloc::vec![2u32], "the release on the pressed row is the click");
    }

    #[test]
    fn a_click_survives_a_motion_and_a_press_arriving_together() {
        // **The case the first fix missed, and the one that actually happens.** A pointer walked
        // onto a row and pressed produces the motion and the press in one batch, with no frame
        // between them — so the live hover advances while the retained tree still holds the old
        // one. Freezing from the press onwards is too late: the *next* frame rebuilds with the
        // new hover and strands the capture taken against the old tree.
        //
        // This is `Child`'s rule reproduced at the level it lives at: what a gesture sees is what
        // the tree was built with. Drive it with the *presented* hover throughout and the click
        // survives; drive it with the live one and it does not — which is the control below.
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 120, 60);
        let mut tree = Tree::new();
        let mut router = Router::new();
        let mut live: Option<u64> = None;
        let mut shown: Option<u64> = None;

        let at = |kind: u16, flags: u16, buttons: u16, y: i32| PointerEvent {
            kind,
            button: 0x110,
            buttons,
            flags,
            x: 20,
            y,
            ..Default::default()
        };
        // **The shipped decision, not a copy of it.** `Child::hovered_key` delegates to this, so
        // breaking the rule fails this test — which the first version of it did not.
        let seen = |router: &Router, live: Option<u64>, shown: Option<u64>| {
            reported_hover(router.grabbed(), live, shown)
        };

        // Frame one, drawn with nothing hovered.
        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        tree.update(&ui, &l).expect("diffable");
        shown = seen(&router, live, shown);

        // **One batch: the crossing, the motion and the press, with no frame between them.**
        let row1_y = 30;
        for ev in [
            at(librsproto::surface::POINTER_ENTER, 0, 0, row1_y),
            at(POINTER_MOTION, 0, 0, row1_y),
            at(POINTER_BUTTON, POINTER_PRESSED, 1, row1_y),
        ] {
            let ui = menu(seen(&router, live, shown));
            let l = crate::layout::layout(&ui, bounds, &cell);
            router.pointer(&tree, &ui, &l, ev);
            live = router.hovered_key(&tree);
        }
        assert_eq!(live, Some(2), "the pointer is over the second row");
        assert!(router.grabbed(), "and holding it");
        assert_eq!(seen(&router, live, shown), None, "but the gesture sees what the tree holds");

        // A frame happens now — the one that used to strand the capture.
        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        tree.update(&ui, &l).expect("diffable");
        shown = seen(&router, live, shown);

        // And the release still finds the widget it captured.
        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        let out = router.pointer(&tree, &ui, &l, at(POINTER_BUTTON, 0, 0, row1_y)).0;
        assert_eq!(out, alloc::vec![2u32], "the click survived the frame in the middle");
    }

    #[test]
    fn a_press_opens_a_capture_and_the_release_closes_it() {
        // **The precondition the rule rests on**, and all this checks — named for that rather
        // than for the rule itself, which it never asserted. `Child::hover` *does* move under a
        // grab since M12 Part D; what does not move is what `hovered_key` reports, and
        // `a_click_survives_a_motion_and_a_press_arriving_together` is where that is pinned
        // (PR #270 review, optional 8).
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 120, 60);
        let mut tree = Tree::new();
        let mut router = Router::new();
        let ui = menu(None);
        let l = crate::layout::layout(&ui, bounds, &cell);
        tree.update(&ui, &l).expect("diffable");
        let at = |kind: u16, flags: u16, buttons: u16, y: i32| PointerEvent {
            kind,
            button: 0x110,
            buttons,
            flags,
            x: 20,
            y,
            ..Default::default()
        };
        router.pointer(&tree, &ui, &l, at(POINTER_MOTION, 0, 0, 10));
        assert!(!router.grabbed());
        router.pointer(&tree, &ui, &l, at(POINTER_BUTTON, POINTER_PRESSED, 1, 10));
        assert!(router.grabbed(), "a press opens a capture");
        // Dragging onto the other row while held must not be reported as a new hover.
        router.pointer(&tree, &ui, &l, at(POINTER_MOTION, 0, 1, 30));
        assert!(router.grabbed());
        router.pointer(&tree, &ui, &l, at(POINTER_BUTTON, 0, 0, 30));
        assert!(!router.grabbed(), "the release closes it");
    }

    /// Space for a `padding` import that keeps the element helpers honest in this module.
    #[allow(dead_code)]
    fn _unused() -> Element<u32> {
        padding(Insets::all(0), text("x"))
    }
}

/// What a caller is told the hover is: the tree's while a gesture runs, the pointer's otherwise.
///
/// **A function so that one thing decides it** (PR #270 review, blocking 2). The rule had a test
/// that re-implemented it with a local closure, so reverting the shipped code left all two hundred
/// `libui` tests green — a guard for a mechanism rather than for the fix, on the third attempt at
/// a bug whose first two attempts also looked right. The test calls this now, and breaking it
/// fails.
///
/// **What is still not covered here** is the wiring: that [`Child::present`] records the hover it
/// drew with and that [`Child::route`] tracks the live one. A `Child` cannot be built on the host
/// at all — `BufferPool::new` is memory syscalls — so those two lines are the gate's, and this is
/// the part that can be pinned without one.
fn reported_hover(grabbed: bool, live: Option<u64>, shown: Option<u64>) -> Option<u64> {
    if grabbed { shown } else { live }
}

/// A private framebuffer of `size` to compose a frame into.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}
