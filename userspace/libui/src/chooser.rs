//! The file chooser: a dialog for picking a file to open, or naming one to save.
//!
//! **A widget over a listing, not a browser** (M14 decision 3). `libui` cannot make a syscall and
//! that rule is not bent here: this module renders and routes over entries it is *given*, and the
//! application lists the directory through [`libfs::list_dir`] and orders it with `libfs::sort`.
//! What is here is what a chooser *is* — a title, a path, a list of rows, a name field when
//! saving, and two answers — and none of that needs to know what a filesystem is.
//!
//! **One widget for both jobs**, because Open and Save As differ in exactly two things: whether
//! there is a name field, and what the accepting button says. A second widget would be the same
//! layout maintained twice, and the first divergence would be a chooser that looks different
//! depending on why it opened.
//!
//! ## What the application still owns
//!
//! Where it is (the current directory), what is in it (the entries), and what happens when a
//! choice is made. This module answers only "what did the person do to the chooser".

use alloc::vec::Vec;

use libdraw::geom::Size;

use crate::element::{Element, Insets, column, padding, row, sized, text};
use crate::widget::{
    DIALOG_PAD, ListRow, ListState, Theme, button, dialog_frame, list_view, text_field,
    TextFieldState, WidgetState,
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
    ///
    /// **The verb, not "OK".** A button that says what it will do is the difference between a
    /// dialog you read and one you dismiss — and it is the only word that differs between the two
    /// jobs once the field is accounted for.
    pub fn verb(self) -> &'static str {
        match self {
            Mode::Open => "Open",
            Mode::Save => "Save",
        }
    }
}

/// What the person has done to the chooser: the part that is not the directory's contents.
///
/// **The listing is deliberately absent.** It belongs to the application, which read it; keeping a
/// copy here would be a second answer to "what is in this directory" that could go stale without
/// anything noticing.
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
        Self { list: ListState { selected: None, offset: 0 }, name: TextFieldState::new() }
    }

    /// A chooser opened to save `name` — the buffer's current name, so Save As starts from what
    /// the file is called rather than from nothing.
    pub fn saving(name: &str) -> Self {
        Self {
            list: ListState { selected: None, offset: 0 },
            name: TextFieldState::with_text(name),
        }
    }
}

impl Default for ChooserState {
    fn default() -> Self {
        Self::new()
    }
}

/// The element keys this widget uses, from `key_base` upwards.
///
/// A caller picks a range that does not collide with its own; the chooser is a whole window's
/// content in practice, so any base clear of the frame's own keys will do.
pub const KEYS: u64 = 6;

/// The chooser's tree: a path strip, the rows, a name field when saving, and two answers.
///
/// `entries` is what the application listed and ordered. `on_row` is sent when a row is
/// activated — the application decides whether that means "descend" or "choose", because only it
/// knows which rows are directories.
#[allow(clippy::too_many_arguments)]
pub fn view<Msg: Clone>(
    mode: Mode,
    path: &str,
    entries: &[ListRow<'_>],
    state: &mut ChooserState,
    key_base: u64,
    hovered: Option<u64>,
    on_row: fn(u64) -> Msg,
    accept: Msg,
    cancel: Msg,
    theme: &Theme,
) -> Element<Msg> {
    let title = padding(Insets::all(DIALOG_PAD), text(match mode {
        Mode::Open => "Open File",
        Mode::Save => "Save As",
    }));
    // **Where you are, always shown.** A chooser that only listed names would leave the person
    // guessing which directory they are about to write into, which is the one thing a Save dialog
    // must not be vague about.
    // **Keyed, like every other child of the body column.** `diff` refuses a parent whose
    // children are only *partly* keyed, and the rows below carry a key — so an unkeyed path strip
    // makes the whole dialog undiffable, which shows up as a window that opens and never draws
    // rather than as anything resembling a layout problem. `the_chooser_diffs_across_a_selection`
    // caught it on the first run.
    let here =
        padding(Insets { top: 0, right: DIALOG_PAD, bottom: 4, left: DIALOG_PAD }, text(path))
            .key(key_base + 4);
    let rows = list_view(
        entries,
        &mut state.list,
        ROWS_H,
        ROW_H,
        on_row,
        None,
        None,
        hovered,
        theme,
    )
    .key(key_base);
    let name_field = text_field(&state.name, false, WidgetState::default(), theme);
    let mut body: Vec<Element<Msg>> = alloc::vec![here, rows];
    if mode == Mode::Save {
        // **The field is below the list, not above it.** What you type is the answer; the list is
        // context for it — and a field above the rows reads as a filter, which is a different
        // control this chooser does not have.
        body.push(
            padding(
                Insets { top: 4, right: DIALOG_PAD, bottom: 0, left: DIALOG_PAD },
                sized(Size::new(0, FIELD_H), name_field),
            )
            .key(key_base + 1),
        );
    }
    let answers = row(alloc::vec![
        text("").flex(1).key(key_base + 5),
        button(
            "Cancel",
            cancel,
            WidgetState { hovered: hovered == Some(key_base + 2), ..Default::default() },
            theme,
        )
        .key(key_base + 2),
        // **The key goes on the child of the row, not on the button inside it.** The diff looks
        // at a parent's direct children; a keyed button wrapped in an unkeyed `padding` leaves the
        // row mixed, which is the same undiffable-dialog failure as an unkeyed sibling and took a
        // probe to see rather than a guess.
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
    // `dialog_frame` docks the question beside a strip it wraps itself, so the question
    // carries a key — its own doc says so, and the diff enforces it.
    dialog_frame(title, column(body).key(key_base + 6), answers, theme)
}

/// How tall the list is, in pixels — enough rows to recognise a directory without scrolling.
const ROWS_H: u32 = 220;
/// One row's height, matching the browser's so a listing does not change shape between them.
const ROW_H: u32 = 20;
/// The name field's height.
const FIELD_H: u32 = 24;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use crate::diff::Tree;
    use crate::layout::{FixedCell, layout};
    use libdraw::geom::Rect;

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const BOUNDS: Rect = Rect::new(0, 0, 420, 340);

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum Msg {
        Row(u64),
        Accept,
        Cancel,
    }

    fn rows() -> Vec<ListRow<'static>> {
        alloc::vec![
            ListRow { key: 1, label: "papers" },
            ListRow { key: 2, label: "notes.txt" },
        ]
    }

    fn build(mode: Mode, st: &mut ChooserState) -> Element<Msg> {
        view(mode, "/home", &rows(), st, 100, None, Msg::Row, Msg::Accept, Msg::Cancel,
             &Theme::default())
    }

    fn labels<M>(e: &Element<M>, out: &mut Vec<String>) {
        if let crate::element::Node::Text(t) = &e.node {
            out.push(t.clone());
        }
        for c in e.children() {
            labels(c, out);
        }
    }

    /// Both jobs are one tree, and the difference between them is the field and the verb.
    ///
    /// **The point of one widget** (M14 decision 3): a second one would be this layout maintained
    /// twice, and the first divergence would be a chooser that looks different depending on why
    /// it opened.
    #[test]
    fn saving_adds_a_name_field_and_opening_does_not() {
        let (mut o, mut s) = (ChooserState::new(), ChooserState::saving("notes.txt"));
        let (open, save) = (build(Mode::Open, &mut o), build(Mode::Save, &mut s));

        let (mut a, mut b) = (Vec::new(), Vec::new());
        labels(&open, &mut a);
        labels(&save, &mut b);
        assert!(a.iter().any(|l| l == "Open File") && a.iter().any(|l| l == "Open"), "{a:?}");
        assert!(b.iter().any(|l| l == "Save As") && b.iter().any(|l| l == "Save"), "{b:?}");
        // Both show where they are, which is what a Save dialog must not be vague about.
        assert!(a.iter().any(|l| l == "/home") && b.iter().any(|l| l == "/home"));
        // The name the buffer already has is what Save As starts from.
        assert!(b.iter().any(|l| l.contains("notes.txt")), "the field is not seeded: {b:?}");
        // …and Open has no field to seed.
        assert!(!a.iter().any(|l| l.contains("notes.txt") && l != "notes.txt"), "{a:?}");
    }

    /// The chooser's tree diffs frame to frame, including across a selection.
    ///
    /// **The failure this catches does not look like a bug in a chooser**: a tree whose child
    /// count changes with state is undiffable, and `Child::present` then returns false every
    /// frame — a dialog that opens, reports its size, and never draws.
    #[test]
    fn the_chooser_diffs_across_a_selection() {
        let mut st = ChooserState::new();
        let mut tree = Tree::new();
        for sel in [None, Some(0), Some(1), None] {
            st.list.selected = sel;
            let e = build(Mode::Save, &mut st);
            let l = layout(&e, BOUNDS, &CELL);
            // **The message names the offending parent's index path**, which is what made
            // this findable: three separate parents here had a keyed child wrapped in an unkeyed
            // `padding`, and the path is the only thing that distinguishes them.
            tree.update(&e, &l).unwrap_or_else(|err| panic!("selection {sel:?}: {err:?}"));
        }
    }
}
