//! Per-buffer damage: what must be repainted into the buffer you just got back.
//!
//! **"Per buffer" and "per frame" are not alternatives.** They answer different questions,
//! and a first implementation typically notices only one of them:
//!
//! | Question | Answer |
//! |---|---|
//! | What must the **client repaint** into the buffer it just acquired? | Damage since *that buffer* was last drawn — per buffer |
//! | What must the **compositor copy** to the screen? | Damage since the *last commit* — per frame |
//!
//! A window has two or more buffers; the client draws into a free one while the compositor
//! reads another. Suppose frame *n* changes rectangle A and is drawn into buffer 0, and
//! frame *n+1* changes B into buffer 1. When buffer 0 comes back its pixels are frame *n*'s
//! — it already has A, but it is missing B. Repainting only the current frame's delta leaves
//! the rest stale.
//!
//! So each buffer accumulates the damage of every frame drawn *while it was away*, and
//! [`BufferDamage::begin_frame`] hands that back and clears it in one step, so a caller
//! cannot repaint without also resetting.
//!
//! ## Why the commit carries the same rectangle
//!
//! What `begin_frame` returns is what gets painted, and it is also what should go in the
//! `Surface::Commit` damage. That is a *superset* of what actually changed on screen — the
//! compositor copies slightly more than it must, which is safe, where copying less is not.
//! Splitting the two quantities to send a minimal rectangle is a later optimisation and
//! needs a reason; at one blit per frame there is none.
//!
//! **This is the piece that only fails under load.** With a compositor that releases a
//! buffer before the next frame, every buffer is drawn every frame and the accumulation is
//! always empty — so a wrong implementation looks perfect until something is slow enough to
//! hold a buffer for more than one frame.

use alloc::vec::Vec;

use libdraw::geom::Rect;

/// The smallest rectangle containing both.
pub fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.origin.x.min(b.origin.x);
    let y0 = a.origin.y.min(b.origin.y);
    let x1 = a.right().max(b.right());
    let y1 = a.bottom().max(b.bottom());
    Rect::new(x0, y0, (x1 - x0 as i64) as u32, (y1 - y0 as i64) as u32)
}

/// [`union`] over optional rectangles, treating `None` and zero-extent as "nothing".
///
/// Zero-extent is filtered rather than unioned: a `0×0` rectangle at the origin covers no
/// pixel, but unioning it drags every result back to the corner, repainting the whole window
/// for an edit far from it.
pub fn union_opt(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    let live = |r: Option<Rect>| r.filter(|r| r.size.w != 0 && r.size.h != 0);
    match (live(a), live(b)) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => Some(union(a, b)),
    }
}

/// Outstanding damage for each of a window's buffers.
pub struct BufferDamage {
    /// Damage owed to each buffer, indexed by buffer slot.
    pending: Vec<Option<Rect>>,
}

impl BufferDamage {
    /// `count` buffers, every one **fully damaged**.
    ///
    /// Fully, because a buffer that has never been drawn holds whatever the memory held —
    /// there is no frame it is merely behind by. The first paint of each buffer is a full
    /// paint, and there is exactly one per buffer.
    pub fn new(count: usize, bounds: Rect) -> Self {
        Self { pending: alloc::vec![Some(bounds); count] }
    }

    /// How many buffers this tracks.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Whether it tracks none.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Damage owed to `buffer` without consuming it. Diagnostic; the loop uses
    /// [`begin_frame`](Self::begin_frame).
    pub fn owed(&self, buffer: usize) -> Option<Rect> {
        self.pending.get(buffer).copied().flatten()
    }

    /// Start a frame: return everything that must be painted into `buffer`, and record that
    /// `frame_damage` changed the window.
    ///
    /// The returned rectangle is this frame's own damage unioned with everything `buffer`
    /// missed while it was held. Pass it to `Surface::Commit` as the damage rectangle too.
    ///
    /// **One call rather than a query and a clear**, because the two must not come apart: a
    /// caller that repainted and forgot to clear would repaint the same region for the rest
    /// of the window's life, and one that cleared without painting would leave pixels stale
    /// forever. Neither is representable here.
    ///
    /// An out-of-range `buffer` returns `None` and changes nothing. A toolkit that panicked
    /// would take the application down over a bookkeeping mistake.
    pub fn begin_frame(&mut self, buffer: usize, frame_damage: Option<Rect>) -> Option<Rect> {
        if buffer >= self.pending.len() {
            return None;
        }
        let repaint = union_opt(self.pending[buffer], frame_damage);
        // **This frame's damage only**, not the whole repainted region. Every other buffer
        // has already accumulated the frames before this one, so adding the union would
        // re-add history they hold and grow the rectangle for no reason.
        for (i, slot) in self.pending.iter_mut().enumerate() {
            if i != buffer {
                *slot = union_opt(*slot, frame_damage);
            }
        }
        self.pending[buffer] = None;
        repaint
    }

    /// Damage every buffer fully — what a resize does.
    ///
    /// Every rectangle is stale after a resize, and re-deriving that node by node is slower
    /// and no more correct than repainting.
    pub fn reset_all(&mut self, bounds: Rect) {
        for slot in &mut self.pending {
            *slot = Some(bounds);
        }
    }

    /// Change the buffer count, damaging everything.
    pub fn resize(&mut self, count: usize, bounds: Rect) {
        self.pending = alloc::vec![Some(bounds); count];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect::new(0, 0, 640, 480);
    const A: Rect = Rect::new(0, 0, 10, 10);
    const B: Rect = Rect::new(100, 100, 10, 10);
    const C: Rect = Rect::new(200, 200, 10, 10);

    /// Two buffers, both already drawn once, so the initial full damage is out of the way.
    fn settled() -> BufferDamage {
        let mut d = BufferDamage::new(2, SCREEN);
        d.begin_frame(0, None);
        d.begin_frame(1, None);
        d
    }

    #[test]
    fn every_buffer_starts_fully_damaged() {
        // A buffer never drawn holds whatever the memory held — it is not merely behind.
        let d = BufferDamage::new(3, SCREEN);
        for i in 0..3 {
            assert_eq!(d.owed(i), Some(SCREEN));
        }
    }

    #[test]
    fn a_frame_clears_its_own_buffer_and_charges_the_others() {
        let mut d = settled();
        assert_eq!(d.begin_frame(0, Some(A)), Some(A));
        assert_eq!(d.owed(0), None, "just painted");
        assert_eq!(d.owed(1), Some(A), "missed it");
    }

    #[test]
    fn a_buffer_held_for_several_frames_gets_back_everything_it_missed() {
        // The §6.2 scenario, and the reason this module exists. Buffer 0 is drawn with A,
        // then two frames happen elsewhere; when it returns it already has A but is missing
        // B and C.
        let mut d = settled();
        d.begin_frame(0, Some(A));
        d.begin_frame(1, Some(B));

        let repaint = d.begin_frame(0, Some(C)).expect("something to paint");
        assert_eq!(repaint, union(B, C), "B and C, and not A — buffer 0 already had A");
        assert!(repaint.contains(B.origin.x, B.origin.y));
        assert!(repaint.contains(C.origin.x, C.origin.y));
    }

    #[test]
    fn a_frame_that_changed_nothing_still_repaints_what_the_buffer_missed() {
        // The client has nothing new to say, but the buffer it was handed is two frames
        // stale. Returning `None` here is the bug that leaves a window showing an old frame
        // until something unrelated changes.
        let mut d = settled();
        d.begin_frame(1, Some(B));
        assert_eq!(d.begin_frame(0, None), Some(B));
    }

    #[test]
    fn accumulation_is_this_frames_damage_not_the_whole_repainted_region() {
        // Buffer 0 owes B; a frame drawn into it repaints B ∪ C. If the *other* buffer were
        // charged that union it would re-acquire B, which it already has — the rectangle
        // grows every frame and eventually covers the window.
        let mut d = settled();
        d.begin_frame(1, Some(B));
        assert_eq!(d.begin_frame(0, Some(C)), Some(union(B, C)));
        assert_eq!(d.owed(1), Some(C), "only what buffer 1 actually missed");
    }

    #[test]
    fn painting_twice_in_a_row_does_not_repaint_the_first_frame_again() {
        // `begin_frame` clears as it hands back, so a caller cannot forget to.
        let mut d = settled();
        d.begin_frame(0, Some(A));
        assert_eq!(d.begin_frame(0, None), None, "nothing outstanding for this buffer");
    }

    #[test]
    fn a_resize_damages_every_buffer_including_the_one_in_hand() {
        let mut d = settled();
        d.begin_frame(0, Some(A));
        let bigger = Rect::new(0, 0, 800, 600);
        d.reset_all(bigger);
        assert_eq!(d.owed(0), Some(bigger));
        assert_eq!(d.owed(1), Some(bigger));
    }

    #[test]
    fn an_out_of_range_buffer_is_refused_rather_than_panicking() {
        // A toolkit that panics takes the application with it, over bookkeeping.
        let mut d = settled();
        assert_eq!(d.begin_frame(7, Some(A)), None);
        assert_eq!(d.owed(0), None, "and nothing was charged to anyone");
        assert_eq!(d.owed(1), None);
        assert_eq!(d.owed(7), None);
    }

    #[test]
    fn union_covers_both_and_does_not_care_about_order() {
        assert_eq!(union(A, B), Rect::new(0, 0, 110, 110));
        assert_eq!(union(B, A), Rect::new(0, 0, 110, 110));
        // Containment is idempotent rather than growing.
        assert_eq!(union(SCREEN, A), SCREEN);
    }

    #[test]
    fn a_zero_extent_rectangle_is_nothing_rather_than_a_corner() {
        // Unioning `0×0` at the origin would drag every result back to (0,0), repainting
        // the whole window for an edit in the far corner.
        let empty = Rect::new(0, 0, 0, 0);
        assert_eq!(union_opt(Some(B), Some(empty)), Some(B));
        assert_eq!(union_opt(Some(empty), Some(B)), Some(B));
        assert_eq!(union_opt(Some(empty), None), None);
        assert_eq!(union_opt(None, None), None);
    }

    #[test]
    fn three_buffers_each_track_their_own_debt() {
        // Two buffers can hide an indexing bug that charges "the other one" rather than
        // "every other one".
        let mut d = BufferDamage::new(3, SCREEN);
        for i in 0..3 {
            d.begin_frame(i, None);
        }
        d.begin_frame(0, Some(A));
        assert_eq!(d.owed(1), Some(A));
        assert_eq!(d.owed(2), Some(A));

        d.begin_frame(1, Some(B));
        assert_eq!(d.owed(0), Some(B), "buffer 0 missed B");
        assert_eq!(d.owed(2), Some(union(A, B)), "buffer 2 missed both");
    }
}
