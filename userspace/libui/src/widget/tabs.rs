//! The tab strip.

use super::*;

/// How tall a tab strip is: the design's 30, fixed because the gates aim at tabs.
pub const TAB_STRIP_H: u32 = 30;

/// How tall one tab is, inside that strip: the design's 24.
pub const TAB_H: u32 = 24;

/// The strip's padding above its tabs: the design's 5, so a tab sits on the strip.
pub const TAB_TOP: u32 = 5;

/// The strip's padding at its left end, before the first tab: the design's 6.
pub const TAB_SIDE: u32 = 6;

/// Between one tab and the next: the design's 1, which separates two faceless inactive tabs.
pub const TAB_GAP: u32 = 1;

/// From one tab's left edge to the next's — what a gate multiplies to find tab `n`.
pub const TAB_PITCH: u32 = TAB_W + TAB_GAP;

/// The corner a tab is rounded to, at the top only; it runs into the content below.
pub const TAB_RADIUS: u32 = CONTROL_RADIUS;

/// The `+` at the end of the strip: the design's 22×24.
pub const NEW_TAB_W: u32 = 22;

/// How wide one tab is.
///
/// Fixed rather than shared out, or every tab would move whenever another opened. Enough tabs
/// run off the end: `TODO(tab-overflow)` names the scrolling that would fix it.
pub const TAB_W: u32 = 120;

/// A tab's box inside the strip: the tab, plus the rule it covers when it is current.
const TAB_BOX_H: u32 = TAB_H + TAB_RULE;

const TAB_RULE: u32 = 1;

/// A tab label's inset from the tab's left edge: the design's 9.
pub(super) const TAB_LABEL_PAD: u32 = 9;

/// Between a tab's label and its close box: the design's 8.
const TAB_CLOSE_GAP: u32 = 8;

/// The key the `+` carries, so the strip's children are all keyed as the diff requires.
pub(super) const NEW_TAB_KEY: u64 = u64::MAX;

const TAB_OUTLINE: u32 = 1;

/// The side of the face behind a hovered tab's `×`, smaller than the close box.
pub(super) const TAB_CLOSE_HOVER: u32 = 16;

/// The bit a tab's close box sets in its tab's key to have a key of its own.
///
/// A caller's tab keys must leave it clear; applications number tabs from `1 << 62` or `1 << 63`.
pub const TAB_CLOSE_BIT: u64 = 1 << 60;

/// The key the close box of the tab keyed `tab` carries, see [`TAB_CLOSE_BIT`].
pub const fn tab_close_key(tab: u64) -> u64 {
    tab | TAB_CLOSE_BIT
}

/// How wide the close box at a tab's right end is.
pub const TAB_CLOSE_W: u32 = 20;

/// The centre of a tab's close box from the tab's left edge; `check-login` aims from a copy.
pub const TAB_CLOSE_CX: i32 = (TAB_W - TAB_CLOSE_W / 2) as i32;

/// One tab: what it is called, whether it is marked, and what identifies it across frames.
pub struct Tab<'a> {
    /// Identity across frames, see [`ListRow::key`]; not the index, as closing renumbers.
    ///
    /// Must leave [`TAB_CLOSE_BIT`] clear: the tab's `×` is keyed with that bit set, and a clash
    /// is a `DuplicateKey`.
    pub key: u64,
    /// What it is called.
    pub label: &'a str,
    /// Shown with a leading mark — an editor's unsaved buffer.
    pub marked: bool,
}

/// A row of tabs, one of them current.
///
/// `select` and `close` take the key, not the index, as the message outlives the frame. A press
/// on the close box does not select the tab: a nearer `on_press` shadows the tab's.
pub fn tab_strip<Msg: Clone>(
    tabs: &[Tab<'_>],
    current: u64,
    hovered: Option<u64>,
    select: impl Fn(u64) -> Msg,
    close: impl Fn(u64) -> Msg,
    extras: TabExtras<Msg>,
    theme: &Theme,
) -> Element<Msg> {
    let mut row_items = alloc::vec::Vec::with_capacity(tabs.len() + 1);
    for t in tabs {
        let current = t.key == current;
        let mut label = String::new();
        if t.marked {
            label.push_str("* ");
        }
        label.push_str(t.label);
        // The current tab is the window's own ground and the others are the strip's; an
        // inactive tab has no face, so hover is a wash over it.
        let close_key = tab_close_key(t.key);
        // Over its `×` is still over the tab, or the highlight would flicker as the pointer
        // reaches it.
        let over_tab = hovered == Some(t.key) || hovered == Some(close_key);
        let mut layers = alloc::vec::Vec::with_capacity(4);
        if current {
            // Outlined in `border` on three sides and open on the fourth. `Outline` is square, so
            // it is two faces: the tab's shape in `border`, then the same shape in the ground
            // inset on three sides. The inner curve is a pixel tighter, so the line stays a
            // pixel wide round the corner, and the ground covers the strip's rule below.
            layers.extend(tab_face(theme.border, TAB_RADIUS));
            layers.push(padding(
                Insets { top: TAB_OUTLINE, right: TAB_OUTLINE, bottom: 0, left: TAB_OUTLINE },
                stack(tab_face(theme.background, TAB_RADIUS.saturating_sub(TAB_OUTLINE))),
            ));
        } else if over_tab {
            // A face rather than a wash, which cannot be rounded. It stops above the rule,
            // which only the current tab may cover: an inactive tab must not read as open.
            layers.push(padding(
                Insets { top: 0, right: 0, bottom: TAB_RULE, left: 0 },
                stack(tab_face(theme.face, TAB_RADIUS)),
            ));
        } else {
            // The face's slot, held when unlit: a face inserted before the label and `×` when a
            // press lit the tab moved them along one, and the release lost the close
            // (`a_tabs_close_box_keeps_its_click_across_a_repaint_that_lights_it`).
            layers.push(text(""));
        }
        let ink_colour = if current { theme.foreground } else { theme.foreground_dim };
        // The close box lights on hover: a small `face_pressed` face and the `×` in full ink,
        // not close's red, as closing a tab ends no program.
        let mut close_layers = alloc::vec::Vec::with_capacity(2);
        let close_ink = if hovered == Some(close_key) {
            close_layers.push(center(sized(
                Size::new(TAB_CLOSE_HOVER, TAB_CLOSE_HOVER),
                rounded_fill(theme.face_pressed, TITLE_BUTTON_RADIUS),
            )));
            theme.foreground
        } else {
            theme.foreground_dim
        };
        // The icon fills the box rather than being centred: `Node::Icon` measures as nothing, so
        // a `center` would hand it a zero rect.
        close_layers.push(ink(close_ink, icon(IconKind::Close)));
        layers.push(row(alloc::vec![
            // The label carries its tab's own key: the close box needs one of its own, and
            // children are all keyed or none. `hovered_key` finds the label, while `locate` and
            // `find_by_key` walk outside-in and find the whole tab; tests pin both.
            padding(
                Insets { top: 0, right: TAB_CLOSE_GAP, bottom: 0, left: TAB_LABEL_PAD },
                center_v(ink(ink_colour, text(label))),
            )
            .flex(1)
            .key(t.key),
            sized(Size::new(TAB_CLOSE_W, TAB_BOX_H), stack(close_layers).on_press(close(t.key)),).key(close_key),
        ]));
        row_items.push(sized(Size::new(TAB_W, TAB_BOX_H), stack(layers).on_press(select(t.key))).key(t.key));
    }
    if let Some(new_tab) = extras.new_tab {
        row_items.push(
            sized(
                Size::new(NEW_TAB_W, TAB_BOX_H),
                stack(alloc::vec![center(ink(
                    if hovered == Some(NEW_TAB_KEY) { theme.foreground } else { theme.foreground_dim },
                    text("+"),
                ))])
                .on_press(new_tab),
            )
            .key(NEW_TAB_KEY),
        );
    }
    let mut strip = alloc::vec::Vec::with_capacity(3);
    strip.push(fill(theme.face_hover));
    strip.push(dock(alloc::vec![docked(Edge::Bottom, sized(Size::new(0, TAB_RULE), fill(theme.border)))], text("")));
    let tabs_row = with_spacing(row(row_items), TAB_GAP);
    strip.push(padding(
        Insets { top: TAB_TOP, right: TAB_SIDE, bottom: 0, left: TAB_SIDE },
        match extras.right {
            // A right-hand slot for a window's own controls, wrapped as in `status_bar`: a keyed
            // control beside keyed tabs would make this dock `MixedKeying`.
            Some(right) => dock(
                alloc::vec![docked(Edge::Right, padding(Insets::all(0), right))],
                padding(Insets::all(0), tabs_row),
            ),
            None => tabs_row,
        },
    ));
    sized(Size::new(0, TAB_STRIP_H), stack(strip))
}

/// What a tab strip carries besides its tabs; a struct as both are optional.
pub struct TabExtras<Msg> {
    /// What the `+` at the end of the tabs sends. `None` draws no `+`.
    pub new_tab: Option<Msg>,
    /// A window's own controls, at the strip's right end.
    pub right: Option<Element<Msg>>,
}

impl<Msg> TabExtras<Msg> {
    /// Tabs and nothing else.
    pub fn none() -> Self {
        Self { new_tab: None, right: None }
    }

    /// Tabs and a `+`.
    pub fn new_tab(msg: Msg) -> Self {
        Self { new_tab: Some(msg), right: None }
    }
}

/// A tab's face: rounded at the top, square below, which covers the strip's rule when current.
fn tab_face<Msg>(colour: Rgb, radius: u32) -> alloc::vec::Vec<Element<Msg>> {
    alloc::vec![
        rounded_fill(colour, radius),
        padding(Insets { top: radius, right: 0, bottom: 0, left: 0 }, fill(colour)),
    ]
}
