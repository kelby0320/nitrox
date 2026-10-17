//! Scroll state, the scrollbar and its grab.

use super::*;

/// Where a scrollbar is and how much of its content is visible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ScrollState {
    /// First visible unit — a line, for a terminal.
    pub offset: u32,
    /// How many units are visible at once.
    pub visible: u32,
    /// How many units there are in total.
    pub total: u32,
}

/// The smallest a thumb may get, in pixels.
///
/// A thumb proportional to a long scrollback would shrink to nothing and stop being grabbable.
pub const MIN_THUMB: u32 = 16;

impl ScrollState {
    /// Whether there is anything to scroll.
    pub fn scrollable(&self) -> bool {
        self.total > self.visible
    }

    /// The thumb's length and offset along a track of `track` pixels.
    ///
    /// Returns `(offset, length)`. With nothing to scroll the thumb fills the track.
    pub fn thumb(&self, track: u32) -> (u32, u32) {
        if !self.scrollable() || self.visible == 0 || track == 0 {
            return (0, track);
        }
        // Proportional, then clamped — and clamped *before* the position is computed, or the
        // thumb runs past the end of the track by however much the clamp added.
        let proportional = (track as u64 * self.visible as u64 / self.total as u64) as u32;
        let len = proportional.max(MIN_THUMB).min(track);
        let span = track - len;
        let max_offset = self.total - self.visible;
        let pos = (span as u64 * self.offset.min(max_offset) as u64 / max_offset as u64) as u32;
        (pos, len)
    }

    /// The offset that puts the thumb's **centre** at `y` pixels down a track of `track`.
    ///
    /// The track half of [`thumb`](Self::thumb)'s inverse; a press on the thumb uses
    /// [`ScrollGrab`] instead. `y` is signed because the router hands a captured widget negative
    /// coordinates; this clamps at the ends.
    pub fn offset_at(&self, track: u32, y: i32) -> u32 {
        let (_, len) = self.thumb(track);
        self.offset_for_thumb_top(track, y - len as i32 / 2)
    }

    /// The offset that puts the thumb's **top** at `top` pixels down a track of `track`, so a
    /// drag keeps the thumb under the part of the cursor it was taken by ([`ScrollGrab`]).
    pub fn offset_for_thumb_top(&self, track: u32, top: i32) -> u32 {
        let max_offset = self.total.saturating_sub(self.visible);
        if max_offset == 0 || track == 0 {
            return 0;
        }
        let (_, len) = self.thumb(track);
        let span = track.saturating_sub(len);
        if span == 0 {
            // A thumb filling its track cannot express a position. Anything but the top would
            // be invented, and `MIN_THUMB` makes this reachable on a short bar.
            return 0;
        }
        let pos = top.clamp(0, span as i32) as u32;
        // Truncating, like [`thumb`](Self::thumb), which keeps the pair consistent: at the bottom
        // `pos` is `span`, so `span * max / span` is `max` however it rounds.
        ((pos as u64 * max_offset as u64) / span as u64) as u32
    }
}

/// Where within the thumb a scrollbar drag took hold.
///
/// Held by the application, like [`click::Clicks`](crate::click::Clicks), as widgets are rebuilt
/// every frame. A press on the thumb moves nothing; a press on the **track** jumps, centring
/// the thumb for the rest of that drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScrollGrab {
    /// Pixels between the thumb's top and where the press landed, while a drag is running.
    within: Option<i32>,
    /// Which button took the grab, the only one whose events this drag answers: the router
    /// hands a captured widget every button's events until all are up.
    button: u16,
}

impl ScrollGrab {
    /// A tracker holding nothing.
    pub const fn new() -> Self {
        Self { within: None, button: 0 }
    }

    /// Whether a drag is in progress.
    pub fn dragging(&self) -> bool {
        self.within.is_some()
    }

    /// Apply one pointer event over the bar; answer where the bar should now be.
    ///
    /// `None` means this event is not part of a drag. `state` and `track` are the numbers passed
    /// to [`scrollbar`].
    pub fn apply(&mut self, state: ScrollState, track: u32, ev: PointerEvent) -> Option<u32> {
        let is_button = ev.kind == POINTER_BUTTON;
        // Only the button that took the grab is answered: a second button's press would re-take
        // the grab, and its release end a gesture the person is still making.
        if is_button && self.within.is_some() && ev.button != self.button {
            return None;
        }
        if is_button && ev.flags & POINTER_PRESSED == 0 {
            self.within = None;
            return None;
        }
        if is_button {
            let (pos, len) = state.thumb(track);
            let (top, bottom) = (pos as i32, pos as i32 + len as i32);
            if ev.y >= top && ev.y < bottom {
                // On the thumb: nothing moves. Returning the offset unchanged keeps a grab exact
                // where the position arithmetic truncates.
                self.within = Some(ev.y - top);
                self.button = ev.button;
                return Some(state.offset);
            }
            self.within = Some(len as i32 / 2);
            self.button = ev.button;
            return Some(state.offset_at(track, ev.y));
        }
        let within = self.within?;
        if ev.buttons == 0 {
            // A motion with nothing held after a grab we never saw released — the router
            // suppresses crossings mid-capture, so this is a lost release rather than a hover.
            // Ending the drag beats following a pointer nobody is pressing.
            self.within = None;
            return None;
        }
        Some(state.offset_for_thumb_top(track, ev.y - within))
    }
}

/// A vertical scrollbar `width` pixels across.
///
/// `height` must be the height the parent will actually give it: the bar sizes itself
/// `width × 0` but the thumb is computed against `height`. A caller in a `Dock` subtracts what
/// the other edges took, as [`reference::view`](crate::reference::view) does.
pub fn scrollbar<Msg>(state: ScrollState, width: u32, height: u32, theme: &Theme) -> Element<Msg> {
    let (pos, len) = state.thumb(height);
    sized(
        Size::new(width, 0),
        stack(alloc::vec![
            fill(theme.groove),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(theme.groove)),
                sized(Size::new(0, len), bevel(theme.thumb)),
                fill(theme.groove).flex(1),
            ]),
        ]),
    )
}

/// How wide a list's scrollbar is, in pixels.
pub(super) const SCROLLBAR_W: u32 = 12;
