//! Points, sizes and rectangles, and the clipping arithmetic over them.
//!
//! Coordinates are **signed**: a surface may sit partly off the left or top edge of
//! the screen, and clipping that correctly is one of the three properties the
//! reference scene exists to exercise (`docs/design/display-substrate.md` §8e).
//! Sizes are unsigned — a negative width has no meaning and should not be
//! representable.

/// A point in screen space. Signed, so off-screen origins are representable.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Point {
    /// Horizontal offset, increasing rightwards.
    pub x: i32,
    /// Vertical offset, increasing downwards.
    pub y: i32,
}

impl Point {
    /// A point at `(x, y)`.
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

/// A width and height in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Size {
    /// Width in pixels.
    pub w: u32,
    /// Height in pixels.
    pub h: u32,
}

impl Size {
    /// A size of `w × h` pixels.
    pub const fn new(w: u32, h: u32) -> Self {
        Self { w, h }
    }

    /// True when the size encloses no pixels.
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

/// An axis-aligned rectangle: an origin plus a size.
///
/// The rectangle covers `x .. x + w` and `y .. y + h`, so the right and bottom
/// edges are exclusive.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    /// Top-left corner.
    pub origin: Point,
    /// Extent from the origin.
    pub size: Size,
}

impl Rect {
    /// A rectangle at `(x, y)` of `w × h` pixels.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Self {
        Self { origin: Point::new(x, y), size: Size::new(w, h) }
    }

    /// A rectangle at the origin of `w × h` pixels.
    pub const fn from_size(size: Size) -> Self {
        Self { origin: Point::new(0, 0), size }
    }

    /// Left edge (inclusive).
    pub const fn left(&self) -> i32 {
        self.origin.x
    }

    /// Top edge (inclusive).
    pub const fn top(&self) -> i32 {
        self.origin.y
    }

    /// Right edge (**exclusive**).
    ///
    /// Widened to `i64` before the add: a rectangle placed near `i32::MAX` would
    /// otherwise overflow, and an overflow here silently produces a *smaller*
    /// rectangle, which reads as correct clipping rather than as a bug.
    pub const fn right(&self) -> i64 {
        self.origin.x as i64 + self.size.w as i64
    }

    /// Bottom edge (**exclusive**). Widened for the same reason as [`Rect::right`].
    pub const fn bottom(&self) -> i64 {
        self.origin.y as i64 + self.size.h as i64
    }

    /// True when the rectangle encloses no pixels.
    pub const fn is_empty(&self) -> bool {
        self.size.is_empty()
    }

    /// The overlap of two rectangles, or `None` when they do not overlap.
    ///
    /// This is the whole of clipping: a surface is drawn through the intersection of
    /// its own bounds, the damage rectangle, and the screen.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        let left = self.left().max(other.left());
        let top = self.top().max(other.top());
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        if right <= left as i64 || bottom <= top as i64 {
            return None;
        }
        Some(Rect::new(
            left,
            top,
            (right - left as i64) as u32,
            (bottom - top as i64) as u32,
        ))
    }

    /// True when `(x, y)` lies inside the rectangle.
    pub fn contains(&self, x: i32, y: i32) -> bool {
        (x as i64) >= (self.left() as i64)
            && (x as i64) < self.right()
            && (y as i64) >= (self.top() as i64)
            && (y as i64) < self.bottom()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_rects_intersect_to_the_shared_region() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(5, 5, 10, 10);
        assert_eq!(a.intersect(&b), Some(Rect::new(5, 5, 5, 5)));
        // Intersection is symmetric.
        assert_eq!(b.intersect(&a), Some(Rect::new(5, 5, 5, 5)));
    }

    #[test]
    fn disjoint_and_edge_touching_rects_do_not_intersect() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(&Rect::new(20, 20, 5, 5)), None);
        // Edges are exclusive: touching at x = 10 shares no pixel.
        assert_eq!(a.intersect(&Rect::new(10, 0, 5, 10)), None);
        assert_eq!(a.intersect(&Rect::new(0, 10, 10, 5)), None);
    }

    #[test]
    fn a_rect_off_the_left_edge_clips_to_the_visible_part() {
        // The case the reference scene exists to exercise: negative origin.
        let screen = Rect::new(0, 0, 64, 32);
        let surface = Rect::new(-6, 4, 16, 10);
        assert_eq!(surface.intersect(&screen), Some(Rect::new(0, 4, 10, 10)));
    }

    #[test]
    fn a_rect_off_the_bottom_right_clips_to_the_visible_part() {
        let screen = Rect::new(0, 0, 64, 32);
        let surface = Rect::new(56, 26, 16, 10);
        assert_eq!(surface.intersect(&screen), Some(Rect::new(56, 26, 8, 6)));
    }

    #[test]
    fn an_empty_rect_intersects_nothing() {
        let a = Rect::new(0, 0, 0, 10);
        assert_eq!(a.intersect(&Rect::new(0, 0, 10, 10)), None);
        assert_eq!(Rect::new(0, 0, 10, 10).intersect(&a), None);
    }

    #[test]
    fn edges_near_i32_max_do_not_overflow_into_a_smaller_rect() {
        // `right()`/`bottom()` widen to i64 precisely so this stays a huge rectangle
        // rather than wrapping to a tiny one that would clip everything away.
        let r = Rect::new(i32::MAX - 1, 0, 16, 4);
        assert_eq!(r.right(), i32::MAX as i64 + 15);
        let screen = Rect::new(0, 0, 64, 32);
        assert_eq!(r.intersect(&screen), None);
    }

    #[test]
    fn contains_uses_exclusive_right_and_bottom_edges() {
        let r = Rect::new(2, 3, 4, 5);
        assert!(r.contains(2, 3));
        assert!(r.contains(5, 7));
        assert!(!r.contains(6, 7));
        assert!(!r.contains(5, 8));
        assert!(!r.contains(1, 3));
    }
}
