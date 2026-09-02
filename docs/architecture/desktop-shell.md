# Nitrox: Desktop Shell

## Status

**Partly built, and checked 2026-09-02** — Milestone 7 Part E built the shell and M8 Part C
added its second bar; M12 Part A added dialog placement and made the taskbar's insist a second
click; M12 Part E bound `/dev/clipboard` into every application namespace it constructs, and
Part F gave it the **wallpaper** — a full-screen bottom-most `Role::Panel` with a zero
reservation, holding a PNG the theme names and this shell decodes, because the shell holds
`/home` and a theme where the compositor holds neither (see [`clipboard.md`](clipboard.md) and
`display-arm-plan.md` M12 decision 2);
[`desktop-shell`](../../userspace/desktop-shell) is the code. Graduated from `design/` on
2026-08-25, revision 2.

**This document outruns its code on purpose, so read it section by section.** What is built:
the **top bar** (§3), the **applications modal** (§4) — `/bin` listed through the profile
server, filtered as you type — **launching** (§6's spawn half), each application into a
namespace the shell constructs, and since M10 Part D **launching on somebody else's behalf**
(§4a): a client names a path over `/dev/desktop` and the shell opens it, **placement and window management** (§8), the shell driving
`Place`/`Raise`/`SetFocus` as the compositor's attached manager, and — since M8 Part C — the
**bottom bar** and its **window list** (§2, §7): one entry per `normal` window, click to raise,
click the focused one to minimize, middle-click to close, `Super+H` to minimize without the bar. The toolkit question
§5 settled is answered and built ([`widget-toolkit.md`](widget-toolkit.md)).

Since M8 Part D it also has **desktops** (§7): several of them, created on demand, switched with
`Super+1..4` or by clicking the **indicator** at the bar's end — the bars are *sticky*, so they
are on every desktop rather than on the one they were created on — with the focused window moved
between them by `Super+Shift+N`. The window list shows the current desktop's windows only.
**Naming a desktop is what makes it persist** — an unnamed desktop disappears when its last
window leaves, a named one stays, and the list always ends with one empty desktop to create
into.

Since M8 Part E the indicator opens the **overview** (§6): frozen thumbnails of the current
desktop's windows, a sidebar of the others, and a window moved to another desktop by dropping its
thumbnail on it. The thumbnails are snapshots — `Manage::Capture` scales a window into a buffer
the shell allocated — so a window drawn after the overview opens shows its state at the moment it
opened, which §6 accepts deliberately.

**Its gestures all work as of 2026-08-26, and most of them did not before**: a thumbnail
*dragged* onto a sidebar row moves that window to that desktop (M8 Part E), a thumbnail
*clicked* raises its window and closes the overview, and a sidebar row *clicked* switches to that
desktop — which is what §6 always claimed. It is dismissed by clicking the desktop row you are
already on, by clicking its background (which makes the indicator a toggle, since the overview
covers the bar), by clicking a window, or by Escape — four ways, because with none of the first
three an overview on a desktop with no windows was a dead end. The overview is **sticky** like the bars, so it
survives the switch and re-captures for the desktop arrived at, whether that switch came from its
own sidebar or from a chord; the applications modal and the rename prompt are sticky for the same
reason. Reported from a real session: only the drag had ever been built, and only the drag had
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

What a user actually sees and touches: the bars, the applications modal, the overview, and
the desktop indicator. Settled with the maintainer 2026-08-04, with two items deliberately
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
(§9); and GNOME 2's full desktop switcher on the bottom bar is replaced by a compact indicator
(§7).

## 2. The surfaces

| Surface | Persistent? | Contents | Churn |
|---|---|---|---|
| **Top bar** | yes | workspaces button (left), applications button, clock (centre), tray (right, v2) | low |
| **Bottom bar** | yes | window list, desktop indicator | **high** — every open, close, retitle, focus change |
| **Applications modal** | no | search field, filtered entries | **highest** — the whole list is rebuilt per keystroke |
| **Overview** | no | thumbnails of the current desktop, sidebar of the others | bursty |

The churn column is not decoration: it is what settled the toolkit question in §5.

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

## 4. The applications modal

**One modal, two triggers**: the applications button in the top bar, and the Super key. They open
the same thing, because they are the same intent.

**Its entries are `/bin` programs**, and this falls out of decisions already made rather than
being designed here: they are ordinary files in the namespace, so type-to-filter runs over them
with no special mechanism. (An earlier version also listed `~/Desktop/*.nxg` templates; templates
were cut in composition revision 3.) "Open the code-editor desktop" is a launcher entry, not a
feature.

The Super key means the shell receives a keystroke **regardless of focus** — see §8's global
hotkey requirement, which is a capability rather than an ambient grab.

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

## 7. The desktop indicator

The bottom bar carries a **compact indicator**, not GNOME 2's full desktop switcher.

The switcher was dropped for a reason that only applies here: with **dynamic** desktops it is a
list that changes length, which makes it the churniest widget in the shell, permanently occupying
bar space, for a job the overview already does better.

The indicator shows **the current desktop's name** — which needs no new mechanism, because
composition v2 §2b already gives a desktop a mutable `label` in its `info`, precisely so a
switcher can show something human. A name is also a better use of the space than a row of
numbered boxes. Clicking it opens the overview.

This is **additive**, which is why it is safe to be undecided about: ship the bar, live with it,
and add more if the absence bites. Building a full switcher first commits bar space and a dynamic
list widget to something that may be removed.

## 8. What the shell needs from the compositor

The actionable output of this document — every one of these is a demand on the substrate, and
several are not in `display-substrate.md` yet:

| Requirement | Why | Status |
|---|---|---|
| **Window roles** — `normal`, `panel`, `popup`, `dialog` | Bars are panels; menus and the modal are popups | Sketched in composition v2 for dialogs; panels and popups make it load-bearing |
| **Panel struts** — reserved edge space | A maximised window must not cover the bars | **Not in the substrate doc** |
| **Global hotkey registration** | Super opens the modal regardless of focus | **Not in the substrate doc**; must be capability-gated, or any application could impersonate the launcher |
| **Window thumbnail capture** | The overview (§6) | **Not in the substrate doc**; capability-gated |
| **Window list, focus and title notifications** | The bottom bar's window list | Implied, never specified |
| **Window placement** | Templates already need it | Already required |
| **Desktop membership** | The overview and the indicator | Composition v2 §2a |

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
