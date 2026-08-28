# Resource Server Protocol — Surface operations

The `Surface` category (`op = 0x09xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations are how a client
obtains a window and gets its pixels onto the screen: create a window, attach a shared
buffer, commit it with a damage rectangle, and receive it back when the compositor is done.

**Status:** Pre-stabilization. Introduced with display-arm Milestone 2 Part A
(`docs/planning/display-arm-plan.md`); the namespace surface completed in Part B.
`CreateWindow`, `AttachBuffer`, `Commit`, `Release`, `DestroyWindow`, `KeyEvent` and
`PointerEvent` are defined, and
three paths resolve: `/dev/draw/new` for a session, `/dev/draw/<N>/info` for a window's
metadata, and `/dev/draw/manage` for the **manager channel** (M6 Part B, 2026-08-19). A bare `/dev/draw/<N>` and `/dev/draw/<N>/ports/…` do not resolve yet. Thumbnail
capture and window movement are later milestones and will extend this category. Ports are
**unscheduled**: durable window-to-window wiring was cut on 2026-08-21, and what keeps ports —
a command line addressing a running window — wants a shape that has not been designed yet
(`TODO(port-shape-rework)`).

**`KeyEvent` and `PointerEvent` are sent** as of M3 Part C3 (2026-08-10). The compositor
consumes `/dev/input/new`, interprets the stream with `libinput`, and routes it:
keys to the topmost window whose role takes focus, pointer events to the window under the
cursor, with an implicit grab from a press to its release. **The receiving end is built too**
as of M3 Part D (2026-08-10): `libsurface` delivers both records into a per-window event queue, and
`cargo xtask check-input` injects a keystroke and a click over QMP and asserts they reach a
window.

Delivery is **queued and retried, not best-effort**. The compositor holds a bounded per-session
outbox, coalesces pointer motion to at most one pending record per window, and re-sends the
head until the client takes it, so a burst of motion delays a keystroke rather than displacing
it. A client that stalls long enough to overrun that queue loses the oldest records and
**is not told** — the protocol has no loss marker, which is a filed gap
(`../rationale/deferred-decisions.md`).

**A client is now told when its window gains or loses the keyboard** (`FocusEvent`, M4 Part
B), and **a pointer is drawn on screen** (M4 Part C) — one fixed arrow, composited after the
window stack. Per-client cursor shapes, an I-beam over a text area or a resize arrow on an
edge, are a protocol addition and are not built.

## Where it sits

The compositor is a **userspace resource server bound at `/dev/draw` with a subtree
base** — the same binding kind `/home` uses. Window paths are therefore *forwarded
resolves*, not bindings: nobody calls `sys_ns_bind` when a window opens and no supervisor
is in the loop (`docs/architecture/ui-composition-model.md` §2a).

**Authority is the binding.** A process can create windows if and only if `/dev/draw` is
in its namespace. There is no display capability bit and no registration call.

**Window ids are scoped to the connection that created them — on a *session* channel.** A
client may only name its own windows: `AttachBuffer`, `Commit` and `DestroyWindow` against an
id belonging to another connection are `NotFound`, exactly as if the id did not exist. Nothing
about the id space itself enforces this — ids are unique compositor-wide so that a desktop
shell holding `/dev/draw` with broader rights can address any of them — so **the server keeps
the per-connection set** and checks membership before dispatch.

**The manager channel is the deliberate exception, and that is what it is for.** Every op on
it names a window by id and none checks ownership: managing windows one did not create is the
whole capability, and the *binding* is what bounds who may hold it. As of **M7 Part E that binding gates**: an application runs
in a namespace `desktop-shell` built, which binds `/dev/draw/new` **as its own path** with
subtree base `/new`, so `manage` is not a component-boundary prefix match against it and
resolves to nothing there. The shell's own session namespace binds the `/dev/draw` subtree
unscoped and reaches both.

The first-come rule remains: the first resolve of `manage` gets it and a second is refused with
`WouldBlock` rather than served, because two managers placing windows is a race with no arbiter.
It is now a second line of defence rather than the only one — **and the two answers are not
interchangeable**. `WouldBlock` means the resolve *reached the compositor*, so a namespace that
wrongly bound the subtree returns it while a correctly narrow one returns `NotFound` from the
kernel without the compositor ever seeing it. A caller checking whether a namespace withholds
`manage` must require `NotFound` specifically; treating any error as "withheld" makes the check
pass for exactly the mis-construction it is meant to catch (PR #237 review).

In Milestone 6 this gated nothing — `/dev/draw` was bound unscoped into init's root namespace
and every graphical client inherited it, so order was all that separated the intended manager
from the rest. That was tracked as `manage-ungated` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md) and is now in its resolved table.
It still gates nothing for anything spawned with `namespace: 0`, which inherits that root — a
property of the selftest path rather than of the design.

Without that rule, holding `/dev/draw` (which this spec makes the whole of the authority to
create windows) would also be the authority to destroy anyone else's. The composition
model already treats a cross-client *pixel read* as a leak worth capability-gating
(`display-substrate.md` §4b); commit and destroy are the symmetric case and get the
symmetric answer.

**This category adds no kernel surface.** Everything it needs already exists:
`sys_memory_create`, `sys_memory_map`, handle transfer on an IPC message, and
notifications. That is the strongest argument for this shape over the alternatives
(`docs/design/display-substrate.md` §4).

## How a client obtains a connection

Resolving **`/dev/draw/new`** mints a channel pair: the compositor keeps the server end and
hands the client end back as the resolve's answer (`OBJECT_KIND_CHANNEL`). The client then
speaks Surface ops over that channel.

This is not a new mechanism — it is the **directory-session** pattern `profile-server`
already uses for `/bin`, where "there is no distinct directory reply kind: a directory
handle *is* a live channel to the server". The forwarded resolve is the introduction; the
channel is the conversation.

**The connection is the channel**, and that is what makes the ownership rule above
enforceable rather than merely stated: a request's identity is the endpoint it arrived on,
so the compositor never has to ask *who is calling* — it already knows. It also fixes the
lifetime question, since a client going away is a channel closing, which is exactly when
its windows should be destroyed.

## Reading a window's metadata — `/dev/draw/<N>/info`

Resolving **`/dev/draw/<N>/info`**, where `<N>` is a decimal window id, answers with a
**`MemoryObject` of exactly 40 bytes** (`OBJECT_KIND_MEMOBJ`) holding one `WindowInfo`. The
same shape `/dev/framebuffer/info` uses: a resolve answers with an object the caller maps,
not with a message.

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 4 | `u32` | `id` |
| 4 | 4 | `u32` | `width` |
| 8 | 4 | `u32` | `height` |
| 12 | 4 | **`i32`** | `x` — screen coordinate, **signed**; a window may sit at a negative origin |
| 16 | 4 | **`i32`** | `y` — likewise |
| 20 | 2 | `u16` | `role` tag, as in `CreateWindow` |
| 22 | 2 | `u16` | `dock` edge for a panel; otherwise zero |
| 24 | 4 | `u32` | `reserve` for a panel; otherwise zero |
| 28 | 4 | `u32` | `parent` for a popup/dialog; otherwise zero |
| 32 | 4 | `u32` | `desktop` — which desktop the window is on; **`0` means sticky**, on all of them |
| 36 | 4 | `u32` | `flags` — bit 0 `minimized`; all other bits reserved, zero |

All fields are little-endian. A read of fewer than 40 bytes is **refused, not read short**.

**Grown from 32 to 40 bytes in Milestone 8 Part A**, when desktops arrived. `flags` is a
bitfield rather than a `minimized` boolean so the next window state — `maximized`, which
Milestone 9 needs — costs a bit rather than another growth, and so a reader that ignores an
unknown bit degrades to "not minimized" rather than to a length mismatch.

**`width`/`height` report the committed buffer's geometry**, not the size requested at
`CreateWindow` — the request is an aspiration and the commit is a fact, and they may
differ. Before the window's first `Commit` there is no committed buffer, and the
**requested** size is reported instead.

**The object is a snapshot, not a live mapping.** Each resolve mints a fresh object; the
compositor unmaps its own copy before replying and never writes to it again. Values are
current as of the resolve and never change afterwards. Poll by resolving again.

**Any holder of `/dev/draw` may read any window's `info`, including windows it does not
own, and may enumerate windows by walking ids.** This is deliberate, and it is the
system's answer to a question the session channel answers differently — so the split is
worth stating exactly, because the two doors are easy to mistake for one.

- **The namespace is where you ask.** `<N>/info` is the enumeration surface. Ids come from a
  counter starting at 1, so walking them reveals which windows exist, their geometry, and
  their roles. A window manager or desktop shell needs precisely this, and holding
  `/dev/draw` is how it is authorised to have it.
- **The session channel is where you act, and it is scoped.** Every operation names a window
  through a connection, and a window belonging to another connection reports exactly what a
  nonexistent one does. That is **ownership enforcement, not secrecy**: a client learns
  nothing *from acting*, so there is no oracle to walk by attempting operations, and it can
  never reach another client's pixels.

A forwarded resolve carries no connection identity — the namespace hands the compositor a
path, not a caller — so `info` **cannot** be scoped to the owning client without inventing
an identity the protocol does not have. Given that, enumerable metadata is a choice, not an
accident: window geometry is not treated as secret between processes that already share a
screen, while **pixels** are, and stay behind connection-scoped ids and the ownership check
that precedes every mapping.

If a future role holds content whose existence or placement must be hidden from a peer that
already has draw access, **both doors have to change together** — scoping the session
channel alone would achieve nothing while this path answers freely.

## The buffer lifecycle

```
client                                        compositor
  │  CreateWindow { w, h, role }  ────────────►
  │  ◄──────────────────────────  { window }
  │
  │  create MemoryObject, map it, draw
  │
  │  AttachBuffer { window, buffer, geom }  ──►   (handle rides the message)
  │
  │  Commit { window, buffer, damage }  ───────►
  │                                              composite
  │  ◄─────────────  Release { window, buffer }  (the *previous* buffer)
  │
  │  draw into the released buffer, Commit again…
```

**The handle is transferred once, not per frame.** `AttachBuffer` carries the
`MemoryObject` handle in the message's transfer slot; the body describes only how to
interpret that memory. Thereafter `Commit` names the buffer by id.

**`Release` names the buffer that just left the screen**, not the one that arrived.
Releasing the newly committed buffer would hand the client back memory the compositor is
about to read — the tearing this protocol exists to prevent. A client that re-commits the
same buffer gets no release, because it already owns nothing else.

**Rejected: pixels over IPC.** Copying a frame through messages is the obvious
non-starter, named here so nobody proposes it as a simplification. **Rejected:
server-allocated buffers** — that makes the compositor the allocator for every client's
rendering and couples buffer lifetime to compositor policy.

## Which requests reply, and why a client must drain

This is the asymmetry a second implementation gets wrong if it reads only the per-op
sections, so it is stated once, normatively:

| Op | On success | On failure |
|---|---|---|
| `CreateWindow` | reply carrying the window id, **then a `Configure`** — not necessarily adjacent, see below | error reply |
| `AttachBuffer` | **silent** | error reply |
| `Commit` | **silent** | error reply |
| `DestroyWindow` | **silent** | error reply |
| `Release` | server-initiated; never a reply | — |
| `Place`, `Raise`, `Lower`, `RaiseAbove`, `SetFocus`, `Configure` (manager channel) | **empty-body reply** | error reply |

An error reply carries `RS_FLAG_REPLY | RS_FLAG_ERROR` with the **same op and request id** as
the request it refuses. Two consequences a client must handle:

1. **A refusal is not a success.** Matching on `RS_FLAG_REPLY` and the request id alone
   accepts an error body as a result — a client doing that will read a window id out of an
   error code.
2. **Refusals of otherwise-silent ops must be consumed.** They arrive unsolicited from the
   client's point of view, since it was not waiting for anything. A client that never drains
   them accumulates them until its receive path gives out, and the failure then appears at an
   unrelated later request with nothing pointing at the cause.

## Window roles

A window carries a **role**, fixed at creation. Each changes what the compositor does:

| Role | Tag | Behaviour |
|---|---|---|
| `normal` | `0` | An ordinary application window. |
| `panel` | `1` | A bar. Docks to a screen edge, visible on every desktop, and **never takes keyboard focus** — clicking the clock must not steal input from the terminal. Reserves space (below). |
| `popup` | `2` | A menu or modal. Transient, parented, and may extend beyond its parent's bounds; a menu clipped to its window is not a menu. |
| `dialog` | `3` | Parented, on its parent's desktop, and **listed**. The parent carries desktop membership and lifetime, not position — a manager places a dialog as it places any other listed window. |

**Role is immutable.** A change would force the compositor to redo struts, focus policy
and stacking mid-flight. A client that wants a different role creates a different window —
which is what a menu or a dialog already is.

**A popup or dialog must name a parent that exists.** Otherwise the compositor holds a
transient window with nothing to be transient to, and its stacking position is undefined.
Destroying a window destroys everything descended from it, transitively.

### Struts

A panel declares the space it reserves as `dock: Edge` plus `reserve: u32`, **separately
from its geometry**. The compositor subtracts the total per edge from the area it offers
`normal` windows, which is what "a maximised window must not cover the bars" means
concretely (`docs/design/display-substrate.md` §4a).

Declared rather than derived, because the two genuinely differ: a **fullscreen** window
covers a panel's pixels while the panel still reserves that space for *maximised* windows.
Deriving from geometry would also make a partial-width bar, or one that reserves less than
it occupies, inexpressible.

Edges: `0` top, `1` bottom, `2` left, `3` right.

`reserve` is bounded at **65536** and a larger value is rejected. Unbounded, two panels can
overflow the compositor's `u32` accumulator — a panic in a debug build, and in release a
wrap to zero that returns the *full* screen as the work area, silently defeating the clamp
below. The compositor also saturates when summing, so neither a single absurd value nor a
large number of legitimate ones can wrap. Over-reservation clamps the work area to empty
rather than inverting the rectangle.

## Operations

All bodies are little-endian. Offsets are byte offsets from the start of the body.

### `CreateWindow` (`0x0900`)

Request, 24 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `width` |
| 4 | 4 | `height` |
| 8 | 2 | `role` tag |
| 10 | 2 | role aux16 — a panel's `dock` edge; otherwise **zero** |
| 12 | 4 | role aux32 — a panel's `reserve`, or a popup/dialog's `parent`; otherwise **zero** |
| 16 | 4 | `offset_x`, signed — a **popup's** offset from its parent's origin; otherwise **zero** |
| 20 | 4 | `offset_y`, signed — likewise |

**A popup is placed by its creator, and the offset is how.** Only the client knows where the
menu item its popup drops from was drawn, so a manager does not place popups — and they are
exempt from the initial-configure hold below, because there is nobody to wait for.

**A `dialog` is not.** It names a parent, but the parent carries its desktop membership and its
lifetime — not its position. In placement terms a dialog is an ordinary listed window: it lands
at the compositor's default origin, a manager places it, and it is held like a `normal`. Its offset words are written and read as **zero**.

A manager needs nothing **from the client** to place one: `WindowCreated` carries the parent id
and the requested size, and the manager already tracks where the parent is from the geometry
stream — so centring a dialog on its parent is something it can work out for itself.

The offset is **resolved once, against the parent's origin at the moment of creation**. A popup
does not follow its parent afterwards; moving a parent leaves its popups where they were. That
is a deliberate limit of M6 — tracking would mean re-placing children on every parent move,
which is placement policy — recorded as `TODO(popup-follows-parent)`.

The offset is **not clamped**. A popup may be placed partly or wholly off the screen; it is
clipped and nothing more. The compositor does not slide it back into view, because that would
silently disagree with where the client asked for it with no way to say so.

An unknown role tag, a panel docked to an edge that does not exist, and a `reserve` above
the bound are all **rejected** rather than defaulted.

**A connection may hold 64 windows at once**, and the 65th `CreateWindow` is refused with
`InvalidArgument` until one is destroyed. The bound is on windows *held*, not on windows ever created, so a client that
opens and closes menus forever is never eventually unable to open another. It is per
connection: one client reaching it does not affect any other.

Bounded because everything else on this server is, and because until M6 C3 the *client library*
was the bound — a window owned its connection, so a well-behaved client held one and nothing
needed a number. 64 is far above any honest use (a window, its menu, that menu's submenus, a
dialog or two) and far below what would let one client exhaust the compositor.

**Encoders write unused role words zero**, so two otherwise-identical requests are
identical on the wire. Decoders do **not** currently require it: for `normal` both words
are ignored, and for `popup`/`dialog` the aux16 word is, so several encodings map to one
role. That is deliberate room for a future field, not an invariant to rely on — do not
treat a zeroed word as proof of anything.

Reply, 4 bytes: the new `window` id.

**And then a `Configure`, on the same channel.** `CreateWindow` is the one request that produces
two messages: the reply, then the window's first [`Configure`](#configure-0x0908). A client must
read both — see the handshake below — and a compositor must send them **in that order**, because a
client blocked on the reply for its id cannot read anything else until it has it.

**Nothing about the new window precedes its configure.** A window whose configure is held is not
on screen, and the compositor treats that as one property: it is not composited, it is not a focus
candidate, and it cannot be hit by a click. So there is no `FocusEvent` for a window that has not
been configured — the configure is what makes it eligible for one.

**But the configure is not necessarily the very next message on the channel.** A connection
carries records for every window the client owns, so a `PointerEvent`, a `Release` or a focus
change *for one of its other windows* may arrive between the reply and the configure — with a
manager attached the gap is however long the manager takes. A client must read events in a loop
until its configure arrives rather than assuming the message after the reply is it; `libsurface`'s
`Session::create` does exactly that.

### `AttachBuffer` (`0x0901`)

Request, 24 bytes. The `MemoryObject` handle rides the message's transfer slot.

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `window` |
| 4 | 4 | `buffer` — client-chosen, unique within the window |
| 8 | 4 | `width` |
| 12 | 4 | `height` |
| 16 | 4 | `pitch` — bytes per row, **not** `width * 4` |
| 20 | 4 | `format` — `0` = `XRGB8888`; anything else is rejected |

The client picks the buffer id so it can name it in a later `Commit` without waiting for a
reply. A `pitch` too small to hold a row is **rejected**: accepting it would alias rows in
a buffer the client owns, which the compositor cannot detect any other way.

### `Commit` (`0x0902`)

Request, 24 bytes: `window`, `buffer`, then a damage rectangle
(`x`, `y`, `w`, `h`) in buffer coordinates. A zero-area damage rectangle is a valid no-op
commit.

**The damage rectangle is binding on the client** (since 2026-08-12; before that the
compositor ignored it and recomposited the whole screen, so no client could depend on it).
The compositor repaints **only** the named region. A client that commits a buffer differing
from the last one *outside* the rectangle it named leaves stale pixels on screen until
something unrelated forces a repaint — a restack, a neighbour's destroy, the cursor passing
over. The obligation is therefore: **name a superset of what changed.** Naming more than
changed is always safe and merely costs work; naming less is a bug with a delayed and
confusing symptom.

The compositor clips the rectangle to the window's own bounds, so an over-large one is
harmless rather than rejected. That clip is a bound on work — an unclipped rectangle makes
every commit a full-screen recomposite — and **not** an isolation barrier: compositing draws
each surface from its own buffer, so a rectangle covering a neighbour cannot read or write a
neighbour's pixels.

A window whose new buffer is a **different size** from its old one is repainted over the union
of both, whatever it named: the region it vacated cannot be described in coordinates of a
buffer that no longer covers it.

### `Release` (`0x0903`)

Server → client, 8 bytes: `window`, `buffer`. Sent for the buffer that *left* the screen.

### `KeyEvent` (`0x0905`) and `PointerEvent` (`0x0906`)

**Server → client, on the window's session channel. No reply.** How input reaches a window;
the compositor sends them, a client never does.

**These are Surface-layer events, not device records.** The device layer
([`rsproto-input-ops.md`](rsproto-input-ops.md)) carries `InputEvent` triples with a `SYN`
state machine and no notion of modifiers. `libinput` runs that machine on the compositor's
side, so a window receives something already usable. A client that had to accumulate `SYN`
groups to learn a key was pressed would be reimplementing the compositor's job, badly and
once per application.

**Both records name their window**, as `Release`, `FocusEvent` and `Configure` do. A session can
hold several windows — a popup is created on its parent's connection — so a client with a menu
open must be able to tell a click on the menu from a click on the window beneath it. A client
**must** discard a record whose `window` is not one it is handling; `libsurface` does this in
`Session::apply_event`, which routes each record to the window it names.

`KeyEvent`, 12 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `window` |
| 4 | 2 | `keycode` — an `EV_KEY` code, unchanged from the device layer |
| 6 | 2 | `pressed` — `0` up, `1` down, `2` **repeat** |
| 8 | 2 | `modifiers` — `MOD_SHIFT`/`MOD_CTRL`/`MOD_ALT`/`MOD_META`, held **at this transition** |
| 10 | 2 | reserved, zero |

**A held key repeats**, generated by the compositor rather than by each client: it knows
which window has focus, so a repeat stops when focus moves, which no client can observe.
`pressed` is non-zero for a repeat, so a client treating the field as a boolean gets what it
wants without knowing repeat exists.

**Modifiers travel with the key**, which is the whole reason the boundary sits at key events
rather than characters: a byte stream cannot express Shift-Enter, because `\n` is `\n`
whatever was held down (`display-substrate.md` §5).

**Left and right share a bit**, as X11's `ShiftMask` and Wayland's xkb mask do — a client
asking "was shift held" should not have to ask twice. The `keycode` stays distinct, so a
consumer needing the side reads that; adding `MOD_*_R` bits later is additive. A sender must
derive the mask from *which modifier keys are down*: with both shifts held, releasing one
leaves `MOD_SHIFT` set. Clearing the bit per release is the obvious implementation and is
wrong — a tracking obligation, not a layout one.

`FocusEvent`, 8 bytes:

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 2 | `u16` | `focused` — non-zero if this window now has the keyboard |
| 2 | 2 | | reserved, zero |
| 4 | 4 | `u32` | `window` — which window this is about |

**`window` is carried because one session can hold several.** A popup is created on its
parent's connection and takes focus from it, so both halves of that change arrive on the one
channel; without an id a client cannot attribute them, and per-window focus state is exactly
what a toolkit keeps.

`KeyEvent` and `PointerEvent` carry one for the same reason, as of M6 C3. They did not when this
record was written — they were already shipped, so closing the gap was a wire break rather than
two spare bytes — and it was closed when the first client with two windows arrived, which is
what the deferral filed against it predicted.

**A toolkit needs this and cannot derive it.** A caret blinks only when *both* the widget has
focus within its window and the window has the keyboard; those are two facts from two sources,
and they must not share a field. Losing window focus does **not** clear widget focus —
returning to a window has to put the caret back where it was.

`PointerEvent`, 24 bytes:

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 4 | `u32` | `window` |
| 4 | 2 | `u16` | `kind` — `0` motion, `1` button, `2` enter, `3` leave |
| 6 | 2 | `u16` | `button` — a `BTN_*` code on a button event, else zero |
| 8 | 2 | `u16` | `buttons` — every button held: `BTN_LEFT`→bit 0, `RIGHT`→1, `MIDDLE`→2 |
| 10 | 2 | `u16` | `flags` — `POINTER_PRESSED` (bit 0) on a press |
| 12 | 2 | `u16` | `modifiers` — `MOD_SHIFT` and friends, as on `KeyEvent` |
| 14 | 2 | | reserved, zero |
| 16 | 4 | **`i32`** | `x` — window-local, **signed** |
| 20 | 4 | **`i32`** | `y` — window-local, signed |

A pointer record needs its `window` more than a key record does, not less: a key goes to the
focused window, which a client could track from `FocusEvent`, but a pointer record goes to the
window *under the cursor* or to the grab holder, and there is nothing to infer that from.

**`buttons` and `modifiers` are meaningful on every kind**, unlike `button` and `flags` which
are about the transition. A drag is motion with a button held, and shift-click is a click with
a modifier held; a client told `buttons == 0` on motion would have to re-accumulate button
state from the transitions, and one with no `modifiers` field would have to track shift from
`KeyEvent`s — which works only while it also holds keyboard focus, so shift-clicking an
unfocused window would silently behave as a plain click. Both are the per-application
duplication this layer exists to prevent.

**Coordinates are window-local**, so a client can use them without knowing where it sits on
screen and they stay correct when the window moves — which a client is not told about and
should not have to be. They are **signed** because a drag can leave a window: a client reading
them unsigned sees the pointer teleport.

**New interaction kinds are new `kind` values**, not new ops — scroll and touch fit without a
wire change, which is the same reason the device layer's extensibility lives in its enums.

### Which window receives them

- **`KeyEvent` goes to the focused window.** Focus is the topmost window whose role takes it;
  a `panel` never does, so clicking a clock cannot steal input from a terminal
  (`display-substrate.md` §4a). With nothing focusable, keys are dropped rather than sent to
  whatever the cursor happens to rest on.
- **Click-to-focus is implemented.** A press on a window whose role takes focus **raises** it,
  and because focus *is* "topmost focusable", the raise is the focus change — there is no
  second piece of state to disagree with the stack. A press on a `panel` raises nothing, or a
  stray click on a clock would cover a window with no way to get it back.
- **A client is told when its window's focus changes**, by `FocusEvent` (`0x0907`). Sent when
  the answer *changes*, so a raise that does not move focus sends nothing, and both halves go
  out — the window that lost the keyboard is told, and so is the one that gained it. A client
  told only about gaining would keep a caret blinking behind whatever took focus from it.
- **`PointerEvent` goes to the window under the pointer**, topmost first, regardless of focus
  and regardless of role — a panel that cannot take a keystroke can still be clicked. A window
  that is not focused still sees the click that is about to focus it.
- **A press grabs, until its release.** Every pointer event from a press to the release of the
  last held button goes to the window the press landed on, **even after the cursor leaves it**.
  Without this a drag ending outside the window delivers a press with no release, and the
  client believes a button is held forever. This is why the coordinates are signed: mid-drag,
  window-local `x` is routinely negative, and such a record is legitimate rather than corrupt.
  Enter and leave are suppressed for the duration — a window being told the cursor left while
  it is still receiving that cursor's events is two contradictory statements at once — and
  re-derived when the grab ends.
- **Loss resets the grab.** An `Input::Events` batch carrying `SYN_DROPPED` may be the one that
  lost a release, so the compositor ends any grab on it. A grab that outlives its button never
  ends, and every later click would go to the wrong window for the rest of the session.

### `Configure` (`0x0908`)

**Server → client. Unsolicited, `request_id` 0, no reply.** Body, 20 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `window` |
| 4 | 4 | `width` |
| 8 | 4 | `height` |
| 12 | 4 | `x` — top-left in screen coordinates, signed |
| 16 | 4 | `y` — top-left in screen coordinates, signed |

**The size is a request; the origin is applied.** The compositor cannot resize a client's buffer,
because the client allocates it. A client answers by attaching and committing a buffer of that size, or **declines**
by committing whatever it likes — and declining is legal and stays legal: a fixed-size window is
an ordinary thing, and a protocol that required compliance would make every client implement
reflow before it could exist. The compositor composites whatever geometry it is given.

It carries an **origin as well as a size** because a manager's answer to "where does this go" is a
placement, and a client that learned its position through a separate message would be reconciling
two mechanisms that can disagree. The manager-side [`Configure`](#configure-0x0915) therefore
*places* the window as `Place` would — the origin is not a suggestion — and only the size is left
for the client to accept or decline. A `Configure` for a window the user is dragging is refused
with `WouldBlock`, exactly as `Place` is.

The `window` field matters because one connection can hold several windows on one channel — the
same reason `FocusEvent` carries one.

#### The initial-configure handshake — a client obligation

> **A window is not composited until it has been configured.** After `CreateWindow` replies, a
> client **must wait for that window's first `Configure` before its first `Commit`.**

This is normative, and it is the only ordering rule in this category that a client can violate
without an error being reported. A client that commits early is not refused — and **nothing it
commits reaches the screen** until its first `Configure` has been sent. It is enforced rather
than merely asked for, because the alternative to enforcing it is the symptom the rule exists to
prevent: a window painted at the default origin that jumps when the manager places it.

**Why the wait is the client's.** It is what lets a window manager place a window *before* it is
ever seen. The manager is a separate process, so somebody has to wait for it — and the alternative,
the compositor withholding the `CreateWindow` reply until the manager answers, puts a userspace
process on the critical path of every window creation, where a wedged shell would stop clients
from starting at all. Waiting costs the client one round trip at creation and nothing afterwards.

**With no manager attached the compositor answers immediately**, echoing the requested size and
the window's default origin, so the wait is a formality — which is the point: a client written
against this rule needs no change when a manager appears.

**With a manager attached the compositor holds the first `Configure`** — for a `normal` or a
`panel`, the two roles a manager places — and sends `WindowCreated` (see "Manager events" below)
instead. It is released by whichever comes first:

| Trigger | The client is told |
| --- | --- |
| The manager sends `Configure` (`0x0915`) for the window | exactly what the manager asked for — that op carries an origin *and* a size, so one message answers both halves |
| The manager sends any other request naming the window (`Place`, `Raise`, `RaiseAbove`, `Lower`, `SetFocus`) | the window's geometry as it now stands — the placed origin, at the requested size |
| Nothing, for **200 ms** | the requested size at the default origin |
| The manager disconnects | the same, at once rather than after the remaining wait |

**A `popup` is never held.** It is placed by its creator, through the offset in its
`CreateWindow`, so there is nobody to wait for: the compositor sends `WindowCreated` to the
manager *and* the first `Configure` to the client, immediately, exactly as it would with no
manager attached. None of the four triggers above applies to one.

A `dialog` **is** held, like a `normal` — it names a parent but a manager places it, so there is
somebody to wait for.

This is not a refinement — it is the difference between a menu that opens and a menu that takes
200 ms to open, on every use, as soon as a shell is running.

A manager that wants to set position *and* size should use `Configure` rather than `Place`
followed by `Configure`: the `Place` releases the hold, so the `Configure` after it arrives as an
ordinary later configure — after the window has been painted once. This is why `Configure` carries
an origin.

**The deadline is the guarantee that a shell can delay a window but never lose one.** A manager
that never answers costs a launch 200 ms; it cannot leave a client blocked in `Session::create`
forever. A compositor that omitted it would make every client's startup depend on a userspace
process being alive and willing, which is what withholding the `CreateWindow` reply was rejected
for.

**A process that is both the manager and a client must not block on its own window's first
`Configure`** — it would be waiting for an answer only it can give, and would be released only by
the deadline. It must issue `CreateWindow` and service the manager channel separately rather than
using a helper that does both. `libsurface`'s `Session::create` is such a helper.

**This does not apply to a popup**, which is never held — so a shell opening its own menu, the
obvious case, may block on that window's configure like any other client. It applies to every
other role such a process creates, `dialog` included.

**Create a popup only after its parent's first `Configure`.** The offset resolves against the
parent's origin *as it stands at that moment*, so a popup created while its parent is still held
resolves against the default origin — and, being exempt from the hold itself, is composited there
while the parent is still invisible, and stays there when the manager finally places the parent.
A client that waits for its parent's configure before opening a menu cannot reach this;
`libsurface`'s `Session::create` waits, so nothing built on it can.

`libsurface` performs the wait inside `Session::create`, which returns only once the first `Configure`
has arrived; the first one is **not** delivered to the application as an event, because it is
permission to draw rather than an opinion about a drawing. Later ones are.

### `StartMove` (`0x090A`)

Request, 4 bytes: `window` (u32). Asks the compositor to move this window with the pointer until
the button that is holding it comes up. Empty reply body on success.

**The only geometry a client may originate, and it is not a position.** Everything about where a
window sits is the manager's — `Place` and `Configure` are manager ops — and a client asking to be
moved *to* somewhere would be placing itself. This asks for something a client cannot compute: it
does not know where it is on screen, and the answer changes with every pointer motion.

**Refused with `NotFound` unless the caller's window holds the implicit pointer grab.** The grab
is what makes "the user is dragging me" true. Without the check a client could move its window at
any moment, from anywhere, with nobody touching it. `NotFound` rather than a distinct code for
the same reason every other op here answers it: a window belonging to another connection and a
window nobody is pressing are both "not yours to move", and a distinguishable answer is an oracle.

**The offset is taken from where the press landed, not from where the pointer is when this
arrives.** Those are different moments — a client learns of the press, routes it through its own
toolkit, decides it landed on a title bar, and only then sends this — and the pointer keeps moving
in between. The compositor records the pointer's position when it takes the grab, so a window
follows the pointer from the point it was grabbed by however long the round trip took.

A second `StartMove` while one is running **changes nothing** and reports the same success. It is
not a fresh request: the recorded press has not moved but the window has, so rebuilding the drag
from where the window is *now* would apply the distance already travelled a second time. One
gesture holds one grab and is one drag; a second request names the drag that is already running.

(This section said the two requests "compute the same offset", which was false in exactly that
way — corrected 2026-08-27, PR #248 review.)

**A window being moved is not being placed.** While a drag is in flight, `Place` for that window
is refused with `WouldBlock` — not `InvalidArgument`, because the request is well-formed and will
be answerable again in a moment. Refusing beats overriding: a `Place` that landed mid-drag would
be undone by the next motion, so a manager racing the pointer would appear to work.

The move produces **one** `WindowGeometry` event, when it ends. Not one per motion: that queue
does not coalesce and discards its oldest when full, so a long drag would push a `WindowCreated`
off the front of a manager's view of the world.

### `RequestState` (`0x090B`)

Request, 8 bytes: `window` (u32), `state` (u32 — `0` normal, `1` minimised, `2` maximised).
Asks the *manager* to put this window in that state. Empty reply body on success.

**A client cannot do any of these itself, and must not be able to.** Minimising is
[`SetMinimized`](#setminimized-0x0917) and maximising is a [`Configure`](#configure-0x0915) to a
rectangle computed from the work area; both are manager operations, and a client holding either
could put another client's window away or place itself. So this asks, the compositor forwards it
as [`WindowStateRequest`](#manager-events-0x09180x091c-0x091f-0x09220x0923), and the manager
answers with the request it would have sent anyway.

`NotFound` for a window the caller does not own; `InvalidArgument` for a short body or a state
above `2`.

**The reply says the compositor accepted and forwarded it**, not that anything happened. What a
manager does with the question is the manager's — a shell that declines to maximise a window is
behaving correctly, and a client that is told nothing further learns the answer the way it learns
any other geometry, through `Configure`.

**A repeat produces no event.** The compositor remembers the state each window last *asked* for
and drops a request for the same one. This is the only manager event a client's own rate drives,
and the manager's queue does not coalesce and discards its oldest — so a client looping on one
state would otherwise push a `WindowCreated` off the front of a manager's view of the world, the
same argument [`SetTitle`](#titles-and-the-one-variable-length-body) makes for an unchanged
title. Alternation is *not* deduplicated, and should not be: a window asked to maximise and then
to restore has changed state twice.

The compositor keeps no notion of a window *being* maximised — that is a rectangle the manager
restores from, and a second copy here could disagree with it.

### `CloseRequested` (`0x090C`)

**Server → client. Unsolicited, `request_id` 0, no reply.** Body, 4 bytes: `window` (u32).

Somebody holding the manager channel is asking this window to close. **There is no way to refuse
it and none is needed**: a client that wants to ask "save first?" opens a dialog and closes when
that resolves, and a client that ignores this stays open. What the request buys is that the
decision reaches the process holding the work rather than being taken from it.

A shell that means it will follow this with [`Manage::Close`](#requestclose-0x0924-and-close-0x0925) after a grace period.
That is a policy, not part of this contract — a client cannot tell how long it has, and should
not try to.

### `DestroyWindow` (`0x0904`)

Request, 4 bytes: `window`. Destroys the window, its attached buffers, and **every popup or
dialog descended from it** — transitively, not just its direct children. A submenu is a
popup parented to a popup; leaving it alive with a dead parent gives it no defined stacking
position and still lets it take focus.

`NotFound` if the id does not belong to this connection.

## The manager channel (`0x0910`–`0x0917`, `0x091D`, `0x091E`, `0x0920`, `0x0921`, `0x0924`–`0x0925`)

Resolved at `/dev/draw/manage`, one holder at a time — see the scoping note above. Every op
names a window by id and **none checks ownership**; that is the capability. Each replies with an
**empty body** on success and an error reply on failure.

### `Place` (`0x0910`)

Request, 12 bytes: `window` (u32), `x` (i32), `y` (i32). Sets the window's origin in screen
coordinates. `NotFound` if no such window.

**Absolute, and there is no relative `Move`.** A manager computes positions from the work area
and from other windows, so it always knows the answer in screen coordinates; a relative move
would only serve an interactive drag, which needs a grab offset the compositor does not keep.
It comes back with decorations, or not at all.

**It came back with decorations** — as [`StartMove`](#startmove-0x090a), which is a *client*
request rather than a relative `Place`, because the client is the one that knows the user grabbed
its title bar and the compositor is the one that knows where the pointer is. `Place` is refused
with `WouldBlock` for a window that is being dragged.

### `Raise` (`0x0911`), `Lower` (`0x0912`), `SetFocus` (`0x0914`)

Request, 8 bytes: `window` (u32), `other` (u32, ignored). Restacking. `NotFound` if no such
window.

**`SetFocus` is `Raise`.** Focus is a consequence of stacking rather than a field: the
compositor's focus candidate is the topmost focusable window, so "give this window the
keyboard" *is* "raise it", and a separate focus field would be a second piece of state to
disagree with the stack.

### `RaiseAbove` (`0x0913`)

Request, 8 bytes: `window` (u32), `other` (u32). Places `window` directly above `other` in the
stack. `NotFound` if either id is unknown.

### `Configure` (`0x0915`)

Request: the same body as the client-facing [`Configure`](#configure-0x0908) — `window`,
`width`, `height`, `x`, `y`. Asks the window's **client** to adopt that geometry; it is the
manager's half of the handshake a client completes on create.

**The origin takes effect when the compositor accepts it; the size when the client commits it.**
That asymmetry is not a wart: position is the compositor's to decide and size is the client's to
allocate. A manager setting both should use this rather than `Place` followed by `Configure` —
and, before M9 Part B, a manager that did so set only the size, because nothing applied the
origin.

The reply says the compositor accepted the request, **not** that the client has adopted it: the
`Configure` is forwarded to a third party, and whether it arrives is a property of that client's
receive ring. `NotFound` if no such window.

### `SetWindowDesktop` (`0x0916`)

Request, 8 bytes: `window` (u32), `desktop` (u32). Moves the window to that desktop. **`0` is
sticky** — the window is on every desktop. `NotFound` if no such window.

Any non-zero id is accepted. The compositor does **not** know which desktops exist: it stores
the attribute and filters on it, and an id no window is on is simply an empty screen. Which
desktops exist, what they are called, and when they disappear is the desktop shell's, and
nothing here can be made inconsistent with it.

### `SetMinimized` (`0x0917`)

Request, 8 bytes: `window` (u32), `minimized` (u32 — zero or one; any non-zero value means
minimized). `NotFound` if no such window.

**Minimized is a second attribute, not a reserved `desktop` value.** A minimized window is
still *on* its desktop: it restores there and it belongs in that desktop's window list, so
folding the two would make restoring a guess about where the window came from.

### `SetCurrentDesktop` (`0x091D`)

Request, 4 bytes: `desktop` (u32). Switches which desktop is composited.

**`0` is refused** with `Malformed`, and it is the only value this op validates. `0` means
sticky, so a current desktop of `0` would blank every non-sticky window *and*, by the rule that
a new window is created onto the current desktop, make every window created afterwards silently
sticky.

**Numbered outside the `0x0910`–`0x0917` block on purpose.** Every other manager request names
a window in its first four bytes; this one names none, because it is a property of the screen
rather than of a window. The block is not a category — see the note under
[`Configure`](#configure-0x0915) — but the shape difference is real, and a reader who assumes
"offset 0 is a window id" is right about every request in that range and would be wrong here.

### `RegisterHotkey` (`0x091E`)

Request, 8 bytes: `id` (u32), `mods` (u16), `code` (u16). Asks the compositor to route a key
chord to the manager instead of to the focused window. `Malformed` if the body is short or `id`
is zero; `Rejected` if `id` is already registered or the table is full (**16 entries**).

**The manager picks the `id`**, the way a client picks a buffer id in
[`AttachBuffer`](#attachbuffer-0x0901) — so the reply carries no body and the manager can name
the chord in its own state before the answer arrives. Zero is reserved so a zeroed body cannot
register anything.

**`mods` must match exactly.** `Super+Shift+2` and `Super+2` are different chords, and a
prefix match would make the first fire the second. A chord with `mods == 0` is a bare key.

There is no unregister. A manager holds the channel for its whole life and the table dies with
it; adding one is additive if a manager ever needs to rebind at runtime.

**Why this is a manager request and not a client one.** Any application able to register `Super`
could impersonate the launcher — take the chord that opens the applications modal and show its
own window instead. The capability is holding `/dev/draw/manage`, which is one holder at a time
and which an application's namespace does not bind.

**A matched chord is consumed, not copied.** The focused window receives **no record of it at
all**: not the press, not its release, and not the key repeat a held press would otherwise arm.
Delivering any of them would make every hotkey also type into whatever has the keyboard —
`Super+2` would switch desktops *and* put a `2` in the terminal.

Three rules make that true, and each exists because a simpler version was wrong:

- **The release is swallowed by keycode, not by re-matching.** A user who lets go of `Super`
  before `2` releases a chord that no longer matches, so a compositor that re-tested the
  modifiers on release would deliver a release for a press the window never saw.
- **A key already down cannot begin a chord.** Pressing `2` alone and *then* holding `Super`
  makes the repeat of `2` match — and that press was already delivered, so swallowing its
  release would leave the window a press it never saw released. A chord fires on the transition
  into it.
- **A consumed press arms no key repeat.** Repeat is armed from the physical transition, so
  without this a chord held past the repeat delay delivers its key to the focused window and
  keeps doing so — bypassing routing entirely, since repeats are enqueued to the focused
  session directly.

**Every registered chord is forgotten when the manager channel closes.** The table is routing
policy its holder asked for; left behind, chords would go on being consumed and delivered to
nobody, and a replacement manager would inherit ids it did not choose.

### `QueryLayout` (`0x0921`)

Empty request body. Reply, 24 bytes: `screen_w`, `screen_h` (u32), `work_x`, `work_y` (i32),
`work_w`, `work_h` (u32).

**The first thing a manager needs that it cannot compute.** Every `panel` declares a strut (see
[Struts](#struts)) and only the compositor sees all of them, so a shell subtracting its own bars
is right exactly until some other client declares one — and then maximised windows sit under it
with nothing able to notice. `Place`'s own note has always said "a manager computes positions
from the work area", which was not something a manager could do until this existed.

The work area is the screen minus **every** panel's reservation — including one that is
minimised, on another desktop, or has never committed a buffer. A strut is a declaration about
space rather than a consequence of being drawn, and a work area that grew and shrank as panels
came and went would move every maximised window with it. Clamped rather than allowed to invert:
panels claiming more than the screen leave an empty rectangle, not a negative one.

### `RequestClose` (`0x0924`) and `Close` (`0x0925`)

Request for both: `MgrWindowRef` (`other` unused, zero).

`RequestClose` forwards [`CloseRequested`](#closerequested-0x090c) to the window's client and
answers when it has queued it. **Nothing about the window changes** — this is the polite half,
and it is the one a shell reaches for first.

`Close` **destroys the window**, exactly as [`DestroyWindow`](#destroywindow-0x0904) does for a
client's own window: descendants go with it, and
[`WindowDestroyed`](#manager-events-0x09180x091c-0x091f-0x09220x0923) follows. The client is not
told, because there is nothing it could do with the information that `RequestClose` had not
already offered it. Its next request naming that window — or any descendant that went with it —
is answered `NotFound`, like any other window that no longer exists: the compositor stops
crediting a connection with a window the moment it leaves the stack, whoever removed it.

`NotFound` if no such window; `RequestClose` also answers `NotFound` when no session owns it.

**Why both exist.** A close button a client paints cannot close a client that has stopped
answering — that is the cost of client-side decorations, and this pair is what pays it. A desktop
that only had `Close` would destroy windows out from under processes that were fine; one that
only had `RequestClose` would have no answer for a wedged application except the serial console.

### `Capture` (`0x0920`)

Request, 16 bytes — `window` (u32), `width` (u32), `height` (u32), `pitch` (u32) — **and one
handle**: a writable memory object the *manager* allocated, at least `pitch * height` bytes. The
compositor box-downscales the window's committed buffer into it and replies with an empty body.

`NotFound` if no such window. `Malformed` if the body is short, no handle came with it, or a
dimension is zero. `Rejected` if the object is smaller than `pitch * height`, or if the requested
size is **larger** than the window in either axis — this scales down, and a caller asking to
scale up has misunderstood what it is for.

**A window that has committed nothing yet is `WouldBlock`, not `Rejected`**, and it is the one
answer here worth branching on: it means *try again once it draws*, where every other refusal
means the request was wrong. A freshly launched application has no thumbnail until its first
commit, and a caller that could not tell that from a malformed body would have no reason to
retry.

**The manager allocates, which is the mirror of [`AttachBuffer`](#attachbuffer-0x0901).** There
a client allocates and the compositor reads; here the manager allocates and the compositor
writes. Either way the compositor gains an operation and no allocation policy, which is the same
argument that rejected a scale-transform pipeline for the overview
([`desktop-shell.md`](../architecture/desktop-shell.md) §6).

**The scale is a box average and is fully specified**, because a gate has to be able to say what
the buffer should contain. Destination pixel `(dx, dy)` is the mean of the source rectangle
`[dx·sw/dw, (dx+1)·sw/dw) × [dy·sh/dh, (dy+1)·sh/dh)`, each edge computed independently and each
span at least one pixel wide, with integer division truncating. Bands are derived from edges
rather than from a step so that **every source pixel belongs to exactly one band** — with a step
it is the last rows that fall outside, which in a terminal is where the most recent output is.
`libdraw::scale::box_downscale` is that function. **The compositor calls it; nothing on the host
does yet** — the shell's buffer never leaves the guest, and the only gate that compares pixels
boots an image with no shell in it. The arithmetic is published here so that a gate which *can*
see a thumbnail links this rather than writing its own, and until then the scale is pinned by
`libdraw`'s unit tests.

**Capture is a snapshot, and the thumbnail does not update.** A window drawn after a capture
shows its state at the moment it was taken. That is deliberate — `desktop-shell.md` §6 accepts
it, and live thumbnails are an optimisation with a trigger rather than a v1 goal.

**Capability-gated by being a manager request.** Handing a client another window's pixels is
exactly the leak per-application namespaces exist to prevent, and the manager channel is one
holder at a time.

### What is on screen

A window is composited, focusable, and able to receive pointer events when **all** of:

- it has been configured (see [`Configure`](#configure-0x0908)), **and**
- it is not minimized, **and**
- its `desktop` is `0` or equals the current desktop.

**One predicate, in one place.** These three conditions are a single function in the
compositor rather than a condition repeated at each site, because they are already needed by
compositing, by focus, and by hit-testing, and a fourth site is exactly how a window becomes
invisible-but-clickable.

**A window that stops being on screen loses the pointer grab, and is told.** A press grabs,
and every pointer event up to the matching release goes to the grabbed window even after the
cursor leaves it — so without this, minimizing or switching away from a window mid-drag keeps
delivering motion and the release to a window that is not on screen.

What the departing window receives, in this order:

1. **A `PointerEvent` of kind `POINTER_BUTTON` with `POINTER_PRESSED` clear** — the release
   that closes the sequence, naming the button that opened the grab. The grab exists so a press
   and its release reach one window even when the cursor leaves it; the window going off screen
   ends the grab, and this is the last thing the compositor owes it. A client left holding a
   pointer capture that only a release clears would act on the next press with stale state.
2. **`POINTER_LEAVE`**, if the cursor was inside it.

Then **nothing at all until every button comes up**: the tail of that sequence is not delivered
to whatever is underneath, which would be a release for a press that window never saw. Crossing
events stay suppressed for the same span, exactly as they are during an ordinary drag — a button
is still down, and walking enters and leaves across the windows the cursor passes would announce
a pointer nobody can act on. When the last button is released, input resumes and the window under
the cursor is entered normally.

A window that has been **destroyed** gets none of this: it is unreachable, so its id is simply
forgotten. The suppression above still applies, because the sequence has still lost its owner.

## Manager events (`0x0918`–`0x091C`, `0x091F`, `0x0922`–`0x0923`)

Sent by the compositor **to** the manager channel, unsolicited. They are records, not
requests: there is no reply, and the manager cannot refuse one.

**Delivery is queued, not best-effort.** A manager event is generated while the compositor is
serving some other client's request, so the manager's receive ring may be full at that moment.
The compositor queues the record and retries rather than dropping it — a manager that missed a
`WindowCreated` would hold a window list that is wrong forever, with no resync op to repair it.
The queue is bounded; on overflow the oldest are discarded and the compositor logs the count.

**A manager is told about the future, not the past.** Events are generated as things happen and
sent to whoever holds the channel at that moment; there is no enumeration op and no resync. A
manager that attaches while windows already exist is never told about them, and its list stays
missing them until each is destroyed and recreated. **A manager must therefore attach before any
client creates a window** — which today means being started by the same supervisor that binds
`/dev/draw`, before it binds it to anyone else. This is a real constraint, not an oversight: the
alternative is an enumeration op that has to answer "what is on screen" atomically against a
stack that changes under it, and nothing needs it until a desktop shell can be restarted
independently of the compositor (M7). Until then a manager restart requires a compositor restart.

**Scoped to the whole screen, not to a connection.** Every other op in this document names
windows within the connection that created them; these name windows the manager did not create
and cannot otherwise see. That is the manager channel's purpose and the reason only one exists.

| Op       | Name             | Body                | Meaning                                        |
| -------- | ---------------- | ------------------- | ---------------------------------------------- |
| `0x0918` | `WindowCreated`  | `MgrWindowCreated`  | A window exists, with the role and size its client asked for |
| `0x0919` | `WindowDestroyed`| `MgrWindowRef`      | A window is gone (`other` unused, zero)        |
| `0x091A` | `WindowGeometry` | `ConfigureEvent`    | A window's committed rectangle changed — place *or* commit |
| `0x091B` | `WindowFocus`    | `FocusEvent`        | A window gained or lost the keyboard           |
| `0x091C` | `WindowTitle`    | window id + UTF-8   | A window's title changed — see *Titles* below  |
| `0x091F` | `Hotkey`         | `MgrHotkey`         | A registered chord was **pressed** — see [`RegisterHotkey`](#registerhotkey-0x091e) |
| `0x0922` | `LayoutChanged`  | `MgrLayout`         | The work area is not what it was — see [`QueryLayout`](#querylayout-0x0921) |
| `0x0923` | `WindowStateRequest` | `WindowState`   | A client asked to be minimised or maximised — see [`RequestState`](#requeststate-0x090b) |

`LayoutChanged` is sent when the work area *differs* from the one last announced, rather than on
any particular cause. Today the only causes are a panel appearing and a panel going away — a
window's role, and therefore its strut, is fixed when it is created — so a cause-driven version
would have exactly two triggers and would quietly grow a third the day a strut becomes settable.
A manager that maximised a window against stale numbers would leave it under a panel, and nothing
else would report it.

`Hotkey` is numbered after the block above because it was added later; it is a manager event
like the rest. Its body is the same `MgrHotkey` the registration carried, echoed back — the
manager already knows the chord by the id it chose, and echoing `mods` and `code` costs four
bytes and lets a manager that lost track tell them apart.

`WindowCreated` fires when the window is created, **before** its client has committed anything
— that is what lets a manager place a window before it is first seen.

`WindowDestroyed` fires once per window removed, and **one `DestroyWindow` can produce several**:
destroy is transitive, so a popup goes with its parent and a submenu with that popup. The parent
is reported before the children it took with it. A client disconnecting produces the same records
for everything it still held.

`WindowGeometry` is sent for a move the *manager itself* requested as well as for one it did
not. A manager that assumed its own `Place` needed no confirmation would have two sources of
truth for where a window is; there is one, and it is this event.

**The rectangle is the committed one**, identical to what `/dev/draw/<id>/info` answers with —
the committed buffer's size, not the size named at `CreateWindow`. The two differ whenever a
client commits a buffer of a different size, which is supported. It follows that a **commit can
be a geometry change on its own**: a client that reflows and commits a taller buffer is reported
without any manager involvement. Restricting the event to manager-initiated moves would leave
polling as the only way to see a client resize itself, which is what this event exists to
remove.

`WindowFocus` reports both halves of a transition: the window losing focus, then the window
gaining it. Either may be absent — nothing had focus, or nothing takes it.

## Titles, and the one variable-length body

`SetTitle` (`0x0909`, client → server) and `WindowTitle` (`0x091C`, server → manager) are
**built as of M7 Part A**, closing what M6 Part B3 split off. They share the protocol's first
variable-length Surface record:

| Field | Bytes | Notes |
|---|---|---|
| `window` | 4, little-endian | which window |
| `title` | the rest of the body | UTF-8, at most `MAX_TITLE` = 256 bytes |

**There is no length field.** The body's own length gives the title's, because a Surface body
arrives inside a message that already carries one; a second length would be a way for the two
to disagree.

**Over-length is truncated on a character boundary, not refused.** `SetTitle` is silent on
success and has no reply a client reads, so refusing would need an error path built for the op
that was specified not to have one — and a dropped tail on a title is benign in a way a
dropped message is not. The boundary is the part that matters: cutting at 256 *bytes* can land
inside a multi-byte character, which would leave the title not UTF-8 at all, so a cap meant to
bound memory would corrupt the string it bounds. The compositor logs the first truncation and
then stays quiet.

A body that is not UTF-8, or too short to hold a window id, is `Malformed`. Renaming another
connection's window is `NotFound` — the same answer as a window that never existed, so a reply
cannot be used to probe for other clients' ids. Re-setting an unchanged title produces no
manager event, because the manager queue is bounded and a client that sets its title every
frame would otherwise push older events out of it.

## See also

- [rsproto wire format](rsproto-wire-format.md) — the envelope every category shares
- [display substrate](../design/display-substrate.md) §4 — why surfaces are shaped this way
- [UI composition model](../architecture/ui-composition-model.md) §2a — the namespace shape
- `userspace/librsproto/src/surface.rs` — the encoder/decoder
- `userspace/compositor/src/lib.rs` — the server-side window model
