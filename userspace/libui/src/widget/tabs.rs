use super::*;

/// How tall a tab strip is: the design's 30 since the refresh's Part H, where it was 24.
///
/// **A fixed metric, like every other piece of chrome here** (M11's decision 2): the gates aim at
/// tabs, and one whose height followed the theme's text size would move under a gate that had to
/// read a theme file to know where to click.
pub const TAB_STRIP_H: u32 = 30;

/// How tall one tab is, inside that strip: the design's 24.
pub const TAB_H: u32 = 24;

/// The strip's padding above its tabs — the design's 5, which is what leaves a tab sitting on the
/// strip rather than filling it.
pub const TAB_TOP: u32 = 5;

/// The strip's padding at its left end, before the first tab: the design's 6.
pub const TAB_SIDE: u32 = 6;

/// Between one tab and the next: the design's 1. Enough to separate two inactive tabs, which
/// have no face of their own to separate them.
pub const TAB_GAP: u32 = 1;

/// From one tab's left edge to the next's — what a gate multiplies to find tab `n`.
pub const TAB_PITCH: u32 = TAB_W + TAB_GAP;

/// The corner a tab is rounded to, at the top only: a tab is a shape that runs into the content
/// below it, so the bottom corners are square and the bottom edge is not there at all.
pub const TAB_RADIUS: u32 = CONTROL_RADIUS;

/// The `+` at the end of the strip: the design's 22×24.
pub const NEW_TAB_W: u32 = 22;

/// How wide one tab is.
///
/// **Fixed rather than shared out**, which is the decision worth stating. Tabs that divided the
/// strip between them would move every one of them whenever another opened — so the tab a person
/// is reaching for slides away as they reach, and a gate's aim point depends on how many tabs
/// happen to be open. Fixed width means a tab is where it was, and the cost is that enough of
/// them run off the end: `TODO(tab-overflow)` names the scrolling that would fix it, and the
/// trigger is somebody opening more than a window's width of them.
pub const TAB_W: u32 = 120;

/// A tab's box inside the strip: the tab itself, plus the rule it covers when it is the current
/// one.
const TAB_BOX_H: u32 = TAB_H + TAB_RULE;

/// The rule along the bottom of the strip.
const TAB_RULE: u32 = 1;

/// A tab label's inset from the tab's left edge: the design's 9.
pub(super) const TAB_LABEL_PAD: u32 = 9;

/// Between a tab's label and its close box: the design's 8.
const TAB_CLOSE_GAP: u32 = 8;

/// The key the `+` carries, so the strip's children are all keyed as the diff requires.
pub(super) const NEW_TAB_KEY: u64 = u64::MAX;

/// The current tab's outline: the design's 1.
const TAB_OUTLINE: u32 = 1;

/// The side of the face behind a hovered tab's `×` — smaller than the 20-wide close box, so the
/// highlight is a square round the glyph rather than a column down the tab.
pub(super) const TAB_CLOSE_HOVER: u32 = 16;

/// The bit a tab's close box sets in its tab's key to have a key of its own.
///
/// **A caller's tab keys must leave it clear.** Every application numbers its tabs from
/// `1 << 62` or `1 << 63` with a small offset, which never reaches bit 60, and each of the three
/// has a test saying so against this constant rather than against a copy of it.
pub const TAB_CLOSE_BIT: u64 = 1 << 60;

/// The key the close box of the tab keyed `tab` carries — see [`TAB_CLOSE_BIT`].
///
/// Public so a caller's `hovered` can be compared against it, and so a caller can test that its
/// tab keys leave room for it.
pub const fn tab_close_key(tab: u64) -> u64 {
    tab | TAB_CLOSE_BIT
}

/// How wide the close box at a tab's right end is.
pub const TAB_CLOSE_W: u32 = 20;

/// The centre of a tab's close box, measured from that tab's left edge.
///
/// Published for the same reason [`DIALOG_LEFT_CX`] is: `check-login` presses it from a copy of
/// its own, because a gate aims from its own numbers (M11 decision 2).
pub const TAB_CLOSE_CX: i32 = (TAB_W - TAB_CLOSE_W / 2) as i32;

/// One tab: what it is called, whether it is marked, and what identifies it across frames.
pub struct Tab<'a> {
    /// Identity across frames — see [`ListRow::key`], whose reasoning is the same one.
    ///
    /// **Not the index**, because closing a tab renumbers every one after it and the diff would
    /// pair each surviving tab's widget with its neighbour's element.
    ///
    /// **Must leave [`TAB_CLOSE_BIT`] clear.** The tab's `×` is keyed as this with that bit set,
    /// so a key that already has it collides with its own close box — `Tree::update` answers
    /// `DuplicateKey` and the window draws nothing (PR #325 review, optional 4).
    pub key: u64,
    /// What it is called.
    pub label: &'a str,
    /// Shown with a leading mark — an editor's unsaved buffer.
    pub marked: bool,
}

/// A row of tabs, one of them current.
///
/// **`select` and `close` both take the key**, not the index, for the reason the key exists: the
/// message outlives the frame that produced it, and by the time an application acts on it the
/// tab it names may have moved.
///
/// A press on the close box does *not* select the tab, because a nearer `on_press` shadows the
/// one on the tab — the same rule that lets a title bar carry buttons without dragging the
/// window. That rule is the toolkit's rather than this widget's.
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
        // **The current tab is the window's own ground and the others are the strip's**, which is
        // what makes a row of boxes read as tabs rather than as buttons: the current one is a
        // continuation of the content below, and the rest are a strip above it. An inactive tab
        // has no face at all — the strip's is what shows through — so hover is a wash over it,
        // the treatment every other momentary highlight in this toolkit gets.
        let close_key = tab_close_key(t.key);
        // **Over its `×` is still over the tab.** The close box carries a key of its own, so
        // `hovered` names it rather than the tab there — and a tab whose highlight went out as the
        // pointer reached its `×` would flicker at exactly the moment a person is aiming.
        let over_tab = hovered == Some(t.key) || hovered == Some(close_key);
        let mut layers = alloc::vec::Vec::with_capacity(4);
        if current {
            // **Outlined in `border` on three sides and open on the fourth** (desktop refresh,
            // after Part K) — the design's `border: 1px solid var(--line)` with its bottom edge in
            // the ground colour. Without it the current tab was the window's white on a strip one
            // shade off white, and the one tab a person most needs to find was the hardest to see.
            //
            // Two faces rather than a stroked outline, because the toolkit's `Outline` is square
            // and this one must follow the rounded top: the tab's shape in `border`, then the
            // same shape in the ground **inset on three sides and not the fourth**. The inner
            // curve is a pixel tighter than the outer, as a CSS border's is, so the line stays a
            // pixel wide round the corner. Down the sides the outline runs through the strip's
            // rule, and across the bottom the ground covers it — which is what "open into the
            // content" is.
            layers.extend(tab_face(theme.border, TAB_RADIUS));
            layers.push(padding(
                Insets { top: TAB_OUTLINE, right: TAB_OUTLINE, bottom: 0, left: TAB_OUTLINE },
                stack(tab_face(theme.background, TAB_RADIUS.saturating_sub(TAB_OUTLINE))),
            ));
        } else if over_tab {
            // **A face rather than a wash**, which every other momentary highlight here is: a
            // wash cannot be rounded, and a square highlight behind a rounded tab reads as a
            // second shape. One shade off the strip is enough to say "this one".
            //
            // **Stopping above the rule**, which the current tab covers and no other tab may:
            // running the hover face down to the strip's bottom made an inactive tab read as
            // open at the bottom for as long as the pointer was over it — the one cue that is
            // supposed to mark the current tab alone (PR #319 review, optional 7).
            layers.push(padding(
                Insets { top: 0, right: 0, bottom: TAB_RULE, left: 0 },
                stack(tab_face(theme.face, TAB_RADIUS)),
            ));
        } else {
            // **The face's slot, held when unlit** (Phase 6 Part F): a press on the `×` lights the
            // tab, and a face inserted before the label and `×` moved them along one, so a release
            // after that repaint found nothing it had captured and the close was lost.
            // `a_tabs_close_box_keeps_its_click_across_a_repaint_that_lights_it`; the list row's
            // button had the same bug first.
            layers.push(text(""));
        }
        let ink_colour = if current { theme.foreground } else { theme.foreground_dim };
        // **The close box lights on hover** (desktop refresh, after Part K): a small face in
        // `face_pressed` behind the `×` and the `×` in full ink — the treatment the title bar's
        // minimise and maximise get, **not** close's red. Closing a tab loses one view of
        // something; closing a window ends a program. The design gives the tab's `×` no hover
        // at all, so this is ours, and the neutral one is the convention every browser keeps.
        let mut close_layers = alloc::vec::Vec::with_capacity(2);
        let close_ink = if hovered == Some(close_key) {
            // `sized` gives the face a size to be centred at; a bare fill measures as nothing.
            close_layers.push(center(sized(
                Size::new(TAB_CLOSE_HOVER, TAB_CLOSE_HOVER),
                rounded_fill(theme.face_pressed, TITLE_BUTTON_RADIUS),
            )));
            theme.foreground
        } else {
            theme.foreground_dim
        };
        // **The icon fills the box rather than being centred in it**: `Node::Icon` measures as
        // nothing and draws into the rect it is given, so a `center` around one hands it a zero
        // rect and the tab loses its close box while still clicking like one.
        close_layers.push(ink(close_ink, icon(IconKind::Close)));
        layers.push(row(alloc::vec![
            // **The label carries its tab's own key.** The close box needs a key of its own for
            // hover to tell it apart from the tab, and the diff wants a container's children all
            // keyed or none. Giving the label the *tab's* key keeps "the pointer is over this tab"
            // meaning exactly what it did — `hovered_key` walks to the nearest keyed node, which
            // is the label — while `locate` and `find_by_key` walk outside-in and still find the
            // whole tab first. A key repeated at a different depth is legal (the diff forbids
            // duplicates only among siblings); both of those properties are pinned by tests.
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
    // **The `+` is the last thing in the row**, where the design puts it: after the tabs rather
    // than at the strip's right end, so it stays beside the tab that was opened last.
    if let Some(new_tab) = extras.new_tab {
        row_items.push(
            sized(
                Size::new(NEW_TAB_W, TAB_BOX_H),
                // **Full ink on hover**, the design's `style-hover="color:var(--fg)"` — the one
                // tab-strip control it lights that had nothing (PR #325 review, optional 5). No
                // face: the design gives the `+` none, and it reads as a glyph beside the tabs
                // rather than as a button among them.
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
    // The strip's own face, and the rule that separates it from what is below. The current tab
    // is drawn over that rule, which is what "its border is open at the bottom" means here.
    strip.push(fill(theme.face_hover));
    strip.push(dock(alloc::vec![docked(Edge::Bottom, sized(Size::new(0, TAB_RULE), fill(theme.border)))], text("")));
    let tabs_row = with_spacing(row(row_items), TAB_GAP);
    strip.push(padding(
        Insets { top: TAB_TOP, right: TAB_SIDE, bottom: 0, left: TAB_SIDE },
        match extras.right {
            // A right-hand slot for a window's own controls — the editor's byte count and its
            // Save. `dock` gives the tabs the rest, so the slot is the size it asks for.
            //
            // **Both sides wrapped**, as `status_bar` wraps its slots and for the same reason:
            // the tabs beside it are keyed, so a caller that keys its own control — and Part J's
            // Save must, for its hover — made this dock `MixedKeying` and the window then drew
            // nothing at all (PR #319 review, worth fixing 6).
            Some(right) => dock(
                alloc::vec![docked(Edge::Right, padding(Insets::all(0), right))],
                padding(Insets::all(0), tabs_row),
            ),
            None => tabs_row,
        },
    ));
    sized(Size::new(0, TAB_STRIP_H), stack(strip))
}

/// What a tab strip carries besides its tabs (desktop refresh, Part H).
///
/// **A struct rather than two more arguments**, because both are optional and an application
/// that wants neither says so once: `TabExtras::none()`.
pub struct TabExtras<Msg> {
    /// What the `+` at the end of the tabs sends. `None` draws no `+`.
    pub new_tab: Option<Msg>,
    /// A window's own controls, at the strip's right end — the editor's byte count and Save.
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

/// A tab's face: rounded at the top, square where it meets the content.
///
/// **Two layers rather than a per-corner radius**, which the toolkit does not have: a rounded
/// fill over the whole box, then a square one below the curve. The square layer is what covers
/// the strip's rule under the current tab.
fn tab_face<Msg>(colour: Rgb, radius: u32) -> alloc::vec::Vec<Element<Msg>> {
    alloc::vec![
        rounded_fill(colour, radius),
        padding(Insets { top: radius, right: 0, bottom: 0, left: 0 }, fill(colour)),
    ]
}
