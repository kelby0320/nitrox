# Nitrox Display Arm — Subproject Plan

**Status:** 🚧 in progress. The design is settled; this is the order it gets built in.
**Milestone 1 is complete** (2026-08-05): the gate, the framebuffer binding, the
self-hash, and the `screendump` smoke gate. **Milestones 1–2 are complete** (2026-08-06). A real client drives the compositor on every
`test-qemu` run, and the display gate compares a picture that arrived through the whole
Surface protocol. **Milestone 3 is complete** (2026-08-10): the i8042 driver, the
`input-server`, `libinput`, the compositor's focus and hit-test routing, and `libui` delivery
— proven end to end by `cargo xtask check-input`, which injects a keystroke and a click over
QMP and asserts they reach a **window**.

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
| **P3** | A **PS/2 keyboard and mouse driver** in the kernel, emitting `InputEvent` records from char device nodes. No input driver of any kind exists — the serial console's COM1 receive is the only path from a human to this system. | M3 |

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

## Milestone 2 — a client with a surface ✅ complete (2026-08-06)

**Deliverable: a program's pixels appear in a window, verified by hash.**

- [x] **Part A — the surface protocol.** ✅ (2026-08-05) Create, share the `MemoryObject`, `Commit { buffer,
      damage }`, release. Host-tested against the in-memory framebuffer. No new syscalls —
      `sys_memory_create`/`_map`, handle transfer and notifications already exist.
- [x] **Part B — `/dev/draw` served.** ✅ (2026-08-06) `new` mints a session; `<N>/info`
      answers with a mapped `WindowInfo` snapshot; replies and `Release` events go out on
      session channels. A bare `<N>` and `<N>/ports/…` are Milestone 6. The same
      `UserspaceServer` + subtree binding `/home` uses, so window paths are forwarded resolves
      and opening a window binds nothing.

      **Window roles and panel struts land here**, not later: bars are `panel`, menus and the
      applications modal are `popup`, and retrofitting a role into a shipped protocol touches
      every client (substrate §4a).
- [x] **Part C — `libui`: the client side of the protocol.** ✅ (2026-08-06) Connect,
      create a window, allocate and commit surfaces, and `acquire` a buffer — blocking when
      the compositor holds them all. *Receiving input and the event loop are M3*, and were
      wrongly claimed here before. The same role `librsproto` plays for
      the RS protocol — **the protocol gets a library, and clients use it.** If the first app
      hand-rolls this instead, the surface protocol immediately has two implementations and the
      second one lives in an application.
- [x] **Part D — a test client** ✅ (2026-08-06) built on `libui`, and the gate extends: the scene is now built
      from a real client's committed buffer rather than a synthetic one.
**On graduating the design docs.** Root `CLAUDE.md` requires the milestone that builds a
`design/` doc to carry its move to `architecture/` as a checkbox. That move is **not** M2's:
the three documents describe subsystems that are only partly built — `display-substrate.md`
still specifies input, text, capture and hotkeys with no code; `ui-composition-model.md`
specifies ports and desktops (M6); `desktop-shell.md` has nothing built at all. Moving a
document to `architecture/` while most of it describes the future is precisely the confusion
that directory split exists to prevent. Each carries an accurate Status line meanwhile, and
graduates with the milestone that finishes its subsystem — the checkboxes live in M6 and M7
below.
- [x] **Part E — release semantics** ✅ (2026-08-06), settled by the first client that
      double-buffers, exactly as this box anticipated. Three rules came out of it, each
      found by something breaking rather than by design: `Release` names the buffer that
      **left** the screen, never the one on it; the compositor **paints before it
      acknowledges**, or a client pacing off the release observes a screen that has not
      caught up; and a client **blocks** for a release rather than polling once, because a
      release that has not arrived yet is not one that never will. Single buffering is
      refused at construction — with one buffer there is never anything to release.

## Milestone 3 — input

**Deliverable: a keystroke and a click injected by the harness reach a client and are reported
back.**

**Four parts, not three** (revised 2026-08-06). The original three assumed a PS/2 driver
that emitted finished key events straight to the compositor. That is one device away from
wrong — see [`input-subsystem.md`](../design/input-subsystem.md) — so the driver shrinks to
raw event records and a userspace **`input-server`** takes the merge-and-policy role, which
is the arrangement `tty-server` already uses over `/dev/console`. The extra part is the cost;
what it buys is not relitigating the kernel boundary for USB HID, touchpads and touchscreens.

- [x] **Part A — P3: the i8042 driver.** ✅ (2026-08-06) Proven end to end by
      `cargo xtask check-input`, which injects a keystroke and a click over QMP and checks
      the decoded `InputEvent`s reach userspace — the driver was "armed" for two commits
      before anything had pressed a key, which is the same shape as the compositor that was
      bound and never answered (PR #174). **One** Tier 1 driver for the controller,
      publishing **two** char `DeviceNode`s — `/dev/input/raw/0` (keyboard) and
      `/dev/input/raw/1` (mouse) — each emitting `InputEvent` records. Keyboard and mouse are
      two devices behind one i8042: they share data port `0x60`, are configured through
      command port `0x64`, and enabling the mouse is a read-modify-write of the *same* config
      byte that carries the keyboard's IRQ-1 enable. Two independently-initialising drivers
      race on that byte and produce a machine that intermittently boots with a dead keyboard.
      Scancode→keycode table and mouse packet framing host-tested.

      **The interrupt is wired inside the arch layer, not through a neutral verb.**
      `arch/mod.rs` refuses to re-export `install_isa_irq` in as many words — "ISA" is
      x86-only jargon, and "a fixed legacy platform device wires its own interrupt inside the
      arch layer". The i8042 is exactly that class of device (ports `0x60`/`0x64` are as
      x86-only as IRQ 1), so it follows the serial console: an `arch::ps2` module owning port
      I/O and IRQ arming behind neutral verbs, with `drivers/ps2.rs` owning the ring, the
      framing and the portable scancode table — mirroring `drivers/console.rs` over
      `arch::serial::console_arm_rx`. Note that `check-arch` **cannot** catch this: it greps
      for literal `arch::x86_64` in non-arch code, and a neutral re-export is precisely how a
      violation would satisfy it.
- [x] **Part B — `input-server`.** ✅ (2026-08-06) Proven end to end by `check-input`, which
      now asserts **through** the server rather than off the driver — the client cannot open
      the raw nodes any more, which is the exclusivity the keylogging boundary rests on,
      demonstrated rather than asserted. A userspace resource server holding every raw node
      exclusively; merges the device streams into one ordered stream and serves
      `/dev/input/new`, minting a per-consumer channel the way `/dev/draw/new` does. A new
      rsproto `Input` category.
- [x] **Part B — `docs/spec/rsproto-input-ops.md`.** ✅ (2026-08-06) Written before the
      server, and allocating its category uncovered the `Tty`/`Surface` collision. Every
      rsproto category in the tree has a
      spec doc; this one also has to pin the `InputEvent` layout and the `EV_*`/`KEY_*`
      numbering, because they are a kernel↔userspace ABI living in at least two crates.
- [x] **Part B — the `EV_*`/`KEY_*` constants under `abi-sync-check`.** ✅ (2026-08-06) Done
      early, in the PR #178 review: the kernel doc claimed a `userspace/libkern` mirror that
      did not exist, so the mirror, a `U16Const` shape and 156 compared values landed
      together. It already compares
      exactly this kind of constant family across the kernel/userspace boundary; a numbering
      that drifts between the driver and the server is a silent misrouting, not a build error.
- [x] **Part C — the Surface-layer events, `libinput`, focus and routing.** ✅ (2026-08-10) Everything up to
      and including *the compositor sending an event*. The boundary with Part D is the
      channel: C ends when a `KeyEvent` goes out; D begins when a client receives one.

      - [x] **C1 — the Surface-layer records.** ✅ (2026-08-06) `KeyEvent` and `PointerEvent`
            in `librsproto` and `rsproto-surface-ops.md`. **`PointerEvent` was not in this
            plan** and is scope this milestone acquired honestly: the plan named only
            `KeyEvent`, because `display-substrate.md` §9 had parked pointer events with the
            trigger "the compositor needing to move a window". Part C's own deliverable — a
            click reaching a client — turned out to be the earlier trigger.
      - [x] **C2 — `libinput`.** ✅ (2026-08-06) The `SYN` state machine, modifier tracking,
            and keycode→character. Pure and host-tested (21 tests, seven breaks); it is what
            turns device triples into something a window can use, on both sides of the
            protocol. It deliberately does **not** track pointer position — deltas need a
            screen to clamp against, which the compositor owns.
      - [x] **C3 — the compositor consumes `/dev/input/new`.** ✅ (2026-08-10) Focus
            (topmost window whose role takes it), pointer hit-testing, and sending, as a
            pure `InputRouter` in the compositor's library half (16 host tests, ten breaks
            verified) plus the loop wiring in the bin. Two things the plan did not name and
            the work required: an **implicit grab** from a press to its release, without
            which a drag that ends outside a window delivers a press with no release; and an
            **init ordering change** — the input server now binds before the compositor is
            spawned, because the compositor resolves `/dev/input/new` before answering
            `Meta::Ready`. `rsproto-surface-ops.md`'s Status line is corrected.

            `cargo xtask check-input` asserts the compositor is **attached**
            (`compositor: input connected`). It cannot yet assert a key reached a window:
            nothing holds a window at injection time. That is Part D's gate.

- [x] **Part D — a client receives it.** ✅ (2026-08-10) `libui` delivers input into a window's event queue,
      and a test client echoes what arrived: a keystroke and a click injected by the harness
      reach a **window** and come back, which is the milestone's deliverable.

      **QMP injection is already done**, pulled forward to Part A (`cargo xtask check-input`)
      because a driver that is merely "armed" proves nothing — the same reason M2 Part D
      existed. What remains here is the client half and extending that gate to assert through
      `libui` rather than off the input server.

      **Two deferred items are folded in**, and the ordering below is the reason: each one
      needs the thing before it to exist. Folding them in was decided 2026-08-10, after
      checking `deferred-decisions.md` rather than working from memory.

      - [x] **D1 — the atomic log-line helper (`TODO(atomic-log-lines)`).** ✅ (2026-08-10) A `kprint`-style
            helper that formats into a stack buffer and issues **one** syscall, in `libkern`,
            plus a sweep of the call sites that build a line from several.

            Its filed trigger was "the next torn line that costs debugging time, or any test
            that needs to assert on a multi-part log line". **Both have now fired.**
            `check-input` was 40% flaky through M3 Part A because six `kprint` calls per line
            were shredded by concurrent output — and it was misdiagnosed as a guest bug first,
            which is precisely the cost the trigger names. D2's gate is the second: an echo
            client has to report keycode, modifiers and coordinates on one line for the
            harness to match.

            The stated reason for deferring — "the shape of the helper is worth deciding once
            rather than per site" — is answered by two independent hand-rolled copies of the
            same buffer (`compositor/src/main.rs`, `test-harness/src/inputclient.rs`). D2
            would make it three. **First, so D2 is built on it rather than copying it again.**

            Swept 47 multi-call sites across `init`, `eshell`, `service-mgr`, `session-mgr`,
            `compositor` and `test-harness`. Verified by **diffing the whole boot transcript
            before and after**: identical modulo addresses. That diff is what caught the
            sweep's own bug — an over-eager transformer collapsed three separate
            `ui-testclient` lines into one, which every gate still passed because each
            asserted substring was present, just no longer on its own line.

      - [x] **D2 — `libui` delivery, the echo client, and the gate.** ✅ (2026-08-10) `KeyEvent` and
            `PointerEvent` into a window's event queue, a client that echoes what arrived, and
            `check-input` extended to assert **through `libui`** rather than off the input
            server. This is the milestone's deliverable and the checkbox that retires
            "`check-input` proves the compositor is attached, not that a key reached a window".

            `Window` gained a bounded event queue (`EVENT_QUEUE_MAX`), `next_event`,
            `wait_event`, and a `WindowEvent::Dropped` marker on overflow. Input arriving
            while a client is blocked in `acquire` is **queued, not lost** — the ordinary
            case, since a client blocks exactly when the user is looking at the result and
            typing into it.

            The echo half went into `input-testclient` rather than a new binary, so one
            injection proves both paths and there is no cross-process print ordering to get
            wrong. Its window is **never committed to**, so the compositor skips it when
            compositing and `check-display` is unaffected — a window that has not drawn shows
            background, which is a real state and not a trick.

            **Still open, and not folded in here:** a client is not told when it gains or
            loses focus (`rsproto-surface-ops.md` records this as a gap, not a design), and
            there is still no cursor drawn on screen. Both belong to M4's toolkit work,
            which is the first thing that needs them.

      - [x] **D3 — back-pressure for compositor→client messages.** ✅ (2026-08-10) Its re-trigger is literally
            "Part D, when a second client exists", and D2 creates that client.

            **After D2, not before** — and D2 duly hit it, losing a keystroke behind twelve
            cursor movements. That failure is what settled the design: the problem was never
            depth. Input is a stream and a `Release` is not, so on a shared ring the cheap
            message reliably evicts the expensive one, and no depth fixes that — it only
            moves the threshold, turning a reproducible hang into a rare one.

            A bounded per-session `Outbox` in the compositor's library half, with **motion
            coalescing** (at most one queued per window, re-pushed at the back so ordering
            survives), head-of-line flushing that parks on refusal instead of dropping, and
            `Release` riding the same queue so input can never displace it. The ring went
            4 → 16: the old value was a literal copied into every resource server and a
            quarter of the kernel's own default, never a decision. 8 host tests, six breaks.

            The gate now injects the exact flood that broke D2. It asserts the **retry**
            half — coalescing is proven by host test, and the plan says so rather than
            letting the gate imply coverage it does not have.

**Pointer events are in this milestone, not deferred.** An earlier draft of this plan put them
with window management in a later milestone; buttons and menus need clicks, and the toolkit
(M4) comes before any application, so the mouse is needed here.

## Milestone 4 — the widget toolkit

**Design pass done** (2026-08-11): [`widget-toolkit.md`](../design/widget-toolkit.md) settles
the five forks this line used to list. In short — a **retained tree with a declarative face**:
the application holds state and writes `view(&state) -> Element`, the runtime diffs that against
the tree it keeps, and **the diff is where damage comes from**. Elm's shape, by way of Iced, and
taken for two reasons specific to this tree: `view` is a pure function so it host-tests like
every other subsystem here, and derived damage cannot rot the way a hand-written
`invalidate()` discipline does.

Layout is measure/arrange with four containers. Routing mirrors the compositor one layer down —
implicit pointer capture on press, and **widget focus kept strictly separate from window
focus**. `Commit` carries one damage rectangle, so the toolkit unions; damage accumulates
**per buffer**, which is the subtlety that is invisible until the compositor holds a buffer for
more than a frame.

**`libui` is renamed `libsurface`, and the toolkit takes the name `libui`.** Today's `libui` is
a Surface-protocol client, not a toolkit; the name was aspirational and the code went elsewhere.
The rename is mechanical and cheapest before a second client exists.

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
      Conflating them is the classic source of text arriving in the wrong field. Carries the
      **focus record** the compositor owes a client, without which the toolkit has no way to
      know the first of those two.
- [ ] **Part C — the first widget set**, bounded by what the terminal needs: `text`, a
      button, a menu, a scrollbar, and a **custom-drawn widget** escape hatch. Plus key
      repeat and the on-screen cursor, which are what make the set usable by a person rather
      than only by the harness.

      **No text area** — the design pass found this line contradicting Milestone 5, which
      makes the terminal grid a *custom-drawn widget of its own* precisely so it is not a
      generic text area. Nothing in M5 would then use one, and "the terminal decides how much
      of it exists" is this milestone's governing rule. It returns when something needs it.

**Three deferrals are folded in**, each because this milestone is its filed trigger: the
compositor telling a client it gained or lost **focus** (Part B — the second focus concept has
no source without it), **key repeat** (Part C), and **a cursor drawn on screen** (Part C). See
`widget-toolkit.md` §9.

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

- [ ] **Graduate `ui-composition-model.md`** to `docs/architecture/` — this milestone builds
      the ports and desktops it specifies, which is the rest of that document.

Multiple windows, stacking, move/resize, and **the overview** — thumbnail capture, the frozen
image grid, and the desktop sidebar (desktop shell §6). Ports under windows, with
`list` answering discovery. The **desktop shell** as a second process: `/dev/desktop`, desktop
membership as a filtered view of the compositor's window set, moving windows between desktops.
Wiring by `sys_ns_bind` into an application's namespace, and the default-handler fallback.
Templates: instantiate, extract, `open ./code.nxg | desktop`, `save`.

## Milestone 7 — the composed desktop

Sketched. File browser and text editor; the patch canvas (Tier 1 drag-and-drop via
`QueryCaps`, Tier 2 durable wiring); and the question the composition doc leaves open — what
happens to a wired graph when an application crashes, and whether the desktop shell respawns
and rewires it.

- [ ] **Graduate `display-substrate.md` and `desktop-shell.md`** to `docs/architecture/` —
      by the end of this milestone the substrate is fully built and the shell exists.

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
