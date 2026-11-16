//! Where a popup lands when it asks to be kept on screen (`rsproto-surface-ops.md` § `CreateWindow`).

use libdraw::geom::{Point, Rect};
use librsproto::surface::Placement;

/// The origin of a popup of `size` anchored at `anchor`, placed `how` against `area`. **In `i64`**:
/// the anchor is a client's saturated offset and the size any `u32`, so a flip in `i32` could wrap.
pub fn place(anchor: Point, size: (u32, u32), area: Rect, how: Placement) -> Point {
    if how == Placement::AsAsked {
        return anchor;
    }
    let flip = how == Placement::AtPoint;
    Point::new(
        axis(anchor.x, size.0, area.origin.x, area.size.w, flip),
        axis(anchor.y, size.1, area.origin.y, area.size.h, flip),
    )
}

/// One axis. **Longer than the area, it takes the area's start**, so a menu's first rows show.
fn axis(at: i32, len: u32, start: i32, span: u32, flip: bool) -> i32 {
    let (at, len, lo) = (i64::from(at), i64::from(len), i64::from(start));
    let hi = lo + i64::from(span);
    let mut from = at;
    if flip && at + len > hi {
        from = at - len;
    }
    from = from.min(hi - len).max(lo);
    i32::try_from(from).unwrap_or(start)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 30, 1280, 740);
    const MENU: (u32, u32) = (200, 300);

    fn at(x: i32, y: i32, how: Placement) -> Point {
        place(Point::new(x, y), MENU, AREA, how)
    }

    #[test]
    fn as_asked_is_where_it_was_asked_wherever_that_is() {
        for (x, y) in [(5, 5), (1200, 700), (-50, 900), (i32::MIN, i32::MAX)] {
            assert_eq!(at(x, y, Placement::AsAsked), Point::new(x, y));
        }
    }

    #[test]
    fn inside_moves_an_overhang_inward_by_exactly_the_overhang() {
        let (right, bottom) = (1280 - 200, 30 + 740 - 300);
        assert_eq!(at(right, bottom, Placement::Inside), Point::new(right, bottom), "flush stays");
        assert_eq!(at(right + 1, bottom + 1, Placement::Inside), Point::new(right, bottom), "one over, one back");
        assert_eq!(at(-1, 29, Placement::Inside), Point::new(0, 30), "the left edge and the top bar");
        assert_eq!(at(0, 30, Placement::Inside), Point::new(0, 30));
    }

    #[test]
    fn at_a_point_flips_before_the_anchor_on_each_axis_it_would_overhang() {
        let (right, bottom) = (1280 - 200, 30 + 740 - 300);
        assert_eq!(at(right, bottom, Placement::AtPoint), Point::new(right, bottom), "flush: no flip");
        assert_eq!(at(right + 1, 100, Placement::AtPoint), Point::new(right + 1 - 200, 100), "opens to the left");
        assert_eq!(at(100, bottom + 1, Placement::AtPoint), Point::new(100, bottom + 1 - 300), "opens upward");
        assert_eq!(at(1270, 760, Placement::AtPoint), Point::new(1070, 460), "the corner: both");
    }

    #[test]
    fn a_flip_that_does_not_fit_is_moved_inward_from_the_edge_it_crossed() {
        let tall = place(Point::new(100, 200), (200, 600), AREA, Placement::AtPoint);
        assert_eq!(tall, Point::new(100, 30), "flipped up past the top bar, moved down to it");
        let wide = place(Point::new(150, 100), (1200, 100), AREA, Placement::AtPoint);
        assert_eq!(wide, Point::new(0, 100), "flipped left past the screen's edge, moved back to it");
    }

    #[test]
    fn a_popup_larger_than_the_area_takes_its_top_left() {
        for how in [Placement::Inside, Placement::AtPoint] {
            assert_eq!(place(Point::new(500, 500), (1300, 800), AREA, how), Point::new(0, 30), "{how:?}");
        }
    }

    #[test]
    fn the_extremes_neither_wrap_nor_leave_the_area() {
        for how in [Placement::Inside, Placement::AtPoint] {
            for (x, y) in [(i32::MIN, i32::MIN), (i32::MAX, i32::MAX), (i32::MIN, i32::MAX)] {
                let p = at(x, y, how);
                assert!(p.x >= 0 && p.x <= 1280 - 200 && p.y >= 30 && p.y <= 30 + 740 - 300, "{how:?} {p:?}");
            }
            let huge = place(Point::new(i32::MAX, i32::MIN), (u32::MAX, u32::MAX), AREA, how);
            assert_eq!(huge, Point::new(0, 30), "{how:?}: wider than any area, at its start");
        }
    }

    #[test]
    fn an_empty_work_area_puts_every_popup_at_its_start() {
        let none = Rect::new(0, 400, 1280, 0);
        assert_eq!(place(Point::new(10, 10), MENU, none, Placement::AtPoint), Point::new(10, 400));
    }

    #[test]
    fn create_places_a_popup_against_the_stacks_own_work_area() {
        use crate::WindowStack;
        use crate::server::{Connection, Outcome, dispatch};
        use librsproto::surface::{CreateWindowRequest, Edge, OP_CREATE_WINDOW, Role, build_create_window_request};

        fn create(conn: &mut Connection, stack: &mut WindowStack, req: CreateWindowRequest) -> (u32, Point) {
            let mut body = [0u8; 32];
            let n = build_create_window_request(&mut body, &req).expect("encodes");
            match dispatch(conn, stack, OP_CREATE_WINDOW, &body[..n], &mut [0u8; 32]) {
                Outcome::Created { configure, .. } => {
                    stack.mark_configured(configure.window);
                    (configure.window, Point::new(configure.x, configure.y))
                }
                other => panic!("not created: {other:?}"),
            }
        }
        let screen = Rect::new(0, 0, 1280, 800);
        for (stack, placed) in [(WindowStack::new().on_screen(screen), true), (WindowStack::new(), false)] {
            let (mut stack, mut conn) = (stack, Connection::new());
            for dock in [Edge::Top, Edge::Bottom] {
                create(&mut conn, &mut stack, CreateWindowRequest::new(1280, 30, Role::Panel { dock, reserve: 30 }));
            }
            let (window, _) = create(&mut conn, &mut stack, CreateWindowRequest::at(600, 700, Role::Normal, 0, 40));
            let popup = |at: (i32, i32), how| {
                CreateWindowRequest::at(MENU.0, MENU.1, Role::Popup { parent: window }, at.0, at.1).placed(how)
            };
            let landed =
                |stack: &mut WindowStack, conn: &mut Connection, at, how| create(conn, stack, popup(at, how)).1;
            let near_corner = (1270, 720);
            let want = if placed { Point::new(1070, 460) } else { Point::new(1270, 760) };
            assert_eq!(landed(&mut stack, &mut conn, near_corner, Placement::AtPoint), want, "placed: {placed}");
            let want = if placed { Point::new(1080, 470) } else { Point::new(1270, 760) };
            assert_eq!(landed(&mut stack, &mut conn, near_corner, Placement::Inside), want, "placed: {placed}");
            let asked = landed(&mut stack, &mut conn, near_corner, Placement::AsAsked);
            assert_eq!(asked, Point::new(1270, 760), "as asked is as asked, with a screen or none");
        }
    }
}
