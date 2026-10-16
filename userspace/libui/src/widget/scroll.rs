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
/// A thumb proportional to a long scrollback shrinks to nothing and stops being grabbable;
/// every real scrollbar clamps it, and the clamp is what makes the position arithmetic
/// non-obvious enough to be worth testing.
pub const MIN_THUMB: u32 = 16;

impl ScrollState {
    /// Whether there is anything to scroll.
    pub fn scrollable(&self) -> bool {
        self.total > self.visible
    }

    /// The thumb's length and offset along a track of `track` pixels.
    ///
    /// Returns `(offset, length)`. With nothing to scroll the thumb fills the track, which is
    /// how a user sees "this is all of it" rather than seeing a full-height thumb that
    /// nevertheless moves.
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
    /// [`thumb`](Self::thumb)'s inverse, and the half M4 did not ship: the toolkit could say
    /// where a thumb goes for a given offset but not what offset a grab means, so a scrollbar
    /// was a picture of a scrollbar. The terminal is the first thing to want to *use* one,
    /// which is the milestone rule working as intended — see the decision log, 2026-08-12.
    ///
    /// **The thumb's centre rather than its top**, which is what "jump to here" means for a
    /// press on the *track*. It is the wrong answer for a press on the **thumb** — that
    /// re-centres a thumb the person grabbed by its end, so it jumps before the drag begins —
    /// and [`ScrollGrab`] is what tells the two presses apart. This is the track half.
    ///
    /// `y` is signed because a drag routinely leaves the widget: the router hands a captured
    /// widget negative coordinates rather than clamping, and this clamps at the ends instead —
    /// which is what makes dragging past the bottom stay at the bottom.
    pub fn offset_at(&self, track: u32, y: i32) -> u32 {
        let (_, len) = self.thumb(track);
        self.offset_for_thumb_top(track, y - len as i32 / 2)
    }

    /// The offset that puts the thumb's **top** at `top` pixels down a track of `track`.
    ///
    /// [`thumb`](Self::thumb)'s inverse, and the half M4 did not ship: the toolkit could say
    /// where a thumb goes for a given offset but not what offset a grab means, so a scrollbar
    /// was a picture of a scrollbar. The terminal is the first thing to want to *use* one,
    /// which is the milestone rule working as intended — see the decision log, 2026-08-12.
    ///
    /// **The top rather than the centre**, because a drag that keeps the thumb under the cursor
    /// has to keep it under the *same part* of the cursor it was taken by — see [`ScrollGrab`].
    /// It took until M14 Part I to say so: `offset_at` centred, so every grab moved the thumb
    /// before the drag began.
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
        // **Truncating, like [`thumb`](Self::thumb)**, which is what keeps the pair consistent:
        // the round trip is exact wherever the division is, and neither end needs help — at the
        // bottom `pos` is `span`, so `span * max / span` is `max` however it rounds. A first
        // version added `span / 2` here for the stated reason that the last line was otherwise
        // unreachable, which is simply not true; deleting it left every test green, so it was
        // half a line of mid-drag accuracy bought with a claim that did not hold.
        ((pos as u64 * max_offset as u64) / span as u64) as u32
    }
}

/// Where within the thumb a scrollbar drag took hold.
///
/// **The interaction state a scrollbar needs and a widget cannot keep.** `widget-toolkit.md` §3
/// reserves retained state for things the application has no opinion about, and this qualifies:
/// nobody has an opinion about where inside a thumb a button landed. It lives in a small value
/// the application holds — the same shape as [`click::Clicks`](crate::click::Clicks), and for
/// the same reason: the toolkit's widgets are rebuilt every frame and have nowhere to put it.
///
/// **What it buys.** Without it the only inverse available is "put the thumb's centre under the
/// cursor", so grabbing a thumb near either end makes it jump by up to half its length before
/// the drag begins — the defect the `scroll-grab` deferral recorded from M4 until M14 Part I.
/// With it,
/// a press on the thumb moves nothing and the thumb then follows the pointer exactly.
///
/// A press on the **track** still jumps, because that is what a track click means; the thumb
/// centres on the cursor and is held there for the rest of that drag.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScrollGrab {
    /// Pixels between the thumb's top and where the press landed, while a drag is running.
    within: Option<i32>,
    /// Which button took the grab — the only one whose events this drag answers.
    ///
    /// **The router hands a captured widget every button's events**, and drops its own capture
    /// only when *all* of them come up, so a second button tapped mid-drag arrives here as a
    /// press and a release belonging to a gesture this grab is not part of. The compositor keeps
    /// `grab_button` for the identical reason one layer down (PR #248 review, blocking 1).
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
    /// `None` means this event is not part of a drag — a hover, a crossing, or the release that
    /// ends one — and the caller should leave its offset alone.
    ///
    /// `state` is the bar as it is *now* and `track` the height it was drawn at; both are the
    /// numbers the caller passed to [`scrollbar`], and a drag converted against a different
    /// geometry from the one drawn puts the thumb where the pointer is not.
    pub fn apply(&mut self, state: ScrollState, track: u32, ev: PointerEvent) -> Option<u32> {
        let is_button = ev.kind == POINTER_BUTTON;
        // **Only the button that took the grab is answered.** Pressing a second one mid-drag
        // would otherwise re-take the grab from wherever the cursor happens to be — a jump, if
        // that is off the thumb — and its *release* would end a gesture the person is still
        // making, leaving the bar stranded while the router goes on delivering to it. The old
        // `if p.buttons != 0` test in each application kept following (PR #288 review, 1).
        if is_button && self.within.is_some() && ev.button != self.button {
            return None;
        }
        if is_button && ev.flags & POINTER_PRESSED == 0 {
            // The release, which the router delivers to the captured widget wherever the
            // cursor has got to. Nothing moves; the grab ends.
            self.within = None;
            return None;
        }
        if is_button {
            let (pos, len) = state.thumb(track);
            let (top, bottom) = (pos as i32, pos as i32 + len as i32);
            if ev.y >= top && ev.y < bottom {
                // **On the thumb: nothing moves.** Remembering where within it the press
                // landed is the whole of this type, and returning the offset unchanged —
                // rather than one recomputed from the thumb's position — keeps a grab exact
                // where the position arithmetic truncates.
                self.within = Some(ev.y - top);
                self.button = ev.button;
                return Some(state.offset);
            }
            // On the track: jump, then hold the thumb centred for the rest of the drag.
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
/// Built out of layout rather than arithmetic on a canvas: a `Column` of a spacer, the thumb
/// and a filler places the thumb without any node needing to offset its child. That is why
/// the toolkit has no `Offset` primitive — this was the thing that would have wanted one.
///
/// **`height` must be the height the parent will actually give it.** The bar sizes itself
/// `width × 0` — full height of whatever slot it lands in, per `sized`'s zero-axis rule — but
/// `height` is what the thumb's length and position are computed against, and the two are not
/// connected. Pass a smaller number and the thumb stops short of the bottom at the last line;
/// pass a larger one and it runs off the end. A caller in a `Dock` therefore has to subtract
/// whatever the other edges took, which is what [`reference::view`](crate::reference::view)
/// does. Making the widget measure itself would need a second layout pass, which the toolkit
/// does not have; until it does, this is an obligation on the caller rather than a guarantee
/// (PR #185 review, finding 7).
pub fn scrollbar<Msg>(state: ScrollState, width: u32, height: u32, theme: &Theme) -> Element<Msg> {
    let (pos, len) = state.thumb(height);
    sized(
        Size::new(width, 0),
        stack(alloc::vec![
            fill(theme.groove),
            column(alloc::vec![
                sized(Size::new(0, pos), fill(theme.groove)),
                sized(Size::new(0, len), bevel(theme.thumb)),
                // The remainder, so the thumb does not stretch to the bottom.
                fill(theme.groove).flex(1),
            ]),
        ]),
    )
}

/// How wide a list's scrollbar is, in pixels.
pub(super) const SCROLLBAR_W: u32 = 12;
