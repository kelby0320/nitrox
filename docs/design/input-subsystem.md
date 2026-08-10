# Nitrox: Input Subsystem — Design Notes (v1)

## Status

**Nothing is built.** No input driver of any kind exists — the only path from a human to
this system today is the serial console's COM1 receive interrupt. This document specifies
the whole path from an interrupt to a keystroke arriving in a window, and is the design
`display-substrate.md` §5 sketched before there was any reason to think about a second
input device. The build order is
[`display-arm-plan.md`](../planning/display-arm-plan.md) Milestone 3. Settled with the
maintainer 2026-08-06.

**Companion documents.** [`display-substrate.md`](display-substrate.md) §5 states the
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
         a window                                  libui delivers; libinput maps
                                                   keycode+modifiers → text
```

**This is the `tty-server` shape.** The kernel owns `/dev/console` — a char `DeviceNode`
fed by COM1's receive interrupt — and a userspace resource server holds it *exclusively*
and serves `/dev/tty`. Input is the same arrangement with more devices below it. That
pattern is built, in use, and documented in
[`console-and-tty.md`](../architecture/console-and-tty.md). This design reuses it almost
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
   idea of which keys are held and where the pointer is are both stale, and silently carrying
   on is how a phantom held modifier survives for the rest of a session.

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
| Port I/O, IRQ, controller state machine | kernel driver | Port I/O is ring 0; the i8042's one-byte output buffer needs a prompt ISR |
| Scancode → keycode | kernel driver | One small table every consumer would otherwise duplicate; getting it wrong is a bug, not a preference (`display-substrate.md` §5) |
| Merging device streams; hotplug; device policy (acceleration, tap-to-click) | `input-server` | Policy, and it needs to see every device at once |
| Triples → logical events; modifier state; click/drag synthesis | `libinput` | Consumer-side interpretation, shared by the compositor and any future privileged consumer |
| Keycode + modifiers → text (layouts, dead keys, compose) | `libinput` | Policy and data; a layout must not be a rebuild of anything |
| Focus, hit-testing, routing to a window | compositor | It owns stacking; routing anywhere else needs a second copy of that state |
| Delivering input events to a window's event queue | `libui` | It arrives on the Surface session channel, alongside `Release` — that is already `libui`'s job |

### 4a. `libinput` is not the client's input library

Worth stating because the opposite is the natural assumption. **A client never reads the
input stream.** Input reaches a window over its Surface session channel, routed by the
compositor. The consumer of `/dev/input` is the compositor, and later a hotkey daemon, a VT
switcher, or a test harness — all privileged.

`libinput` is therefore used at *both ends for different reasons*: the compositor uses it to
interpret the device stream, and a client uses it to turn a delivered keycode into text. It
owns **interpreting** input; `libui` owns **transporting** it to a window. Same layer as
`libui` and `libdraw`, and `libui` may depend on it — the tree already has that shape, since
`libui` imports `libdraw`.

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
  below is only as true as the binding discipline, so Part B must **not** bind
  `/dev/input/raw/*` anywhere but the input-server's namespace, and a session's `/dev`
  projection must not carry it.
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
- **Key repeat.** Belongs in `libinput` (the `value == 2` case exists in the record for it),
  but needs a timer story; deferred until something types.

## 7. Open questions

- **Batch size and overflow.** How many records per rsproto message, and what a consumer
  that stops reading does to the server. The compositor's session channel is four messages
  deep, and PR #175 established that a silently-dropped message is worse than a slow one —
  see the back-pressure entry in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md). Input has the same problem
  and should not solve it differently.
- **Absolute coordinates and screen scaling.** `EV_ABS` values are device-space; who maps
  them to screen space, and where the resolution lives. Not needed until a touchscreen.
- **Whether the input-server or the compositor owns pointer position.** The server sees
  every device; the compositor owns the screen and the cursor. Deltas are unambiguous,
  accumulation is not.
