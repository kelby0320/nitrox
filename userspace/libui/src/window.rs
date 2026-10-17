//! A window an application owns: its main one, a menu, or a dialog.
//!
//! A `Child` is **any window a client owns**, its main one included: an id, a size, a retained
//! tree, a router, a compose buffer and a pool.
//!
//! **`Child::open` takes only the parented roles**, [`Role::Popup`] and [`Role::Dialog`], because
//! such a window's size is fixed at creation and a `normal` created that way would silently ignore
//! every resize a manager asked of it (declining a `Configure` is legal). [`Child::open_sized`] and
//! [`Child::resize`] are what answer that.

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use librsproto::surface::{CreateWindowRequest, KeyEvent, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, Transport, WindowEvent};

use crate::diff::Tree;
use crate::element::Element;
use crate::layout::{Constraints, Layout, layout, measure};
use crate::paint::{FontMetrics, Theme, paint};
use crate::route::Router;

/// The largest extent [`Child::open`] will measure against; not `u32::MAX`, which overflows.
const MEASURE_MAX: u32 = u32::MAX / 4;

/// A window an application opens beside its main one.
pub struct Child {
    id: u32,
    size: Size,
    tree: Tree,
    /// Routing state. Focus *within* this window is not focus within its parent.
    router: Router,
    scratch: MemFramebuffer,
    pool: BufferPool,
    /// The live hover: what the pointer is actually over.
    hover: Option<u64>,
    /// The hover the retained tree was **last built with**.
    ///
    /// **This is the one a gesture must see**: a capture names a *tree id*, and the motion onto a
    /// widget is usually in the same batch as the press, so rebuilding with the live hover strands
    /// the capture.
    shown: Option<u64>,
    /// Whether this window has been handed the keyboard since it opened.
    had_keyboard: bool,
}

impl Child {
    /// Open a child window sized to what `content` measures, at `at` in the role's own coordinates.
    ///
    /// `at` is from the **parent's** origin for a [`Role::Popup`]; a [`Role::Dialog`]'s is a
    /// requested origin a manager overrides, so pass `(0, 0)`. `None` for a role that is not one of
    /// the two parented ones, if the compositor refuses, if the tree measures to nothing, or if the
    /// memory could not be had.
    ///
    /// **Everything after the create destroys the window on the way out**: an abandoned child is
    /// configured, hence a focus candidate, yet never drawn.
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
        // **Nothing, and everything, are both refusals**: a bare `dock` measures as whatever it is
        // offered, i.e. `MEASURE_MAX`; wrap it in a `sized`.
        if size.w == 0 || size.h == 0 || size.w >= MEASURE_MAX || size.h >= MEASURE_MAX {
            return None;
        }
        let id = session.create(&CreateWindowRequest::at(size.w, size.h, role, at.0, at.1), buffers).ok()?;
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
            had_keyboard: false,
        })
    }

    /// Open a window at a size the **caller** chose, in any role (a top-level may contain a `dock`,
    /// which measures as whatever it is offered).
    #[allow(
        clippy::too_many_arguments,
        reason = "a window's role, geometry, content and appearance are separate inputs"
    )]
    pub fn open_sized<T: Transport, Msg>(
        session: &mut Session<T>,
        role: Role,
        at: (i32, i32),
        size: Size,
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
        buffers: usize,
    ) -> Option<Self> {
        let mut me = Self::create(session, role, at, size, buffers)?;
        if me.present(session, content, font, theme) {
            Some(me)
        } else {
            me.close(session);
            None
        }
    }

    /// [`open_sized`](Self::open_sized) **without the first frame**, for a `panel` that must be
    /// moved before it is seen.
    ///
    /// **A panel only**: a *focusable* window that has committed nothing is an invisible window
    /// eating keystrokes; a panel never takes focus.
    pub fn create_sized<T: Transport>(
        session: &mut Session<T>,
        role: Role,
        size: Size,
        buffers: usize,
    ) -> Option<Self> {
        if !matches!(role, Role::Panel { .. }) {
            return None;
        }
        Self::create(session, role, (0, 0), size, buffers)
    }

    /// Create the window and its buffers, drawing nothing.
    fn create<T: Transport>(
        session: &mut Session<T>,
        role: Role,
        at: (i32, i32),
        size: Size,
        buffers: usize,
    ) -> Option<Self> {
        if size.w == 0 || size.h == 0 {
            return None;
        }
        let id = session.create(&CreateWindowRequest::at(size.w, size.h, role, at.0, at.1), buffers).ok()?;
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
            had_keyboard: false,
        })
    }

    /// Take a new size — from a `Configure`, or from a popup's owner — reallocating what depends
    /// on it.
    ///
    /// **`None` if nothing changed**, so a caller can skip a repeated size; `Some(false)` means the
    /// memory could not be had and the window keeps its old size.
    ///
    /// **The pool is kept, and there is no `Session` here for that reason**: building a new pool
    /// was rejected by the compositor, which still holds the old buffers, and the window silently
    /// stopped committing; `acquire` replaces a buffer left at the old shape.
    pub fn resize(&mut self, size: Size) -> Option<bool> {
        if size == self.size {
            return None;
        }
        let Some(scratch) = compose_buffer(size) else { return Some(false) };
        self.scratch = scratch;
        self.size = size;
        self.tree = Tree::new();
        Some(true)
    }

    /// The compositor's id for this window — what an event names.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Its size in pixels. A top-level's changes with every `Configure` it answers through
    /// [`resize`](Self::resize), and a popup's when its owner resizes it to its contents.
    pub fn size(&self) -> Size {
        self.size
    }

    /// The key of the widget under the pointer, for a caller building the next frame.
    ///
    /// **While a gesture is in progress this is what the retained tree was built with**, not what
    /// the pointer is over — see [`shown`](Self::shown).
    pub fn hovered_key(&self) -> Option<u64> {
        reported_hover(self.router.grabbed(), self.hover, self.shown)
    }

    /// Paint `content` and put it on screen **if anything changed**. `false` if this frame could
    /// not be drawn.
    ///
    /// **A failure is recoverable, and it has to be made so here**: the diff has advanced the tree,
    /// so the frame's damage is spent; the tree is cleared and the next attempt repaints
    /// everything.
    ///
    /// **Gated on the diff, and that is not merely thrift**: the third commit blocks in `acquire`
    /// inside the render half of a client's loop.
    pub fn present<T: Transport, Msg>(
        &mut self,
        session: &mut Session<T>,
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
    ) -> bool {
        let bounds = Rect::new(0, 0, self.size.w, self.size.h);
        let l = layout(content, bounds, &FontMetrics::new(font, theme.font_px));
        self.present_laid_out(session, content, &l, font, theme)
    }

    /// [`present`](Self::present), for a caller that has already laid the tree out.
    ///
    /// `l` must be the layout of `content` at this window's size, or the damage this reports
    /// describes a tree that was never drawn.
    pub fn present_laid_out<T: Transport, Msg>(
        &mut self,
        session: &mut Session<T>,
        content: &Element<Msg>,
        l: &Layout,
        font: &Font,
        theme: &Theme,
    ) -> bool {
        self.present_custom(session, content, l, font, theme, None, &mut |_, _, _, _| {})
    }

    /// [`present_laid_out`](Self::present_laid_out), for a window with a `custom` node in it.
    #[allow(
        clippy::too_many_arguments,
        reason = "a window's role, geometry, content and appearance are separate inputs"
    )]
    pub fn present_custom<T: Transport, Msg>(
        &mut self,
        session: &mut Session<T>,
        content: &Element<Msg>,
        l: &Layout,
        font: &Font,
        theme: &Theme,
        extra: Option<Rect>,
        custom: &mut dyn FnMut(u32, Rect, Rect, &mut MemFramebuffer),
    ) -> bool {
        self.shown = self.hovered_key();
        // **Damage the diff cannot see, unioned in**: a `custom` node is fingerprinted by kind and
        // size, so changed *contents* report nothing.
        let damage = match self.tree.update(content, l) {
            Ok(None) => match extra {
                None => return true,
                Some(e) => e,
            },
            Ok(Some(d)) => crate::damage::union_opt(Some(d), extra).unwrap_or(d),
            Err(_) => return false,
        };
        paint(&mut self.scratch, font, theme, content, l, damage, &mut |k, a, b, fb| custom(k, a, b, fb));
        let mut drawn = false;
        if let Some(mut w) = session.window(self.id)
            && let Ok(b) = self.pool.acquire(&mut w, self.size)
            && self.pool.write(b, self.scratch.bytes())
        {
            let region = (damage.origin.x as u32, damage.origin.y as u32, damage.size.w, damage.size.h);
            drawn = session.window(self.id).is_some_and(|mut w| w.commit(b, region).is_ok());
        }
        if !drawn {
            self.tree.clear();
        }
        drawn
    }

    /// Route one event through *this* window's tree, and return what it produced.
    ///
    /// **Three variants and no default action**: `Configure` is the owner's (see
    /// [`resize`](Self::resize)), `Dismissed` is the caller's to act on, and a `Drop` is routed by
    /// [`drop_at`](Self::drop_at).
    pub fn route<Msg: Clone>(
        &mut self,
        content: &Element<Msg>,
        font: &Font,
        theme: &Theme,
        event: &WindowEvent,
    ) -> Vec<Msg> {
        match event {
            WindowEvent::Key(k) => self.router.key(&self.tree, content, *k).into_iter().collect(),
            WindowEvent::Pointer(p) => {
                let bounds = Rect::new(0, 0, self.size.w, self.size.h);
                let l = layout(content, bounds, &FontMetrics::new(font, theme.font_px));
                let out = self.router.pointer(&self.tree, content, &l, *p).0;
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

    /// Give the keyboard to the widget carrying `key`, if the tree has one; for a window that must
    /// not start focused on its first widget.
    pub fn focus_key<Msg>(&mut self, content: &Element<Msg>, key: u64) {
        if let Some(id) = self.tree.find_by_key(key) {
            self.router.focus(&self.tree, content, id);
        }
    }

    /// Route a key, returning the message it produced; **separate from [`route`](Self::route)
    /// because a main window needs the `None`**.
    pub fn route_key<Msg: Clone>(&self, content: &Element<Msg>, k: KeyEvent) -> Option<Msg> {
        self.router.key(&self.tree, content, k)
    }

    /// Whether a drop at `(x, y)` landed on a widget that declared itself an acceptor; `l` must be
    /// the layout `content` was presented with.
    pub fn drop_at<Msg: Clone>(&mut self, content: &Element<Msg>, l: &Layout, x: i32, y: i32) -> bool {
        self.router.drop_at(&self.tree, content, l, x, y).is_some()
    }

    /// Whether `event` hands this window the keyboard **for the first time since it opened**.
    ///
    /// **The line a gate may type after**: a key reaching the compositor before the window is
    /// placed and focused goes to the parent (`check-login` once typed a Save As backspace into the
    /// document); the owner prints a receipt on this
    /// (`docs/conventions/qemu-integration-tests.md`).
    pub fn took_keyboard(&mut self, event: &WindowEvent) -> bool {
        first_keyboard(&mut self.had_keyboard, event)
    }

    /// Destroy the window and give this side's pixels back.
    pub fn close<T: Transport>(self, session: &mut Session<T>) {
        if let Some(w) = session.window(self.id) {
            let _ = w.destroy();
        }
    }
}

/// What a caller is told the hover is: the tree's while a gesture runs, the pointer's otherwise;
/// the wiring in `Child` is the gate's, as a `Child` cannot be built on the host.
fn reported_hover(grabbed: bool, live: Option<u64>, shown: Option<u64>) -> Option<u64> {
    if grabbed { shown } else { live }
}

/// Whether `event` is a window's first keyboard, recording in `had` that it has had one.
fn first_keyboard(had: &mut bool, event: &WindowEvent) -> bool {
    match event {
        WindowEvent::Focus(true) => !core::mem::replace(had, true),
        _ => false,
    }
}

/// A private framebuffer of `size` to compose a frame into.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::Element;
    use crate::layout::FixedCell;
    use crate::route::Router;
    use crate::widget::{Theme, menu_item};
    use librsproto::surface::{POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent};

    fn menu(hovered: Option<u64>) -> Element<u32> {
        let theme = Theme::default();
        crate::element::column(alloc::vec![
            menu_item("one", 1u32, hovered == Some(1), &theme).key(1),
            menu_item("two", 2u32, hovered == Some(2), &theme).key(2),
        ])
    }

    /// The freeze decision itself, as a table.
    ///
    /// **This pins the decision**, which catches reverting `reported_hover` to `live`; no host test
    /// drives the ancestor-reshaping case end to end, as `Child` cannot be built here.
    #[test]
    fn the_presented_hover_is_what_a_gesture_sees_while_it_is_held() {
        assert_eq!(reported_hover(false, Some(2), None), Some(2));
        assert_eq!(reported_hover(false, None, Some(1)), None);
        assert_eq!(reported_hover(true, Some(2), None), None, "the tree holds no hover yet");
        assert_eq!(reported_hover(true, None, Some(1)), Some(1), "…and it is not resampled");
        assert_eq!(reported_hover(true, Some(2), Some(1)), Some(1), "the presented one wins");
    }

    #[test]
    fn a_click_survives_a_repaint_between_the_press_and_the_release() {
        // **A click survives a repaint between its press and its release.** `Child` cannot be built
        // here, so this drives the `Tree` and `Router` it wires together.
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 120, 60);
        let mut tree = Tree::new();
        let mut router = Router::new();
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

        step(at(POINTER_MOTION, 0, 0, row1_y), &mut hover, &mut tree, &mut router);
        assert_eq!(hover, Some(2), "the pointer is over the second row");
        let down = step(at(POINTER_BUTTON, POINTER_PRESSED, 1, row1_y), &mut hover, &mut tree, &mut router);
        assert!(down.is_empty(), "a press is not a click");
        let up = step(at(POINTER_BUTTON, 0, 0, row1_y), &mut hover, &mut tree, &mut router);
        assert_eq!(up, alloc::vec![2u32], "the release on the pressed row is the click");
    }

    #[test]
    fn a_click_survives_a_motion_and_a_press_arriving_together() {
        // **The case the first fix missed**: a pointer walked onto a row and pressed gives the
        // motion and the press in one batch, so the live hover advances while the tree holds the
        // old one.
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
        let seen =
            |router: &Router, live: Option<u64>, shown: Option<u64>| reported_hover(router.grabbed(), live, shown);

        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        tree.update(&ui, &l).expect("diffable");
        shown = seen(&router, live, shown);

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

        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        tree.update(&ui, &l).expect("diffable");
        shown = seen(&router, live, shown);

        let ui = menu(seen(&router, live, shown));
        let l = crate::layout::layout(&ui, bounds, &cell);
        let out = router.pointer(&tree, &ui, &l, at(POINTER_BUTTON, 0, 0, row1_y)).0;
        assert_eq!(out, alloc::vec![2u32], "the click survived the frame in the middle");
    }

    #[test]
    fn a_press_opens_a_capture_and_the_release_closes_it() {
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
        router.pointer(&tree, &ui, &l, at(POINTER_MOTION, 0, 1, 30));
        assert!(router.grabbed());
        router.pointer(&tree, &ui, &l, at(POINTER_BUTTON, 0, 0, 30));
        assert!(!router.grabbed(), "the release closes it");
    }

    #[derive(Default)]
    struct Counting {
        requests: usize,
    }

    impl libsurface::Transport for Counting {
        fn request(
            &mut self,
            _op: u16,
            _body: &[u8],
            _handle: Option<u64>,
            _reply: &mut [u8],
        ) -> Result<Option<usize>, libsurface::UiError> {
            self.requests += 1;
            Err(libsurface::UiError::Malformed)
        }
        fn poll_event(&mut self, _buf: &mut [u8]) -> Result<Option<(u16, usize)>, libsurface::UiError> {
            Ok(None)
        }
        fn wait_event(&mut self, _buf: &mut [u8]) -> Result<(u16, usize), libsurface::UiError> {
            Err(libsurface::UiError::Malformed)
        }
    }

    #[test]
    fn only_a_panel_may_be_created_undrawn() {
        let size = Size::new(100, 30);
        for role in [Role::Normal, Role::Popup { parent: 1 }, Role::Dialog { parent: 1 }] {
            let mut session = Session::new(Counting::default());
            assert!(Child::create_sized(&mut session, role, size, 2).is_none());
            assert_eq!(session.into_transport().requests, 0, "{role:?} reached the compositor");
        }
        let mut session = Session::new(Counting::default());
        let panel = Role::Panel { dock: librsproto::surface::Edge::Bottom, reserve: 30 };
        assert!(Child::create_sized(&mut session, panel, size, 2).is_none(), "the mock refuses");
        assert!(session.into_transport().requests > 0, "a panel was never asked for");
    }

    #[test]
    fn a_window_takes_the_keyboard_once() {
        let key = WindowEvent::Key(librsproto::surface::KeyEvent::default());
        let (gain, loss) = (WindowEvent::Focus(true), WindowEvent::Focus(false));
        let mut had = false;
        assert!(!first_keyboard(&mut had, &loss), "a loss first is not the keyboard");
        assert!(!first_keyboard(&mut had, &key), "a key is not the keyboard");
        assert!(first_keyboard(&mut had, &gain), "the first gain is");
        assert!(!first_keyboard(&mut had, &loss));
        assert!(!first_keyboard(&mut had, &gain), "once: back from the parent is not announced");
        assert!(had, "kept");
        assert!(first_keyboard(&mut false, &gain));
    }
}
