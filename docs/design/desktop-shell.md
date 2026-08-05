# Nitrox: Desktop Shell — Design Notes (v1)

## Status

**Not built.** This describes a subsystem with no code behind it; the build order is
[`display-arm-plan.md`](../planning/display-arm-plan.md). It graduates to `architecture/`
when the desktop shell lands (Milestone 7).

What a user actually sees and touches: the bars, the applications modal, the overview, and
the desktop indicator. Settled with the maintainer 2026-08-04, with two items deliberately
shelved (§9).

Three documents divide this space:

- **`display-substrate.md`** — mechanism. Framebuffer, surfaces, input, the test gate.
- **`ui-composition-model.md`** — semantics. What a window *is*, ports, desktops,
  templates.
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
argument in the composition model is about *users wiring applications*; the shell's own chrome is
not something anyone wires.

Because each surface is a window with a **role** (§8), splitting later costs nothing at the
compositor. That is the cheap option kept open rather than exercised.

## 4. The applications modal

**One modal, two triggers**: the applications button in the top bar, and the Super key. They open
the same thing, because they are the same intent.

**Its entries are `/bin` programs and `~/Desktop/*.nxg` templates**, and this falls out of
decisions already made rather than being designed here: both are ordinary files in the
namespace, so type-to-filter runs over both with no special mechanism. "Open the code-editor
desktop" is a launcher entry, not a feature.

The Super key means the shell receives a keystroke **regardless of focus** — see §8's global
hotkey requirement, which is a capability rather than an ambient grab.

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

- **Desktop lifecycle — shelved, not decided.** GNOME 3 auto-removes a workspace when it empties;
  an explicit "new empty desktop" button implies desktops live until closed. The two fight, and
  the maintainer has deliberately deferred choosing. Note that saved desktops (composition v2 §7)
  pull toward explicit lifecycle: a template-instantiated desktop vanishing because you closed its
  last window would be surprising.
- **The system tray is v2.** It is an *inter-process protocol* — applications register icons and
  receive click callbacks — and that is real scope, not a widget.
- **Does the launcher search beyond programs and templates?** Files and open windows are the
  obvious extensions, and each adds an indexing problem.
- **Live thumbnails** as an optimisation. Trigger: the frozen ones being visibly wrong in use.
- **Indicator or switcher** (§7) — decidable empirically after living with the bar.
