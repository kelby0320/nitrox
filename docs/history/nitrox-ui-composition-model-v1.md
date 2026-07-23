# Nitrox: UI Composition Model — Design Notes (Revision 1)

## Status

This document revises the "User Interface and Shell" section of `os-design-v5.1.md`. That
section was written early, before kernel/system design matured, and its central mechanism —
`WidgetRecord` as a TSM1 stream variant — doesn't hold up under scrutiny. This document
replaces it. Not everything in the old section is wrong: TSM1's core (`Value` enum, `Table`,
`TypedRecord` derive, port-based wiring) is retained as-is. What's revised is how interactive/live
objects (widgets, windows) fit into the model, and what "pipes between windows" actually means.

Shell semantics beyond this — grammar, scripting language, error propagation, the
builtin/external boundary — are still open and are the next topic of discussion.

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

**Where they live:** namespace-resident. A window is a resource bound into the compositor's
namespace (or the terminal's, for the ANSI/text case). A form's fields are child resources under
the form's own subtree. "Showing UI" becomes structurally identical to mounting a device — bind
the resource where a renderer is watching, and rendering happens because something's listening
there, not because a special record got pushed down a pipe.

**Precedent and caveat:** this is a real extension of the Plan 9 `/dev/draw` idea, not just an
analogy — consistent with the namespace model already being Plan 9-derived. Honest caveat: Plan
9's windowing system was never proven at real-world scale, so this tells us the idea is coherent,
not that it's battle-tested. Go in eyes open.

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

**No connection present:** falls back to ordinary desktop default-handler behavior (spawn
whatever's registered as default for the type). Tier 2 wiring is additive, never required to make
the system usable.

## 6. Saved environments

A saved "development environment" (terminal + file browser + text editor, wired together,
arranged on screen) is a serialized instance of the Tier 2 graph: which nodes to spawn, how their
ports are bound to each other, where windows land. A desktop icon replays it — spawn, wire, place.
No new save format required; it's the same graph the patch canvas already shows.

## 7. Open questions carried forward

- Does "windows/widgets as namespace-resident resources" overload namespace semantics past what
  they're meant for? Flagged, not resolved.
- Shell semantics, not yet addressed in this document:
  - Error propagation model: does a mid-stream `ErrorRecord` halt the pipeline by convention, or
    require explicit `try`/`catch`?
  - Redirection/`save` semantics for structured data going to disk.
  - Builtin vs. external/coreutils boundary — depends partly on how cheap process spawn turns out
    to be.
  - Environment variables as an explicit, non-ambient, namespace-scoped resource rather than
    Unix-style ambient inheritance.
  - Grammar/syntax — deliberately still deferred; semantics first.
