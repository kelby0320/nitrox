# Nitrox: UI Composition Model

## Status

**Revision 3 (2026-08-21) cuts durable window-to-window wiring**, with no replacement — the
old §5 "Tier 2" and §7 "Templates". See "Changes in revision 3" below; the reasoning is worth
reading before proposing anything shaped like it.

**Partly built, and checked 2026-09-01** — graduated from `design/` with Milestone 8, revision 3.

**What is built: desktops (§6), and the split §6 rests on.** The compositor holds a `desktop`
attribute per window and one `current` value and has **no notion of a desktop object** — no list,
no names, no lifecycle — while [`desktop-shell`](../../userspace/desktop-shell) owns all of that.
Membership is one attribute (§2a) and moving a window between desktops is one write. Ids are
stable and never reused (§2b), and the human name lives beside them as metadata — which turned
out to be load-bearing rather than decorative: **naming is what makes a desktop persist**.
Desktops are reachable as a resource at `/dev/desktop`, with a `desktop` command as the proof.

**And window-to-window composition (§5) is built as of M10 Part E** — the one mechanism this
section describes, in the shape the details pass corrected it to: a window declares its acceptors
once (`Surface::DeclareAcceptor`), the compositor matches the table it already holds while the
pointer moves, highlights what would take the payload, and delivers one `Surface::Dropped` if the
gesture ends over it. The payload is a **path**, for the reason §5's own "nothing is persisted"
argument implies: a handle in flight would have to belong to somebody, and a refused transfer has
no clean owner. `nxfiles` offers files and folders; `nxedit` takes files.

**What is not built: ports (§5a), and they are unscheduled** — `TODO(port-shape-rework)`. Naming,
stream-versus-message, what happens when nothing is listening, and which server owns the path are
all unsettled, and revision 3 cut the mechanism they were designed for. §5's window-to-window
composition is therefore intent, not behaviour.

**Nor is `/dev/desktop` the path-per-object namespace §2a sketches.** `new`, `current`,
`N/info` and `N/windows/` are not served: the bare path resolves to a session channel, the way
`/dev/draw/new` and `/dev/tty` do, because the operations that matter are *mutations* and a
resolve is a lookup rather than a call. The per-object paths would duplicate what one `List`
returns, for no consumer.

**A caution the rest of `architecture/` does not need.** §§1–4 describe a model rather than code:
TSM1's data-only rule and the resource-server framing are decisions this system is built on, but
`form` (§3) has never been implemented and the widget-as-resource-server idea in §2 is realised
only at the *window* seam, which §2b's scope note is careful about.

This document revises the "User Interface and Shell" section of `os-design-v5.1.md`. That
section was written early, before kernel/system design matured, and its central mechanism —
`WidgetRecord` as a TSM1 stream variant — doesn't hold up under scrutiny. This document
replaces it. Not everything in the old section is wrong: TSM1's core (`Value` enum, `Table`,
`TypedRecord` derive, port-based wiring) is retained as-is. What's revised is how interactive/live
objects (widgets, windows) fit into the model, and what "pipes between windows" actually means.

Shell semantics beyond this — grammar, scripting language, error propagation, the
builtin/external boundary — are still open and are the next topic of discussion.

### Changes in revision 3 (2026-08-21)

**Durable, per-instance window wiring is cut, and nothing replaces it.** Gone: Tier 2 standing
connections, the patch-canvas view and its "show connections" overlay, the desktop shell's
`sys_ns_bind` wiring, the default-handler fallback that existed to make an unwired port
harmless, and templates as serialised wiring graphs (§7 entire).

**Why.** The whole of Tier 2 was justified by one example — composing a file browser, editor and
terminal into an IDE — and that example did not survive examination:

- **It did not generalise.** Every other candidate that came up (email → calendar, log viewer →
  editor, media player → file browser) wants *ephemeral dispatch*, not a standing connection. You
  do not want every log-viewer click bound to one editor window forever. A dev session is
  unusually long-lived, which is what made the IDE case look natural — and close to the only case
  where durable per-instance wiring is the right shape at all.
- **It did not beat the boring answer.** Any widget toolkit gives you a file tree, tabs and panels
  inside a single application for free. If the pitch is "compose an IDE from independent programs
  rather than building one", the composition machinery has to be worth more than the thing it
  replaces, and it is not.
- **It could not absorb the hard case.** LSP is bidirectional, stateful and high-frequency —
  diagnostics streaming, completion mid-keystroke. Expressing it as ports would mean building
  LSP-specific logic on top of the port abstraction, at which point the generic layer underneath
  earns nothing. A design that needs an escape hatch for its most demanding real integration was
  drawn in the wrong place.

**The lesson, which outlives this document.** Durable, tightly-integrated tool composition is
better served by *building the application* than by general OS-level wiring. Treat "durable
cross-app wiring as an OS primitive" as a pattern to be **skeptical of by default** — not a
prohibition, but a prior to check future proposals against, given it failed to generalise past a
single example here.

**What survives is justified without any composition story**: windows as resource servers (§2),
which buys capability-gated window creation and scriptable window management on its own; ports as
paths under a window (§5a), which a *command line* can address as readily as another window; and
structural drag-and-drop (§5), which is a self-contained improvement on MIME-table dispatch.

### Changes in v2 (2026-08-04)

Kept as history; the section numbers below are v2's own, and revision 3 renumbered §7 and
dropped §8.

The mechanism beneath this document is now specified separately:
**`display-substrate.md`** — framebuffer ownership, the surface protocol, input,
text rendering, and the test gate. This document keeps the semantics; that one has the
pixels. Where they touch, the namespace shape below is authoritative.

What is new or revised here:

- **§2a — the namespace shape**, concretely. Windows are *paths a server answers for*, not
  bindings anyone maintains, which is closer to Plan 9 than v1's phrasing and needs no
  kernel mechanism that does not exist.
- **§2b — identity is not a name.** A path segment is a stable numeric id; human names are
  metadata or filenames. v1 never said this, and the omission produced a desktop called
  `code` that nothing had named.
- **§5a — ports live under the window**, so `list` answers discovery.
- **§6 — desktops**, which v1 did not have at all. A desktop is a namespace, dynamic by
  default, nameable after the fact.
- **§7 — templates**, replacing v1's one-paragraph "saved environments". The distinction
  that matters: a template is a *file* and a desktop is a *namespace*; extraction
  translates between them rather than snapshotting.
- **§8 — five of v1's six carried-forward shell questions are answered** by the shell
  subproject (Milestone 4, design `docs/spec/shell-language.md`), and the namespace-overload
  question is largely resolved by §2a.

## 1. TSM1 stays data-only

The original `Record` variant set included a `widget_tag` alongside `record_tag`/`error_tag`.
Problem: a widget isn't data. It has identity, mutable state, an event stream, a lifecycle.
Putting it in the same enum as a table row means every stream consumer has to ask "is this row
actually alive?" before treating a stream generically. That's a foreign body in the pipe, and no
amount of extending the wire format fixes the underlying category error.

**Revision:** drop `widget_tag`. TSM1 carries data only:

```
Record := record_tag:u8(0x01)  field_values
       |  error_tag:u8(0x02)   ErrorRecord
```

A stream that needs to carry something interactive does so via an ordinary field of type
`Handle`, pointing at a resource server. TSM1 doesn't need to know or care.

## 2. Widgets and windows are resource servers

This follows directly from the system's own core principle — resource servers as the universal
abstraction — which the old `WidgetRecord` design quietly violated. A widget/window is a live,
addressable, stateful thing; it should be a resource, addressed by capability handle, with its
own protocol (state, events, mutation), same as any other resource in the system.

**Where they live:** namespace-resident. A window is a resource the compositor **serves** at a
path (see §2a — v1 said "bound into the compositor's namespace", which implied someone
maintaining bindings; serving is both closer to Plan 9 and cheaper). A form's fields are child
resources under the form's own subtree. "Showing UI" becomes structurally identical to mounting
a device — the resource appears where a renderer is watching, and rendering happens because
something's listening there, not because a special record got pushed down a pipe.

**Precedent and caveat:** this is a real extension of the Plan 9 `/dev/draw` idea, not just an
analogy — consistent with the namespace model already being Plan 9-derived. Honest caveat: Plan
9's windowing system was never proven at real-world scale, so this tells us the idea is coherent,
not that it's battle-tested. Go in eyes open.

### 2a. The namespace shape

The compositor is a userspace resource server bound at `/dev/draw` with a subtree base — the
**same binding kind `/home` already uses**, where the fs-server answers resolves for everything
beneath it. Window paths are therefore forwarded resolves, not bindings: nobody calls
`sys_ns_bind` when a window opens, and no supervisor is in the loop.

```
/dev/draw/                     served by the compositor
├── new                        resolve → a fresh window
├── 1/
│   ├── surface                buffers, damage, present
│   ├── info                   title, role, parent, desktop
│   └── ports/
│       ├── in/…
│       └── out/…
└── 2/…

/dev/window                    the app's own *main* window — the same
                               resource as /dev/draw/N, under the name the
                               app sees

/dev/desktop/                  served by the desktop shell
├── new
├── current
└── 1/
    ├── info                   label, origin
    ├── graph                  live wiring
    └── windows/
        └── 3                  the same resource as /dev/draw/3
```

**`/dev/window` is the rio move.** In Plan 9 it is not a global registry of windows that makes
UI feel like files — it is that rio gives each client a namespace where the standard device
names refer to *that client's* window. Same names, different resources, per process. Nitrox
already does exactly this: `/home` is a different resource per session and `/bin` a different
projection per profile. A window is one more instance of a pattern that is already
load-bearing.

**Two names for one resource is the point, not a duplication.** An app reaches its window as
`/dev/window`; a canvas reaches the same window as `/dev/draw/3`; a desktop re-serves it as
`/dev/desktop/1/windows/3`. Which name you have *is* your relationship to it.

**Window ids are global.** `3` means the same window everywhere. A per-desktop index would
renumber a window's neighbours when it moved and invalidate every saved reference.

**Membership lives in exactly one place: the compositor.** It must know which desktop a window
is on in order to render only the current one, so `desktop` is an attribute in
`/dev/draw/3/info`, and `/dev/desktop/1/windows/` is the desktop shell serving a *filtered
view* rather than keeping a second copy. Moving a window between desktops is therefore a single
attribute change — the old path stops resolving and the new one starts, with nothing to keep in
step.

### 2b. Identity is not a name

**A path segment is a stable numeric identity. Human names are metadata, or filenames.**

- A desktop must be addressable **before** anyone names it — that is the entire dynamic case
  (§6), and v1's implicit assumption that things have names produced a desktop called `code`
  that nothing had named.
- Renaming must not change a path, or every held handle and every saved reference breaks.
- Two desktops instantiated from one template can both be labelled "code" without colliding.
- It matches the rest of the system: a pid is not a name, a handle is not a name.

A user-assigned label lives in `info` — useful for a switcher, load-bearing for nothing.

**Scope boundary (important):** the resource-server boundary sits at the *composable seam* — the
window as a whole, and the specific ports it exposes for composition — not at every pixel inside
it. A single app's internal widget tree stays local and monolithic (consistent with the existing
model-view split: view is monolithic, model has typed ports). Per-keystroke IPC through a full
resource-server round trip would make typing feel terrible; nothing about this design requires
that. Only the parts of a window meant to compose with other programs need to be addressable
resources.

## 3. Interactive input (`form`) under this model

`form { ... }`:
1. Creates a form resource, binds it into whatever namespace location is the current display
   target (terminal or compositor).
2. Awaits the resource's own event stream for a "submitted" event.
3. Writes the resulting value as an ordinary `Record` onto its actual `stdout`.

`form { ... } | next_command` is structurally just a pipe to a slow producer —
`next_command` awaits on a channel same as it would for any pipeline stage. No new mechanism
needed given the async-first syscall model.

**Who gets to create UI at all:** resolved by namespace, not by a new channel type. A process can
create UI if (and only if) its granted namespace includes a mount point for a display surface. No
mount point, no UI — consistent with least-authority: a compromised or careless `sort` can't throw
up a fake prompt because it was never given anywhere to put one. (This supersedes an earlier idea
of a dedicated `display` IPC channel — not needed; the namespace grant does the same job with one
less concept.)

## 4. Two kinds of composition

"Pipes between windows" stayed vague for a reason: it was being asked to do two different jobs.

| | Streaming composition | Discrete composition |
|---|---|---|
| Shape | Continuous flow of records | Single typed message, dispatched once |
| Example | `cmd1 \| cmd2` | Click a file in a browser → "open this" to an editor |
| Closest existing pattern | Unix pipe | Qt signal/slot, "open with" handler |

Revision 2 read these as one graph with two front-ends, a text shell and a visual canvas. The
canvas is cut and the graph with it, and the distinction is more useful without that framing:
they are two *shapes of traffic* over the same substrate — a port that is a path (§5a).

**The shell is a client of both.** A pipeline stage streaming into a window's input port and a
drag dropping one message on it differ in cadence, not in kind: each resolves a path and gets a
handle. That is why ports survive the cut and the graph does not — the substrate is a namespace,
which the system already has, rather than a wiring model, which it would have had to build.

## 5. Window-to-window composition

**One mechanism, deliberately small.** Dragging a file from a browser makes valid targets
highlight; dropping sends one message, once. Nothing is pre-wired, nothing is persisted, no
standing connection is created.

**Built in M10 Part E, and the match is declared rather than queried** — corrected 2026-08-30 by
Milestone 10's details pass, which is where this became a thing to build rather than a thing to
describe. This section
said a drag triggers a *live* `QueryCaps` against visible windows. It does not: a window declares
its acceptors once, and the compositor matches a table it already holds. Milestone 9 spent three
parts establishing that per-gesture traffic to a manager is what to avoid, and a round trip to
every visible window at the start of every drag is a worse version of the same mistake. What the
section claims is untouched — the match is **structural**, against a type rather than a hardcoded
MIME table; what changed is *when* it happens. See `display-arm-plan.md`, Milestone 10.

This should feel like ordinary desktop drag-and-drop, just driven by **structural type match**
instead of a hardcoded MIME table — and that is the whole of its claim. It is an improvement on
how drag-and-drop works elsewhere, not the foundation of a composition system.

**There is no second tier.** Revision 3 cut durable per-instance wiring, the patch canvas and
templates; see the Status section for why, and for the prior it leaves behind. A workflow that
wants tools durably integrated is asking for an application, and should be given one.

**What an acceptor is, now that one exists.** A *name* and a set of kinds, held per window and
cleared with it — and the name is the part that matters for §5a: it is the port's name in
waiting. When ports arrive, `/dev/draw/3/ports/in/document` is the same sink a drag addresses by
ending over it, rather than a second mechanism that happens to look similar. That is why the
protocol carries a string rather than an index, and why a `Dropped` says which acceptor it landed
on even though every window built so far declares exactly one.

**Where on a window a drop is allowed is the client's**, not the protocol's: the event carries the
pointer position and `libui` routes it to the widget under that point, exactly as it routes a
press. `nxedit` takes a file on its text area and not on its title bar, and the compositor knows
nothing about either.

### 5a. Ports live under the window

A port is a path under the window that owns it — `/dev/draw/3/ports/in/open-file` — served by
the compositor.

**Kept, but on a different justification than it was written for.** Revision 2 justified ports as
the thing durable wiring bound into; that is gone. What keeps them is a use case the wiring story
was obscuring rather than serving: **a command line can address a GUI program's port**. Sending a
file to a running editor, or reading a selection out of a browser, is exactly the everything-is-a-
resource claim this system is built on, and it wants a *path* — there is nothing for `QueryCaps`
alone to hand a shell. That the compositor's drag-and-drop uses the same ports is then a
consequence rather than the reason.

Three things follow from ports being paths:

- **Discovery is `list`.** A window's ports are enumerable, and `QueryCaps` is answered from the
  same place rather than through a side channel.
- **The server is not in the data path.** Resolving a path returns a *handle*; after that the two
  ends talk directly. The compositor brokers the introduction and steps out — the same shape the
  shell uses when it wires two pipeline stages together, and what makes a CLI-to-GUI stream
  possible without the compositor relaying it.
- **Per-instance granularity falls out**, because ports hang off the window rather than the
  process. An application with two editor windows has two input ports without anything special.

**An application cannot compose other applications.** This is a requirement rather than an
accident, and it is structural: no application holds a handle to another's namespace, so it has
nothing to reach into. The desktop shell is the sole exception, and it is one by construction —
it holds a full-rights handle to every application's namespace because it *created* that
namespace at spawn (§6). An application cannot construct a namespace, so it can never acquire
what the shell has. Both halves survive the cut untouched — neither was ever about wiring; both
are about who builds a namespace — and together they constrain whatever this section becomes.

**The shape of this is not settled.** What is above was drawn for a mechanism that no longer
exists, and the CLI case has different pressures — naming, whether a port is a stream or a single
message, what happens when nothing is listening, and whether the compositor is the right server
for a path an application defines. Reworking it is a discussion in its own right and has not
happened; nothing here should be read as decided beyond "ports are paths, and the shell is a
first-class client of them" (`TODO(port-shape-rework)`).

## 6. Desktops

**A desktop is a namespace** — a named container of windows, where switching desktops is
rendering a different subtree. That is the structural justification, not an analogy, and it is
why desktops belong in this document at all.

Desktops are served by the **desktop shell**, a separate process from the compositor: the
compositor owns pixels, surfaces, windows, focus and input routing; the desktop shell owns
desktops and spawning applications. Keeping the compositor small matters because it is what
everything visible depends on.

**Dynamic by default, nameable afterwards.** Desktops are created and destroyed on demand — the
GNOME shape — and *naming* one is something a user does later, not a precondition. A scratch
desktop never gets a name; a purposeful one does. That gets KDE's built-for-a-purpose feel
without KDE's fixed slots, and it means **there is nothing to set up first**: make a desktop,
start what you like, place it how you like, name it if it turns out to matter.

Two properties keep it a workspace rather than a cage:

- **A desktop needs no setting up.** Opening two terminals must not require naming or saving
  anything.
- **Windows move between desktops** — one attribute change (§2a), since membership lives only
  in the compositor.

Two cases were named but not settled; **both are settled by Milestone 8's details pass
(2026-08-26)**, in [`display-arm-plan.md`](../planning/display-arm-plan.md):

- **A window on no desktop does not exist.** A window is assigned to the current desktop when it
  is created, and moving it is a single attribute write — so the transient this worried about
  ("just created, or mid-move") is never observable. Removing the state is cheaper than
  specifying what renders it.
- **Sticky is `desktop = 0`**, reserved in the attribute now even though the UI to set it may
  land later. Rendering is `w.desktop == 0 || w.desktop == current`, which is the whole of the
  break in the 1:1 — one comparison, not a second membership model.

**Lifecycle, also settled 2026-08-26: naming pins a desktop.** An unnamed empty desktop is
removed; a named one is kept; the list always ends with one empty unnamed desktop to create into.
This makes "name it if it turns out to matter", above, the lifecycle rule itself rather than a
separate mechanism.

A third — **dialogs** — is settled by revision 3. It was "genuinely *on* a desktop and should be
listed, but the canvas should not offer it as a wirable node", which was a filter by `role`
against a canvas that no longer exists. What remains is the half that was always the substance: a
dialog is on its parent's desktop and is listed. Being listed is now the whole of what
distinguishes it, and it is placed like any other listed window.

## 7. Open questions carried forward

Still open, from this document:

- **The shape of ports** (§5a) — `TODO(port-shape-rework)`. Kept for the command-line case, but
  designed for a mechanism that no longer exists. Naming, stream-versus-message, what happens when
  nothing is listening, and which server owns the path are all unsettled. This is the live
  question in this document.
- ~~**Sticky windows and windows on no desktop**~~ (§6) — **answered 2026-08-26** by Milestone 8's
  details pass: sticky is `desktop = 0`, and a window is never on no desktop. The *dialog
  filtering* half of this question had already gone with the canvas — a `dialog` is distinguished
  by being listed and parented, not by what a canvas declines to offer.

Answered or made moot by revision 3:

- ~~Where port names come from~~ — **a client declares them**, once, as *named acceptors*
  (`open-file` accepts `file`). Revision 3 answered this with "`QueryCaps`, asked of the live
  window"; Milestone 10's details pass (2026-08-30) replaced that for the reason §5 now gives —
  a live query per drag is per-gesture traffic, which is what M9 spent three parts learning to
  avoid. The alternative revision 3 was rejecting, an application manifest read at spawn, is
  still rejected and for its original reason: it existed to wire a program that had not started
  yet, and nothing pre-wires anything now. What a client declares at runtime is neither.
- ~~A namespace name for the default handler~~ — moot. The default handler was the fallback for an
  unwired port.
- ~~Template parameters~~ — moot with templates.
- ~~What happens to a wired graph when an application crashes~~ — moot. It was among the better
  arguments *for* Tier 2, and it went with it.

