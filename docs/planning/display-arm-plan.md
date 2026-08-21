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
**Milestone 6 is in progress**: planned in detail 2026-08-13, and **Part A — geometry in
the stack, plus the initial-configure handshake — landed the same day.** Parts B, C and D are
not started.
**Milestone 5 Parts A and B were complete** (2026-08-12): `libterm`'s parser, grid, render and
encoder, the blend that unblocked antialiasing, and `nxterm` itself — window, chrome, scrollback,
key repeat, and the display gate's third region. **Part C landed 2026-08-13 — `Tty::AttachBackend`,
per-backend routing, and `nxterm` hosting `nxsh` over what is a pty with the pieces renamed.**

**The arm was re-scoped on 2026-08-12**, from a gap found while planning Part C: the plan had
`session-mgr` spawning `nxterm`, which assigns a graphical job to the serial column's supervisor.
Nothing in `docs/` said who authenticates a graphical user or who spawns the desktop shell — the
top of that column was empty. It is specified now in
[`graphical-session.md`](../design/graphical-session.md), and the milestones below it changed
shape: the old Milestone 6 ("windows, ports, desktops") bundled work at three different
dependency depths, so it splits into **M6 — window management** (compositor only),
**M7 — the graphical session** (new: login, `desktop-session-mgr`, `desktop-shell`), and
**M8 — desktops, ports, templates**; the old M7 becomes **M9**.

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
- **Who logs you in:** [`docs/design/graphical-session.md`](../design/graphical-session.md)
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
still specifies input, text, capture and hotkeys with no code; `ui-composition-model.md`
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
| `display-substrate.md` | **M9** | input, text and capture are finished there |

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
      triggered on Milestone 7, where [`graphical-session.md`](../design/graphical-session.md)
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
It does now ([`graphical-session.md`](../design/graphical-session.md)), it lands in Milestone 7,
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

## Milestone 6 — window management

**Planned in detail 2026-08-13. Part A landed 2026-08-13; B, C and D are not started.**

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
([`graphical-session.md`](../design/graphical-session.md) §3, §5a). Until it exists, no binding can
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
  for window moves anyway") and needs a decoration to grab. Both move together, to M7 — and the
  absence of the gesture is why B2 has no relative `Move`.
- **Terminal reflow**, per question 3.
- **Desktops, thumbnails, global hotkeys** — M8, M8, M7.

### The risk worth naming

**A seam designed with no consumer is a seam designed wrong.** The mitigation is B1's test client
and the fact that M7 follows immediately: the shell is written against this within one milestone,
so the feedback arrives while the design is still fresh. The alternative — a throwaway shell in
M6 — was considered and rejected (maintainer's call, 2026-08-12): a shell built to be discarded is
a third thing to maintain and its feedback is worth less than it costs.

## Milestone 7 — the graphical session

Sketched. **New in the 2026-08-12 re-scope**, and the piece whose absence caused the M5 Part C
misassignment: nothing in `docs/` said who authenticates a graphical user or who spawns the
desktop shell. [`graphical-session.md`](../design/graphical-session.md) now specifies it.

- [ ] **Graduate `graphical-session.md` and `desktop-shell.md`** to `docs/architecture/` — this
      milestone builds both.

- [ ] **The shared session core.** "Authenticate → construct the namespace → spawn the leader →
      reap → tear down" is the same logic in both columns, against different arguments. It
      factors into a crate both supervisors link — Linux's PAM precedent: a shared library, not a
      merged process.

      **Constraint attached:** the core must honour `session-mgr`'s dependency rule (`libkern` +
      `librsproto` + `libstream` + `libheap`, no `libos`), because `session-mgr` links it. The
      greeter — the part that draws — stays in each supervisor, which is where they diverge
      anyway.

- [ ] **`desktop-session-mgr`.** `session-mgr`'s graphical twin: spawned by `service-mgr` with
      `BIND_NAMESPACE` re-delegated, plus the fs/profile/tty endpoints, an auth channel, and
      — the new part — a `/dev/draw` connection, because its greeter is itself a compositor
      client. Presents a login **window**, calls the *same* `auth-service` over the *same*
      protocol, constructs a session namespace, spawns `desktop-shell` into it.

      That `auth-service` needs no change is the evidence the existing split was drawn in the
      right place. `/dev/console` is deliberately **not** bound into a graphical session —
      governing decision 3's failure is on the record.

- [ ] **`desktop-shell`, minimally**: the top bar, the applications modal, window placement
      policy driving M6's ops, and — the load-bearing part — **constructing a namespace per
      application it spawns**. `ui-composition-model.md` §5a requires this; §5's guarantee that
      "an application cannot compose other applications" rests on the shell being the process
      that built them.

- [ ] **`nxterm` becomes launchable**, which is what makes this milestone visible: the
      applications modal spawns it into a namespace the shell constructed, and M5 Part C's
      `TODO(gui-dev-tty)` is discharged with a real `/dev/tty` binding
      ([`graphical-session.md`](../design/graphical-session.md) §6.1).

- [ ] **Decide concurrency** (`graphical-session.md` §6.2). Two supervisors able to authenticate
      independently fires `session-and-auth.md`'s deferred "one console, one session at a time".
      Serial must stay available while a graphical session runs — it is the recovery path — but
      whether that is two sessions or one session with two views is undecided.

## Milestone 8 — desktops and the overview

Sketched. The remainder of the old Milestone 6, now resting on a shell that exists.

**Rescoped 2026-08-21**, when durable window-to-window wiring was cut
([`ui-composition-model.md`](../design/ui-composition-model.md) revision 3). Ports-as-wiring, the
default-handler fallback and templates went with it; desktops never depended on any of them.

- [ ] **Graduate `ui-composition-model.md`** to `docs/architecture/` — this milestone builds the
      desktops it specifies. Ports are the part that will still be unbuilt, so the graduation has
      to say so rather than move the whole document.

Multiple desktops and **the overview** — thumbnail capture, the frozen image grid, the desktop
sidebar (desktop shell §6). Desktop membership as a filtered view of the compositor's window set;
moving windows between desktops.

**No longer here:** ports with `list` discovery as a *wiring* surface, wiring by `sys_ns_bind`
into an application's namespace, the default-handler fallback, and templates
(`instantiate`/`extract`/`save`, `open ./code.nxg | desktop`). Ports themselves survive as paths
and are unscheduled — see `TODO(port-shape-rework)`.

## Milestone 9 — applications, and drag-and-drop between them

Sketched. A file browser and a text editor, as ordinary applications rather than as parts to be
wired together, and the one composition mechanism that survived: **structural drag-and-drop** —
`QueryCaps` against visible windows on a drag, valid targets highlighting, one message on drop.

The applications come first in the ordering because the mechanism needs two honest consumers, and
because they are the two the cut argues for building properly: revision 3's conclusion was that a
workflow wanting tools tightly integrated is asking for an *application*.

**No longer here:** the patch canvas, Tier 2 durable wiring, and "what happens to a wired graph
when an application crashes" — that question was among the better arguments *for* the graph and
went with it.

- [ ] **Graduate `display-substrate.md`** to `docs/architecture/` — by the end of this milestone
      the substrate is fully built.

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
