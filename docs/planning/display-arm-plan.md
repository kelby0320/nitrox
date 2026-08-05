# Nitrox Display Arm — Subproject Plan

**Status:** 🚧 in progress. The design is settled; this is the order it gets built in.
**Milestone 1 is complete** (2026-08-05): the gate, the framebuffer binding, the
self-hash, and the `screendump` smoke gate. **Milestone 2 is in progress** — Part A (the
surface protocol and the window model) landed 2026-08-05.

## What this is

The build sequence for Phase 4's display arm: framebuffer to compositor to GUI terminal, and
the composition layer above it. The design docs are the source of truth for *what*; this plan is
the source of truth for *the order*.

- **Mechanism:** [`docs/design/display-substrate.md`](../design/display-substrate.md)
  — framebuffer ownership, surfaces, input, text, determinism, the test gate.
- **Semantics:** [`docs/design/ui-composition-model.md`](../design/ui-composition-model.md)
  — windows, ports, desktops, templates.
- **The shell itself:** [`docs/design/desktop-shell.md`](../design/desktop-shell.md)
  — bars, applications modal, overview, and the operations it demands of the compositor.

The typed shell + coreutils subproject ([`shell-coreutils-plan.md`](shell-coreutils-plan.md)) is
the CLI arm of the same phase and is complete through Milestone 4. Several of its deferred
items are unblocked by this one, and they are listed under "What this unblocks" below.

## Governing decisions

1. **The gate lands with the first commit, not after.** No compositing code merges without the
   `Framebuffer` trait and host tests behind it. Everything else in this system has a gate;
   pixels are the one place where "we'll add tests once it works" would actually happen, and
   the design doc's §8 exists to prevent it.
2. **The compositor stays small.** Pixels, surfaces, windows, input routing, focus. Anything
   else — desktops, the wiring graph, templates, spawning applications — belongs to the desktop
   shell. It is the process everything visible depends on.
3. **Serial keeps working throughout.** `test-qemu`, `test-interactive`, `eshell`'s recovery
   path and headless CI all depend on the serial console. The GUI is a *second* backend behind
   the tty server's existing `backend_write` seam, never a replacement. The precedent is
   concrete: when the tty server first held a permanent console read it swallowed
   `session-mgr`'s login input, and that took a failing interactive test to find.
4. **Bitmap font first.** The 8×16 table already in the boot path. A rasterizer — and with it
   the first external crate in userspace — waits for a real trigger (text at more than one
   size), and gets its own discussion.
5. **Compositing is deterministic.** Design constraint, not a testing preference: the gate
   hashes output, so no timestamps, no uninitialised padding, no dependence on commit order.

## Prerequisites

Three pieces of substrate, none of them display *policy*, each needed before the milestone that
names it:

| | What | Needed by |
|---|---|---|
| **P1** | A QEMU **monitor/QMP channel** in `xtask`: `screendump` for the smoke gate, `input-send-event` for injection. Nothing uses a monitor today. | M1 Part D |
| **P2** | The kernel exposes **Limine's framebuffer as a mappable resource**, bound into a namespace. `DeviceNode` models char and block; this is a third shape. | M1 Part B |
| **P3** | A **PS/2 keyboard and mouse driver** in the kernel, emitting key events with modifiers and pointer events. No input driver of any kind exists. | M3 |

## Milestone 1 — pixels, and the gate ✅ complete (2026-08-05)

**Deliverable: the compositor composites a known scene, and the host test and the guest agree
on the hash.** No window, no client, no font, no terminal.

- [x] **Part A — `libdraw`: the `Framebuffer` trait and host compositing tests.** ✅ (2026-08-05) Base, width,
      height, pitch, format; a real implementation and an in-memory one; rect fills, blits,
      clipping. Compositing becomes a pure function over (surfaces, geometry, damage, stacking),
      asserted pixel-exactly on the host. No kernel change, no QEMU. **This part is the gate**,
      and it lands first for that reason.

      It is a *shared* crate from the start: the compositor composites surfaces and a client
      draws into one, and both do the same rect and glyph work. Building it as compositor-only
      would mean writing it twice.
- [x] **Part B — P2: the framebuffer reaches userspace.** ✅ (2026-08-05) Kernel resource + namespace binding;
      a minimal program maps it and fills it. Proves the binding and the geometry hand-off.
- [x] **Part C — the self-hash.** ✅ (2026-08-05) The compositor composites a reference scene from *synthetic*
      surfaces and hashes the result; `test-qemu` adjudicates via the existing `isa-debug-exit`
      verdict; Part A's host test asserts the same constant.
- [x] **Part D — P1: `screendump`.** ✅ (2026-08-05) QMP channel in `xtask`, reference image compared once per
      display change. Catches what a self-hash structurally cannot: wrong base address, wrong
      stride, swapped channels.

**The reference scene needs choosing deliberately** (substrate §8e): overlap, clipping at a
screen edge, and a non-trivial stride. A solid fill would hash fine and prove nearly nothing.

## Milestone 2 — a client with a surface

**Deliverable: a program's pixels appear in a window, verified by hash.**

- [x] **Part A — the surface protocol.** ✅ (2026-08-05) Create, share the `MemoryObject`, `Commit { buffer,
      damage }`, release. Host-tested against the in-memory framebuffer. No new syscalls —
      `sys_memory_create`/`_map`, handle transfer and notifications already exist.
- [ ] **Part B — `/dev/draw` served.** `new`, numbered windows, `info`. The same
      `UserspaceServer` + subtree binding `/home` uses, so window paths are forwarded resolves
      and opening a window binds nothing.

      **Window roles and panel struts land here**, not later: bars are `panel`, menus and the
      applications modal are `popup`, and retrofitting a role into a shipped protocol touches
      every client (substrate §4a).
- [ ] **Part C — `libui`: the client side of the protocol.** Connect, create a window, allocate
      and commit surfaces, receive input, run an event loop. The same role `librsproto` plays for
      the RS protocol — **the protocol gets a library, and clients use it.** If the first app
      hand-rolls this instead, the surface protocol immediately has two implementations and the
      second one lives in an application.
- [ ] **Part D — a test client** built on `libui`, and the gate extends: the scene is now built
      from a real client's committed buffer rather than a synthetic one.
- [ ] **Part E — release semantics**, settled by the first client that double-buffers. A client
      that redraws into a buffer still being read produces tearing that is invisible in testing
      and obvious in use.

## Milestone 3 — input

**Deliverable: a keystroke and a click injected by the harness reach a client and are reported
back.**

- [ ] **Part A — P3: the PS/2 driver.** Key events with modifiers, and pointer events, from a
      char device; scancode→keycode table host-tested.
- [ ] **Part B — focus and routing** in the compositor.
- [ ] **Part C — QMP injection** in `test-interactive`, plus a client that echoes what it
      received. Keycode→character (the keymap) is userspace and host-tested.

**Pointer events are in this milestone, not deferred.** An earlier draft of this plan put them
with window management in a later milestone; buttons and menus need clicks, and the toolkit
(M4) comes before any application, so the mouse is needed here.

## Milestone 4 — the widget toolkit

**Design pass first**: the toolkit gets its own document before this milestone starts, the way
the substrate and the shell did. The forks it has to settle are retained-versus-immediate mode,
the layout model, event routing and focus-within-a-window, how a widget's invalidation becomes a
damage rectangle, and how much of it the terminal actually uses.

**Deliverable: enough toolkit to build the terminal, and no more.**

**Why it comes before the first application rather than being extracted from two.** Extraction
is the right instinct when application #1 is cheap to write — but the terminal is the flagship,
and it needs a text area, menus, and a scrollbar. If it invents those inline we have built the
toolkit anyway: coupled to a terminal, and with a second copy due the moment the file browser
lands. So it ships first, and **the terminal decides how much of it exists**. Minimal is a
requirement, not a compromise.

- [ ] **Part A — the widget tree, layout, and invalidation.** Where a widget marking itself
      dirty becomes a damage rectangle on a surface commit.
- [ ] **Part B — event routing**: hit testing, pointer capture during a drag, and
      **widget-level keyboard focus**, which is a *second* focus concept — the compositor
      decides which window has focus, the toolkit decides which widget within it does.
      Conflating them is the classic source of text arriving in the wrong field.
- [ ] **Part C — the first widget set**, bounded by what the terminal needs: a text area, a
      button, a menu, a scrollbar, and a **custom-drawn widget** escape hatch.

**No ABI question today.** With everything statically linked, the toolkit is an ordinary Rust
crate that applications link. The seam matters when dynamic linking lands — the phase plan
schedules the two together, and notes that two applications each embedding the toolkit share no
pages, which is exactly when it starts to pay.

## Milestone 5 — the GUI terminal (the MVP flagship)

**Deliverable: the shell running in a window, drivable by the harness over QMP.**

- [ ] **Part A — glyph rendering** with the bitmap font, in `libdraw`. Host-tested; the
      text/ANSI render path is pure logic.
- [ ] **Part B — the terminal client**, built on the toolkit: window chrome, menus and scrollbar
      from M4's widget set, with **the grid as a custom-drawn widget of its own**. A terminal's
      selection, wrapping and scrollback semantics are not a text editor's, and bending a
      generic text area to serve both would distort the whole text stack.
- [ ] **Part C — the tty server's second backend.** `backend_write` finally has the compositor
      surface it was built for, and a session chooses serial or GUI. **Both must keep working**
      (governing decision 3).

This is where `test-interactive` gains a GUI path alongside its serial one, and where the
harness's QMP input channel stops being a test-only affordance.

## Milestone 6 — windows, ports, desktops

Sketched; detail when M5 lands.

Multiple windows, stacking, move/resize, and **the overview** — thumbnail capture, the frozen
image grid, and the desktop sidebar (desktop shell §6). Ports under windows, with
`list` answering discovery. The **desktop shell** as a second process: `/dev/desktop`, desktop
membership as a filtered view of the compositor's window set, moving windows between desktops.
Wiring by `sys_ns_bind` into an application's namespace, and the default-handler fallback.
Templates: instantiate, extract, `open ./code.nxg | desktop`, `save`.

## Milestone 7 — the composed desktop

Sketched. File browser and text editor; the patch canvas (Tier 1 drag-and-drop via `QueryCaps`,
Tier 2 durable wiring); and the question the composition doc leaves open — what happens to a
wired graph when an application crashes, and whether the desktop shell respawns and rewires it.

## What this unblocks

Filed shell items waiting on this arm:

| Item | Waiting on |
|---|---|
| `TODO(history-pager)` — list-style reverse-search | A terminal that can address the cursor (M4) |
| Shift-Enter continuation | Key events with modifiers (M3) |
| The prompt's live `PipelineStatus` glyph | A redraw-capable surface (M4) |
| Completion's *candidate list* UI | M4 (the engine itself is schema work, not display) |
| **`form`** (composition §3) | Designed since v1, **never built**. It lands *late*, after the toolkit and the first applications — and it is more useful there: if `form` can be built from the existing widget set **without adding new widgets**, that is evidence the toolkit's abstractions were right. As the first consumer it would have shaped the toolkit around generated, spec-driven UI, which is the narrower case. |

## Risks worth naming now

- **Pixel format varies by firmware.** QEMU is one data point. Keep format handling
  data-driven from Limine's report rather than hardcoding a layout that happens to work.
- **No modesetting means arbitrary resolution.** The terminal must handle whatever the firmware
  chose, including sizes that are not a multiple of the glyph cell.
- **Two console readers is a known failure mode.** Adding a GUI path while serial keeps working
  is exactly the shape that broke login once already.
- **Determinism is easy to lose accidentally** — one timestamp in a title bar and the gate
  becomes noise.

## What this does not do

Multiple outputs, USB HID, dynamic linking and the process-memory-model bundle, the
`WidgetRecord` typed-UI research bet, the full `std` cluster, and 3D or accelerated rendering of
any kind. Each is tracked in [`phase-4-desktop.md`](phase-4-desktop.md) or
`deferred-decisions.md`.
