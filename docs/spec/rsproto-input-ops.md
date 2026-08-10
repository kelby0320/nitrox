# Resource Server Protocol — Input operations

The `Input` category (`op = 0x0Axx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). This is how merged input reaches a
privileged consumer — today the compositor, later a hotkey daemon or a VT switcher.

**Status:** Pre-stabilization. Introduced with display-arm Milestone 3 Part B
(`docs/planning/display-arm-plan.md`). `Events` is defined; hotkey registration and device
enumeration are later milestones and will extend this category. The design is
[`input-subsystem.md`](../design/input-subsystem.md).

## Where it sits

The `input-server` is a **userspace resource server bound at `/dev/input/new`**. It holds
every raw device node (`/dev/input/raw/<n>`, served by the kernel's i8042 driver)
**exclusively**, merges their streams, and forwards the result.

**Authority is the binding**, as everywhere else:

| Path | Held by | What it authorises |
|---|---|---|
| `/dev/input/raw/<n>` | the `input-server`, alone | reading one device unfiltered |
| `/dev/input/new` | the compositor | receiving merged input for the whole machine |
| *(nothing)* | ordinary clients | input arrives only via their Surface session |

That the raw nodes reach nothing but the server is a **constraint on the supervisor**, not a
consequence of this protocol — see `input-subsystem.md` §5, which records the same gap in
`tty-server`'s precedent.

## How a consumer obtains a stream

Resolving **`/dev/input/new`** mints a channel pair: the server keeps its end and hands the
consumer end back as the resolve's answer (`OBJECT_KIND_CHANNEL`). Events then arrive on that
channel.

The **directory-session** pattern, the same one `/dev/draw/new`, `/dev/tty` and
`/log/<principal>` use. The forwarded resolve is the introduction; the channel is the
conversation — and because a connection *is* a channel, the server can serve different
consumers different streams, which is what makes a filtered or blocked input stream for a
sandboxed compositor a construction rather than a feature.

## Operations

### `Events` (`0x0A00`)

**Server → consumer. No reply.** The body is a whole number of 16-byte `InputEvent` records,
back to back; a body that is not a multiple of 16 is malformed.

### The `InputEvent` record

One record, unchanged from the kernel driver to the consumer — there is no translation step
anywhere on this path. Mirrored in `kernel/src/libkern/input.rs` and
`userspace/libkern/src/abi.rs`, both carrying layout asserts, and the constants below are
compared across the boundary by `cargo xtask abi-sync-check`.

| Offset | Size | Type | Field |
|---|---|---|---|
| 0 | 2 | `u16` | `kind` — event class |
| 2 | 2 | `u16` | `code` — code within the class |
| 4 | 4 | **`i32`** | `value` — **signed**; a delta may be negative |
| 8 | 8 | `u64` | `time_ns` — kernel monotonic time **at the interrupt**, not at delivery |

All fields little-endian. The numbering is Linux's `evdev`, deliberately:

| `kind` | Meaning | `code` | `value` |
|---|---|---|---|
| `0x00` `EV_SYN` | group separator | `0` `SYN_REPORT`, `3` `SYN_DROPPED` | records lost, for `SYN_DROPPED` |
| `0x01` `EV_KEY` | key or button | a keycode, or `0x110+` for buttons | `0` release, `1` press, `2` repeat |
| `0x02` `EV_REL` | relative axis | `0x00` `REL_X`, `0x01` `REL_Y`, `0x08` `REL_WHEEL` | signed delta |
| `0x03` `EV_ABS` | absolute axis | reserved | device-space position |

**`REL_Y` is positive-down**, matching screen coordinates — the PS/2 wire reports positive-up
and the driver negates, so exactly one place knows.

**Buttons are keys.** `BTN_LEFT`/`BTN_RIGHT`/`BTN_MIDDLE` (`0x110`–`0x112`) are `EV_KEY`
codes, because a button has the same down/up/held state machine as a key and a separate class
would duplicate it.

**Extensibility lives in the enums, never the layout.** A touchscreen is `EV_ABS` codes that
do not exist yet; a dial is an `EV_REL` code. The struct does not change, so no future device
breaks the wire.

**Batches never split a `SYN` group.** A logical event (a diagonal mouse move is `REL_X`,
`REL_Y`, `SYN`) is always delivered whole, so a consumer accumulating until `SYN_REPORT`
never has to carry state across messages.

## Ordering

**Events are ordered within a batch, not globally.**

Each time the server wakes it drains whatever both devices have ready, sorts that set by
`time_ns` — the kernel's stamp taken *at the interrupt* — and forwards it. Events already
forwarded are never reordered.

This is deliberately weaker than "the server decides the order", and the difference matters:

- **What it guarantees.** A click and a keystroke that happen close together are both
  buffered by the time the server wakes, so they arrive in the order they physically
  happened. That is the case merging exists for — shift-click, and any modifier held across
  a button press.
- **What it does not.** If the mouse's event reaches the server a wakeup later than the
  keyboard's, it is forwarded later, even if its `time_ns` is older.

A global order is unobtainable without holding every event until the slowest device has
spoken, which would add latency to every keystroke to fix an ordering nobody observes.
Consumers that genuinely need a total order over a long window have `time_ns` and can sort.

## Loss

**A consumer that falls behind is told, using the mechanism the record format already has.**

Channels are finite (four messages), and input is high-rate; a consumer that stops reading
will eventually have nowhere to put the next batch. When a send would fail, the server
discards the batch and sets a pending-loss flag; the next batch that *does* go out is
preceded by an `EV_SYN`/`SYN_DROPPED` record.

That is the same contract the kernel's per-device ring uses (`input-subsystem.md` §3a), and
it means input needs no separate back-pressure design: **discard plus announce** is already
the protocol's answer to loss, so a slow consumer degrades to a resynchronising one rather
than stalling the server or wedging the machine.

It is worth being explicit that this is *not* the resolution of the Surface protocol's
back-pressure question (`deferred-decisions.md`), which is a different problem: there, a
dropped `Release` is unrecoverable because nothing tells the client its buffer is free again.
Input is recoverable because a `SYN_DROPPED` tells the consumer exactly what to do.

## See also

- [`input-subsystem.md`](../design/input-subsystem.md) — the design: why a server, why this
  record, where each concern lives.
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — the envelope every category shares.
- [`rsproto-surface-ops.md`](rsproto-surface-ops.md) — the Surface category, whose
  directory-session shape this follows.
- [`io-operation.md`](io-operation.md) — how the server reads the raw nodes, including the
  record-stream rules those nodes follow.
