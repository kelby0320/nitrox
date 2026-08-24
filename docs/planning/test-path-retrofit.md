# Nitrox Test-Path Retrofit — Subproject Plan

**Status:** ✅ complete (2026-08-24), bar one open box named in Part C. Planned 2026-08-21;
Parts A, B and C1 landed that day, C2 and D on 2026-08-24. What it did:
`parse_all` and the schema change under it, `boot-probe` started from a declaration,
`service-mgr` holding more than one service, `session-mgr` down to **zero** test cfgs with the
verdict and the substrate gates moved out of it, and the five filesystem tests out of PID 1 —
which turned up a kernel lock-order violation in `AddressSpace::drop`. Sits between Milestone 6 and Milestone 7 of
the [display arm](display-arm-plan.md), and is a **prerequisite for M7** — not because M7
needs it to function, but because M7 adds three processes to the set that would otherwise
grow test paths of their own.

## What this is

Every service that is *under test* should be the service that *ships*. Today three of them
are not, and the gate that adjudicates the whole boot runs a build in which the code it
claims to prove has been compiled out.

This plan removes that, and gives the in-guest probes a home that is not a supervisor.

**Maintainer's call, 2026-08-21**: *"I want to move away from having custom test paths in
services like init or session-mgr. Ideally all software under test should work basically the
way it does on release builds."*

## The measurement

Only three crates carry a `#[cfg(feature = "test-harness")]` or `#[cfg(feature = "selftest")]`
at all. The compositor, `tty-server`, `input-server`, `fs-server-ext4`, `auth-service`,
`profile-server`, `nxsh` and every library are clean, which is what makes this contained
rather than systemic.

**Method**, stated because an earlier draft of this table mixed two and the comparison it
supported inverted when they were reconciled (PR #225 review, finding 2). For every
`#[cfg(… feature = "test-harness" | "selftest" …)]`, take the set of line numbers it governs —
the attribute through the end of the item, block or statement it applies to, brace-matched past
comments and literals. Line numbers go into a **set**, so a cfg nested inside an
already-counted item cannot be counted twice. Top-level items and inline blocks are both
counted, everywhere.

| Crate | cfg sites | Test apparatus | Shipping path, omitted under test | Lines that differ | Of the file |
|---|---|---|---|---|---|
| ~~`init`~~ | ~~41~~ **1** | ~~657~~ | ~~**15**~~ **0** | ~~672~~ ~30 | **~2 %** ✅ Part C |
| ~~`session-mgr`~~ | ~~31~~ **0** | ~~241~~ | ~~**147**~~ | ~~388~~ | **0 %** ✅ Part B |
| `nxterm` | 9 | 74 | 0 | 74 | 10 % |

**The percentages are close, and the comparison is not the point** — an earlier draft leaned on
a "32 % vs 28 %" contrast that a consistent method removes. What matters is the third column.

`init` is PID 1, which [`init/CLAUDE.md`](../../userspace/init/CLAUDE.md) says must never panic;
about a third of it is filesystem tests — `overwrite_test`, `grow_test`, `create_test`,
`subtree_bind_test`, `read_large_file` — plus the four `run_*` spawns of graphical clients.

**`session-mgr`'s 147 are not test apparatus at all**: they are the *shipping* login — the
interactive `login()`, `tty_open`, `tty_request`, `tty_write`, `tty_set_echo`, `tty_read_line`,
`tty_close`, and the branches that call them — compiled **out** under `test-harness`. Code CI
builds in one configuration and never executes in the other.

**`init`'s 15 are worse than their size**, and were missed twice — by the draft that said the
column was zero and by the review that agreed. They are `#[cfg(not(feature = "selftest"))]` on
two blocks: the normal boot's `spawn_service_mgr` + `reap_loop(notif, root_ns, service_mgr_h)`,
and the restart of `service-mgr` when it dies. Under `selftest`, init closes service-mgr's handle
immediately and calls `reap_loop(…, 0)` — the demo `parent` is the primary child, service-mgr is
**not supervised at all**, and the critical-fault recovery path is a different branch. So PID 1's
supervision loop is one of the things `test-qemu` structurally cannot exercise.

## The diagnosis, which is not "cfgs accumulated"

The probes are in `session-mgr` **because the verdict is in `session-mgr`**, and the verdict is
there because it is the last thing to run. `sched_gate`'s own doc comment says so — it calls
itself "the Phase 3 **clause 3** verdict gate, checked synchronously at the single PASS point",
and justifies the placement by "because this runs *before* the only `SYS_TEST_EXIT(PASS)` call, a
failure cannot lose a race to the verdict".

That is correct reasoning from a bad premise. It has already produced one visible duplicate:
`session-mgr::sched_gate` and `test-harness::sched_stats_demo` read the same
`/proc/sched/stats` for the same purpose, in two processes, because one of them is where the
verdict happens to live.

**Move the verdict to a program whose job is adjudication, and the probes follow it out.**

## The pattern already exists, and is proven

`cargo xtask test-interactive` boots `BuildMode::Normal` — the **release image** — and drives a
real login over the serial console: wrong password refused, right password accepted, a shell
prompt, a program from `/bin`, `Ctrl-C`, `exit N`. Expect-driven rather than sleep-driven, and
it runs in CI. Its doc comment made this plan's argument, in the words it carried **before Part
B** (it is past tense there now, and the counts have grown — 78 expectations across 25 steps):

> `test-qemu` runs the `test-harness` build, where session-mgr auto-logs-in and runs a fixed
> script; the `login:` prompt, a typed password, a real shell prompt and `exit` are all
> `#[cfg(not(feature = "test-harness"))]` code that CI compiled and never executed. **Every
> interactive bug this project has had lived exactly there.**

So the conclusion was reached once and the answer was built — for the serial column, as one
gate. What did not happen is the second half: **the substitutions it made redundant were never
deleted**, and the pattern never reached the display arm. `check-display`, `check-terminal` and
`check-input` all boot the `test-harness` image.

## Three classes, three answers

The 81 sites are not one problem. Sorting them is most of the work of deciding what to do:

| Class | What it does | Example | Answer |
|---|---|---|---|
| **Substitution** | Replaces the shipping path | `login()` is a *different function*; `tty_open`/`tty_write`/`tty_set_echo`/`tty_read_line`/`tty_close` are all `#[cfg(not(test-harness))]` | **Delete.** `test-interactive` already covers it |
| **In-guest probe** | Checks something with no user-facing surface | `sched_gate`, `fp_gate`, init's five filesystem tests | **Move to a program** |
| **Observation** | Prints what happened | `nxterm`'s cursor-row and focus lines | **Keep** |

Observation is the cheap class and is deliberately left alone. It is not free — console writes
change timing, and a torn log line has already been a real bug here (session-mgr's
"session ended" message, assembled from four `kprint`s, came back interleaved with the tty
server's) — but a `kprint` behind a cfg does not make the shipping path a different path.

## Governing decisions

1. **The shipping path is the tested path.** A service may print more under test. It may not
   *behave* differently, and it may not have a function that exists only in one build.
2. **Adjudication is a program, not a phase of a supervisor.** Whatever runs last writes the
   verdict; nothing else needs to know a verdict exists.
3. **The test image differs from the release image by data, not code.** `service-mgr` parses
   `[service.<name>]` declarations from `/initramfs/etc/services.toml`; the test image ships
   that file with **one more table** in it. `init` and `service-mgr` compile identically in
   both, and their ELFs are byte-identical.
4. **`SYS_TEST_EXIT` stays.** The kernel having a test-only syscall is not what this plan is
   about — a kernel facility is not a service code path — and `isa-debug-exit` distinguishes
   "the guest decided it failed" from "a log line happened to match". The expect-only
   alternative is named in Part D as an option, not adopted here.
5. **Nothing green goes red.** Every gate that passes today passes at the end of every part.
   The coverage the harness script proves moves into `test-interactive` **before** the script
   is deleted, not after.

## Part A — the probe program, and a service declaration that can carry it

**The home already exists.** `userspace/test-harness/` ships five bins (`test-harness`,
`test-stage`, `display-selftest`, `ui-testclient`, `input-testclient`) and is embedded only in
selftest images. The probes move there; they do not need a new crate.

- [x] ~~**`service_toml` learns `syscaps` and `[handles]`.**~~ ✅ **Dropped 2026-08-21 — not
      needed, and the reason it was listed was wrong.** This box said "a probe that reads
      `/proc/sched/stats` and writes the verdict needs stated authority". It needs none:
      `SYS_TEST_EXIT` is gated by the kernel's own `test-harness` feature and no syscap, and
      `SPAWN_SERVICE` already passes `namespace: 0, syscaps: 0` — an inherited LOOKUP-only root
      namespace, which is exactly what every existing test program gets. Parsing `syscaps` and
      `[handles]` is still worth doing when a service needs them; nothing here does.

- [x] **`service_toml` parses every service in a file** ✅ — `parse_all`, with the schema
      change that made it necessary. The original plan had the probe found by a **directory
      scan**, which the schema describes and nothing can do: the initramfs is a CPIO archive
      the kernel looks up by name, `sys_ns_enumerate` lists namespace bindings and says so, and
      `profile-server` projects only packages' `bin/`. One file with many `[service.<name>]`
      tables replaces it (`docs/spec/service-toml-schema.md`, changed 2026-08-21).

- [x] **`boot-probe` exists and is started** ✅ — a sixth bin in `test-harness`, present only
      in selftest images. It carries no checks yet and says so; Parts B and C move them in.
      What it proves now is the *plumbing*: a program the test image runs and the release image
      does not, started from data.

- [ ] **Move the checks in.** The SMP and floating-point probes from `session-mgr`
      (`sched_gate`, `cpus_with_switches`, `parse_field`, `fp_gate`, `fp_cpuid`,
      `fp_avx2_usable`, `fp_sum_squares_avx2` — 178 lines, Part B) and the five filesystem
      tests from `init` (330 lines in the test functions; 346 with their two helpers and their
      call sites, Part C), and with them the boot verdict.

      **The duplicate resolves here**: `sched_stats_demo` and `sched_gate` become one check.

- [x] **A service declaration starts it** ✅ — `[service.boot-probe]` is appended to
      `/initramfs/etc/services.toml` when `mode.features().is_some()`, so `init` and
      `service-mgr` are byte-identical in both images and one of them reads a file with an
      extra table. Governing decision 3, concretely.

- [x] **`init`'s `run_*` spawns became declarations** ✅ (Part C2) — the demo chain, the
      display self-test, `nxterm` and the two test clients. Their order is the file's order,
      which preserves the one constraint that mattered: `nxterm` before `ui-testclient`,
      because windows stack in creation order at the origin.

- [x] **`service-mgr` holds more than one service** ✅ — and this was the part nobody planned
      for. **No supervisor in this system could tell its children's exits apart**:
      `KIND_CHILD_EXITED` carries a pid, and nothing maps a process handle to a pid. Starting
      the probe alongside `heartbeat` made the probe's exit read as `heartbeat`'s, restarting a
      service that never stopped — demonstrated, not theorised.

      The fix needed **no kernel change**, which the option chosen for it assumed it would:
      each child's control channel is destroyed when it exits, the kernel already signals the
      survivor on the same path `sys_wait` uses, and a non-blocking `sys_channel_recv` answers
      `PeerClosed` (`-13`) rather than `WouldBlock` (`-11`). `KIND_PEER_CLOSED` remains
      unemitted and unneeded. See `TODO(child-exit-attribution)` for what is still open — the
      exit *code* is still unattributed.

- [x] **`test-qemu` asserts the attribution** ✅ — because the verdict cannot see it. With the
      pid-blind rule in place the guest restarts a live service and **still exits PASS**; the
      transcript is what distinguishes them, so `check_service_attribution` requires
      `'boot-probe' exited` and forbids `restarting 'heartbeat'`. Negative-controlled both ways.

- [ ] **Ordering.** The verdict must be last. `[service.<name>].after` is already in the schema
      and unparsed; either it lands here or the probe waits on what it needs. Deferred to
      Part B, which is when anything depends on the order.

      **Measured while probing Part A, and it is closer than it looks.** `boot-probe` exits
      microseconds after it starts, and `session-mgr` writes the verdict roughly 0.1 s later.
      A spin loop of 20 million iterations inserted into the probe was enough for the boot to
      finish first — so the probe never reached its exit, and the gate correctly reported no
      attribution. Once Part B moves real checks in, the probe *will* take that long. Whatever
      carries the verdict has to be ordered against the probe rather than racing it.

## Part B — `session-mgr` ships one login ✅ complete (2026-08-21)

- [x] **Move the login proof into `test-interactive` first** ✅ — steps 5a–5c: `$env.PWD ==
      $env.HOME`, a write to home read back, and a directory read finding the file. The
      fourth thing the script proved — a program from `/bin` runs — was already step 5.
      Closed **before** the box below it, so no coverage lapsed in between. `list . | count >= 1`
      became "the listing names the file", which a count cannot express and which is stronger
      anyway: a count of one passes on a home holding some *other* file.

- [x] **Delete the substitutions** ✅ — one `login()`, `tty_open` and the four `tty_*` helpers
      in every build, and no demo credential in the service. `session-mgr` has **zero**
      `#[cfg(feature = "test-harness")]`, down from 31 sites and 1171 lines to 857.

- [x] **The verdict comes from `boot-probe`** ✅ — with `sched_gate`, `fp_gate` and their four
      helpers, which were in `session-mgr` only because the verdict was. `fp_gate`'s own
      doc explains why it must sit immediately before the only `SYS_TEST_EXIT(PASS)` call:
      it was in the demo `parent` first and completed in 2 of 15 KVM runs, because whoever
      owns the verdict races the demo chain. That property moved intact — `init::supervise`
      runs the demo chain **synchronously** and only then hands off to the login chain that
      reaches `boot-probe`.

- [x] **A transcript assertion replaces the one verdict `session-mgr` still fired** ✅ — it
      called `verdict(false)` when its endpoint handoff failed, which is a session supervisor
      adjudicating a test run. Removing it would have let a broken login chain reach PASS,
      since nothing else in `test-qemu` reads it; `check_login_chain` requires the success
      line instead.

- [x] **The attribution assertion moved to `check-terminal`** ✅, and Part B is why. Once
      `boot-probe` writes the verdict, its last act terminates the machine — so under
      `test-qemu` `service-mgr` never sees it exit and there is nothing to attribute.
      `check-terminal` boots the **same image** without the `isa-debug-exit` device, so the
      verdict write is ignored, the probe reaches `exit(0)`, and the exit is attributed
      normally. Negative-controlled there: the pid-blind rule fails the gate.

**Two things this run exposed, both worth having.** `check-terminal`'s boot is long enough that
`heartbeat`'s **graceful-shutdown demo actually fires** — `'heartbeat' exited code=0`,
`stopped as requested (policy=always overridden)`. It never had before: `test-qemu` reached the
verdict first, so the control-channel shutdown path shipped untested. And `boot-probe` exits
microseconds after starting, while the verdict lands ~0.1 s later — twenty million spin
iterations in the probe were enough for the boot to finish first, which is what makes the
ordering box below real rather than theoretical.

## Part C — `init` ships PID 1

- [x] **The five filesystem tests leave PID 1** ✅ (2026-08-21) — 376 lines out of `init`,
      into `boot-probe`. They test `fs-server-ext4` through the namespace, which a program with
      the right bindings can do as well as init can — better, since it may fail without taking
      the boot with it.

      **They now gate the run, which they never did.** Every failure path in `init` was a bare
      `return` after a `FAIL` print — 19 of them — so a broken filesystem printed
      `init: create MISMATCH` and the boot passed. Each returns `bool` now and the verdict is
      their conjunction, `&` not `&&` so one failure does not hide the rest.

- [x] **A kernel bug came with them, and only this move could have found it** ✅.
      `AddressSpace::drop` took its own `LockRank::KernelObject` lock and then released the
      VMAs it owns — and a VMA holds the last `ObjectRef` to a file-backed mapping, whose drop
      takes the *file's* lock, same rank. `lock-order violation: acquiring KernelObject … while
      holding KernelObject`, kernel panic.

      It needed a process that **exits** holding the last reference to a file mapping. `init`
      ran these tests and never exits; `boot-probe` maps `/system/large.bin`, closes its
      handle, and exits. `Drop` has `&mut self`, so `SpinLock::get_mut` is both the fix and
      simply correct — there is nothing to exclude.

      **Found by `check-terminal`, not `test-qemu`**, for the reason Part B recorded: the probe
      writes the verdict, so under `test-qemu` the machine stops before it can exit.

- [ ] **The `/subtreetest` binding is the one cfg left in `init`, and the blocker is narrower
      than this box first said.** It read "data cannot express a namespace bind", which is true
      of *service* declarations — `[handles].namespace` is unparsed and a declared service gets
      `namespace: 0`, an inherited LOOKUP-only root — and was over-generalised. `init` reads a
      manifest too, `/initramfs/etc/init.toml`, and `MountSpec` already carries an
      `options: Option<Table>` that nothing consumes.

      **What is actually missing is a bind-mount concept in that manifest.** A `[[mount]]` entry
      spawns an fs-server for a device; `/subtreetest` is a *second bind of the endpoint already
      mounted at `/`*, scoped to a subtree, so declaring it as a mount would spawn a redundant
      server for the same partition. The manifest needs "bind an already-mounted server's
      endpoint at another path, with a subtree base" — which is not a test accommodation:
      `session-mgr` does exactly that for `/home` on every login, and it is what `mount --bind`
      is everywhere else.

      Two things need the binding: `boot-probe`'s `subtree_bind_test`, and the demo harness's
      case 8, which needs a binding that is *also* an openable directory to prove `move` refuses
      to recurse through a mount. Removing it without checking broke the second one.

      **Deferred past this plan deliberately** (maintainer's call, 2026-08-24): it is capability
      work on the boot manifest — critical path — rather than retrofit, and the retrofit's value
      does not depend on it.

      **`init` still has 20 `selftest` cfgs**, and they are C2's, not this one's: the demo
      chain (`run_test_harness`), the four `run_*` graphical spawns, and the
      `cfg(not(selftest))` supervision of `service-mgr` — ordinary code that becomes service
      declarations. An earlier draft of this box said `init` had "one cfg left", which would
      have scoped C2 to a namespace-bind mechanism and nothing else (PR #228 review,
      finding 2).

- [x] **The graphical spawns left too** ✅ (Part C2). `run_nxterm`, `run_ui_testclient`,
      `run_input_testclient` and `run_display_selftest` are declarations, which is also the
      honest fix for a comment `init` carried: *"Until Milestone 7 there is nothing to launch
      `nxterm` from."* There is — a service declaration — and after M7 the real answer is
      `desktop-shell`.

- [x] **`init`'s supervision of `service-mgr` is the same code in both images** ✅. It was
      `#[cfg(not(feature = "selftest"))]`: a test image reaped the demo `parent` as its primary
      child instead, so PID 1's supervision — and its restart when `service-mgr` dies — was
      code `test-qemu` structurally could not exercise. `supervise` is now three lines with no
      cfg, and `test_exit` is unconditional (the syscall returns `Unsupported` where the verdict
      device is absent, so a release `init` degrades rather than diverging).

- [ ] **`init` compiles identically in both images** — not yet, and the same single thing
      blocks it. 41 cfg sites became **1**; the ELFs differ by 4,096 bytes. This box and the one
      above are the same box, and both wait on a bind-mount concept in `init.toml`.

- [x] **Two mechanisms the plan had written off** ✅. `after` and `syscaps` are parsed now.
      Part A dropped `syscaps` as unnecessary — true of `boot-probe`, false of the milestone:
      the demo chain declares `BIND_NAMESPACE`, and moving it into a declaration without that
      stopped it at `session user bind FAIL` **while `test-qemu` still passed**. `after` is the
      ordering the verdict rests on, and it means "has exited" rather than the schema's
      unimplementable "reached ready state".

## Part D — the gate set, reconciled ✅ complete (2026-08-24)

**The measurement the decisions rest on.** Of the initramfs's **8** entries, **5 are
byte-identical** between a test image and a release image. The three that differ are
`etc/services.toml` and `etc/profiles/system.toml` — data — and `sbin/init`, the one remaining
`#[cfg(feature = "selftest")]`. The *set of files* is identical in both.

- [x] **They remain two gates, and the difference is stateable** ✅.

      They **cannot** merge as things stand, and the reason is structural rather than
      preferential: `boot-probe` fires `SYS_TEST_EXIT(PASS)` and QEMU terminates, so a
      self-adjudicating image cannot afterwards be typed at. `test-interactive` could be
      pointed at the test image without the `isa-debug-exit` device — that is exactly what
      `check-terminal` does — but then it would no longer boot the **release** image, which is
      its entire justification.

      And that justification survives the retrofit: `sbin/init` still differs, the kernel is
      built with `test-harness`, and the store carries a package a release image does not. The
      two gates also assert different things — `test-qemu` adjudicates the substrate,
      `test-interactive` drives the path a person takes.

- [x] **`SYS_TEST_EXIT` stays, and now for a narrower reason** ✅.

      Governing decision 4 kept it provisionally. Re-examined with `test-qemu` now carrying
      **four** transcript checks, it is no longer the adjudication — it is one signal of five,
      and everything it reports is also *printed*: a failed gate prints
      `boot-probe: … FAIL`, a critical-path failure prints `init: critical-path failure`, a
      panic prints `*** KERNEL PANIC ***`. An expect-only `test-qemu` is therefore possible.

      Three things keep it. It is the only signal that **cannot be produced by accident** — a
      log line can be; the syscall is served only by a kernel built with its own `test-harness`
      feature. It **terminates the run** at the decision point instead of leaving the runner to
      match a final line and kill QEMU. And the **kernel panic handler** can fire it from a
      context where further output may not survive, which is the case where a transcript is
      least trustworthy.

      What changed is the balance, and the doc says so: the device is the verdict, the
      transcript is the coverage, and new adjudication should go to the transcript.

- [x] **Root `CLAUDE.md`'s build-command list** ✅ — `test-interactive` and `check-input` were
      added with the plan itself; `check-images` joins them here.

- [x] **`docs/conventions/qemu-integration-tests.md`** ✅ — updated in each part as the thing it
      describes changed, rather than once at the end.

- [x] **`cargo xtask check-images`, so the result is a wall and not a number** ✅. It builds both
      initramfs archives and compares them entry by entry, failing on any divergence not on a
      short allow-list. Add a `#[cfg(feature = "test-harness")]` to `eshell` and its ELF starts
      differing, with nothing else in CI to notice — confirmed by doing it. Runs in the QEMU job,
      which already builds both images.

## What "done" means

Measured 2026-08-24. Three of four met; the fourth is one binding short, and the box for it is
open above rather than reworded.

- ✅ `session-mgr` has **zero** cfg sites (was 31). `init` has **one** (was 41) — the
  `/subtreetest` binding, which needs a bind-mount concept in `init.toml`.
- ✅ Everything else is `nxterm`'s observation prints (9) and the `test-harness` crate itself.
- ✅ `test-qemu`, `test-interactive`, `check-display`, `check-terminal`, `check-input`,
  `check-input --no-ps2-irq` and `check-images` all pass, and the boot verdict is written by one
  program — `boot-probe`, which did not exist when this plan was written.
- ⚠️ The two images build the same `session-mgr` and **not** the same `init`. Five of eight
  initramfs entries are byte-identical; of the three that differ, two are data.

**And a thing worth recording that was not a criterion.** Three separate times, moving a program
out of a supervisor silently moved its *verdict* nowhere — the demo chain, the display self-test,
and every spawn failure each reached PASS with the failure printed. Each was caught by a review
or a probe, never by a gate. The retrofit's rule is "the shipping path is the tested path"; the
lesson it did not anticipate is that **relocating a program means relocating whatever read its
result**, and nothing checks that for you.

## Out of scope, deliberately

- **`nxterm`'s nine sites.** Observation, plus one injection (F1 opens the menu so
  `check-terminal` can drive it without a pointer). The injection is worth revisiting when the
  gate can click the menu bar directly — which it can, since M6 C3 — but it is not what this
  plan is about.
- **The kernel's `test-harness` feature.** Governing decision 4.
- **Host unit tests.** `cargo xtask test` is unaffected; nothing here is about `#[cfg(test)]`.

## What this unblocks

**Milestone 7 writes three new processes** — `desktop-session-mgr`, `desktop-shell` and the
shared `libsession` — each of which would otherwise want an auto-login of its own. Finishing
this first is what makes "no substitution in the new code" a rule the milestone can simply
follow, and it is why the display arm's M7 names this plan as a prerequisite.

It also settles the shape of M7's **graphical login gate**: a greeter is drivable by exactly
the PS/2 injection `check-terminal` already uses, against a release-shaped image, with no
auto-login anywhere.
