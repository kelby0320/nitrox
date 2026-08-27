# Resource Server Protocol — Input operations

The `Input` category (`op = 0x0Axx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). This is how merged input reaches a
privileged consumer. **Today's only consumer is `input-testclient`**, in test-harness builds;
the compositor becomes one in Milestone 3 Part C, when focus and routing land, and a hotkey
daemon or VT switcher could later.

**Status:** Pre-stabilization. Introduced with display-arm Milestone 3 Part B
(`docs/planning/display-arm-plan.md`). `Events` is defined; hotkey registration and device
enumeration are later milestones and will extend this category. The design is
[`input-subsystem.md`](../architecture/input-subsystem.md).

## Where it sits

The `input-server` is a **userspace resource server bound at `/dev/input/new`**. It holds
every raw device node (`/dev/input/raw/<n>`, served by the kernel's i8042 driver)
**exclusively**, merges their streams, and forwards the result.

**Authority is the binding**, as everywhere else:

| Path | Held by | What it authorises |
|---|---|---|
| `/dev/input/raw/<n>` | the `input-server`, alone | reading one device unfiltered |
| `/dev/input/new` | the compositor, once M3 Part C lands routing; `input-testclient` today | receiving merged input for the whole machine |
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
| `0x00` `EV_SYN` | group separator | `0` `SYN_REPORT`, `3` `SYN_DROPPED` | **whole records** lost, for `SYN_DROPPED` — the same unit whichever producer sent it, and never counting relative motion, which is carried forward instead (see [Loss](#loss)) |
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

**A consumer that falls behind keeps its movement and is told about the rest.**

Channels are finite (sixteen messages to a consumer), and input is high-rate; a consumer that
stops reading will eventually have nowhere to put the next batch. When a send fails, the server
takes the batch back and splits it:

- **Relative axes** (`REL_X`, `REL_Y`, `REL_WHEEL`) are **summed and carried forward**, then
  re-emitted as one group in front of the next batch that does go out, stamped with the time it
  is sent. Nothing is lost, so nothing is announced about them.
- **Everything else** — keys, buttons, and an upstream `SYN_DROPPED`'s own count — is added to a
  running total, and the next batch that goes out is preceded by an `EV_SYN`/`SYN_DROPPED`
  carrying that total.

The unit is load-bearing: a consumer cannot tell which producer sent a given `SYN_DROPPED`,
so the kernel's per-device ring and the server must count the same thing. Both count whole
records. Counting batches here would have made one field mean two things and left a stalled
consumer under-reporting by the batch size.

**Why the split, in the past tense since 2026-08-26.** This section used to say the server
discards the whole batch, on the reasoning that "a slow consumer degrades to a resynchronising
one". That holds for state a consumer can re-derive — which keys are down, which buttons are
held — and does not hold for a relative axis, where **the delta is the state**. A consumer told
"three records went missing" can reset its modifiers; nothing tells it how far the mouse moved
while it was not listening, and no later event ever will. The compositor's cursor drifted
permanently from the host pointer by exactly the motion discarded while it was repainting, and
the visible symptom was a screen edge that could no longer be reached.

Summing deferred deltas is lossless rather than approximate: addition is what the consumer was
going to do with them, and doing it one layer earlier changes only where the sum is taken. What
is not preserved is the *timing* within a deferred run — the movement arrives as one group
carrying its total, so a consumer that measures velocity sees one fast sample rather than
several slow ones. That is a deliberate trade against losing the distance entirely.

Deferred motion is flushed on the next wakeup, and within `5 ms` if nothing else happens: it is
movement the user has already made, and holding it until the next event would leave a cursor
short of the mouse until the mouse was moved again.

It follows that input still needs no back-pressure design — a consumer that falls behind is
caught up rather than stalling the server — and that this is still *not* the resolution of the
Surface protocol's back-pressure question (`deferred-decisions.md`): a dropped `Release` is
unrecoverable because nothing tells the client its buffer is free again, whereas a deferred
delta is recoverable because the server still holds it.

## See also

- [`input-subsystem.md`](../architecture/input-subsystem.md) — the design: why a server, why this
  record, where each concern lives.
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — the envelope every category shares.
- [`rsproto-surface-ops.md`](rsproto-surface-ops.md) — the Surface category, whose
  directory-session shape this follows.
- [`io-operation.md`](io-operation.md) — how the server reads the raw nodes, including the
  record-stream rules those nodes follow.
