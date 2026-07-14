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

### Who fires the verdict

| Situation | Verdict | Fired by |
|---|---|---|
| Demo chain reaps cleanly | PASS (`0x10` → exit **33**) | `init` (`test_exit`) after `parent` reaps with code 0 |
| A demo child crashes | FAIL (`0x11` → exit **35**) | `init`, on the non-zero reap code |
| Spawn / critical-path boot failure | FAIL | `init` (`supervise` spawn-fail; `emergency`) |
| Kernel `panic!` | FAIL | the kernel panic handler (`main.rs`) |
| Kernel triple-fault | (nonzero) | QEMU itself, via `-no-reboot` |
| Hang (no verdict) | timeout → FAIL | the `timeout(1)` wrapper (90 s) |

The runner treats exit **33** as the only pass; everything else (35, 124 timeout,
triple-fault, signal) is a failure. `isa-debug-exit` can never produce exit `0`
(the low bit is always set), so "pass" is a chosen odd code, not zero.

## The `test-harness` feature

`test-qemu` builds the kernel and `init` with the **`test-harness`** cargo feature
(`= ["selftest"]`), which is distinct from `selftest` because it changes *terminal*
behavior:

|  | `selftest` (`xtask qemu --selftest`) | `test-harness` (`xtask test-qemu`) |
|---|---|---|
| Boot self-tests / demos | run | run |
| After the demos | drop to the interactive `eshell` | fire the PASS verdict, exit QEMU |
| On a kernel panic | print + halt (inspect in GDB) | fire the FAIL verdict, exit QEMU |
| Display / serial | interactive | headless, serial captured |

`test-harness` is compiled out of production kernels entirely: `SYS_TEST_EXIT`,
`arch::debug_exit`, and the panic-handler exit path only exist under it — there is
no emulator-exit backdoor in a shipping build, and it is not in the ABI hash.

## Running it

```
cargo xtask test-qemu      # exit 0 = pass, nonzero = fail; serial echoed to stdout
```

It builds a `test-harness` image, runs QEMU with `-smp 4` (so the SMP
distribution/affinity self-tests are meaningful), `-display none`,
`-serial stdio`, `-no-reboot`, and the `isa-debug-exit` device, all under a
`timeout(1)` ceiling.

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
