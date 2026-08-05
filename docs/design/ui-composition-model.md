# Nitrox: UI Composition Model — Design Notes (Revision 2)

## Status

**Not built.** This describes a subsystem with no code behind it; the build order is
[`display-arm-plan.md`](../planning/display-arm-plan.md). It graduates to `architecture/`
when windows, ports and desktops land (Milestone 6).

This document revises the "User Interface and Shell" section of `os-design-v5.1.md`. That
section was written early, before kernel/system design matured, and its central mechanism —
`WidgetRecord` as a TSM1 stream variant — doesn't hold up under scrutiny. This document
replaces it. Not everything in the old section is wrong: TSM1's core (`Value` enum, `Table`,
`TypedRecord` derive, port-based wiring) is retained as-is. What's revised is how interactive/live
objects (widgets, windows) fit into the model, and what "pipes between windows" actually means.

Shell semantics beyond this — grammar, scripting language, error propagation, the
builtin/external boundary — are still open and are the next topic of discussion.

### Changes in v2 (2026-08-04)

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

## 4. Two kinds of composition, one substrate

"Pipes between windows" stayed vague for a reason: it was being asked to do two different jobs.

| | Streaming composition | Discrete composition |
|---|---|---|
| Shape | Continuous flow of records | Single typed message, dispatched once |
| Example | `cmd1 \| cmd2` | Click a file in a browser → "open this" to an editor |
| Closest existing pattern | Unix pipe | Qt signal/slot, "open with" handler |

Both are instances of the same underlying thing: nodes with typed ports, wired together. The
port-based visual shell already planned for CLI dataflow doesn't need a sibling system for
windows — it needs its scope extended to include long-running window-nodes alongside ephemeral
pipeline-stage-nodes. One graph. Two front-ends onto it (text shell, visual canvas), plus a third
informal one (drag-and-drop, below).

## 5. Window-to-window composition — two tiers

**Tier 1 — zero-setup, ephemeral.** Dragging a file from a browser triggers a live capability
query against visible windows (`QueryCaps`, already defined in `librsproto`'s Meta category) —
"who has an input port shaped like `FileRef`." Valid targets highlight; drop sends one message. No
pre-wiring, nothing persisted. This should feel like ordinary OS drag-and-drop, just driven by
structural type match instead of a hardcoded MIME table. This is the case the dev-environment
example is actually describing day-to-day.

**Tier 2 — durable, inspectable.** "Clicking in the browser always routes to this specific editor
instance, for the rest of this session." A standing port connection between two specific
instances. Surfaced via the visual shell/patch-canvas view — deliberately *not* baked into default
window chrome (connector nubs on every window border all the time is noise for something set up
rarely). Exposed instead through an explicit overlay: window context menu → "show connections," or
similar. Same underlying graph the visual shell already renders — one data model, two views.

**Binding granularity: per-instance**, not per-type. ("Route to *this* editor window," not
"anything that can open text files.")

### 5a. Ports live under the window

A port is a path under the window that owns it — `/dev/draw/3/ports/in/open-file` — served by
the compositor. Three things follow:

- **Discovery is `list`.** The canvas draws connectable nubs by listing a window's ports, and
  `QueryCaps` is answered from the same place rather than through a side channel.
- **The server is not in the data path.** Resolving a path returns a *handle*; after that the
  two programs talk directly. The compositor brokers the introduction and steps out — the same
  shape the shell uses when it wires two pipeline stages together.
- **Per-instance granularity falls out**, because ports hang off the window rather than the
  process. An app with two editor windows has two input ports without anything special.

**The two kinds of composition materialise differently, and should.** §4 calls them one thing
because they are one graph, but their cost profiles differ:

| | Resolution | Why |
|---|---|---|
| **Discrete** (click → "open this") | resolve at send time | User-initiated and rare; always sees current wiring, so a connection made a second ago is live |
| **Streaming** (a pipe between windows) | wire once into a direct channel | Per-message resolution would be absurd for a stream, and the wirer must not become a relay |

**Wiring is `sys_ns_bind`; unwiring is `sys_ns_unbind`.** The desktop shell holds a full-rights
handle to every application's namespace because it *created* those namespaces at spawn (§6),
so installing a connection is one bind into an already-running program. No new mechanism.

**An application cannot compose other applications** — the maintainer's requirement, and it is
structural rather than policy: no application holds a handle to another's namespace, so it has
nothing to bind into. Only the desktop shell does.

**Absence is the fallback.** An unwired output port simply does not resolve, and the program
falls back to the desktop's default handler (§5). That is the same shape as "absence is the
sandbox" everywhere else in this system: nothing is denied, it is merely not there.

**No connection present:** falls back to ordinary desktop default-handler behavior (spawn
whatever's registered as default for the type). Tier 2 wiring is additive, never required to make
the system usable.

## 6. Desktops

**A desktop is a namespace** — a named container of windows, where switching desktops is
rendering a different subtree. That is the structural justification, not an analogy, and it is
why desktops belong in this document at all.

Desktops are served by the **desktop shell**, a separate process from the compositor: the
compositor owns pixels, surfaces, windows, focus and input routing; the desktop shell owns
desktops, the graph, wiring, templates, and spawning applications. Keeping the compositor small
matters because it is what everything visible depends on.

**Dynamic by default, nameable afterwards.** Desktops are created and destroyed on demand — the
GNOME shape — and *naming* one is something a user does later, not a precondition. A scratch
desktop never gets a name or a file; a purposeful one does. That gets KDE's
built-for-a-purpose feel without KDE's fixed slots, and it means **the desktop is fully usable
with none of §7's machinery**: make a desktop, start what you like, place it how you like, wire
it or don't.

Two properties keep it a workspace rather than a cage:

- **A desktop works with an empty graph.** Opening two terminals must not require naming or
  saving anything.
- **Windows move between desktops** — one attribute change (§2a), since membership lives only
  in the compositor.

Three cases are named but not settled: a window on **no** desktop (just created, or mid-move);
**sticky** windows on all desktops, which every DE has and which breaks a plain 1:1; and
**dialogs**, which are genuinely *on* a desktop and should be listed, but which the canvas
should not offer as wirable nodes — a filter by `role`, not a different tree.

## 7. Templates

A saved development environment — terminal + file browser + editor, wired and placed — is a
**template**: a description of which applications to spawn, where their windows go, and how
their ports are wired.

**A template is a file; a desktop is a namespace.** They share a shape, which makes translation
cheap, but they are not the same thing:

| | Template | Live desktop |
|---|---|---|
| Where | `/home/Desktop/code.nxg`, ordinary data | `/dev/desktop/1/`, exists while running |
| Nodes | program + args + geometry | window ids |
| Status | a recipe | what is actually true right now |

Instantiating reads the file, spawns, places and wires. Extracting walks the live desktop and
produces a value — **translating window ids into node descriptions**, which is the point where
"a template is not a snapshot" stops being a principle and becomes a specific transformation.

**Both directions are existing shell verbs**, because a live graph is a readable resource and a
template is ordinary data:

```
open ./code.nxg | desktop                              # instantiate
open /dev/desktop/current/graph | save ~/Desktop/code.nxg   # capture what I have now
```

v1 said "no new save format required". With windows and ports as paths and the graph as TSM1,
that is now literally true — and the first version of this needs **no GUI at all**: a meta-app
can be built and saved from the shell before a canvas exists to draw one.

Four rules that keep templates honest:

- **No live link between a template and its instances.** Extraction produces a new value;
  writing it back over the original is a deliberate act, like save-versus-save-as. A live link
  would mean moving one window silently rewrote the arrangement — the failure mode every
  workspace manager that tried it has had.
- **A template may be instantiated more than once.** Two code desktops for two projects. The
  live desktop's identity is its own (§2b); the template's origin is at most a note in `info`.
- **Most desktops have no template**, and nothing about a desktop's behaviour depends on
  whether it had one.
- **Templates are per-user and profile-relative.** They name programs (`/bin/edit`), so they
  resolve against whoever opens them. Living in `/home/Desktop` makes that a non-issue by
  construction — a template in your home is yours. A template naming a program you do not have
  should fail loudly at instantiation rather than quietly producing a smaller desktop.

**Templates should take parameters.** One template per project is the wrong answer; a
code-editor desktop wants the terminal in *this* project's directory and the editor on *this*
project's tree. `open ./code.nxg | desktop ~/src/nitrox` reads correctly and has an obvious
spelling available, since the shell already has named arguments for `def` calls (shell design
§5b). Flagged rather than specified.

## 8. Open questions carried forward

**Largely resolved in v2: does "windows as namespace-resident resources" overload namespace
semantics?** The worry was churn — windows come and go by the second, and namespace binding is a
supervisor operation. §2a answers it: windows are **paths a server answers for**, not bindings
anyone maintains, so opening a window calls no `sys_ns_bind` and involves no supervisor. It is
the mechanism `/home` already uses. What remains open is narrower and worth watching: whether
serving ports through the compositor makes it a **discovery bottleneck** on a canvas with many
windows.

**The shell questions v1 carried are now answered** by the shell subproject (Milestone 4;
design `docs/spec/shell-language.md`):

| v1 question | Where it landed |
|---|---|
| Error propagation: convention or explicit `try`/`catch`? | Explicit. Failures raise and propagate; `try`/`catch` is an expression, `fail` raises, errors carry a `kind` (§2) |
| Redirection/`save` for structured data | `save`/`open` replace redirection entirely; format from the extension (§4) |
| Builtin vs external boundary | Four categories, resolved by what a thing structurally *can* be (§3) |
| Env vars as non-ambient, namespace-scoped | A TSM1 `Record` on the setup message — not ambient, not inherited (Milestone 3.5) |
| Grammar/syntax | Settled (§8/§9), built, and revised once in light of use |

Still open, from this document:

- **Where port names come from** (§5a) — an application manifest at spawn, or `QueryCaps` asked
  of the live window? Discovery wants the second; wiring a program that has not started yet
  wants the first.
- **A namespace name for the default handler** (§5). "Falls back to ordinary desktop
  default-handler behaviour" is a path the program resolves, so the desktop shell wants a
  binding of its own in every application's namespace.
- **Sticky windows, windows on no desktop, and dialog filtering** (§6).
- **Template parameters** (§7) — the idea is accepted, the spelling is not.
- **What happens to a wired graph when an application crashes.** Does the desktop shell respawn
  and rewire it? The template makes that mechanically possible, and it may be one of the better
  arguments for the whole idea — a meta-app that repairs itself is something a pile of manually
  arranged windows cannot do.
