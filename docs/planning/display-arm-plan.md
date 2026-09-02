# Nitrox Display Arm — Subproject Plan

**Status:** 🚧 in progress. The design is settled; this is the order it gets built in.
**Milestone 1 is complete** (2026-08-05): the gate, the framebuffer binding, the
self-hash, and the `screendump` smoke gate. **Milestones 1–2 are complete** (2026-08-06). A real client drives the compositor on every
`test-qemu` run, and the display gate compares a picture that arrived through the whole
Surface protocol. **Milestone 3 is complete** (2026-08-10): the i8042 driver, the
`input-server`, `libinput`, the compositor's focus and hit-test routing, and `libui` delivery
— proven end to end by `cargo xtask check-input`, which injects a keystroke and a click over
QMP and asserts they reach a **window**. **Milestone 4 is complete** (2026-08-11) bar one
doc-graduation box, which M5 carries as a prerequisite: `libui` — the retained tree, layout,
the keyed diff, per-buffer damage, event routing, painting, and the widget set — plus real
TrueType glyphs, key repeat, an on-screen pointer, and the font on the root filesystem.
**Milestone 5 is complete** (2026-08-13): the GUI terminal runs a real shell.
**Milestone 6 is complete** (2026-08-20): geometry in the stack and the initial-configure
handshake, the manager seam and its four events, popups positioned by their creator and clipped
to the screen, and the contract written down. **Milestone 7 was planned in detail 2026-08-21**,
and carries a prerequisite outside this arm — [`test-path-retrofit.md`](test-path-retrofit.md),
which lands first.
**Milestone 5 Parts A and B were complete** (2026-08-12): `libterm`'s parser, grid, render and
encoder, the blend that unblocked antialiasing, and `nxterm` itself — window, chrome, scrollback,
key repeat, and the display gate's third region. **Part C landed 2026-08-13 — `Tty::AttachBackend`,
per-backend routing, and `nxterm` hosting `nxsh` over what is a pty with the pieces renamed.**

**The arm was re-scoped on 2026-08-12**, from a gap found while planning Part C: the plan had
`session-mgr` spawning `nxterm`, which assigns a graphical job to the serial column's supervisor.
Nothing in `docs/` said who authenticates a graphical user or who spawns the desktop shell — the
top of that column was empty. It is specified now in
[`graphical-session.md`](../architecture/graphical-session.md), and the milestones below it changed
shape: the old Milestone 6 ("windows, ports, desktops") bundled work at three different
dependency depths, so it splits into **M6 — window management** (compositor only),
**M7 — the graphical session** (new: login, `desktop-session-mgr`, `desktop-shell`), and
**M8 — desktops and the overview**; the old M7 becomes **M10**. (M8 said "desktops, ports,
templates" until durable window-to-window wiring was cut on 2026-08-21; ports survive as paths
and are unscheduled, and templates went with the wiring.)

**Renumbered again 2026-08-26**, when minimize/maximize and snap-to-edge were raised: a new
**M9 — window decorations and interaction** goes in ahead of applications, which become **M10**,
and visual theming becomes **M11**. The reason is in this document already — `Place`'s spec note
says a relative move "would only serve an interactive drag, which needs a grab offset the
compositor does not keep. **It comes back with decorations, or not at all.**" Drag-to-move needs
somebody to own a grab region, so decorations are the *prerequisite* for snap rather than polish
that follows it. Themes are the polish half, and they are what M11 is.

## What this is

The build sequence for Phase 4's display arm: framebuffer to compositor to GUI terminal, and
the composition layer above it. The design docs are the source of truth for *what*; this plan is
the source of truth for *the order*.

- **Mechanism:** [`docs/architecture/display-substrate.md`](../architecture/display-substrate.md)
  — framebuffer ownership, surfaces, input, text, determinism, the test gate.
- **Semantics:** [`docs/architecture/ui-composition-model.md`](../architecture/ui-composition-model.md)
  — windows, ports, desktops.
- **The shell itself:** [`docs/architecture/desktop-shell.md`](../architecture/desktop-shell.md)
  — bars, applications modal, overview, and the operations it demands of the compositor.
- **Who logs you in:** [`docs/architecture/graphical-session.md`](../architecture/graphical-session.md)
  — the graphical column beside `session-mgr`'s serial one, and what the two share.

The typed shell + coreutils subproject ([`shell-coreutils-plan.md`](shell-coreutils-plan.md)) is
the CLI arm of the same phase and is complete through Milestone 4. Several of its deferred
items are unblocked by this one, and they are listed under "What this unblocks" below.

## Governing decisions

1. **The gate lands with the first commit, not after.** No compositing code merges without the
   `Framebuffer` trait and host tests behind it. Everything else in this system has a gate;
   pixels are the one place where "we'll add tests once it works" would actually happen, and
   the design doc's §8 exists to prevent it.
2. **The compositor stays small.** Pixels, surfaces, windows, input routing, focus. Anything
   else — desktops and spawning applications — belongs to the desktop shell. It is the process
   everything visible depends on.
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
      session channels. A bare `<N>` and `<N>/ports/…` are Milestone 8. The same
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
the four documents describe subsystems that are only partly built — `display-substrate.md`
still specified input, text, capture and hotkeys with no code when this was written; `ui-composition-model.md`
specifies ports and desktops (M8); `desktop-shell.md` has nothing built at all. Moving a
document to `architecture/` while most of it describes the future is precisely the confusion
that directory split exists to prevent. Each carries an accurate Status line meanwhile, and
graduates with the milestone that finishes its subsystem.

**Where each checkbox lives**, as of the 2026-08-12 re-scope:

| Document | Graduates in | Because |
|---|---|---|
| `input-subsystem.md` | **M4** ✅ | done 2026-08-12, as M5's P1 |
| `widget-toolkit.md` | **M4** ✅ | done 2026-08-12, as M5's P1 |
| `graphical-session.md` | **M7** | the milestone that builds the graphical login |
| `desktop-shell.md` | **M7** | the milestone the shell lands in |
| `ui-composition-model.md` | **M8** | ports and desktops, which M8 builds |
| `display-substrate.md` | **M9** ✅ | done 2026-08-30 — **one milestone late**, and by review rather than by the box; see below |

Added 2026-08-10 once there were five of these rather than three, because
`input-subsystem.md`'s subsystem finished with M3 and its box was simply never written — the
failure this paragraph exists to prevent, one level up.

**It then suffered that failure itself.** The re-scope moved three of the six and added a
fourth document, and the prose version of this list was left naming M6/M7 — misfiling
`ui-composition-model.md` by two milestones and omitting `graphical-session.md` entirely (PR
#193 review, finding 2). It is a table now, which is harder to leave half-updated, and the
`display-substrate.md` row also corrects a discrepancy that predates the re-scope: the list said
M6 while the document's own Status line said M7.
- [x] **Part E — release semantics** ✅ (2026-08-06), settled by the first client that
      double-buffers, exactly as this box anticipated. Three rules came out of it, each
      found by something breaking rather than by design: `Release` names the buffer that
      **left** the screen, never the one on it; the compositor **paints before it
      acknowledges**, or a client pacing off the release observes a screen that has not
      caught up; and a client **blocks** for a release rather than polling once, because a
      release that has not arrived yet is not one that never will. Single buffering is
      refused at construction — with one buffer there is never anything to release.

## Milestone 3 — input ✅ complete (2026-08-10)

**Deliverable: a keystroke and a click injected by the harness reach a client and are reported
back.**

**Four parts, not three** (revised 2026-08-06). The original three assumed a PS/2 driver
that emitted finished key events straight to the compositor. That is one device away from
wrong — see [`input-subsystem.md`](../architecture/input-subsystem.md) — so the driver shrinks to
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

## Milestone 4 — the widget toolkit ✅ complete (2026-08-11)

**Design pass done** (2026-08-10): [`widget-toolkit.md`](../architecture/widget-toolkit.md) settles
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

- [x] **Part A — the widget tree, layout, and invalidation.** ✅ (2026-08-11) Where a widget
      marking itself dirty becomes a damage rectangle on a surface commit — except that
      nothing marks itself dirty: **the diff is where damage comes from**, which is the whole
      reason the declarative model was chosen. Carried the **crate rename**
      (`libui` → `libsurface`), because everything after it imports the new names.

      Four steps: the rename; the user stack; `element` + `layout`; `diff` + `damage`.
      50 host tests in `libui`, 23 breaks verified across the four.

      **Two things Part A found that the design pass had not.** The retained tree initially
      *retained nothing* — every field was derived from the new frame, so keyed and
      positional pairing produced identical trees and identical damage, and the reorder test
      passed against pairing by position. Widgets now carry a stable id, which is also what
      Part B's focus paths need. And the stack guard gap turned out to need **two** changes
      rather than one: `MMAP_MAX` bounds only `sys_memory_map(hint = 0)`, so the hinted path
      needed its own check or the gap guaranteed nothing.

      **Also the user stack: 32 KiB → 8 MiB, plus a guard gap.** One of the four deferred
      items this milestone absorbs, and one whose filed trigger names it — a toolkit is recursive by
      construction (measure, arrange, paint and diff are all tree walks), and `libui` already
      documents a ~9 KiB struct held by value being enough to run a client off the end of its
      stack, silently, before its first line of output.

      Cheap because the stack is **demand-paged**: the cost is address space, not memory. The
      guard gap is two small changes: `MMAP_MAX` currently tops out at exactly the stack's
      lowest address, so leaving a gap means an overflow faults instead of landing in mapped
      memory — **and `sys_memory_map` must refuse a hinted range inside the gap**, since
      `MMAP_MAX` bounds only the `hint == 0` path. It lands in Part A rather than later
      because it is independent of the toolkit and wanted *before* the deep call chains
      exist.
- [x] **Part B — event routing** ✅ (2026-08-11): hit testing, pointer capture during a drag, and
      **widget-level keyboard focus**, which is a *second* focus concept — the compositor
      decides which window has focus, the toolkit decides which widget within it does.
      Conflating them is the classic source of text arriving in the wrong field. Carries the
      **focus record** the compositor owes a client, without which the toolkit has no way to
      know the first of those two.

      `Element<Msg>` arrived here, with the routing that can fire a handler; handlers are
      `fn` pointers because a tuple-variant constructor already is one. `libui::route` does
      hit-testing, capture, Tab traversal, key bubbling, widget-local coordinates and
      crossing synthesis; `FocusEvent` (op `0x0907`) carries window focus. 78 tests in
      `libui`, 25 breaks verified.

      **The gate routes through the toolkit**, so an injected keystroke reaches a *widget*
      and not merely a window: `element -> layout -> diff -> route -> handler`, with events
      that came from QMP through the i8042 driver, the input server, the compositor's router
      and `libsurface`. Every link is unit-tested; this is the only thing asserting they are
      wired to each other.

      **Six things the plan did not anticipate**, each found by breaking something or by
      reading the design against the code:

      - A **click could only land at the window's origin.** The containment check added the
        widget's own origin to a point already in that space, so a button in a second column
        or below a menu bar had every click silently cancelled — and the one test of the rule
        put its widget at (0, 0), where both expressions agree.
      - **`announce_focus` never ran on a create**, because create is the one op that replies
        with a window id and so returns `Outcome::Reply` rather than `Applied`. The gate could
        not see it: focus was announced anyway by leftover input tripping another call site.
        It now asserts **ordering** — a window's first event must be its focus change.
      - **A second server-initiated message class broke `libsurface`.** `ChannelTransport`
        failed a *request* when its parked queue overflowed, so an unrelated request died of
        traffic it had nothing to do with. `MAX_PARKED = 8` was fine with one class and wrong
        with two.
      - **§7.1 promised two things the router did not do**: widget-local coordinates and
        crossing synthesis. Found by reading the design against the code rather than by any
        test. The compositor's `ENTER`/`LEAVE` turned out to be *inputs* to that state
        machine rather than events to forward — its crossings are about windows, the
        toolkit's about widgets, and forwarding both handed a widget two enters.
      - **§7.2's motivating example was unreachable.** `on_key` returned `Msg`, so having a
        handler *meant* handling — a focused text field would swallow every accelerator, and
        "unhandled keys reach the menu" could not happen. It returns `Option<Msg>` now, and a
        non-capturing closure still coerces to a `fn` pointer, so nothing is lost.
      - **Widget focus is an id, not a path**, which is a divergence from §7.2 in the code's
        favour: a path breaks under exactly the reordering keys exist to survive. The design
        predates `Widget::id`, which Part A added; the doc is corrected rather than the code.

- [x] **Graduate two design docs to `architecture/`.** Done 2026-08-12, as M5's P1.
      [`input-subsystem.md`](../architecture/input-subsystem.md) and
      [`widget-toolkit.md`](../architecture/widget-toolkit.md) both describe built subsystems and
      had been sitting in `design/`, which root `CLAUDE.md` tells every session never to read
      as current behaviour. Both Status lines now say what exists, what is specified and not
      built, and where each part lives; the toolkit's build-order section became a record of
      what landed and of the four places the code diverged from the document.

- [x] **Part C — the first widget set**, bounded by what the terminal needs: `text`, a
      button, a menu, a scrollbar, and a **custom-drawn widget** escape hatch. Plus key
      repeat and the on-screen cursor, which are what make the set usable by a person rather
      than only by the harness.

      **Glyph rendering moves here from M5 Part A** (2026-08-11). Three of the four widgets
      draw text, and a toolkit that cannot draw its own labels has no gate worth the name.
      What moves is *glyphs* — the rasterizer, a glyph cache, and a blit; what stays in M5 is
      the **ANSI/terminal render path**, which is terminal semantics rather than toolkit
      capability.

      **Real TrueType from the start, via `ab_glyph`** — userspace's first external
      dependencies, verified to build for the custom target before the decision was taken
      (`display-substrate.md` §6). No bitmap font and no PSF loader is written, because that
      code would be deleted rather than grown. Coverage is thresholded to 1 bit until
      `libdraw` can blend, which is one branch in the per-pixel callback.

      **The font file ships on the root filesystem**, not the initramfs — a client that draws
      text starts long after `fs-server-ext4` is mounted, and the compositor never needs it.
      Done: `/system/fonts/DejaVuSansMono.ttf`, staged by the image build with its licence and
      loaded by `libdraw::text::load`.

      **The initramfs cleanup was folded in** (2026-08-11), because the same forty lines of
      `assemble_image` had to be touched either way and because M5 adds more display servers
      to the same list. `compositor` and `input-server` moved into the `system` store package
      and the five test programs into a new `test` one, so the boot image is 223,888 bytes in a
      release build and 232,668 in a test one — **the same program list either way**, where it
      used to be 323 KB against 680 KB. Init's boot order changed as a consequence: the
      display arm now comes up after `/bin` exists. See the 2026-08-11 decision-log entry.

      **The toolkit reached a screen** — `ui-testclient` presents `libui::reference` (one of
      each widget, drawn with the font it read off the disk) in a second window, and
      `check-display` renders the same function on the host and compares it pixel for pixel.
      That first render found a real defect: `Node::Fill` measured to `c.max`, so the first
      `button` in a row took the whole row and its siblings got nothing.

      **No text area** — the design pass found this line contradicting Milestone 5, which
      makes the terminal grid a *custom-drawn widget of its own* precisely so it is not a
      generic text area. Nothing in M5 would then use one, and "the terminal decides how much
      of it exists" is this milestone's governing rule. It returns when something needs it.

**Four deferred items are folded in**, each because this milestone is what triggers it: three
from the display arm — the compositor telling a client it gained or lost **focus** (Part B —
the second focus concept has no source without it), **key repeat** (Part C), and **a cursor
drawn on screen** (Part C), all in `widget-toolkit.md` §9 — plus the **user stack** in Part A
above. Of the four, only key repeat and the stack are entries in `deferred-decisions.md`; the
focus record and the cursor are gaps recorded in `rsproto-surface-ops.md`.

**No ABI question today.** With everything statically linked, the toolkit is an ordinary Rust
crate that applications link. The seam matters when dynamic linking lands — the phase plan
schedules the two together, and notes that two applications each embedding the toolkit share no
pages, which is exactly when it starts to pay.

## Milestone 5 — the GUI terminal (the MVP flagship) ✅ complete (2026-08-13)

**Deliverable: `nxsh` running in a compositor window, driven by keystrokes a person could
have typed, rendering its output with a real font.**

Planned in detail 2026-08-12, before any of it was built. Everything above this line was
planned the same way, and the M4 pass earned its keep by finding a contradiction between the
plan and Milestone 5 before either was written; this pass found the crate boundary below.

### What "done" means

Two gates, because they prove different things and one of them cannot be pixel-exact:

- **`cargo xtask check-terminal`** — boot, inject `whoami` and Enter over QMP, and assert the
  answer appears **in the terminal's grid**. This is the whole loop: i8042 → `input-server` →
  compositor → `nxterm` → `tty-server` → `nxsh` → back out. Asserted on the grid's contents
  rather than on pixels, because what the shell prints is not fixed by this milestone.

  **Written and passing, but not yet reliable** (2026-08-13). It passes and fails on the same
  build, and the flakiness is in driving a GUI from QMP rather than in the terminal: the banner,
  the prompt and the per-keystroke echo reach the grid on every boot. Two causes are known and
  one is a lead worth following — **input appears to stop reaching the compositor once
  `input-testclient` exits**, which if true is an `input-server` bug and not this milestone's.
  Not wired into CI until it is deterministic; a gate that fails on a good build teaches people
  to ignore gates. See `cmd_check_terminal`'s doc comment for the full diagnosis.
- **The display gate grows a third region** — `libterm::reference`, a fixed grid with every
  attribute the renderer supports, rendered host-side and guest-side and compared pixel for
  pixel. Same shape as `libui::reference`, for the same reason: the end-to-end gate above
  would pass with the glyphs in the wrong colour, at the wrong cell, or absent from a row it
  never checks.

`test-interactive` gains a GUI path alongside its serial one, and the harness's QMP input
channel stops being a test-only affordance.

### Prerequisite, before Part A

One item, not three. The first draft of this plan listed two protocol prerequisites and
justified both on the premise that the terminal's menu is a compositor-level popup **window**.
The toolkit says otherwise, in the widget this milestone is going to use:

> The popup half of a menu is not here: **an open menu is a `stack` layer the application adds
> over its content**, which needs the popup positioned under its item and is Milestone 5's
> first real requirement for it.
> — `userspace/libui/src/widget.rs`, `menu_bar`

`widget-toolkit.md` §5 agrees, listing `stack`'s reason for existing as "menu popups over the
grid". So as scoped, **`nxterm` creates exactly one window**, and neither protocol change is
something this milestone forces. Both were dropped in review (PR #186); what M5 actually needs
is an *offset within a stack*, which is a `libui` change and lives in Part B. See "Two
prerequisites that were not" below for where those deferrals now stand.

- [x] **P1 — graduate `input-subsystem.md` and `widget-toolkit.md` to `architecture/`.**
      ✅ 2026-08-12. Both moved, both Status lines rewritten to describe what exists, what is
      specified and not built, and where each piece lives. Two questions
      `input-subsystem.md` §7 left open turned out to have been answered by building it —
      batch overflow, and who owns pointer position — and are recorded with their answers
      rather than deleted.

      The same pass replaced root `CLAUDE.md`'s enumeration of what does and does not exist
      with the invariant it was an instance of: `design/` now holds exactly the three
      documents above Milestone 4, so the rule a session applies is "`design/` means not
      built" rather than a list that goes stale every milestone. That list had already been
      wrong for a day — it told every fresh session `libui` was hypothetical, which is the
      specific harm this box exists to prevent.

### Two prerequisites that were not

Recorded rather than deleted, because "why is this not being done" is the question a reader
of the next milestone will have.

**`KeyEvent` and `PointerEvent` still do not carry a window id, and M5 does not fire the
trigger.** The filed trigger is "the first client with two windows"; `nxterm` has one. The
record is structurally incomplete — a session *can* hold several windows and input cannot be
attributed — but a wire break with no client that needs it is speculative work, and this
project defers on triggers rather than on cost curves. **Corrected trigger: the first client
that genuinely holds two compositor windows**, which is a `Role::Dialog` or a popup that must
escape its parent — M6 or M7. (The size in the first draft was also wrong: `KeyEvent` is
**8 bytes**, four `u16`s, asserted at `librsproto/src/surface.rs`. Adding a `u32 window` takes
it 8 → 12, not 12 → 16, and there are two spare bytes at offset 6 rather than four.)

**A `Role::Popup` window still cannot be positioned**, and M5 does not need one. This leaves a
real tension in the existing documents, which is worth naming rather than leaving for whoever
hits it: `rsproto-surface-ops.md` says a popup "may extend beyond its parent's bounds; **a menu
clipped to its window is not a menu**", while the toolkit makes M5's menu an in-window layer
that is clipped to its window by construction. Both are right for their case, and the boundary
between them is the thing to state: **a menu that fits inside its window is a `stack` layer; a
menu that must escape it is a `Role::Popup`.** M5's terminal is 80×24 with a menu bar along its
top, so its dropdowns fit. Trigger for the compositor path: the first menu that does not — a
context menu near a window edge, or a combo box in a small dialog.

### Part A — terminal semantics, and the blend that unblocks antialiasing

- [x] **A1 — `libdraw` learns to blend.** ✅ 2026-08-12.
      `Font::draw_str` thresholds `ab_glyph`'s 8-bit coverage to one bit because `libdraw`
      composites opaque XRGB8888 and has no alpha path. That was the right interim answer for
      a toolkit drawing a handful of labels. A terminal is *entirely* text, which is where
      the filed trigger — "text that looks bad enough to prompt one" — actually fires.

      Built as `Rgb::blend` (pure colour arithmetic, host-tested for its endpoints, its
      monotonicity and its asymmetry) plus `Framebuffer::blend_pixel` (the read-modify-write,
      a provided method so the real framebuffer and the test one share it), with
      `Font::draw_str` passing the rasteriser's coverage straight through.

      **The threshold was deleted, not kept as a fallback** — a correction to this line and to
      `display-substrate.md` §6. A fallback is for a caller that cannot take the good path;
      thresholding is not that, it is only worse output. The one caller that could want it is a
      surface that cannot be read back, which cannot blend anything at all and is therefore a
      `Framebuffer` capability question rather than a second glyph loop.

      **No alpha channel.** Coverage is an argument to a blend, not a fourth byte; surfaces
      stay opaque XRGB8888 and `compose` stays a copy.

      The gate that mattered was the existing one: `check-display` renders `libui::reference`
      on the host and compares it against the guest pixel for pixel, so it now asserts that
      floating-point rasterisation *and* the blend agree between a host build and a
      `x86_64-unknown-nitrox` one. Both new host tests were checked against a reinstated
      threshold; one of them passed and was rewritten — it compared whole buffers over two
      backgrounds, and the pixels the glyph never touched satisfied it.

      **This is the deferral folded into this milestone**, and it is folded into Part A rather
      than done as a fourth prerequisite because it belongs with the text work and host-tests
      alongside it.

- [x] **A2 — a new crate: `libterm`. Not `libdraw`.** ✅ 2026-08-12.
      The pre-existing line said "the text/ANSI render path, in `libdraw`". **That is the
      wrong home**, and this pass is where it gets corrected. `libdraw`'s own doc-comment
      calls it "the pixel layer: geometry, pixel formats, framebuffers, and compositing".
      Escape-sequence interpretation, a cell grid, scrollback and line wrapping contain no
      pixels; they are terminal semantics, and putting them in `libdraw` would make the pixel
      layer know what a cursor is.

      `libterm` sits above `libdraw` exactly as `libui` does, and depends on it only for the
      render in A5:

      ```text
      nxterm            ← the client binary
        ↓
      libui   libterm   ← toolkit; terminal semantics
        ↓         ↓
      libdraw           ← pixels
      ```

      Everything in A3–A4 is a function of values and host-tests in milliseconds, which is
      the same split that let `fs-server-ext4`'s ext4 parser and `nxsh`'s evaluator be tested
      without booting.

      **Landed with the vocabulary A3 and A4 both need** (`userspace/libterm/src/cell.rs`),
      because a crate with nothing in it is not an increment. Three decisions in it:

      - **A cell stores a colour *name*, not a pixel.** `Colour::Ansi(Ansi::Red)` and
        `Colour::Default`, resolved against a `Palette` at render time. Storing `Rgb` would
        freeze the theme into the scrollback — re-theming would recolour new text only — and
        `Default` is not a synonym for white: SGR 39 means "whatever the theme says".
      - **The sixteen colours are named exhaustively** rather than held as a `u8` index, so the
        supported set *is* the type and there are no 240 values every consumer needs a rule
        for. 256-colour becomes a `Colour` variant when something emits one.
      - **Bold brightens the foreground, and reverse swaps what results.** The order is the
        whole of `Attributes::resolve`: a swap applied before resolution turns
        default-on-default into default-on-default, so reverse video on default text — where a
        shell actually uses it — would do nothing. Bold brightens because `libdraw` has one
        font weight, so `SGR 1` would otherwise be invisible; a real bold face supersedes it.
        Both orderings are host-tested and both fail when reversed.

- [x] **A3 — the output parser: bytes → grid operations.** ✅ 2026-08-12.
      A state machine over the byte stream: ground, ESC, CSI, and parameter accumulation.
      What it must handle is bounded by what `nxsh` and the coreutils actually emit, plus
      what a person's `Ctrl-L` needs:

      - `CUP`/`CUU`/`CUD`/`CUF`/`CUB` — cursor addressing, which is what the filed
        `TODO(history-pager)` has been waiting on
      - `ED`/`EL` — erase in display and line
      - `SGR` — bold, underline, reverse, and the 16 colours (30–37/40–47 plus the bright
        90–97/100–107)
      - `CR`, `LF`, `BS`, `TAB`, `BEL` (ignored)

      **Parameters are parsed generically**, so 256-colour and truecolour SGR become a match
      arm rather than a reshape. They are not in this milestone: nothing emits them, and a
      colour table nothing indexes is a guess.

      **An unrecognised sequence is consumed and dropped, never printed.** The `Discipline`'s
      input parser learned this already — a stray sequence that leaks its bytes into the grid
      is how a terminal ends up with `[0m` scattered through it.

      **It emits operations rather than driving a grid.** `Parser::feed` returns `Op`s and
      touches no state but its own, so A3 lands before A4 and an escape-sequence bug and a
      wrapping bug cannot present identically. Attributes stay the *grid's* state — `SGR`
      emits an effect and the grid applies it — because they are what `DECSC` saves and `RIS`
      resets, and splitting them across two owners is how they end up disagreeing.

      **UTF-8 decoding was added to this list** (it was not in the plan). `Cell.ch` is a
      `char`, chosen in A2, so a parser that only ever produced ASCII would mean A2 picked the
      wrong type. A broken sequence yields one `U+FFFD` and **re-feeds the offending byte**,
      because that byte is usually the start of something valid and eating it turns one
      malformed character into two.

      **Nine bugs in total**, four found by tests written before the code was believed and
      five more by its review — and every one of the nine was the same invariant, *an
      unrecognised sequence is consumed and never printed*, failing at a different byte. A three-byte `ESC ( B` was swallowed as two, leaking a `B`; `ESC`
      did not cancel a sequence in flight, so an interrupted program's replacement sequence
      printed as text; and a seventeen-parameter CSI indexed a sixteen-element array. The
      fourth arrived *with the fix for the second*: the ESC-restart went into the CSI state and
      not the intermediate one, and the test that checks **every** mid-sequence state rather
      than one caught it immediately.

      The review found five more, all by sweeping where I had sampled: `:` was the one byte of
      ECMA-48's parameter range with no arm, so colon-form SGR leaked; `OSC`/`DCS` introducers
      were treated as two-byte escapes, so every window-title sequence dumped its payload;
      an over-long UTF-8 encoding decoded to a *control character* and printed as a cell; a C0
      control mid-CSI both lost the control and leaked the rest; and — found by the same sweep,
      unprompted — `ESC [ 38;2;r;g;b m` read the colour's payload as ordinary codes and emitted
      **two spurious `Reset`s**, silently clearing attributes with nothing visible to say so.

- [x] **A4 — the grid: cells, attributes, cursor, wrapping, scrollback.** ✅ 2026-08-12.
      A cell is a `char` plus an attribute set. The grid is fixed at 80×24 for this milestone
      (see "Out of scope" — no resize until M6), with a scrollback ring above it.

      The parts worth naming because they are where terminals are subtly wrong:

      - **Wrapping is deferred-wrap**, not wrap-on-write: writing to the last column leaves
        the cursor *on* it with a pending-wrap flag, and the next character wraps. Writing
        exactly 80 characters and then a newline must not produce a blank line, and the
        naive implementation does.
      - **Scroll moves lines into scrollback**, and the scrollback is a ring with a bound.
      - **The cursor is clamped**, not wrapped, by cursor-addressing sequences: `CUP` past
        the last row is the last row.

      **`Cell::BLANK` was right and the erase fill was missing**, which is the sharper version
      of the concern raised when A2 landed. A never-written cell *is* the default; an **erased**
      one takes the current background, because `ED` after `SGR 44` is how a program paints a
      coloured region. Ink attributes go — a space has nothing to embolden — and with `SGR 7`
      set the swap in `Attributes::resolve` means an erase while reversed fills with the
      foreground, which is what makes "reverse, erase line" paint a solid bar.

      **`LF` is index — down only.** Returning to column 0 is `CR`'s job, and translating a bare
      `\n` is the *line discipline's* (Unix `ONLCR`), which this system has a tty server for.
      **This is a Part C item**: `tty-server` writes `\r\n` where it echoes and does not
      translate on the `Tty::Write` path, so a program emitting bare `\n` will stairstep in the
      GUI terminal until Part C decides where that translation lives.

      **Three rules had no test until a break-test said so**, which is the pattern worth
      recording: cursor clamping (the probe used `CUP 99` on a 3-row grid, where `99 % 3` and
      the clamp agree — 4 separates them), `ED`'s damage covering rows the cursor is not on, and
      the pending-wrap flag being cleared *after* a wrap. That last was a real defect: the wrap
      test wrote exactly one character past the margin and stopped, so a stale flag — which
      makes every subsequent character wrap — looked identical to correct behaviour, and a
      paragraph would have descended one line per character.

- [x] **A5 — the render, and `libterm::reference`.** ✅ 2026-08-12.
      `render(grid, font, &mut fb, damage)` — cells to pixels, with a block cursor. Cell
      metrics come from the font's advance, so the grid's pixel size is derived rather than
      assumed.

      **Damage is per-cell-row**, which is the whole point of doing this in a crate that
      knows about cells: a keystroke dirties one row, and the union of dirty rows is the
      rectangle that reaches `Commit`. A terminal that repaints its window per keystroke is
      the thing the toolkit's diff exists to avoid, one layer up.

      `libterm::reference` is the fixed grid the display gate compares: every attribute, a
      wrapped line, a cursor, and content varying in both axes. **Built through the real
      parser**, not by calling `Grid` directly — it is the only place A3, A4 and A5 are
      exercised together, and a mismatch in what an `Op` *means* would otherwise survive both
      halves being individually right.

      **The cursor is the cell drawn inverted**, not a shape over it: the character under it
      stays readable and it needs no colour of its own, which would be a third thing to keep in
      step with a theme.

      **The palette claim from A2's review is now enforced.** `libterm` and `libui` are siblings
      and neither may depend on the other, so their shared background colour had nowhere to be
      checked — `xtask` links both, and a host test there fails the build if either side is
      retuned.

      **A5's review found the bug that lived between the two parts**: the render paints the
      cursor *into* its cell, and A4 reported no damage when the cursor moved — so the row the
      cursor left kept an inverted block. Each half was defensible alone; the pair was wrong,
      and the grid test's own comment argued the opposite. `take_damage` now reports both the
      row the cursor left and the one it reached, which is the rule the compositor already
      follows for the pointer and for the same reason: something drawn *over* a surface rather
      than composited into it owns both of its positions.

      Two more tests were passing for the wrong reason, both found by break-tests. The
      underline test parked the cursor on the underlined cell, so the cell was drawn *inverted*
      and filled with the foreground — the assertion passed on the inversion and stayed green
      when the rule was moved into the next row. And a bounds check in `render_rows` turned out
      to be redundant rather than defensive: `Grid::cell` is total, so two guards covered one
      condition and only one was reachable.

- [x] **A6 — the input encoder: key events → bytes.** ✅ 2026-08-12 — **Part A complete.**
      The direction nothing in the plan mentioned, and half of what a terminal *is*. `nxterm`
      receives `KeyEvent { keycode, pressed, modifiers }`; `nxsh` expects bytes.

      **The gap is narrower than the first draft claimed, and the shape of it is the
      interesting part.** `libinput::keymap::to_char` does *not* stop at text: it folds control
      to the C0 range, so `Ctrl-C` is already `0x03`, and its doc says why ("the convention
      every terminal expects"). What is genuinely missing is:

      - **the arrows, Home, End, Delete** — absent from the `US` table entirely, and they are
        escape sequences (`ESC [ A`) rather than characters, so they could not live in a
        `keycode → u8` function anyway;
      - **Backspace** — keycode 14, absent from the table, wants `0x7f`;
      - **Enter** — present, but encoded `\n`, where a terminal wants `\r`.

      **So `libterm`'s encoder delegates to `to_char` for the text-and-control half** and adds
      the sequences and the two overrides on top. It does not reimplement the C0 fold: two
      copies of that would silently disagree, and one of them is already tested.

      **The round-trip test, stated honestly.** `tty_server::Discipline` parses input escape
      sequences because a serial terminal sends them, so `Discipline::feed(encode(Up))` must
      yield `Up` — two independently-written halves of one wire checked against each other.
      But `Discipline`'s `Key` enum is exactly `Up` and `Down`, and its CSI state returns
      `Step::None` for everything else, so **the round-trip covers two keys of the several this
      adds**, byte at a time. It is worth having and it is not coverage of the encoder. Home,
      End and Delete need their own assertions against the sequences a real terminal emits.

      Built with **a second round-trip that turned out to matter more**: a sequence the
      discipline does *not* know must be consumed rather than accumulated into the line being
      edited, or a terminal puts `[H` in someone's command. That is the same never-leak
      invariant as A3's, checked from the other end.

      **And it found a live bug in `tty-server`.** The first version of that test probed `Home`
      alone and passed, because `ESC [ H` is three bytes and the discipline consumed exactly one
      after `ESC [`. The four `~` forms are four bytes: `ESC [ 3 ~` ended at the `3` and the `~`
      was typed **and echoed** — press Delete, type `list /bin`, and the shell is handed
      `~list /bin`. Live over the serial console since the discipline was written, because that
      is what a host terminal sends for Delete; the old comment there claimed "the sequence
      cannot leak into the line", which was the claim to check. `Discipline` now ends a CSI at
      its *final* byte, and `ESC` restarts from inside one.

      **Three additions the plan did not name**, each because a keymap cannot express it and
      dropping it silently would be worse: `Escape` sends `0x1b`; `Insert`/`PageUp`/`PageDown`
      send their `~` forms alongside `Delete`; and **`Alt` prefixes with `ESC`**
      (`metaSendsEscape`), without which a modifier a person pressed simply vanishes. Alt does
      *not* double an escape that is already there — `ESC ESC [ A` parses as nothing, and in
      this crate's own parser the second `ESC` cancels the first.

      **Modifiers on the sequence keys are dropped**, so `Ctrl-Left` sends what `Left` sends.
      xterm's `ESC [ 1 ; 5 D` is additive and nothing in this system reads it. **One
      consequence is not merely missing**: through a terminal, `Shift-Enter` is
      indistinguishable from `Enter`, so the filed `Shift-Enter continuation` shell item is
      **not** unblocked by M3 as the table below says — it needs an encoding here, or a path
      that is not a byte stream. Raised 2026-08-12.

### Part B — `nxterm`, the terminal client

- [x] **B1 — the window and its chrome.** ✅ 2026-08-12.
      A `libui` tree: `menu_bar` docked top, `scrollbar` docked right, the grid filling the
      rest. This is M4's widget set having its first real user, and the point at which
      "bounded by what the terminal needs" gets audited — anything the terminal wants and the
      toolkit lacks is a finding about M4, and belongs in the decision log as one.

- [x] **B1a — the menu's popup half, which is a `libui` change and not a protocol one.**
      ✅ 2026-08-12 (the toolkit half; `nxterm`'s menu lands with B1).
      `menu_bar` shipped without it, and its doc names this milestone: "an open menu is a
      `stack` layer the application adds over its content, which needs the popup positioned
      under its item and is Milestone 5's first real requirement for it."

      Today `arrange` gives every `Stack` layer the parent's whole rectangle — "overlays that
      want to be smaller wrap themselves in a `Sized` or a `Padding`" — so a popup *can* be
      placed with computed insets today, by having the application derive `left` from the menu
      item's laid-out rect. That works and it is the thing §5 rejected when it ruled out
      absolute positioning: a layout engine written in application code.

      **Taken: the toolkit addition**, because M4's governing rule is "the terminal decides how
      much toolkit exists" and the terminal is deciding. Two pieces, both small and both reused
      by every later popup:

      - **`offset(dx, dy, child)`** — a child shifted within its parent, at its own *measured*
        size. §5 rules out absolute positioning for ordinary widgets, and that argument does
        not reach a popup, whose whole definition is "here, under the item that opened it".
        Like `Sized`, it changes its **own** rect rather than only its child's, or the
        containment invariant `paint` uses to skip a subtree would not hold.
      - **`locate(element, layout, key)`** — where a keyed element was laid out. Without it an
        application computes the item's width from the font, which *is* the layout engine §5
        refused, written in application code and wrong the moment a label changes.

      A break-test removed one of these additions again: `Fingerprint::Offset` initially carried
      `(dx, dy)` so that a moved popup would damage. It already did — `reconcile` damages on a
      rect change and an offset *is* a rect change — so the field was dropped as redundant, the
      same call as `libterm`'s `render_rows` bounds check. The test that found it stays, because
      the property is worth holding whichever mechanism provides it.

- [x] **B2 — the grid as a `custom` widget.** ✅ 2026-08-12.
      By the plan's own decision: a terminal's selection, wrapping and scrollback semantics
      are not a text editor's. The `custom` node's paint callback draws A5's render; its
      `on_key` takes raw key events and hands them to A6's encoder.

- [x] **B3 — scrollback wiring.** ✅ 2026-08-12.
      The scrollbar's offset drives the grid's view. **Any keystroke snaps back to the
      bottom**, which is what every terminal does and what makes scrollback usable rather
      than a trap.

      Mouse-wheel scrolling is *not* in this part: `PointerEvent` reserves `kind` for scroll
      and nothing produces one yet. The scrollbar is the milestone's answer; the wheel is
      additive and lands when the driver emits `REL_WHEEL`.

- [x] **B4 — key repeat reaching a *widget*.** ✅ 2026-08-12.
      Repeat is generated compositor-side (M4 Part C) and **already has a consumer and a
      gate**: `input-testclient` prints `win repeat code=` and `cargo xtask check-input`
      asserts it before the key is released. The first draft of this plan said it had neither,
      which was wrong.

      What is genuinely new is the *widget* half: a `KEY_REPEAT` reaching the grid through
      `libui::route` and being encoded like a press. Smaller than "prove the unproven
      mechanism", and a different test — the existing one asserts delivery to a **window**.

      Building it moved the whole key path through the router: the grid is a focusable widget
      with an `on_key`, so "typed a character" and "pressed a menu accelerator" are the same
      path with a different widget claiming it. Two things fell out. The toolkit gained
      **`Tree::find_by_key`** — `locate`'s companion, answering "which widget is it" where
      `locate` answers "where was it laid out" — because focus has to start somewhere and
      `focus_next` lands on the menu button. And keying the grid meant **keying its two
      siblings**, since the diff pairs a parent's children all by key or all by position and
      refuses a mixture.

- [x] **B5 — the display gate's third region.** ✅ 2026-08-12.
      `libterm::render::reference` had been a fixture with tests on it since A5: "growing it a
      third region needs a guest-side client that draws a terminal, which is Part B". It has one
      now. `ui-testclient` presents it in a window between the toolkit's and the scene's, and
      `check-display` compares 15,232 pixels of it against a host render — the only place a
      terminal render is checked against pixels that reached a screen.

      **The reference rather than `nxterm`'s own window**, which is also on screen and whose
      first frame is deterministic. A live terminal shows a boot banner: one plain line. The
      reference stream is built so each of its lines fails differently, and a gate should compare
      the picture that discriminates rather than the one that happens to be there.

      The three windows are nested — 320×160, 180×96, 64×32 — because windows stack at the
      origin in creation order and each must be smaller than the one beneath it. The gate now
      **asserts that nesting** rather than assuming it: its exclusions are what make each region
      mean something, and a window that grew past its neighbour would silently hollow one out.

### Part C — the tty server's second backend

This is the part with a real design question in it, and it is not "add a write path".

- [x] **C1 — the data flow inverts, and the pty is the shape.** ✅ 2026-08-13.
      Today the tty server holds `/dev/console` and *reads keystrokes from the device*. In a
      GUI terminal the keystrokes arrive at `nxterm` — a compositor client with a window and
      focus — and the shell's output has to reach `nxterm` to be rendered. The server is no
      longer next to the device; it is between two userspace processes.

      Unix solves this with a pty: the emulator holds the master, the shell gets the slave,
      and the kernel's line discipline sits between them. **Here the tty server *is* the line
      discipline**, so the same shape falls out: `nxterm` supplies a channel and the server
      uses it as that terminal's backend instead of the serial console. One line-discipline
      implementation, which is the property `console-and-tty.md` built the backend seam for.

      Concretely: one new op (`Tty::AttachBackend`, `0x0B06`), and `backend_write` stops being
      a free function that calls `kprint` and becomes a method on a backend that is either
      the serial console or a channel.

- [x] **C2 — routing is per-terminal, keyed by the backend.** ✅ 2026-08-13 (the server half;
      `nxterm` attaches in C3).
      **Not per-session**, which is what this said until 2026-08-12. The correction came from
      the maintainer asking why `session-mgr` would spawn `nxterm` at all, and it is worth
      keeping because the original was wrong in two independent ways.

      *It assumed a model the tty server does not have.* "A session's programs each reach the
      same terminal" is not how this works: a `Tty` is minted **per resolver**, not per session
      — `session-mgr` opens one for the login prompt, `nxsh` opens its own, and every stage
      `nxsh` spawns can open another. `console-and-tty.md` already carries errata saying so.
      Per-terminal is therefore not overturning a model; it is naming one that never existed.

      *And it made `nxterm` a session fixture rather than an application.* A terminal emulator
      is a program the user launches, possibly several of, exactly as `gnome-terminal` is. A
      backend keyed per session gives the second one nowhere to route.

      So: each `nxterm` owns a backend, and the ttys routed to it are the ones minted against
      it. The session's login shell keeps the serial console. This is *less* new structure than
      session-grouping would have been — the group key is a handle the server already holds.

      Two consequences to handle in the same change: `drive_input` routes console bytes to the
      first waiter in a flat `Vec` and broadcasts `Ctrl-C` to **every** open terminal, both
      because it cannot tell terminals apart; and a comment in `tty-server/src/main.rs` asserts
      "a session has one" terminal, which was already false and should be corrected rather than
      left to contradict the code around it.

      **Both backends must keep working** (governing decision 3), so the serial path stays the
      default and the GUI one is additive.

- [x] **C3 — the shell inside the window, and the `/dev/tty` it cannot have yet.** ✅ 2026-08-13.
      `nxterm` spawns `nxsh` itself, with `namespace: 0` — inherit — exactly as `nxsh` spawns
      `ls`. It hands the tty channel down in the setup message, the way `libstream` already
      passes streams and the way Unix inherits fd 0/1/2.

      **This is not the rule violation the earlier draft thought it was.** That draft rejected
      it because `nxterm` "would need to hold spawn authority and construct a namespace". There
      is no spawn syscap at all — `sys_process_spawn` gates on nothing, and `nxsh` spawns with
      `syscaps: 0` — so the only real constraint is namespace *construction*, and inheriting
      does not construct. `ui-composition-model.md` §5's guarantee ("no application holds a
      handle to another's namespace") is untouched: `nxterm` holds its own, and its child gets
      the LOOKUP-only view of it that every spawned program gets.

      **The gap this leaves, stated rather than hidden:** `/dev/tty` resolved *inside* the
      window still reaches the session's console, because that binding belongs to the namespace
      `desktop-shell` will construct. **No program that could run inside a window resolves
      `/dev/tty` except `nxsh`**, which will have been handed one — so it is inert today, and it
      is the wrong thing to fix here. (Not "nothing in the tree resolves it", which an earlier
      draft claimed and is false: `session-mgr` resolves it for the login prompt and the test
      harness resolves it in the gate. Neither runs in a window — PR #193 review, finding 7.) It gets a `TODO(gui-dev-tty)` and an entry in `deferred-decisions.md`
      triggered on Milestone 7, where [`graphical-session.md`](../architecture/graphical-session.md)
      §6.1 owns the question.

- [x] **C4 — does a dead backend end that terminal's ttys?** ✅ 2026-08-13 — yes, it does.
      `console-and-tty.md` already answers the single-terminal case: a terminal ends when its
      holder exits, via `PeerClosed`. Per-terminal routing makes the plural question small —
      `nxterm` exiting ends the ttys routed to *its* backend and touches nothing else in the
      system, where a per-session backend would have taken every terminal in the session with
      it.

      The answer is presumably yes, end them: a terminal whose window is gone cannot be
      interacted with, and leaving it alive gives its programs a `/dev/tty` that silently
      discards. Stated as a question because the alternative — keep them and let a replacement
      terminal reattach — is what session recovery would want, and nothing yet says whether it
      should exist.

**What Part C no longer contains.** An earlier draft had `session-mgr` spawning `nxterm` and
registering it. That assigned a graphical job to the serial column's supervisor, and it did so
because the process that should own it — `desktop-session-mgr` — did not exist in any document.
It does now ([`graphical-session.md`](../architecture/graphical-session.md)), it lands in Milestone 7,
and Part C is what its title says: the tty server's second backend. `nxterm` stays spawned by
`init` in the test image until there is something to launch it from.

### Decisions taken in this pass

1. **`libterm`, not `libdraw`** — the plan's own line was wrong, for the reason A2 gives.
2. **The input encoder exists at all** — it was absent from the plan, and it is half of what
   a terminal does.
3. **Antialiasing folds into Part A** rather than becoming a prerequisite: it is text work
   and belongs with the text work.
4. **The backend is per-session** rather than per-terminal, which follows from stage 1c's
   "each program resolves its own terminal" and is not otherwise obvious.
5. **80×24 fixed, no resize** — M6 owns move and resize, and a resizable grid is a different
   reflow problem than a fixed one.

### Out of scope, deliberately

Each of these is a thing a mature terminal has, and each would be a guess made a milestone
early:

- **Selection and clipboard.** There is no clipboard anywhere in the system; inventing one
  for a terminal would decide its design by accident. M6/M7.
- **Resize and reflow.** M6 owns move/resize. Reflowing scrollback on resize is the hard part
  of it and wants its own pass.
- **The alternate screen buffer.** `vi` and `less` use it; neither exists.
- **256-colour and truecolour SGR.** Nothing emits them. A3's generic parameters make them a
  match arm.
- **Cursor blink.** A static block cursor. Blink needs a timer in the client and buys
  nothing the milestone is measured on.
- **Job control.** Needs process groups, which do not exist and cannot be built on signals.
  Unchanged from `console-and-tty.md`.

### The deferral this milestone is most likely to fire

**The Surface protocol has no loss marker.** If a session's outbox overflows, the compositor
discards the oldest event and the client is never told. Its filed trigger is "the toolkit
needing to resynchronise held-key state after one", and a terminal that silently drops a
keystroke is the first thing anyone would notice. It stays deferred — the trigger is an
*observed* overflow and none has happened outside a deliberately wedged test client — but if
Part B drops input under load, the wire record lands there rather than being worked around.

## Milestone 6 — window management ✅ complete (2026-08-20)

**Planned in detail 2026-08-13. Part A landed 2026-08-13; B, C and D followed, and every
checkbox below is ticked.**

**Re-scoped 2026-08-12.** This milestone was "windows, ports, and desktops", which bundled three
things at very different dependency depths: windows need only the compositor, while ports and
desktops need a desktop shell that does not exist. They are split — windows here, ports and
desktops in M8 — and the shell's own milestone (M7) sits between them.

### Where this starts from

**The compositor has no window management at all.** `WindowStack::set_origin` exists, has no
protocol op, and has **no non-test caller** — all three call sites are inside `#[cfg(test)]`.
Every window is created at (0,0) and stacks in creation order. That is a missing subsystem rather
than a missing feature, and M5 Part B ran into it directly: the display gate creates its three
reference windows largest-first and *asserts* they nest, because nothing can move them.

**The pointer sprite is not a counterexample.** The cursor has no window id: its position lives in
the input router and it is drawn over the composed output, deliberately —
`compositor/src/lib.rs:149`, *"Composited **after** the window stack, because a cursor under a
window is not a cursor."* Reading it as a positioned window would suggest routing cursor motion
through the new placement op, which is the one thing the compositor is built not to do (PR #193
review, finding 1).

**What is already here and does not need building:** roles (`normal`/`panel`/`popup`/`dialog`),
panel struts and `work_area`, `raise`, `focus_candidate` (topmost-focusable), click-to-focus, and
damage-bounded repaint. M6 adds geometry and the seam; it does not revisit those.

### The three questions this milestone exists to answer

Everything below follows from these, and getting them wrong is what would be discovered in M7
with the shell half-written.

**1. Who may position a window?** Three answers coexist in every real system and conflating them
is the classic mistake:

| Who | What | Why |
|---|---|---|
| a **manager** | place, move, restack, focus **any** window | the shell's job (M7) |
| an **application** | place its **own popup**, relative to a parent it owns | a menu must land under its item; nobody else knows where that is |
| an application, for a **normal** window | **nothing** | a window that positions itself is `libui` §5's rejected absolute positioning, one level up |

The application carve-out is the same one `libui`'s `offset` makes and for the same reason: a
popup's position is *derived* from another element's rect, not chosen. Everything else is the
manager's.

**2. What is a manager, before there is one?** A capability, held as a namespace binding:
`/dev/draw/manage` resolves to a channel carrying the ops in Part B. The intended gating is that a
supervisor binds it into the shell's namespace and nowhere else — the mechanism this system
already uses for everything, and the shape `desktop-shell.md` §8 asks for when it says a
comparable capability *"must be capability-gated, or any application could impersonate the
launcher"* (that line is the **global hotkey** row, not the placement row — the argument
generalises, the citation does not, and an earlier draft cited it as if it did).

**In M6's configuration that binding gates nothing, and the plan must say so rather than imply
otherwise** (PR #195 review, finding 1). Three facts compose:

- `/dev/draw` is bound **unscoped** into init's root namespace (`init/src/main.rs:1063` — a
  `base_len` of 0, which `syscall-abi.md` defines as a whole-tree mount).
- Every graphical client is spawned with `namespace: 0` — inherit a LOOKUP-only handle to init's.
- Resolves are classified by **suffix alone**, with no caller identity anywhere in the path
  (`compositor/src/main.rs`'s `classify`). The compositor already records the consequence for a
  different suffix: *"Any holder of `/dev/draw` can resolve `info` in a loop."*

So `nxterm`, `ui-testclient` and `input-testclient` could all resolve `manage`, and the only thing
separating them would be B1's first-come rule — which makes the **race** the gate, when B1 exists
to avoid exactly that.

**This is not a hole to plug in M6; it is M7's work arriving early.** Namespace-based gating needs
*per-client namespaces*, and the process that constructs them is `desktop-shell`
([`graphical-session.md`](../architecture/graphical-session.md) §3, §5a). Until it exists, no binding can
be given to one client and withheld from another, because there is one namespace and everybody
inherits it.

What M6 does about that, explicitly:

- **The op set and the events are designed as if the gating were real**, because they will be.
- **`manage` is refused unless the resolver is the first to ask** (B1), and M6's image arranges
  for that to be `ui-testclient` by spawning it before anything else that would want it. That is
  an ordering, not a capability, and it is written down as such — `TODO(manage-ungated)`.
- **M7 closes it** by binding `manage` only into the shell's session namespace, at which point the
  first-come rule stops being load-bearing and becomes a belt-and-braces check.

Nothing in M6 is a shell, so the manage channel is exercised by `ui-testclient` under the harness.
That is the honest way to gate a seam with no consumer, and it is also the M7 shell's first
integration test written a milestone early.

**3. What does "resize" even mean here?** The compositor cannot resize a client's buffer — the
client allocates it. So resize is a **request**: `Surface::Configure` (server → client) says "be
this size", and the client answers by attaching and committing a buffer of that size.
`Window::bounds()` already follows the committed buffer, so the compositor side of this is
mostly already true.

**Terminal reflow is not in this milestone.** `nxterm` honouring a `Configure` means resizing
`libterm`'s grid and reflowing scrollback, which M5 called "a different problem, not a parameter
of this one" and was right to. M6 defines the op and proves it with a client that has nothing to
reflow.

### Part A — geometry in the stack

- [x] **A1 — placement, and a default policy.** ✅ 2026-08-13. `place(id, origin)` in the stack.

      **The default stays the origin, and cascade is dropped rather than deferred.** The plan
      left this open; building it settled it. A compositor-side placement policy is a policy the
      shell then has to override, which is the failure mode this milestone's seam exists to
      avoid — and with a manager attached the manager places, so a cascade would only ever apply
      to the manager-less case. That case is a test image and a degraded boot, neither of which
      is better served by windows landing somewhere clever.

- [x] **A2 — `Surface::Configure`** ✅ 2026-08-13, server → client, and the client half in `libsurface` —
      a `WindowEvent::Configure { origin, width, height }` a client may honour or ignore.
      Ignoring a *later* one is legal and must stay legal: a fixed-size window is an ordinary
      thing, and a protocol that required compliance would make every client implement reflow
      before it could exist.

      **The first one is different**: a client waits for it before its first `Commit`, which is
      what makes B4's ordering work. It carries an origin as well as a size because the manager's
      answer is a placement, and a client that had to learn its position some other way would be
      back to two mechanisms.

- [x] **A3 — restack beyond `raise`.** ✅ 2026-08-13. `lower`, and `raise_above(id, other)` for the shell's
      alt-tab. `raise` exists and click-to-focus drives it; the others have no caller until M7,
      so they land with tests and the manage channel rather than on their own.

- [x] **A4 — damage for a moved window.** ✅ 2026-08-13 — and the trap is now unreachable rather than known: `place` **returns** the damage, so it cannot be computed after the mutation. A move dirties the **union of the old and new
      rectangles**, exactly as M5's resize case does — and for the same reason, that a rectangle
      cannot express "old minus new". This is a real trap: `dirty` is computed *after* the
      mutation in most paths, and a move computed the same way repaints the destination and
      leaves the source on screen. M5 shipped that bug once already (PR #192, finding 3), which
      makes it a known shape rather than a surprise.

      **And there is a working pattern to copy**, which an earlier draft's "everywhere else"
      obscured: the two paths that need pre-mutation state already capture it — `server.rs`'s
      commit reads `let was = …` before `stack.commit`, and its destroy reads `let before: Vec<…>`
      before `stack.destroy`. A move is the third of the same shape, not a new problem
      (PR #195 review, finding 10).

### Part B — the manager seam

- [x] **B1 — `/dev/draw/manage`** ✅ 2026-08-19, a second resolve path minting a manager channel.
      Bound by a supervisor, held by nobody in M6. **One manager at a time**: a second resolve is
      refused rather than served, because two managers placing windows is a race with no arbiter
      and the failure would look like windows moving on their own.

      *The wiring existed in the parked WIP; what B1 needed to be done was the evidence.
      `ui-testclient` places its reference windows through the channel and falls back silently to
      the compositor's default if the resolve fails — deliberately, it is a fixture — and the
      windows land at the origin either way, because the origin **is** the default. So the whole
      path could break with every gate still green. `check-display` now asserts the placement
      by **read-back**, not by the reply: the client places a window at a non-default origin,
      reads it through `/dev/draw/<id>/info`, and restores it. The first version asserted only
      that the resolve worked, because every placement was to `(0, 0)` and that is already the
      default — so a refused `Place` looked identical, and the gate stayed green with every
      `Place` failing (PR #216 review). The client also asks a second time so the refusal branch
      is executed at all: it was implemented and unreachable, pinned by nothing. Controlled three
      ways — break the one-manager rule and the refusal line fails; break the resolve, or make
      `Place` reply `Ok` without moving anything, and the placement line fails. Gating is still open as `TODO(manage-ungated)`, closed by M7's per-client
      namespaces.*

- [x] **B2 — the manager's ops** ✅ 2026-08-19: `Place`, `Raise`, `Lower`, `SetFocus`, `Configure`
      (and `RaiseAbove`), with dispatch and host tests in `compositor/src/manager.rs`. A manager
      `Configure` is queued through the outbox like every other server-initiated record rather
      than sent `NOBLOCK` and forgotten, so a client whose ring is briefly full still gets it.
      Every one names a window by id and none of them checks ownership — that *is* the
      capability: a manager manages windows it did not create, which is the whole point, and the
      binding is what bounds who may.

      **`Move` is not on that list, and an earlier draft had it beside `Place` with no stated
      difference** (PR #195 review, finding 8). Both set an origin, so one of them is redundant:
      an absolute `Place` is what a manager wants — it computes positions from the work area and
      from other windows, so it always knows the answer in screen coordinates — and a relative
      `Move` would only serve an interactive drag, which is out of scope here and needs a grab
      offset the compositor does not keep. It comes back with decorations, or not at all.

- [x] **B3 — the manager's events**: window created, destroyed, geometry changed, focus changed,
      title changed. `desktop-shell.md` §8 lists *"Window list, focus and title notifications"* as
      **"Implied, never specified"** — this is where they get specified. Titles need
      `Surface::SetTitle`, which does not exist.

      ✅ 2026-08-20 — **four of the five.** `WindowCreated`, `WindowDestroyed`, `WindowGeometry`
      and `WindowFocus` are live and specified (`docs/spec/rsproto-surface-ops.md`, "Manager
      events"). Titles are split out as **B3b** — `TODO(m6-b3b-titles)` — because they are the
      only one needing a client-facing op *and* the wire's first variable-length body, which is
      a format question rather than a window-management one.

      Delivery is the queued outbox, not a `NOBLOCK` send: a manager that missed a
      `WindowCreated` holds a wrong window list forever and there is no resync op.
      `WindowDestroyed` is emitted per window *removed* — destroy is transitive, so
      `WindowStack` records the removed set as it goes rather than making each caller diff the
      stack around the call.

      Proven on the wire, not only in host tests: `ui-testclient` attaches as manager and
      watches one window's whole life (created → focus → geometry after a move → destroyed),
      naming whichever record fails to arrive. Each of the four was negative-controlled by
      breaking its emission and confirming the *named* assertion fires.

      **What `created` carries matters as much as when it fires**: the id alone is useless to a
      manager, which must know the **role** (a `panel` is not placed like a `normal`, and a
      `popup` is placed by its own client) and the **requested geometry** (centring needs a
      size). `CreateWindowRequest` already carries both, so this costs nothing — but it is the
      seam M7 is written against, and an event that forces the shell to ask a follow-up question
      is a seam with a round trip in it (PR #195 review, finding 9).

      **`geometry changed` is on the list for the same reason**: A2 makes size changes routine,
      and a window list that learned about them by polling would be the thing this event exists
      to avoid.

- [x] **B4 — placement before first paint, which needs an ordering rule and not just an early
      event.** A manager must place a window before the user sees it, or every launch visibly
      jumps.

      ✅ 2026-08-20 — the rule is enforced, not merely asked for: `Window.configured` gates
      compositing, so a client that jumps the handshake paints *nothing* rather than painting at
      the default origin. With a manager attached the compositor holds the first `Configure` and
      releases it on the manager's first request naming the window — `Configure` answers position
      and size together, which is why that op carries an origin — or on a **200 ms deadline**, or
      when the manager disconnects.

      Gated three ways in `check-display`, each with a control that fires: the configure is
      *withheld* (a client that sees one before answering as manager fails), it *carries the
      placement* (releasing at the default origin fails, naming both positions), and the
      *deadline* releases a window whose manager never answers (asserted from the compositor's
      own log, since a client cannot see why it was released).

      One consequence worth writing down: **a process that is both manager and client must not
      block on its own window's configure**, which `Window::new` does. B3's probe does exactly
      that and is released by the deadline — which is what makes it a deterministic trigger for
      the deadline assertion, and why B4's probe splits create from wait using the raw transport.

      An earlier draft said the interval between `CreateWindow` and the first `Commit` was the
      manager's window of opportunity, and that firing the created-event on create was "early
      enough". **It is not, and the reason is that the interval is the client's** (PR #195 review,
      finding 2): `CreateWindow` replies with an id, then `AttachBuffer` and `Commit` are silent
      sends the client issues back to back — which is exactly what `ui-testclient` and `nxterm`
      do. The manager is a different process and must be woken, scheduled, read the event and
      send `Place`. Nothing orders that against a `Commit` already queued. The symptom is a
      window that paints at the default origin and then jumps: intermittent, load-dependent, and
      precisely what this item exists to prevent.

      **The rule: a window is not composited until it has been configured.** `CreateWindow`
      replies as it does now, and the client must wait for a `Surface::Configure` (A2) before its
      first `Commit` — the compositor sends one immediately when no manager is attached, and
      after the manager answers when one is. A **deadline** covers a manager that never answers,
      so a wedged shell delays a window rather than losing it.

      This is Wayland's initial-configure handshake and it is the right shape here for the reason
      it is there: the round trip is the **client's** to wait out, not the compositor's. The
      rejected alternative — the compositor blocking on the manager before replying to
      `CreateWindow` — puts a userspace process on the critical path of every window creation,
      where a wedged shell stops clients from starting at all. Waiting costs the client one round
      trip at creation and nothing afterwards.

      It follows that A2's `Configure` is **not optional at creation** even though honouring a
      *later* one stays optional (see question 3): a client must commit at some size, and the
      first `Configure` is where it learns whether anyone had an opinion.

### Part C — popups, and what an application may do

**Detailed 2026-08-20.** Three decisions were taken before starting; the reasoning is below each
item. Two facts found while detailing changed the shape of the work:

- **C2 is almost entirely a proving job.** Nothing clips a popup to its parent — popups are
  separate windows, so the property holds by construction — and `libdraw::compose` already clips
  to the screen (`area.intersect(&screen)`, then `blit_clipped`). What is missing is any test
  that says so.
- **B4 would make every menu lag.** The initial-configure hold is role-blind, so with a manager
  attached a popup waits out the 200 ms deadline for a manager that, by design, must not place
  popups. That is a bug in waiting rather than a choice, and C1 fixes it.

- [x] **C1 — a popup is positioned relative to its parent.** ✅ 2026-08-20. `Role::Popup { parent }` carries the
      parent already; what is missing is an offset. Placed by its **creator**, checked against
      `conn.owns(parent)` — the ownership test the connection already performs, and which
      `server.rs` already does.

      **The offset travels in `CreateWindowRequest`, which grows 16 → 24 bytes.** Two new
      role-specific words: `x` and `y`, the offset from the parent's origin, zero for roles that
      have no parent. A wire break, taken deliberately while the ABI is pre-stabilisation.

      **"Popup" means popup.** C1 shipped treating `dialog` the same way — creator-placed and
      exempt from the hold — because the two roles share a wire shape. That was corrected the
      same day: a dialog names a parent but a manager places it, so it is held like a `normal`
      and carries no offset. See the 2026-08-20 decision-log entry "A `dialog` is placed by the
      manager, not by its creator".

      The alternative — a new `Surface::PlacePopup` op sent between `CreateWindow` and the first
      `Commit` — was rejected: it puts a second message on the path of every menu open, and it
      creates the popup at `(0, 0)` before moving it. Nothing would *see* it there, since a
      window is not composited before its first commit, but the manager would see a spurious
      `WindowGeometry`, and the position would be a thing that is briefly wrong rather than a
      thing that is never wrong. Carrying it in the create request makes the popup's position
      atomic with its existence.

      **Popups are exempt from B4's hold** (dialogs were too, briefly — see the note above). A window whose position is its creator's
      business has nobody to wait for. Without this every menu open costs the full deadline the
      moment a shell is attached — 200 ms, on the interaction most sensitive to latency.

      **The offset is resolved once, at creation.** The compositor stores absolute origins; a
      popup that tracked its parent would have to be re-placed whenever the parent moved, which
      is placement policy and belongs with the shell. In M6 only a manager moves a window, and
      a menu open outlives no such move. Recorded as a limitation rather than left implicit.

- [x] **C2 — a popup is clipped to the screen, not to its parent.** ✅ 2026-08-20 — the tests, as detailed; no behaviour changed. *"A menu clipped to its window
      is not a menu"* (`display-substrate.md` §4a) is the whole reason popups are windows rather
      than `libui` nodes; `libui`'s `offset` clips at the parent's edge and that is correct one
      level down. The screen is the only clip left.

      **Both halves already hold; this item is the tests that pin them.** A host test for a popup
      whose bounds extend past its parent's on every side, and one for a popup crossing a screen
      edge — including a negative origin, where the source-coordinate arithmetic in `blit_clipped`
      would be the thing to get wrong. Each negative-controlled, because a test that passes for
      the broken version too is decoration.

      **The compositor clips and does nothing else.** It does not slide a popup back onto the
      screen: that silently disagrees with where the client asked for it, with no way to tell the
      client. Nor does M6 expose the screen size so a client can flip a menu upward — that is a
      contract the shell (M7) may want to own, and nothing in M6 places a window near an edge.

- [x] **C3 — `nxterm`'s menu becomes a real popup** ✅ 2026-08-20., which is the first honest consumer and the
      thing that proves C1/C2 rather than asserting them. This is also the moment `libui`'s
      in-window popup stops being the only option, so the toolkit's `offset` gets a doc note
      saying which to reach for.

      **The bulk of Part C.** `nxterm` has exactly one `Window` and no window-id dispatch
      anywhere: the conversion needs a second surface, its own buffers, its own render pass, and
      an event loop that routes by window id. The menu is currently a `Stack` layer over the
      whole window (`nxterm/src/lib.rs`), anchored under the bar item.

      **It cannot prove the geometry, and should not be bent into trying.** `nxterm`'s menu fits
      inside its own window, so converting it demonstrates that an application *can* use a popup —
      not that a popup escapes its parent. Designing the menu to overflow in order to test the
      compositor would be shaping the terminal around its gate. C2's host tests and a
      `ui-testclient` case carry the geometry; `nxterm` carries the plumbing.

      **Three pieces, two of them prerequisites, both now done.**

      1. ✅ *Input records name their window* (2026-08-20). `KeyEvent` and `PointerEvent` carried
         no window id, so a client with a menu open could not tell a click on the menu from one
         on the window beneath. Closed the deferral filed at the PR #184 re-review, whose stated
         trigger was "the first client with two windows".
      2. ✅ *`libsurface` grew a session type* (2026-08-20). `Window` owned its `Transport`, so a
         client held one usable window per connection while a popup may only name a parent its
         *own* connection owns. `Session` owns the transport and lends windows through
         `WindowRef`. The compositor gained `MAX_WINDOWS_PER_CONNECTION` at the same time,
         because the old API had been the only bound.
      3. ✅ *The conversion itself* (2026-08-20) — a second surface for the menu, its own
         buffers, render pass and diff state, and a loop that routes by window id. `libui`'s
         `offset` gets no doc note after all: the menu no longer uses it, and the note belongs
         with whatever still does.

      **And it found a bug that made the whole feature moot.** `libui`'s hit-testing returns the
      *deepest* widget under the cursor and dispatch looked for a handler on exactly that — but
      `widget::button` puts its handler on the outer `Stack` and draws a `text` label inside it,
      so clicking a button hit the label, which handles nothing. **No button in this toolkit was
      clickable, anywhere.** Nothing caught it because the routing tests attach handlers to
      leaves and every gate that clicked, clicked a `custom` node, which is a leaf. Dispatch
      walks up to the nearest ancestor with a handler now.

      **Focus comes back for free, and gets a test anyway.** `Role::Popup` takes focus, so the
      menu window takes the keyboard from the grid; on close `focus_candidate` is the topmost
      configured focusable window, which is the terminal again. That it works by construction is
      the reason to pin it — nothing would fail if it stopped.

### Part D — the contract

- [x] **D1 — `rsproto-surface-ops.md` grows M6's ops.** ✅ 2026-08-20 — done incrementally as each op landed, and verified against this list: the manager channel section (all six ops), the manager events section, the `manage` resolve path, `Surface::Configure`, and `SetTitle` under "Reserved". Nothing was left for a doc pass at the end, which is what this checkbox existed to prevent. A resolve path (`manage`), B2's five
      manager ops, B3's five manager events, `Surface::Configure` and `Surface::SetTitle`. The spec is the
      canonical contract — its "How a client obtains a connection" section documents
      `/dev/draw/new` as *the* way, and "Which requests reply, and why a client must drain" is
      normative, so an op without a row is an op a second implementation gets wrong.

      Listed as a checkbox rather than assumed, because "What done means" lists only tests and
      that is how doc work goes unwritten (PR #195 review, finding 6). The `Tty` category spent
      months without a spec file for exactly this reason.

- [x] **D2 — the initial-configure handshake is documented as a client obligation** ✅ 2026-08-13,
      not just as a compositor behaviour. B4's rule only works if every client waits, and a client
      author reading the spec is the person who needs to know.

      **Landed with A2 rather than with the rest of Part D**, because A2 is what ships the op: a
      protocol change merging without its spec row is the thing finding 6 was about, and holding
      it for a later PR would have reproduced it.

### The gate collision, which is the first thing to resolve

**Placement is load-bearing for two gates and one spawn order, all of which assume windows land
at the origin.**

- **`check-display`** creates three reference windows largest-first *because* they land at (0,0)
  and nest, and asserts that nesting — with an error message that says so. Cascade them and every
  region it compares moves.
- **`check-terminal`** clicks at a point *"inside `nxterm` and clear of the reference windows
  above its top-left corner (the largest is 320×160)"*. That coordinate was computed from where
  `ui-testclient`'s windows land. Move one into its neighbourhood and the click focuses the wrong
  window, the typed line never reaches the grid, **and the failure reads as a tty regression
  rather than a placement change** (PR #195 review, finding 4).
- **`init`'s spawn order** is commented as load-bearing for the same reason — *"the terminal
  first, because windows stack in creation order at the origin and it is the largest"*. A1 makes
  that comment false and must correct it, not leave it to contradict the code.

The third is the one that would bite hardest, because a stale comment asserting a retired
invariant is what the last two milestones each shipped once.

Three ways out, and this should be decided before A1 rather than discovered:

- **`ui-testclient` places its windows explicitly** through the manager channel, and the gate's
  nesting assertion becomes a placement it performs rather than an accident it relies on. Best:
  it makes the gate stronger *and* becomes B1's first consumer.
- **The default stays the origin**, and cascade waits for the shell. Cheapest, and leaves M6
  without a visible behaviour change.
- **The gate learns the default** and computes expected positions. Worst: it couples a test to a
  policy that is about to move to the shell.

Recommendation: the first. It is the only one that leaves the seam exercised — and it must be
evaluated against **both** gates, because moving a reference window to satisfy `check-display`
is exactly what would break `check-terminal`'s click point. Whichever way it goes, `check-terminal`
should stop deriving its click from an assumed layout: M6's `SetFocus` gives it a way to focus the
terminal without a click at all, which is the more valuable outcome than a placement it can
predict.

### What "done" means

- **Host tests in `compositor`**, which is where the stack already lives (82 tests today):
  placement, move damage, restack order, focus after destroy, one-manager-at-a-time, popup
  clipping.
- **`cargo xtask check-display` grows a placement region** — two windows placed apart by the
  manager, screendumped, each compared where it was put. The existing regions prove compositing;
  this proves *geometry*, and a wrong sign or a swapped axis is exactly the class of error a
  host test cannot see.
- **`check-input` gains a two-window focus case**: two overlapping windows, click the lower,
  assert the keystroke follows the raise. **Not because click-to-focus is untested** — an earlier
  draft said that and it is false: `check-terminal` clicks a bottom-most window and asserts the
  typed line reaches the grid, and its own comment says *"click-to-focus is what gives `nxterm`
  the keyboard"* (PR #195 review, finding 5). What this adds is the *two-window* case, where a
  raise has to reorder something rather than confirm an ordering that already held.

- **`check-terminal` gets less fragile**, which is a by-product worth naming: `SetFocus` lets it
  focus the terminal directly instead of clicking a coordinate derived from another client's
  layout.

### Out of scope, deliberately

- **Decorations.** Nobody draws titlebars in M6, so nothing has a titlebar to drag by. Whether
  they are client-side (`libui`) or shell-drawn is a real question and it belongs with the shell.
- **Interactive move and resize** — the *gesture*. M6 gives `Place`, which sets an absolute
  origin; turning a drag into a sequence of placements needs a grab offset, which is
  `TODO(scroll-grab)`'s question ("M6's window management, which needs press-relative dragging
  for window moves anyway") and needs a decoration to grab. Both move together — to M7 when this
  was written, and **to M9 as of 2026-08-26**, the milestone that finally builds decorations and
  the interactive gesture together (PR #239 review, finding 10). The absence of the gesture is
  why B2 has no relative `Move`.
- **Terminal reflow**, per question 3 — **M9**, where it is the blocker for maximize and snap.
- **Desktops, thumbnails, global hotkeys** — M8, M8, and **M8 Part B** (hotkeys were pencilled
  in at M7 and did not land there).

### The risk worth naming

**A seam designed with no consumer is a seam designed wrong.** The mitigation is B1's test client
and the fact that M7 follows immediately: the shell is written against this within one milestone,
so the feedback arrives while the design is still fresh. The alternative — a throwaway shell in
M6 — was considered and rejected (maintainer's call, 2026-08-12): a shell built to be discarded is
a third thing to maintain and its feedback is worth less than it costs.

## Milestone 7 — the graphical session

**New in the 2026-08-12 re-scope**, and the piece whose absence caused the M5 Part C
misassignment: nothing in `docs/` said who authenticates a graphical user or who spawns the
desktop shell. [`graphical-session.md`](../architecture/graphical-session.md) now specifies it.

**Planned in detail 2026-08-21.** The details pass found that the 45-line sketch this replaces
described roughly half the milestone: three of its parts are work the sketch did not name at all,
and two of its claims about the existing system are false. Both are recorded below rather than
smoothed over, because the sketch read as complete.

### Where this starts from

M6 left a compositor that manages windows for a manager that does not exist yet: roles with
panel struts, placement, raise/lower/focus, the initial-configure handshake, four of the five
manager events, and popups positioned by their creator. `nxterm` is a real application with a
real menu. What has never existed is anything **above** the compositor — no login, no shell, no
process that spawns an application because a person asked for one.

**The display arm is also entirely `selftest`-gated today**, which the sketch did not say and
which changes what this milestone is. `run_nxterm`, `run_ui_testclient`, `run_display_selftest`
and `run_input_testclient` are all `#[cfg(feature = "selftest")]` in `init`. A release boot binds
`/dev/draw`, draws a cursor, and spawns nothing — so **M7 is where the graphical arm first exists
in an image a person would run.**

### The three questions this milestone exists to answer

1. **Who logs a graphical user in, and how is that not `session-mgr`?** Answered by
   `graphical-session.md` §1 and built here: a second supervisor, sharing a core, never the
   parent of the serial column.
2. **What does it mean for an application to be *launched*?** A constructed namespace, not an
   inherited one — which is the first time `sys_ns_create` is called by something that is not a
   supervisor tier, and the first time the compositor's manager channel can be withheld from
   anyone.
3. **Can the toolkit build the hard case?** `desktop-shell.md` §5 says the shell — not the
   terminal — is what decides the toolkit's central question, and the answer it gives is "an
   explicit toolkit plus one model-backed list widget". That widget does not exist. Building it
   is how the question gets settled.

### Prerequisite — the test-path retrofit

[`test-path-retrofit.md`](test-path-retrofit.md) lands **before** this milestone starts.
It is not something M7 needs in order to function; it is what stops M7 from tripling the
problem. `init` and `session-mgr` carry 1060 lines between them that differ between the release
and test builds, and the direction is what matters: **147 of `session-mgr`'s are the *shipping*
login path**, compiled out under `test-harness` — one `login()` in the release build and a
different one in the tested build — and **15 of `init`'s are its supervision of `service-mgr`**,
including the restart on death. This milestone adds three more processes, each of which would
otherwise want an auto-login of its own.

**The rule that follows, and it is the whole reason for the ordering:** nothing built in M7
contains a substitution cfg. The greeter is driven by the PS/2 injection `check-terminal`
already uses, against a release-shaped image, and there is no auto-login anywhere in it.

### What the details pass found, before the parts

Two claims in the design docs are false against the code, and both are load-bearing for the
sketch's own plan:

- **`auth-service` cannot serve a second client.** `graphical-session.md` §2 says it is
  "untouched", and the *protocol* is — but `auth-service/src/main.rs` creates exactly one channel
  pair at startup, transfers the one client end in `Meta::Ready`, and `serve_loop` blocks on the
  single serve end. There is no second endpoint for `desktop-session-mgr` to hold. Part C.
- **`/svc/auth` does not exist.** [`session-and-auth.md`](../architecture/session-and-auth.md) is
  an `architecture/` doc — a claim about current behaviour — and says auth-service's "endpoint is
  bound at `/svc/auth`". Nothing in the tree binds anything under `/svc`; `service-mgr` hands
  `session-mgr` a direct channel over its control channel. Were the doc true, the finding above
  would be free: a second supervisor would resolve its own. Corrected as part of Part C, and the
  reason `check-docs` could not catch it is that it validates `userspace/…` paths, not namespace
  paths.

And one requirement the sketch inherited without checking — where the check found the *sketch*
right and a first draft of this section wrong. `TODO(manage-ungated)` says M7 closes the
manager-channel hole "by binding", and it does: a namespace binding is per-path with an optional
subtree base, so `/dev/draw/new` can be bound **on its own**, with base `/new`, into an
application's namespace. An exact resolve of `/dev/draw/new` matches, forwards the suffix `new`,
and mints a session; `/dev/draw/manage` is not a component-boundary prefix match against that
binding, so it resolves to nothing. Part E, and the caveat that decides how long it lasts is
there too.

### Part A — the two widgets, and window titles ✅ complete (2026-08-25)

The three things every later part draws with, none of which exist. Gated on the host and through
`check-display` before anything new spawns.

- [x] **A single-line text field**, with a masked mode ✅ (2026-08-25). The greeter needs it for a password and
      the applications modal needs it for a search; `widget-toolkit.md` §8 says the **text
      area** is deliberately absent and that it "returns when something needs it — plausibly the
      file browser or a *find* box". This is that trigger, arriving earlier and narrower: a
      single line is not an editor, and building the editor's widget now would be the guess §8
      refuses to make. The distinction goes in the doc.

      The toolkit already anticipated this: `route.rs` has focus, a focus ring, tree-order Tab
      traversal, and `on_key` returning `Option<Msg>` **specifically** so a focused field cannot
      swallow a menu accelerator. The plumbing is there and unexercised.

- [x] **A model-backed list view** ✅ (2026-08-25). The window list, the launcher results and (in M8) the desktop
      previews are `desktop-shell.md` §2's churn, and §5's answer to the toolkit question is one
      widget covering all three rather than a diffing engine. **Design it against two callers,
      not one** — a list of windows and a filtered list of programs — because a model API drawn
      for a single consumer is the failure mode §5 was avoiding.

- [x] **Window titles ✅ (2026-08-25)** — the `m6-b3b-titles` deferral closed here. `Surface::SetTitle` (`0x0909`) and
      `WindowTitle` (`0x091C`) are declared in `librsproto` and unimplemented; the deferral's own
      trigger is "the desktop shell (M7) drawing a window list", and a window list without titles
      is a row of blanks. This brings the wire-format question the deferral split off: the
      **first variable-length Surface record**, so a length convention, a cap, and a stated answer
      for a client that sends 64 KiB of title.

- [x] **`check-display`'s reference render covers both widgets** ✅ (2026-08-25). They are the first widgets since
      M4 and the gate is how a widget is known to draw what it claims.

### Part B — the shared session core ✅ complete (2026-08-25)

- [x] **`libsession`** ✅ (2026-08-25): "authenticate → construct the namespace → spawn the leader → reap →
      tear down", the same logic in both columns against different arguments. Linux's PAM
      precedent — a shared library, not a merged process
      ([`graphical-session.md`](../architecture/graphical-session.md) §4).

      **Constraint:** `libkern` + `librsproto` + `libstream` + `libheap`, no `libos`, because
      `session-mgr` links it ([`session-mgr/CLAUDE.md`](../../userspace/session-mgr/CLAUDE.md)).
      Verified satisfiable: `libstream` and `librsproto` depend only on `libkern`, and only under
      their `io` feature. The greeter — the part that draws — stays in each supervisor, which is
      where they diverge anyway.

- [x] **`session-mgr` moves onto it first** ✅ (2026-08-25), and `test-interactive` stays green. The serial column
      proves the core before the graphical one depends on it; the alternative is a crate whose
      only caller is also new.

      This is why the retrofit is a prerequisite rather than a follow-up: factoring a `login()`
      that has two compilations would carry the fork into the shared crate.

### Part C — one credential oracle, two clients ✅ complete (2026-08-25)

- [x] **`auth-service` serves more than one client** ✅ (2026-08-25). Two shapes, and the choice is the part's
      first job: mint *N* client endpoints at `Meta::Ready` and multiplex with a wait set (which
      is what the compositor does with `MAX_SESSIONS`), or become a namespace forwarder that each
      supervisor resolves its own session from. The second matches how `fs-server`,
      `profile-server` and the compositor already work, and matches what
      [`session-and-auth.md`](../architecture/session-and-auth.md) has claimed all along.

- [x] **`/svc/auth` is true** ✅ (2026-08-25) — the forwarder shape was chosen, so the doc's original claim is now the code's behaviour. An
      `architecture/` doc that describes a binding nobody makes is the failure root
      `CLAUDE.md` names — the source wins and the doc is a bug — and it is a doc two designs
      have now leaned on.

- [x] **The protocol did not change** ✅ (2026-08-25). `Authenticate { username, password } → { AUTHENTICATED,
      principal, home } | DENIED` is untouched, and that remains the evidence the split was drawn
      in the right place. What changes is plumbing, and saying so precisely is the correction.

### Part D — `desktop-session-mgr`, the greeter, and the gate ✅ complete (2026-08-25)

- [x] **`desktop-session-mgr`** ✅ (2026-08-25), `session-mgr`'s graphical twin: spawned by `service-mgr` with
      `BIND_NAMESPACE` re-delegated, plus the fs/profile/tty endpoints, an auth channel and — the
      new part — a `/dev/draw` connection, because its greeter is itself a compositor client.
      Presents a login **window**, calls the same `auth-service` over the same protocol,
      constructs a session namespace, spawns `desktop-shell` into it.

      `/dev/console` is deliberately **not** bound into a graphical session — governing decision
      3's failure is on the record.

- [x] **The greeter draws before anyone has authenticated** ✅ (2026-08-25), and outlives each session
      (`graphical-session.md` §4). It is closer to `gdm`'s `class=greeter` than to anything
      `session-mgr` does, and it is the first compositor client that exists at boot in a release
      image.

      One M6 interaction that already works and should stay working: the initial-configure hold
      is skipped when no manager is attached, so the greeter composites without waiting for a
      shell that does not exist yet.

- [x] **The graphical login gate, built here rather than at the end of the milestone** ✅ (2026-08-25) — `cargo xtask check-login`. A wrong
      password, then a right one, then a shell — the sequence `test-interactive` runs on serial,
      driven by the PS/2 injection `check-input` and `check-terminal` already use, adjudicated on
      the host. Parts E and F then land against a gate that exists.

- [x] **Concurrency is decided: two independent sessions** ✅ (2026-08-25) — and demonstrated, not just decided: `check-login` logs in graphically and then on serial in the same boot, and requires that `desktop-session-mgr` never reported a session ending in between. (`graphical-session.md` §6.2,
      `session-and-auth.md`'s deferred "one console, one session at a time"). `session-mgr` and
      `desktop-session-mgr` each authenticate and run a session, unaware of each other. Serial
      stays the recovery path by construction, which is governing decision 3 holding trivially
      rather than by care. The costs are real and accepted: the same user may be logged in twice
      with two namespaces, and nothing arbitrates. It matches Linux — `getty` and `gdm` do not
      coordinate — and needs no registry, which is what `graphical-session.md` §1 says Nitrox
      does not need yet.

### Part E — `desktop-shell` ✅ complete (2026-08-25), bar the `/dev/desktop` binding — `TODO(desktop-endpoint)`

- [x] **The shell, minimally** ✅ (2026-08-25): the top bar, the applications modal, and window placement policy
      driving M6's manager ops. It is the compositor's first real manager — everything M6 built
      for one has been exercised by a test client until now.

- [x] **Constructing a namespace per application it spawns** ✅ (2026-08-25) — the load-bearing part.
      `ui-composition-model.md` §5a requires it: the guarantee that "an application cannot compose
      other applications" rests on the shell being the process that built them, and the shell
      holds `BIND_NAMESPACE` for exactly this.

- [x] **The shell does not bind its own endpoint** ✅ (2026-08-25) — the architectural half. The `/dev/desktop` *binding* is `TODO(desktop-endpoint)`, deferred until something resolves it. `desktop-session-mgr` binds `/dev/desktop`
      into the session namespace, exactly as `init` binds the tty server's and `session-mgr` binds
      `/dev/tty`. The shell holds `BIND_NAMESPACE` to construct *application* namespaces
      continuously, not to register itself once — which is what reconciles a process that both
      serves and constructs with [`syscaps.md`](../architecture/syscaps.md)'s rule
      (`graphical-session.md` §3).

- [x] **`manage-ungated` closed, by binding — which is what the deferral said** ✅ (2026-08-25). An
      application's namespace binds `/dev/draw/new` **as its own path**, with subtree base
      `/new`, rather than binding the `/dev/draw` subtree. Resolving `/dev/draw/new` is an exact
      match, so the forwarded suffix is empty, the base becomes the whole of it, and the
      compositor classifies `new` and mints a session. Resolving `/dev/draw/manage` is not a
      component-boundary prefix match against that binding
      (`kernel/src/object/namespace.rs`, `match_suffix_offset`), so nothing answers it. The
      shell's session namespace binds the `/dev/draw` subtree unscoped and gets both.

      No protocol change and no new endpoint: `session-mgr` already binds `/home` this way, with
      a subtree base, through the six-argument `SYS_NS_BIND`.

      **A first draft of this milestone specified a second forwarding endpoint** for management,
      carried in `Meta::Ready` and couriered `init` → `service-mgr` → `desktop-session-mgr`, on
      the reasoning that the compositor classifies by suffix with no caller identity so binding
      could not distinguish. Both premises are true and the conclusion does not follow from them:
      what a namespace can *reach* is decided by what it **binds**, not by how the server on the
      far side dispatches (PR #225 review, finding 1).

- [x] **The caveat is recorded** ✅ (2026-08-25), in the resolved-deferral row. A narrow
      bind expresses "`new` and not `manage`". It cannot express "the `/dev/draw` subtree minus
      `manage`" — so the moment an application needs `/dev/draw/<id>/info` for ids it does not
      know in advance, a subtree bind is required again and `manage` comes with it. Today no
      application library resolves anything but `new` (`libsurface`, `libui`, `libdraw`, `nxterm`
      — `<id>/info` appears only in the test client and in manager-facing code), so the narrow
      bind is sufficient and is what M7 builds. **The second endpoint is the fallback, and this
      is its trigger**: the first application that needs to read a window's metadata by id.

      **Also open, and a property of the test image rather than the design**: anything spawned
      with `namespace: 0` inherits root, where `/dev/draw` is bound unscoped — so the selftest
      path stays ungated whatever the session namespaces do.

### Part F — `nxterm` becomes launchable

What makes the milestone visible: a person clicks an entry in the applications modal and a
terminal opens.

- [x] **The shell spawns `nxterm` into a namespace it constructed** ✅, and §6.1 is answered —
      though `TODO(gui-dev-tty)` is *narrowed* rather than discharged, which the box did not
      anticipate. The three candidate shapes all assumed `/dev/tty` names a terminal. It names
      the tty **server**, which mints them: the namespace binds it uniformly, `nxterm` mints one
      and attaches its own window as the backend, and `nxsh` gets that terminal as a **handle**
      — so two terminals cannot contend, and neither a per-application binding nor named
      terminal groups are needed. What survives is not a naming question but an attenuation one:
      a terminal minted *without* a backend sits on the **console**, which is authority this
      session withholds elsewhere, and closing it needs a mint-only `/dev/tty` rather than an
      edit to `build_app_namespace`. Original box: `graphical-session.md` §6.1 holds three candidate shapes;
      the second is already what the code does — `nxsh` takes a handed-down terminal when its
      parent gives one and resolves `/dev/tty` otherwise, and its comment points at §6.1. The part
      confirms that as the answer or replaces it, and either way §6.1 stops being an open
      question.

- [x] **`nxterm` gets an environment, and hands it down** ✅. It takes a setup channel of its
      own and forwards the environment to `nxsh`; `desktop-shell` keeps the environment it
      receives and gives every application it launches a setup channel carrying `argv` + env.
      A launched terminal's shell now sees three environment fields, the same as a serial
      login's, and `build_app_namespace` binds the `/home` those fields name. `check-login`
      asserts it from **`nxsh`'s own** `nxsh: up (env: N fields)` — two attempts to have the
      *sender* report it were both blind to a broken forward, the second demonstrated in review
      (finding 1). Three controls run: break either hop, or delete the line, and the gate fails.
      `nxterm` still logs what it received, which is now diagnostic rather than the assertion —
      it is what tells the two hops apart when the gate does fail. Original box: It currently spawns `nxsh` with
      `Record::default()` and takes no setup channel of its own, so a terminal launched into a
      constructed namespace would give its shell no `$env.HOME` — unlike every serial login. It
      needs to receive a setup message and forward it.

- [x] **`init` stops spawning graphical clients** ✅ — done by retrofit Part C2, closed here.
      The comment from 2026-08-12 is answered twice: C2 made the test image's clients service
      declarations, and `desktop-shell` now launches a terminal in a **release** image. The
      declarations stay, and are not a duplicate: they put a terminal on screen *without a
      login*, which is what lets `check-display` and `check-terminal` test the display arm
      without depending on authentication. Original box: The retrofit made them service declarations;
      this part makes the real answer real, and closes the comment `init` has carried since
      2026-08-12: *"Until Milestone 7 there is nothing to launch `nxterm` from."*

- [x] **Graduated [`graphical-session.md`](../architecture/graphical-session.md) and
      [`desktop-shell.md`](../architecture/desktop-shell.md)** ✅ to `docs/architecture/`
      (2026-08-25), each with a Status line naming what is built. `desktop-shell.md` is the
      first doc to graduate while still outrunning its code — its overview and desktop
      indicator are M8, its tray is v2 — so its Status line says which sections describe
      behaviour and which describe intent, rather than the whole document being trusted.
      `design/` now holds two documents. Original box: — this milestone
      builds both. `desktop-shell.md`'s overview (§6) and tray (§9) are M8 and v2 respectively, so
      the graduation says what is built rather than moving a document that outruns its code.

### What "done" means

- A release image boots to a **login window**, a typed password reaches a desktop, and the
  applications modal launches a terminal into a namespace the shell constructed.
- `session-mgr` still presents `login:` on serial at the same time, and `test-interactive` still
  passes — the recovery path, demonstrated rather than asserted.
- An application cannot resolve `/dev/draw/manage`, and there is a test that says so.
- `grep -rn 'feature = "test-harness"' userspace/desktop-session-mgr userspace/desktop-shell`
  returns nothing.

### Out of scope, deliberately

- **Desktops, the overview, the desktop indicator** — Milestone 8. The shell ships with one
  implicit desktop.
- **The system tray** — `desktop-shell.md` §9 marks it v2; it is an inter-process protocol, not a
  widget.
- **Thumbnail capture and global hotkey registration** — `desktop-shell.md` §8 lists both as
  demands not yet in the substrate. Capture belongs with the overview (M8). The Super key is the
  modal's *second* trigger; the applications button is the first, and it needs no new compositor
  op. Building a capability-gated global-hotkey path for a shortcut is work the milestone can do
  without.
- **Per-user profile overlays, session tokens, switch-user, lock screen, seats** —
  `graphical-session.md` §7, all deferred with their serial equivalents.

### The risk worth naming

**Part E is where three untried things meet**: the compositor's first real manager, the first
namespace constructed by something that is not a supervisor tier, and the first process that both
serves and holds `BIND_NAMESPACE`. Each is designed; none has run. The mitigation is that Part D's
gate exists before Part E starts, so a shell that comes up wrong is distinguishable from a greeter
that never logged in — which is exactly the confusion that would otherwise cost the most time.

## Milestone 8 — desktops and the overview

**Details pass 2026-08-26.** The remainder of the old Milestone 6, now resting on a shell that
exists. Six parts, in dependency order; the shape follows Milestone 7's, where each part is
gated before the next begins.

**Rescoped 2026-08-21**, when durable window-to-window wiring was cut
([`ui-composition-model.md`](../architecture/ui-composition-model.md) revision 3). Ports-as-wiring, the
default-handler fallback and templates went with it; desktops never depended on any of them.

### Governing decisions

These were the open questions; they are answered here so the parts below can be built rather
than re-argued.

**1. Membership lives in the compositor, policy lives in the shell.** The compositor gains a
`desktop` attribute per window and one `current` value, and renders, focuses and routes input to
the windows matching it. It gains *no notion of a desktop object* — no list, no names, no
lifecycle. Which desktops exist, what they are called and when they disappear is the shell's,
which is what keeps composition §6's split ("the compositor owns pixels, surfaces, windows, focus
and input routing; the desktop shell owns desktops") true of the code and not only of the prose.

**2. A window is on exactly one desktop, or on all of them — there is no third state.** This
settles composition §7's "windows on **no** desktop": a window is assigned to the current desktop
when it is created, and moving it is one attribute write, so the transient the question worried
about never exists. **Sticky is `desktop = 0`**, reserved now even though the UI to set it may
land later, because a reserved value costs nothing today and is awkward to retrofit into a
shipped attribute. Rendering is `w.desktop == 0 || w.desktop == current`.

**3. Naming pins a desktop** — the lifecycle §9 shelved, decided 2026-08-26. An **unnamed** empty
desktop is removed; a **named** one is kept. The list always ends with exactly one empty unnamed
desktop to create into, and there is always at least one desktop. This makes composition §6's
"name it if it turns out to matter" the lifecycle rule rather than a separate mechanism: a scratch
desktop costs nothing and cleans itself up, a purposeful one survives its last window closing.
It also avoids GNOME 3's surprise, where a name a user set is discarded silently.

**4. Thumbnails are frozen, and the shell owns the memory.** `Capture` takes a window and a
buffer handle the *shell* allocated, and the compositor scales into it — the mirror image of
`AttachBuffer`, where a client allocates and the compositor reads. The compositor gains one
operation and no allocation policy. Capture is at thumbnail size, once per window per overview
open (desktop-shell §6).

**5. Hotkeys are gated by the manager channel.** Registration is a manager request, not a client
one, for the reason desktop-shell §8 gives: any application able to register `Super` could
impersonate the launcher. A registered chord is consumed before focus routing, so it does not
also reach the focused window.

### Part A — the compositor learns desktops

- [x] **`desktop` becomes a window attribute, and `current` a compositor-wide one** ✅. `0` is
      sticky; new windows are created onto `current`. Two manager requests —
      `SetWindowDesktop(window, desktop)` and `SetCurrentDesktop(desktop)` — and `desktop` joins
      `/dev/draw/N/info`. The compositor validates nothing about *which* desktops exist, because
      it does not know: any non-zero id is acceptable, and an id with no windows is simply an
      empty screen. **`current` is never `0`**, and that is the one thing it does validate:
      `0` means sticky, so a current of `0` would blank every non-sticky window *and* make every
      window created afterwards silently sticky, by the create-onto-current rule. Part F ships
      `desktop switch N` taking `N` off a command line, which puts `desktop switch 0` one
      keystroke away (PR #239 review, finding 7).

- [x] **Rendering, focus and input all follow the filter, and the last is the one that bites** ✅ —
      one predicate, `Window::visible_on`, used by `compose_into`, `focus_candidate` and `hit`.
      A window off the current desktop must not paint, must not be focusable, and must not
      receive pointer or key events — a window that is invisible but still hit-testable is the
      bug this part is most likely to ship. On a desktop switch the compositor focuses the
      topmost window on the new desktop, and the manager may override with `SetFocus`.

- [x] **The in-flight pointer grab is the half a filter in `hit()` does not cover** ✅ — widened
      the router's existing lazy reconciliation from *gone* to *not on screen*, which is the seam
      that already existed for destroys and cannot go out of date. Added
      2026-08-26 (PR #239 review, finding 2). `InputRouter::target()` is
      `self.grab.or_else(|| self.hit(stack))`, and `hit()` is where the existing on-screen
      predicate lives — so the natural place to add the desktop filter is inside `hit()`, and
      that leaves the grab path entirely unfiltered. `grab` is cleared when the window leaves the
      stack, on last-button-up, and on `Dropped`; **a desktop switch is none of those.** Hold a
      button down, press `Super+2`, and motion and release keep reaching a window that is not on
      screen. Same for minimizing mid-drag. The rule: switching desktops or minimizing the
      grab-holder **drops the grab and re-derives the crossing state**, which is what `Dropped`
      already does.

- [x] **A gate that fails for the right reason, with two controls rather than one** ✅ — and the
      *screendump* half moved to Part B, deliberately. Both controls were run and both are
      decisive:
      **(a)** filter only on `configured` (the pre-Part-A predicate) → the fresh-press test
      fails; **(b)** keep `hit()` filtered and revert *only* the grab reconciliation → exactly
      the two mid-drag tests fail and the fresh-press one still passes. (b) is the one that
      matters: it proves the two halves are covered independently, which a single control
      cannot show.

      **What did not land, and why.** The box said "screendump, and compare against a `libdraw`
      render the way `check-display` already does". That needs the host to hold the guest in a
      switched state, and `ui-testclient` **parks on `sys_wait` rather than reading input** — it
      has no way to be told to switch. The natural trigger is Part B's global hotkey, which is
      exactly a host-injectable state change, so the screendump comparison is a Part B box now
      rather than a silently dropped one. What did land in the guest is the coverage a host test
      genuinely cannot give: `ui-testclient` drives all three requests down `/dev/draw/manage`
      and reads each back through `/dev/draw/<id>/info`, which is the **wire** — the gap PR #233
      fell into. Negative-controlled by making `dispatch` answer `Ok` and change nothing: the
      gate times out, the same "a reply is not an effect" trap PR #216 turned into a rule.

- [x] **`minimized` is a second attribute, and deliberately not a desktop value** ✅. Added
      2026-08-26. A minimized window is still *on* its desktop — it restores there and it belongs
      in that desktop's window list — so folding it into `desktop` as a reserved id would conflate
      two orthogonal properties and make "restore" mean "guess where it came from". The filter
      becomes `!minimized && (desktop == 0 || desktop == current)`, and `Manage::SetMinimized`
      joins the two requests above. No client cooperation is involved: a minimized window is
      simply not composited, which is why this lands here and maximize does not (see M9).

- [x] **Spec first, because this is protocol** ✅. `WindowInfo` grew 32 → 40 bytes with
      `desktop` and a `flags` bitfield — a bitfield so M9's `maximized` costs a bit rather than
      another growth. Three callers sized a buffer at the old literal; all now use
      `WINDOW_INFO_LEN`, and the test client's was a **silent** break (the image builds, `read`
      just returns `None`). `docs/spec/rsproto-surface-ops.md` gains the three
      requests, both attributes and the sticky reservation before the code lands.

### Part B — global hotkeys

- [x] **`RegisterHotkey(mods, code)` on the manager channel** ✅, answered with an empty body
      and a `Hotkey` event carrying the chord back. **The manager picks the id**, the way a
      client picks a buffer id, so the reply needs to carry nothing. Capability-gated by
      construction: the manager channel is the only place the request is accepted, and
      `verify_app_namespace` already proves an application cannot reach it.

- [x] **A registered chord is consumed** ✅, not duplicated — and the focused window gets **no
      record of it at all**: not the press, not the release, and not the key repeat a held press
      would otherwise arm. Three rules, each because a simpler version was wrong: the release is
      swallowed by keycode rather than by re-matching; a key already down cannot begin a chord;
      and a consumed press arms no repeat (PR #241 review, blocking 1). The compositor matches before focus
      routing and does not also deliver the key to the focused window — with a test that types
      the chord into a focused text field and asserts nothing lands in it.

- [x] **`check-input` grows the chord** ✅ — injected on the real PS/2 wire, into a stack where a
      real client holds the keyboard, and the transcript must contain neither `win key code=59`
      nor `widget key code=59`. **The first placement was vacuous and the control caught it**: run
      after `input-testclient: PASSED`, the client has stopped logging, so a compositor that
      delivered the chord produced the same silence as one that consumed it. It runs inside the
      live window phase now, twice so the desktop ends where it started.

- [x] **And the screendump Part A could not take** ✅. `ui-testclient` no longer parks: it
      registers a chord and serves it for the rest of its life, so `check-display` injects
      `Super+F1`, waits for the guest to say it switched, captures, and asserts every pixel of the
      scene region is background — then chords back and compares the restored scene against the
      `libdraw` render **pixel for pixel, with no client commit in between**. The round trip is
      both ways on purpose: a one-way check passes for a compositor that filtered a window out
      permanently or lost its buffer on the way. Negative-controlled by removing the desktop
      clause from `compose_into` *only* — leaving hit-testing filtered — which is precisely the
      guest-consistent-and-wrong case a host test cannot reach: 660 of 2048 pixels still drawn.

### Part C — the bottom bar and the window list

- [x] **A second panel, docked `Bottom` with a strut** ✅ — and created *after* the manager
      is held, unlike the top bar. A dock edge tells the compositor what to reserve; it does not
      move the window, so a bottom bar has to be **placed**, and only a manager can place. The mechanism exists — the top bar
      already reserves space this way — so this is the shell managing two panel windows rather
      than new substrate. It closes half of what `desktop-shell.md`'s Status line lists as not
      built.

- [x] **The window list, from events the shell already receives** ✅ — and the shell had been
      **discarding three of the four**: `place_new_windows` read every manager event and acted
      only on `WindowCreated`. Desktop filtering is Part D's, since nothing switches yet. `WindowCreated`,
      `WindowDestroyed` and `WindowFocus` have been in hand since M6 Part B, which shipped four
      of its five events, and `WindowTitle` since M7 Part A, which closed the fifth
      (`TODO(m6-b3b-titles)`) — all four are in hand today; the
      list shows `normal` windows on the current desktop, excludes the shell's own, and a click
      raises and focuses. The focused window is shown as such — the first thing in the shell
      that reflects compositor state continuously rather than at a moment.

- [x] **Minimize and restore, from the window list** ✅ — clicking the focused entry puts its
      window away, clicking any other brings it forward, and `Super+H` does the first without
      reaching for the bar. The chord is Part B's mechanism getting its first real consumer. Added 2026-08-26. The list is already this
      part's work and it is exactly the right restore path — it is the answer to "where did my
      window go", which is the question minimize otherwise leaves unanswered. A minimized window
      stays listed and is shown as minimized; clicking it restores and focuses. Without a title
      bar there is no minimize *button* yet, so the gesture is the list entry and a hotkey; the
      button arrives with decorations in M9.

- [x] **The gate that sees the bar is `check-login`, not `check-display`** ✅ — the box had this
      wrong and the premise is worth recording. `desktop-shell` runs only in a **session**, and a
      `--selftest` boot never logs in, so the shell's bars have never been on `check-display`'s
      screen; its reference render needed no change and got none. Verified rather than assumed:
      the only `desktop-shell` line in a `check-display` run is the one saying it was *built*.
      The bottom bar, the window list, minimize, restore and the chord are all asserted in
      `check-login`, which boots a release image and logs in.

### Part D — desktops in the shell

- [x] **The desktop list, and the lifecycle rule** ✅ — one `normalize_desktops`, called
      after every change to *either* list, because the rule is about the pair: a window moving
      empties one desktop and fills another. Naming is done through the launcher's popup, since
      a `panel` takes no keyboard focus and the bar could never read a typed name. Governing decision 3, implemented: create,
      switch, name, and the auto-removal with its two exceptions (named, or the trailing empty).
      The shell holds the list; the compositor is told only `SetCurrentDesktop`.

- [x] **The indicator** ✅ (desktop-shell §7) — the current desktop's name, or its number when
      unnamed, at the end of the bottom bar. Clicking it opens the overview, which lands in
      Part E; until then it is the switch affordance itself.

- [x] **`Super+N` switches and `Super+Shift+N` moves the focused window** ✅ — for N up to
      **four**, not nine: `MAX_HOTKEYS` is sixteen and each desktop costs two chords, so nine
      would leave no room for the minimize and rename chords, let alone Part E's overview.
      Desktops past the fourth exist and hold windows; they are reached by the indicator. both on Part B's
      hotkeys, both ending in one attribute write. The move is deliberately available without
      the overview open, which is the half of "Both" that the drag cannot cover.

- [x] **A gate over the lifecycle rule specifically** ✅ — **by moving windows, not closing
      them.** The box said "close it", and no gate can: the only way to close the launched
      terminal is through its shell, which draws into the grid, and the grid renders under
      `test-harness` only (PR #242 review, optional 7). A desktop also empties when its last
      window is *moved away*, which is a gesture this part builds. `check-login` names a
      desktop, moves the terminal off it and shows it survived **because** it is named, then
      moves the terminal back and shows the desktop it vacated — unnamed — is gone, by the
      desktop count dropping. Both halves independently controlled: make naming not pin, and
      the first fails; remove nothing ever, and the second does. A third control unfilters the
      window list, which fails the same assertion for the third reason.

### Part E — capture and the overview

- [x] **`Capture(window, buffer, width, height)`** ✅ — governing decision 4. The scale is
      `libdraw::scale::box_downscale`, fully specified down to the band edges so that a gate
      which can see a thumbnail links it rather than writing its own. **No such gate exists**:
      the shell's buffer never leaves the guest. The scale is pinned by unit tests written
      against inputs where averaging and sampling disagree — the first version re-derived the
      band arithmetic inside the test and passed against both a nearest-neighbour scale and one
      that dropped the last source row (PR #244 review, blocking 1). The compositor
      area-averages the window's current contents into the shell's buffer. Deterministic, so a
      gate can compare a thumbnail against a `libdraw` downscale of the same source.

- [x] **The overview** ✅: a fullscreen window the shell creates, fills with the current desktop's
      frozen thumbnails, and destroys on close — the applications modal's lifecycle, which
      already works. A sidebar previews the other desktops and switches between them, which
      costs only a different set of captures.

- [x] **Drag a thumbnail onto a sidebar desktop to move its window** ✅ — and
      `TODO(scroll-grab)` is **re-deferred**, not answered, with a reason: this drag names a
      *drop target* rather than a position, so the thumbnail never follows the cursor and there
      is no offset for a grab to get wrong. The first consumer that needs it is M9's
      drag-to-move, where a window really does follow the pointer. Shell-internal drag: press,
      motion and release on the shell's own window, ending in `SetWindowDesktop`. This is *not*
      M10's structural drag-and-drop, which is between applications and needs protocol.
      `TODO(scroll-grab)`'s press-relative offset question is the same one, and this is the
      second consumer that deferral named — so it is answered here or explicitly re-deferred.

- [x] **A gate that opens the overview and drops a window on another desktop** ✅, then verifies
      by switching to that desktop — **by reading the window list rather than comparing the
      screen**, for the reason Part C established: `desktop-shell` runs only in a session, and
      the only gate that compares pixels boots a `--selftest` image that never logs in.
      What a screen comparison would have caught is covered in two halves, neither of them a
      comparison: `box_downscale`'s output is pinned by unit tests on inputs where averaging and
      sampling disagree, and the guest asserts the thumbnail came back with painted pixels —
      because the compositor logs a successful capture whether or not the scale wrote anything,
      and a black thumbnail reads like a dark window on a serial console. That control passed
      until the check existed. **What is still not covered is whether the thumbnail resembles
      the window**, which needs an end that can see both.

### Part F — `/dev/desktop`, its first consumer, and the graduations

- [x] **The shell serves `/dev/desktop`** ✅ — **as a session channel, not the path-per-object
      namespace §2a sketches.** `new`, `current`, `N/info` and `N/windows/` are not served: the
      operations that matter are *mutations*, and a resolve is a lookup rather than a call, so
      the bare path answers with a session the way `/dev/draw/new` and `/dev/tty` do. The
      per-object paths would duplicate what one `List` returns, for no consumer. The graduated
      document says so in its Status line. Original box:

- [x] **It is bound into *application* namespaces, not into the session namespace** ✅ — corrected
      2026-08-26 (PR #239 review, finding 1), and the correction is the whole of what makes the
      next box work. The first draft had `desktop-session-mgr` binding it into the session
      namespace, following [`graphical-session.md`](../architecture/graphical-session.md) §3. But
      the consumer is a `/bin` command, and a `/bin` command runs under the `nxsh` that `nxterm`
      spawns with `namespace: 0` — inheriting `nxterm`'s namespace, which is the **hand-written
      five-bind list** `build_app_namespace` constructs, not a projection of the session's. So
      `desktop list` would have failed to resolve the very path the part had just bound: an
      endpoint with a consumer that cannot reach it, which is `TODO(desktop-endpoint)`'s own
      failure mode wearing a fix.

      **And the session-namespace binding turns out to have no consumer at all.** The session
      namespace is the shell's own, and nothing else runs in it. Building it anyway is exactly
      what that deferral refuses, so it is not built — which also **removes the cost the deferral
      named**: no `Meta::Ready` handshake, and no change to `spawn_leader` to wait for a ready
      before reaping. §3's substance survives — the shell does not register *itself* with its
      supervisor — and only its mechanism sentence changes; see the box below.

- [x] **Settled: the subtree is granted** ✅ — and `verify_app_namespace` **cannot check it the
      way it checks the others.** A resolve is forwarded to whoever serves the path, so the shell
      asking the kernel for its own endpoint blocks it waiting for its own answer: *a process
      cannot verify a binding of itself by using it.* What it reports is that the bind succeeded;
      the consumer is what proves reachability — the strongest argument the deferral could have
      had for insisting the two ship together. Original box: Every
      application in the session reaches `new`, `current` and switching — strictly more than
      applications have today, since one cannot even raise its own window. The narrow-bind
      mechanism that withholds `/dev/draw/manage` while granting `/dev/draw/new` is available
      here too, and the honest v1 is nonetheless to **grant the subtree**: withholding mutation
      leaves `desktop switch` with no way to work, and a binding whose only consumer is disarmed
      is the shape this part exists to avoid. Recorded as a decision with the narrowing kept as
      an escape hatch. `verify_app_namespace` gains `/dev/desktop` as a positive check, beside
      `new` granted and `manage` withheld.

- [x] **Shipped with a consumer, in the same part** ✅. That deferral exists because this milestone
      has been caught three times shipping a specified, tested, unreachable capability (PRs #233,
      #236, #237), and a bound endpoint nothing resolves is exactly that shape. A small `desktop`
      command — `desktop list`, `desktop switch N`, `desktop name N <label>` — is the consumer,
      and it is also the first evidence that the desktop model is reachable from the command line
      rather than only from the shell that implements it. **The gate types it into a real
      terminal**, because that is the only way to prove the resolve works from where a user is —
      the same lesson as `check-login` asserting from `nxsh` rather than from its parent.

- [x] **Re-checked [`graphical-session.md`](../architecture/graphical-session.md) §3 against what
      shipped.** Its block was amended when this part was planned — the shell does not
      self-register, and binds its endpoint into the namespaces it *constructs* rather than into
      the session namespace — and it carries a "not built" status until this part lands. Clear
      that status, and confirm the prose matches the code rather than the plan.

- [x] **Graduated [`ui-composition-model.md`](../architecture/ui-composition-model.md)** ✅ to
      `docs/architecture/` with a Status line saying what is built. **Ports stay unbuilt**
      (`TODO(port-shape-rework)`), so the graduation follows `desktop-shell.md`'s pattern: name
      the sections that describe behaviour and the sections that describe intent, rather than
      moving a document that outruns its code.

- [x] **Closed the questions this milestone answered** ✅ — composition §7's sticky/no-desktop item
      and `desktop-shell.md` §9's lifecycle item both resolve here, and `design/` drops to
      **two** documents — `display-substrate.md` and `fault-survival.md`, the latter not a
      display document and not graduating with this arm. (`display-substrate.md` graduated
      2026-08-30, one milestone after it should have; `design/` now holds one document.)

### What "done" means

- Several desktops, created on demand, switched from the indicator, a hotkey, or the overview.
- A window moved between desktops by dragging its thumbnail and by `Super+Shift+N`.
- An unnamed desktop cleans itself up when its last window closes; a named one does not.
- The overview shows frozen thumbnails of the current desktop, with a sidebar for the others.
- An application still cannot reach `/dev/draw/manage`, register a hotkey, or capture a window.
- `grep -rn 'feature = "test-harness"' userspace/desktop-shell userspace/compositor` returns
  nothing new — the retrofit's rule holds for everything this milestone adds.

### Out of scope, deliberately

- **Ports as a wiring surface**, the default-handler fallback, and templates — cut in composition
  revision 3. Ports survive as paths and are unscheduled (`TODO(port-shape-rework)`).
- **Maximize, snap-to-edge, and drag-to-move** — Milestone 9. Maximize is not withheld for
  tidiness: `Manage::Configure` already carries size and position, but `nxterm` *declines* every
  `Configure` on purpose (resizing `libterm`'s grid and reflowing scrollback is "a different
  problem", M5), so maximize today would be a no-op on the only application there is. The client
  half is the work, and it belongs with the milestone that also gives the gesture a title bar.
- **The system tray** — `desktop-shell.md` §9 marks it v2; it is an inter-process protocol.
- **Live thumbnails** — an optimisation with a trigger (§9), not a v1 goal.
- **A full desktop switcher in the bar** — §7 argues the indicator first, and that it is additive.
- **A UI polishing pass.** Named here so it is tracked rather than assumed: the shell is
  deliberately plain, and polish is worth more once there is more to polish (maintainer,
  2026-08-26).

## Milestone 9 — window decorations and interaction

**Details pass 2026-08-27.** Six parts in dependency order, gated one at a time, the shape
Milestones 7 and 8 used.

**Inserted ahead of applications** because this document already said where it belongs:
`Place`'s spec note explains there is no relative `Move` because one "would only serve an
interactive drag, which needs a grab offset the compositor does not keep — **it comes back with
decorations, or not at all**". Drag-to-move needs somebody to own a grab, and a title bar is
where minimise and maximise live. Decorations are the *prerequisite* for snap, not polish that
follows it. The polish half — colours, fonts, styling — is Milestone 11.

### Governing decisions

Settled with the maintainer 2026-08-27, so the parts below can be built rather than re-argued.

**1. Decorations are client-side, drawn by `libui`.** Clients keep drawing their whole window;
`libui` gains a title-bar widget every application puts at the top of its tree. The compositor
gains the *interactions* — `StartMove`, `StartResize` — and no renderer, no font and no theme.

The argument that decided it is geometry, not taste. Under server-side decorations "the window's
rectangle" stops meaning "the client's committed buffer", and that meaning is threaded through
compositing, damage, hit-testing, placement, struts, `Capture` and the overview's thumbnails —
`Window::bounds()` reads the committed buffer's geometry today, and every one of those callers
would need to learn the difference between an outer and an inner rect. Client-side decorations
change none of it: the chrome is pixels the client committed, like every other pixel.

The counter-argument is real and is answered in decision 4 rather than dismissed: a close button
the client draws cannot close a client that has stopped answering.

**2. The compositor owns the drag, and exactly two messages cross the wire during one.**
`StartMove` and `StartResize` are client requests that hand the compositor an interactive
gesture — it already keeps the implicit pointer grab, so it is the only participant that can
follow a pointer without a round trip per motion.

**What that means for the outline and the snap preview, spelled out because the first version of
this section quietly broke it.** An outline that follows the pointer and an edge-proximity test
both need the drag state *per motion*. Reporting that to the shell would put back exactly the
round trip this decision exists to avoid — and into the one queue that cannot take it: the
manager outbox does not coalesce ("there is no manager-side equivalent of pointer motion",
`outbox.rs`), and when it is full it evicts the **oldest**, so a five-second drag at 100 Hz would
push a `Created` off the front and leave the shell with a window it will never place and never
hear about again (PR #247 review, blocking 1). So:

- **The compositor draws the outline** — a rectangle, four thin edges, no font and no theme. This
  does not reopen decision 1: what that decision refuses is chrome the compositor has to *lay
  out and style*, and what makes it refuse is that chrome changes what a window's rectangle
  means. An outline is neither. It is the cursor sprite's neighbour: a fixed shape the compositor
  draws over the composed stack and damages like any other rect.
- **Snap targets are a registered table, the way chords already are.** The shell computes its
  zones from the work area and registers them (`RegisterSnapZone`, trigger rect → target rect);
  the compositor tests the pointer against that table during a drag and draws the outline at the
  matching target. The policy — which region means which rect, and how close counts — is entirely
  in the table's contents, which the shell owns and re-registers when the work area changes. The
  compositor evaluates a lookup, exactly as `RegisterHotkey` made it match chords without knowing
  what any of them mean.
- **The manager hears two events per gesture**, not per motion: one when a drag begins, one when
  it ends carrying the final rect and which zone (if any) it ended in. The shell answers the
  second with the `Configure` it would have sent anyway. **The compositor never resizes a
  client** — that stays the manager's, so there is one path to a window's geometry rather than
  two that can disagree.

This is also where **`TODO(scroll-grab)`** is answered for windows: the press-relative offset
lives in the compositor's drag state. It is recorded when the **grab is taken**, at the press —
not read when `StartMove` arrives, which is a full round trip later through the client's own
router, by which time the pointer has moved and coalescing may have handed that client a stale
position (PR #247 review, finding 4). The toolkit's scrollbar keeps its own copy of the question:
that deferral stays open, and this milestone is its **third** named consumer without being its
first answer.

**3. Resize commits on release.** Dragging an edge moves an *outline*; one `Configure` goes out
when the button comes up. Live resize — a `Configure` per pointer motion — is a client cost, not
a protocol one: each one makes the client allocate new shared buffers, map them, re-lay-out and
repaint, which under TCG is the expensive path. Committing on release costs almost nothing on top
of snap, because snap already needs a preview overlay and a single `Configure` to a rect.

The protocol needs nothing for this: `Configure` is *"a request, not a command"* — a client may
commit whatever size it likes, and the compositor composites the geometry it is given. A terminal
that takes 79 whole cells instead of the 1003 pixels it was offered is an already-solved case,
which is why no size-hint or increment mechanism appears anywhere below.

**4. Close asks, and the shell can insist.** The title bar's close button is the client's own: it
sends nothing, it exits. For a client that has stopped answering, the shell's window list gains a
close that ends in a new manager request, `Manage::Close`, and the compositor destroys the window
— which is the *only* answer available under decision 1, and the reason that decision is
affordable. A hung application must be removable from the desktop without a serial console.

**5. Reflow is real reflow.** When `nxterm` honours `Configure` it rewraps its scrollback to the
new width rather than keeping the old breaks. This is the largest single item here and it is
deliberately not deferred: M5 called it "a different problem" when nothing could resize a window,
and Part D is the milestone where something can.

### Part A — the title bar, and a window that can be dragged by it

- [x] **`libui` gains a title bar** ✅. A widget: the window's title, a focus state, and up to
      three buttons at the top of a client's tree. **It did cost the toolkit one new mechanism**,
      which this box said it would not: `on_press` is a *click* — a release inside the widget
      that took the press — and a drag begins at the press, so the bar would have started
      following the pointer at the moment the user let go. `on_press_down` fires at the press,
      and a nearer `on_press` shadows it, which is what lets one bar carry a drag and hold
      buttons that do not drag. A button the caller has no message for is **not drawn**, so the
      minimise and maximise buttons arrive with Part B rather than sitting there dead.

- [x] **`nxterm` adopts it** ✅, above the menu bar it already draws — the only application
      there is, rather than a demo client nobody runs. Its window grew by the bar's height and
      its grid did not shrink: chrome that costs the user a row of text is chrome charged to the
      wrong account.

- [x] **The router records where a press landed** ✅, alongside the grab it already takes.
      Everything else depends on it: a drag offset measured when `StartMove` *arrives* is measured
      a round trip late, and the window jumps by however far the pointer travelled while the
      client was deciding the press was on its title bar.

- [x] **`Surface::StartMove(window)`** ✅ — "the user has grabbed a part of me that moves the
      window". The compositor moves the window with the pointer, offset by the recorded press
      position, and ends on button-up. Refused when the caller's window holds no pointer grab, so
      a client cannot move its window while nobody is touching it — and the refusal **says so on
      the console**, because a drag that silently does not happen is indistinguishable from one
      whose request never arrived, which cost a boot to find out.

      **The catch-up is applied when the drag starts**, not at the next motion: the pointer has
      already moved by the time the request lands, so a drag that waited would leave the window
      trailing — and would never move it at all for a gesture that ended in the meantime.

- [x] **The rule a manager and a drag must agree on** ✅: a window being interactively moved is
      not being `Place`d at the same time. `Place` during a drag is refused with `WouldBlock` —
      well-formed, and answerable again in a moment — rather than silently overridden, because a
      manager that lost the race would otherwise appear to work and fight the pointer. The flag
      lives on the *stack* rather than in the router, because the manager path never sees the
      router, and it goes with the window if the client exits mid-drag.

      **One `WindowGeometry` for the gesture, at its end.** Per motion it would be a manager event
      per pointer event, into the queue that does not coalesce and evicts its oldest — the failure
      PR #247's review named. Draining it needed a fix of its own: the *input* path never drained
      stack events at all, so a geometry change made by the pointer waited for some unrelated
      request to flush it.

- [x] **Gate** ✅: `check-login` presses on `nxterm`'s title bar, moves the pointer **before the
      client's `StartMove` can arrive**, and asserts the origin the shell reports. Both named
      controls fail it: a drag that reads the pointer at the request loses the motion injected
      before it, and one that puts the window at the pointer loses the grab offset.

      **And the button has to stay down until the drag is accepted**, which the first version got
      wrong: this harness can press and release faster than the round trip to the client and back,
      and a release that lands first takes the grab away — so the compositor correctly refused a
      move for a window nobody was holding. A person holds a button for a fraction of a second;
      a gate has to be told to.

### Part B — what the shell needs to answer a button, and the buttons that ask

- [x] **A manager op for the work area, because nothing could ask for it** ✅. `work_area()`
      exists in the compositor and has **zero production callers** — every reference is in a
      `#[cfg(test)]` module — and there is no op that exposes it. `desktop-shell` hardcodes
      `SCREEN_W`/`SCREEN_H` and says so in a comment: "the compositor has no 'what size is the
      screen' op, and adding one to draw a bar would be a protocol change made for a stub's
      convenience". Maximise is that convenience arriving: a `Manage::QueryLayout` returning the
      screen and the work area, plus an event when a strut changes, so a second panel-role client
      cannot leave maximised windows sitting under it with nothing able to notice (PR #247
      review, blocking 3). It also makes `Place`'s spec note — "a manager computes positions from
      the work area" — true, which it was not.

      `QueryLayout` answers with the screen *and* the work area, and `LayoutChanged` reports the
      work area **differing from the one last announced** rather than any particular cause: a
      strut moves through four different requests, and comparing the answer covers all of them
      and whatever comes next. The shell logs what it got, and `check-login` asserts it is
      smaller than the screen — an equal one would mean the struts were not counted.

- [x] **A client-initiated request needs a manager decision** ✅. A client cannot call
      `SetMinimized` — that is a manager op, and giving clients manager rights is the thing the
      capability model exists to prevent. So the button sends `Surface::RequestState(window,
      state)` to the compositor, which forwards it to the manager as an event; the shell decides
      and answers with the `SetMinimized` / `Configure` it would have sent anyway. **The client
      asks, the shell disposes** — the same shape as supervisor registration.

- [x] **And it is the first client-rate-controlled producer of manager events** ✅, which the
      queue's bound was not sized for: every existing producer is compositor lifecycle or
      `SetTitle`, and `SetTitle` is already deduped "because that queue is bounded". A request
      for a state the window is already in produces **no event**, by the same argument and for
      the same reason — otherwise an unprivileged client in a loop evicts `Created` records off
      the front of the shell's queue and corrupts its window list (PR #247 review, finding 5).

- [x] **Maximise is `Configure` to the work area** ✅, plus restore-to-previous-geometry. The
      previous geometry is the *shell's* to remember: the compositor has no notion of a window
      being maximised, and a `maximized` flag there would be a second source of truth about a
      rectangle. The shell keeps the origin as well as the size for exactly this, which it was
      not doing — `WindowGeometry` carries both and only the size was being read.

- [x] **Gate, and it stops at the layer this part builds** ✅. Minimise is asserted end to end — the
      window leaves the screen and the bottom bar marks it — because nothing in it depends on a
      client honouring anything. **Maximise is asserted as far as the request**: the shell logs
      the rect it asked for, and the assertion is that the rect is the *work area* rather than
      the screen. It cannot yet be asserted on the window, because `nxterm` declines every
      `Configure` until Part D — the plan says so itself, and the first version of this gate
      would have failed for that reason (PR #247 review, blocking 2). Part D's gate is where this
      one is completed.

      **The first version of that assertion passed against a shell maximising to the screen**,
      because the shell logged the work area it had computed while the request carried something
      else — the defect PR #238's review found in a different assertion, arriving by the same
      route. `configure_window` prints the arguments it is about to send, inside the function
      that sends them, so the two cannot disagree.

### Part C — close, and closing something that will not close itself

- [x] **The button is the client's own** ✅. `nxterm`'s close button exits; nothing crosses the
      wire, and the gate asserts that by *absence* — the transcript must contain no
      `RequestClose` for that window, which is why the gate closes two different terminals and
      names the second one's id.

- [x] **`Manage::Close(window)`** ✅ — the manager asks the compositor to destroy a window whose
      client is not answering. Distinct from `DestroyWindow`, which is the client's own request
      on its own session: this one names a window the caller does not own, which is exactly what
      the manager channel is for.

      **And `Manage::RequestClose` beside it**, which this box did not name. Decision 4 said the
      taskbar's close "ends in a new manager request, `Manage::Close`, and the compositor
      destroys the window" — one step — while this part's own control said closing a live client
      "must reach the client's own path rather than destroying a window out from under a process
      that was fine". Those cannot both be built. The control wins: a window holds a process's
      work, so the shell asks first and insists only when nothing happens, and asking needs an op
      of its own because only the compositor can reach a client's session.

- [x] **The shell offers it** from the window list ✅ — middle-click, which every taskbar this
      borrows from uses and which needs no room in a layout that is one fixed slot per window.
      It asks, remembers a two-second grace period, and insists when that runs out; a window that
      goes away on its own has already left the list, so the ordinary case never reaches
      `Manage::Close` at all.

- [x] **Gate**, adapted, and the adaptation is worth stating ✅. The box asked for "a test client
      that stops answering, closed from the window list" — and those two cannot be in the same
      image: the test clients live in the selftest build, `desktop-shell` runs only in the
      release one, and `check-images` exists to keep that difference from growing. So:

      - **`check-login` gates the ask, end to end, with the only client there is**: middle-click
        the taskbar entry, the compositor forwards, `nxterm` says it was asked and says it is
        closing, and the list loses it. The stated control is the assertion itself — a shell that
        destroyed the window would produce neither client line while the window went away all the
        same, and it fails the gate.
      - **`Manage::Close` is host-tested** on the manager dispatch: a window the caller does not
        own is removed, its rectangle is reported before it goes, and `WindowDestroyed` follows.
        Two more host tests came out of the review — the damage is the union of *every* window
        the destroy took, since a popup is not bounded by its parent, and the owning connection
        stops claiming a window somebody else removed, which is the state a wedged client is
        left in by construction.
      - **The child shell going with the window is gated too**, which nothing had observed:
        `nxterm` holds the pty master, so closing the window must end the `nxsh` it spawned or
        every close leaks a process. `check-login` reads `nxsh: terminal closed` off the
        transcript, and that line exists only on the path that exits.

      **What is not gated end to end is the insist**, because the release image has no client
      that can be made to ignore a request. Named here rather than left to be discovered:
      **trigger — the first application that can be wedged on purpose** (M10's file browser, or
      any client with a blocking operation), at which point the grace period and the
      `Manage::Close` that follows it can be driven from a gate.

      **The trigger fired in M12 Part A, and it took the grace period with it.** An editor
      holding an unanswered confirmation is a client that deliberately does not answer, which is
      indistinguishable from a wedge — and against it a timer destroys the buffer the question
      was about. Insisting is a *second middle-click* now, and `check-login` step 12 drives
      `Manage::Close` end to end for the first time. This box's own reasoning still stands: the
      shell asks before it destroys, and only the arbiter changed.

### Part D — `nxterm` honours `Configure`, and `libterm` reflows

The blocker for everything sized: until this lands, maximise, snap and resize are no-ops on the
only application there is. **Its gate is Part B's, completed** — maximise is the trigger, so no
test-only path is invented to drive a resize, and the two parts are honest about being one
dependency cut in two rather than two independent things.

- [x] **Resizing is a client-side mechanism, not an `nxterm` one** ✅ — `libsurface::buffers`.
      Allocating shared memory, attaching it, and the half with an ordering rule in it: replacing
      a buffer at a new size without touching the one the compositor is reading. **The protocol
      needed one change to make that bounded**: re-attaching a buffer id now *replaces* it, and
      is refused only for the committed buffer. There is no detach, so a resize with fresh ids
      would grow a window's buffer list — and the compositor's mappings with it — by two for
      every maximise and every restore.

- [x] **`Grid::resize(cols, rows)`** ✅ — the whole terminal re-laid out, the cursor following the
      character it was on, damage taken for everything. The screen's blank tail is dropped rather
      than rewrapped, or every resize of an idle terminal would push a screenful of nothing into
      the history.

- [x] **Scrollback rewrap, and the data model it needs** ✅ — `libterm::Line { cells, wrapped }`,
      set at the wrap in `print`, cleared by an explicit line feed on that row and by an erase
      that takes the row's tail. The rewrap joins logical lines and re-splits at the new width,
      trimming the trailing blanks that padded the old break.

- [x] **And the scroll anchor has to survive it** ✅ — `Grid::resize` returns a `Reflow` mapping
      old absolute line numbers onto new ones, and `nxterm`'s `view_top` goes through it. Without
      that a reader who has scrolled up sees a different part of the history after a resize.

- [x] **Gate** ✅, and it is the completion of Part B's: `check-login` maximises `nxterm` and reads
      back the **committed** geometry — exactly the work area, which is why the client takes the
      size it was asked for and fits whole cells inside it rather than rounding the window down —
      and asserts the grid grew, from the client's own report of what it came to in cells.

      **The reflow is asserted by an invariant rather than by content**, and that is a
      consequence of this being the release image: a terminal's rows are somebody's session and
      the serial log is not the place for them. A rewrap moves where the breaks are and does not
      create or destroy *lines*, so `nxterm` reports the logical-line count either side of every
      resize and the gate requires them equal. The control the box asked for — a wrapped line and
      **two deliberately short adjacent lines**, where an implementation that ignores the flag
      joins the pair and still passes "the long line is one row" — is the same property stated
      over the whole history: that implementation collapses it to one line, in one number.
      Negative-controlled by making the rewrap ignore `Line::wrapped`: `check-login` fails with
      "a resize turned 3 logical lines into 1". The row-level control, with the long line and the
      short pair spelled out, is `libterm`'s own test (PR #247 review, optional 6).

- [x] **And the maximise button had to become a toggle** ✅, which was not in this part's plan and
      is its most direct consequence. The shell has had a restore path since Part B and nothing
      could reach it: `nxterm`'s button only ever sent `WINDOW_STATE_MAXIMIZED`, which was
      invisible while the client declined every `Configure` and is *a window you cannot get back*
      the moment it does not.

### Part E — resize by an edge, committing on release

- [x] **`Surface::StartResize(window, edges)`** ✅ — the same shape as `StartMove` with a mask of
      edges, refused the same way (the grab is the authority) and offsetting from the same
      recorded press position. A corner is two bits; naming no edge, or both of an opposite pair,
      is `InvalidArgument`.

- [x] **The compositor tracks the rect and draws its outline** ✅, per decision 2 — no manager
      traffic during the drag, and the outline reaches no client at all: it is drawn over the
      composed stack, the cursor sprite's neighbour rather than a window. Its damage is the four
      **edge strips**, not the rectangle: the union of where an outline was and where it is is
      very nearly the window, and repainting that per motion is the ~100 ms recompose that
      starves input. On button-up one `ResizeEnded` goes to the manager and the shell sends the
      `Configure`; the compositor never resizes a client. (Renamed `DragEnded` in Part F, when a
      second gesture started producing it.)

- [x] **Gate** ✅ — `check-login` step 6j drags the maximised terminal's corner inward and asserts
      the mechanism in the order it happens: the compositor took the gesture and says which
      edges, it reports one rectangle at the release, the **shell** turns that into a
      `Configure` (the half that proves the compositor did not resize anything itself), the
      client accepts, and the **committed** geometry is the new rectangle.

      **Three controls, each failing at a different named assertion**, because the chain is
      ordered by mechanism and each control breaks a different link: the release reporting
      nothing fails at *"ended at …"*; the shell hearing it and sending no `Configure` fails at
      *"resize window … to …"*; a client that accepts without committing fails at a geometry
      assertion. The plan asked for the third one alone — "an outline that tracks but a release
      that does not commit … only the final assertion catches it" — and the last assertion is
      still the only one that reads the *committed* rect, which is what a client may decline.

- [x] **Only the button coming up finishes a gesture** ✅ — a lost input batch or a window put
      away mid-drag takes the outline down and asks the shell for nothing. A move has applied
      every step already, so ending it stops something on screen; a resize has applied none, so
      reporting *initiates* a change — from a pointer position a `SYN_DROPPED` has just declared
      a guess, with the button still down (PR #253 review, finding 5).

- [x] **Two deviations from decision 2, both "do not build what nothing uses"** ✅, and both
      worth a reviewer's disagreement. Decision 2 says the manager hears an event when a drag
      *begins* as well as when it ends: nothing in Part E or Part F has anything to do with one,
      so it is not built. And it says the end event carries **which zone** the gesture ended in:
      the event carries the rectangle only, because the rectangle is what a manager acts on and
      the zone that produced it is derivable from a table the manager itself registered. Part F
      adds a zone identifier if it turns out to need one — a wire change in a pre-stabilization
      protocol, against a field that would be zero and unread until then.

### Part F — snap to edge and corner

- [x] **`Manage::RegisterSnapZone`** ✅ — the shell computes eight zones from the work area (four
      edges, four corners), registers them at startup, and re-registers all eight on
      `LayoutChanged`. A table the compositor matches against, exactly as `RegisterHotkey` gave it
      chords it does not understand: first match wins, and the ordering — corners before edges —
      is the manager's because the manager wrote the table.

      **Registering an existing id replaces it**, which is where a zone differs from a chord and
      the difference is what the two tables *are*. A chord table is a set of distinct chords, so a
      duplicate id is a manager confusing itself. A zone table is a layout: the zones are the work
      area, so a panel appearing makes all eight wrong at once, and a refusal would leave a shell
      holding zones for a screen that has changed shape with no way to say so.

- [x] **The gesture** ✅ — a move-drag whose pointer enters a zone shows that zone's *target* as
      the outline, and a release inside one asks for that rectangle. Half the work area for an
      edge, a quarter for a corner, `SNAP_BAND` (24 px) wide: all of it numbers the shell put in
      the table, and none of it reachable from the compositor, which matches a pointer against a
      rectangle and reports the one it matched.

      **One event, renamed.** Part E's `ResizeEnded` becomes `Manage::DragEnded`: a resize release
      and a snap drop ask the same question and the shell's answer does not depend on which it
      was. The wire change Part E's review anticipated turned out to be the *name*, not the zone
      field — which is still absent, and still for the reason given there.

- [x] **Gate** ✅ — `check-login` steps 6k1 and 6k2. The drop asserts the committed geometry is
      half the **work area** at its origin; the control that the zones were computed from the
      screen was run and fails at exactly that line.

      **The control is a step of its own with a positive assertion**: a drag that passes through
      the zone and is released outside it must produce the *ordinary move's* geometry. Asserting
      the absence of a snap would not work — the step after it produces exactly that line and an
      `expect` scans forward — so what the pass-through asserts is where the window actually
      ended up, and the press that follows is asserted to land on its title bar. Run against a
      compositor that does not clear the zone on exit: it fails there.

      **And the gate had to slow down.** The first version injected thirty-six motions as fast as
      QMP would take them; the consumer ring overran, and a `SYN_DROPPED` ends a gesture without
      asking for anything — Part E's rule, working. A person dragging a window produces nothing
      like that rate.

### Out of scope, deliberately

- **Live resize** — a `Configure` per pointer motion. Decision 3 explains the cost and where it
  falls; the trigger is a client whose repaint is cheap enough for it to be worth having, or a
  person complaining that an outline is not enough.
- **A grid resize that does not reflow.** Decision 5 rejects it *as the destination*, and this
  is the escape hatch if Part D's reflow turns out to be as large as its own description says —
  earlier drafts offered it as the first version ("what several real terminals did for years")
  and then dropped it entirely, which left the fallback unwritten with no trigger against it
  (PR #247 review, optional 7). Trigger: Part D's rewrap not landing inside the milestone, in
  which case the resize half ships without it and the rewrap becomes its own item rather than
  blocking maximise and snap indefinitely.
- **A snap-layouts dropdown** (the Windows 11 affordance: drag to the top edge and pick a
  layout). Maintainer's call 2026-08-26, and it costs nothing to defer — it is an *affordance*
  over the identical mechanism, a menu that ends in the same `Configure`-to-a-rect. Trigger:
  wanting a layout the edge and corner gestures cannot express.
- **Themes and visual styling** — Milestone 11.
- **Server-side decorations.** Decision 1 rejects them for this system, not in general; the
  reasoning is geometry, and it would be worth revisiting if a client ever needed chrome it
  could not draw.

## Milestone 10 — applications, and drag-and-drop between them

**Details pass 2026-08-30.** Six parts in dependency order, gated one at a time, the shape M8 and
M9 used. A file browser and a text editor as ordinary applications rather than as parts to be
wired together, and the one composition mechanism that survived revision 3: **structural
drag-and-drop** — drag a file out of one window, valid targets highlight, one message on drop.

The applications come first in the ordering because the mechanism needs two honest consumers, and
because they are the two the cut argues for building properly: revision 3's conclusion was that a
workflow wanting tools tightly integrated is asking for an *application*.

**No longer here:** the patch canvas, Tier 2 durable wiring, and "what happens to a wired graph
when an application crashes" — that question was among the better arguments *for* the graph and
went with it.

### Governing decisions

Settled with the maintainer 2026-08-30, so the parts below can be built rather than re-argued.

**1. A drop carries a path, a kind and a display name — not a handle.** The obvious
capability-shaped answer is to transfer the open file, and it was rejected for a reason that only
shows up when you ask what a *refused* drop does: nobody owns that handle. The source cannot know
whether the target wants read or write, the target may never read the message at all, and a
transferred handle that is never received has no clean owner and no moment at which it is closed.
A path has none of that shape — the consumer opens it, and **a consumer that cannot open it
reports its own error**, which is a failure a person can act on.

**What this rests on, stated because it is not free.** A path is only a portable reference here
because `desktop-shell::build_app_namespace` gives *every* application the identical `/home` bind
— the user's subtree, constructed by one process the same way each time. It is not a property of
the system; it is a property of that function. **Trigger for revisiting: the first application
given a narrower namespace than its neighbours** — a sandboxed viewer, a per-application scratch
subtree — at which point a dropped path resolves for the source and not for the target, and the
answer becomes a handle with the ownership question answered, or a broker.

The **kind** (`file`, `dir`, and room for more) is what a target matches on, and is what makes
"accepts files, rejects folders" expressible without a MIME table. The **name** is metadata for a
tab or a title bar, never authority.

**2. Named acceptors are ports in waiting, and that is the shared abstraction.** A client declares
what it accepts as *named* acceptors — `open-file` accepts `file` — rather than as a bare type
list. The drop event then says "this record, for acceptor `open-file`", and a future port path
`/dev/draw/3/ports/in/open-file` writes **the same record to the same handler**. The port is a
second *carrier*, not a second feature.

This is the whole answer to "the window drop event and a port write must be two interfaces to one
functionality". It also does something for `TODO(port-shape-rework)` that no amount of design
could: that deferral is open because ports were drawn for a mechanism that was cut and nothing has
needed one since. After this milestone a port has a **name and a type taken from a shipped
consumer**, and what is left to settle — stream versus message, what a resolve does when nothing
is listening, which server owns the path — is a smaller question asked against a real example.
Ports themselves stay deferred.

**3. Regions are the client's, and cost nothing in the protocol.** The drop event carries the
pointer position in window coordinates, and `libui` routes it to the widget under it exactly as it
routes a press — so a widget gets `on_drop` and a window with one droppable panel is an ordinary
tree, not a protocol feature. The compositor highlights at **window** granularity, which is the
one thing given up: a region cannot glow. That is the same division the whole display arm already
uses — the compositor knows where a pointer is and which window it is over, and everything finer
is the toolkit's.

**4. Declared, not queried — `QueryCaps` is not the mechanism.** `ui-composition-model.md` §5 says
a drag triggers "a live capability query against visible windows". That is superseded here.
Milestone 9 spent three parts establishing that per-gesture traffic to a manager is the thing to
avoid — a queue that does not coalesce, evicting its oldest — and a round trip to *every visible
window* at the start of *every drag* is a worse version of the same mistake. A window declares its
acceptors once; the compositor matches against a table it already holds, exactly as it does for
hotkeys and snap zones.

The claim §5 makes — that this is drag-and-drop driven by **structural type match** rather than a
hardcoded MIME table — is untouched. What changes is when the match happens, not what it is.

**5. Thin but honest applications, and M12 is named for the rest.** Both applications want to
become real projects: tabs, undo, find, file operations, drag-and-drop *within* the browser. None
of that is here. What is here is the smallest version of each that is not a demo — an editor that
opens, edits and saves a real file, and a browser that lists, navigates and can hand a file to
something else. **Milestone 12 — applications, deepened** is named now (below) so the rest has
somewhere to be rather than accreting into this one.

### Part A — the filesystem helpers move below the applications

`coreutils::fs` already has what a file browser needs — `ns_children`, `is_dir`, `create_file`,
`rename`, `copy_tree`, `remove_tree`, `basename`, `join` — and it is inside a crate whose library
is *shell-program* infrastructure: the Tier-1 stage prologue, GNU-style argument parsing, TSM1
stdout plumbing. A GUI application wants the filesystem half and none of the rest, and depending
on `coreutils` to get it would make one application depend on another's crate.

- [x] **`libfs`** ✅ — above `librsproto` (it uses `session::Dir`) and below the programs that
      call it: the whole-file and path helpers, **moved rather than copied**, with `coreutils`
      depending on it. Not one line of behaviour changed, which is the point: a second consumer
      is this project's rule for when something goes down a layer, and it is the argument that
      put `BufferPool` in `libsurface` in M9 Part D rather than leaving the ordering rule in
      `nxterm`. The rule is now written in `userspace/CLAUDE.md` generally — *a helper with one
      consumer belongs to that consumer; a helper with two belongs below both.*

      **The move was clean because `fs.rs` had no `crate::` references at all** — it used only
      `alloc`, `libkern` and `librsproto`. A module that reaches into its siblings is a module
      that has to be untangled before it can move; this one had already been written as if it
      might leave.

- [x] **Gate** ✅ — `cargo xtask test` (the three host tests moved with the code, so the count is
      unchanged at 1908), plus the two guest gates that already drive this code: **`test-qemu`**
      runs `copy`, `move`, `rename` and `remove` against a real filesystem through
      `test-harness`'s demos, and **`test-interactive`** types `list` at a real prompt. Nothing
      new is asserted, deliberately — a refactor's gate is the *existing* one still passing, and
      a new test written alongside a move proves only that the new copy works.

      **Naming the wrong gate is its own defect**, and this part shipped one for a day: the
      first version credited `test-interactive` with all four programs, and it types none of the
      other three. A false statement about *which* gate covers a thing is worse than no
      statement, because it is the one somebody acts on — they run it, it passes, and the gate
      that would have caught them was never started (PR #256 review, blocking 1).

      **One thing the move did surface**: `cargo test -p libfs` alone failed where the workspace
      build passed, because `librsproto`'s `io` feature was reaching `libfs` through
      `coreutils`. Feature unification had been hiding a dependency the crate did not declare —
      exactly the shape that breaks the moment a second consumer arrives without that feature,
      which is the situation this part exists to create.

### Part B — `nxfiles`, the file browser

The simpler of the two applications and the one that needs no new widget: `list_view` exists, and
its second designed consumer was always a list of things on disk.

- [x] **List a directory** ✅, one row per entry, directories marked with a trailing separator
      and sorted before files. Reads through `libfs`, starting at `HOME` from its Tier-1
      environment record — `/home`, which `build_app_namespace` binds to the user's subtree.

      **The listing rule moved down a layer first.** "A path's entries are the filesystem's plus
      the namespace bindings mounted there, with bindings shadowing" lived in `list.rs`; it is
      `libfs::list_dir` now, shared with `list`. A browser that re-derived it would show a mount
      point twice — once as the directory it covers — and that is exactly the class of rule Part
      A's own review caught a copy of.

- [x] **Navigate** ✅ — into a directory on a row press or Enter, up on the strip's control or
      Backspace. A path strip showing where you are, which is also the affordance the address
      bar becomes later. **The keyboard and the pointer reach the same three messages**, because
      a browser where Enter and a row press disagree about what "open" means is two browsers.

      **`widget-toolkit.md` §8 names "the file browser" as the trigger for scrolling containers**,
      and this part expects not to fire it: `list_view` carries its own scroll, and a browser whose
      whole content is one list needs no container that scrolls. If a chrome element forces one —
      a path strip that must stay put while the list moves — that is the trigger firing, and it
      becomes a part rather than a quiet addition. Said here because M10 is otherwise careful about
      honouring §8's *other* trigger, the text area, and a sibling passed over in silence reads as
      an oversight.

- [x] **Open a file** by launching the editor on it ✅ — **built in Part D**, where there was an
      editor to launch. Building the mechanism here would have meant gating a launch request
      against a program that does not exist, and a row that launched nothing would be a control
      that looks live — the defect M8's overview shipped three of. The waiting also changed the
      answer: what landed is not a launch at all but `Desktop::Open`, a *path* handed to the
      shell, because a browser holds no authority to spawn and asking for one would have been
      the wrong shape however it was tested.

- [x] **Gate** ✅ — `check-login` step 7, and it uses the two sessions as one fact. The **serial**
      shell creates a directory; the **graphical** browser is then asserted to see it, descend
      into it with Enter, report it empty, and come back up with Backspace. Nothing is arranged
      by the harness: the two halves see the same subtree because `libsession::build_namespace`
      and `desktop-shell::build_app_namespace` bind it identically, which is M10's decision 1
      load-bearing for the third time.

      The count in `HOME` is *read and reported*, not asserted — what is in a user's home is the
      image's business, and a gate that pinned it would fail the first time anything else wrote
      there. What is asserted is the part the gate creates: `listed /home/papers - 0 entries`.

      **Four host controls, each watched to fail by name**: files sorting among directories, a
      file row navigating, the selection surviving a new listing, and "up" not stopping at the
      root. **Two of those tests were decoration until the control was run** — with the reset
      deleted, one arrow press leaves the selection at `Some(0)`, which is what the correct code
      produces, so both tests passed for both implementations. They press twice now.

- [x] **And the browser closes itself, which the first version could not** ✅ (PR #257 review).
      Without a close button and without honouring `CloseRequested`, the *only* way to close it
      was the taskbar's middle-click — which asks, waits out the two-second grace period, and
      then destroys the window with `Manage::Close`. That is the path the shell documents as
      being for a client that has stopped answering, and every close of this application would
      have taken it. (The grace period is gone since M12 Part A — a second middle-click is what
      insists now — which changes when that path is reached and not the point being made here.)

### Part C — the text area

The widget `libui` has deliberately not built since M4, on the stated grounds that "building an
editor's widget remains a guess at requirements no editor has yet posed". An editor is now posing
them, which is the trigger §8 named.

- [x] **A multi-line text buffer with a cursor** ✅ — insert, delete, the arrows, Home/End, Enter
      splitting a line, Backspace joining one, selection with Shift, and the scrolling that
      follows a cursor that can leave the viewport. `TextAreaState` carries all of it.

      **The wait bought a requirement a guess would not have produced**: the **goal column**.
      Without it, moving down through a short line and back up leaves the cursor at the short
      line's end — a person who pressed only vertical keys has had their column moved for them.
      That is a rule you learn from *using* an editor, not from designing a widget, which is what
      §8's "no editor has yet posed them" was protecting against.

      **And the widget takes its state by `&mut`**, scrolling it itself, rather than returning it
      for the caller to store — a widget whose correctness depends on somebody remembering
      something has the wrong signature. **`list_view` was converted to match**, since leaving
      two widgets in one toolkit disagreeing about it is worse than either choice: fourteen call
      sites, and `desktop-shell`'s three-deep chain of `(T, ListState)` returns collapses.

- [x] **What it is not** ✅: no wrapping decisions borrowed from `libterm`. The terminal's grid is a
      `custom` widget for a documented reason, and a text area is a different problem — lines are
      logical and of unbounded length, and the reflow question `libterm` answered in M9 Part D is
      not the same question. Sharing code between them is a **non-goal**, and stating that is
      what stops a plausible-looking merge later. Nothing is shared, and `widget-toolkit.md` §1's
      standing contradiction — M5 Part B saying a generic text area "would distort the whole text
      stack" while a plan item asked for one — is now closed the way waiting was supposed to
      close it: built for an editor, sharing nothing with the terminal.

- [x] **Gate** ✅ — twenty-one host tests in `libui`, and **eleven controls**, each run alone and
      watched to fail by name: no goal column, insert not replacing a selection, an unshifted
      move keeping the anchor, `selection()` not normalising, the widget not scrolling, `place`
      not clamping, backspace not joining lines, and `with_text` adding a trailing newline —
      then, from PR #258's review, the caret drawn only after the highlight, an anchor surviving
      the edit that shortens the text it names, and a multi-line selection highlighted on its
      first line only.

      **The last three are the drawing half, and it had no host coverage at all** when this box
      was first ticked: eight controls for the state, one picture for the pixels, and nothing
      counting what the widget actually emits. Both bugs the review found lived exactly there.
      A picture checks one arrangement — the reference's selection runs forward — so the caret
      was missing from every backward selection and the gate could not see it.

      `check-display`'s reference render gains the widget — **a selection spanning a line break
      with the caret at its far end**, which is the arrangement where three separate mistakes
      show as pixels: a highlight drawn over the whole line rather than the selected run, one
      that stops at the first line's end instead of continuing, and a caret at the line's end
      rather than at the cursor. The gate now compares 78,720 pixels of a 320×300 window.

      **The press-and-drag half is tested through the state rather than the `Router`**, which is
      a deviation from the box worth naming: mapping a pixel to a `(line, col)` needs the
      application's own font metrics, so the widget takes `place`/`extend_to` and the
      application does the arithmetic — exactly as `scrollbar` takes `offset_at`. Routing a
      press *to* the widget is `Router`'s and is already covered by `title_bar`'s and
      `resize_grip`'s tests; what is new here is what a press *means*, and that is state.

### Part D — `nxedit`, the text editor

- [x] **Open, edit, save** ✅ — one file, one buffer, in [`nxedit`](../../userspace/nxedit).
      Opened from `argv[1]` (which is what the shell puts there), read through `libfs::read_file`,
      written back with `libfs::write_file` + `rename`. The title bar shows the name and marks a
      modified buffer, and "modified" is derived rather than tracked: `TextAreaState` gained a
      **revision counter**, because the two alternatives are both wrong — comparing byte length
      misses replacing a one-character selection with one character, and re-deriving which
      keycodes edit is a second copy of the widget's own dispatch.

      **How the browser opens it is not a launch**, which is the part the plan did not predict.
      A browser holds no authority to spawn a program and should not: no `/bin`, no
      `BIND_NAMESPACE`. So `Desktop::Open` (`0x0C03`) carries a **path** to the shell, which
      decides what runs — the same argument decision 1 made about the drop payload, arriving in
      a second place. `librsproto::desktop` gained the client both callers share, and the
      `desktop` coreutil's hand-rolled request plumbing moved into it.

- [x] **Save is the part with a failure mode worth designing** ✅ — a temporary beside the
      target, renamed over it, with the temporary removed on either failure. What it does when
      the write or the rename fails is *report and keep the buffer*: the text stays, and it stays
      **modified**, because a buffer marked saved is one a person can close without being asked.

      **And the failure the plan did not name: a buffer that could not be read.** A failed read
      shows an empty window, and saving that over a file is the file destroyed by an editor that
      never displayed it — so a blocked buffer declines to be written and says why. A *missing*
      file is not a failure; opening a path that is not there is how a file gets made.

- [x] **Gate** ✅ — `check-login` step 8, and the assertion is made **from outside the editor**:
      the serial session creates the file, the graphical browser opens it, six characters are
      typed with a receipt each, `Ctrl+S` saves, and then the *shell* reads the file back with
      `open` and the gate matches what it prints. Asking the editor what it had saved would be
      asking the accused — and the control proves the difference: with the rename removed the
      editor still reported "saved — 7 bytes" and the gate failed at the read-back.

      The two halves see the same bytes because `libsession::build_namespace` and
      `desktop-shell::build_app_namespace` bind `/home` to the same subtree — decision 1's
      assumption, load-bearing a second time.

      **Nine host controls, each run alone and watched to fail by name**: a failed save marking
      the buffer saved, a blocked buffer writing anyway, a chord typing into the file, no trailing
      newline, a file row opening nothing, a listing keeping a stale notice, and — in `libui` —
      movement counting as an edit, typing not counting as one, and the caret guard. **Two of
      them were decoration until the control was run.** `ctrl_s` used `Ctrl+X`, which the keymap
      folds to a control byte the text area already declines, so it passed with the guard
      deleted; it uses `Ctrl+1` now, which the keymap leaves printable. And the revision test
      made a selection before typing, so the insert's own count was covered by the deletion of
      the selection.

### Part E — drag-and-drop between them

Everything above exists so that this part has two honest consumers rather than a test client.

- [x] **`Surface::DeclareAcceptor(name, kinds)`** ✅ — a client says what it takes, once. Bounded
      per window (`MAX_ACCEPTORS` = 4) and cleared with the window, because an acceptor describes
      a *window* rather than a session. Re-declaring a name replaces it, as a snap zone id does; a
      full table is refused rather than evicting one the client still believes in.

- [x] **`Surface::StartDrag(window, kind, path, name)`** ✅ — the same shape as `StartMove` and
      `StartResize`, refused the same way, and `Surface::Dropped` carries what the plan said it
      would. Released anywhere else, nothing is sent and the drag is over.

      **The exclusion runs in every direction, and a test found that it did not.** `start_move`
      and `start_resize` each refused while the *other* ran, and both happily started while a
      drag was in flight — a window would have followed the pointer while a payload was in flight
      out of it, and the release would have meant two things at once. One grab carries one
      gesture; the compositor now says so three ways rather than two.

- [x] **The highlight is the compositor's own drawing** ✅ — the same `Outline` a resize moves and
      a snap zone previews, so its damage is the four edge strips M9 Part E already computes. It
      outlines the window a drop would land on *right now*, and never the window the drag came
      out of: a browser that also accepted drops would otherwise outline itself the instant a
      drag began, which is a gesture nobody is making.

      **One colour for all three**, which is a theming decision deferred rather than taken: M11
      owns what a drop target looks like, and a second colour chosen here would be a guess made
      in the wrong milestone.

- [x] **`libui` gains `on_drop`** ✅ — `Router::drop_at`, routed by position like a press
      (decision 3), walking up to a handler on an ancestor exactly as a press does. `nxedit`
      takes a file on its **text area** and not on its title bar, which is the whole demonstration
      that a region costs the protocol nothing.

      **The message carries no payload**, deliberately: an `Option<Msg>` cannot be parameterised
      by a value the toolkit does not have, and the payload is already in the event the
      application is holding when it calls the router. What the toolkit answers is *which widget*.

      `list_view` gained a press-**down** hook in the same part, because a drag out of a row is
      decided when the button lands on it. The row a press landed on is a fact the widget has and
      its caller would otherwise recompute from the pointer's y, the row height and the scroll
      offset — three numbers to keep in step, which is how two implementations of "which row is
      that" come to disagree.

- [x] **Gate** ✅ — `check-login` step 9: the editor is snapped to the right half (M9 Part F's own
      gesture, so the geometry is the work area's rather than a number the harness invented), the
      browser is walked out and back into the directory so it lists a file the *serial* side just
      made, and that row is dragged across and dropped on the editor's document area. Four
      assertions in the order the mechanism goes: the browser says it is dragging, the compositor
      says it took the gesture, the compositor says which window and acceptor the drop landed on,
      and the editor says it opened the file.

      **Both windows' positions are read from the shell's own geometry lines**, not computed:
      re-deriving the placement cascade in the harness would be a second copy of a policy that is
      the shell's to change.

      **The two controls are host tests in `compositor::input`, which is stronger than a gate
      run**: a `dir` dragged over an editor that declares only `file` highlights nothing and
      delivers nothing, and a drop over a window with **no** acceptor does the same — so
      "declares nothing" cannot come to mean "takes everything". Eight tests in all, including
      the source window never highlighting itself, and every way a gesture can end without the
      button coming up delivering nothing.

      **What the gate cannot see is the highlight**, and the plan should not pretend otherwise:
      it is pixels the compositor draws over the composed stack, and `check-display` boots a
      `--selftest` image with no drag in it. The host tests assert the rectangle; the gate asserts
      the delivery.

### Not a part: graduating `display-substrate.md`

**It was M9's, it did not happen, and it is done now** (2026-08-30) rather than being reassigned
here. This details pass proposed it as a Part F, which put three statements in disagreement — the
table above said M9, the document's own Status said "not M10", and the new part said M10 — and
that disagreement is what made the missed checkbox visible at all. M9 built the last substrate
piece and shipped six parts without the box; reassigning the debt to a later milestone would have
been the third repetition of a failure the paragraph above already exists to prevent. See
`decision-log.md`, 2026-08-30.

That M10 Part E *adds* to the substrate is not a reason to have kept it in `design/`: an
`architecture/` document absorbing new mechanism as it lands is what those documents are for.

### Out of scope, deliberately

- **Ports** (`TODO(port-shape-rework)`) — decision 2 gives them a name and a type from a shipped
  consumer, and leaves the rest deferred. Trigger unchanged: the first client that needs to
  address a window from a command line.
- **Drag-and-drop within one application** — reordering a list, moving a file between two panes of
  the browser. The mechanism here crosses windows; a drag inside one window is the toolkit's and
  is M12's.
- **A drop that starts outside a window** — dragging from the desktop background or the taskbar.
  Nothing owns those pixels as a drag source today.
- **Cursor feedback during a drag.** There are no per-client cursor shapes, so the highlight is
  the whole of the feedback. Filed with per-client cursors, which the substrate doc already
  defers.

## Milestone 11 — themes and visual polish

**Details pass 2026-09-01.** Sketched 2026-08-26 and planned now that M10 has closed, which is
when its own trigger fired: there is something to polish. Six parts, and the fifth is not like
the others — it is open-ended by nature, and the plan says so rather than pretending a list of
checkboxes can describe taste.

**What the sketch got right and what it had backwards.** It expected the work to be "a theme the
shell, the toolkit and the decorations share, rather than three sets of hardcoded colours". The
seam turns out to be in better shape than that: every client already calls `Palette::default()`
and `Theme::default()`, so the sharing is mostly plumbing a value through. What is actually
scattered is smaller and more specific — the compositor's own chrome (cursor, drag outline,
background), which does not link `libui` at all, and `libterm`'s ANSI palette, which is a
terminal's own thing and not the desktop's.

**And the largest visible change is not a colour.** The entire desktop renders in
`DejaVuSansMono` — the only font the image ships — because `SYSTEM_FONT_PATH` is a constant every
client loads. A proportional UI font is one asset and one theme field, and it changes every label
in the system.

### Governing decisions

Settled with the maintainer 2026-09-01, so the parts below can be built rather than re-argued.

**1. The theme is data read at session start — not code, and not a live protocol.** The shell
reads a theme file and hands each application its values on the **setup channel it already sends
`HOME` on**, so nothing new goes on the wire. A change takes effect when an application starts.

The reasoning is that nothing *else* needs more than this: polish iterations rebuild the image
anyway, so a live push would be protocol work bought entirely for the control panel. **Trigger
for revisiting: a control panel that must show a change without a restart.** The shape it would
take is already known — a server-to-client event, exactly M10 Part E's `Dropped`.

**2. Colour and type are themeable; chrome metrics are not.** Font size and the palette move;
padding, title-bar height and grip size stay constants. The consequence is named because it is
the reason: `check-login` clicks a title bar at `+13` and a close button at `-39`, and gates that
derive those from a theme would have to read one. Only `check-display`'s reference moves, which
is a gate designed to move.

**3. Two font roles, not one — and the UI role becomes proportional.** `nxterm`'s grid needs a
fixed advance and keeps the mono font; every label in every other window does not. This is the
single largest visible change in the milestone and it costs one asset, its licence, and a field.

**4. One theme, built to hold a second.** Every polish decision is made once and `check-display`
keeps one reference. Dark-and-light doubles both, and doubles the review it takes to land each
batch. The mechanism carries a second theme; nothing ships one.

**5. The polish list is an input this plan does not contain.** It is written by the maintainer
while test-driving, one line per thing that looks or feels wrong, and Part E works from it.
**M11 ends when that list is empty** — anything new found along the way goes to a second list for
M12 rather than extending this one. A milestone about taste needs a stopping condition agreed
before it starts, or it does not have one.

**6. Feel is not appearance, and is not in this milestone.** "Moving a window is slow" (reported
2026-08-28, on TCG) is recompose and damage work; it shares nothing with a colour but the window
it is noticed on. Its own list, and its own milestone.

### Part A — `xtask preview`, because it changes the cost of everything after it

- [x] **Render an arrangement of widgets to a viewable image on the host** ✅ —
      `cargo xtask preview [ui|term|all]`, writing `tools/build-cache/preview-*.png`. A new entry
      point onto the renderer `check-display` already adjudicates with, not a second renderer.

      **First, because it is what makes the loop affordable.** Polish is a hundred small
      judgements, and a judgement that costs a boot to see is a judgement not made.

      **An external crate, and the first in `tools/`.** The forbidden list is the *kernel's*, and
      `userspace/CLAUDE.md`'s bar is about what ships in the image; this is build tooling that
      runs on the host and reaches no target. A hand-rolled PNG writer was started and thrown
      away — a stored-block encoder is a hundred lines of checksum arithmetic standing between a
      judgement about how something looks and seeing it, which is precisely the cost this part
      exists to remove (settled with the maintainer 2026-09-01).

      **What it does not show, stated rather than discovered later**: anything the *compositor*
      draws — the cursor, the drag outline, the background between windows — and the arrangement
      of real windows on a real screen. Those are composed in the guest by clients that have to
      run. What it covers is the toolkit's own surfaces, which is where most of the polish lives.

- [x] **Gate** ✅ — a host test with two claims, and **three controls, each run alone**. The
      *sizes* are the display gate's own constants, so a preview that rendered an arrangement of
      its own would fail: the control is a preview-only frame, and it does. And the PNG is
      **decoded back** and compared to the framebuffer pixel by pixel, which covers the
      conversion — the toolkit reference's pitch is 1292 for a 1280-byte row and `XRGB8888` is
      little-endian, so a direct copy is wrong twice over. Both controls fire: channels swapped
      fails at pixel 0,0, and the pitch ignored fails at pixel 0,1 — the first pixel of the
      second row, which is exactly where a stride mistake begins.

### Part B — one theme, with `libdraw` as the seam

- [x] **Collapse `Theme` and `Palette` into one theme, and give the compositor's chrome the same
      source** ✅ — `libdraw::theme::Theme`, with a **`const fn` constructor** so the compositor's
      cursor and outline colours stay `const` items while still coming from here. The compositor
      does not link `libui` and should not; `libdraw` is what both link.

      **And it reached one crate further than the plan said.** `libterm`'s default foreground and
      background — what `Colour::Default` means — now read from the same theme. They had been a
      pair of literals in two crates kept equal by a host test in `xtask`, written when "a theme
      colour in `libdraw` would make the pixel layer own a theme"; Part B did exactly that on
      purpose, so the equality is a shared constructor rather than a coincidence two crates
      maintain. **The sixteen ANSI colours stay the terminal's**: they are what a program
      addresses with `ESC[31m`, and retheming a desktop must not retint `ls`.

- [x] **The compositor's three colours stay compiled in** ✅ — the one thing decision 1's file does
      not reach, because the compositor is started by `init` and never sees a setup record. Named
      in the code rather than quietly skipped. **Trigger: a control panel that wants to change the
      cursor or the drop highlight** — a manager op on a channel the shell already holds.

- [x] **Gate** ✅ — `check-display` green, **and the picture byte-identical to `main`'s**, which is
      the claim the gate alone cannot make: it compares the host's render against the *guest's*
      screen, and a theme change moves both together. Part A's `preview` is what makes the
      stronger check a one-liner — render `main` in a worktree, `md5sum` both PNGs — and both
      match. A refactor that moves no pixel is exactly what this part is.

      `xtask`'s cross-crate colour test survives with a **different claim**, and the first
      version of that claim was wrong. It asserted that no ANSI colour equals a *chrome* colour —
      provenance encoded as inequality — and the counterexample was in the palette itself:
      `ansi[0]` and `title_inactive` are both `#1C222A`. It passed only because `title_inactive`
      was the one theme colour left out of the list it checked (PR #262 review, blocking 1). What
      it asserts now is what comparing values can establish: the grounds follow the theme, and no
      cell colour equals the ground it is drawn on.

### Part C — the theme becomes data

- [x] **A theme file, read by the shell, handed to each application on the setup record** ✅ —
      `/home/theme.toml`, parsed once at session start and put on the environment record every
      launch already carries. Schema: [`theme-toml-schema.md`](../spec/theme-toml-schema.md).

      **In the user's home rather than `/etc`, which is a namespace decision.** A session
      namespace binds `/home`, `/bin`, `/dev/tty` and `/system/fonts` and has no `/etc`, and
      `session-mgr/CLAUDE.md` requires that adding a member be decided each time. A theme is a
      *user's*: the subtree they already own needs no new authority, and it is somewhere a person
      can actually delete — which is what makes the control below a real one rather than an
      assertion. A system default under `/etc` merged beneath it is the obvious next step;
      trigger is a second user, or a control panel offering "reset to system".

      **The shell's own bars follow it too**, not just the applications it launches — otherwise
      the file themes the windows and not the chrome around them.

- [x] **Gate** ✅ — `check-login` asserts the whole path in the order it happens: the shell reads
      the file before it draws anything, and `nxfiles` reports the `font_px` that reached it. One
      number rather than a colour, because colours are pixels and this gate boots a release image
      with nothing rendered to read.

      **The staged file carries `font_px = 14`, which is not the built-in 16** — and that is the
      gate rather than decoration. A gate asserting the default proves nothing: a client that
      never received the theme reports the same number, so the assertion passes with the wire
      cut. It also asserts the shell's resolved size and the client's *agree*, which holds
      whichever way the theme came.

      **Two controls, each run alone.** Delete the shell's `with_str_field` — the theme never
      reaches the wire — and the shell says 14 while the client says 16, so the gate fails. And
      **the file deleted entirely** (with the one staged-value check removed, which the code
      names): the shell logs `theme /home/theme.toml absent; using the built-in theme, font_px 16`,
      the client agrees at 16, and the *whole gate passes* — greeter, bars, work area, snap zones,
      the modal, the editor, the drag and the drop. A theme mechanism a missing file can break is
      worse than no mechanism, and that is checked rather than claimed.

      The absent-file *decision* is also a host test on `Theme::from_config`, where an empty
      file, a comment-only file and a file of typos are each exactly the built-in theme.

### Part D — the UI font stops being a terminal font

- [x] **A proportional UI font in the theme, and the mono font kept for the grid** ✅ — two keys,
      `font_ui` and `font_mono`, holding paths rather than one `SYSTEM_FONT_PATH` every client
      loaded. The old constant is *deleted* rather than aliased, so a call site that did not
      choose a role does not compile; `nxterm` is the one program that loads both, because its
      menus are widgets and its grid is not.

      **One asset and no new licence question**: `DejaVuSans.ttf` beside the mono face it already
      ships with, and DejaVu's notice covers the family in one `Files: *` stanza. 760 KiB on the
      root filesystem, where the font has always lived — nothing that draws text runs before the
      root is mounted.

      **A path, not a name, and bounded at 64 bytes** so a `Theme` stays `Copy` and
      `const`-constructible — the compositor keeps theme colours as `const` items and a heap
      allocation cannot appear in a constant. The bound is also what keeps a theme file from
      overflowing the 4 KiB setup record it travels on. A path that does not load falls back to
      the built-in face for its role and says so, because whether a path resolves is a question
      only the *application's* namespace can answer, not the shell that parsed the file.

- [x] **Gate** ✅ — `check-display`'s reference moved and both sides moved with it: the host now
      renders each reference with the file `Theme::dark()` names for that role, through the same
      mapping the image build stages with, so "the guest reads the font the host drew with" is a
      property of the build rather than two lists kept equal by hand.

      **The pixel comparison cannot make the claim on its own**, which is the trap this part had
      to be built around: it checks that the host and the guest *agree*, so a swap on both sides
      at once stays green. Two assertions cover what it cannot. `check-display` expects the guest
      to name both faces, in the order it loads them — that catches a client taking the wrong
      role. And a host test asserts the two roles are *different files* — that catches the theme
      collapsing them, which is the one-constant version of the same mistake and the state the
      system was in before this part.

      **`check-terminal` proves the grid still measures with a fixed advance**, by recomputing
      the cell on the host from the same file at the size the guest reports, and comparing.
      Handing `libterm` a proportional face is not a crash: it takes a cell's width from one
      glyph's advance, so it gets a plausible number and then draws every column at the wrong x —
      and every other assertion in that gate is about cells rather than pixels, so all of them
      would still pass. Controlled: measuring with the UI face fails with
      `the guest measured a 12x16 cell … the host makes it 9x16`. The property itself is a host
      test in `libterm`, where the negative control is that the desktop's own font fails it.

      **Two refinements from the review, both about a program reporting what it asked for rather
      than what it got.** The themed loaders return the path they actually *opened*, because a
      fallback is invisible otherwise — and `nxterm`'s line is not decoration, the gate feeds it
      back into `host_font`. And the size is printed as whole pixels and hundredths rather than
      truncated, with `from_config` rounding there, so "the size printed is the size used" holds
      for every value the system can carry rather than only integral ones.

### Part E — the polish passes

- [x] **A way to look at the whole desktop** ✅ — `cargo xtask shot`, which is the half Part A's
      command said it could not be. `preview` renders the toolkit's own surfaces on the host in a
      second; its doc names what that structurally cannot show — the cursor, the drag outline, the
      ground between windows, and how real windows sit next to each other. So this **photographs**
      rather than renders: it boots the release image, drives it to four moments (the greeter, the
      bare desktop, the applications modal, two real windows) and writes what QEMU says is on the
      display. A photograph adds no second renderer to keep in step with the first, which is the
      trap `preview_frames` exists to avoid.

      Several moments per boot, because the boot is the cost. It is a tool and not a gate: it
      asserts only enough to know the picture is of a working desktop rather than a blank screen,
      that being the one failure that would otherwise be read as a design opinion.

- [ ] **The list itself** — [`m11-polish-list.md`](m11-polish-list.md), the maintainer's, written
      while driving. Decision 5 says this plan does not contain it; what this box does is give it
      a file, and a place for the two kinds of thing that are *not* polish to go instead (feel,
      and M12 capability).

- [ ] **Batch 1 — the palette turns light** ✅ (2026-09-01). One theme, and `Theme::light()`
      *replaces* `Theme::dark()` rather than joining it: decision 4 applied, because two themes
      double the reference pictures and double the judgement each polish item takes. Values are
      measured from the maintainer's reference desktop rather than invented.

      **Three things the light theme forced**, each recorded where it happened. `background` split
      into two fields — it was also the ground *between* windows, on the argument that a seam
      shows when a client's buffer is smaller than its frame, and a light theme ends that because
      the two stopped being the same kind of thing. `outline` had to become saturated, being the
      one colour composited over *both* grounds. And the terminal's grid keeps its own dark
      ground: Part B tied it to the theme when there was one theme and it was dark, and turning
      the desktop light is exactly the event that shows the tie was to the sixteen ANSI colours,
      which are tuned for a dark ground and would put invisible text on a white one.

- [ ] **Batch 2a — the chrome grows a third dimension** ✅ (2026-09-01). Gradients, drawn window
      controls, and a line around anything with an edge.

      **One bevel number for every gradient**, not a pair of colours per surface: the reference's
      own gradients span ±10 and ±14 around their midpoints, so one amount reproduces both, and a
      palette keeps one value coherent rather than eight. `Node::Bevel` is a second fill rather
      than a flag on the first, because the two differ in what they need to know — a flat fill is
      correct from the clip alone and a gradient is only correct from the node's rect, so a
      one-row repaint of a gradient must draw the same picture as a full one. That is a host test.

      **The window controls are drawn, not typed.** `_`, `[]` and `X` were three characters
      standing in for three controls on a bar already full of text. Shapes cost a paint arm;
      images would cost a decoder, an asset path and a size convention — filed, per the settled
      decision, and the trade is worth revisiting when something needs an icon that is not three
      strokes.

      **And a selection is blue with a darker edge**, which is the second half of the request and
      the half that matters: without the border, two adjacent selected rows read as one block.

- [x] **Batch 2b — the window frame** ✅ (2026-09-01). A one-pixel edge, three pixels of frame on
      the left, right and bottom, and the title bar flush at the top — which is what the reference
      does and is not arbitrary: a title bar *is* the window's edge, the thing you grab to move
      it, and insetting it would put a strip of frame above a bar that already reads as one.

      **Held back from 2a because it moves geometry.** `window_frame` publishes what it costs, and
      three applications subtract it; `nxterm` needed one pair of constants where it had three
      open-coded sums of `BAR_H + TITLE_BAR_H` that agreed only because nothing had ever been
      added between a window's edge and its content.

      **The frame's dock wraps both children**, because the diff requires a container's children
      to be all keyed or all unkeyed and every caller keys its title bar. The alternative was for
      the toolkit to invent a key in the application's namespace.

      **And it surfaced a gate bug that had nothing to do with it.** `check-login`'s drag ignored
      the six slop motions when computing its walk, so the pointer ended clamped against the
      screen's right edge rather than at the target — which landed on the editor's text area
      anyway, because the area reached the window's last pixel column. Four pixels of frame moved
      the content in and the drop started landing on the frame.

- [x] **Batch 3 — the pointer starts meaning something** ✅ (2026-09-01). The second half of the
      menu request, and it turned out to be the whole system: **nothing had ever reacted to the
      pointer being over it.** `Router::inside` has reported the widget under the cursor since M4
      and `WidgetState::hovered` has existed just as long; no application had ever connected them,
      so every button, row and menu item painted its resting face whatever the pointer did.

      A menu row now highlights the way a selected list row does — they are the same thing seen
      twice, the item that would happen if you acted now — and a hovered list row gets the face
      rather than the blue, because two highlights of equal weight is two answers to one question.

      **`Router::hovered_key` exists because two id spaces meet here.** `inside` is a diff-tree id
      and `.key(…)` is the application's own numbering; comparing one to the other compiles and
      returns a stable wrong answer. That is what shipped first, and `check-terminal` caught it —
      a menu item keyed 2 reporting as 4.

      **The shell's modal is the one surface still without hover**, and it is a shape rather than
      an omission: `desktop-shell` has no `Router` at all, hit-testing the modal's coordinates by
      hand. Giving it feedback means giving it a router first, which is a change to how it handles
      input rather than to how it looks.

- [x] **Batch 4 — the applications menu, and two small things** ✅ (2026-09-01). Four reports with
      one cause: the shell read pointer events for the overview's thumbnails, the applications
      button and the taskbar, and **never for the modal's own window**. So its rows could not be
      clicked, nothing under the cursor reacted, and a click aimed elsewhere was never seen at
      all. It has a `Router` now — the modal's contents are a widget tree that filters and
      scrolls, and hit-testing that by hand would be the toolkit's layout written twice.

      Placement was separate and simpler: a popup created with `new` takes its parent's origin,
      so the modal covered the bar it dropped from. `nxterm`'s menu has always used `at`.
      Dismissal needed a signal — `InputLost` is queue overflow, not focus, and reading it as one
      would close the modal on a burst of motion; `WindowEvent::Focus(false)` is the right one and
      already reached clients.

      **Three latent bugs surfaced on the way**, none of them in what was reported: two row
      builders keyed the same row differently the moment a query was non-empty; the shell's
      placement line hardcoded `at 0,` — a value written into the line that reports it, right
      until the cascade moved; and `check-login` measured every title-bar button from a window's
      *width*, which is its right edge only while windows start at x=0.

      **No hover receipt for this one, deliberately.** `nxterm` reports its menu hover because a
      gate has no other way to see it. This shell has zero build-mode `cfg` sites — the state the
      test-path retrofit exists to preserve — and `check-login` boots the release image, so such a
      line would be both a reintroduction and invisible to the gate wanting it. The click proves
      the router; hover rides the same router.

- [x] **Batch 5 — the highlight is blue, and a press outside really does dismiss** ✅
      (2026-09-01). Both were reported as still broken after batch 4, and both were right.

      **The highlight** was batch 3's rule applied to a list that has no selection: hover was
      "quieter than the selection and loses to it", and the applications modal keeps no selection
      at all, so every hover took the quiet branch. The rule is still one primary highlight —
      where there is no selection to compete with, the pointer's is not competing.

      **The dismissal** needed a protocol op. Batch 4 used `Focus(false)`, which arrives when
      something *raises* — so clicking another window worked and clicking the desktop or a panel
      did not, because focus here is a consequence of stacking and neither raises anything. The
      gate proved only the case that worked, having noted the other in a comment. `Dismissed`
      (`0x0931`) is the compositor saying a press landed elsewhere, which is the half a client
      cannot see: a popup's owner never receives a press aimed at another window.

      **Its own op, not a second meaning for `CloseRequested`** — that one may deserve a "save
      first?" and this deserves nothing but going away, and two things read as "close" in one arm
      is what `Dropped`/`InputLost` had to be renamed out of. **And no parent exemption**: a first
      version spared the popup's parent to protect the button that opened it, which was wrong
      twice — the parent is the whole bar, and dismissing is what makes clicking that button again
      close what it opened.

- [x] **Batch 6 — a scrollbar you can drag** ✅ (2026-09-01). The arithmetic was never the
      problem: `ScrollState::offset_at` has been right since M5 and `nxterm` has dragged its grid
      with it ever since. `list_view` built a scrollbar and gave it **no pointer handler at all**,
      so a list showed its position and could not be moved — a control that looks live and is not,
      which is the defect this toolkit's own notes keep naming.

      The conversion lives on `ListState` rather than in each caller, so a list's thumb and a
      terminal's cannot drift apart on rounding. Two consumers wired it, and the launcher needed a
      second fix to go with it: its list state was rebuilt every frame — true to "the launcher
      keeps no selection", and it also meant the scroll offset reset every frame, so `/bin`'s 26
      entries were ten reachable rows and a filter.

      **The control is the `None` case.** Asserting that the bar carries a handler would pass for
      a widget that attached one unconditionally, which is a different bug.

- [x] **Batch 7 — the editor opens untitled** ✅ (2026-09-01). "nxedit doesn't launch from the
      menu" was true in the most literal way: it required `argv[1]`, the applications modal passes
      none, so it printed "no file to edit" and exited. The refusal had a reason — M10 Part D
      declined an untitled buffer because there was no way to ask for a name — and the answer is
      to ask.

      **In the editor, not through the shell.** The details pass offered a `Desktop` op that would
      have the shell collect a name; that would make the shell a dialog provider for arbitrary
      clients, which is an authority question, and would need a blocking exchange over an async
      protocol. A field in the editor's own status strip is no protocol at all — and this crate's
      key path already carried a note that "the first widget that wants a key needs exactly this
      shape". This is that widget.

      **Naming is a mode**, the editor's first: while a name is being typed the keys are the
      field's, buffer and chords included, and the strip shows the field instead of the status. An
      empty name is refused rather than treated as one, because it would write to the directory.

      Gated end to end: launched from the menu, and the *placement* is the proof rather than the
      launch — the shell said the same thing before, and the editor exited before creating a
      window. Then Ctrl+S, a name, and the file written into the session's `/home`.

- [x] **Batch 8 — taskbar buttons, and a greeter that could not be centred** ✅ (2026-09-01). Two
      small requests, one of which was not small underneath.

      **The taskbar entries are bordered boxes.** They were labels on a flat bar, so two windows
      read as one line with a gap in it; the focused one is filled as well as marked, from the
      same flag the glyph comes from.

      **The login prompt could not be centred, and finding out why is the batch.** A `normal`
      window's requested origin was discarded — zeroed by the encoder, ignored by the parser, and
      overwritten by the compositor — on the rule that a manager places it. True, and it leaves no
      answer for a window that exists *before* any manager: the greeter is what somebody uses to
      start the process that would place it.

      So the offset is now a **preference** for `normal` and `dialog`: the first configure is
      still held for a manager, so with one attached nothing changes, and it is the window's
      origin only when nobody is managing. A `panel` still discards it — its role already names
      the edge it docks to.

      **Three layers had to agree**, which is why the first two attempts changed nothing: the
      compositor placed it at the origin, then the encoder zeroed it, then the parser would have.
      Each had its own comment explaining that the offset was a popup's.

- [x] **Batch 9 — a clock on the top bar** ✅ (2026-09-01), the first of the three stretches.

      **No timer object.** `sys_wait` already takes an absolute monotonic deadline and the shell
      already computes one for the close it may have to insist on; the clock's next change is one
      more candidate for the same minimum. A bar that ticks costs one wake a minute and no new
      kernel object. The alignment is read from the *wall* clock and the deadline is *monotonic*,
      which is not a mix-up — one says how far into the minute we are, the other is what the wait
      counts.

      **`YYYY-MM-DD HH:MM`, UTC, and no month names.** There is no timezone database and no
      locale, so a localised form would be a fiction — the reason `date` emits fields and prints
      UTC — and it means the bar and the command agree about what time it is. **Empty when the
      clock is unset**, rather than 1970: the kernel reports the RTC as unreadable rather than
      inventing an epoch, and a bar showing a fabricated date would undo that one layer up.

      **The calendar arithmetic moved below both consumers**, from `coreutils::time` to a new
      `libtime`. That is `userspace/CLAUDE.md`'s rule applied on the day it triggered: a helper
      with one consumer belongs to that consumer, a helper with two belongs below both — and the
      shell reaching into `coreutils` for it is the shape that rule exists to catch.

- [x] **Batch 10 — desktop previews, and an image subsystem that turned out not to be needed** ✅
      (2026-09-01). The request read as "this needs image support"; it does not.

      **A sidebar row is a desktop that is not being composited**, so there is nothing to
      photograph — the overview's existing thumbnails are captures of the *current* desktop's
      windows, and the compositor can only capture what it composites. What the shell does have is
      every window's origin, size and desktop, kept for the taskbar. So a miniature is arithmetic:
      the desktop's ground, with a bordered box where each of its windows is, scaled by the
      screen's own ratio. No capture, no scaling of pixels, no decoder.

      **The sidebar stopped being a white sheet.** `paint` clears to `background`, which since the
      theme turned light is the white an application draws on — so the sidebar was a white column
      down the side of a blue desktop, which is what the request called out first. It is the
      desktop's own ground darkened, with the window ground as ink: derived from two colours the
      theme already has, so a new palette needs no extra decision. **Translucency is what was
      asked for and it still waits on an alpha channel** — that decision is unchanged.

      `cargo xtask shot` gained a fifth moment, because the overview is the one surface with no
      other way to be looked at.

- [x] **Batches, from the maintainer's list, each ending in a preview and — where the item is
      behaviour rather than appearance — a boot.** ✅ Ten of them, in three passes of feedback,
      recorded above and in [`m11-polish-list.md`](m11-polish-list.md).

- [x] **No checkbox list here on purpose.** ✅ The shape held: batch, look, review, apply. What it
      produced that the plan did not anticipate is that **most of the second and third passes were
      defects rather than taste** — a menu whose rows could not be clicked, a scrollbar with no
      pointer handler, an editor that exited when launched, a window that could not ask to be
      centred. Polish is where a system is used for the first time in the way a person would use
      it, and that is what finds those.

- [x] **The stopping condition, and what "empty" turned out to mean** ✅ (2026-09-01). Decision 5
      says M11 ends when the list is empty. It is not literally empty; what is true is that
      **nothing left on it is polish**: real icons and a background image wait on the images item
      filed for M12, and transparency waits on an alpha channel `libdraw` rules out today. Drop
      shadows are the third, and they are held for a reason worth keeping — a shadow makes every
      window's damage region larger than the window, and the compositor clears before it draws
      straight into the scanned-out framebuffer, so shadows would make the flicker they sit on top
      of worse. They belong after the feel work, not before it.

### Part F — the control panel, not built and no longer scheduled (2026-09-01)

- [~] **Desktop settings a person can drive**: the theme file above, and the desktops
      `/dev/desktop` already serves. Its scope is stated here so that slipping it is a decision
      rather than a disappearance — **and it slipped**. The details pass allowed exactly this, and
      the reason it is the right call rather than the convenient one is decision 5's other half:
      polish is what this milestone is for, and a settings application arriving *instead of* a
      finished polish list was named as the wrong trade before either existed. The list is
      finished; this is not started.

- [~] **It did not move to M12 either — it became a trigger**: *when the settings outgrow a
      hand-edited file*, on the maintainer's judgement after living with the theme file for a
      milestone. A part deferred twice is a part nobody has needed, and a schedule slot would be
      the third deferral rather than the first delivery. Recorded in
      [`deferred-decisions.md`](../rationale/deferred-decisions.md) with the rest, since it now
      belongs to no milestone.

- [ ] **Gate, kept with it**: `check-login` drives it — change a setting, and read the *file* back
      the way Part D of M10 reads a saved buffer back, from outside the application that wrote
      it.


## Milestone 12 — applications, deepened

Named 2026-08-30 as part of M10's details pass, so that "the editor should have tabs" has
somewhere to go that is not M10.

Both applications ship thin in M10 and both want to be real: **tabs** in each, **undo/redo** and
**find** in the editor, **file operations** in the browser (rename, delete, copy, new folder, with
the confirmation dialogs they imply), and **drag-and-drop within** a window. None of it is
designed here; what this entry does is exist, so the line M10 draws is a line rather than an
omission.

**And images** — filed here on 2026-09-01, out of M11's polish list, because it is a subsystem
rather than a batch. Two of that list's items wanted it: a background image, which M12 Part F
builds, and window controls drawn from an icon set, which it does **not** — an icon set is a
naming convention, a size convention and a lookup path, and stays filed behind the decoder that
would make it possible. What it is *not* wanted for is the desktop
previews, which turned out to need only geometry the shell already holds (M11 Part E batch 10).

Four options were costed, and the choice is deliberately not made here:

| | Guest cost | Build cost | Disk, 1280×800 |
|---|---|---|---|
| Raw pixels staged at build time (P6) | ~40-line reader; the *writer* in `libdraw::ppm` is 10 | `xtask` converts with the `png` crate it already has | 3.0 MB, uncompressed |
| QOI | ~250–300 lines, no dependency | ~200-line encoder, or the `qoi` crate host-side | ~400 KB–1 MB |
| PNG in the guest | inflate (~400+ lines), unfiltering, refuse interlaced — or vet a `no_std` crate's whole tree | none | ~300–800 KB |

The cost that makes it a milestone item is not any one of those: it is a **format decision, an
asset pipeline in the image build, a size budget, and a place in the layering** — plus, for the
wallpaper specifically, the **shell-owned background window** M11's settled decisions sketched
(the shell holds `/home` and a theme; the compositor holds neither and should not gain a
filesystem to draw wallpaper).

**Its trigger is M10 landing**, and its ordering against M11 (themes and visual polish) is open —
polish wants something finished to polish, and these are the two applications that will most show
it.

### Scope, settled 2026-09-01

M11's close is what forced this: the polish list emptied into things that were not polish, and
they needed somewhere to go that was not "M12, eventually".

**Three strands, and the parts that carry them are in the details pass below** — this list is
what the milestone is *for*, not how it is cut up. (It assigned its own A/B/C before the details
pass existed, which left two incompatible letterings dated the same day; PR #266 review, finding
2.)

- **Application depth.** Tabs in each, undo/redo and find in the editor, file operations in the
  browser (rename, delete, copy, new folder), and drag-and-drop *within* a window. Its
  confirmation dialogs make this the first *application* to create a `Role::Dialog`.
- **Copy and paste.** The largest strand, and the one that needed a decision before it could be
  scoped.
- **Images.** The decoder, the asset path, and the wallpaper as a shell-owned background window.

**What moved out.** The compositor work is [Milestone 13](#milestone-13--the-compositors-feel):
different work, driven by measurement rather than by use, and a milestone holding both cannot say
when it is done. **The control panel is trigger-gated rather than scheduled** — see below.

### The control panel is a trigger now, not a part

It was M11 Part F, "allowed to slip", and it slipped. Rather than move it a second time, its
condition is written down: **when the settings outgrow a hand-edited file.**

That is the maintainer's own judgement, on 2026-09-01, after living with the theme file for a
milestone: *"the file is fine. A control panel will be needed when we have a lot more settings and
the file becomes cumbersome."* A part that is deferred twice is a part nobody has needed yet, and
a trigger says that honestly where a schedule pretends otherwise. Its scope is unchanged and its
gate is unchanged; what it loses is a slot it was not going to fill.

`form` (see [What this unblocks](#what-this-unblocks)) is now *eligible* by its own stated
condition — "after the toolkit and the first applications" — and is deliberately left filed
rather than scheduled, for the same reason.

### Decision 1 — the clipboard is a resource server, and a binding is the authority

Settled with the maintainer 2026-09-01, so Part E can be built rather than re-argued.

**Its own process, with its endpoint bound per session.** A clipboard is shared mutable state
between mutually untrusting programs, and "anything running may read what you last copied" is
ambient authority — the mechanism by which a password manager's clipboard gets scraped on real
systems. This system already has the answer: **you can read the clipboard if it is in your
namespace**, and rights are attenuable, so a profile can be given a write-only clipboard or none
at all. No new machinery, and a capability story Wayland does not have.

**It stores, which is the point of choosing it over offers.** The alternative considered was
Wayland's model — the copier keeps the data and the compositor brokers a transfer on paste, so
nothing is stored and no third party ever holds your text. It was rejected for one reason: the
clipboard dies with the application you copied from, which is the behaviour Linux users install
clipboard managers to escape. Copy from the editor, close it, paste.

**The binding is the authority, and focus-gating is deferred.** A read succeeds for anyone holding
the endpoint, whenever they like — consistent with every other resource here. Focus-gated reads
are what modern desktops do and would close background scraping *within* a session, at the cost of
a dependency between the clipboard server and the compositor and a read that fails for reasons its
caller cannot see. **Trigger: an application inside a session that the person does not trust** —
which is the day profiles stop being a build-time idea.

**Text first.** `text/plain` is the honest start; the type tag exists so a later image or a typed
stream is a second kind rather than a second clipboard.

**And the terminal needs selection before it can copy at all** — `libterm`'s grid has none, which
M5 deferred alongside the clipboard itself. That is part of this work, not a prerequisite for it.

### Decision 2 — PNG, decoded in the guest, because a wallpaper is the user's

Settled with the maintainer 2026-09-01, **against the recommendation**, and the reasoning is worth
keeping because it is a judgement about what the system is for rather than about cost.

The question upstream of the format was: **is a wallpaper a shipped asset or a file a person drops
in their home directory?** Shipped assets would have allowed the decode to happen on the host at
build time, and the guest to read something trivial — QOI at ~300 lines, or raw P6 at ~40 and 3 MB
of disk. Both were costed and both were cheaper.

The answer is that a wallpaper a person cannot supply is not really a wallpaper. So the guest
decodes what a person actually has, which is PNG: **inflate** (RFC 1951 — fixed and dynamic
Huffman, a 32 KiB window), **unfiltering**, and a refusal for interlaced. Roughly 400–600 lines,
and the largest single piece of code M12 contains.

**Hand-rolled or a crate is not settled here.** The bar for a userspace dependency is in
`userspace/CLAUDE.md` and the whole transitive tree must clear it; the precedent cuts both ways —
`ab_glyph` was taken deliberately, and `libcrypto` was hand-rolled on the same page. Inflate is
well specified and testable against published vectors, and it is **reusable**: a package format,
compressed logs and anything else later want exactly this. That argues for owning it, but not
loudly enough to settle it before somebody has tried building the alternatives for
`x86_64-unknown-nitrox`.

**Consequences that follow from "the user's file", not from PNG.** The wallpaper is read by
`desktop-shell` — it holds `/home` and a theme, where the compositor holds neither and should not
gain a filesystem to draw with — and shown as a full-screen background window it owns. The theme
file gains a key naming the path, bounded like `font_ui` is and for the same reason. An image that
does not load falls back to the ground colour and says so, which is the stance every other
unreadable value in that file already gets.

**Icons stay filed.** PNG makes an icon set *possible*; an icon set is a naming convention, a size
convention and a lookup path, which is a second decision and not this one.

### Details pass, 2026-09-01

Six parts, and the governing decisions below them, so each can be built rather than re-argued.
The two taken before this pass — the clipboard's owner and the image format — are above.

### Part A — dialogs, and the second window ✅ complete (2026-09-01)

- [x] **`Role::Dialog` gets its first *application*, and the toolkit grows a dimension** ✅. The
      role has existed since M2 Part A and is created today by exactly one thing: `ui-testclient`,
      which makes one deliberately so that the held-configure and ignored-offset halves of its
      contract are asserted — added because a reviewer asked for it (PR #220, finding 2), and
      watched by `check-display`. Its placement change in M11 Part E batch 8 is covered by a host
      test named for that batch.

      So the gap is not coverage, it is *use*: no program a person runs has ever created one, and
      `widget-toolkit.md` §11 named exactly that as the trigger — "Multi-window applications. One
      `App` drives one window. Trigger: dialogs that are real windows rather than `stack`
      overlays". This is that day.

      (The first version of this said the role had "no consumer at all" and was "exercised by
      nothing", which was false twice over and would have sent the building session looking for
      coverage that exists — PR #266 review, blocking 1.)

      **What it became**: `nxedit` asks before discarding an unsaved buffer. `Msg::Close` no
      longer ends the run when the buffer is modified — however many times it arrives, which is
      the shape a second `CloseRequested` needs — and only `Discard` does. The dialog is a real
      window with its own title bar, and its one title-bar button is *keep editing*: a frame must
      not be a third way to discard.

- [x] **`libui::window::Child`, which is where the dimension actually went** ✅. `nxterm` had
      grown a `Popup` struct in M6 Part C3 — an id, a `BufferPool`, a scratch framebuffer, a
      `Tree` and a `Router`, with `open`/`present`/`close` over them — and the confirmation wanted
      the same six fields. Two consumers is when a helper goes down a layer, so `nxterm`'s went
      down and became the menu's window unchanged.

      **It is the first module in `libui` that is not a function of values**, which is why the
      crate gained a `libsurface` dependency the layering had always allowed. It is not
      host-tested, for the reason `libsurface::buffers` is not: every line is a call into a
      `Session` or into `paint`/`layout`/`Tree`/`Router`, and both halves are tested already.
      What is left is the order, which is what a gate sees.

      **A main window is deliberately still each application's own loop** — it owns the
      `sys_wait`, answers `Configure` by reallocating everything a `Child` holds, and `nxterm`'s
      paints a `custom` grid. Trigger: the next part that touches both applications' main loops.

- [x] **The shell places dialogs, and does not list them** ✅. A `dialog` is *held* for the
      manager exactly as a `normal` is, and `desktop-shell` filtered on `ROLE_NORMAL` — so the
      first dialog would have waited out the compositor's 200 ms deadline and appeared in the
      corner. It is centred on its parent and clamped to the work area, which
      `rsproto-surface-ops.md` already said a manager can work out for itself. No taskbar slot:
      an entry that closes or minimises a question on its own is a question hidden behind its own
      window.

- [x] **And the insist became a second click rather than a clock** ✅ — the part of this nobody
      predicted. `nxedit` holding a question is the first client in the tree that deliberately
      does not answer `CloseRequested`, and from the shell's side that is indistinguishable from
      a client that has stopped listening. Against it the two-second grace period destroys the
      window, and the buffer with it, two seconds after one middle-click. A shell cannot tell
      "wedged" from "asking"; the person looking at the dialog can. **M9 Part C named this
      trigger** — "the first application that can be wedged on purpose" — and it fired here.

      **The arming expires**, five seconds after the ask, which the first version did not do:
      it disarmed only when the window went away, so answering *keep editing* left the entry
      armed for the window's life and a later click destroyed it with no question — the same
      lost buffer, unbounded (PR #267 review, blocking 1). A second click counts only while it
      is still part of the first gesture, which is the rule M12's kill ring already settles for
      cycling.

- [x] **Gate** ✅: `check-login` steps 11 and 12. Step 11 drives the confirmation to **both**
      answers from the editor's own close button — a dialog that only ever gets "yes" is half a
      control — and asserts the dialog's origin is its parent's centre, clamped, rather than
      merely reading the number back. Step 12 middle-clicks the taskbar twice: the first ask
      reaches a client that declines it, the second destroys the window. **That is the first time
      `Manage::Close` has been driven end to end**, which M9 Part C left open for want of a client
      that could be wedged.

      Step 12 also drives the **expiry**: the ask is answered with *keep editing*, the arming
      is allowed to run out, and the next click is asserted to **ask again** rather than
      destroy. That assertion is the control for the bug above — under the version review
      caught, it printed "did not answer; closed it" instead. It is the one deliberate sleep in
      a gate that is otherwise expect-driven, because an expiry is the absence of a state and
      has no line to wait for.

      **Ten host tests in `nxedit`, each run alone against a broken implementation**: closing a
      modified buffer asking rather than exiting, a clean one closing at once, a *second* close
      request not discarding (the control for the obvious wrong spelling), keep-editing changing
      nothing but the question, saving removing the reason to ask, `Esc` and only `Esc`
      answering, a dialog that will not open leaving the window alone, the tree measuring to
      exactly its declared size, the two published button centres being where the buttons are,
      and the dialog's own close button keeping the buffer. Plus two in `xtask` for the lines the
      gate parses.

      **The four button coordinates are published from `nxedit` and hardcoded in the gate**,
      because a host tool in another workspace cannot link the crate — the same arrangement that
      already hardcodes a title bar's height. The host test asserts **the literals themselves**,
      not merely that the derived constants agree with the tree: both sides derive from
      `CONFIRM_PAD`, so comparing them pinned nothing, and with `CONFIRM_PAD` at 40 every host
      test passed while the gate went on clicking a `y` the buttons no longer covered (PR #267
      review, finding 2). The claim that a padding change fails beside the change rather than
      after a three-minute boot is true now, and was not.

- [x] **Two bugs found on the way, and neither was in any of this** ✅.

      **The parked-event gap**, which is real by inspection: `desktop-shell` blocks in
      `sys_wait`, and input that `libsurface` parked inside the transport while a *session*
      request awaited its reply is not in a kernel queue and cannot wake it. The `sent_request`
      belt covers manager requests only. The shell pumps before it waits now and does not block
      while anything is queued.

      **And the launcher-row flake, which was blamed on it and was not it.** Removing the close
      timer coincided with `check-login`'s row click failing; a clean worktree passed and the
      branch failed twice, which looked like a bisect over an intermittent step and is not one.
      It failed again in CI after the pump fix. The cause was the thing this shell's own source
      names for its *other* typing path — an unacknowledged burst of injected keys — with the
      receipt deliberately withheld from the launcher "so the launcher's typing stays quiet".
      The filter reports a count per character now, the gate waits for one per key, and
      `nxedit`'s name field got the same, its gate comment having claimed a discipline it did
      not follow.

### Part B — the browser: file operations, and drag-and-drop within a window ✅ complete (2026-09-02)

- [x] **Rename, delete, copy, new folder — and new file, which the maintainer added** ✅, from a
      **File** menu and an **Edit** menu, settled 2026-09-02. `nxfiles` holds `/home` and performs
      them itself: that binding *is* the authority these need, and routing them through the shell
      would be asking a supervisor to do what the application is already entitled to. Opening a
      *program* needs `/bin` and a namespace and stays the shell's; renaming a file in a directory
      the browser can already list needs nothing it does not hold.

      **Three shapes, not five.** The three that need a name — new file, new folder, rename, and
      copy, so four — share one **prompt**: a field that replaces the path in the strip while it
      is open, which is the shape `nxedit`'s save-as established. Delete needs a **question**
      instead, which is Part A's dialog. Nothing needed a fifth mechanism.

      **An operation with nothing selected is answered rather than ignored.** Three of the five
      act on a row; a menu item that silently did nothing without one is a control that looks
      live and is not.

- [x] **Copy goes through `libfs::copy_file`** ✅, which maps source and destination and copies
      between the mappings with no heap at all, bounded by `MAX_COPY` (8 MiB) — a bound whose own
      doc settles the question, since "the pages themselves are demand-paged, so this bounds VA,
      not RAM", and which names a windowed copy as the refinement if a real workload exceeds it.
      It is what `copy_tree` and the `copy` coreutil already use.

      (An earlier draft re-opened streaming-versus-bound as though it were open, and pointed at
      `read_file` — the one function here that *does* allocate the whole file, and the wrong one
      for a copy to call. PR #266 review, finding 4.)

      **And nothing overwrites.** Rename, copy and move all pass `replace: false`: overwriting is
      a second question, and a browser that answered it silently would be one whose most ordinary
      mistake — typing a name that is already there — destroys a file.

- [x] **Drag-and-drop within a window** ✅ — the half M10 Part E did not build. **The compositor
      is not merely uninvolved, it could not be**: `highlight_target` skips the source window
      deliberately, so a drag out of this list can never be delivered back to it. So the browser
      keeps the gesture itself past the slop, tracks the row under the pointer, and hands it to
      the compositor **only when the pointer leaves the window** — which is the last thing the
      client can decide, because from `StartDrag` onwards the compositor owns the grab and the
      client goes blind.

      **A drop is a move**, settled with the maintainer: within one filesystem that is what a drag
      means, and it is one `libfs::rename` rather than a copy and a delete that can half-happen.
      Only a directory row is a target, and never the row the drag came from.

      **The highlight borrows the list's hover face**, so a drop target and a pointer highlight
      cannot come to look different and the widget needed no new state.

- [x] **`libfs::mkdir`, because the browser was its second consumer** ✅ — and its third and
      fourth were already there: `mkdir`'s `make_one` and `copy_tree` each open-coded the same
      three lines. One helper now, with `--parents` staying the coreutil's flag.

- [x] **And the dialog's frame moved down to `libui`** ✅. `nxedit` published five metrics and
      four aim points for `check-login` to type; a second confirmation would have given the gate
      two tables to keep in step, which is the shape that goes wrong silently. `dialog_frame` and
      `DIALOG_*` are shared, and the host test that pins the aim points to a real tree moved with
      them — so it guards both dialogs.

- [x] **Cut and paste are deliberately absent** ✅ — `TODO(file-clipboard)`. They are a *pair*,
      and a pair that holds something between two gestures is a clipboard however it is spelled;
      building a private one in the browser would ship a second clipboard before Part E's real
      one, which decision 1 made a resource server precisely so that what you last copied is not
      readable by everything running. Part E's own words leave the hook: "the type tag exists so
      a later image or a typed stream is a second kind rather than a second clipboard".

- [x] **Gate** ✅: `check-login` step 9b — the File menu is opened *by clicking its bar item*, the
      `rename` row is pressed, a name is typed with a receipt per character, and then the **serial
      session** reads the file back under its new name with `open`. Asking the browser to re-list
      would be asking the accused; the content is step 8's, so a rename that had copied or
      truncated shows up here too.

      **The menu row's height is divided out of the popup rather than derived from the theme**,
      because a menu row is text plus padding and therefore follows `font_px` — which a theme file
      sets, and which M11's decision 2 keeps out of the metrics a gate may assume. The browser
      logs the popup's rectangle; the gate divides.

      **Fourteen host tests in `nxfiles`**, seven of them run alone against a broken
      implementation: no selection check, a path accepted as a name, delete performed without
      asking, keeping deleting anyway, a file row as a drop target, the drag reaching the
      compositor at the slop instead of at the edge, and the list taking keys while a name is
      being typed.

      **One bug the tests found before a person could**: a rejected name — empty, or one with a
      separator — leaves the prompt open so it can be fixed, and the explanation was written into
      the notice slot *the prompt had replaced*. The answer existed and was never drawn. The test
      asserted the message rather than the absence of an operation, which is what caught it.

- [x] **Three the review found, and they were one bug wearing three hats** ✅ (PR #268). *An
      operation's target was recomputed from state that can move between the gesture that chose
      it and the gesture that confirmed it.* A delete answered after walking into another
      directory removed a file **there** with the same name — one nobody was asked about, while
      the dialog's own text still named the one they chose; a rename after clicking another row
      renamed that row instead; and a drop below the last drawn row moved a file into a folder
      that was never on screen, because `row_at` bounded against `entries.len()` rather than
      against the rows `list_view` actually draws.

      Each is now resolved **when the operation is chosen** rather than when it is answered, and
      a new listing drops the prompt and the question outright. The two fixes overlap only
      partly, which the code says: a listing is the only thing that changes the directory, so for
      delete either would do — but the *selection* moves without one, and there the captured
      target is the only thing between a rename and the wrong file. The test for the delete case
      is honest about not pinning the capture; the rename's does.

      **And `perform`'s refusals were three-quarters decoration.** `libfs::create_file` is
      documented idempotent, so *new file* onto an existing name succeeded and said "created"
      while the old file and its contents were still there; `libfs::rename` deliberately does not
      distinguish an occupied destination, so a correctly-refused rename reported a fault. The
      destination is tested first now, which puts "nothing overwrites" in this program rather
      than in what a server happens to return.

- [x] **And one the gate found, in `libui` rather than here** ✅. The menu's rows could be
      clicked and did nothing, about one run in two: a capture is a *tree id* of the deepest node
      under the cursor, a hovered `menu_item` draws three layers where a quiet one draws one, and
      a frame presented between the press and the release therefore gives that node a new id —
      `path_to_id` finds nothing and the click is gone. `Child` samples its hover only between
      gestures now, and `Router::grabbed` is published so a main window's loop can follow the same
      rule; `desktop-shell`'s modal does. **This is very probably the launcher-row flake** that
      cost three CI failures and two wrong attributions — see the decision log, which records it
      as likely rather than proven.

### Part C — the editor: undo, redo and find ✅ complete (2026-09-02)

- [x] **Undo grouping is the decision**, not the stack ✅. Per keystroke is unusable and per save
      is useless; what a person expects is a word or a line. It lives in `TextAreaState`, which
      already owns the buffer, the cursor and the selection.

      **The rules, and each is a group boundary somebody would notice if it were missing**: a run
      of printable characters is one group; a **separator ends it**, so a word and its trailing
      space come back together and the next word is its own step; **`Enter` ends it**, so a line
      and its break are one; a run of **deletions** is a group of a different kind, so typing
      after deleting does not extend the deletion; **any movement ends whatever was open**,
      because the cursor moving means what comes next is a different edit wherever it lands; and
      an edit that would do nothing — `Backspace` at the start of the buffer — opens no group at
      all, because an undo press that visibly does nothing reads as a broken undo.

      **And a save is a boundary the buffer cannot see**, so `end_group` is published and
      `nxedit` calls it. That one was **found by the gate rather than by a host test**: the editor
      opened an empty file, six characters were typed, it was saved, two more were typed — and
      one undo emptied the buffer, because nothing had closed the group across the save. The
      byte-count assertion caught it on the first run.

      **Snapshots, not deltas**, and the trade is stated rather than assumed: a delta stack costs
      the size of the change instead of the size of the file, and costs a separate inverse for
      every kind of edit — each a way to be subtly wrong and none checkable by reading. Bounded
      at `MAX_UNDO`, with `TODO(undo-deltas)` naming the trigger.

- [x] **Find reuses the shape the save-as field established** ✅ (M11 Part E batch 7): a mode in
      which the keys are the field's rather than the buffer's. That was called "the first widget
      that wants a key"; this is the second — and it is the *same* field, `Field::{Naming,
      Finding}`, because a second copy would be two places that can disagree about what `Esc`
      does.

      `Ctrl+F` opens it, `Enter` walks forward through every match and wraps, `Esc` gives the keys
      back. The field **stays open** on a hit, which is what makes the second press the next match
      rather than a re-type. A match is *selected* rather than merely scrolled to. An empty needle
      matches nothing, because it is what the field holds before the first character and a search
      that jumped on the way there would move the buffer under the person.

      **One bug the tests found**: the search started one *byte* past the cursor, which lands
      inside a multi-byte character — `str::get` then yields `None`, the rest of that line was
      silently skipped, and find appeared to go backwards.

- [x] **Gate** ✅: `check-login` step 8b, and the grouping is asserted **by byte count from
      outside**, which is the one way it cannot be argued with. Two characters typed together are
      one group, so one undo leaves seven bytes on disk rather than eight, and a redo brings both
      back to nine. Per-*keystroke* grouping leaves eight; per-*save* grouping leaves seven after
      the redo as well. Then `Ctrl+F`, a needle typed with a receipt per character, and the line
      the match landed on.

- [x] **What the review found, and one regression it led me into** ✅ (PR #269, no blockers).
      Two were controls that could not fire: the `Ctrl+F` re-open guard is unreachable — a field
      takes the keys before the chord match is looked at — and its comment described a
      needle-across-`Esc` behaviour the editor does not have. And `Msg::Save` on an untitled
      buffer was a **silent no-op while the find field was open**, because "already asking" was
      written when naming was the only field there could be: the save button stayed clickable and
      did nothing.

      **Removing two unreachable guards broke something the existing suite caught.**
      `delete_selection` does *two* jobs — it removes a selection, and it clears an anchor the
      cursor has walked back onto — and restructuring `backspace` to call it only when there *is*
      a selection brought PR #258's stale-anchor bug straight back. The test written for that bug
      a milestone ago failed within seconds. It is the best argument in this part for writing the
      test where the bug was rather than where the code is.

      Plus two gate comments that overclaimed: the byte count discriminating per-*keystroke*
      grouping is eight and not nine, and the `Esc` after a search is **not** asserted by anything
      downstream — nothing types into that editor again.

      **Thirteen host tests in `libui` and six in `nxedit`**, thirteen of them run alone against a
      broken implementation: no separator boundary, no movement boundary, deletions sharing a kind
      with typing, a no-op edit opening a group, an edit keeping the way forward, find starting one
      raw byte on, find not wrapping, an unbounded history, undo reporting nothing, the find field
      closing on its first match, `Ctrl+F` typing into the buffer, a failed search reporting
      success, and undo leaving the buffer looking saved.

      **The receipt names its field.** There are two now, and a gate waiting on "name so far"
      while somebody is typing a search would wait for ever; `Field::label` is the one place that
      spelling lives. A search's own receipt is the **line it landed on**, not the needle — what
      somebody is looking for in their own file is theirs, the same rule that keeps the buffer's
      receipt a count.

### Part D — tabs, in both applications ✅ complete (2026-09-02)

- [x] **A tab strip is a widget** ✅, and the toolkit had none. §8's rule is that an application
      needed it, so it exists; two wanting it cleared that bar with room to spare.

      **Fixed width, not shared out**, which is the decision inside the widget. Tabs that divided
      the strip between them move *every* tab whenever another opens — so the one a person is
      reaching for slides away as they reach, and a gate's aim point depends on how many happen
      to be open. The cost is that enough of them run off the end: `TODO(tab-overflow)`.

      A press on a tab's close box does not also select it, which is the toolkit's shadowing rule
      rather than this widget's — the same one that lets a title bar carry buttons.

- [x] **And the split each application had to make** ✅. `nxedit` grew a `Buffer` and `nxfiles` a
      `Pane`: what stays on the `App` is what a *window* has — its size, its focus, the outboxes,
      the strip's field, the question it may be asking — and what moves is what a person expects
      to survive switching. Getting that line wrong is how a second tab inherits the first's undo
      history, and the split makes it impossible rather than careful. The browser's scroll offset
      moved too: a tab that came back at the top would lose your place every time you glanced at
      another folder.

      **The two differ where the data does.** Closing an editor tab over unsaved work asks —
      Part A's dialog, with the tab's key captured when the question is asked, which is Part B's
      lesson one part on. Closing a browser tab asks nothing: a listing is a view of the
      filesystem rather than work. And the last tab closing closes the window in both, which is
      what keeps the "never empty" invariant every accessor rests on.

      **A drop now opens a tab**, which removed a refusal rather than adding one: it used to
      replace the buffer, so it had to be declined while there was unsaved work — a drop that
      visibly did nothing. Dropping a file that is already open switches to its tab, because two
      tabs on one file are two buffers that can disagree about it.

      **Tab keys carry the top bit**, because `Router::hovered_key` reports the nearest keyed
      ancestor across the *whole* window: a tab keyed `2` and a button keyed `2` are one number,
      and hovering the tab would have drawn the button hovered. The base was `1000` — far from
      the chrome's keys and *not disjoint* from the browser's, which keys its list rows by index
      and can hold any number of them. `1 << 63` is what the claim needed to be true (review,
      optional 6).

- [x] **Gate** ✅: `check-login` step 9c — the drop from step 9 now makes a second tab, the first
      is clicked, and the save is asserted **from outside** to have reached that tab's file. That
      is the whole risk of tabs in an editor: a save that went to the buffer opened last rather
      than the one on screen.

      **And the gate found a real interaction on its first run.** Step 8b's search leaves its
      match *selected* — which is what a find is for — and typing replaces a selection, so a
      keystroke two steps later edited the middle of the file instead of appending: seven bytes
      reached disk where ten were expected. Both halves are right and together they are
      hand-made find-and-replace; what was wrong was the gate's assumption. It presses `End`
      first now, and `typing_after_a_find_replaces_what_was_found` pins the behaviour.

- [x] **And the lost click, finished** ✅. Part B's fix was half of one: it froze the hover from
      the press onwards, and the motion that brings a pointer onto a row is usually in the *same*
      batch as the press — so the live hover had already moved while the tree still held the old
      one, and the next frame stranded the capture. It failed about one run in seven. A four-line
      probe in the guest answered it immediately, where two rounds of reasoning about timing had
      not. `Child` answers a gesture with the hover its **tree** was built with; the shell's modal
      holds its resample until after the drain **and refuses to apply it while a grab is held** —
      the review proved that deferring alone only moves the bug one drain later, because the
      motion that opened the gesture has already sampled the new hover. The mechanism has a host
      test whose control loses the click; the gate's improvement is eight consecutive runs, which
      is evidence and not proof, and the log says so.

      **Eleven host tests** across the three crates, including the widget's two published
      metrics asserted against a real tree, tabs holding their own cursor and history, a question
      about one tab surviving a switch to another, and a tab chord not typing into an open name
      prompt.

- [x] **Review fixes** ✅ (PR #270). Two blocking: the shell applying the deferred hover while
      still grabbed, above; and a `Child` test that re-stated the rule in a local closure, so
      reverting the implementation left all 200 green — fixed by extracting `reported_hover` and
      calling it from both, after which the control fails. Then a save that named its buffer with
      a `bool` and wrote one tab's bytes to another tab's path — **late target resolution, third
      occurrence this milestone**; the key spaces made disjoint rather than distant; and the two
      applications made to agree that a tab chord is the *window's* and is checked before an open
      field, while the buffer chords stay the field's. A comment justifying that ordering with a
      reason the reviewer disproved was replaced rather than kept.

### Part E — copy and paste ✅ complete (2026-09-02)

- [x] **A clipboard server** ✅, endpoint bound per session, storing rather than brokering.
      Decision 1 above, and the architecture is [`clipboard.md`](../architecture/clipboard.md).

      `init` spawns it from `/bin` and binds `/dev/clipboard`, non-critical-path beside the tty
      server; the endpoint travels `init` → `service-mgr` → **both** session columns. That last
      part makes it the first endpoint neither display-specific nor console-specific — `/dev/draw`
      goes only to the graphical column, and this goes to both because decision 4's pipeline runs
      in either. `desktop-shell` binds it into every application namespace it constructs, for
      `/dev/desktop`'s reason: the session namespace is the shell's own and nothing else runs in
      it.

      **The binding is the authority**, so granting it is a capability decision each time, and the
      mechanism for narrowing it needs no protocol change: an endpoint attenuated to `RIGHT_SEND`
      is an application that can copy and not read.

- [x] **A kill ring, not a slot** ✅ — decision 3, with its three rules built rather than
      paraphrased. A paste takes index 0 and consults no cursor; a cycle is a *continuation* that
      replaces what was just inserted and is ended by any other action; and where it can still go
      stale — a pipeline pushing mid-cycle — every entry carries the ring's serial and the server
      refuses a cycle that carries an old one.

      **A terminal does not cycle**, which the part settled: a paste there is bytes already sent
      down the pty, and taking them back would mean sending backspaces to something that may not
      be a line editor. Cycling lives where the buffer is the client's own.

- [x] **Reachable as a path** ✅, `/dev/clipboard`, and usable from a pipeline — decision 4, as the
      `clip` coreutil: `clip`, `clip N`, `clip --list`, `… | clip --copy`.

      **And it found that the pipe could not carry text at all.** `"hello" | clip --copy` was
      refused — only a `Table` could be piped into a program — while `StreamFlags::TEXT_FALLBACK`,
      the "Unix floor" the spec defines for exactly this, had been written by `libstream` and read
      by **nothing** for four milestones. Both ends now exist: the shell wraps a `String` (or a
      list of them) as one, and `display` renders one as the text it is rather than printing
      `line` above somebody's clipboard.

- [x] **Selection in the terminal grid** ✅, which `libterm` had never had. The question inside it
      is answered rather than avoided: a **logical line's text is invariant across a rewrap** — the
      exact property `Line::wrapped` was added for in M9 Part D — so a selection is a pair of
      absolute `(line, column)` positions and a reflow maps them by re-dividing the offset by the
      new width. `Reflow::map_position` does it and **the cursor's own remap now goes through the
      same function**, so the two cannot disagree about where a character went.

      Clearing the selection on resize was the alternative, and is what several real terminals do.
      Not taken: the mechanism already existed, because the scrolled-back viewport's anchor had to
      survive a rewrap a milestone earlier.

- [x] **Gate** ✅, in two halves, because the crossing has two halves.

      `check-login` step 9d is the graphical one: the editor copies a **selection** (a find, so no
      keystroke changes the buffer and steps 11 and 12 find it as step 9c left it), a terminal
      launched for the purpose pastes it into `touch ./` + a typed `.clip` suffix, and the
      **serial** session lists `nitrox.clip`. Asserted through the filesystem rather than a log
      line: `nxterm` in a release image does not report its grid, and a paste that delivered the
      wrong bytes makes a differently-named file rather than a matching count.

      `test-interactive` step 19c is the other: a pipeline pushes, `clip` reads it back, and
      **index 1 is still the entry before it** — the property a single slot fails while passing
      everything above it. From a process with no window at all, which is the half a windowed test
      cannot see.

- [x] **Three bugs the part found, two of them latent for a milestone** ✅. A drag in the
      *scrollback* would have highlighted nothing: `Grid::select_from` damages every **screen**
      row, and a view scrolled back into the history is showing none of them — the same
      two-damage-spaces mistake `nxterm::view_moved` was added for in M9, arriving from a new
      direction. Caught by reading the damage path rather than by a gate, and pinned by a test
      whose control reports `got []`. And: A control channel of
      depth 4 carrying five `NOBLOCK` handoffs drops the fifth **silently** — the graphical
      session came up with no `/dev/clipboard` and the send reported success. `libsession` carries
      a comment about the identical failure from M7 Part F, which is what named this one on sight;
      both ring depths now state that the number bounds the send count. And `init`'s
      `close_retained_endpoints` had been closing three of what were four endpoints since the
      compositor's was added, leaking it on a failed `service-mgr` spawn.

### Part F — images and the wallpaper

- [ ] **PNG decoded in the guest** — inflate, unfiltering, no interlacing. Decision 2 above.
      Whether that is hand-rolled or a crate is settled by building both for
      `x86_64-unknown-nitrox`, which `userspace/CLAUDE.md` requires before taking a dependency
      anyway.

- [ ] **The wallpaper is a window `desktop-shell` owns**, full-screen and bottom-most. The shell
      holds `/home` and a theme; the compositor holds neither and should not gain a filesystem in
      order to draw.

- [ ] **Fit if larger, centre if smaller** — decision 6.

- [ ] **Gate**: `check-login` names a staged image in the theme, and the shell reports the size it
      decoded. A picture is pixels a release-image boot has no reference for; the dimensions are
      what can be asserted.

### Governing decisions

Settled with the maintainer 2026-09-01. Decisions 1 and 2 (the clipboard's owner, and PNG) are
recorded above with M12's scope; these are the rest.

**3. The clipboard is a kill ring, and the ring is the server's while the cursor is the
client's.** A single slot loses the thing you copied two copies ago, which is what every editor
with a kill ring exists to avoid. So the server keeps the last N entries, most recent first, and
`Copy` pushes.

The division is the interesting half: **the ring is shared and the position in it is not.** A
"paste the one before that" gesture is a property of the editing somebody is doing right now, not
of the machine — two applications cycling at once would fight over one cursor, and a cursor that
one client advanced would move under another. So the server answers by index and holds no
per-client state.

**A paste always takes the newest entry, and never consults a cursor.** That is the ordinary case
and the one that matters: copy in one application, paste in another. The client asks for index 0
and gets what was last copied, whoever copied it.

**Cycling is a continuation of a paste, not state a client keeps.** It is valid only immediately
after one — it *replaces* what was just inserted — and any other action ends the sequence:
typing, a copy, focus moving away. That is Emacs's rule for `M-y`, and it is the rule rather than
an implementation detail, because it is what makes a stale cursor unreachable: the position exists
only inside one uninterrupted gesture, and anything that could invalidate it has already ended it.

**Where it can still go stale, the server says so.** Decision 4 makes the clipboard reachable from
a pipeline, so something *not* being driven by the person can push while they are mid-cycle. Each
entry therefore comes back with the ring's serial, and a cycle request carries the serial it last
saw; if the ring has moved under it the server says so and the client starts again from the
newest. One `u64`, and it turns a silent wrong paste into a visible restart.

(The under-specified version of this said only that "a client remembers which entry it last pasted
and asks for the next", which reads as persistent state — the maintainer asked what that does to a
copy between two applications, and the answer is nothing, but the question is what produced the
three rules above.)

**4. It is reachable as a path, and usable from a pipeline.** `/dev/clipboard`, bound into the
session namespace the way `/dev/tty` and `/dev/draw` are. There is no generic read/write verb in
this system — a resource server speaks its own ops, as the tty does — so shell access is a small
utility either side of the pipe rather than a new file interface.

That is the maintainer's addition to this pass, and it is the more interesting half of the
decision: a clipboard that only graphical applications can reach would be the first resource in
this system that a pipeline cannot. Making it a path also makes the ring *inspectable* — listing
what is in it is a command rather than a feature somebody has to build a window for.

**5. A capped payload first, and chunking is expected rather than hypothetical.** The IPC payload
is 4008 bytes, so one exchange carries about two screens of terminal text; the cap is a named
promise like `MAX_EVENT_BODY` is. The maintainer's judgement is that this will not be enough for
long — "we can start with 1, but we may need to end up at 2" — so the trigger is written as an
expectation: **the first thing somebody cannot copy.** A shared memory object was the third option
and is not taken: M10 rejected handle transfer for drops because a refused handle has no clean
owner, and the clipboard would inherit that question.

**6. `Ctrl+C`/`Ctrl+V`, and `Ctrl+Shift` in the terminal.** What fingers already know, and the
terminal has to differ because `Ctrl+C` means interrupt there and always will. The kill ring needs
a third binding to cycle; it is a part-level detail, and `Ctrl+Shift+V` cycling on repeat is the
obvious candidate.

**7. The wallpaper fits or centres, and scaling to fill is deferred as a *mode*.** `box_downscale`
already scales down with a box average and explicitly refuses to scale up, so fitting a too-large
picture and centring a smaller one needs no new code. Filling needs an upscaler and a decision
about interpolation. The maintainer wants both eventually — "it'd be nice to have 1 and 2 as
options" — so the theme key is designed with room for a mode beside the path, and only one mode
ships.

## Milestone 13 — the compositor's feel

Named 2026-09-01, when M11's polish pass produced two reports that were not about appearance and
one diagnosis that explained both.

**The work is ordered, and the order is the point.**

- **Part A — the shadow buffer.** Compose into RAM and copy the finished damage rectangle to the
  aperture in one pass. Today `libdraw::compose::compose` fills each damage rectangle with the
  background and *then* blits the surfaces back, directly into the framebuffer being scanned out —
  so every motion of a drag paints the union of the old and new rectangles background-first and
  the scanout catches it. That is the flicker reported on 2026-09-01, and probably also
  "moving a window is slow" from 2026-08-28. **A measurement comes first**: the claim that this is
  also *faster* — the per-pixel work moving off MMIO into cached RAM — is plausible and unproven,
  and a milestone that opens by proving it is worth more than one that assumes it.
- **Part B — alpha.** `libdraw` says in as many words that there is no alpha channel and this is
  not the beginning of one; changing that is a substrate decision, not a batch. **It comes after
  Part A because Part A makes it cheap**: once compositing goes through RAM, blending is a small
  change to a loop that already exists, where doing it first means writing the blend against the
  scanned-out aperture and then rewriting it.
- **Part C — what alpha unlocks**, both of which M11 deferred *onto* it: drop shadows around
  windows and menus, and the translucent overview sidebar. Shadows in particular must not come
  first — a shadow makes every window's damage region larger than the window, which without Part A
  enlarges exactly the flash it sits on top of.

**Its gate is not `check-display`.** That gate compares a settled screen against a render, and
none of this changes a settled screen — it changes what happens *between* settled screens. What
Part A needs is a measurement and, for the flicker specifically, a person looking at it; what
Parts B and C need is `check-display`'s reference moving deliberately, which is the shape M11
already used.

## What this unblocks

Filed shell items waiting on this arm:

| Item | Waiting on |
|---|---|
| `TODO(history-pager)` — list-style reverse-search | A terminal that can address the cursor (**M5 Part A**, not M4 — corrected 2026-08-12; M4 built the toolkit, and cursor addressing is the ANSI parser) |
| Shift-Enter continuation | Key events with modifiers (M3) — **and an encoding through the terminal, which A6 does not give it**: `Shift-Enter` currently sends what `Enter` sends |
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
