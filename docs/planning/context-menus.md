# Context menus

**Status: detailed 2026-11-16.** Part F of [`pre-phase-7.md`](pre-phase-7.md): a menu on the right
button, in Files, the editor and the terminal.

## Why

Nothing in the desktop answers the right button on purpose. Every action a context menu would
offer already exists, on a menu bar or a chord, but a person has to know which word on which bar
it is under. A context menu puts what can be done to a thing at the thing.

## Part F in detail

*Detailed 2026-11-16, measured on `main` at PR #444's merge (`11a08e2d`), the maintainer's calls
folded in.* Where the outline in `pre-phase-7.md` and this differ, this is the plan. **Five
sub-parts, a PR each.**

### What the pass found

1. **A right click is a left click today.** The router opens a capture on whichever button goes
   down first, and fires that widget's `on_press` on *that* button's release and its
   `on_press_down` on its press (`libui/src/route.rs`). So:
   - a right click on a title bar's close button closes the window;
   - a right press on a title bar starts a move;
   - a right click on a tab, a list row or a menu word does what a left click does.

   A probe test, a right press and release over an `on_press`, got the message. The router's own
   test, `only_the_capturing_buttons_release_is_a_click`, pins "the capturing button" without
   saying which. **The shell's middle-click-to-close depends on it**: the bottom bar routes the
   event, then asks the event whether its button was the middle one (`desktop-shell/src/main.rs`).
2. **The editor drops its selection on any press.** `take_area_pointer` places the caret on a
   press of any button, so a right click on a selection would lose what Copy acts on. The terminal's
   grid and both click counters (Files' and the terminal's) already read only `BTN_LEFT`.
3. **A client cannot keep a menu on screen.** It knows its window's origin, from the last
   `Configure`, and the screen's size, from `/dev/draw/screen`. It does not know the bars'
   reservations, and the compositor places a popup exactly where asked:
   [`rsproto-surface-ops.md`](../spec/rsproto-surface-ops.md) says *the offset is not clamped*. The
   shell's power menu is right-aligned by hand, because only the shell knows the screen. A menu
   opened at the cursor near the right or bottom edge would open partly off the screen.
4. **Each application carries its own copy of a menu's window.** `nxterm`, `nxedit` and `nxfiles`
   each do the same work in two places in their main loop, 80 to 125 lines each:
   - open, switch, redraw and close the drop-down's popup;
   - track its hover;
   - route its pointer, its keys and its `Dismissed`.

   A context menu would make a second copy in each. The shell's `BarMenus` is a fourth, with a
   filter field and a size that changes as you type.
5. **Every file Part F would grow is at its size entry**:

   | Crate | Files at their entry |
   |---|---|
   | `libui` | `route.rs` 1,235 |
   | `nxfiles` | `app.rs` 1,719, `main.rs` 1,095 |
   | `nxedit` | `lib.rs` 3,811 |
   | `nxterm` | `lib.rs` 2,339 |
   | `compositor` | `lib.rs` 2,643, `input.rs` 2,636, `main.rs` 1,926 |
   | `desktop-shell` | `main.rs` 1,392 |

   New code goes into new modules, and a shared helper is what lets the main loops shrink.
6. **A press that dismisses a menu also arrives, and nothing promises the order.** The compositor
   queues the `Dismissed` while routing the batch (`route_one_batch`), and delivers the press after
   every batch (`deliver`). So today the `Dismissed` arrives first, but the spec says nothing about
   the order. A loop written for the other order would close a menu that a second right-click meant
   to move.
7. **Both keyboards already send the Menu key**: PS/2 scancode `0x5D` and USB usage `0x65`, as
   `KEY_COMPOSE`. Nothing reads it.
8. **The graphical prompt plans the same placement**
   ([`graphical-prompt.md`](../design/graphical-prompt.md), step 1): *an application's popup is kept
   inside the work area… a menu near an edge moves inward.* It also needs the compositor to know
   which connection is the manager's, so that the rule can be required of applications.

### The maintainer's calls (2026-11-16)

- **The compositor places a menu**, when the menu asks it to. Today's popups are unchanged.
- **A menu opens on the right press**, as GNOME and macOS open theirs. The compositor holds a
  press's grab until the release, so the release goes to the window that took the press. It cannot
  choose a row in a menu that opened under it.
- **No desktop menu yet.** What it would hold, wallpaper settings and the desktop's own context (a
  new file or folder, a terminal opened there), does not exist yet. `TODO(desktop-menu)`.
- **No window menus.** A title bar or a taskbar button gets no menu in Part F. `TODO(window-menu)`.

### The order

**F.1 and F.2 need nothing else. F.3 needs F.2**: its host asks for F.2's placement. **F.4 and
F.5 need F.1 and F.3**, and neither needs the other.

### How each is held

Every sub-part gets a `/pr-review` and the host tests. Each runs the KVM gates its files reach:
F.1 to F.3 reach every window, so they run all of them. Each new test fails without its change,
which is its control.

**Miri runs once, at Part F's end**, by the maintainer's rule for a part's end (2026-11-06). It runs
the userspace targets: Part F changes no kernel code, and the kernel's tests ran clean under Miri
at Part E's end. Part F is done when F.5 has merged and what Miri reports is triaged.

### F.1 A click is the left button's

**The router gives each button its own meaning.**
- `on_press` and `on_press_down` answer `BTN_LEFT` only.
- A right press goes to a new `on_context: Option<Msg>`, the nearest one on the hit's path. It fires
  on the press, once, and after any `on_pointer` on the same path. So a widget that places a caret
  has placed it before the menu reads the selection.
- A middle click, pressed and released inside, goes to a new `on_middle`.
- A capture still opens on whichever button goes down first. So a release outside, or a drag,
  still reaches the widget that took it, and `ScrollGrab` keeps answering its own button.
- Click-to-focus is unchanged for every button.

**The shell's bottom bar moves its middle-click to `on_middle`**, and stops asking the event which
button it was.

**`route.rs`'s tests move first**, to `route/tests.rs`, in a commit of their own that lowers the
file's entry. That commit can be the implementer's.

**Two deferrals are written here**, each entry with its marker:
- `TODO(desktop-menu)` at the wallpaper's surface. Trigger: wallpaper settings, or a desktop that
  is a folder.
- `TODO(window-menu)` at `title_bar`. Trigger: an action a window's menu would offer that its title
  bar and the bottom bar do not.

**Tests, each failing on today's router:**
- A right click produces no message on a button, a title bar's close, a tab or a list row.
- A right press on a title bar starts no move.
- `on_context` fires on the press, the nearest one, once, and not on the release.
- A right press on a widget without `on_context`, inside one with it, reaches the outer one.
- A middle click on a task reaches `on_middle`.
- The scrollbar's second-button tests still pass, unchanged.

Docs: [`widget-toolkit.md`](../architecture/widget-toolkit.md) §7.1 and §8.2. Every gate, as every
window routes through it. Author.

### F.2 The compositor places a menu

**A popup's role aux16 word says how to place it.** The spec reserves that word for exactly this:
*deliberate room for a future field*. Its values:
- **`0`, as asked**: today's rule, so every popup that exists keeps its place.
- **`1`, inside**: moved inward the least it takes to sit inside the work area.
- **`2`, at a point**: on each axis it would overhang, flipped to the other side of its anchor, so it
  opens to the left of or above the point. Then it is moved inward, as `1` is.

The anchor is the parent's origin plus the offset, as now. A popup larger than the work area takes
the area's top-left corner and is clipped. Any other value is refused with `InvalidArgument`.

**It is computed once, at creation**, against the work area at that moment, as the offset is already
resolved once (`TODO(popup-follows-parent)`). The client's first `Configure` carries the origin
the popup got, so a client that logs where its menu is logs where it landed.

**The code goes in a module of its own**, as `lib.rs` is at its entry: one pure function from the
anchor, the size, the work area and the value to an origin. `create` calls it, a net-zero edit.
In librsproto, `CreateWindowRequest` carries the value, written into aux16 for a popup and as zero
for every other role. Its decoder refuses a value it does not know.

**This is not the graphical prompt's step 1.** That step makes `1` required for every connection
but the manager's, which needs the manager's connection marked first. It will call F.2's function.
`TODO(app-covers-panels)`'s entry says so.

**Tests**, against a work area with both bars, at the neighbour of every edge:
- an overhang of one pixel on each of the four edges, and a popup exactly flush;
- a flip that fits, and a flip that does not fit and is moved inward;
- a popup larger than the area;
- `0` unchanged;
- the decoder handed a `3` it must refuse. A round trip through the encoder would never produce one.

Docs: [`rsproto-surface-ops.md`](../spec/rsproto-surface-ops.md) (the aux16 word, and "not
clamped" made "not clamped unless asked"), [`display-substrate.md`](../architecture/display-substrate.md)
§4a, `graphical-prompt.md`, and `deferred-decisions.md`. Every gate, as every window is
created through `create`. A real menu near the corner is F.4's gate. Author: the wire format.

### F.3 A menu's window, once, and a context menu

**`libui::menu::Context` is a context menu's state:**
- the window-local point where it was asked;
- the keyboard's row;
- a serial, which each opening bumps.

Its `key` works as `MenuState`'s does. Arrows skip separators and disabled rows, Enter chooses and
Esc closes. It has no Left or Right: there is only one menu.

**`libui::menu::Host` is a menu's window**, and the only copy of it. It holds:
- the popup's `Child`;
- its hover;
- what it shows: a drop-down by index, or a context menu by serial.

`sync` opens, switches, redraws or closes the window to match what the application wants shown.
It opens a drop-down with placement `1` and a context menu with `2`. `event` routes the popup's
events:
- a pointer event through its tree;
- a key through the state, a chosen row giving the message from the table the popup drew;
- a `Dismissed` closes only the menu that window was showing.

The last is the serial's job (finding 6). A right press elsewhere in the same window moves the menu,
whichever event arrives first. **A window shows one menu at a time**: a context menu closes an open
drop-down, and the other way round.

**The three applications' drop-downs move onto it**, and their main loops shrink by about what
finding 4 counts. A context menu is then a second kind of request to the same host. The shell's
`BarMenus` stays as it is, because it keys a filter field, resizes as you type, makes its windows
sticky and logs as the manager. It is the host's next caller if the host grows those.

**Tests**, with `libsurface`'s `MockTransport`:
- the host opens a menu when asked, and switches File to Edit by rebuilding the window, since a
  `Child`'s size is fixed when it opens;
- it closes when nothing is wanted;
- a `Dismissed` naming a replaced menu closes nothing, and one naming the menu on show closes it;
- a key chooses the row the popup drew;
- a context menu asks for placement `2` at its point;
- the popup's ink is painted and counted, since a widget can be present and draw nothing.

`the_popup_diffs_from_one_highlight_to_the_next`, and each application's
`the_window_tree_diffs_across_the_menu_states`, still pass.

Docs: `widget-toolkit.md` §8.1. Every gate, as every application's loop changes. `check-terminal`
and `check-login` open the drop-downs, at the places they log. Author.

### F.4 Files' menus

**A right press on a row selects it**, as a press does, without counting toward a double click,
and opens that row's menu at the press. `list_view` gives each row a context message beside
`activate`'s. The library's other two callers, the chooser and the reference picture, pass none.
**The listing's own `on_context` opens the folder's menu**; a row's, being nearer, shadows it.

| Where | Rows |
|---|---|
| A file or folder | Open · — · Cut · Copy · Duplicate… · — · Rename · Delete · — · Properties |
| The listing's empty space | New File · New Folder · — · Paste · — · Show Hidden Files |
| A drive in the sidebar | Open · Eject |

**Every row is a message the window already sends, built by the same definition as the bar's
row.** So its label, its chord and whether it is enabled cannot differ from the bar's: Delete in
`deny`, Paste offered whatever is on the clipboard, Show Hidden Files marked. A context menu adds
no action. The bar's tables keep their rows, since `check-login` aims at the File menu's by index.
A place in the sidebar gets no menu: its one action is the press.

**The Menu key and `Shift+F10`** open the selected row's menu at the row's bottom-left. With
nothing selected, they open the folder's at the listing's top-left.

**The gate**, `check-login`'s Files flow:
- a right click on a file's row, then Properties: its dialog's lines, which the gate knows already;
- Files maximized, then a right click on the listing's empty space near its bottom-right corner.
  The menu's logged rectangle must lie inside the work area, its bottom-right at the click. That is
  F.2's placement shown on a real menu;
- the Menu key, which opens the selected row's menu.

Docs: `widget-toolkit.md` (`list_view`). Author; the menu tables can be the implementer's.

### F.5 The editor's and the terminal's menus

**The editor**: Undo · Redo · — · Cut · Copy · Paste · — · Select All. These are the Edit menu's
rows, from the same definitions, so they are greyed while a field is open, as the bar's are.
- `take_area_pointer` acts on a left press only.
- A right press outside the selection places the caret. One inside the selection keeps it, so
  Copy copies what was selected.
- The Menu key and `Shift+F10` open the menu at the caret.

**The terminal**: Copy · Paste · — · Select All · — · Find · Clear Scrollback. These are the Edit
menu's rows, Copy enabled only with a selection. Clear and Reset stay on the bar only.
- A right press keeps the selection: the grid already ignores every button but the left.
- The Menu key and `Shift+F10` open the menu at the cursor's cell. A program in the terminal no
  longer receives `Shift+F10`, as in GNOME Terminal.

**The gates:**
- `check-terminal`: a right click on a selection, then Copy; a second right click, then Paste. The
  shell reads back the pasted text, which the gate checks. The Menu key opens the menu.
- `check-login`'s editor flow: a right click in the text, then Paste. The clipboard's text is in
  the buffer, by its receipt.

Docs: `widget-toolkit.md` (`text_area`). Author.

### What Part F does not do

- **A desktop menu**, or **a window's menu** on its title bar or task button: the maintainer's calls
  above, each deferred with its trigger.
- **A menu in a text field.** `text_field` has no selection, so there is nothing for Cut or Copy to
  act on.
- **Submenus.** The compositor allows a popup parented to a popup, and no menu here needs one.
- **New actions.** Every row is one its window already has.
- **The shell's `BarMenus`** on the new host (F.3 says why).
- **The graphical prompt's rule** that every application's popup stays inside the work area: that is
  its step 1, and F.2 builds the function it calls.
