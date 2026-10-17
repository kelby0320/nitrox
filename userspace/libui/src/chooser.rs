//! The file chooser: a dialog for picking a file to open, or naming one to save.
//!
//! **A widget over a listing, not a browser**: `libui` cannot make a syscall, so entries are
//! *given*, listed by the application with [`libfs::list_dir`] and ordered with `libfs::sort`.
//!
//! **One widget for both jobs**: Open and Save As differ in whether there is a name field and what

use alloc::vec::Vec;

use libdraw::geom::Size;

use crate::element::{Element, Insets, center_v, column, padding, row, sized, text};
use crate::widget::{
    DIALOG_BUTTON_H, DIALOG_PAD, ListRow, ListState, TextFieldState, Theme, WINDOW_BORDER, WINDOW_FRAME, WidgetState,
    button, dialog_frame_sized, list_view, text_field,
};

/// Which job the chooser is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Pick an existing file. No name field; the answer is the selected row.
    Open,
    /// Name a file to write. The answer is the field's text, joined to the current directory.
    Save,
}

impl Mode {
    /// What the accepting button says.
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Open => "Open",
            Mode::Save => "Save",
        }
    }
}

/// What the person has done to the chooser; **the listing is deliberately absent**, as a copy could
/// go stale unnoticed.
#[derive(Clone, Debug)]
pub struct ChooserState {
    /// Which row is selected, and where the list is scrolled to.
    pub list: ListState,
    /// The name being typed, when saving. Untouched in [`Mode::Open`].
    pub name: TextFieldState,
}

impl ChooserState {
    /// A chooser with nothing selected and an empty name.
    pub fn new() -> Self {
        Self { list: ListState::at(None, 0), name: TextFieldState::new() }
    }

    /// A chooser opened to save `name`, so Save As starts from what the file is called.
    pub fn saving(name: &str) -> Self {
        Self { list: ListState::at(None, 0), name: TextFieldState::with_text(name) }
    }
}

impl Default for ChooserState {
    fn default() -> Self {
        Self::new()
    }
}

/// The element keys this widget uses, from `key_base` upwards: `key_base .. key_base + KEYS`.
///
/// A caller picks a base colliding with nothing of its own, including **the keys it gives the
/// rows**: `hovered` is matched against both.
pub const KEYS: u64 = 9;

/// How wide the *up* button is, in pixels: **a square**, as its label is an arrow.
const UP_W: u32 = 28;

/// How tall the row holding the up button and the path is: the name field's height.
const UP_H: u32 = FIELD_H;

/// The chooser's tree: a path strip, the rows, a name field when saving, and two answers.
///
/// `entries` is what the application listed and ordered; `on_row` is sent when a row is activated.
#[allow(clippy::too_many_arguments, reason = "the chooser's state and the message each control sends")]
pub fn view<Msg: Clone>(
    mode: Mode,
    path: &str,
    entries: &[ListRow<'_>],
    state: &mut ChooserState,
    key_base: u64,
    hovered: Option<u64>,
    on_row: fn(u64) -> Msg,
    up: Msg,
    accept: Msg,
    cancel: Msg,
    focused: bool,
    theme: &Theme,
) -> Element<Msg> {
    let title = padding(
        Insets::all(DIALOG_PAD),
        text(match mode {
            Mode::Open => "Open File",
            Mode::Save => "Save As",
        }),
    );
    // **Where you are, and the way out of it**: `Up` is all the navigation here; every other move
    // is a row. **Keyed like every other child** of the body column (`diff` refuses partly keyed
    // parents).
    let here = padding(
        Insets { top: 0, right: DIALOG_PAD, bottom: 4, left: DIALOG_PAD },
        // **Sized, because the frame's height is derived from what it contains**: left to measure
        // itself the row takes the button's height.
        sized(
            Size::new(0, UP_H),
            row(alloc::vec![
                sized(
                    Size::new(UP_W, 0),
                    button(
                        "\u{2191}",
                        up,
                        WidgetState { hovered: hovered == Some(key_base + 7), ..Default::default() },
                        theme,
                    )
                )
                .key(key_base + 7),
                // Centred *down* so the path sits on the button's middle; not across, or it floats
                // mid-dialog.
                padding(Insets { top: 0, right: 0, bottom: 0, left: 6 }, center_v(text(path)))
                    .flex(1)
                    .key(key_base + 8),
            ]),
        ),
    )
    .key(key_base + 4);
    // **Sized to the height it was built for**: `list_view` computes `visible` from the height it
    // is *told*, so a shorter list has rows it will not scroll to and no scrollbar to say so.
    let rows = sized(
        Size::new(0, ROWS_H),
        list_view(entries, &[], &mut state.list, ROWS_H, ROW_H, on_row, None, None, hovered, None, theme),
    )
    .key(key_base);
    // **Active, because in `Save` this field is the only thing taking characters**: drawn at rest
    // it is a live control that looks disabled.
    let name_field = text_field(&state.name, false, WidgetState { active: true, ..Default::default() }, theme);
    let mut body: Vec<Element<Msg>> = alloc::vec![here, rows];
    if mode == Mode::Save {
        // **The field is below the list, not above it**: a field above the rows reads as a filter.
        body.push(
            padding(
                Insets { top: FIELD_GAP, right: DIALOG_PAD, bottom: 0, left: DIALOG_PAD },
                sized(Size::new(0, FIELD_H), name_field),
            )
            .key(key_base + 1),
        );
    }
    let answers = row(alloc::vec![
        text("").flex(1).key(key_base + 5),
        button("Cancel", cancel, WidgetState { hovered: hovered == Some(key_base + 2), ..Default::default() }, theme,)
            .key(key_base + 2),
        // **The key goes on the child of the row, not on the button inside it**: a keyed button
        // wrapped in an unkeyed `padding` leaves the row mixed, and `diff` refuses it.
        padding(
            Insets { top: 0, right: 0, bottom: 0, left: 8 },
            button(
                mode.verb(),
                accept,
                WidgetState { hovered: hovered == Some(key_base + 3), ..Default::default() },
                theme,
            ),
        )
        .key(key_base + 3),
    ]);
    dialog_frame_sized(Size::new(CHOOSER_W, CHOOSER_H), title, column(body).key(key_base + 6), answers, focused, theme)
}

/// How wide the chooser's window is: **its own size, not a question's**.
pub const CHOOSER_W: u32 = 420;

/// How tall it is, derived from what it contains rather than chosen.
///
/// Top border, title, body, button strip, frame and border, with `TEXT_H_MAX` standing in for the
/// tallest line a theme asks for, since the title is *measured*; slack at smaller sizes sits below
/// the strip, so the list is exactly [`ROWS_H`] at every size.
pub const CHOOSER_H: u32 = WINDOW_BORDER
    + (2 * DIALOG_PAD + TEXT_H_MAX)
    + (FIELD_GAP + UP_H)
    + ROWS_H
    + (FIELD_GAP + FIELD_H)
    + (DIALOG_BUTTON_H + DIALOG_PAD)
    + WINDOW_FRAME
    + WINDOW_BORDER;

/// The tallest line of text this frame is sized to survive.
const TEXT_H_MAX: u32 = 20;

/// How tall the list is, in pixels — enough rows to recognise a directory without scrolling.
const ROWS_H: u32 = 220;
/// One row's height, matching the browser's so a listing does not change shape between them.
const ROW_H: u32 = 20;
/// The name field's height.
const FIELD_H: u32 = 24;
/// The gap between the list and the name field below it.
const FIELD_GAP: u32 = 4;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Tree;
    use crate::layout::{FixedCell, Layout, layout, locate};
    use alloc::string::String;
    use libdraw::geom::Rect;

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const BOUNDS: Rect = Rect::new(0, 0, 420, 340);

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum Msg {
        Row(u64),
        Up,
        Accept,
        Cancel,
    }

    /// **No row may be named what a `Save` field is seeded with**, or every assertion about the
    /// field is satisfied by the row label.
    fn rows() -> Vec<ListRow<'static>> {
        alloc::vec![
            ListRow { key: 1, label: "papers", marked: false, ..Default::default() },
            ListRow { key: 2, label: "notes.txt", marked: false, ..Default::default() },
        ]
    }

    const SEEDED: &str = "a-name-no-row-has.txt";

    fn find<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
        if e.key == Some(key) {
            return Some(e);
        }
        e.children().find_map(|c| find(c, key))
    }

    fn build(mode: Mode, st: &mut ChooserState) -> Element<Msg> {
        view(
            mode,
            "/home",
            &rows(),
            st,
            100,
            None,
            Msg::Row,
            Msg::Up,
            Msg::Accept,
            Msg::Cancel,
            true,
            &Theme::default(),
        )
    }

    fn labels<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let crate::element::Node::Text(t) = &e.node {
            out.push(t.clone());
        }
        for c in e.children() {
            labels(c, out);
        }
    }

    #[test]
    fn saving_adds_a_name_field_and_opening_does_not() {
        let (mut o, mut s) = (ChooserState::new(), ChooserState::saving(SEEDED));
        let (open, save) = (build(Mode::Open, &mut o), build(Mode::Save, &mut s));

        let (mut a, mut b) = (Vec::new(), Vec::new());
        labels(&open, &mut a);
        labels(&save, &mut b);
        assert!(a.iter().any(|l| l == "Open File") && a.iter().any(|l| l == "Open"), "{a:?}");
        assert!(b.iter().any(|l| l == "Save As") && b.iter().any(|l| l == "Save"), "{b:?}");
        assert!(a.iter().any(|l| l == "/home") && b.iter().any(|l| l == "/home"));

        // **The field is asserted as a node, not as a string**: a label search cannot tell a seeded
        // field from a row that reads the same.
        let body = |e| find(e, 106).expect("the body column is keyed 106").children().count();
        assert_eq!(body(&open), 2, "Open is the path strip and the list");
        assert_eq!(body(&save), 3, "Save adds the name field to them");
        assert!(find(&save, 101).is_some(), "the name field is in the Save tree");
        assert!(find(&open, 101).is_none(), "and is not in the Open tree");

        assert!(b.iter().any(|l| l == SEEDED), "the field is not seeded: {b:?}");
    }

    /// **The list and the field get the height they were built for**: `list_view` is drawn at
    /// whatever the layout leaves, so a mismatch means unreachable rows and a `Save` name field
    /// with no pixels. Bracketed across cell heights so it cannot pass by a metric coincidence.
    #[test]
    fn the_list_and_the_field_are_drawn_at_the_size_they_were_built_for() {
        for h in [8u32, 10, 12, 14, 16, 20] {
            let cell = FixedCell { w: 8, h };
            let bounds = Rect::new(0, 0, CHOOSER_W, CHOOSER_H);
            let mut st = ChooserState::saving("notes.txt");
            let e = build(Mode::Save, &mut st);
            let l = layout(&e, bounds, &cell);
            let list = crate::layout::locate(&e, &l, 100).expect("the list is keyed 100");
            assert_eq!(
                list.size.h, ROWS_H,
                "cell height {h}: the list is drawn {}px tall and was built for {ROWS_H}",
                list.size.h
            );
            let field = crate::layout::locate(&e, &l, 101).expect("the field is keyed 101");
            assert_eq!(field.size.h, FIELD_GAP + FIELD_H, "cell height {h}: the name field is {}px tall", field.size.h);
        }
    }

    /// The name field is drawn **active**, asserted as a difference rather than a count; the
    /// inactive one is the negative control.
    #[test]
    fn the_name_field_is_drawn_with_a_caret() {
        fn nodes<M>(e: &Element<M>) -> usize {
            1 + e.children().map(nodes).sum::<usize>()
        }
        let theme = Theme::default();
        let mut st = ChooserState::saving(SEEDED);
        let lit =
            nodes(&text_field::<Msg>(&st.name, false, WidgetState { active: true, ..Default::default() }, &theme));
        let rest = nodes(&text_field::<Msg>(&st.name, false, WidgetState::default(), &theme));
        assert!(lit > rest, "an active field should cost more nodes than a resting one");

        let save = build(Mode::Save, &mut st);
        let field = find(&save, 101).expect("the name field is keyed 101");
        assert_eq!(nodes(field), lit + 2, "the chooser's field is not the active one");
        assert_ne!(nodes(field), rest + 2, "…and is not the resting one");
    }

    #[test]
    fn the_chooser_diffs_across_a_selection() {
        let mut st = ChooserState::new();
        let mut tree = Tree::new();
        for sel in [None, Some(0), Some(1), None] {
            st.list.selected = sel;
            let e = build(Mode::Save, &mut st);
            let l = layout(&e, BOUNDS, &CELL);
            tree.update(&e, &l).unwrap_or_else(|err| panic!("selection {sel:?}: {err:?}"));
        }
    }

    fn text_rect<M>(e: &Element<M>, l: &Layout, want: &str) -> Option<Rect> {
        if let crate::element::Node::Text(t) = &e.node
            && t == want
        {
            return Some(l.rect);
        }
        e.children().zip(l.children.iter()).find_map(|(c, cl)| text_rect(c, cl, want))
    }

    /// The path sits on the up button's middle and starts beside it, not in the middle of the
    /// dialog (`center` put it at x = 207 of 420).
    #[test]
    fn the_path_sits_beside_the_up_button_not_in_the_middle_of_the_dialog() {
        let mut st = ChooserState::new();
        let e = build(Mode::Open, &mut st);
        let l = layout(&e, BOUNDS, &CELL);
        let up = locate(&e, &l, 100 + 7).expect("the up button is keyed key_base + 7");
        let path = text_rect(&e, &l, "/home").expect("the path is drawn");

        assert_eq!(
            path.origin.x,
            up.origin.x + up.size.w as i32 + 6,
            "the path should start 6px right of the up button, not at {}",
            path.origin.x
        );
        let middle = BOUNDS.origin.x + (BOUNDS.size.w.saturating_sub(path.size.w) / 2) as i32;
        assert_ne!(path.origin.x, middle, "the path is centred across the dialog");

        let path_mid = path.origin.y + (path.size.h / 2) as i32;
        let up_mid = up.origin.y + (up.size.h / 2) as i32;
        assert!((path_mid - up_mid).abs() <= 1, "the path's middle is at {path_mid} and the button's at {up_mid}");
    }
}
