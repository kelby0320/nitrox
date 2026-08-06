# Resource Server Protocol — Surface operations

The `Surface` category (`op = 0x09xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations are how a client
obtains a window and gets its pixels onto the screen: create a window, attach a shared
buffer, commit it with a damage rectangle, and receive it back when the compositor is done.

**Status:** Pre-stabilization. Introduced with display-arm Milestone 2 Part A
(`docs/planning/display-arm-plan.md`); the namespace surface completed in Part B.
`CreateWindow`, `AttachBuffer`, `Commit`, `Release` and `DestroyWindow` are defined, and
two paths resolve: `/dev/draw/new` for a session and `/dev/draw/<N>/info` for a window's
metadata. A bare `/dev/draw/<N>` and `/dev/draw/<N>/ports/…` do not resolve yet. Input
events, thumbnail capture, window movement and port wiring are later milestones and will
extend this category.

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
own.** This is deliberate. A forwarded resolve carries no connection identity — the
namespace hands the compositor a path, not a caller — so `info` cannot be scoped to the
owning client without inventing an identity the protocol does not have. What the protocol
gates is **pixels**: buffers are reachable only through connection-scoped window ids, and
the ownership check precedes every mapping. Window *geometry* is not treated as secret
between processes that already share a screen. If a future role holds content whose
existence or placement must be hidden from a peer that already has draw access, that is a
change to this rule and needs its own decision.

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
