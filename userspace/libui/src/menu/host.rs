//! A window's menu as a window: the popup the shown menu lives in, opened, switched and closed to
//! follow a [`MenuState`](super::MenuState), and the events it receives.

use alloc::vec::Vec;

use libdraw::geom::Size;
use libdraw::text::Font;
use librsproto::surface::{KeyEvent, Placement, Role};
use libsurface::{Session, Transport, WindowEvent};

use super::Shown;
use crate::element::Element;
use crate::paint::Theme;
use crate::window::Child;

/// What one [`Host::sync`] did, for its owner's log lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Synced {
    /// A window this pass opened: its id, its origin as configured, and its size.
    pub opened: Option<(u32, (i32, i32), Size)>,
    /// A window was wanted and could not be opened. The owner closes its state, or every pass of
    /// its loop asks again.
    pub failed: bool,
    /// The row under the pointer changed to this.
    pub hover: Option<Option<u64>>,
    /// This frame could not be drawn.
    pub undrawn: bool,
}

/// What one of the popup's events asks of its owner.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Routed<Msg> {
    /// A press landed outside the window showing this; [`MenuState::dismissed`](super::MenuState::dismissed).
    Dismissed(Shown),
    /// A key, for [`MenuState::choose`](super::MenuState::choose): the popup holds the keyboard.
    Key(KeyEvent),
    /// What a pointer event chose.
    Msgs(Vec<Msg>),
}

/// The popup one window's menus are shown in, the only copy of it.
pub struct Host {
    child: Option<Child>,
    /// What `child` shows.
    shown: Option<Shown>,
    /// The hover the popup was last drawn with: a gesture routes against that tree.
    hovered: Option<u64>,
    buffers: usize,
}

impl Host {
    /// No window yet, one to be opened with `buffers` buffers.
    pub const fn new(buffers: usize) -> Self {
        Self { child: None, shown: None, hovered: None, buffers }
    }

    /// Whether `window` is this host's popup: an event from it belongs to [`event`](Self::event).
    pub fn owns(&self, window: u32) -> bool {
        self.child.as_ref().is_some_and(|c| c.id() == window)
    }

    /// What the popup shows.
    pub fn shown(&self) -> Option<Shown> {
        self.shown
    }

    /// Make the popup show `want`: open it, switch it, redraw it or close it. `view` builds a menu's
    /// popup, given which and the row under the pointer.
    ///
    /// **Keyed on which menu, not whether one**: File to Edit rebuilds the window, whose size was
    /// fixed when it opened.
    #[allow(clippy::too_many_arguments, reason = "the session, the window and what it shows, each separate")]
    pub fn sync<T: Transport, Msg>(
        &mut self,
        session: &mut Session<T>,
        parent: u32,
        want: Option<(Shown, (i32, i32))>,
        view: impl Fn(Shown, Option<u64>) -> Element<Msg>,
        font: &Font,
        theme: &Theme,
    ) -> Synced {
        let mut out = Synced::default();
        if want.map(|(shown, _)| shown) != self.shown {
            self.close(session);
            if let Some((shown, at)) = want {
                // A drop-down is moved inside the work area; a context menu flips about the pointer.
                let how = match shown {
                    Shown::Bar(_) => Placement::Inside,
                    Shown::Context(_) => Placement::AtPoint,
                };
                // Measured with no hover: a highlight does not change a menu's size.
                let content = view(shown, None);
                let role = Role::Popup { parent };
                let Some(c) = Child::open_placed(session, role, at, how, &content, font, theme, self.buffers) else {
                    out.failed = true;
                    return out;
                };
                let origin = session.window(c.id()).and_then(|w| w.configured()).map_or((0, 0), |c| (c.x, c.y));
                out.opened = Some((c.id(), origin, c.size()));
                self.child = Some(c);
                self.shown = Some(shown);
            }
        }
        if let (Some(c), Some(shown)) = (self.child.as_mut(), self.shown) {
            let now = c.hovered_key();
            if now != self.hovered {
                self.hovered = now;
                out.hover = Some(now);
            }
            out.undrawn = !c.present(session, &view(shown, now), font, theme);
        }
        out
    }

    /// Route one of the popup's events; `view` as for [`sync`](Self::sync).
    pub fn event<Msg: Clone>(
        &mut self,
        event: &WindowEvent,
        view: impl Fn(Shown, Option<u64>) -> Element<Msg>,
        font: &Font,
        theme: &Theme,
    ) -> Routed<Msg> {
        let (Some(c), Some(shown)) = (self.child.as_mut(), self.shown) else { return Routed::Msgs(Vec::new()) };
        match event {
            WindowEvent::Dismissed => Routed::Dismissed(shown),
            WindowEvent::Key(k) => Routed::Key(*k),
            _ => Routed::Msgs(c.route(&view(shown, self.hovered), font, theme, event)),
        }
    }

    /// Destroy the popup, if one is up.
    pub fn close<T: Transport>(&mut self, session: &mut Session<T>) {
        if let Some(c) = self.child.take() {
            c.close(session);
        }
        self.shown = None;
        self.hovered = None;
    }
}

#[cfg(test)]
mod tests;
