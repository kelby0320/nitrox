# Nitrox Implementation Plan

Working document tracking implementation progress. Updated as work proceeds — this is meant to be edited freely, not preserved as a snapshot.

This file is the **index**. Each phase's detailed checklist lives in its own file (see the
phase table below); this page holds the current status, the phase map, and the cross-cutting
workstreams that span all phases.

## How to use this document

- Each phase has a goal, a checklist of work items, and a milestone definition ("how do I know this phase is done?").
- Check items off (`- [x]`) as they're completed.
- Items can be reordered within a phase if dependencies allow. The order shown is a suggested execution order, not a strict requirement.
- Add sub-items under any task if it grows complex enough to need breakdown.
- When deviating from the plan, note it inline (`Note: ...`) rather than rewriting silently — the reasons matter later.
- Phases overlap in practice. "Phase 1" being the focus doesn't mean nothing from Phase 2 can be touched; it means Phase 1's milestone is the next target.
- **The phase detail is split across files.** Edit the phase file, not this index, for checklist changes; update the Current status here when a phase's headline changes.

## Cross-references

Throughout the phase documents, links to `docs/architecture/`, `docs/spec/`, and `docs/rationale/` point to specific documents that contain the design and rationale. The architecture overview at `docs/architecture/overview.md` is the recommended entry point if context is needed.

## Phases

| Phase | Detail | Status |
|---|---|---|
| 0 — Foundation | [phase-0-foundation.md](phase-0-foundation.md) | ✅ complete |
| 1 — Kernel substrate | [phase-1-kernel-substrate.md](phase-1-kernel-substrate.md) | ✅ complete |
| 2 — Filesystem and namespace | [phase-2-filesystem-namespace.md](phase-2-filesystem-namespace.md) | ✅ complete (2026-06-26) |
| 3 — Service ecosystem | [phase-3-service-ecosystem.md](phase-3-service-ecosystem.md) | ✅ complete (2026-07-21) |
| 4 — A usable windowed desktop | [phase-4-desktop.md](phase-4-desktop.md) | 🚧 active |

**Subproject plans** (detailed breakdowns that hang off a phase):

- [shell-coreutils-plan.md](shell-coreutils-plan.md) — the typed shell + coreutils (a Phase 4
  subproject; assumes the Phase 4 CLI substrate prereqs are built first).

## Current status

- **Phase 0 (Foundation):** ✅ complete — kernel boots under QEMU+OVMF and renders a framebuffer
  boot screen. See the Phase 0 deviation notes.
- **Phase 1 (Kernel substrate):** ✅ complete — memory foundation, kernel diagnostics, paging +
  `AddressSpace`, the ELF loader, user-memory-access discipline, the handle table, the
  kernel-object substrate, threading + the context switch, the syscall fast path, the first
  userspace process, and the full syscall surface (handles, memory objects, clocks/timers,
  `sys_wait`, notifications, IPC with handle transfer, spawn + lifecycle, `sys_thread_create` +
  supervised exception suspend/resume).
- **Phase 2 (Filesystem and namespace):** ✅ complete (2026-06-26) — boots Limine → kernel/PCI →
  init → ext4 mount (userspace fs-server) → demand-paged reads → a live `eshell>`. Slice 10 (FAT,
  read-only) deferred to Phase 4.
- **Phase 3 (Service ecosystem):** ✅ complete (2026-07-21) — the kernel-first and userspace-runtime
  bands, the service-ecosystem machinery (service-mgr + supervision, RS startup protocol,
  path-based ELF spawn, RW ext4, the auth/session login chain, logging + profile servers), and the
  Definition-of-Done clauses (libstream + typed-log demo; the `/proc` scheduler-stats surface). The
  remaining backlog services are consumer-driven and defer to Phase 4. `std` is a serious
  compatibility target (2026-07-20 std stance).
- **Phase 4 (a usable windowed desktop → browser, networking, sysadmin):** 🚧 active.
  - **Substrate hardening** (the concurrency-review gate into Phase 4) — ✅ done (Parts A–F, F1–F12
    fixed; decision log 2026-07-21).
  - **Floating-point + SIMD enablement** — ✅ done (Parts A–D; per-thread XSAVE, hard-float userspace
    target `x86_64-unknown-nitrox`, proven in ring 3; decision log 2026-07-21, PR #110).
  - **CLI substrate prereqs** — directory ops, `Value` collection types, and the stdio/pipe
    convention (plus the `parent`/`child` retirement into a conforming test harness) — ✅ done
    (2026-07-23/24). See [phase-4-desktop.md](phase-4-desktop.md) → "CLI substrate prereqs".
  - **The typed shell + coreutils subproject** — 🚧 active from 2026-07-24. **Milestones 1,
    2, 3 and 3.5 complete**: the coreutils, the `nxsh` interpreter, and the shell as the
    login leaf (2026-07-31). **Milestone 4 — language completeness — planned 2026-08-04**
    from an audit of the built language against the design doc (now v1.2); it carries one
    kernel item, `sys_process_terminate`. Tracked in
    [shell-coreutils-plan.md](shell-coreutils-plan.md).
  - **Substrate gaps it surfaced** — exit-time handle reclamation, the wall clock, and file
    truncate — all ✅ (2026-07-24). See [phase-4-desktop.md](phase-4-desktop.md) → "Substrate
    gaps surfaced by the coreutils subproject".
  - **The display arm** — 🚧 in progress (**M1–M4 complete**: pixels + the gate 2026-08-05,
    a client with a surface 2026-08-06, input end to end 2026-08-10, the widget toolkit
    2026-08-11. **M5 — the GUI terminal — ✅ complete (2026-08-13)**: Part A (terminal semantics —
    `libterm`'s parser, grid, render and encoder, plus the blend that unblocked antialiasing)
    and Part B (`nxterm` itself: window, chrome, scrollback, key repeat, and the display gate's
    third region) both landed 2026-08-12, and Part C — the tty server's second backend, with
    `nxterm` hosting a real `nxsh` — on 2026-08-13, closing M5. **M6 — window management — ✅
    complete (2026-08-20)**: Part A (placement, restacking, move damage and the initial-configure
    handshake), Part B (the manager seam and four of its five events), Part C (popups positioned
    by their creator and clipped to the screen) and Part D (the spec). **M7 — the graphical
    session — ✅ complete (2026-08-25)**: Part A (text field and list view, window titles), Part B
    (`libsession`, the session core both columns share), Part C (`auth-service` at `/svc/auth`),
    Part D (`desktop-session-mgr`, the greeter, and `check-login`), Part E (`desktop-shell` — top
    bar, applications modal, per-application namespaces, window management) and Part F (`nxterm`
    launchable, with an environment). Its prerequisite outside the display arm,
    [test-path-retrofit.md](test-path-retrofit.md), landed first — which is why the whole
    milestone is proved on a **release** image by `cargo xtask check-login` rather than by a
    harness build.)
    **Re-scoped 2026-08-12** after a gap surfaced while planning Part C: nothing said who logs a
    *graphical* user in or spawns the desktop shell. That column is specified now in
    [docs/architecture/graphical-session.md](../architecture/graphical-session.md), and the milestones above
    M5 changed shape — M6 is window management, **M7 is the graphical session** (new), M8 is
    desktops and the overview — **detail-passed 2026-08-26**, six parts plus minimize, with the
    desktop lifecycle, sticky windows and windows-on-no-desktop settled. **M9 (new, 2026-08-26)
    is window decorations and interaction** — drag-to-move, maximize, snap to edge and corner —
    inserted ahead of applications because an interactive drag needs somebody to own a grab
    region, so decorations are snap's prerequisite rather than polish after it.
    **Detail-passed 2026-08-27**, six parts, with five governing decisions settled: decorations
    are **client-side** in `libui` (server-side would make "the window's rectangle" stop meaning
    "the client's committed buffer" everywhere geometry is computed), the compositor owns the
    drag and the shell owns what a drag *means*, resize **commits on release** rather than per
    motion, close asks the client and the shell can insist through a new `Manage::Close`, and
    `nxterm`'s `Configure` brings **real scrollback reflow**. **M10 is applications and
    drag-and-drop between them** — detail-passed 2026-08-30, six parts, with five governing
    decisions settled: a drop carries a **path** rather than a handle (a refused handle transfer
    has no clean owner, and a consumer that cannot open a path reports its own error), the
    acceptors a window declares are **ports in waiting** so a future port is a second carrier
    rather than a second feature, drop *regions* are the client's and cost nothing in the
    protocol, the match is **declared rather than queried** (superseding the composition model's
    live `QueryCaps`), and both applications ship **thin**, with M12 named to hold the rest. M11
    is themes and visual polish; **M12 is applications, deepened** — tabs, undo, find, file
    operations.
    **Built since: M8 — ✅ complete (2026-08-26)**, desktops, the overview and the bottom bar;
    **M9 — ✅ complete (2026-08-28)**, client-side decorations, interactive move, close,
    `Configure` with real reflow, edge resize and snap. **M10 — ✅ complete (2026-09-01)**, in five parts:
    `libfs` (A), the file browser `nxfiles` (B), the text area the toolkit had deferred four
    times (C), the editor `nxedit` with `Desktop::Open` (D) — a client naming a *path* for the
    shell to open, because an application holds no authority to spawn — and **drag-and-drop
    between the two (E)**: `DeclareAcceptor`, `StartDrag`, `Dropped`, and a compositor that
    highlights what would take a payload.
    **There is no Part F**: the details pass counted six, and the sixth — graduating
    `display-substrate.md` — was retired as "not a part" and paid off under M9's account on
    2026-08-30, where the debt belonged.
    **M11 — ✅ complete (2026-09-01), in five parts**: `xtask preview`, which renders the
    toolkit's reference on the host so a judgement about how something looks costs a second
    rather than a boot (A); one `Theme` in `libdraw`, where the compositor and the toolkit can
    both reach it (B); that theme read from `/home/theme.toml` and handed to each application on
    the setup record it already gets (C); **the UI font stopping being a terminal font** (D) — two
    font roles, a proportional face for every label and the fixed-advance one kept for the grid;
    and **the polish passes** (E) — ten batches over three rounds of maintainer feedback, from a
    light palette measured off a reference desktop to desktop previews in the overview. **Part F,
    the control panel, was not built** — the list finished and it had not started, and a settings
    application arriving instead of a finished polish list was named as the wrong trade before
    either existed. It is **trigger-gated** rather than rescheduled; see below.

    **What the polish passes actually found**: most of rounds two and three were *defects*, not
    taste — an applications menu whose rows could not be clicked, a scrollbar built with no
    pointer handler, an editor that exited when launched from the menu, a window that had no way
    to ask where it should appear. Polish is the first time a system is used the way a person
    would use it, and that is what surfaces those.

    **M12 and M13 were scoped 2026-09-01**, when M11's list emptied into things that were not
    polish. **M12 — applications, deepened**: application depth (tabs, undo/redo, find, file
    operations and the confirmation dialogs that make it the first *application* to create a
    `Role::Dialog` — **Part A landed 2026-09-01**: `nxedit` asks before discarding, `libui`
    grew `window::Child` for the windows an application opens beside its main one, the shell
    places dialogs, and the taskbar's insist became a second click rather than a two-second
    timer, because a client asking its user a question is indistinguishable from a wedged one;
    **Part B landed 2026-09-02**: `nxfiles` grew File and Edit menus over five operations, a
    prompt shared by the four that need a name, a delete that asks first, and drag-and-drop
    *within* the window — which the compositor could not have carried, since it skips the source
    window when it looks for a drop target. Cut and paste are filed for Part E rather than built,
    because a pair that holds something between two gestures is a clipboard; **Part C landed
    2026-09-02**: undo and redo grouped by word, line, deletion run, movement and save — the
    grouping being the decision rather than the stack — plus find, sharing the very field the
    save-as prompt uses; **Part D landed 2026-09-02**: a `tab_strip` widget both applications
    wanted at once, and the line each had to draw between what a window holds and what a buffer
    or a pane does),
    **copy and paste** — a clipboard *resource server* whose endpoint is a namespace binding,
    because "anything running may read what you last copied" is ambient authority; **Part E
    landed 2026-09-02**: `clipboard-server` holds a kill ring, `/dev/clipboard` is bound into
    both session columns and into every application namespace the shell builds, `libterm` grew
    the selection M5 deferred — a pair of absolute positions that follows a rewrap through the
    same map the cursor does — and `clip` puts the ring either side of a pipe, which is what
    caught `StreamFlags::TEXT_FALLBACK` having been written for four milestones and read by
    nothing — and
    **images**, decoding PNG in the guest so that a wallpaper is a file a person supplies rather
    than one the build ships; **Part F landed 2026-09-02**: `libdraw::png` decodes every colour
    type at depth 8 and refuses the rest by name, inflate is `miniz_oxide` — taken by the
    procedure the plan named, building it for the custom target before agreeing to it — and the
    wallpaper is a `Role::Panel` with a zero reservation, which is bottom-most, unfocusable and
    claims no work area without any protocol change. **M12 is complete.** **M13 — the compositor's feel**: the shadow buffer first (with a
    measurement in front of it), then alpha, then the drop shadows and translucency that both
    wait on it. **Part A is done** (2026-09-03): the measurement it opened with disproved its own
    rationale twice — the aperture is not uncached, and the background fill was not where a frame's
    time went — and the second wrong answer led to the one that was, a row-wise `memcpy` in
    `blit_clipped` where the two formats match. Composing off-screen now *also* runs 3.7× faster
    than painting the aperture did, by a mechanism the plan never named. **Part B is done** too
    (2026-09-03): `libdraw` has an alpha channel, opt-in as `ARGB8888`, after the two cheaper
    answers were tried — shadows need no substrate, and per-surface opacity dims a panel's text
    along with its ground. Opaque surfaces keep Part A's row copy. **Part C closes the milestone**
    the same day: windows and menus cast shadows, and the overview is a translucent surface over
    the live desktop rather than an opaque one redrawing it. **M13 is complete.** The order is load-bearing — the shadow buffer makes alpha cheap, and shadows
    without it would enlarge the flicker they sit on.
    **M14 — the applications, and what a menu is** — detail-passed 2026-09-03 from the
    maintainer's list after living with the desktop, with the north star stated as a comparison:
    the three applications should be about equivalent to the versions of GNOME Terminal, Text
    Editor and Files that ship, which is a bar anyone can check against a machine they own. Eight
    parts, built F, H, A, B, C, D, E, G — H split out of F when costing showed desktop entries
    cross the namespace, and F is what is left, which is genuinely small. Five governing decisions — an
    application is a thing that *says* it is one (desktop entries, because "is this graphical?"
    cannot be read off a binary), an accelerator is declared once so its label and its binding
    cannot drift, the file chooser is a widget over a listing rather than a browser inside a
    toolkit that may not make syscalls, Quit means every window, and a single click selects while
    a double click opens. **Part F is done** (2026-09-03): the three labels capitalised — the capital
    in `desktop_label`'s *fallback*, because a desktop can be renamed and title-casing a name a
    person chose would be the shell editing their text — and the cursor's tail redrawn against
    MATE's pointer, measured off a screenshot rather than argued about, after three attempts that
    changed the wrong thing. **Part H is done** (2026-09-04): the applications modal lists desktop entries — a
    package declaring which of its programs are applications and what to call them — projected at
    `/applications` by the profile server the way `bin/` is projected at `/bin`, because "is this
    graphical?" cannot be read off a binary. **Part A is done** (2026-09-04): a drop-down menu is
    a value in `libui` — items with chords, separators, per-item availability and the arrow keys —
    and all three applications took it, the editor gaining a menu bar it had never had, above its
    tab strip. Decision 2 became true rather than aspirational in the same change: `accel_match`
    routes a chord through the table the popup draws, so a label and its binding are one
    statement. The smaller half is that rows which used to be refused *after* being chosen are
    greyed before. **Part B is done** (2026-09-04): `nxterm` grows tabs, each with its own
    grid, scrollback and shell, opened with `Ctrl+Shift+T`; then all three applications open second
    top-level windows, which the protocol has allowed since M2 and nothing had done — the "shape of
    every `main`" conversion the toolkit doc has had a trigger for since M12 Part A, done uniformly
    because a first window kept as the loop's own would take the rest with it when closed. Quit
    means every window, asking each exactly as its own close button asks. The same change turned up
    a real drag bug that had been dismissed as a gate flake twice. **Part C is done** (2026-09-04):
    a file chooser in `libui`, one tree for both Open File and Save As, rendering over a listing the
    application read — decision 3 held without an exception, so `libfs` grew the sort that keeps two
    directory views from disagreeing about what "newest" means. Save As changes what the buffer
    *is* rather than prompting for a destination, which is what makes the tab's label, the unsaved
    marker and the next `Ctrl+S` follow it. **Part G — syntax highlighting — was wanted as a stretch and promoted the
    same day**: costing it found that reusing `nxsh`'s lexer does not work (it is fallible, and
    parser-mode-driven, while a highlighter must be total over text that is not a program yet), and
    the table-driven scanner that replaces it makes each further language a table rather than code
    — so it covers nxsh, TOML, Markdown and Rust instead of one. **The control panel is trigger-gated**
    rather than scheduled: when settings outgrow a hand-edited file. (Desktops and the applications milestone — M8, and what is now M10 — were
    rescoped 2026-08-21 when durable window-to-window wiring was cut; the milestone numbered 9 on
    that date is today's M10. See the decision log.)
    Design in
    [docs/architecture/display-substrate.md](../architecture/display-substrate.md) +
    [docs/architecture/ui-composition-model.md](../architecture/ui-composition-model.md);
    build order in [display-arm-plan.md](display-arm-plan.md). Milestone 1 is the test
    gate: the compositor composites a known scene and host and guest agree on the hash.
  - **Now: pre-CLI substrate hardening** (the deferral audit, 2026-07-24) — four slices
    landing before coreutils Milestone 2: trustworthy deferral docs + CI running the QEMU
    gate; the demand-fault path (fill cookie, read-ahead, blocking second faulter, shared
    file-backed text); fs/ext4 completeness for M2; and the now-triggered hygiene items.
    See [phase-4-desktop.md](phase-4-desktop.md) → "Pre-CLI substrate hardening".

---

## Cross-cutting workstreams

Things that need ongoing attention across all phases, not phase-specific:

### Testing

- [x] Host-side unit tests for everything that doesn't require the kernel runtime (allocators, parsers, data structures, ABI encoding) — 784 and growing, via `cargo xtask test`
- [x] QEMU integration tests via `isa-debug-exit` for everything that does — `cargo xtask test-qemu` adjudicates the whole boot (`docs/conventions/qemu-integration-tests.md`)
- [x] CI runs both on every push (2026-07-24, PR #120) — a second job builds the image and runs `test-qemu` under `--kvm`, since the kernel is x2APIC-only and the runner's QEMU predates TCG x2APIC support
- [x] Add a test for any non-trivial bug fix — established practice; each fix lands with the test that fails without it, and the boot-loop campaigns cover what unit tests structurally cannot

### Documentation

- [ ] Architecture deep-dive docs in `docs/architecture/` written alongside the corresponding implementation
- [ ] Reference catalogues (`docs/reference/`) — kernel objects, syscalls, error codes, syscaps, rights — grown as the kernel grows
- [ ] Convention docs (`docs/conventions/`) — code style, unsafe policy, testing — written from observed patterns

### Decision log

- [ ] `docs/decision-log.md` updated whenever a significant decision is made during implementation — what was decided, why, what alternatives were considered

### Conventions enforcement

- [ ] `unsafe` blocks have SAFETY comments (clippy lint where possible)
- [ ] No external crate dependencies introduced into the kernel
- [ ] Lock ordering documented in `kernel/docs/lock-ordering.md` updated as new locks are added

---

## Where this document lives

Recommended location: `docs/planning/implementation-plan.md` or `IMPLEMENTATION.md` at the repo root. <!-- check-docs: allow-missing --> The repo root has the advantage of being easy to find; `docs/planning/` keeps the docs tree tidy. Either is fine — pick one and stick with it.
