# Nitrox: Input Subsystem

**Status: built — the device path in M3 (2026-08-10), the last two rows in M4 (2026-08-11),
loss reworked so relative motion survives a slow consumer (2026-08-26) — and this document
describes what exists.** The whole path from an interrupt to a keystroke
arriving in a widget runs on every boot:

| Stage | Where |
|---|---|
| i8042 controller, keyboard + mouse, one driver | `kernel/src/drivers/ps2/` |
| Per-device lossy ring with a `SYN_DROPPED` marker | `kernel/src/drivers/ps2/ring.rs` |
| The `InputEvent` record crossing the kernel boundary | `kernel/src/libkern/input.rs` |
| Raw device nodes at `/dev/input/raw/<n>` | `kernel/src/object/kernel_server.rs` |
| The merged stream at `/dev/input/new` | `userspace/input-server/` |
| Device stream → logical events, and the keymap | `userspace/libinput/` |
| Focus, hit-testing, implicit grab, key repeat | `userspace/compositor/src/input.rs` |
| Delivery to a widget | `userspace/libui/src/route.rs` |

`cargo xtask check-input` injects a keystroke and a click over QMP and asserts both reach a
widget, so the path is gated end to end rather than assumed. `cargo xtask check-login` adds the
property that paced injection cannot test: a burst of motion delivered *while* the compositor is
recomposing the whole screen must still put the cursor exactly where the arithmetic says (§7).

**What is specified here and not built:** USB HID (§1 — the seams are placed for it and
nothing is written), and anything above one keyboard and one mouse — touchpads, gestures,
absolute-coordinate devices. `SYN_DROPPED` is produced by the driver and honoured by
`libinput`, but the Surface protocol has **no loss marker**, so a client is not told when the
*compositor's* outbox overflows (§5, and `../rationale/deferred-decisions.md`).

**Graduated from `design/` 2026-08-12**, as Milestone 5's first prerequisite. It had sat in
`design/` since the subsystem finished, which root `CLAUDE.md` tells every session to read as
"not current behaviour" — so a fresh session was told the input path did not exist while it
was running on every boot. Verified against source 2026-08-12. Design settled with the
maintainer 2026-08-06; the reasoning below is unchanged from that pass except where marked,
and it remains the design `display-substrate.md` §5 sketched before there was any reason to
think about a second input device.

**The body was audited against source on 2026-08-12**, in the PR's review. Two claims of
absence still stand and were re-checked rather than assumed: `session-mgr` does bind
`/dev/console` into every session namespace unconditionally (§5's "the precedent this document
leans on does not currently honour it"), and there is no userspace-facing interrupt syscall
(§6).

**Companion documents.** [`display-substrate.md`](../design/display-substrate.md) §5 states the
principles this elaborates — key events not bytes, scancode→keycode in the kernel,
keycode→character in userspace, the compositor owns focus. Nothing there is reversed here;
§5's `KeyEvent` turns out to describe a *different layer* than the device stream, and this
document names both. [`ui-composition-model.md`](ui-composition-model.md) owns what a window
is; input routing terminates there.

## 1. Why this is a subsystem and not a driver

The obvious shape — a PS/2 driver that emits key events, in the kernel, read by the
compositor — is one device away from being wrong. PS/2 is the only input hardware this
system can reach today, and it is the last input hardware anyone will add. What comes next
is USB HID, and after that touchpads with gesture and palm-rejection policy, touchscreens
with absolute coordinates and multitouch slots, and keypads and dials that are none of the
above.

Each of those pushes on a different seam, and the seams are not in the same place:

- **HID report descriptors are a parsing problem**, not a device problem. A descriptor is
  close to a bytecode; parsing one in ring 0 to find out where the X axis lives is an
  attack surface with no upside.
- **Pointer acceleration, tap-to-click and palm rejection are policy**, and the argument is
  already settled in this project for a weaker case: `display-substrate.md` §5 puts
  keycode→character in userspace because "a keyboard layout should not be a kernel
  rebuild". An acceleration curve is more policy-laden than a keymap, not less.
- **Hotplug is a lifetime problem.** USB devices arrive and leave. Something has to hold a
  stable stream across that, and it should not be every consumer.
- **Merging is an ordering problem.** Two keyboards and a touchpad are one logical input
  stream, and exactly one component should decide what order their events happened in.

Putting all of that in the kernel means relitigating the kernel boundary at every device
class. Putting a **merge-and-policy server** in userspace settles it once, and matches what
this system already does with the console.

## 2. The layers

```
        i8042 controller             (later: USB HID, i2c touchpad, …)
       ┌──────┴──────┐
   kbd port      aux port                          kernel: ONE Tier 1 driver
      │ IRQ 1       │ IRQ 12                       ports 0x60/0x64 are shared, so
      ▼             ▼                              this is one driver, two nodes
  /dev/input/raw/0   /dev/input/raw/1              emitting InputEvent records
      │            │
      └──────┬─────┘
             ▼
      ┌──────────────┐                             userspace resource server
      │ input-server │   merge · policy · hotplug  holds every raw node exclusively
      └──────┬───────┘
             │  /dev/input/new  → a per-consumer channel
             ▼
      ┌──────────────┐                             holds /dev/input/new
      │  compositor  │   libinput: triples → logical events, modifier state
      └──────┬───────┘   focus and routing
             │  Surface protocol, on the client's existing session channel
             ▼
         a window                                  libsurface delivers; libinput maps
                                                   keycode+modifiers → text
```

**The two IRQs are not the only way a byte enters the driver, and on the failure that matters
they are not the way at all.** The i8042's line is a level the interrupt controller renders as
an edge, so a byte arriving while the line is already asserted raises nothing; nobody then
empties the one-byte buffer, and the line never falls, so no *later* byte can raise anything
either. The controller and the driver deadlock with a byte in hand. `drivers::ps2::poll`,
called from the timer IRQ dispatcher ahead of the DPC drain, is what breaks that — one status
read per tick, a drain only when the buffer is full. The ISR is the fast path; the sweep is
what makes the fast path's loss recoverable rather than fatal (2026-08-13; see the decision
log).

The lesson generalises past this controller: **a driver for a shared-buffer device with an
edge-derived interrupt needs a recovery path that does not depend on that interrupt.** A USB
HID or i2c touchpad driver arriving later should be designed knowing that, not rediscover it.

**This is the `tty-server` shape.** The kernel owns `/dev/console` — a char `DeviceNode`
fed by COM1's receive interrupt — and a userspace resource server holds it *exclusively*
and serves `/dev/tty`. Input is the same arrangement with more devices below it. That
pattern is built, in use, and documented in
[`console-and-tty.md`](console-and-tty.md). This design reuses it almost
unchanged — with **one** deliberate departure, in §3a: the raw node's ring is event-granular
rather than byte-granular, because a fixed-size record over a ring that drops single bytes
desynchronises permanently.

## 3. One record format, everywhere

A single `#[repr(C)]` record travels from the kernel driver to the compositor unchanged.
There is no translation step and no second format.

```rust
#[repr(C)]
pub struct InputEvent {
    /// Event class: `EV_SYN`, `EV_KEY`, `EV_REL`, `EV_ABS`.
    pub kind: u16,
    /// Code within the class: a keycode, `REL_X`, `BTN_LEFT`.
    pub code: u16,
    /// `0`/`1`/`2` (release/press/repeat) for `EV_KEY`; a signed delta for
    /// `EV_REL`; an absolute position for `EV_ABS`.
    pub value: i32,
    /// Kernel monotonic time **at the interrupt**, not at delivery. Load-bearing from
    /// Part B onward — see below.
    pub time_ns: u64,
}
// 16 bytes, naturally aligned, no padding.
```

**Extensibility lives in the enums, not the layout.** This is the one property the design
is chosen for. A touchscreen is `EV_ABS` codes that did not exist before; a dial is an
`EV_REL` code; a device class nobody has thought of is a `kind` value. The struct never
changes, so no future device breaks the wire — which is precisely the corner a "fat" record
with named `x`/`y`/`modifiers` fields paints you into, because the second device class
either wastes fields or forces a layout change.

**`time_ns` is what makes merging possible, not a convenience for double-click.** §1 calls
merging an ordering problem that exactly one component should decide. The `input-server`
reads two device nodes over *independent* `sys_io_submit` round trips, so without an
interrupt-time stamp the only order available to it is the order its own reads happened to
complete in — which is scheduling noise, and would make a keystroke land before or after a
click depending on how the server was descheduled. Stamping at the interrupt is the only
place the true order still exists. Double-click and key-repeat timing are a second use, and
the reason the field is 8 bytes rather than a counter: they need real time, not sequence.

**The order is per batch, not global — and this document originally claimed more than that.**
Strict global ordering is unobtainable without waiting: to know that no *older* mouse event
is still coming, the server would have to hold every keystroke until the mouse had spoken,
which buys a guarantee nobody asked for at the cost of latency on every key. What the server
actually does is drain both nodes on each wakeup and sort **that set** by `time_ns` before
forwarding; already-forwarded events are never reordered.

That is exactly right for the case merging exists for — a click and a keystroke that happen
together are both buffered by the time the server wakes, so they sort correctly — and it is
honestly weaker than "the server decides the order" implies. A consumer that needs a total
order across a long window must sort for itself, which `time_ns` lets it do. Settled with the
maintainer 2026-08-06; `docs/spec/rsproto-input-ops.md` states it normatively.

The shape is deliberately Linux's `evdev`, numbering included. Twenty-five years across
mice, tablets, touchscreens, accelerometers and game controllers without a layout break is
the strongest available evidence, and matching the numbering means existing knowledge about
keycodes transfers.

**A logical event is a group terminated by `EV_SYN`/`SYN_REPORT`.** A diagonal mouse move
is `REL_X`, `REL_Y`, `SYN`. Consumers must accumulate until the `SYN` rather than acting on
each record, and a batch delivered to a consumer never splits a group.

### 3a. Loss, and why a fixed-size record needs `SYN_DROPPED`

The record is fixed-size and the raw node is a **byte**-granular char device. Those two facts
do not compose safely by default, and getting it wrong is unrecoverable rather than merely
lossy:

- The console's ring drops **one byte** when full (`drivers/console.rs`, `RING_CAP = 256` —
  sixteen `InputEvent`s at this size). A single byte dropped at a non-record boundary
  permanently misaligns the stream: `kind` starts reading the high half of the previous
  `time_ns`, and nothing recovers, because `InputEvent` has no sync word and `EV_SYN` is a
  *value inside* a record rather than a framing marker you can scan for.
- A short read that ends mid-record is ordinary for that mechanism. **Splitting is fine** —
  the reader buffers the remainder — but **dropping is not**.

So the borrow from evdev has to include the half that makes it survivable, and this design
takes both:

1. **The raw node's ring drops whole records, never bytes.** The ring is event-granular; when
   it is full the *oldest* whole record is discarded. This is a departure from
   `drivers/console.rs` and is the one place this subsystem does not reuse the console's
   mechanism unchanged.
2. **A drop is announced.** The driver sets a flag and the next group the consumer receives
   is preceded by `EV_SYN`/`SYN_DROPPED`, evdev's marker meaning *discard your accumulated
   state and resynchronise from the next `SYN_REPORT`* — because after a loss the consumer's
   idea of which keys are held is stale, and silently carrying on is how a phantom held
   modifier survives for the rest of a session.

   **What resynchronising cannot do is recover motion**, and this paragraph used to imply
   otherwise by listing "where the pointer is" among the things a consumer re-derives. It has
   nowhere to re-derive it from: a relative delta *is* the position, so a lost `REL_X` is a
   distance no later record mentions. The driver's ring is not where that bites — it drops the
   oldest records under a flood the server is always draining — but the server→consumer
   direction is, and §7 says what happens there now.

§7's overflow question covers the server→consumer channel. This is the harder half —
driver→server — and it is settled here rather than deferred, because it is a property of the
record format rather than of a policy.

**Costs, stated plainly.** One logical event is several records, so every consumer needs a
small state machine — that is what `libinput` exists to be. Multitouch will need slot
semantics bolted on, as it was for evdev, because grouping alone cannot express "which
finger". Neither cost is avoided by a fat record; they are deferred by it, and paid with a
wire break.

## 4. Where each concern lives

| Concern | Where | Why there |
|---|---|---|
| Port I/O, IRQ, controller state machine | kernel driver | Port I/O is ring 0; the i8042's one-byte output buffer needs a prompt ISR — *and* a tick-driven sweep for the bytes that buffer loses, see §2 |
| Scancode → keycode | kernel driver | One small table every consumer would otherwise duplicate; getting it wrong is a bug, not a preference (`display-substrate.md` §5) |
| Merging device streams; hotplug; device policy (acceleration, tap-to-click) | `input-server` | Policy, and it needs to see every device at once |
| Triples → logical events; modifier state; click/drag synthesis | `libinput` | Consumer-side interpretation, shared by the compositor and any future privileged consumer |
| Keycode + modifiers → text (layouts, dead keys, compose) | `libinput` | Policy and data; a layout must not be a rebuild of anything |
| Focus, hit-testing, routing to a window | compositor | It owns stacking; routing anywhere else needs a second copy of that state |
| Delivering input events to a window's event queue | `libsurface` | It arrives on the Surface session channel, alongside `Release` — that is already `libsurface`'s job |

### 4a. `libinput` is not the client's input library

Worth stating because the opposite is the natural assumption. **A client never reads the
input stream.** Input reaches a window over its Surface session channel, routed by the
compositor. The consumer of `/dev/input` is the compositor, and later a hotkey daemon, a VT
switcher, or a test harness — all privileged.

`libinput` is therefore used at *both ends for different reasons*: the compositor uses it to
interpret the device stream, and a client uses it to turn a delivered keycode into text. It
owns **interpreting** input; `libsurface` owns **transporting** it to a window. Same layer as
`libsurface` and `libdraw`, and `libsurface` may depend on it — the tree already has that shape, since
`libsurface` imports `libdraw`.

### 4b. Two `KeyEvent`s, and neither is wrong

`display-substrate.md` §5 specifies `KeyEvent { keycode, pressed, modifiers }`. That is the
**Surface-layer** event — compositor to client — and it survives exactly as written. The
**device layer** is `InputEvent` triples, which have no modifiers field because shift and
control are ordinary key events there.

The bridge is `libinput`'s modifier tracking: the compositor accumulates modifier state from
the device stream and stamps it onto the Surface-layer event it forwards. Modifier policy
(which key is Meta, what Caps Lock does) then sits next to keymap policy instead of two
layers away.

## 5. Namespace and authority

| Path | Served by | Held by |
|---|---|---|
| `/dev/input/raw/<n>` | kernel, one char `DeviceNode` per device | `input-server`, exclusively |
| `/dev/input/new` | `input-server` | the compositor |
| *(nothing)* | — | ordinary clients |

**Authority is the binding**, as everywhere else. Three tiers fall out for free:

- Holding a raw node is the authority to read one device unfiltered — the input-server's
  alone, granted by init at spawn.

  **This is a constraint on the supervisor, not a consequence of the design**, and it is
  worth stating because the precedent this document leans on does not currently honour it.
  `tty-server` holds `/dev/console` exclusively by convention: `session-mgr` still binds the
  raw device into *every* session namespace unconditionally, a bind left vestigial when the
  shell moved to `/dev/tty`. Nothing in the mechanism prevented it. The keylogging argument
  below is only as true as the binding discipline, so the raw nodes are bound **only** into
  the input-server's namespace, and a session's `/dev` projection does not carry them. That
  holds today: the kernel binds `/dev/input/raw/<n>` into the root namespace at boot, and
  `init` hands the input-server a namespace handle through which it resolves them.
- Holding `/dev/input/new` is the authority to receive merged input for the whole machine.
  The compositor has it. Nothing else does, which is what makes keylogging a namespace
  question rather than a permission check.
- A client has neither, and receives only what the compositor routes to its own windows.

Resolving `/dev/input/new` mints a per-consumer channel, exactly as `/dev/draw/new` does —
the directory-session pattern, which by now is the house style rather than a novelty
(`/log/<principal>`, `/dev/tty` and `/dev/draw/new` all mint on resolve, and `profile-server`
and `fs-server` use the directory-session form for `/bin` and `/home`). That the server can serve *different*
consumers *different* streams is what makes a filtered or blocked input stream for a
sandboxed compositor a construction rather than a feature.

## 6. Deliberately not now

- **Tier 2 (a fully userspace driver).** The right destination — `InterruptObject` was
  designed for it — but unreachable today: there is no userspace-facing interrupt syscall
  and no port-I/O capability (no IOPL, no TSS I/O bitmap), and granting port I/O per process
  is its own capability-model question. This design is what will later host such a driver
  without the compositor noticing, because the record format and `/dev/input/new` do not
  change.
- **USB HID, hotplug, multitouch slots, gesture recognition.** All land in the
  `input-server` or `libinput` when there is hardware to justify them. None requires a
  kernel change, which is the point of the arrangement.
- **Key repeat.** ~~Belongs in `libinput`~~ — **decided 2026-08-10: the compositor generates
  it** (M4 Part C, [`widget-toolkit.md`](widget-toolkit.md) §9.2). It cannot live in
  `libinput`: that crate is pure and issues no syscalls, so it has nowhere to put a timer.
  The compositor knows which window has focus, so a repeat stops on a focus change without
  any client involvement. The `value == 2` case in the record is still what carries it.

## 7. Questions this document opened, and where they landed

Two of the three were answered by building it. Kept with their answers rather than deleted,
because "why is it like this" is what a reader arrives with.

- ~~**Batch size and overflow.**~~ **Answered, in two places.** The `input-server` merges both
  devices into one batch ordered by `time_ns`, never splits a group across batches, and on
  overflow **defers** the batch rather than discarding it: relative motion is summed and
  re-emitted in front of the next batch that goes out, and the records that cannot be recovered
  that way — keys, buttons — are announced with `SYN_DROPPED` carrying the count of *records*
  lost, the same contract as the driver's ring (§3a). A consumer therefore has one rule for
  gaps, and no rule at all for movement, which simply arrives late.

  **This discarded whole batches until 2026-08-26**, and the cost was a cursor that drifted
  from the host pointer by the movement thrown away while the compositor was repainting — until
  edges of the screen could not be reached at all. The reasoning that a slow consumer "degrades
  to a resynchronising one" is true only of state a consumer can re-derive.
  [`rsproto-input-ops.md`](../spec/rsproto-input-ops.md) § Loss is the normative version, and
  `check-login` gates it by injecting a burst across a full-screen recompose and requiring the
  cursor to land on the pixel the arithmetic names.

  A consumer's ring is **sixteen** messages (`CONSUMER_QUEUE_DEPTH`), sized against a repaint
  rather than against the mouse: deferral is what makes motion safe at any depth, but a button
  in an overflowing batch is a click that never happens. Downstream, the compositor's session
  queue went from 4 to **16** with a retry timer and motion coalescing rather than a deeper
  queue alone; that is the back-pressure entry in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md).

  **The half that is still open** is not batch size: the Surface protocol has no loss marker,
  so where `libinput` is *told* about a gap, a client is not. Filed, with a trigger.
- **Absolute coordinates and screen scaling.** Still open. `EV_ABS` values are device-space;
  who maps them to screen space, and where the resolution lives. Not needed until a
  touchscreen, and none of the code below assumes an answer.
- ~~**Whether the input-server or the compositor owns pointer position.**~~ **The compositor
  owns it** — `InputRouter` holds a `Point` and `pointer()` is what draws the cursor and
  hit-tests. The server stays stateless about position and forwards deltas, which is what
  lets it serve a consumer that is not a compositor without inventing a screen for it.
