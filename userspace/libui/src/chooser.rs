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
    DIALOG_BUTTON_H, DIALOG_PAD, ListRow, ListState, Theme, WINDOW_BORDER, WINDOW_FRAME, button,
    dialog_frame_sized, list_view, text_field, TextFieldState, WidgetState,
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

/// The element keys this widget uses, from `key_base` upwards: `key_base .. key_base + KEYS`.
///
/// A caller picks a base whose range collides with nothing of its own — including **the keys it
/// gives the rows**, which are its numbering and not this widget's. `hovered` is matched here
/// against the buttons' keys and inside `list_view` against the rows', so a row numbered onto a
/// button lights that button as the pointer passes over it. It was seven keys called six until
/// PR #284's review, which is the version of this that bites a caller starting its next range at
/// `key_base + KEYS`.
pub const KEYS: u64 = 7;

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
    // **Sized to the height it was built for**, which is the rule every other `list_view` caller
    // follows and this one did not: the widget computes `visible` from the height it is *told*,
    // so a list drawn shorter than that has rows it will not scroll to and no scrollbar to say so
    // — `rows.len() > visible` is false, and `ensure_visible` moves nothing (PR #284 review,
    // blocking 1).
    let rows = sized(
        Size::new(0, ROWS_H),
        list_view(entries, &mut state.list, ROWS_H, ROW_H, on_row, None, None, hovered, theme),
    )
    .key(key_base);
    // **Active, because in `Save` this field is the only thing taking characters.** The chooser's
    // window holds the keyboard and the application routes every key into this state, so a field
    // drawn at rest — no caret, no focus ring — is a live control that looks disabled. Every other
    // in-application field passes `active: true`; this one passed the default (PR #284 review,
    // finding 4).
    let name_field =
        text_field(&state.name, false, WidgetState { active: true, ..Default::default() }, theme);
    let mut body: Vec<Element<Msg>> = alloc::vec![here, rows];
    if mode == Mode::Save {
        // **The field is below the list, not above it.** What you type is the answer; the list is
        // context for it — and a field above the rows reads as a filter, which is a different
        // control this chooser does not have.
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
    // `dialog_frame_sized` docks the question beside a strip it wraps itself, so the question
    // carries a key — its own doc says so, and the diff enforces it.
    dialog_frame_sized(
        Size::new(CHOOSER_W, CHOOSER_H),
        title,
        column(body).key(key_base + 6),
        answers,
        theme,
    )
}

/// How wide the chooser's window is.
///
/// **Its own size, not a question's.** A confirmation dialog is 340x132, which is a two-line
/// question and two buttons; a chooser holds a list and a name field, and inside that frame it
/// drew one and a half rows and a field of zero height.
pub const CHOOSER_W: u32 = 420;

/// How tall it is, derived from what it contains rather than chosen.
///
/// Top border, the title (its own padding either side of one line of text), the body, the button
/// strip, and the frame and border below — with `TEXT_H_MAX` standing in for the tallest line of
/// text a theme is expected to ask for, since the title is *measured* rather than fixed and a
/// frame that fitted at 14px would clip at 20. Slack at smaller sizes sits below the strip, which
/// is why the list can still be asserted to be exactly [`ROWS_H`] at every one of them.
pub const CHOOSER_H: u32 = WINDOW_BORDER
    + (2 * DIALOG_PAD + TEXT_H_MAX)
    + (FIELD_GAP + TEXT_H_MAX)
    + ROWS_H
    + (FIELD_GAP + FIELD_H)
    + (DIALOG_BUTTON_H + DIALOG_PAD)
    + WINDOW_FRAME
    + WINDOW_BORDER;

/// The tallest line of text this frame is sized to survive.
///
/// Not a limit the toolkit enforces — a theme asking for more would crowd the list rather than
/// break it — but the number the height above is honest about depending on.
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

    /// **No row may be named what a `Save` field is seeded with.** The fixture used to list
    /// `notes.txt` and seed the field with `notes.txt`, so every assertion about the field was
    /// satisfied by the row label instead — the test passed with the field removed from the tree
    /// entirely (PR #284 review, blocking 3).
    fn rows() -> Vec<ListRow<'static>> {
        alloc::vec![
            ListRow { key: 1, label: "papers" },
            ListRow { key: 2, label: "notes.txt" },
        ]
    }

    /// What a `Save` chooser's field is seeded with here — deliberately not any row's label.
    const SEEDED: &str = "a-name-no-row-has.txt";

    /// The element carrying `key`, if the tree has one.
    fn find<'a, M>(e: &'a Element<M>, key: u64) -> Option<&'a Element<M>> {
        if e.key == Some(key) {
            return Some(e);
        }
        e.children().find_map(|c| find(c, key))
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
        let (mut o, mut s) = (ChooserState::new(), ChooserState::saving(SEEDED));
        let (open, save) = (build(Mode::Open, &mut o), build(Mode::Save, &mut s));

        let (mut a, mut b) = (Vec::new(), Vec::new());
        labels(&open, &mut a);
        labels(&save, &mut b);
        assert!(a.iter().any(|l| l == "Open File") && a.iter().any(|l| l == "Open"), "{a:?}");
        assert!(b.iter().any(|l| l == "Save As") && b.iter().any(|l| l == "Save"), "{b:?}");
        // Both show where they are, which is what a Save dialog must not be vague about.
        assert!(a.iter().any(|l| l == "/home") && b.iter().any(|l| l == "/home"));

        // **The field is asserted as a node, not as a string.** A label search cannot tell a
        // seeded field from a row that happens to read the same, and that is exactly how this
        // test came to pass for a `Save` tree with no field in it at all. The body column holds
        // the path strip and the list in both modes, and the field only in `Save`.
        let body = |e| find(e, 106).expect("the body column is keyed 106").children().count();
        assert_eq!(body(&open), 2, "Open is the path strip and the list");
        assert_eq!(body(&save), 3, "Save adds the name field to them");
        assert!(find(&save, 101).is_some(), "the name field is in the Save tree");
        assert!(find(&open, 101).is_none(), "and is not in the Open tree");

        // Seeded with what the file is called — and `SEEDED` is a name no row carries, so this
        // can only be satisfied by the field.
        assert!(b.iter().any(|l| l == SEEDED), "the field is not seeded: {b:?}");
    }

    /// **The list and the field get the height they were built for**, which is what the widget
    /// was drawn at 340x132 for want of.
    ///
    /// `list_view` does not size itself — every other caller wraps it, and `nxfiles`'s wrapper
    /// carries the reason: the widget builds rows for the height it is *told* and is drawn at
    /// whatever the layout leaves, so a mismatch means rows that exist and cannot be reached
    /// (`visible` comes from the told height, so `rows.len() > visible` stays false and no
    /// scrollbar appears) and, in `Save`, a name field with no pixels at all. This chooser was the
    /// caller that omitted the wrapper *and* sat in a frame hard-sized for a two-line question.
    ///
    /// Bracketed across cell heights so it cannot pass by a metric coincidence.
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
            // The key sits on the *padded* field, which is the gap above it plus the field —
            // so this is `FIELD_H` seen through the wrapper the tree actually carries a key on.
            let field = crate::layout::locate(&e, &l, 101).expect("the field is keyed 101");
            assert_eq!(
                field.size.h,
                FIELD_GAP + FIELD_H,
                "cell height {h}: the name field is {}px tall",
                field.size.h
            );
        }
    }

    /// The name field is drawn **active**: it has a caret and a focus ring.
    ///
    /// **Asserted as a difference rather than as a count**, because the number of nodes a caret
    /// costs is `text_field`'s business and not this module's. What is this module's business is
    /// that it asked for the lit version — so the field in the tree is compared against both
    /// versions built directly, and the inactive one is the negative control.
    #[test]
    fn the_name_field_is_drawn_with_a_caret() {
        fn nodes<M>(e: &Element<M>) -> usize {
            1 + e.children().map(nodes).sum::<usize>()
        }
        let theme = Theme::default();
        let mut st = ChooserState::saving(SEEDED);
        let lit = nodes(&text_field::<Msg>(
            &st.name,
            false,
            WidgetState { active: true, ..Default::default() },
            &theme,
        ));
        let rest = nodes(&text_field::<Msg>(&st.name, false, WidgetState::default(), &theme));
        assert!(lit > rest, "an active field should cost more nodes than a resting one");

        let save = build(Mode::Save, &mut st);
        let field = find(&save, 101).expect("the name field is keyed 101");
        // The key is on the padding, which wraps the `sized` that wraps the field itself.
        assert_eq!(nodes(field), lit + 2, "the chooser's field is not the active one");
        assert_ne!(nodes(field), rest + 2, "…and is not the resting one");
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
