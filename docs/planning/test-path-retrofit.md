# Nitrox Test-Path Retrofit — Subproject Plan

**Status:** 📋 planned 2026-08-21, not started. Sits between Milestone 6 and Milestone 7 of
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

Counting lines in cfg-gated **top-level items**, and splitting them by which build they are
absent from — a distinction that turns out to matter:

| Crate | cfg sites | Test apparatus | Shipping path, omitted under test | Total | Of the file |
|---|---|---|---|---|---|
| `init` | 41 | 684 | 0 | 684 | **32 %** |
| `session-mgr` | 31 | 208 | **117** | 325 | **28 %** |
| `nxterm` | 9 | ~21 | 0 | ~21 | 2 % |

`init` is PID 1, which [`init/CLAUDE.md`](../../userspace/init/CLAUDE.md) says must never
panic; a third of it is filesystem tests — `overwrite_test`, `grow_test`, `create_test`,
`subtree_bind_test`, `read_large_file` — plus the four `run_*` spawns of graphical clients.

**`session-mgr`'s middle column is the one to read.** 117 lines of it are not test apparatus at
all: they are the *shipping* login — the interactive `login()`, `tty_open`, `tty_request`,
`tty_write`, `tty_set_echo`, `tty_read_line`, `tty_close` — compiled **out** under
`test-harness`. That is code CI builds in one configuration and never executes in the other.
`nxterm`'s figure counts top-level items only; its remaining sites are inline blocks that print.

## The diagnosis, which is not "cfgs accumulated"

The probes are in `session-mgr` **because the verdict is in `session-mgr`**, and the verdict is
there because it is the last thing to run. `sched_gate`'s own comment says so:

> The clause-3 sched gate runs at the single PASS point … login proving alone must not PASS a
> boot whose SMP substrate is dead.

That is correct reasoning from a bad premise. It has already produced one visible duplicate:
`session-mgr::sched_gate` and `test-harness::sched_stats_demo` read the same
`/proc/sched/stats` for the same purpose, in two processes, because one of them is where the
verdict happens to live.

**Move the verdict to a program whose job is adjudication, and the probes follow it out.**

## The pattern already exists, and is proven

`cargo xtask test-interactive` boots `BuildMode::Normal` — the **release image** — and drives a
real login over the serial console: wrong password refused, right password accepted, a shell
prompt, a program from `/bin`, `Ctrl-C`, `exit N`. 71 expectations, expect-driven rather than
sleep-driven, and it runs in CI. Its own doc comment already makes this plan's argument:

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
3. **The test image differs from the release image by data, not code.** `service-mgr` already
   parses `[service.<name>]` declarations from `/initramfs/etc/services/`; the test image ships
   one more file. `init` and `service-mgr` compile identically in both.
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

- [ ] **`service_toml` learns `syscaps` and `[handles]`.** The parser's own header says these
      are "parsed as those features are consumed by later parts" — this is that part. A probe
      that reads `/proc/sched/stats` and writes the verdict needs stated authority, and stating
      it in the declaration is what keeps the difference between the images in *data*.
      Useful beyond testing: no service can currently declare what it needs.

- [ ] **`boot-probe`**, a sixth bin: the in-guest checks with no user-facing surface, in one
      place with one exit path. It takes the SMP and floating-point probes from `session-mgr`
      (`sched_gate`, `cpus_with_switches`, `parse_field`, `fp_gate`, `fp_cpuid`,
      `fp_avx2_usable`, `fp_sum_squares_avx2` — 178 lines) and the five filesystem tests from
      `init` (330 lines), and it writes the boot verdict.

      **The duplicate resolves here**: `sched_stats_demo` and `sched_gate` become one check.

- [ ] **A service declaration starts it**, shipped in the test image's initramfs and nowhere
      else. `run_test_harness`, `run_display_selftest`, `run_ui_testclient` and
      `run_input_testclient` become declarations too, which is what removes their `#[cfg]`
      blocks from `init` rather than relocating them.

- [ ] **Ordering.** The verdict must be last. `[service.<name>].after` is already in the schema
      and unparsed; either it lands here or the probe waits on what it needs. Decide in the
      part, do not assume.

## Part B — `session-mgr` ships one login

- [ ] **Delete the substitutions.** One `login()`, not two. `tty_open` and the four `tty_*`
      helpers compile in every build. No `DEMO_USER`/`DEMO_PASSWORD` constants in the service —
      the credential belongs to the gate that types it.

- [ ] **Move the login proof into `test-interactive` first.** The harness script asserts four
      things (`$env.PWD == $env.HOME`, a write to home round-trips, `list .` finds something,
      a program from `/bin` runs). `test-interactive` already covers the last;
      the other three become three added expectations. **This box closes before the box above
      it**, so the coverage exists before the script is removed.

- [ ] **`test-qemu`'s verdict comes from `boot-probe`.** With the auto-login gone, nothing in
      `session-mgr` writes one, which is the point.

- [ ] **What remains in `session-mgr` is a session supervisor.** ~1170 lines to ~910, and the
      part that is left is all about sessions.

## Part C — `init` ships PID 1

- [ ] **The five filesystem tests leave PID 1** (Part A gives them somewhere to go). They test
      `fs-server-ext4` through the namespace, which a program with the right bindings can do as
      well as init can — better, since it may fail without taking the boot with it.

- [ ] **The graphical spawns leave too.** `run_nxterm`, `run_ui_testclient`,
      `run_input_testclient`, `run_display_selftest` are declarations after Part A. This is also
      the honest fix for a comment `init` already carries: *"Until Milestone 7 there is nothing
      to launch `nxterm` from, so `init` does it in the test image and nowhere else."* There is
      something to launch it from before M7 — a service declaration — and after M7 the real
      answer is `desktop-shell`.

- [ ] **`init` compiles identically in both images**, which is the box that says this part is
      done.

## Part D — the gate set, reconciled

- [ ] **Decide whether `test-qemu` and `test-interactive` remain two gates.** Once the test
      image is the release image plus a service declaration, they boot nearly the same thing and
      differ only in whether a host types at the prompt. Merging is plausible and is not
      assumed here; what is *not* acceptable is two gates whose difference nobody can state.

- [ ] **Record the expect-only option, and reject or take it deliberately.** Governing decision
      4 keeps `SYS_TEST_EXIT`. If Part D concludes that host-side expect is sufficient — it is
      what every other gate already does — then the syscall and the `isa-debug-exit` device go,
      and that is a decision-log entry, not a refactor.

- [ ] **Root `CLAUDE.md`'s build-command list gains `test-interactive` and `check-input`.**
      Both run in CI; neither is listed. That omission is the likeliest reason the release-image
      pattern never spread — a session reading the project's own command list does not learn
      that the release image is bootable under test.

- [ ] **`docs/conventions/qemu-integration-tests.md`** describes the arrangement this plan
      changes, and is a current-behaviour doc. It updates in the same change.

## What "done" means

- `grep -rn 'feature = "test-harness"\|feature = "selftest"' userspace/init/src userspace/session-mgr/src`
  returns **nothing**.
- The remaining sites are `nxterm`'s observation prints and the `test-harness` crate itself.
- `test-qemu`, `test-interactive`, `check-display`, `check-terminal`, `check-input` and
  `check-input --no-ps2-irq` all pass, and the boot verdict is written by one program.
- The test image and the release image build the same `init` and the same `session-mgr`.

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
