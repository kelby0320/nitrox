# Nitrox: Desktop Shell

## Status

**Partly built, and checked 2026-09-18** — Milestone 7 Part E built the shell and M8 Part C
added its second bar; M12 Part A added dialog placement and made the taskbar's insist a second
click; M12 Part E bound `/dev/clipboard` into every application namespace it constructs, and
Part F gave it the **wallpaper** — a full-screen bottom-most `Role::Panel` with a zero
reservation, holding a PNG the theme names and this shell decodes, because the shell holds
`/home` and a theme where the compositor holds neither; **M13 Part C made the overview a
translucent `ARGB8888` surface over the live desktop**, replacing the dimmed copy of the wallpaper
it used to redraw; **M14 Part H made the applications modal list desktop entries** rather than
every program in `/bin`; **Phase 5 Part E laid it out on the screen it is on** — every bar, the
wallpaper, the overview and the placement cascade sized from `/dev/draw/screen` rather than a
written-down 1280×800 (see [`clipboard.md`](clipboard.md) and `display-arm-plan.md` M12 decision
2); and **the desktop refresh's Part C (checked 2026-09-18) gave it the design's panels** — 30
pixels each on the panel ground: an Applications *menu* that still filters as you type and a
Places menu, replacing the applications modal (§4), and a bottom bar with show-desktop, restyled
window buttons and a bounded desktop switcher, replacing the indicator (§7);
[`desktop-shell`](../../userspace/desktop-shell) is the code. Graduated from `design/` on
2026-08-25, revision 2.

**This document outruns its code on purpose, so read it section by section.** What is built:
the **top bar** (§3) and its **Applications and Places menus** (§4) — `/applications` listed
through the profile server, filtered as you type, and the places a person's files live —
**launching** (§6's spawn half), each application into a
namespace the shell constructs, and since M10 Part D **launching on somebody else's behalf**
(§4a): a client names a path over `/dev/desktop` and the shell opens it, **placement and window management** (§8), the shell driving
`Place`/`Raise`/`SetFocus` as the compositor's attached manager, and — since M8 Part C — the
**bottom bar** and its **window list** (§2, §7): one entry per `normal` window, click to raise,
click the focused one to minimize, middle-click to close, `Super+H` to minimize without the bar. The toolkit question
§5 settled is answered and built ([`widget-toolkit.md`](widget-toolkit.md)).

Since M8 Part D it also has **desktops** (§7): several of them, created on demand, switched with
`Super+1..4` or from the **switcher** at the bar's end — the bars are *sticky*, so they
are on every desktop rather than on the one they were created on — with the focused window moved
between them by `Super+Shift+N`. The window list shows the current desktop's windows only.
**Naming a desktop is what makes it persist** — an unnamed desktop disappears when its last
window leaves, a named one stays, and the list always ends with one empty desktop to create
into. When the desktop that disappears is the current one, the shell lands on the one that takes
its place **and tells the compositor** — which, until the desktop refresh's Part C, it did not:
the two went on disagreeing about which desktop was current until a switch showed it.

Since M8 Part E the desktop's name at the bar's end opens the **overview** (§6): frozen
thumbnails of the current desktop's windows, a sidebar of the others, and a window moved to
another desktop by dropping its thumbnail on it. The thumbnails are snapshots — `Manage::Capture` scales a window into a buffer
the shell allocated — so a window drawn after the overview opens shows its state at the moment it
opened, which §6 accepts deliberately.

**Its gestures all work as of 2026-08-26, and most of them did not before**: a thumbnail
*dragged* onto a sidebar row moves that window to that desktop (M8 Part E), a thumbnail
*clicked* raises its window and closes the overview, and a sidebar row *clicked* switches to that
desktop — which is what §6 always claimed. It is dismissed by clicking the desktop row you are
already on, by clicking its background (which makes the desktop's name a toggle, since the overview
covers the bar), by clicking a window, or by Escape — four ways, because with none of the first
three an overview on a desktop with no windows was a dead end. The overview is **sticky** like the bars, so it
survives the switch and re-captures for the desktop arrived at, whether that switch came from its
own sidebar or from a chord; the menus and the name prompt are sticky for the same reason. Reported from a real session: only the drag had ever been built, and only the drag had
ever been gated.

Since Milestone 9 the shell is also the **other end of a window's own chrome** (§8's
"placement", from the other side). A client
draws its title bar and its buttons, and the two that are not the client's to perform arrive
here as requests: `Surface::RequestState` becomes a `SetMinimized` or a `Configure` — maximise
*moves* the window to the work area's origin as well as resizing it, so a maximised window does
not cover the bars — and a drag becomes `Surface::StartMove`, which the compositor performs
itself while the shell watches the geometry go by. **Dragging a window's corner** is the same
division seen from the other side (Part E): the compositor runs the gesture and draws an outline,
and hands the shell one `DragEnded` at the release — which the shell answers with the `Configure`
it would have sent anyway, because changing a window's geometry is the manager's and there is one
path to it rather than two that can disagree. **Snapping** (Part F) is that same event reached a
different way: the shell registers eight `SnapZone`s computed from the work area — four edges,
four corners, a 24-pixel band — and re-registers all of them whenever the work area changes,
because the zones *are* the work area. A window dropped in one takes that zone's target: half the
work area for an edge, a quarter for a corner. The compositor previews the target under the
pointer and matches the table; which region means which rectangle never reaches it. Closing is the pair the milestone added at
both ends: a client's own close button ends the client, and the window list's middle-click sends
`Manage::RequestClose` — an *ask*, which a client with unsaved work answers with a dialog.

**The insist is a second middle-click, not a clock** (M12 Part A). It was `Manage::Close` two
seconds later, and that was safe for exactly as long as no client could decline: every
application answered `CloseRequested` by exiting, so the timer never fired outside a wedge.
`nxedit`'s confirmation is the first client that deliberately does not answer — it is asking the
person the shell's own question — and against it a timer destroys the window, and the buffer with
it, two seconds after one click with no way to intervene. **A shell cannot tell "wedged" from
"asking"; the person looking at the dialog can.** So the first click asks and the second insists,
which is what a Force Quit is on every desktop this borrows from.

**And the arming expires**, five seconds after the ask. Without that the shell would never learn
that a client *answered*: a person who middle-clicked, read the question and chose "keep editing"
left the entry armed for the life of the window, and a middle-click at any later moment went
straight to `Manage::Close` with no question — the same lost buffer, with the two-second bound
replaced by an unbounded one (PR #267 review). There is no signal that says a client declined —
`CloseRequested` has no refusal by design, and inferring one from a dialog appearing is the
coupling this milestone rejected — so the second click counts only while it is still part of the
first gesture. A click after that asks again. `check-login` drives all three: the ask, the expiry,
and the insist.

**And a `dialog` is placed and not listed** (M12 Part A). It is *held* for the manager exactly as
a `normal` is, so a shell that ignored one would leave every dialog waiting out the compositor's
200 ms deadline and then appearing where its client asked — which, for a client that cannot know
where it is, is the corner. It is centred on its parent and clamped to the work area, which
[`rsproto-surface-ops.md`](../spec/rsproto-surface-ops.md) says a manager can work out for itself
from the `WindowCreated` it already gets and the geometry it already tracks. It gets no taskbar
slot: an entry offering to close or minimise a question on its own is a question minimised behind
its window, and its parent's slot stands for both.

What is **not**: the **system tray** (§9), which is v2 and an inter-process protocol rather than
a widget; and **live thumbnails**, an optimisation §9 gives a trigger rather than a v1 goal. Sections describing those describe intent,
not behaviour — the rule the rest of `architecture/` follows does not hold there.

What a user actually sees and touches: the bars, the Applications and Places menus, the overview,
and the desktop switcher. Settled with the maintainer 2026-08-04, with two items deliberately
shelved (§9).

Three documents divide this space:

- **`display-substrate.md`** — mechanism. Framebuffer, surfaces, input, the test gate.
- **`ui-composition-model.md`** — semantics. What a window *is*, ports, desktops. (Templates
  and durable wiring were cut in its revision 3, 2026-08-21.)
- **This document** — the shell built on both, and the demands it places back on the compositor
  (§8). It is also what settled the widget toolkit's central question (§5).

The toolkit itself gets its own document; the plan
([`display-arm-plan.md`](../planning/display-arm-plan.md)) requires it before Milestone 4.

## 1. The shape, and where it comes from

**Somewhere between GNOME 2 and GNOME 3/Cosmic**, deliberately, and worth recording because it
explains choices that would otherwise look arbitrary:

- **From GNOME 2** — persistent top *and* bottom bars, with a window list on the bottom. Not a
  dock.
- **From GNOME 3 / Cosmic** — an overview that shows the current desktop's windows at a glance
  with a sidebar of the others, an applications modal with a search field, and desktops that
  are created when wanted rather than fixed at a set of four.

The parts that are *not* copied are the interesting ones. Cosmic's dock is replaced by a
GNOME 2 window list; GNOME 3's automatic workspace lifecycle is shelved rather than adopted
(§9); and GNOME 2's full desktop switcher on the bottom bar was replaced by a compact indicator,
which the desktop refresh bounded and brought back (§7a).

## 2. The surfaces

| Surface | Persistent? | Contents | Churn |
|---|---|---|---|
| **Wallpaper** | yes | the picture the theme names, placed by its `wallpaper_mode`: fitted and centred, or filling the screen with the overhang cropped | **none** — drawn once at startup |
| **Top bar** | yes | `Applications` and `Places` (left), the clock (centred on the screen), a tray (right, v2) | low |
| **Bottom bar** | yes | show-desktop, window list, desktop switcher | **high** — every open, close, retitle, focus change |
| **Applications and Places menus** | no | a filter field over the matching desktop entries; the places, with their paths | **highest** — the Applications menu is rebuilt, and resized, per keystroke |
| **Overview** | no | thumbnails of the current desktop, sidebar of the others, over the live desktop dimmed by its own translucency | bursty |

The churn column is not decoration: it is what settled the toolkit question in §5 — and the
wallpaper's zero is why it is the one surface here that builds no element tree at all: it is a
picture blitted into a buffer, not a widget.

**The wallpaper is a `Role::Panel` with `reserve: 0`** (M12 Part F), which is what makes it
bottom-most, unfocusable and free of any claim on the work area without a new role: a panel
cannot take focus, a zero reservation subtracts nothing, and the compositor's stack is
creation-ordered so creating it first puts it under everything. Like the bars it is made
**sticky**, because a picture behind everything belongs to the screen rather than to one desktop.
It is absent when the theme names no file, which is the shipped default.

**Everything here is sized from the screen, read once** (Phase 5 Part E): `/dev/draw/screen` at
startup gives the width the bars span, the height the window list sits a bar above, the wallpaper's
and the overview's full-screen buffers, and the height the placement cascade wraps at. The
arithmetic — where the window list sits, how many thumbnail columns fit beside the sidebar — is
`desktop_shell::Screen`, host-tested at every size rather than asserted at one; how many window
buttons fit beside the switcher is `panel::task_capacity` of the switcher's measured width, since
the switcher carries the desktop's name (§7). A screen the shell cannot read is fatal: the leaf resolves
through the same binding its connection just did, and bars sized by a guess are what this replaced.

**And the overview sits over it.** The overview is a full-screen **`ARGB8888`** window (M13
Part C) — the one surface this shell creates that is not opaque. Its ground is a single
`fill_rect_alpha` of black at 210/255, so what shows through is the *live* desktop the compositor
already has underneath: wallpaper, windows and all. The sidebar is filled at 150 over that, which
is why it reads as a lighter sheet laid on the ground rather than a hole cut in it, and the rows
are drawn with `paint_over` — `paint` without its clear — so the text and the miniatures on the
panel stay opaque. **That combination is what needed a per-pixel alpha channel rather than a
per-window opacity**: the panel is see-through and its content is not, and one opacity for the
whole surface cannot say both.

Each sidebar miniature still draws the wallpaper **scaled**, and that is not the same picture: a
row is a desktop that is *not* being composited, so there is nothing underneath it to show
through. The shell keeps the decoded wallpaper for those — four megabytes, against re-decoding the
file on a gesture that should feel instant.

**What this replaced is worth recording.** Until Part C the overview was a full-screen *opaque*
window, so to look like an overlay it had to redraw the desktop itself: the wallpaper, dimmed,
composited by a `libdraw::scale::dim` that existed for this one caller and is now deleted. A flat
ground made the picture disappear whenever you looked at the desktops (reported 2026-09-02); the
dimmed copy was the nearest thing to translucency reachable without a channel.

## 3. One process, several windows

The shell is **one process owning several windows**, not separate taskbar, launcher and overview
clients.

They share state intensely — the window list, the desktop list and focus all come from the
compositor, and one subscriber is simpler than three with identical authority. The composability
argument in the composition model was about *users wiring applications* — and that wiring was cut
in its revision 3; what remains is a drag that dispatches one message. Either way the shell's own
chrome is not something anyone composes.

Because each surface is a window with a **role** (§8), splitting later costs nothing at the
compositor. That is the cheap option kept open rather than exercised.

## 4. The Applications and Places menus

**Two words on the top bar, each opening a menu that hangs from it** (desktop refresh, Part C,
which took the design's panels). `Applications` carries the design's accent dot; the clock is
centred on the screen, not on what the words leave; the right-hand end the design gives quick
settings and notifications is empty, because neither exists (§9). The bar is a `libui` `Child`
like every application's window, so its words are routed and light under the pointer, and the
menus hang from where a layout of the bar says the words are.

It replaced the **applications modal**: a 320×240 popup at the bar's corner, a search field over a
scrolling list. The design's menu is a menu, and following it literally would have lost the one
thing the modal did best — so the look is the design's and the behaviour below is the modal's.

**One menu, two triggers**: the `Applications` word and `Super+A`. They open the same thing,
because they are the same intent. A second `Super+A` closes it — or the name prompt, if that is
what is up — which the word cannot: a press on it dismisses the open menu, and the click then
opens it again, as on every window's menu bar.

**`Super+A`, not a tap of `Super`** (Phase 5). A bare modifier is the chord a *launcher* wants —
one field over applications, files and settings — and this is the applications menu, so the tap is
left unspent for that. It is also what makes the menu reachable at all on a machine with no
pointer: the word sits on a `panel`, panels take no keyboard focus, and the laptop Phase 5
targets has no pointing device until USB (Phase 6).

**Typing narrows it.** A filter field sits above the rows, every key the menu does not claim is
the field's, and the menu is as tall as what matches — it shrinks as you type, a popup resizing
itself by committing a smaller buffer. **The top match is lit**, so Enter launches the row a person
can see rather than an unnamed "top hit". Enter with nothing typed closes the menu instead, which
is the rule every menu here keeps for a chord-opened menu with no cursor: guessing fires an action
nobody pointed at. A filter that matches nothing says so in a row that cannot be chosen.

**Arrows, Enter, Escape, and Left and Right between the two menus** are `libui`'s `MenuState`, the
same value every window's menu bar keeps — which is what makes `Places` reachable without a
pointer at all.

**Its entries are desktop entries** (M14 Part H) — one TOML file per graphical application,
projected at `/applications` the way `/bin` is projected, carrying a display name and the program
to spawn. See [`desktop-entry.md`](../spec/desktop-entry.md).

Until then they were `/bin` programs, which meant every service, server and command-line tool the
profile projects, under the name of its binary. Filtering that needs a fact that cannot be read
off an ELF — "is this graphical?" is a claim somebody has to make — and a name a person recognises
has to come from somewhere too. Both come from the entry, so they cannot disagree. Type-to-filter
still runs over them with no special mechanism, and matches the program as well as the name: the
people using this system are as likely to type `nxedit` as "editor". (An earlier version also listed `~/Desktop/*.nxg` templates; templates
were cut in composition revision 3.) "Open the code-editor desktop" is a launcher entry, not a
feature.

**What the design has that this does not.** No **categories**: the design groups three programs
under "Accessories" and a System section, and a taxonomy invented for three entries is one to
live with (maintainer, 2026-09-17) — a flat list until there are enough to need grouping. No
**icons**: the design's are three CSS boxes, and an icon is an asset question — a format, where the
files live beside a desktop entry, who draws them — rather than a drawing one; three glyphs keyed
by program name would be the thing that has to be removed when that is answered. And
**`Run Application…` and `End session` are absent, not disabled**: the launcher is deferred and
there is no logout, and a row that does nothing is worse than none.

**The Places menu** is `Home`, `Documents`, `Downloads`, `Pictures` and `Root`, each with a swatch
— the accent, and `deny` for the root, the one place past the person's own files — and its path
beside it, dim, with the home written `~`. The list is `libfs::places`, which the file browser's
sidebar asks too: it moved there from `nxfiles` so the two cannot come to disagree about what a
person's places are. Choosing one launches `nxfiles` with the path as `argv[1]`, which is where
its first window opens; its home is still `HOME`. The design's `Trash` is dropped, because there is
no trash.

**The desktop-name prompt** (`Super+R`) is a popup of its own above the bottom bar's right-hand
end, where the desktop's name is: a line saying what it is for, and the field. It borrowed the
modal until the modal became a menu.

The chord means the shell receives a keystroke **regardless of focus** — see §8's global hotkey
requirement, which is a capability rather than an ambient grab. The shell registers it through the
manager channel (`Manage::RegisterHotkey`), so an application cannot take it.

### 4a. Opening a path, which is launching asked for by somebody else

Since M10 Part D an application can ask the shell to open a path — `Desktop::Open` on
`/dev/desktop` ([`rsproto-desktop-ops.md`](../spec/rsproto-desktop-ops.md)) — and the file
browser is its first caller: pressing a file row names the path and the shell launches
[`nxedit`](../../userspace/nxedit) on it.

**The client names a path and never a program**, and that is the whole of the capability
argument. An application has no `/bin` in its namespace and no `BIND_NAMESPACE`, so it cannot
launch anything and should not be able to; what it has is a question. A request naming a program
would be the shell running arbitrary code on a caller's say-so — ambient authority arriving
through a protocol rather than through a handle, which is exactly the shape this system rejects.

**What opens a path is one constant** (`nxedit`, whatever the path). A table keyed on extension
is what that becomes when a second program can open something, and a mechanism with one entry
has no second case to check itself against. The launcher's policy staying in the launcher is the
part that matters; which program it picks is data.

**The shell does not check the path first.** It could ask whether the path is a file, and the
answer would be about the *shell's* namespace rather than the caller's or the opener's — three
namespaces that agree today only because one process builds all three. What the path turns out to
be is reported by whatever opens it, in the window the person who asked is looking at.

## 5. What the shell settled about the toolkit

The terminal is the easy case for a widget toolkit — static chrome around a grid that draws
itself. **The shell is the hard case**, and it is what decides the toolkit's central question.

Its two most important surfaces — the window list and the launcher results — are **lists whose
contents derive from state and whose length changes**. In a purely explicit toolkit the
application writes "diff my window list against my row widgets; create, destroy, reorder" by
hand, which is hand-rolled diffing: the exact work a declarative toolkit exists to automate.

**The answer is neither extreme.** An explicit toolkit *plus one model-backed list widget* —
GTK's `ListView`, Qt's model/view — covers the window list, the desktop previews and the
launcher results, which is essentially all of the churn, for a fraction of a diffing engine's
machinery.

The widget set that follows is small enough to be believable, and covers the terminal and the
whole shell:

> **label · button · container (row/column) · text field · list view · scrollbar · menu ·
> custom-draw**

**And the decision is reversible.** Explicit and declarative are both *retained* — both keep a
persistent tree that knows what changed, so both produce damage rectangles. A declarative
front-end is a layer that consumes descriptions and emits mutations against the same tree, so it
can be added later without disturbing the foundations, provided the mutation API stays clean
enough to be a target.

## 6. The overview

The workspaces button shows the current desktop's windows laid out so all are visible, with a
sidebar previewing the other desktops. You can switch desktops from inside it, and drag a window
onto another desktop — including onto a new one.

**Thumbnails are frozen, and that changes what this is.**

The obvious implementation is the compositor compositing **live windows with a scale transform**,
with the shell moving real windows into a grid — requiring scale as a window attribute, geometry
save and restore, and windows physically relocating.

Frozen thumbnails make all of that unnecessary. The shell asks the compositor for a **snapshot**
of each window and draws those images into its own overview window like any other content. Real
windows never move. **The compositor gains one operation — capture — instead of a transform
pipeline**, which is the right side of "the compositor stays small".

Three consequences:

- **Capture at thumbnail size, not full size.** Snapshotting eight 1920×1080 surfaces is ~66 MB;
  scaling once at capture and storing 480×270 is ~4 MB. The downscale happens once per window on
  entry rather than once per frame, which is what makes this affordable with no GPU.
- **Switching desktops inside the overview is trivial** — it fetches a different set of images.
  Nothing moves and nothing needs restoring. Sidebar previews are smaller versions of the same
  thing.
- **A window's content does not update while the overview is open.** A terminal printing behind
  the overview shows its state at the moment you opened it. Accepted deliberately; live
  thumbnails are an optimisation with a trigger (§9), not a v1 goal.

**Capture must be capability-gated.** Handing a client another window's pixels is exactly the
leak the composition model's namespace rule exists to prevent. The shell may do it because it
holds `/dev/draw` with rights an application does not — the same shape as `session-mgr` holding
bindings a session never sees.

## 7. The bottom bar: show-desktop, the window list, and the switcher

**The design's bottom panel** (desktop refresh, Part C): a show-desktop button, a rule, one
button per window, and at the right-hand end a desktop **switcher** — `‹`, up to three cells, `›`,
and the current desktop's name. The bar is a `libui` `Child`, so every one of those is routed by
the toolkit rather than hit-tested by dividing an x coordinate by a width written down beside the
layout — which a switcher whose width changes with the desktop's name could not have survived.

**A window's button** is 186 pixels: a dot, then its title. The dot is dim for a window put away,
the accent for the one with the keyboard, and `ok` for the rest — the design's three states, where
the old list put `_` and `>` in the label; the serial log keeps those markers, because every gate
reads the list from there. The focused window's button is a raised face on the window's own
ground, and the pointer draws any button's border in the accent. **How many fit is measured**: the
switcher's width is laid out, and `panel::task_capacity` gives what is left, host-tested at seven
widths and four names never to put a button under it. The chord that minimises the focused window
is bounded by the same count, for the reason it always was — a window past the bar's end has no
button to come back from.

**Show-desktop** puts away every window the bar shows on this desktop, and lights; pressed again
it brings back exactly what it put away — not everything minimised — the one that had the
keyboard last, so it has it again. **The restore set is the shell's**, because minimising is a
manager operation: the plan's first draft put it in the compositor, the process the architecture
keeps free of policy. It lets go the moment anything on that desktop comes back another way — a
button, a new window, a window moved there — because "bring back what the first press put away"
stops being a coherent request once the desktop is not what that press left, and the light is what
says whether there is anything to bring back. One set at a time.

### 7a. Why the switcher is back

**This section used to record the opposite decision**: a *compact indicator* showing the current
desktop's name, with GNOME 2's full switcher rejected for two reasons — with **dynamic** desktops a
row of boxes is a list that changes length, the churniest widget in the shell permanently holding
bar space; and "a name is also a better use of the space than a row of numbered boxes".

The design answers both, which is a decision revisited on new evidence rather than reversed by
accident. **The row is bounded**: at most three cells, `min(3, total)`, around the current desktop,
so the widget is as wide with ten desktops as with three. **The name stays**, beside the cells, and
still opens the overview — the indicator's job, which it keeps. The name is the desktop's own if
it has one and its position otherwise (`Desktop 2`), which is what `Super+N` addresses; composition
v2 §2b's mutable `label` is what makes the first possible.

**The rule for which cells**: the current desktop in the middle where it can be — on the first of
three or more it is the first cell and `‹` is disabled; on the last it is the last and `›` is. The
trailing scratch desktop the lifecycle rule keeps is a cell like any other, which is how a person
reaches an empty desktop without a chord. A cell with windows carries a mark; the current one is
in the accent. **An empty desktop's cell has a faint border**, where the design dashes it: the
difference it draws is "nothing here", which the missing mark already says, and a dashed rectangle
is a primitive this toolkit does not have for the sake of one border.

## 8. What the shell needs from the compositor

The actionable output of this document — every one of these is a demand on the substrate, and
several are not in `display-substrate.md` yet:

| Requirement | Why | Status |
|---|---|---|
| **Window roles** — `normal`, `panel`, `popup`, `dialog` | Bars are panels; the menus and the name prompt are popups | Sketched in composition v2 for dialogs; panels and popups make it load-bearing |
| **Panel struts** — reserved edge space | A maximised window must not cover the bars | **Not in the substrate doc** |
| **Global hotkey registration** | `Super+A` opens the Applications menu regardless of focus | **Built** (M8): `Manage::RegisterHotkey` on the manager channel, which only the shell holds |
| **Window thumbnail capture** | The overview (§6) | **Not in the substrate doc**; capability-gated |
| **Window list, focus and title notifications** | The bottom bar's window list | Implied, never specified |
| **Window placement** | Templates already need it | Already required |
| **Desktop membership** | The overview and the switcher | Composition v2 §2a |

**Roles and struts should be settled before Milestone 2 freezes the window protocol.** Retrofitting
a role into a shipped protocol is the kind of change that touches every client.

## 9. Open questions

- ~~**Desktop lifecycle — shelved, not decided.**~~ **Decided 2026-08-26: naming pins it.** GNOME
  3 auto-removes a workspace when it empties; an explicit "new empty desktop" button implies
  desktops live until closed. The two fight, and the argument that used to settle it — saved
  desktops pulling toward explicit lifecycle — went with templates in composition revision 3, so
  the question came back open on its own terms. The answer uses the naming that
  [`ui-composition-model.md`](ui-composition-model.md) §6 already had: an **unnamed**
  empty desktop is removed, a **named** one is kept, and the list always ends with one empty
  unnamed desktop to create into. A scratch desktop costs nothing and cleans itself up; a
  purposeful one survives its last window closing; and a name a user deliberately set is never
  discarded, which was GNOME 3's surprise. Built in Milestone 8 Part D.
- **The system tray is v2.** It is an *inter-process protocol* — applications register icons and
  receive click callbacks — and that is real scope, not a widget.
- **Does the launcher search beyond programs?** Files and open windows are the obvious
  extensions, and each adds an indexing problem.
- **Live thumbnails** as an optimisation. Trigger: the frozen ones being visibly wrong in use.
- **Indicator or switcher** (§7) — decidable empirically after living with the bar.
