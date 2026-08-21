# QEMU integration tests (`cargo xtask test-qemu`)

`cargo xtask test-qemu` boots Nitrox under QEMU **headless** and turns "did the
system boot correctly?" into a process exit code, so a regression fails CI instead
of requiring a human to read the serial log. It complements — does not replace —
the host-side unit tests (`cargo xtask test`) and the interactive
`cargo xtask qemu --selftest`.

## What it exercises

The whole boot, end to end: kernel bring-up (allocators, paging, APIC, timer, SMP,
scheduler) → the kernel boot self-tests (`kernel/src/boot_selftest.rs`) → the first
userspace process → init → ext4 mount via the userspace fs-server → init's demo
chain (the `parent`/`child` capability + IPC + page-cache exercises). **The
self-test payload _is_ the test suite** — there is no separate per-case framework
yet (see [Deferred](#deferred)).

## Mechanism

QEMU's `isa-debug-exit` device (`-device isa-debug-exit,iobase=0xf4,iosize=0x04`)
turns a guest I/O-port write into a host process exit: writing value `v` to port
`0xf4` makes QEMU terminate with status `(v << 1) | 1`. The guest writes a
**verdict**; the `xtask` runner maps the exit code to pass/fail.

Because a userspace process (init) can't touch I/O ports (ring 3), the write goes
through a kernel syscall, `SYS_TEST_EXIT` (`0xFFFF_0002`), which calls
`arch::debug_exit`. This is what lets the verdict come from **after** userspace has
run, covering the full boot rather than just `kernel_main`.

### The device is attached by `test-qemu` only, and the verdict is a no-op elsewhere

**Only `cargo xtask test-qemu` passes `-device isa-debug-exit`.** `check-input`,
`check-display`, `check-terminal` and `test-interactive` boot the *same*
`test-harness` image without it, because they need the guest alive and driveable long
after the boot self-test has reached its verdict. For those gates the port write lands
on no device, is ignored, and `SYS_TEST_EXIT` returns `Unsupported` to its caller,
which carries on. That is the designed behaviour, not a misconfiguration.

**`debug_exit` must therefore return rather than park the CPU**, and this is load-bearing
rather than stylistic. It used to fall through to a `hlt` loop, on the belief that reaching
there meant a broken host. The CPU halted with interrupts disabled while still counted in
`sched::online_mask`, so the next TLB shootdown — i.e. the next large `free` in any process —
waited forever for an acknowledgement it could never get, and the calling thread was stranded
on a CPU that would never run again. Both failures were invisible for as long as nothing
happened to need them, which is why `check-terminal` looked like a terminal that froze after
a particular keystroke. See the decision log, 2026-08-13.

### Who fires the verdict

| Situation | Verdict | Fired by |
|---|---|---|
| The substrate gates pass | PASS (`0x10` → exit **33**) | **`boot-probe`**, after `sched_gate` + `fp_gate` |
| A substrate gate fails | FAIL (`0x11` → exit **35**) | `boot-probe`, same call |
| A demo child crashes | FAIL | `init`, on the non-zero reap code |
| Spawn / critical-path boot failure | FAIL | `init` (`supervise` spawn-fail; `emergency`) |
| Kernel `panic!` | FAIL | the kernel panic handler (`main.rs`) |
| Kernel triple-fault | (nonzero) | QEMU itself, via `-no-reboot` |
| Hang (no verdict) | timeout → FAIL | the runner's wall-clock timeout |

**PASS moved to `boot-probe` on 2026-08-21** (retrofit Part B); before that it was
`session-mgr`, after its auto-login proved. The row above said `init` fired it, which had not
been true for longer than that — `init` has no `test_exit(true)` call and only ever fires FAIL.

**The ordering is `init::supervise`'s, and it is what makes the gates mean anything.** The demo
chain runs synchronously and a non-zero exit fails the run there; only then is the login chain
handed off, and only then does `service-mgr` start `boot-probe`. So the gates run last, which is
the property `fp_gate` needs — it completed in 2 of 15 KVM runs when it lived in the demo
`parent`, because whoever owns the verdict races the demo chain.

The runner treats exit **33** as the only pass; everything else (35, 124 timeout,
triple-fault, signal) is a failure. `isa-debug-exit` can never produce exit `0`
(the low bit is always set), so "pass" is a chosen odd code, not zero.

### The exit code is not the only assertion

A verdict is one bit, and some defects do not reach it. `test-qemu` therefore also matches
against the **captured serial transcript** after a PASS, and a transcript check failing fails
the run.

`test-qemu` has one: **`check_login_chain`**, requiring
`session-mgr: received fs + profile endpoints + auth channel`. It replaces a `verdict(false)`
`session-mgr` used to fire when its endpoint handoff failed — a session supervisor adjudicating
a test run — and without it a broken login chain reaches PASS, because nothing else in
`test-qemu` reads it. It asserts the chain came *up*, not that anyone logged in: nothing types
a password here.

**`check-terminal` has the other: `check_service_attribution`**, and *why it is there* is the
useful part. `service-mgr` supervises two children in a test image, and a supervisor that mixes
their exits up restarts a service that never stopped — while every child still exits 0 and the
guest still reports PASS. It was run that way deliberately to confirm it. But once `boot-probe`
became the verdict-writer, its last act terminates the machine, so under `test-qemu` nothing
ever sees it exit. `check-terminal` boots the **same image without the `isa-debug-exit`
device**: the verdict write returns `Unsupported`, the probe reaches `exit(0)`, and the exit is
attributed like any other service's. Requiring `service-mgr: 'boot-probe' exited code=0` and
forbidding `service-mgr: restarting 'heartbeat'` distinguishes them.

That relocation is a general point about this device: **a gate that boots without
`isa-debug-exit` sees the guest keep running past the verdict**, and can therefore assert things
the adjudicated run structurally cannot.

**When to add one:** the behaviour is observable on the console, and a wrong version of it
would still reach the same verdict. If a defect *would* change the exit code, the exit code is
the better assertion — a transcript match is coupled to log wording, which is a cost.

## The `test-harness` feature

`test-qemu` builds the kernel and `init` with the **`test-harness`** cargo feature
(`= ["selftest"]`), which is distinct from `selftest` because it changes *terminal*
behavior:

|  | `selftest` (`xtask qemu --selftest`) | `test-harness` (`xtask test-qemu`) |
|---|---|---|
| Boot self-tests / demos | run | run |
| After the demos | drop to the interactive `eshell` | fire the PASS verdict, exit QEMU |
| On a kernel panic | print + halt (inspect in GDB) | fire the FAIL verdict, exit QEMU (and halt if no device is attached) |
| Display / serial | interactive | headless, serial captured |

`test-harness` is compiled out of production kernels entirely: `SYS_TEST_EXIT`,
`arch::debug_exit`, and the panic-handler exit path only exist under it — there is
no emulator-exit backdoor in a shipping build, and it is not in the ABI hash.

## Running it

```
cargo xtask test-qemu        # exit 0 = pass, nonzero = fail; serial echoed to stdout
cargo xtask test-qemu --kvm  # same, under hardware virtualisation
```

It builds a `test-harness` image, runs QEMU with `-smp 4` (so the SMP
distribution/affinity self-tests are meaningful), `-display none`,
`-serial stdio`, `-no-reboot`, and the `isa-debug-exit` device, all under a
`timeout(1)` ceiling.

### Host requirement: x2APIC

The kernel is **x2APIC-only** (decision log, 2026-06-26 — the ≈2014 baseline
guarantees it, so no xAPIC fallback is carried), which puts a floor on the host:

| Accelerator | Requirement |
|---|---|
| TCG (default) | **QEMU ≥ 9.0** — TCG only emulates x2APIC from 9.0 |
| KVM (`--kvm`) | `/dev/kvm`; any QEMU, since KVM's in-kernel APIC has long supported x2APIC |

On an older QEMU under TCG the guest panics with `CPU lacks x2APIC`, which reads
like a kernel bug and is not one. `xtask` **preflights** this: it checks the QEMU
version (TCG) or `/dev/kvm` (KVM) *before* launching and fails with an actionable
message instead. This is why CI runs `--kvm` — GitHub's `ubuntu-latest` ships
QEMU 8.2.

`--kvm` is also the fast path, which matters for boot-loop campaigns: the project
convention for a change touching the scheduler, the fault path, or process
lifecycle is a KVM boot loop (0/60 has been the bar) on top of a single
`test-qemu`.

## Adding coverage

Today: add an assertion to the existing self-test payload — a kernel check in
`kernel/src/boot_selftest.rs`, or a userspace exercise in init's `selftest` block.
A check that fails should `panic!` (kernel) or drive init to a non-zero verdict
(userspace); either fails the run. Keep additions deterministic and free of
wall-clock/timing assumptions (TCG timing is not real hardware).

## Deferred

- **A per-case framework** under `tests/qemu-tests/` (independent named cases,
  selective runs, structured result reporting). Trigger: a test that needs to
  assert something the boot chain doesn't already exercise, or isolation between
  cases. Until then the single boot-and-adjudicate run is the harness.
- **An `-smp` matrix** (running the suite at `-smp 1`, `2`, `4`). Today it runs
  `-smp 4` only.
- **CI wiring** (running `test-qemu` on a runner with KVM/TCG + OVMF). The harness
  is CI-ready; the pipeline that invokes it is separate.
