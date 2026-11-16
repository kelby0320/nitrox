# Context menus

**Status: detailed 2026-11-16; F.1 to F.4 built 2026-11-16.** Part F of [`pre-phase-7.md`](pre-phase-7.md): a menu on the right
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

   **In Files it opens files nobody asked to open.** A right click on a row reaches `Msg::Press`,
   which reads the click run and its modifiers. Only a left press updates those (`note_press`).
   A second probe test sent a press the counter never saw, as a right press is:
   - after a double click on one file, it **opened** the next;
   - after a Shift-click range, it shrank the range to end at that row.
2. **The editor drops its selection on any press.** `take_area_pointer` places the caret on a
   press of any button, so a right click on a selection would lose what Copy acts on. The terminal's
   grid and both click counters (Files' and the terminal's) already read only `BTN_LEFT`. **The
   editor places its caret late**: its `on_pointer` only stores the event (`Msg::AreaPointer`), and
   the binary resolves it in `take_area_pointer`, after every routed message, because only the
   binary has the font to turn a pixel into a line and a column.
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
5. **Most files Part F would grow are at their size entry, or one line under the limit**:

   | Crate | Files at their entry |
   |---|---|
   | `libui` | `route.rs` 1,235 |
   | `librsproto` | `surface.rs` 2,561 |
   | `libsurface` | `lib.rs` 1,423 |
   | `nxfiles` | `app.rs` 1,719, `main.rs` 1,095 |
   | `nxedit` | `lib.rs` 3,811 |
   | `nxterm` | `lib.rs` 2,339 |
   | `compositor` | `lib.rs` 2,643, `input.rs` 2,636, `main.rs` 1,926; `server.rs` is 999 with no entry |
   | `desktop-shell` | `main.rs` 1,392 |

   `nxterm`'s and `nxedit`'s `main.rs`, 862 and 957 lines, have room. A move that lowers a file's
   entry gives it no room, since the entry follows it down, unless the file drops under 1,000. So
   new code goes into new modules, and each sub-part says what moves first.
6. **A press that dismisses a menu also arrives, and nothing promises the order.** On the wire,
   the `Dismissed` goes first: the compositor queues it while routing the batch
   (`route_one_batch`), and delivers the press after every batch (`deliver`). **The application
   sees the other order** when one pump picks up both: `libsurface`'s `next_event` drains its
   windows in creation order, so the parent's press comes before its popup's `Dismissed`. Neither
   order is in the spec. A loop written for one would close a menu that a second right-click meant
   to move.
7. **The compositor's `create` cannot see the screen.** `WindowStack` holds none, and neither
   `create` nor `server::dispatch` is handed one: only `main.rs`'s `Server` has it. `work_area`
   takes the screen as an argument.
8. **Two of the tools the plan needs are not reachable yet.** `libsurface`'s `MockTransport` is
   private to its own tests, and `libui` has no dev-dependencies. Its one transport double,
   `Counting`, refuses every request. And Miri's job leaves `libui` and the three applications out
   by name (`tools/xtask/miri.rs`, `LEFT_OUT`), so "the userspace targets" never reach Part F's
   router, its menu host or its menus.
9. **Both keyboards already send the Menu key**: PS/2 scancode `0x5D` and USB usage `0x65`, as
   `KEY_COMPOSE`. Nothing reads it.
10. **The graphical prompt plans the same placement**
    ([`graphical-prompt.md`](../design/graphical-prompt.md), step 1): *an application's popup is
    kept inside the work area… a menu near an edge moves inward.* It also needs the compositor to
    know which connection is the manager's, so that the rule can be required of applications.

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

**Miri runs once, at Part F's end**, by the maintainer's rule for a part's end (2026-11-06). It
does not run the kernel: Part F changes no kernel code, and the kernel's tests ran clean under
Miri at Part E's end. What it runs:
- **The job's targets that Part F changes**: `compositor`, `librsproto`, `libsurface` and
  `desktop-shell`.
- **`libui`, named on its own** (`cargo xtask miri libui`), as the job leaves it out for the half
  hour its tests take, and its router and menu host are Part F's core.
- **Not the three applications.** The job leaves them out because they hold no `unsafe` and spend
  sixteen minutes each rasterising, and Part F adds no `unsafe` to them. That is a gap: their menus
  are never run under Miri.

Part F is done when F.5 has merged and what Miri reports is triaged.

### F.1 A click is the left button's

**The router gives each button its own meaning.**
- `on_press` and `on_press_down` answer `BTN_LEFT` only.
- A right press goes to a new `on_context: Option<Msg>`, the nearest one on the hit's path. It fires
  on the press, once, and **only on a press that opens a gesture**, as `on_press_down` does: one
  with no other button held. A right press made while another button is held is part of that
  gesture, and opens no menu.
- **`on_context` carries no point**: like every press handler, it is a fixed message. A widget that
  needs the press, to place a caret or pick a cell, reads it through its own `on_pointer`. Files
  takes the point from the press its loop sees before routing, as it takes a left press for its
  click counter.
- A middle click, pressed and released inside, goes to a new `on_middle`.
- A capture still opens on whichever button goes down first. So a release outside, or a drag,
  still reaches the widget that took it.
- **`ScrollGrab` takes a left press only**, as a click is the left button's. A right press on a
  scrollbar scrolls nothing, and so cannot both scroll a list and open the menu of the listing
  around it.
- Click-to-focus is unchanged for every button.

**The shell's bottom bar moves its middle-click to `on_middle`**, and stops asking the event which
button it was.

**`route.rs`'s tests move first**, to `route/tests.rs`, in a commit of their own. The file drops
from 1,235 lines to about 525 and leaves the size list, which gives it room. That commit can be the
implementer's.

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
- A right press while the left button holds a capture fires no `on_context`.
- A middle click on a task reaches `on_middle`.
- A right press on a scrollbar's track scrolls nothing.

**And tests that hold what must not change**, passing before and after:
- a left drag, and a left release outside its widget, still reach the widget that took the press;
- a right press still focuses the focusable widget it lands on, as a left press does;
- `on_pointer` still sees every button's press;
- the scrollbar's second-button tests, unchanged.

Docs: [`widget-toolkit.md`](../architecture/widget-toolkit.md) §7.1 and §8.2. Every gate, as every
window routes through it. Author.

**Built 2026-11-16** (`a89531e2`, after two moves: `route.rs`'s tests, `c77508f3`, and
`panel.rs`'s, `737993e6`).
- `on_press` and `on_press_down` answer the left button only.
- A right press reaches `on_context` at the press, and only on a press that opens the capture.
- A middle click reaches `on_middle`. The shell's task buttons close their window on it, and the
  shell no longer asks the event which button made a message.
- `ScrollGrab` answers the left button only, and keeps no button of its own.

**Three departures, for the maintainer's eye:**
- **`desktop-shell/src/panel.rs` moved its tests as well.** It sat at its 1,211-line entry, and the
  task button's `on_middle` and its test needed room.
- **`nxterm`'s two tests that pressed button 0 now press the left one.** Before this change any
  button clicked, button 0 included.
- **A middle click anywhere else on the bottom bar now sends nothing.** The shell used to drop it
  with an arm of its own.

**Seven tests, each failing with its guard removed:**
- a right or a middle click on a title bar, its close box, a button, a tab and a list row sends
  nothing, where the left sends each one's message;
- `on_context` fires on the nearest handler, once, at the press;
- no `on_context` fires inside a left gesture;
- a middle click reaches `on_middle`;
- every button still focuses and still reaches `on_pointer`;
- the scrollbar ignores a right or middle press;
- the shell's middle click closes a task.

Five mutations each failed the test meant to catch them:
- the click by any button;
- the press-down by any button;
- `on_context` on any press;
- the middle button as a left click;
- the scrollbar taking any button.

So did removing the task's `on_middle`. 4,510 host tests pass, and all nineteen gates under KVM.
`check-login` middle-clicks the editor's task and waits for the close it asks for.

**After review** (`af2d4cd7`, after the editor's document tests moved to a module of their own,
`e85e9368`):
- **A press opens a gesture only with no other button held**, by the event's own `buttons`. One
  joining a held button fires nothing, even where nothing captured the first. A press made alone
  ends a capture whose release never came.
- **The capture's two new terms have tests**: a menu handler inside a clickable element, and a
  middle click across a repaint.
- **The editor's document takes a left press only**, the one `on_pointer` handler the sweep found
  acting on any button. F.5 builds its right press on that.
- **The reason first given for the guard was wrong.** The compositor does dismiss a menu opened
  mid-gesture, on the next press.

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

**The placement is one pure function in a module of its own**: from the anchor, the size, the work
area and the value to an origin. It computes in `i64`, since the anchor is a saturating `i32` sum
of a client's offset and the size any `u32`, so a flip cannot wrap.

**Three moves come first**, each in a commit of its own, which can be the implementer's:
- **`compositor/src/lib.rs`** sends its tests (line 1,088 on) to `tests.rs`, and the cursor, the
  move outline and key repeat (lines 250 to 414) to a module of their own. The file drops under
  1,000 lines and leaves the size list, which gives it room for what follows.
- **`librsproto/src/surface.rs`** sends its window-creation codec to `surface/create.rs`,
  re-exported, so no caller changes: `Edge`, `Role`, `CreateWindowRequest`, and the create
  request's and reply's writers and readers, lines 43 to 390, with `WindowInfo` and `ScreenInfo`,
  which sit among them. The placement value lands there. `surface.rs` stays over 1,000 lines, so
  its entry is lowered and it takes none of F.2's lines.
- **`libsurface/src/lib.rs`** sends its `MockTransport` to an exported `mock` module, which F.3's
  tests need too. Its entry is lowered.

**How the work area reaches `create` (finding 7).** The stack learns its screen once:
`WindowStack::new().on_screen(r)`, in the one line where `main.rs` builds the stack, which already
has the rectangle. `create` then asks `work_area` itself. So `server::dispatch` keeps its signature,
and `server.rs`, at 999 lines, is not touched. **A stack with no screen places every popup as
asked**, so the host tests' 175 `WindowStack::new()`s keep their meaning.

In librsproto, `CreateWindowRequest` carries the value, written into aux16 for a popup and as zero
for every other role. Its decoder refuses a value it does not know.

**This is not the graphical prompt's step 1.** That step makes `1` required for every connection
but the manager's, which needs the manager's connection marked first. It will call F.2's function.
`TODO(app-covers-panels)`'s entry says so.

**Tests**, against a work area with both bars, at the neighbour of every edge:
- an overhang of one pixel on each of the four edges, and a popup exactly flush;
- a flip that fits, and a flip that does not fit and is moved inward;
- a popup larger than the area;
- the extremes: an offset of `i32::MIN` and of `i32::MAX`, and a width above `i32::MAX`, for both
  values;
- `0` unchanged, and a stack with no screen placing `1` and `2` as asked;
- `create` placing a popup against the stack's own work area, through `dispatch`, with both bars
  up;
- the decoder handed a `3` it must refuse. A round trip through the encoder would never produce one.

Docs: [`rsproto-surface-ops.md`](../spec/rsproto-surface-ops.md) (the aux16 word, and "not
clamped" made "not clamped unless asked"), [`display-substrate.md`](../architecture/display-substrate.md)
§4a, `graphical-prompt.md`, and `deferred-decisions.md`. Every gate, as every window is
created through `create`. A real menu near the corner is F.4's gate. Author: the wire format.

**Built 2026-11-16** (`e65ee9a0`, after three moves by the implementer):
- `cbea944f`: the compositor's tests to `tests.rs`, and its cursor, outline and key repeat to
  `cursor.rs`.
- `af9a51c0`: the create codec to `surface/create.rs`.
- `8cb5c54f`: `MockTransport` exported behind libsurface's `mock` feature.

A popup's aux16 is its `Placement`. The compositor's `placement::place` computes in `i64`, and
`WindowStack::new().on_screen(..)` is set in `main.rs`'s one line, so `dispatch` and `server.rs` are
untouched.

**Four departures, for the maintainer's eye:**
- **The compositor's moved tests take a size entry of their own**, at 1,548 lines, as test files
  already do elsewhere. `lib.rs` drops to 926 lines, and to 941 with the placement, under the limit.
- **A flip that does not fit is moved inward from the edge it crossed**, so a menu taller than the
  space above and below the pointer starts at the top bar. That is the plan's "flipped, then moved
  inward", and it keeps a menu's first rows on screen.
- **A comment written twice in `WindowStack::new` is written once.**
- **`input.yml` runs on librsproto's new `surface/` too** (`a79ba503`). The workflow test caught
  it: a change confined to the create codec would not have run `check-input`.

**Eleven tests:**
- the neighbours of each edge;
- flips that fit, and flips that do not;
- a popup larger than the area, and an empty area;
- the extremes;
- placement `0` unchanged;
- one through `dispatch` with both bars up, with a screen and without, which checks the first
  `Configure`'s origin;
- the codec's round trip, its refusal of bytes no encoder writes, and a dialog's aux16 still
  ignored.

**Five mutations each failed the test meant to catch them:**
- `create` ignoring the placement;
- no flip;
- `i32` arithmetic;
- a decoder accepting any value;
- an encoder writing zero.

4,527 host tests pass, and all nineteen gates under KVM: every window is created through `create`.

**After review** (`f523276a`):
- **A placed popup is placed again when a commit changes its size**, from the anchor it was created
  at, so a menu that grows after it opens stays inside.
- **With the work area reserved away, a popup is kept inside the screen** instead of placed below
  it.
- **A stack never given its screen refuses a placed popup**, rather than placing it as asked with
  nothing said.
- **The wire values are tested against literal bytes**, and so is `WindowCreated`'s zero for a
  placed popup.
- **`librsproto` denies `missing_docs`.** Its sweep found three items besides the one the move
  lost, one of them behind the `io` feature.
- **The mock's doc says it places nothing.**
- **The spec and the graphical prompt now read "a placement other than `0`"**, not "`1`", so a
  context menu keeps its flip; a popup longer than the area still needs bounding there.

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

**An open that fails is answered once.** `Child::open` returns `None` when:
- the session already holds 64 windows;
- a buffer cannot be had;
- the content measures to nothing;
- the compositor refuses the placement.

Each copy the host replaces then logs and closes the application's menu state, and so does the
host: the bar word goes dark, and nothing is asked again until the person asks again. Without
that, `sync` would issue a `CreateWindow` on every pass of the loop, which in the terminal is every
burst of output.

**Clicking an open drop-down's own bar word may open it again** (found by this plan's review, and
older than Part F). The press dismisses the menu, and the same click's release then reaches the word's
`on_press`, which toggles it open. It needs the press and the release in different pumps, which a
person's click usually is. **F.3 reproduces it in a boot first**: open File, then press and,
100 ms later, release on File. If it reopens, the host fixes it. A `Dismissed` that closes a
drop-down is remembered until every button is up, and that same gesture's toggle of the same menu
does not reopen it.

**The three applications' drop-downs move onto it**, and their main loops shrink by about what
finding 4 counts. A context menu is then a second kind of request to the same host. The shell's
`BarMenus` stays as it is, because it keys a filter field, resizes as you type, makes its windows
sticky and logs as the manager. It is the host's next caller if the host grows those.

**What the tests run on.** `libui` takes `libsurface`, with its `mock` feature (F.2 exports the
mock), and `libos`, with its `mock` feature, as dev-dependencies: a real `Child` allocates its
buffers through libos. The failing cases run on `Counting`, the double `window.rs` already has,
which refuses every request. **The mock places nothing**: its `Configure` carries the origin `(0, 0)`
whatever was asked, so a test reads a placement from the request the host sent, not from where the
window says it is.

**Tests:**
- the host opens a menu when asked, and switches File to Edit by rebuilding the window, since a
  `Child`'s size is fixed when it opens;
- it closes when nothing is wanted;
- **one menu at a time**: a context menu asked for while a drop-down is up closes the drop-down,
  and the other way round;
- a `Dismissed` naming a replaced menu closes nothing, and one naming the menu on show closes it;
- **a right press elsewhere moves the menu in either order**: the press then the `Dismissed`, as
  `libsurface` hands them out, through a real `Session` on the mock; and the `Dismissed` then the
  press, as the wire carries them;
- `Context::key`: Up and Down skip separators and disabled rows in both directions, Enter chooses
  the row the popup drew, Enter with no row closes, Esc closes, and Left and Right do nothing;
- **a failed open**, on `Counting`: one `CreateWindow`, the state closed, and a second `sync`
  issues none;
- a context menu asks for placement `2` at its point, and a drop-down for `1`;
- if the bar word's reopening reproduced: a `Dismissed`, then the same gesture's release on that
  word, leaves the menu closed, and a later click on the word opens it;
- the popup's ink is painted and counted, since a widget can be present and draw nothing.

`the_popup_diffs_from_one_highlight_to_the_next`, and each application's
`the_window_tree_diffs_across_the_menu_states`, still pass.

Docs: `widget-toolkit.md` §8.1. Every gate, as every application's loop changes. `check-terminal`
and `check-login` open the drop-downs, at the places they log. Author.

**Built 2026-11-16** (`f638f3c9`, after the menu's tests moved to `menu/tests.rs`, `83bae804`).
`menu::Host` is every application's menu window, and `MenuState` holds a context menu beside the
bar's. nxterm's, nxedit's and nxfiles' main loops lose 98, 67 and 78 lines, and log the same lines.

**Three departures, for the maintainer's eye:**
- **The context menu's state is `MenuState`'s, not a `Context` of its own.** One state shows one
  menu at a time by construction: opening either closes the other, where two types would need
  every caller to close the one it was not opening.
- **The bar word's reopening reproduced, and is fixed.** A boot on the release image clicked the
  open File menu's own word and it opened again every time, released 20 or 100 ms after the press.
  `dismissed` remembers the bar menu until every button is up (`buttons_up`, which each loop calls
  after routing a pointer event with none held). The same click's release toggles nothing, and a
  press elsewhere ends its gesture with its own release. (Review replaced `buttons_up`; see below.)
- **A drop-down's anchor that is not laid out yet waits a frame.** nxedit used to close the menu
  then; nothing can ask before the first layout.

**Eleven tests, seven of the host's and four of the state's:**
- the host opens a drop-down inside and rebuilds the window for another menu;
- it closes when nothing is wanted, and asks for nothing more;
- one menu at a time, and a context menu asks to be placed at its point;
- a dismissal closes only the menu its window showed;
- a right press elsewhere moves the menu in either order, through a real `Session` on the mock;
- a failed open is answered once;
- the popup's ink is counted;
- the state's word click;
- its context keyboard;
- `choose` on the bar;
- `shown_at`.

Eight mutations each failed the test meant to catch them:
- the word-click guard removed;
- no serial;
- the host keyed on whether a menu is shown, not which;
- the placements swapped;
- a failed open unsaid;
- no skip;
- a toggle keeping the context menu;
- nothing drawn.

**`check-login` now clicks the open File menu's own word** (`2cc73038`). It holds the press 100 ms,
waits for the compositor's dismissal, expects no popup within two seconds, then opens the menu
again. With the fix removed the gate fails on this step. 4,542 host tests pass, and all nineteen
gates under KVM.

**After review** (`f0064f4c`, after the input router's tests moved to `input/tests.rs`,
`008778ec`):
- **The compositor sends a `Dismissed` after the press that caused it.** It used to queue the
  dismissal while routing and the press after the batch, so a client could read them in either
  order, with motion between.
- **The latch is set only while a button is down.** Each loop reports what its window's latest
  pointer event held (`buttons`, replacing `buttons_up`). A dismissal read after its own release
  held back the next click on the word; it now holds back nothing.
- **A loop that forgot the call** now only loses the fix, as the menu reopens, rather than eating a
  click. `check-login` holds Files' call.
- **A bar menu reached by Left or Right gives way to a press outside.**
- **The host's hover, `owns`, configured origin and key routing are tested.** Two mutants survive,
  routing against the unlit tree and measuring the lit one: a menu row's hover layer lies inside the
  row, the widget a press captures, and a highlight changes no size.

### F.4 Files' menus

**A right press on a row opens that row's menu at the press, and is not `Msg::Press`.** That path
reads the click run and its modifiers, which only a left press sets (finding 1), so it would open
files and reshape ranges. Instead:
- **On a row already selected or marked**, the selection and the marks stay, and the menu acts on
  all of them, as Cut and Copy already do. So a right click inside a Ctrl- or Shift-picked set
  copies the set.
- **On any other row**, that row alone is selected and the marks are cleared, as a plain click
  would. But the click run and its modifiers are neither read nor changed, so nothing opens and
  no range moves.

`list_view` gives each row a context message beside `activate`'s. The library's other two callers,
the chooser and the reference picture, pass none. **The folder's menu hangs on the rows' area**, a
row's `on_context` shadowing it as the nearer, and **not on `list_view`'s whole element**, which
holds the scrollbar too.

| Where | Rows |
|---|---|
| A file or folder | Open · — · Cut · Copy · Duplicate… · — · Rename · Delete · — · Properties |
| The listing's empty space | New File · New Folder · — · Paste · — · Show Hidden Files |
| A drive in the sidebar | Open · Eject |

**Every row sends a message the window already sends, and a row that is also on the bar is built
by the bar row's definition.** So its label, its chord and whether it is enabled cannot differ
from the bar's: Delete in `deny`, Paste offered whatever is on the clipboard, Show Hidden Files
marked. **Three rows have no bar row** and so get a definition of their own: Open, which sends
`Activate`, and a drive's Open and Eject, which send `Drive` with the row's key and its eject
button's, as a press on either does. A context menu adds no action. The bar's tables keep their
rows, since `check-login` aims at the File menu's by index. A place in the sidebar gets no menu:
its one action is the press.

**The Menu key and `Shift+F10`** open the selected row's menu at the row's bottom-left. **A selected
row scrolled out of view is brought into view first**: `list_view` builds only the rows that fit,
so there is nothing to hang the menu from until it is. The key makes the list follow its selection
again (`ensure_visible`), and the menu opens on the next frame, at the row as it is then laid out.
With nothing selected, they open the folder's menu at the listing's top-left.

**Tests**, each failing without its change:
- after a double click on one file, a right press on another opens nothing and selects it alone;
- after a Shift range, a right press inside it keeps the range, and Copy copies every file in it;
- after Ctrl marks, a right press on a marked row keeps the marks, and on an unmarked row clears
  them;
- a right press on a row opens the row's menu, and on the rows' empty space the folder's;
- a right press on the scrollbar opens nothing;
- the Menu key and `Shift+F10` open the selected row's menu at its bottom-left, and with the
  selection scrolled away bring it into view first;
- a row's label and availability equal the bar row's for every row the two share.

**The gate**, `check-login`'s Files flow:
- a right click on a file's row, then Properties: its dialog's lines, which the gate knows already;
- Files maximized, then a right click on the listing's empty space near its bottom-right corner.
  The menu's logged rectangle must lie inside the work area, its bottom-right at the click. That is
  F.2's placement shown on a real menu;
- the Menu key, which opens the selected row's menu.

Docs: `widget-toolkit.md` (`list_view`). Author; the menu tables can be the implementer's.

**Built 2026-11-16** (`c6fb94d6` and `1adbcba0`, after the bar's menu table moved to `menus.rs`, `140080e6`, by the
implementer). `list_view_with_menus` takes a `ListMenus`; `list_view` keeps its signature.
`ListState::follow` re-follows a selection. Files' `Context` is a row, the folder or a drive, asked
for by `Msg::ContextMenu`. `note_pointer`, which the binary calls before routing each pointer event,
counts a left press and records where a right one landed. `KEY_F10` is new in `libkern::abi`.

**Departures, for the maintainer's eye:**
- **A context row the bar has is taken from the bar's table by its message**, rather than built
  from a definition the two share. It is the same row, so the two cannot differ, and there is no
  definition to keep.
- **A new listing closes a context menu.** A drive pulled while a row's menu is open re-lists home,
  and the menu's rows would act on home's first row. `show` already drops a prompt and a question,
  for the same reason. **A menu whose drive has gone closes** at the next frame, rather than staying
  up empty.
- **A right press on the folder's space leaves the selection alone**, as its rows act on none.
- **A menu opened by the keyboard has no row lit**: `select_first` serves bar menus, so Down lights
  the first row and Up the last.
- **Files logs `context menu at x,y for <name>`**, window-local, after `menu popup`: the gate reads
  what a menu is for and where it was asked for.
- **The gate's walk down a menu is one helper**, `walk_to_menu_row`, which the File menu's rename
  now uses too. `right_click_at` confirms a right press as `click_at` does a left one.

**Twenty tests.** Four of the list's:
- a row's menu on a row, the space's below the rows, none from `list_view`;
- a row's on its button;
- none on the scrollbar;
- `follow`.

Fifteen of Files', each driving the real tree and router:
- after a double click, a right press on another file opens nothing and selects it alone;
- inside a Shift range it keeps the range, and Copy copies all of it;
- on a marked row it keeps the marks, and on another clears them;
- a row's menu at the press, the folder's below the rows and in an empty folder, and Open opening;
- none on the scrollbar, which the right button does not page;
- the Menu key and `Shift+F10` at the selected row's bottom-left, and `F10` alone not;
- the Menu key bringing a scrolled-away selection back first;
- with nothing selected, the folder's menu at the listing's top-left;
- every shared row equal to the bar's, in four states;
- a drive's menu sending what its row and its button send;
- a context menu's keys;
- the receipt and the row the gate aims at;
- a new listing closing a menu, and a keyed request;
- a drive's menu closing when the drive goes;
- the popup drawing the context menu.

And the gate's receipt parser.

**Mutations**: each failed the test meant to catch it.
- The list's four: no space menu, the space menu on the whole element, no row menu on a button's
  row, `follow` doing nothing.
- Files' sixteen: a right press as a press, every row selected alone, marks kept, the press not
  noted, no folder menu, no `follow`, the row's top-left, `F10` without Shift, Delete built by hand,
  a context row not closing the menu, `show` keeping the menu or the keyed request, Eject always
  enabled, the keys ignoring the context menu, the popup drawing none, a drive's empty menu kept.
- Two of `check-login`'s: a context menu placed inside rather than about its point fails the
  maximized step, and Files without the Menu key fails the keyboard step.

### F.5 The editor's and the terminal's menus

**The editor**: Undo · Redo · — · Cut · Copy · Paste · — · Select All. These are the Edit menu's
rows, from the same definitions, so they are greyed while a field is open, as the bar's are.

**The menu opens from `take_area_pointer`, not from `on_context`** (finding 2). That is the one
place that has both the press, with its point and its button, and the font that turns the point
into a line and a column. For a right press, it:
1. finds the line and column under the press;
2. keeps the selection if that position is inside it, or places the caret there if not, dropping
   the selection;
3. asks for the menu at the press, the area's origin added back to make the point window-local.

So the menu's rows read the selection as it stands after the press: Cut never removes text away
from the click. A left press is handled as today, and a middle press does nothing. The text area
takes no `on_context`. The Menu key and `Shift+F10` open the menu at the caret, the bottom-left of
its cell.

**The terminal**: Copy · Paste · — · Select All · — · Find · Clear Scrollback. These are the Edit
menu's rows, Copy enabled only with a selection. Clear and Reset stay on the bar only.
- **The menu opens from the grid's own pointer handler**, which already sees every press. A right
  press keeps the selection and asks for the menu at the press, the grid's origin added back.
- The Menu key and `Shift+F10` open the menu at the cursor's cell. A program in the terminal no
  longer receives `Shift+F10`, as in GNOME Terminal.

**Tests**, each failing without its change:
- the editor: a right press inside a selection keeps it and opens the menu; outside, it places the
  caret, drops the selection, then opens; Cut after either acts on what the menu showed;
- the editor: a left press is unchanged, and a middle press moves nothing;
- the editor: with a field open, the menu's rows are greyed as the bar's are;
- both: the Menu key and `Shift+F10` open the menu at the caret's or the cursor's cell;
- the terminal: a right press keeps the selection and opens the menu at the press;
- the terminal: `Shift+F10` reaches no program, and every other F-key still does.

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
