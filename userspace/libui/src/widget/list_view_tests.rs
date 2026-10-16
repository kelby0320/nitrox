    use super::*;
    use crate::element::Node;
    use libdraw::format::Rgb;

    fn rows<'a>(labels: &'a [(u64, &'a str)]) -> alloc::vec::Vec<ListRow<'a>> {
        labels.iter().map(|&(key, label)| ListRow { key, label, marked: false, ..Default::default() }).collect()
    }

    /// A heading sits over its column whether or not the list scrolls.
    ///
    /// **The doc says "the two cannot drift apart" and they did** (PR #320 review, worth fixing
    /// 5): a list that overflows puts its rows beside a scrollbar, so every column moved twelve
    /// pixels left of its heading — in any directory long enough to scroll, which is most.
    /// Measured as the right edge of each, which is what has to agree.
    #[test]
    fn a_heading_sits_over_its_column_whether_or_not_the_list_scrolls() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        let t = Theme::default();
        let (w, h, row_h) = (400u32, 100u32, 25u32);
        let columns = [ListColumn { width: 70, align: ColumnAlign::Right }];
        const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");
        let font = libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font");
        // **Where the ink lands, not where the box is**: the reservation moves a cell *inside*
        // the strip, so an outer rect is the same either way — a first version of this test
        // compared those and passed against the bug.
        let rightmost_ink = |e: &Element<u64>| {
            let all = libdraw::geom::Rect::new(0, 0, w, 25);
            let m = crate::paint::FontMetrics::new(&font, t.font_px);
            let l = crate::layout::layout(e, all, &m);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, 25, PixelFormat::XRGB8888));
            fb.clear(t.background);
            let mut custom = |_: u32, _: libdraw::geom::Rect, _: libdraw::geom::Rect, _: &mut MemFramebuffer| {};
            crate::paint::paint(&mut fb, &font, &t, e, &l, all, &mut custom);
            // **Text only**: a scrolling list draws its bar at the right edge, and the bar is
            // ink too — what has to line up is the cell's glyphs against the heading's.
            (0..w)
                .rev()
                .find(|x| {
                    (0..25).any(|y| {
                        let c = fb.get_pixel(*x, y);
                        c != Some(t.background)
                            && c != Some(t.face_hover)
                            && c != Some(t.border)
                            && c != Some(t.groove)
                            && c != Some(t.thumb)
                    })
                })
                .expect("something is drawn")
        };
        let right_edges = |count: usize| {
            let labels: alloc::vec::Vec<(u64, &str)> = (0..count as u64).map(|i| (i, "entry")).collect();
            let rows: alloc::vec::Vec<ListRow<'_>> = labels
                .iter()
                .map(|&(key, label)| ListRow { key, label, marked: false, cells: &["848"], swatch: None, button: None })
                .collect();
            let scrolls = list_scrolls(rows.len(), h, row_h);
            let mut st = ListState::default();
            let list: Element<u64> = list_view(&rows, &columns, &mut st, h, row_h, |k| k, None, None, None, None, &t);
            let header: Element<u64> = list_header("Name", &["Size"], &columns, false, scrolls, &t);
            (rightmost_ink(&list), rightmost_ink(&header), scrolls)
        };
        // **`848` under `Size`, both right-aligned, so their ink ends at the same x** — within a
        // pixel, since the two strings are different glyphs.
        let (short_row, short_head, short_scrolls) = right_edges(2);
        assert!(!short_scrolls, "two rows do not overflow");
        assert!(
            short_row.abs_diff(short_head) <= 1,
            "with no scrollbar: row ends at {short_row}, heading at {short_head}"
        );
        let (long_row, long_head, long_scrolls) = right_edges(40);
        assert!(long_scrolls, "forty rows overflow");
        assert!(long_row.abs_diff(long_head) <= 1, "scrolling: row ends at {long_row}, heading at {long_head}");
        assert!(long_row < short_row, "and a scrolling list gave up the scrollbar's width");
    }

    /// **A row's button is a target of its own** (Phase 6 Part F): a press on it is `activate`
    /// with its key, not the row's, and not a grab of the row; a press beside it is the row's. The
    /// row stays lit while the pointer is over its button, the glyph is drawn at the row's trailing
    /// end — and a row without one draws nothing there.
    #[test]
    fn a_rows_button_is_its_own_target_and_keeps_the_row_lit() {
        use crate::diff::Tree;
        use crate::route::Router;
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};
        const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");
        let font = libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font");
        let t = Theme::default();
        let (w, h) = (200u32, 25u32);
        #[derive(Clone, PartialEq, Debug)]
        enum M {
            Open(u64),
            Grab(u64),
        }
        let build = |button: Option<RowButton<'static>>, hovered: Option<u64>| {
            let row = ListRow { key: 7, label: "NXFAT", swatch: Some(Swatch::dot(t.ok)), button, ..Default::default() };
            let mut st = ListState::default();
            list_view(&[row], &[], &mut st, h, h, M::Open, Some(M::Grab), None, hovered, None, &t)
        };
        let eject = Some(RowButton { key: 8, glyph: "\u{23CF}" });
        let all = libdraw::geom::Rect::new(0, 0, w, h);
        let metrics = crate::paint::FontMetrics::new(&font, t.font_px);

        let e = build(eject, None);
        let l = crate::layout::layout(&e, all, &metrics);
        let mut tree = Tree::new();
        tree.update(&e, &l).expect("a clean frame: the row's halves are both keyed");
        let at = |x: i32, pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y: h as i32 / 2,
            ..Default::default()
        };
        let click = |x: i32| {
            let mut r = Router::new();
            let down = r.pointer(&tree, &e, &l, at(x, true)).0;
            let up = r.pointer(&tree, &e, &l, at(x, false)).0;
            (down, up, r.hovered_key(&tree))
        };
        let button_x = (w - ROW_BUTTON_W / 2) as i32;
        assert_eq!(click(button_x), (alloc::vec![], alloc::vec![M::Open(8)], Some(8)), "the button: no grab");
        assert_eq!(click(60), (alloc::vec![M::Grab(7)], alloc::vec![M::Open(7)], Some(7)), "the row");

        let paint = |e: &Element<M>| {
            let l = crate::layout::layout(e, all, &metrics);
            let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
            fb.clear(t.background);
            crate::paint::paint(&mut fb, &font, &t, e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
            fb
        };
        let ink_at_end = |fb: &MemFramebuffer| {
            (w - ROW_BUTTON_W..w)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter(|&(x, y)| fb.get_pixel(x, y) != fb.get_pixel(w - 1, 0))
                .count()
        };
        assert!(ink_at_end(&paint(&build(eject, None))) > 10, "the glyph is drawn at the row's end");
        assert_eq!(ink_at_end(&paint(&build(None, None))), 0, "and nothing is without a button");
        let ground_at = |fb: &MemFramebuffer| fb.get_pixel(30, 1);
        let unlit = ground_at(&paint(&build(eject, None)));
        assert_ne!(ground_at(&paint(&build(eject, Some(8)))), unlit, "over the button lights the row");
        assert_eq!(
            ground_at(&paint(&build(eject, Some(8)))),
            ground_at(&paint(&build(eject, Some(7)))),
            "as over the row itself"
        );
    }

    /// **A press on a row's button is still the button's after a repaint lights the row** (Phase 6
    /// Part F, `check-media`'s third run): the press establishes hover, the next frame draws the
    /// row washed and the button's face behind its glyph, and the release that follows lands in
    /// that frame. Every pairing of the two frames, each way round.
    #[test]
    fn a_rows_button_keeps_its_click_across_a_repaint_that_lights_it() {
        use crate::diff::Tree;
        use crate::route::Router;
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};
        let t = Theme::default();
        let (w, h) = (200u32, 25u32);
        let build = |hovered: Option<u64>, selected: Option<usize>| {
            let row = ListRow {
                key: 7,
                label: "NXFAT",
                swatch: Some(Swatch::dot(t.ok)),
                button: Some(RowButton { key: 8, glyph: "x" }),
                ..Default::default()
            };
            let mut st = ListState { selected, ..Default::default() };
            list_view::<u64>(&[row], &[], &mut st, h, h, |k| k, None, None, hovered, None, &t)
        };
        let at = |pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x: (w - ROW_BUTTON_W / 2) as i32,
            y: h as i32 / 2,
            ..Default::default()
        };
        let all = libdraw::geom::Rect::new(0, 0, w, h);
        const CELL: crate::layout::FixedCell = crate::layout::FixedCell { w: 8, h: 16 };
        let states = [(None, None), (Some(8), None), (Some(7), None), (None, Some(0)), (Some(8), Some(0))];
        for before in states {
            for after in states {
                let mut tree = Tree::new();
                let e1 = build(before.0, before.1);
                let l1 = crate::layout::layout(&e1, all, &CELL);
                tree.update(&e1, &l1).expect("a clean frame");
                let mut r = Router::new();
                let down = r.pointer(&tree, &e1, &l1, at(true)).0;
                let e2 = build(after.0, after.1);
                let l2 = crate::layout::layout(&e2, all, &CELL);
                tree.update(&e2, &l2).expect("a clean frame");
                let up = r.pointer(&tree, &e2, &l2, at(false)).0;
                assert_eq!((down, up), (alloc::vec![], alloc::vec![8]), "pressed in {before:?}, released in {after:?}");
            }
        }
    }

    /// **An inactive tab's `×` keeps its click across the repaint that lights the tab**, the class
    /// of [`RowButton`]'s bug: the hover face an inactive tab gains came before its label and `×`,
    /// and moved them along one.
    #[test]
    fn a_tabs_close_box_keeps_its_click_across_a_repaint_that_lights_it() {
        use crate::diff::Tree;
        use crate::route::Router;
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};
        #[derive(Clone, PartialEq, Debug)]
        enum M {
            Select(u64),
            Close(u64),
        }
        let t = Theme::default();
        let tabs = [Tab { key: 1, label: "one", marked: false }, Tab { key: 2, label: "two", marked: false }];
        let build = |hovered: Option<u64>| tab_strip(&tabs, 1, hovered, M::Select, M::Close, TabExtras::none(), &t);
        const CELL: crate::layout::FixedCell = crate::layout::FixedCell { w: 8, h: 16 };
        let all = libdraw::geom::Rect::new(0, 0, 400, TAB_STRIP_H);
        let x = (TAB_SIDE + TAB_W + TAB_GAP) as i32 + TAB_CLOSE_CX;
        let at = |pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y: TAB_STRIP_H as i32 / 2 + 2,
            ..Default::default()
        };
        let states = [None, Some(2), Some(tab_close_key(2))];
        for before in states {
            for after in states {
                let mut tree = Tree::new();
                let e1 = build(before);
                let l1 = crate::layout::layout(&e1, all, &CELL);
                tree.update(&e1, &l1).expect("a clean frame");
                let mut r = Router::new();
                let _ = r.pointer(&tree, &e1, &l1, at(true));
                let e2 = build(after);
                let l2 = crate::layout::layout(&e2, all, &CELL);
                tree.update(&e2, &l2).expect("a clean frame");
                let up = r.pointer(&tree, &e2, &l2, at(false)).0;
                assert_eq!(up, alloc::vec![M::Close(2)], "pressed with {before:?} lit, released with {after:?}");
            }
        }
    }

    /// **A title bar's buttons keep their click across a repaint that changes focus or hover** — the
    /// class of [`RowButton`]'s bug, swept: a click on an unfocused window is a raise between the
    /// press and the release, so the release always meets a repainted bar.
    #[test]
    fn a_title_bars_buttons_keep_their_click_across_a_repaint() {
        use crate::diff::Tree;
        use crate::route::Router;
        use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED, PointerEvent};
        #[derive(Clone, PartialEq, Debug)]
        enum M {
            Drag,
            Min,
            Max,
            Close,
        }
        let t = Theme::default();
        let build = |focused: bool, hovered: Option<u64>| {
            title_bar(
                "a window",
                Some("/home"),
                focused,
                M::Drag,
                TitleButtons { minimise: Some(M::Min), maximise: Some(M::Max), close: Some(M::Close) },
                hovered,
                &t,
            )
        };
        const CELL: crate::layout::FixedCell = crate::layout::FixedCell { w: 8, h: 16 };
        let all = libdraw::geom::Rect::new(0, 0, 400, TITLE_BAR_H);
        let step = (TITLE_BUTTON_W + TITLE_BUTTON_GAP) as i32;
        let close_x = (400 - TITLE_BUTTON_PAD - TITLE_BUTTON_W / 2 - 1) as i32;
        let at = |x: i32, pressed: bool| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: u16::from(pressed),
            flags: if pressed { POINTER_PRESSED } else { 0 },
            x,
            y: 16,
            ..Default::default()
        };
        for (x, want, key) in [
            (close_x, M::Close, TITLE_CLOSE_KEY),
            (close_x - step, M::Max, TITLE_MAXIMISE_KEY),
            (close_x - 2 * step, M::Min, TITLE_MINIMISE_KEY),
        ] {
            for before in [false, true] {
                for after in [false, true] {
                    let mut tree = Tree::new();
                    let e1 = build(before, None);
                    let l1 = crate::layout::layout(&e1, all, &CELL);
                    tree.update(&e1, &l1).expect("a clean frame");
                    let mut r = Router::new();
                    let _ = r.pointer(&tree, &e1, &l1, at(x, true));
                    // The repaint the press causes: the button lit, and focus as the raise left it.
                    let e2 = build(after, Some(key));
                    let l2 = crate::layout::layout(&e2, all, &CELL);
                    tree.update(&e2, &l2).expect("a clean frame");
                    let up = r.pointer(&tree, &e2, &l2, at(x, false)).0;
                    assert_eq!(up, alloc::vec![want.clone()], "focused {before} then {after}");
                }
            }
        }
    }

    /// A row's columns are where the list declares them, and its swatch is drawn.
    ///
    /// **Painted, and written before the screendump this time.** Three parts running have shipped
    /// a widget that was in the tree, routed correctly and drew nothing — so what this asserts is
    /// ink: a swatch's colour at the row's left end, a right-aligned cell's text inside its own
    /// column and not the one beside it, and the name giving way to the columns rather than the
    /// other way round (desktop refresh, Part I).
    #[test]
    fn a_rows_cells_land_in_their_columns_and_its_swatch_is_drawn() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
        const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");
        let font = libdraw::text::Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font");
        let t = Theme::default();
        let (w, h) = (400u32, 25u32);
        let columns =
            [ListColumn { width: 70, align: ColumnAlign::Right }, ListColumn { width: 60, align: ColumnAlign::Left }];
        let mark = t.accent;
        let row = ListRow {
            key: 1,
            label: "a-very-long-file-name-that-would-run-past-the-columns.txt",
            marked: false,
            cells: &["848", "toml"],
            swatch: Some(Swatch::block(mark)),
            button: None,
        };
        let mut st = ListState::default();
        let e: Element<u64> = list_view(&[row], &columns, &mut st, h, h, |k| k, None, None, None, None, &t);
        let all = libdraw::geom::Rect::new(0, 0, w, h);
        let l = crate::layout::layout(&e, all, &crate::paint::FontMetrics::new(&font, t.font_px));
        let mut fb = MemFramebuffer::new(Geometry::packed(w, h, PixelFormat::XRGB8888));
        fb.clear(t.background);
        crate::paint::paint(&mut fb, &font, &t, &e, &l, all, &mut |_, _, _, _: &mut MemFramebuffer| {});
        let count = |x0: u32, x1: u32, c: Rgb| {
            (x0..x1).flat_map(|x| (0..h).map(move |y| (x, y))).filter(|(x, y)| fb.get_pixel(*x, *y) == Some(c)).count()
        };
        // The swatch, at the row's left end and in its colour.
        assert!(
            count(0, ROW_PAD.left + SWATCH_W + 2, mark) >= (SWATCH_W * SWATCH_H / 2) as usize,
            "the swatch is drawn where the row starts"
        );
        assert_eq!(count(120, w, mark), 0, "and nowhere else");
        // Ink in each column's own band. The columns are the last 130 pixels: 70 then 60.
        let (size_from, kind_from) = (w - ROW_PAD.right - 130, w - ROW_PAD.right - 60);
        let ink_in = |x0: u32, x1: u32| {
            (x0..x1)
                .flat_map(|x| (0..h).map(move |y| (x, y)))
                .filter(|(x, y)| {
                    let c = fb.get_pixel(*x, *y);
                    c != Some(t.background) && c != Some(mark)
                })
                .count()
        };
        assert!(ink_in(size_from, kind_from) > 10, "the size cell is drawn in its column");
        assert!(ink_in(kind_from, w - ROW_PAD.right) > 10, "and the kind cell in its own");
        // **Right-aligned means against the right edge**, which nothing pinned: making
        // `ColumnAlign::Right` behave as `Left` passed this test (PR #320 review, 10). The size
        // column's ink sits in its second half, and the kind column's — left-aligned — in its
        // first.
        let half = |x0: u32, x1: u32| (x0 + x1) / 2;
        assert_eq!(
            ink_in(size_from, half(size_from, kind_from)),
            0,
            "a right-aligned cell puts nothing in the left half of its column"
        );
        assert!(ink_in(kind_from, half(kind_from, w - ROW_PAD.right)) > 0, "…and a left-aligned one does");
        // **The name gives way, not the columns.** A long name is clipped at the column's edge
        // rather than pushing it along — asserted by *which ink* is in the columns' band, since
        // the name is body ink and a cell is the dim step: the darkest pixel there must be no
        // darker than `foreground_dim`. (A first version asserted a blank gap before the
        // columns, which a name is entitled to fill.)
        let darkness = |c: Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let darkest = (size_from..w)
            .flat_map(|x| (0..h).map(move |y| (x, y)))
            .filter_map(|(x, y)| fb.get_pixel(x, y))
            .map(darkness)
            .min()
            .expect("pixels");
        assert!(
            darkest >= darkness(t.foreground_dim),
            "something in the columns is body ink at {darkest}: the name has run into them"
        );
    }

    /// The wheel moves the offset by whole rows, and stops at the top.
    ///
    /// **The bottom is not clamped here and that is deliberate** — `list_view` does it against
    /// the rows it is handed, which is the only place the count is known. This checks the half
    /// that *is* here: the direction, the multiplier, and that scrolling up past the first row
    /// stops rather than wrapping to an enormous offset (`offset` is unsigned, so the failure
    /// would be a list showing nothing at all).
    #[test]
    fn the_wheel_moves_whole_rows_and_stops_at_the_top() {
        let mut st = ListState::at(None, 0);
        st.wheel(2);
        assert_eq!(st.offset, 2 * crate::click::WHEEL_UNITS as usize, "positive is down");
        st.wheel(-1);
        assert_eq!(st.offset, crate::click::WHEEL_UNITS as usize);
        st.wheel(-100);
        assert_eq!(st.offset, 0, "a scroll off the top stops there");
    }

    /// A saturated delta scrolls a long way down rather than wrapping upward.
    ///
    /// **20 000 rather than `i16::MAX`**: `32767 * 3` wraps back round to a *positive* `i16`, so
    /// the largest value passes against the broken arithmetic this exists to catch. The value
    /// has to be one whose overflow is visible — the same trap `nxterm`'s copy of this test hit.
    #[test]
    fn a_huge_wheel_delta_does_not_wrap_upward() {
        let mut st = ListState::at(None, 100);
        st.wheel(20_000);
        assert!(st.offset > 100, "scrolled down, not back to the top: {}", st.offset);
    }

    /// A row rests on the list's own ground, and is lit from it.
    ///
    /// **A row used to fill `theme.track` whatever the list's ground was** (M15 Part F), so a
    /// panel with a ground of its own had list-coloured tiles painted over it — the panel showed
    /// only in the gap below the last row, which is not what a panel is. The hover is a wash of
    /// the accent over that ground since the desktop refresh's Part B — "this surface, lit",
    /// which a hover derived by shading the ground only approximated.
    #[test]
    fn a_rows_ground_is_the_lists_and_its_hover_is_a_wash_over_it() {
        use crate::element::Node;
        let p = Theme::default();
        let data = [(1u64, "alpha"), (2, "beta")];
        let panel = Rgb::new(0xDD, 0xDA, 0xD6);
        // **A selection is kept, and the hover is on a *different* row.** With nothing selected
        // a hovered row is the primary highlight, so a fixture without a selection never reaches
        // the hover branch at all (M11 Part E, batch 5).
        //
        // Each row's fill and wash, in order.
        let rows_of = |ground: Option<Rgb>, hovered: Option<u64>| {
            let mut st = ListState::at(Some(0), 0);
            let e: Element<u64> =
                list_view(&rows(&data), &[], &mut st, 100, 20, |k| k, None, None, hovered, ground, &p);
            #[allow(clippy::type_complexity, reason = "a local accumulator in one test")]
            let mut out: Vec<(Option<Rgb>, Option<(Rgb, u8)>)> = Vec::new();
            walk(&e, &mut |n| {
                if n.key.is_none() {
                    return;
                }
                let (mut f, mut w) = (None, None);
                walk(n, &mut |c| match &c.node {
                    Node::Fill(rgb) if f.is_none() => f = Some(*rgb),
                    Node::Wash { colour, coverage } => w = Some((*colour, *coverage)),
                    _ => {}
                });
                out.push((f, w));
            });
            out
        };
        let hover = p.scheme.hover_coverage();
        let sel = libdraw::theme::SELECTION_COVERAGE;

        // The default list: rows on `track`; the selected row washed at 20%, the hovered one at
        // the hover's coverage, both over their own ground.
        assert_eq!(
            rows_of(None, Some(2)),
            [(Some(p.track), Some((p.accent, sel))), (Some(p.track), Some((p.accent, hover)))]
        );
        // A list on a panel rests on the panel, and the same washes land on it.
        assert_eq!(
            rows_of(Some(panel), Some(2)),
            [(Some(panel), Some((p.accent, sel))), (Some(panel), Some((p.accent, hover)))]
        );
        // And a row nobody is pointing at is its ground and nothing more.
        assert_eq!(rows_of(Some(panel), None)[1], (Some(panel), None));
    }

    /// A **marked** row is drawn as a selected one, and an unmarked one is not.
    ///
    /// **Asserted on the tree, because nothing else can be.** `check-display` renders the guest's
    /// expected picture with this same code, so a broken highlight is identical on both sides by
    /// construction; `check-login` presses rows and reads no pixels. Breaking `|| r.marked` left
    /// the entire host suite green — 309 tests — until this existed (PR #285 review, worth fixing
    /// 5).
    #[test]
    fn a_marked_row_is_drawn_like_a_selected_one() {
        fn nodes<M>(e: &Element<M>) -> usize {
            1 + e.children().map(nodes).sum::<usize>()
        }
        let label = [(1u64, "alpha"), (2, "beta")];
        let build = |marked: bool, selected: Option<usize>| {
            let mut r = rows(&label);
            r[1].marked = marked;
            let mut st = ListState { selected, offset: 0, ..Default::default() };
            nodes(&list_view(&r, &[], &mut st, 100, 20, |k| k, None, None, None, None, &Theme::default()))
        };
        let plain = build(false, None);
        let marked = build(true, None);
        let selected = build(false, Some(1));
        assert!(marked > plain, "a marked row must cost more layers than a resting one");
        assert_eq!(marked, selected, "and be drawn as the selected row is");
    }

    /// The whole point of the widget: a hundred rows cost as many elements as fit.
    #[test]
    fn only_the_visible_rows_become_elements() {
        let data: alloc::vec::Vec<(u64, &str)> = (0..100u64).map(|i| (i, "row")).collect();
        let r = rows(&data);
        let e: Element<u64> =
            list_view(&r, &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(keys(&e).len(), 5, "the list built rows it cannot show");
    }

    /// Without keys the diff pairs by position, and its own doc names the failure: insert at
    /// the top and "row 2's widget" pairs with "row 3's element".
    #[test]
    fn every_row_carries_its_key_not_its_index() {
        let data = [(70u64, "a"), (80, "b"), (90, "c")];
        let e: Element<u64> = list_view(
            &rows(&data),
            &[],
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            None,
            None,
            None,
            &Theme::default(),
        );
        assert_eq!(keys(&e), alloc::vec![70, 80, 90], "rows are keyed by position");
    }

    /// The window-list caller: rows reorder in place on every raise.
    #[test]
    fn a_reordered_window_list_keeps_each_rows_identity() {
        let before = [(1u64, "term"), (2, "editor")];
        let after = [(2u64, "editor"), (1, "term")];
        let a: Element<u64> = list_view(
            &rows(&before),
            &[],
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            None,
            None,
            None,
            &Theme::default(),
        );
        let b: Element<u64> = list_view(
            &rows(&after),
            &[],
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            None,
            None,
            None,
            &Theme::default(),
        );
        assert_eq!(keys(&a), alloc::vec![1, 2]);
        assert_eq!(keys(&b), alloc::vec![2, 1], "the reorder did not move the keys");
    }

    /// The launcher caller: results are replaced wholesale on every keystroke, so the list
    /// gets shorter under a scroll offset that was valid a frame ago.
    #[test]
    fn a_list_that_shrinks_under_a_stale_offset_still_renders() {
        let long: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "hit")).collect();
        let mut state = ListState::at(Some(19), 0);
        let _: Element<u64> =
            list_view(&rows(&long), &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.offset, 15, "the scroll did not follow the selection");
        let short = [(0u64, "hit"), (1, "hit"), (2, "hit")];
        let e: Element<u64> =
            list_view(&rows(&short), &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.offset, 0, "a stale offset survived the list shrinking");
        assert_eq!(keys(&e).len(), 3, "the list rendered blank");

        // **The selection half, which the first version of this test did not assert and so
        // passed against a widget that left it dangling** (PR #233 review, finding 2). A
        // selection of 19 into three rows highlights nothing, and `down` is *dead*: it takes
        // `Some(i) if i + 1 < len` and otherwise returns `i` unchanged, so a stale index never
        // comes back into range on its own.
        assert_eq!(state.selected, Some(2), "the selection still indexes the longer list");
        assert!(
            row_washes(&e).iter().any(|w| *w == Some((Theme::default().accent, libdraw::theme::SELECTION_COVERAGE))),
            "no row is painted as selected"
        );
        assert!(!state.down(3), "the selection is already on the last row");
        assert!(state.up(), "the arrow keys are dead after the shrink");
        assert_eq!(state.selected, Some(1));
    }

    /// Shrinking to nothing leaves nothing selected, rather than row `-1`.
    #[test]
    fn a_list_that_empties_clears_the_selection() {
        let mut state = ListState::at(Some(3), 2);
        let _: Element<u64> =
            list_view(&[], &[], &mut state, 100, 20, |k| k, None, None, None, None, &Theme::default());
        assert_eq!(state.selected, None, "an empty list kept a selection");
        assert_eq!(state.offset, 0);
    }

    /// A selection moved with the keyboard must stay on screen.
    /// Following the selection does not undo a deliberate scroll.
    ///
    /// **The bug the maintainer reported as "the scrollbar doesn't work"** (M15 Part D). Both
    /// `list_view` and `text_area` call `ensure_visible` on every build, and an application
    /// repaints after every event — so with the selection on row 0, a scrollbar drag computed
    /// the right offset and the very next frame put it back. The bar moved and the list did not.
    /// `nxterm` was unaffected because its grid follows nothing: it keeps a `view_top` instead.
    #[test]
    fn following_a_selection_happens_once_per_selection_not_once_per_build() {
        let mut s = ListState::at(Some(0), 0);
        s.ensure_visible(5);
        assert_eq!(s.offset, 0);

        // A scrollbar drag, then the repaint that follows every event.
        s.offset = 20;
        s.ensure_visible(5);
        assert_eq!(s.offset, 20, "the repaint dragged the list back to its selection");
        s.ensure_visible(5);
        assert_eq!(s.offset, 20, "…and again on the frame after that");

        // **But a selection that *moves* is still followed**, which is what the rule is for.
        s.down(40);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "the selection moved and the view did not follow it");
    }

    /// A window that gets shorter brings the selection back into view.
    ///
    /// **The case a key of "the selection we last followed" alone gets wrong**, and the one
    /// flagged when Part D landed: a shrink can put the selection off screen without the
    /// selection moving, so a rule that waits for it to move leaves a list highlighting a row
    /// nobody can see (PR #290 review, 1).
    #[test]
    fn a_shorter_window_brings_the_selection_back() {
        let mut s = ListState::at(Some(9), 0);
        s.ensure_visible(10);
        assert_eq!(s.offset, 0, "precondition: ten rows fit, so nothing moved");

        // Four fit now, and the selection is past the bottom.
        s.ensure_visible(4);
        assert_eq!(s.offset, 6, "the selection was left below a window that shrank");
    }

    /// The same rule for a document's caret.
    #[test]
    fn a_document_scrolls_away_from_its_caret_and_stays_there() {
        let mut a = TextAreaState::with_text("0\n1\n2\n3\n4\n5\n6\n7\n8\n9");
        a.ensure_visible(3);
        assert_eq!(a.offset(), 0);

        a.scroll_to(6, 3);
        a.ensure_visible(3);
        assert_eq!(a.offset(), 6, "the repaint dragged the document back to its caret");

        // Typing moves the caret, and the view follows it again.
        a.apply(KEY_DOWN, 0, 0);
        a.ensure_visible(3);
        assert_eq!(a.offset(), 1, "the caret moved to line 1 and the view did not follow it");
    }

    #[test]
    fn the_scroll_follows_the_selection_in_both_directions() {
        let mut s = ListState::at(Some(7), 0);
        s.ensure_visible(5);
        assert_eq!(s.offset, 3, "scrolling down did not bring the selection into view");
        s.selected = Some(1);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "scrolling up did not bring the selection into view");
        s.selected = Some(3);
        s.ensure_visible(5);
        assert_eq!(s.offset, 1, "a visible selection scrolled anyway");
    }

    /// Down from nothing selects the first row.
    #[test]
    fn down_from_nothing_selects_the_first_row() {
        let mut s = ListState::default();
        assert!(s.down(3));
        assert_eq!(s.selected, Some(0));
        assert!(s.down(3));
        assert_eq!(s.selected, Some(1));
    }

    /// Both ends stop rather than wrapping, and report no change.
    #[test]
    fn the_selection_stops_at_both_ends() {
        let mut s = ListState::at(Some(2), 0);
        assert!(!s.down(3), "the selection moved past the last row");
        s.selected = Some(0);
        assert!(!s.up(), "the selection moved above the first row");
        let mut empty = ListState::default();
        assert!(!empty.up());
        assert_eq!(empty.selected, None);
    }

    /// Enter and Escape belong to whatever owns the list.
    #[test]
    fn declines_the_keys_that_belong_above_it() {
        for key in [28u16, 1, 15] {
            let mut s = ListState::at(Some(1), 0);
            assert!(!s.apply(key, 4), "keycode {key} was claimed by the list");
            assert_eq!(s.selected, Some(1));
        }
        let mut s = ListState::at(Some(1), 0);
        assert!(s.apply(108, 4), "Down was not claimed");
        assert_eq!(s.selected, Some(2));
    }

    #[test]
    fn a_menu_item_highlights_under_the_pointer_and_is_flat_otherwise() {
        let p = Theme::default();
        let hot: Element<u8> = menu_item("Clear", 1, true, &p);
        let cold: Element<u8> = menu_item("Clear", 1, false, &p);

        // Every painting node in the item, whatever its kind.
        let paints = |e: &Element<u8>| {
            let mut out = alloc::vec::Vec::new();
            walk(e, &mut |n| match &n.node {
                Node::Fill(c) | Node::Bevel(c) => out.push((*c, 255)),
                Node::Wash { colour, coverage } => out.push((*colour, *coverage)),
                _ => {}
            });
            out
        };

        // **One layer: the accent washed over the bar at the hover's coverage** (desktop refresh,
        // Part B) — the design's `--soft`. It was a border in the accent and the selection
        // bevelled inside it.
        assert_eq!(
            paints(&hot),
            alloc::vec![(p.accent, p.scheme.hover_coverage())],
            "the hovered item is not the hover wash"
        );

        // **And nothing at all otherwise**, which is the half that fails if a highlight sticks:
        // an item that paints a face when it is not hovered is a menu with every row lit.
        assert!(paints(&cold).is_empty(), "an idle item drew a face");
    }

    #[test]
    fn the_scrollbar_takes_the_pointer_only_when_the_caller_offered_somewhere_to_send_it() {
        // **The missing half was the handler, not the arithmetic.** `ScrollState::offset_at` has
        // been right since M5 and `nxterm` has always dragged with it; a *list's* bar was built
        // without an `on_pointer` at all, so the events never left the router. This asserts the
        // wiring, and the `None` case is the control — without it the test would pass for a
        // widget that attached a handler unconditionally, which is a different bug.
        let p = Theme::default();
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let handlers = |e: &Element<u64>| {
            let mut n = 0;
            walk(e, &mut |c| {
                if c.on_pointer.is_some() {
                    n += 1;
                }
            });
            n
        };
        let with: Element<u64> =
            list_view(&rows(&many), &[], &mut ListState::default(), 100, 20, |k| k, None, Some(|_| 0), None, None, &p);
        assert_eq!(handlers(&with), 1, "the scrollbar took no pointer handler");
        let without: Element<u64> =
            list_view(&rows(&many), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert_eq!(handlers(&without), 0, "a handler appeared with nowhere to send it");
    }

    /// A press at `y` on a bar, and the offset it asks for.
    fn press_bar(g: &mut ScrollGrab, st: ScrollState, track: u32, y: i32) -> Option<u32> {
        g.apply(
            st,
            track,
            PointerEvent {
                kind: POINTER_BUTTON,
                button: 0x110,
                buttons: 1,
                flags: POINTER_PRESSED,
                y,
                ..Default::default()
            },
        )
    }

    /// A motion to `y` with the button still held.
    fn drag_bar(g: &mut ScrollGrab, st: ScrollState, track: u32, y: i32) -> Option<u32> {
        g.apply(
            st,
            track,
            PointerEvent { kind: librsproto::surface::POINTER_MOTION, buttons: 1, y, ..Default::default() },
        )
    }

    #[test]
    fn a_drag_on_the_track_moves_the_offset_and_a_release_does_not() {
        // **The scrollbar was decoration.** `list_view` built one and gave it no pointer handler,
        // so a list showed its position and could not be dragged — a control that looks live and
        // is not, which is the defect this crate's own notes keep naming. `nxterm` builds its
        // scrollbar directly and has always wired this (M11 Part E batch 6).
        let mut st = ListState::default();
        let mut g = ScrollGrab::new();
        // Twenty rows of 20px in a 100px viewport: five visible, fifteen of travel.
        let bar = |st: &ListState| st.bar(100, 20, 20);
        st.offset = press_bar(&mut g, bar(&st), 100, 100).expect("a press on the track scrolls") as usize;
        assert!(st.offset > 0, "a drag to the bottom of the track moved nothing");
        let bottom = st.offset;
        st.offset = drag_bar(&mut g, bar(&st), 100, 0).expect("still dragging") as usize;
        assert_eq!(st.offset, 0, "a drag to the top did not come back");
        assert!(bottom <= 15, "the offset ran past the last full screen of rows");

        // The release moves nothing and ends the drag.
        let released = g.apply(
            bar(&st),
            100,
            PointerEvent { kind: POINTER_BUTTON, button: 0x110, buttons: 0, flags: 0, y: 100, ..Default::default() },
        );
        assert_eq!(released, None, "a release is not a position");
        assert!(!g.dragging());
        assert_eq!(drag_bar(&mut g, bar(&st), 100, 90), None, "and the bar stopped following");

        // A list that fits has nowhere to go, and must not be moved by a drag on a track that is
        // not drawn — the case the position arithmetic returns zero for.
        let mut st = ListState::default();
        let mut g = ScrollGrab::new();
        st.offset = press_bar(&mut g, st.bar(100, 20, 3), 100, 100).unwrap_or(0) as usize;
        assert_eq!(st.offset, 0, "a list shorter than its viewport scrolled");
    }

    /// Grabbing the thumb by its end does not move it — the `scroll-grab` deferral, closed.
    ///
    /// **The defect this exists for is a *jump* on the press**, before the drag has begun: with
    /// only "put the thumb's centre under the cursor" available, a press on the thumb's top edge
    /// moved the content by half a thumb, so aiming at the thumb threw away the position you
    /// were looking at. Every toolkit that avoids it remembers where within the thumb the press
    /// landed.
    #[test]
    fn a_press_on_the_thumb_does_not_move_it_and_the_drag_then_tracks_the_pointer() {
        // A long scrollback: 1000 units, 20 visible, in a 200px track. The thumb is clamped to
        // `MIN_THUMB`, so it is *much* shorter than the travel — which is when a re-centring
        // grab is least noticeable and a wrong one still throws the view a long way.
        let track = 200;
        let st = ScrollState { offset: 400, visible: 20, total: 1000 };
        let (pos, len) = st.thumb(track);
        assert!(len >= MIN_THUMB, "premise: the thumb is at its floor");

        for (what, y) in [("its top edge", pos as i32), ("its bottom edge", pos as i32 + len as i32 - 1)] {
            let mut g = ScrollGrab::new();
            assert_eq!(
                press_bar(&mut g, st, track, y),
                Some(400),
                "{what}: taking hold of the thumb moved the content"
            );
            assert!(g.dragging());

            // And from there it follows the pointer: ten pixels down the track is ten pixels of
            // thumb travel, whichever end it was taken by.
            let moved = drag_bar(&mut g, st, track, y + 10).expect("dragging");
            let expected = st.offset_for_thumb_top(track, pos as i32 + 10);
            assert_eq!(moved, expected, "{what}: the thumb did not follow by the distance moved");
        }
    }

    /// A second button tapped mid-drag neither ends the drag nor re-takes it.
    ///
    /// **The router delivers every button's events to the captured widget**, and clears its own
    /// capture only when *all* of them come up (`released && event.buttons == 0`) — so pressing
    /// the right button without letting go of the left arrives here as a press and a release of
    /// a button this grab knows nothing about. Answering either of them would strand the bar:
    /// the router still believes the gesture is live, so the person goes on dragging and
    /// nothing follows. The code this replaced (`if p.buttons != 0` in each application) kept
    /// following, so this is a regression the review caught (PR #288, finding 1).
    #[test]
    fn a_second_buttons_press_and_release_do_not_disturb_the_drag() {
        const RIGHT: u16 = 0x111;
        let track = 200;
        let st = ScrollState { offset: 400, visible: 20, total: 1000 };
        let (pos, _) = st.thumb(track);
        let mut g = ScrollGrab::new();
        press_bar(&mut g, st, track, pos as i32);
        assert!(g.dragging(), "precondition: the left button took the thumb");

        // The right button goes down while the left is still held: two buttons in the mask.
        let other = |flags: u16, buttons: u16| PointerEvent {
            kind: POINTER_BUTTON,
            button: RIGHT,
            buttons,
            flags,
            // Far from the thumb, so re-taking the grab here would visibly jump the bar.
            y: track as i32 - 1,
            ..Default::default()
        };
        assert_eq!(g.apply(st, track, other(POINTER_PRESSED, 0b011)), None, "the press answered");
        assert_eq!(g.apply(st, track, other(0, 0b001)), None, "the release answered");
        assert!(g.dragging(), "a button this grab never took ended it");

        // And the drag still tracks the pointer, from the offset it was taken at.
        let moved = drag_bar(&mut g, st, track, pos as i32 + 10).expect("still dragging");
        assert_eq!(moved, st.offset_for_thumb_top(track, pos as i32 + 10));
    }

    /// A press on the track still jumps, and holds the thumb centred afterwards.
    ///
    /// **The other half of the rule**, and the reason the grab is not simply "always keep the
    /// press offset": a click on empty track means "go there", and there is no thumb under the
    /// cursor to preserve a relationship with.
    #[test]
    fn a_press_on_the_track_jumps_and_then_holds_the_thumb_centred() {
        let track = 200;
        let st = ScrollState { offset: 0, visible: 20, total: 1000 };
        let (_, len) = st.thumb(track);
        let mut g = ScrollGrab::new();

        let jumped = press_bar(&mut g, st, track, 150).expect("a track press scrolls");
        assert_eq!(jumped, st.offset_at(track, 150), "the thumb's centre came to the cursor");
        assert!(jumped > 0);

        let after = drag_bar(&mut g, st, track, 160).expect("dragging");
        assert_eq!(
            after,
            st.offset_for_thumb_top(track, 160 - len as i32 / 2),
            "and it stayed centred as the drag continued"
        );
    }

    #[test]
    fn with_nothing_selected_the_hovered_row_is_the_highlight() {
        // **The applications modal is this list**, and it keeps no selection at all — Enter takes
        // the first filtered entry — so before batch 5 every hover landed on the quiet branch and
        // a menu that is nothing *but* hover highlighted in grey. One primary highlight is the
        // rule; where there is no selection to compete with, the pointer's is not competing.
        let p = Theme::default();
        let data = [(1u64, "a"), (2, "b")];
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::default(), 100, 20, |k| k, None, None, Some(2), None, &p);
        // The primary highlight is the selection's wash, not the quieter hover's.
        let sel = Some((p.accent, libdraw::theme::SELECTION_COVERAGE));
        assert_eq!(row_washes(&e)[1], sel, "the hovered row is not the primary highlight");
        assert_eq!(row_washes(&e)[0], None, "an untouched row reacted");
        assert_eq!(row_faces(&e)[0], p.track, "and it is the list's ground");
    }

    #[test]
    fn a_hovered_row_is_quieter_than_a_selected_one_and_loses_to_it() {
        let p = Theme::default();
        let data = [(1u64, "a"), (2, "b")];
        // Row 1 selected, row 0 hovered: two different highlights, and they must not be the
        // same weight — two answers to "what happens if I act now" is one too many.
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::at(Some(1), 0), 100, 20, |k| k, None, None, Some(1), None, &p);
        // Quieter: the hover's coverage against the selection's — 10% against 20% in the light
        // scheme, which is the whole difference in weight.
        let washes = row_washes(&e);
        assert_eq!(washes[0], Some((p.accent, p.scheme.hover_coverage())), "the hovered row did not react");
        assert_eq!(washes[1], Some((p.accent, libdraw::theme::SELECTION_COVERAGE)), "the selected row lost it");
        assert!(p.scheme.hover_coverage() < libdraw::theme::SELECTION_COVERAGE, "the hover is not quieter");

        // And hovering the *selected* row leaves it selected rather than downgrading it.
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::at(Some(1), 0), 100, 20, |k| k, None, None, Some(2), None, &p);
        assert_eq!(row_washes(&e)[1], Some((p.accent, libdraw::theme::SELECTION_COVERAGE)), "selection lost to hover");
    }

    /// The selected row paints differently, or selection is invisible.
    #[test]
    fn the_selected_row_is_painted_differently() {
        let data = [(1u64, "a"), (2, "b")];
        let p = Theme::default();
        let e: Element<u64> =
            list_view(&rows(&data), &[], &mut ListState::at(Some(1), 0), 100, 20, |k| k, None, None, None, None, &p);
        // **The ground and a wash over it** (desktop refresh, Part B): both rows rest on the
        // list's ground, and the selected one carries the accent at the selection's 20%. It was a
        // one-pixel accent border and the selection bevelled inside it (M11 Part E, batch 2); the
        // design is flat, so a gradient on either row is now the regression.
        let faces = row_faces(&e);
        assert_eq!(faces, [p.track, p.track], "a row is not on the list's own ground");
        let washes = row_washes(&e);
        assert_eq!(
            washes[1],
            Some((p.accent, libdraw::theme::SELECTION_COVERAGE)),
            "the selected row looks like the others"
        );
        assert_eq!(washes[0], None, "an unselected row is washed too");
        assert_eq!(row_bevels(&e), [None, None], "a row is drawn with a gradient");
    }

    /// A scrollbar that is always there wastes width; one that never appears strands rows.
    #[test]
    fn the_scrollbar_appears_only_when_there_is_more_than_fits() {
        let p = Theme::default();
        let few = [(1u64, "a"), (2, "b")];
        let e: Element<u64> =
            list_view(&rows(&few), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert!(!has_row_node(&e), "a list that fits drew a scrollbar");
        let many: alloc::vec::Vec<(u64, &str)> = (0..20u64).map(|i| (i, "x")).collect();
        let e: Element<u64> =
            list_view(&rows(&many), &[], &mut ListState::default(), 100, 20, |k| k, None, None, None, None, &p);
        assert!(has_row_node(&e), "a list that overflows drew no scrollbar");
    }

    /// Each row's activation message carries that row's key.
    #[test]
    fn a_rows_message_carries_its_own_key() {
        let data = [(11u64, "a"), (22, "b")];
        let e: Element<u64> = list_view(
            &rows(&data),
            &[],
            &mut ListState::default(),
            100,
            20,
            |k| k,
            None,
            None,
            None,
            None,
            &Theme::default(),
        );
        assert_eq!(presses(&e), alloc::vec![11, 22], "a row sent another row's message");
    }

    /// Zero row height must not divide by zero.
    #[test]
    fn a_degenerate_row_height_is_not_a_division() {
        let data = [(1u64, "a")];
        let e: Element<u64> = list_view(
            &rows(&data),
            &[],
            &mut ListState::default(),
            100,
            0,
            |k| k,
            None,
            None,
            None,
            None,
            &Theme::default(),
        );
        assert_eq!(keys(&e).len(), 0);
    }

    fn walk<Msg>(e: &Element<Msg>, f: &mut impl FnMut(&Element<Msg>)) {
        f(e);
        for c in e.children() {
            walk(c, f);
        }
    }

    fn keys<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<u64> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if let Some(k) = n.key {
                out.push(k);
            }
        });
        out
    }

    fn presses(e: &Element<u64>) -> alloc::vec::Vec<u64> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if let Some(m) = n.on_press {
                out.push(m);
            }
        });
        out
    }

    /// The wash each row carries, if any — `(colour, coverage)` — which is how a selected or a
    /// hovered row is drawn since the desktop refresh's Part B.
    fn row_washes<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Option<(Rgb, u8)>> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut found = None;
            walk(n, &mut |c| {
                if found.is_none()
                    && let Node::Wash { colour, coverage } = &c.node
                {
                    found = Some((*colour, *coverage));
                }
            });
            out.push(found);
        });
        out
    }

    /// The bevelled fill each row carries, if any — the visible face of a selected row from M11
    /// Part E until the desktop refresh's Part B, and the negative a flat design has to keep.
    fn row_bevels<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Option<Rgb>> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut found = None;
            walk(n, &mut |c| {
                if found.is_none()
                    && let Node::Bevel(rgb) = &c.node
                {
                    found = Some(*rgb);
                }
            });
            out.push(found);
        });
        out
    }

    fn row_faces<Msg>(e: &Element<Msg>) -> alloc::vec::Vec<Rgb> {
        let mut out = alloc::vec::Vec::new();
        walk(e, &mut |n| {
            if n.key.is_none() {
                return;
            }
            let mut first = None;
            walk(n, &mut |c| {
                if first.is_none()
                    && let Node::Fill(rgb) = &c.node
                {
                    first = Some(*rgb);
                }
            });
            if let Some(rgb) = first {
                out.push(rgb);
            }
        });
        out
    }

    fn has_row_node<Msg>(e: &Element<Msg>) -> bool {
        let mut found = false;
        walk(e, &mut |n| {
            if matches!(n.node, Node::Row { .. }) {
                found = true;
            }
        });
        found
    }
