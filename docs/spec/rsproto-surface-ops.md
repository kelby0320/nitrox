# Resource Server Protocol — Surface operations

The `Surface` category (`op = 0x09xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations are how a client
obtains a window and gets its pixels onto the screen: create a window, attach a shared
buffer, commit it with a damage rectangle, and receive it back when the compositor is done.

**Status:** Pre-stabilization. Introduced with display-arm Milestone 2 Part A
(`docs/planning/display-arm-plan.md`); the namespace surface completed in Part B.
`CreateWindow`, `AttachBuffer`, `Commit`, `Release`, `DestroyWindow`, `KeyEvent` and
`PointerEvent` are defined, and
two paths resolve: `/dev/draw/new` for a session and `/dev/draw/<N>/info` for a window's
metadata. A bare `/dev/draw/<N>` and `/dev/draw/<N>/ports/…` do not resolve yet. Thumbnail
capture, window movement and port wiring are later milestones and will extend this category.

**`KeyEvent` and `PointerEvent` are sent** as of M3 Part C3 (2026-08-10). The compositor
consumes `/dev/input/new`, interprets the stream with `libinput`, and routes it:
keys to the topmost window whose role takes focus, pointer events to the window under the
cursor, with an implicit grab from a press to its release. **The receiving end is built too**
as of M3 Part D (2026-08-10): `libui` delivers both records into a per-window event queue, and
`cargo xtask check-input` injects a keystroke and a click over QMP and asserts they reach a
window.

Delivery is **queued and retried, not best-effort**. The compositor holds a bounded per-session
outbox, coalesces pointer motion to at most one pending record per window, and re-sends the
head until the client takes it, so a burst of motion delays a keystroke rather than displacing
it. A client that stalls long enough to overrun that queue loses the oldest records and
**is not told** — the protocol has no loss marker, which is a filed gap
(`../rationale/deferred-decisions.md`).

Still missing: no cursor is drawn on screen — the compositor knows where the pointer is and
nothing shows it — and a client is not told when it gains or loses focus. Both are for
Milestone 4, the toolkit being the first thing that needs either.

## Where it sits

The compositor is a **userspace resource server bound at `/dev/draw` with a subtree
base** — the same binding kind `/home` uses. Window paths are therefore *forwarded
resolves*, not bindings: nobody calls `sys_ns_bind` when a window opens and no supervisor
is in the loop (`docs/design/ui-composition-model.md` §2a).

**Authority is the binding.** A process can create windows if and only if `/dev/draw` is
in its namespace. There is no display capability bit and no registration call.

**Window ids are scoped to the connection that created them.** A client may only name its
own windows: `AttachBuffer`, `Commit` and `DestroyWindow` against an id belonging to
another connection are `NotFound`, exactly as if the id did not exist. Nothing about the id
space itself enforces this — ids are unique compositor-wide so that a desktop shell holding
`/dev/draw` with broader rights can address any of them — so **the server keeps the
per-connection set** and checks membership before dispatch.

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
**`MemoryObject` of exactly 32 bytes** (`OBJECT_KIND_MEMOBJ`) holding one `WindowInfo`. The
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

All fields are little-endian. A read of fewer than 32 bytes is **refused, not read short**.

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
| `CreateWindow` | reply carrying the window id | error reply |
| `AttachBuffer` | **silent** | error reply |
| `Commit` | **silent** | error reply |
| `DestroyWindow` | **silent** | error reply |
| `Release` | server-initiated; never a reply | — |

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
| `dialog` | `3` | Parented, on its parent's desktop, listed but **not** offered as a wirable node on the composition canvas. |

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

Request, 16 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `width` |
| 4 | 4 | `height` |
| 8 | 2 | `role` tag |
| 10 | 2 | role aux16 — a panel's `dock` edge; otherwise **zero** |
| 12 | 4 | role aux32 — a panel's `reserve`, or a popup/dialog's `parent`; otherwise **zero** |

An unknown role tag, a panel docked to an edge that does not exist, and a `reserve` above
the bound are all **rejected** rather than defaulted.

**Encoders write unused role words zero**, so two otherwise-identical requests are
identical on the wire. Decoders do **not** currently require it: for `normal` both words
are ignored, and for `popup`/`dialog` the aux16 word is, so several encodings map to one
role. That is deliberate room for a future field, not an invariant to rely on — do not
treat a zeroed word as proof of anything.

Reply, 4 bytes: the new `window` id.

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

`KeyEvent`, 8 bytes:

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | `keycode` — an `EV_KEY` code, unchanged from the device layer |
| 2 | 2 | `pressed` — non-zero if the key went down |
| 4 | 2 | `modifiers` — `MOD_SHIFT`/`MOD_CTRL`/`MOD_ALT`/`MOD_META`, held **at this transition** |
| 6 | 2 | reserved, zero |

**Modifiers travel with the key**, which is the whole reason the boundary sits at key events
rather than characters: a byte stream cannot express Shift-Enter, because `\n` is `\n`
whatever was held down (`display-substrate.md` §5).

**Left and right share a bit**, as X11's `ShiftMask` and Wayland's xkb mask do — a client
asking "was shift held" should not have to ask twice. The `keycode` stays distinct, so a
consumer needing the side reads that; adding `MOD_*_R` bits later is additive. A sender must
derive the mask from *which modifier keys are down*: with both shifts held, releasing one
leaves `MOD_SHIFT` set. Clearing the bit per release is the obvious implementation and is
wrong — a tracking obligation, not a layout one.

`PointerEvent`, 20 bytes:

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 2 | `u16` | `kind` — `0` motion, `1` button, `2` enter, `3` leave |
| 2 | 2 | `u16` | `button` — a `BTN_*` code on a button event, else zero |
| 4 | 2 | `u16` | `buttons` — every button held: `BTN_LEFT`→bit 0, `RIGHT`→1, `MIDDLE`→2 |
| 6 | 2 | `u16` | `flags` — `POINTER_PRESSED` (bit 0) on a press |
| 8 | 2 | `u16` | `modifiers` — `MOD_SHIFT` and friends, as on `KeyEvent` |
| 10 | 2 | | reserved, zero |
| 12 | 4 | **`i32`** | `x` — window-local, **signed** |
| 16 | 4 | **`i32`** | `y` — window-local, signed |

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
  stray click on a clock would cover a window with no way to get it back. **A client is not
  told when it gains or loses focus**; if it paints a focus indicator it must infer this from
  the keys and crossings it receives, which is a gap, not a design (Milestone 4).
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

### `DestroyWindow` (`0x0904`)

Request, 4 bytes: `window`. Destroys the window, its attached buffers, and **every popup or
dialog descended from it** — transitively, not just its direct children. A submenu is a
popup parented to a popup; leaving it alive with a dead parent gives it no defined stacking
position and still lets it take focus.

`NotFound` if the id does not belong to this connection.

## See also

- [rsproto wire format](rsproto-wire-format.md) — the envelope every category shares
- [display substrate](../design/display-substrate.md) §4 — why surfaces are shaped this way
- [UI composition model](../design/ui-composition-model.md) §2a — the namespace shape
- `userspace/librsproto/src/surface.rs` — the encoder/decoder
- `userspace/compositor/src/lib.rs` — the server-side window model
