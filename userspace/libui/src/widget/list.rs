//! The list view: rows, columns, row buttons and swatches.

use super::*;

/// One row of a [`list_view`].
///
/// **Borrowed, and built fresh each frame from whatever the application already has.** That
/// is the "model" in model-backed: a window list derives these from its window records and a
/// launcher derives them from its filtered program list, neither keeping a parallel array of
/// row widgets to reconcile by hand — which is the hand-rolled diffing `desktop-shell.md` §5
/// says a list widget exists to avoid.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ListRow<'a> {
    /// Identity across frames.
    ///
    /// **Not the index.** [`Element::key`](crate::element::Element::key) exists because the
    /// diff otherwise pairs by position, and its own doc names this failure: insert a row at
    /// the top and "the diff pairs row 2's widget with row 3's element". A window list
    /// reorders on every raise, so position is never stable here. Use the window id, or the
    /// index into the *unfiltered* list — anything that survives the list changing shape.
    ///
    /// **Must be unique among the rows in one list.** A repeat is
    /// [`DiffError::DuplicateKey`](crate::diff::DiffError::DuplicateKey) — a hard error from
    /// the diff, not a degraded pairing — which is worth knowing before reaching for a hash of
    /// a name that two rows could share.
    pub key: u64,
    /// What the row says.
    pub label: &'a str,
    /// Whether this row is part of a **multiple** selection, beside `ListState::selected`.
    ///
    /// **A property of the row, not a second selection in the state** — the same shape [`Tab`]
    /// uses. `ListState` is `Copy` and every list in the system builds its rows each frame
    /// anyway, so a caller that knows a row is picked can just say so; a set inside the state
    /// would cost every caller a `Vec` for a thing only the file browser has.
    ///
    /// Drawn exactly as `selected` is, because to a person they are the same thing: the rows an
    /// action will act on.
    pub marked: bool,
    /// The trailing cells, one per column the list declares (desktop refresh, Part I).
    ///
    /// **Empty for every list but one.** A browser's listing is a name and three facts about it;
    /// a window list, a launcher's results and a chooser's rows are a name and nothing else, and
    /// they say so by leaving this `&[]` — which is what `Default` gives them.
    ///
    /// Fewer cells than columns is fine: the rest are blank. More are ignored.
    pub cells: &'a [&'a str],
    /// A small shape before the label — the browser's folder-or-file mark, or a place's dot.
    pub swatch: Option<Swatch>,
    /// A control at the row's trailing end — a drive's eject (Phase 6 Part F) — or `None`.
    pub button: Option<RowButton<'a>>,
}

/// A control at a row's trailing end: a glyph that does something to the row's subject rather
/// than opening it, as a tab's `×` closes its tab (Phase 6 Part F).
///
/// **Pressed through the list's own `activate`, with a key of its own**, so a list with buttons
/// takes no more parameters than one without: the caller tells a press on the button from one on
/// the row by the key, as it tells rows apart. The key must differ from every row's key in the
/// list, the row's own included.
///
/// **Over the button is still over the row**: the row stays lit while the pointer is on its
/// button, as a tab does over its `×`. And a press on the button is not a grab of the row, by the
/// router's rule that a nearer `on_press` shadows an `on_press_down`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowButton<'a> {
    /// Its key — what `activate` is handed when it is pressed, and what `hovered` names over it.
    pub key: u64,
    /// What it shows: one glyph, drawn dim and in full ink under the pointer.
    pub glyph: &'a str,
}

/// How wide a row's button is, in pixels.
pub const ROW_BUTTON_W: u32 = 24;

/// The face behind a row's button under the pointer: the tab close box's.
const ROW_BUTTON_HOVER: u32 = TAB_CLOSE_HOVER + 2;

/// The shape drawn before a row's label. See [`ListRow::swatch`].
///
/// **Two shapes, named rather than described at each call site**: the design marks a listing's
/// rows with a small block and a sidebar's places with a dot, and a caller that spelled out
/// widths and radii would be free to invent a third (desktop refresh, Part I).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Swatch {
    /// What colour it is drawn in.
    pub colour: Rgb,
    /// How wide, in pixels.
    pub width: u32,
    /// How tall, in pixels.
    pub height: u32,
    /// The corner it is rounded to; half the height makes it round.
    pub radius: u32,
}

impl Swatch {
    /// The listing's mark: the design's 11×9 with the barest corner on it.
    pub fn block(colour: Rgb) -> Self {
        Self { colour, width: SWATCH_W, height: SWATCH_H, radius: 1 }
    }

    /// A place's dot in a sidebar: the design's 5, round.
    pub fn dot(colour: Rgb) -> Self {
        Self { colour, width: DOT_W, height: DOT_W, radius: DOT_W / 2 }
    }
}

/// A trailing column in a list: how wide it is, and which end its text sits against.
///
/// **Fixed widths, and the label takes the rest.** The design's browser is `Name` flexible with
/// `Size`, `Kind` and `Modified` at 70, 60 and 96 — columns that shared the width out would move
/// under each other as a listing changed, which is the argument `TAB_W` already makes for tabs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListColumn {
    /// How wide, in pixels.
    pub width: u32,
    /// Whether the text sits against the column's left edge or its right.
    ///
    /// **A size reads right-aligned** — the digits line up and the eye compares them — and
    /// everything else reads left.
    pub align: ColumnAlign,
}

/// Which end of a column its text sits against. See [`ListColumn::align`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColumnAlign {
    /// Against the left edge.
    Left,
    /// Against the right edge.
    Right,
}

/// Which row is selected, and how far the list is scrolled.
///
/// Scroll is a row index rather than a pixel offset: a list scrolls by whole rows, and the
/// arithmetic that keeps a selection visible is unreadable in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ListState {
    /// The selected row, as an index into the rows passed to [`list_view`].
    pub selected: Option<usize>,
    /// The first visible row.
    pub offset: usize,
    /// What the last [`ensure_visible`](Self::ensure_visible) was for: the selected row, and how
    /// many rows fitted.
    ///
    /// **What stops "keep the selection on screen" from meaning "never scroll anywhere else".**
    /// `list_view` calls `ensure_visible` on every build, so without this a list whose selection
    /// is row 0 snaps its offset back to 0 on every repaint — and since an application repaints
    /// after every event, a scrollbar drag computed the right offset and had it thrown away
    /// before anything was drawn. The bar moved and the list did not (M15 Part D).
    ///
    /// **The row count is part of it**, so a window that got shorter brings the selection back
    /// into view rather than waiting for it to move. `TextAreaState` carries the text's revision
    /// here too; a list has no equivalent, because everything that changes a list's rows also
    /// replaces its selection.
    pub(super) followed: Option<(usize, usize)>,
}

impl ListState {
    /// A list showing `offset` first, with `selected` picked.
    ///
    /// **A constructor rather than a literal**, since M15 Part D: the state carries one piece of
    /// bookkeeping ([`followed`](Self::followed)) that no caller has an opinion about, and a
    /// private field cannot be written in a struct literal from another module — which is the
    /// point. `selected` and `offset` stay public, because a caller does have opinions about
    /// those.
    pub fn at(selected: Option<usize>, offset: usize) -> Self {
        Self { selected, offset, followed: None }
    }

    /// Move the selection down one row, answering whether anything changed.
    ///
    /// From nothing selected this selects the first row, which is what a launcher wants when
    /// the user presses Down having only typed a query.
    pub fn down(&mut self, len: usize) -> bool {
        if len == 0 {
            return false;
        }
        let next = match self.selected {
            None => 0,
            Some(i) if i + 1 < len => i + 1,
            Some(i) => i,
        };
        let moved = self.selected != Some(next);
        self.selected = Some(next);
        moved
    }

    /// Move the selection up one row, answering whether anything changed.
    pub fn up(&mut self) -> bool {
        let Some(i) = self.selected else {
            return false;
        };
        if i == 0 {
            return false;
        }
        self.selected = Some(i - 1);
        true
    }

    /// Scroll so the selected row is on screen, given how many rows fit.
    ///
    /// **The thing both callers would get wrong.** A selection moved with the keyboard walks
    /// off the visible window and the list appears to stop responding — the state changed and
    /// nothing on screen did. Called by [`list_view`] itself rather than left to the caller,
    /// because a widget that can compute this and makes its callers do it is a widget that
    /// will have two implementations of it.
    pub fn ensure_visible(&mut self, visible: usize) {
        let Some(i) = self.selected else {
            return;
        };
        if visible == 0 {
            return;
        }
        // **Once per selection, not once per build.** Following a selection is what a *changed*
        // selection asks for; doing it every time the widget is built makes every other way of
        // scrolling impossible, because the next repaint undoes it.
        if self.followed == Some((i, visible)) {
            return;
        }
        self.followed = Some((i, visible));
        if i < self.offset {
            self.offset = i;
        } else if i >= self.offset + visible {
            self.offset = i + 1 - visible;
        }
    }

    /// Apply a key, answering whether the list changed.
    ///
    /// Declines everything it does not claim, for the reason
    /// [`TextFieldState::apply`](TextFieldState::apply) does: Enter activates the selection
    /// and Escape dismisses the list, and neither belongs to the list itself. A launcher's
    /// query field and its results list are both focusable, and arrow keys have to reach the
    /// list while the field holds focus — so a caller routes these itself rather than relying
    /// on widget focus, which is why this takes a keycode instead of being wired to `on_key`.
    pub fn apply(&mut self, keycode: u16, len: usize) -> bool {
        match keycode {
            KEY_DOWN => self.down(len),
            KEY_UP => self.up(),
            _ => false,
        }
    }
}

/// How much space a row's label gets around it — public so a heading over a list's rows can
/// start where they do (Files' Drives, Phase 6 Part F).
pub const ROW_PAD: Insets = Insets { top: 2, right: 6, bottom: 2, left: 6 };

impl ListState {
    /// Scroll by a turn of the wheel, `dz` detents, positive **down**.
    ///
    /// **No upper clamp here**, deliberately: [`list_view`] already clamps the offset against
    /// the rows it is given every time it builds, and it is the one that knows how many fit.
    /// A second clamp would need the caller to pass a count it has no other reason to have,
    /// and two clamps disagreeing is how a list ends up unable to reach its last row.
    ///
    /// **Widened before multiplying**, because `dz` saturates at the compositor: a consumer
    /// that stalled while somebody scrolled receives thousands of detents, and `i16`
    /// arithmetic on that scrolls the other way.
    pub fn wheel(&mut self, dz: i16) {
        let rows = i64::from(dz) * i64::from(crate::click::WHEEL_UNITS);
        self.offset = if rows < 0 {
            self.offset.saturating_sub(rows.unsigned_abs() as usize)
        } else {
            self.offset.saturating_add(rows as usize)
        };
    }

    /// This list as a scrollbar's state.
    ///
    /// **The conversion belongs here, not in each caller**, because the widget already knows the
    /// arithmetic and the caller only knows the numbers it passed in. `nxterm` builds the same
    /// thing from a grid's line numbers — this is that for a list, so the two cannot drift apart
    /// on rounding.
    ///
    /// Takes the same `height`, `row_height` and row count [`list_view`] was given: a drag
    /// converted against a different geometry from the one drawn puts the thumb where the pointer
    /// is not (M11 Part E batch 6).
    pub fn bar(&self, height: u32, row_height: u32, total: usize) -> ScrollState {
        let visible = height.checked_div(row_height).unwrap_or(0);
        ScrollState { offset: self.offset as u32, visible, total: total as u32 }
    }
}

/// A scrolling list of rows, with one selected.
///
/// **The one model-backed widget** `desktop-shell.md` §5 settles on — "an explicit toolkit
/// *plus one model-backed list widget*" covering the window list, the desktop previews and the
/// launcher results, "which is essentially all of the churn, for a fraction of a diffing
/// engine's machinery". Designed against two of those callers rather than one, because a model
/// API drawn for a single consumer is the failure mode §5 was avoiding: a window list is
/// **reordered and mutated in place** while a launcher's results are **replaced wholesale on
/// every keystroke**, and those stress different halves — the first the keying, the second the
/// selection and scroll surviving a list that changed length.
///
/// **Only the visible rows become elements.** That is the point of the widget rather than an
/// optimisation: a list of a hundred windows costs as many elements as fit on screen, and the
/// diff walks that many.
///
/// **`height` must be the height the parent will actually give it** — the same obligation
/// [`scrollbar`] states, and this widget does not size itself to it, so a caller that lets the
/// list flex will show a different number of rows than it built. Wrapping the result in
/// `sized` is what [`crate::reference`] does and is the reliable way to keep the two in step.
/// (An earlier version of this sentence claimed a caller "cannot get it out of step", which is
/// exactly backwards; PR #233 review.)
///
/// **`ground` is the colour behind the rows**, or `None` for the ordinary list ground. The one
/// caller that passes something is a *sidebar*: a panel beside content has to be told from the
/// content at a glance, and drawn in the list's own ground it reads as a list with a gap in it
/// (M15). A colour rather than a flag, because what a panel is depends on the theme.
///
/// **`state` is taken by `&mut` and scrolled in place** to follow the selection — see
/// [`ensure_visible`](ListState::ensure_visible).
///
/// It returned the scrolled state instead until M10 Part C, and the difference is not
/// stylistic: **nothing in the type system could make a caller keep it.** `#[must_use]` fires on
/// an *unused* return, and `let (e, _) = list_view(…)` uses the tuple; putting it on `ListState`
/// does not help either, since binding to `_` is the documented way to silence exactly that
/// lint. `ListState` is `Copy`, which removed the last chance — a caller passing `self.list`
/// by value and dropping the result kept a perfectly valid stale copy, where a non-`Copy` state
/// would have been a move-out error. `nxfiles` shipped precisely that: an offset re-derived
/// from zero every frame, so the selection never left the last visible row (PR #257 review).
///
/// The obligation also *propagated*: `desktop-shell` grew `(T, ListState)` returns three
/// functions deep to carry state none of them used. In-place update is what Rust uses for this
/// — `Vec::sort`, `Vec::retain`, `read_line(&mut String)` — and a returned value is for when a
/// caller may genuinely decline it. There is no correct program that ignores a scroll offset.
#[allow(
    clippy::too_many_arguments,
    reason = "the list's rows, handlers and appearance are separate inputs of one constructor"
)]
pub fn list_view<Msg>(
    rows: &[ListRow<'_>],
    columns: &[ListColumn],
    state: &mut ListState,
    height: u32,
    row_height: u32,
    activate: fn(u64) -> Msg,
    grab: Option<fn(u64) -> Msg>,
    scroll: Option<fn(PointerEvent) -> Msg>,
    hovered: Option<u64>,
    ground: Option<Rgb>,
    theme: &Theme,
) -> Element<Msg> {
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
    // **The selection is clamped first, because it is an index into a list that may have just
    // been replaced.** A launcher rebuilds its results on every keystroke, so a selection made
    // against twenty of them is not an index into the three that remain — and a stale one is
    // worse than useless: nothing paints as selected, `down` sees `i + 1 < len` fail and
    // returns the same index so the key is *dead*, and a caller reading `selected` to decide
    // what Enter activates gets an out-of-range index. Clamped to the last surviving row
    // rather than cleared, so something is highlighted and the arrows work on the next press
    // (PR #233 review, finding 2).
    if let Some(i) = state.selected
        && i >= rows.len()
    {
        state.selected = rows.len().checked_sub(1);
    }
    state.ensure_visible(visible);
    // Never scrolled past the end: a list that shrinks under a stale offset would otherwise
    // render blank while holding rows.
    let max_offset = rows.len().saturating_sub(visible);
    state.offset = state.offset.min(max_offset);

    // **The surface the rows sit on.** A row used to fill `theme.track` whatever the list's
    // ground was, so a panel with a ground of its own had list-coloured tiles painted over it and
    // the panel showed only below the last row (M15 Part F).
    //
    // **And a highlight is a wash over it** (desktop refresh, Part B): the accent at the
    // selection's 20%, or at the scheme's hover coverage. That is the design's model, and it is
    // why the hover no longer has to be derived per ground — "this surface, lit" was what the
    // derivation approximated, and a wash over whatever surface is under it is that exactly.
    let ground = ground.unwrap_or(theme.track);
    let hover = theme.scheme.hover_coverage();
    let last = (state.offset + visible).min(rows.len());
    let mut items = alloc::vec::Vec::with_capacity(last.saturating_sub(state.offset));
    for (i, r) in rows.iter().enumerate().take(last).skip(state.offset) {
        let selected = state.selected == Some(i) || r.marked;
        // **A selection is the accent washed over the ground**, flat, since the desktop refresh's
        // Part B. It was a one-pixel accent border around a bevelled fill (M11 Part E, batch 2),
        // the reference desktop's, and the border was what kept two adjacent selections from
        // merging into one block; the design lets them merge, as a block is what a run of
        // selected rows is.
        //
        // **Hover is quieter than selection and loses to it** (batch 3) — *unless nothing is
        // selected*, in which case it is the answer and gets the blue (batch 5). The rule is
        // still "one primary highlight": two of equal weight is two answers to "what happens if
        // I act now", and where there is no keyboard selection to compete with, the pointer's is
        // not competing. The applications modal is exactly that list — it keeps no selection at
        // all, so every hover landed on the quiet branch and the menu highlighted in grey.
        // Over a row's button is over the row (see [`RowButton`]).
        let over = hovered == Some(r.key) || r.button.is_some_and(|b| hovered == Some(b.key));
        let primary = selected || (over && state.selected.is_none());
        let mut layers = alloc::vec![fill(ground)];
        if primary {
            layers.push(wash(theme.accent, libdraw::theme::SELECTION_COVERAGE));
        } else if over {
            layers.push(wash(theme.accent, hover));
        } else if r.button.is_some() {
            // **A row with a button keeps its layers in place, lit or not** (Phase 6 Part F). The
            // router finds the widget a press captured by its id, and an id survives a repaint
            // only while its siblings keep their positions: a wash inserted here when the press
            // lit the row moved the body and its button along one, and the release that followed
            // the repaint was lost — or, in `check-media`, opened the drive instead of ejecting
            // it. A row with no button has its handler on the row itself, which no layer moves.
            layers.push(text(""));
        }
        // **The label, then whatever trailing cells the list declares** (desktop refresh,
        // Part I). A row with no columns is what it always was: one padded label. With them,
        // the label takes the space the columns leave — so a long name is what gets cut short,
        // never a size or a date, which are the same width in every row and are read as a
        // column rather than as text.
        // **A plain row stays exactly what it was**: one padded label, with no row wrapper around
        // it. Every list in the system but the browser's has no columns and no swatch, and
        // wrapping those in a `Row` would put a node per row in every window list and launcher
        // result for nothing.
        if columns.is_empty() && r.swatch.is_none() && r.button.is_none() {
            layers.push(padding(ROW_PAD, text(r.label)));
            let row_el = stack(layers);
            let mut item = sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
            if let Some(f) = grab {
                item = item.on_press_down(f(r.key));
            }
            items.push(item);
            continue;
        }
        let mut cells = alloc::vec::Vec::with_capacity(columns.len() + 2);
        if let Some(sw) = r.swatch {
            // **The lead-in is the same width whatever the shape**, so a sidebar's dots and a
            // listing's blocks both leave their labels on one edge.
            cells.push(center_v(sized(
                Size::new(SWATCH_W, sw.height),
                center(sized(Size::new(sw.width, sw.height), rounded_fill(sw.colour, sw.radius))),
            )));
            cells.push(sized(Size::new(SWATCH_GAP, 0), text("")));
        }
        cells.push(center_v(text(r.label)).flex(1));
        for (i, c) in columns.iter().enumerate() {
            let cell = r.cells.get(i).copied().unwrap_or("");
            let text_el = center_v(ink(theme.foreground_dim, scaled(TextSize::Small, text(cell))));
            cells.push(sized(
                Size::new(c.width, 0),
                match c.align {
                    ColumnAlign::Left => text_el,
                    // **Pushed rather than aligned**: a `Row` lays its children out left to
                    // right, so the way to put text against the right edge is to give the space
                    // before it to something that takes what is left.
                    ColumnAlign::Right => {
                        row(alloc::vec![text("").flex(1), text_el, sized(Size::new(CELL_PAD, 0), text(""))])
                    }
                },
            ));
        }
        let body = padding(ROW_PAD, row(cells));
        match r.button {
            None => layers.push(body),
            // **Both halves keyed**, since the diff wants a container's children all keyed or none
            // and the button needs a key for hover to name it: the body takes the row's own, as a
            // tab's label takes its tab's, so `hovered` over it still names the row.
            Some(b) => {
                // The face may come and go: it is inside the button, which is the widget a press
                // captures, and that keeps its id however its own children move.
                let mut face = alloc::vec::Vec::with_capacity(2);
                let glyph_ink = if hovered == Some(b.key) {
                    face.push(center(sized(
                        Size::new(ROW_BUTTON_HOVER, ROW_BUTTON_HOVER),
                        rounded_fill(theme.face_pressed, TITLE_BUTTON_RADIUS),
                    )));
                    theme.foreground
                } else {
                    theme.foreground_dim
                };
                face.push(center(ink(glyph_ink, text(b.glyph))));
                layers.push(row(alloc::vec![
                    body.flex(1).key(r.key),
                    sized(Size::new(ROW_BUTTON_W, 0), stack(face).on_press(activate(b.key))).key(b.key),
                ]));
            }
        }
        let row_el = stack(layers);
        let mut item = sized(Size::new(0, row_height), row_el).key(r.key).on_press(activate(r.key));
        // **A press *down* on a row, for the caller that needs the gesture rather than the
        // click.** Dragging a row somewhere is decided the moment the button lands on it — by
        // the time it comes up the drag is over — and the row a press landed on is a fact this
        // widget has and its caller would otherwise recompute from the pointer's y, the row
        // height and the scroll offset. Three numbers to keep in step with this function is how
        // two implementations of "which row is that" come to disagree.
        //
        // `on_press` on the same element does **not** shadow it: the router's shadowing rule
        // compares depth, and these are the same element (M10 Part E).
        if let Some(f) = grab {
            item = item.on_press_down(f(r.key));
        }
        items.push(item);
    }

    let list = column(items);
    let body = if rows.len() > visible {
        let bar = ScrollState { offset: state.offset as u32, visible: visible as u32, total: rows.len() as u32 };
        // **The bar takes the pointer when the caller has somewhere to send it** (M11 Part E
        // batch 6). It was built without a handler, so a list's scrollbar showed a position and
        // could not be dragged — a control that looks live and is not, which is the defect this
        // toolkit's own notes keep naming. `nxterm` builds its scrollbar directly and has always
        // wired this; a list's was simply never offered.
        let mut bar_el = scrollbar(bar, SCROLLBAR_W, height, theme);
        if let Some(f) = scroll {
            bar_el = bar_el.on_pointer(f);
        }
        row(alloc::vec![list.flex(1), bar_el])
    } else {
        list
    };
    stack(alloc::vec![fill(ground), body]).focusable()
}

/// A row swatch: the design's 11×9, with the barest corner on it.
pub const SWATCH_W: u32 = 11;

/// See [`SWATCH_W`].
pub const SWATCH_H: u32 = 9;

/// A place's dot in a sidebar: the design's 5.
pub const DOT_W: u32 = 5;

/// Between a swatch and the name beside it: the design's 8.
const SWATCH_GAP: u32 = 8;

/// The space after a right-aligned cell's text, so a column of sizes does not sit hard against
/// the next column's edge.
const CELL_PAD: u32 = 6;

/// How tall a list's column header is: the design's 25.
pub const LIST_HEADER_H: u32 = 25;

/// The header over a list's columns: the primary heading, then one per column.
///
/// **Here rather than in the application**, because a heading that did not sit exactly over its
/// column would be worse than none — and the insets that put it there are this module's: a row's
/// padding, a swatch's width and the gap after it. One source, two readers, which is the
/// arrangement every pair of numbers in this refresh ended up with (desktop refresh, Part I).
///
/// The headings are `Small` and dim, like the cells under them, on the `face_hover` ground every
/// strip of chrome in a window uses.
pub fn list_header<Msg>(
    primary: &str,
    headings: &[&str],
    columns: &[ListColumn],
    swatched: bool,
    scrolling: bool,
    theme: &Theme,
) -> Element<Msg> {
    let heading = |s: &str| center_v(ink(theme.foreground_dim, scaled(TextSize::Small, text(s))));
    let mut cells = alloc::vec::Vec::with_capacity(columns.len() + 2);
    // The same lead-in the rows have, so `Name` starts where a name does.
    if swatched {
        cells.push(sized(Size::new(SWATCH_W + SWATCH_GAP, 0), text("")));
    }
    cells.push(heading(primary).flex(1));
    for (i, c) in columns.iter().enumerate() {
        let label = heading(headings.get(i).copied().unwrap_or(""));
        cells.push(sized(
            Size::new(c.width, 0),
            match c.align {
                ColumnAlign::Left => label,
                ColumnAlign::Right => {
                    row(alloc::vec![text("").flex(1), label, sized(Size::new(CELL_PAD, 0), text(""))])
                }
            },
        ));
    }
    sized(
        Size::new(0, LIST_HEADER_H),
        stack(alloc::vec![
            fill(theme.face_hover),
            dock(alloc::vec![docked(Edge::Bottom, sized(Size::new(0, 1), fill(theme.border)))], text(""),),
            padding(
                // **The scrollbar's width, when there is one** (PR #320 review, worth fixing 5): a
                // list that overflows puts its rows beside a scrollbar and they lose that width,
                // while this spans the whole strip — so every column sat twelve pixels left of its
                // heading in any directory long enough to scroll, which is most of them.
                Insets { right: ROW_PAD.right + if scrolling { SCROLLBAR_W } else { 0 }, ..ROW_PAD },
                row(cells),
            ),
        ]),
    )
}

/// Whether a list of `rows` at `row_height` overflows `height` — and so draws a scrollbar.
///
/// **The rule `list_view` uses, published**, because a caller drawing a header above one needs
/// the same answer and computing it twice is how the two come to disagree.
pub fn list_scrolls(rows: usize, height: u32, row_height: u32) -> bool {
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
    rows > visible
}
