//! The list view: rows, columns, row buttons and swatches.

use super::*;

/// One row of a [`list_view`], borrowed and built fresh each frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ListRow<'a> {
    /// Identity across frames.
    ///
    /// Not the index, which the diff would pair by position; use the window id or the index into
    /// the *unfiltered* list. Must be unique in one list, or the diff answers
    /// [`DiffError::DuplicateKey`](crate::diff::DiffError::DuplicateKey).
    pub key: u64,
    /// What the row says.
    pub label: &'a str,
    /// Part of a **multiple** selection, drawn as `selected` is; a property of the row, since
    /// `ListState` is `Copy`.
    pub marked: bool,
    /// The trailing cells, one per column; fewer than columns leaves the rest blank, more are
    /// ignored.
    pub cells: &'a [&'a str],
    /// A small shape before the label — the browser's folder-or-file mark, or a place's dot.
    pub swatch: Option<Swatch>,
    /// A control at the row's trailing end, such as a drive's eject, or `None`.
    pub button: Option<RowButton<'a>>,
}

/// A control at a row's trailing end that acts on the row's subject, as a tab's `×` does.
///
/// Pressed through the list's `activate` with a key distinct from every row's; a press on it is
/// not a grab of the row, a nearer `on_press` shadowing `on_press_down`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RowButton<'a> {
    /// Its key: what `activate` is handed when it is pressed, and what `hovered` names over it.
    pub key: u64,
    /// What it shows: one glyph, drawn dim and in full ink under the pointer.
    pub glyph: &'a str,
}

/// How wide a row's button is, in pixels.
pub const ROW_BUTTON_W: u32 = 24;

const ROW_BUTTON_HOVER: u32 = TAB_CLOSE_HOVER + 2;

/// The shape drawn before a row's label: two named shapes, so a caller cannot invent a third.
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
/// Fixed widths, and the label takes the rest, so columns do not shift as a listing changes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ListColumn {
    /// How wide, in pixels.
    pub width: u32,
    /// Whether the text sits against the column's left edge or its right; a size reads right.
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

/// Which row is selected, and how far the list is scrolled, in rows rather than pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ListState {
    /// The selected row, as an index into the rows passed to [`list_view`].
    pub selected: Option<usize>,
    /// The first visible row.
    pub offset: usize,
    /// What the last [`ensure_visible`](Self::ensure_visible) was for: the selected row, and how
    /// many rows fitted.
    ///
    /// Without it each build would snap a list whose selection is row 0 back to offset 0,
    /// discarding a scrollbar drag. The row count is included so a shrunken window re-follows.
    pub(super) followed: Option<(usize, usize)>,
}

impl ListState {
    /// A list showing `offset` first, with `selected` picked (`followed` is private).
    pub fn at(selected: Option<usize>, offset: usize) -> Self {
        Self { selected, offset, followed: None }
    }

    /// Move the selection down one row, answering whether anything changed; from nothing it
    /// selects the first row.
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

    /// Scroll so the selected row is on screen, given how many rows fit; [`list_view`] calls it.
    pub fn ensure_visible(&mut self, visible: usize) {
        let Some(i) = self.selected else {
            return;
        };
        if visible == 0 {
            return;
        }
        // Once per selection, not once per build: doing it every build makes every other way of
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

    /// Apply a key, answering whether the list changed; Enter and Escape are declined.
    ///
    /// A caller routes keys itself rather than through `on_key`, since arrow keys must reach the
    /// list while a launcher's query field holds focus.
    pub fn apply(&mut self, keycode: u16, len: usize) -> bool {
        match keycode {
            KEY_DOWN => self.down(len),
            KEY_UP => self.up(),
            _ => false,
        }
    }
}

/// How much space a row's label gets around it; public so a heading over the rows can start
/// where they do.
pub const ROW_PAD: Insets = Insets { top: 2, right: 6, bottom: 2, left: 6 };

impl ListState {
    /// Scroll by a turn of the wheel, `dz` detents, positive **down**.
    ///
    /// No upper clamp: [`list_view`] clamps against its rows every build, and two clamps could
    /// disagree. `dz` is widened before multiplying because it saturates at the compositor.
    pub fn wheel(&mut self, dz: i16) {
        let rows = i64::from(dz) * i64::from(crate::click::WHEEL_UNITS);
        self.offset = if rows < 0 {
            self.offset.saturating_sub(rows.unsigned_abs() as usize)
        } else {
            self.offset.saturating_add(rows as usize)
        };
    }

    /// This list as a scrollbar's state, from the geometry [`list_view`] was given; any other
    /// puts the thumb where the pointer is not.
    pub fn bar(&self, height: u32, row_height: u32, total: usize) -> ScrollState {
        let visible = height.checked_div(row_height).unwrap_or(0);
        ScrollState { offset: self.offset as u32, visible, total: total as u32 }
    }
}

/// A scrolling list of rows, with one selected.
///
/// Only the visible rows become elements. `height` must be the height the parent will give it,
/// or the list shows a different number of rows than it built: wrap the result in `sized`, as
/// [`crate::reference`] does. `ground` is the colour behind the rows, or `None` for the
/// ordinary one; a sidebar passes one to read apart from its content.
///
/// `state` is taken by `&mut` and scrolled in place to follow the selection, see
/// [`ensure_visible`](ListState::ensure_visible): returned by value, nothing made a caller keep it.
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
    // The selection is clamped first: it indexes a list that may have just been replaced, and a
    // stale index paints nothing and leaves `down` dead. Clamped, not cleared.
    if let Some(i) = state.selected
        && i >= rows.len()
    {
        state.selected = rows.len().checked_sub(1);
    }
    state.ensure_visible(visible);
    let max_offset = rows.len().saturating_sub(visible);
    state.offset = state.offset.min(max_offset);

    // The surface the rows sit on; a highlight is an accent wash over it.
    let ground = ground.unwrap_or(theme.track);
    let hover = theme.scheme.hover_coverage();
    let last = (state.offset + visible).min(rows.len());
    let mut items = alloc::vec::Vec::with_capacity(last.saturating_sub(state.offset));
    for (i, r) in rows.iter().enumerate().take(last).skip(state.offset) {
        let selected = state.selected == Some(i) || r.marked;
        // A selection is the accent washed over the ground, flat. Hover is quieter and loses to
        // it, unless nothing is selected: then it is the one primary highlight. Over a row's
        // button is over the row (see [`RowButton`]).
        let over = hovered == Some(r.key) || r.button.is_some_and(|b| hovered == Some(b.key));
        let primary = selected || (over && state.selected.is_none());
        let mut layers = alloc::vec![fill(ground)];
        if primary {
            layers.push(wash(theme.accent, libdraw::theme::SELECTION_COVERAGE));
        } else if over {
            layers.push(wash(theme.accent, hover));
        } else if r.button.is_some() {
            // A row with a button keeps its layers in place, lit or not. The router finds the
            // widget a press captured by its id, and an id survives a repaint only while its
            // siblings keep their positions: a wash inserted here when the press lit the row
            // lost the release (`check-media` opened the drive instead of ejecting it).
            layers.push(text(""));
        }
        // A plain row stays one padded label with no row wrapper, which would cost a node per
        // row. With columns, the label takes the space they leave.
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
                    // Pushed rather than aligned: a `Row` lays out left to right.
                    ColumnAlign::Right => {
                        row(alloc::vec![text("").flex(1), text_el, sized(Size::new(CELL_PAD, 0), text(""))])
                    }
                },
            ));
        }
        let body = padding(ROW_PAD, row(cells));
        match r.button {
            None => layers.push(body),
            // Both halves keyed, since a container's children are all keyed or none: the body
            // takes the row's own key so `hovered` over it still names the row.
            Some(b) => {
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
        // A press down on a row, for a caller that needs the gesture (a drag); `on_press` on the
        // same element does not shadow it, as shadowing compares depth.
        if let Some(f) = grab {
            item = item.on_press_down(f(r.key));
        }
        items.push(item);
    }

    let list = column(items);
    let body = if rows.len() > visible {
        let bar = ScrollState { offset: state.offset as u32, visible: visible as u32, total: rows.len() as u32 };
        // The bar takes the pointer when the caller has somewhere to send it.
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

const CELL_PAD: u32 = 6;

/// How tall a list's column header is: the design's 25.
pub const LIST_HEADER_H: u32 = 25;

/// The header over a list's columns: the primary heading, then one per column.
///
/// Here because the insets that put a heading over its column are this module's.
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
                // The scrollbar's width when there is one: the rows lose it and this strip does
                // not, so columns would otherwise sit left of their headings.
                Insets { right: ROW_PAD.right + if scrolling { SCROLLBAR_W } else { 0 }, ..ROW_PAD },
                row(cells),
            ),
        ]),
    )
}

/// Whether a list of `rows` at `row_height` overflows `height` and so draws a scrollbar.
pub fn list_scrolls(rows: usize, height: u32, row_height: u32) -> bool {
    let visible = height.checked_div(row_height).unwrap_or(0) as usize;
    rows > visible
}
