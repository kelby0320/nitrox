# Power

**Status: the kernel half built with administration Part E.3; last checked 2026-09-28.** The
system-control object, `sys_power`'s halt and reboot, and the FADT facts a reset and the clock
use. Not built: `shutdown`, and the sequence that stops the sessions, the services and the
filesystems before `init` calls `sys_power` — administration Part E.4,
[`administration.md`](../planning/administration.md). Nothing calls `sys_power` yet. Not built at
all: power-off, which needs AML.

## The system-control object

**One exists.** `run_first_userspace` makes it and hands it to `init` in `rdx`, beside the
notification channel and the root namespace ([`boot-flow.md`](boot-flow.md) §4). Its handle
carries `WRITE`, which `sys_power` requires, and `INSPECT`, and neither `DUPLICATE` nor
`TRANSFER`:

- `init` cannot give it away, even by mistake — a duplicate is refused, and nothing without
  `TRANSFER` can be sent or granted at spawn;
- nothing binds it in a namespace, so no lookup yields it.

So the process that stops the machine is the one the kernel started first. That is deliberate:
`init` mounted the root, and a shutdown's last steps are its own — its mounts written back and
marked clean, then the power operation.

It is a **handle, not a syscap**. A syscap is ambient: every process `init` spawns with it could
stop the machine. The object is held, so exactly one process can.

`init` checks what `rdx` holds with `sys_handle_stat` and says `init: holds the system-control
object`, or that it has none, in which case the boot goes on and a shutdown could reach every step
but the last. The object is `kernel/src/object/system_control.rs`, type `15`.

## `sys_power`

`sys_power(system_control, op)` ([`syscall-abi.md`](../spec/syscall-abi.md) § Power) is
`kernel/src/power.rs`, in three steps.

1. **Every disk is flushed.** `IoOpcode::Flush` — AHCI's `FLUSH CACHE EXT` — goes to every block
   device except a partition, whose flush is its disk's, all at once. The kernel waits for them for
   at most **10 seconds together**, then logs each disk that failed or did not finish, and
   `power: flushed N of M disks`. A disk that does not finish does not stop the stop: a machine
   that cannot be stopped is worse than a drive that took too long. By the time `init` asks, every
   filesystem has been written back, so a flush has little to move.
2. **Every other processor stops**, by the path a panic takes: `Cpu::stop_other_cpus`, which
   `stop_the_machine` is built on. From here nothing takes a lock or allocates, since a stopped
   processor may have held either.
3. **A halt** writes *"It is now safe to turn off your computer."* on COM1, through the emergency
   writer, and as the screen's last line, taking the screen back from whoever held it — the
   compositor, usually — the way a panic does (`fbcon::reclaim_for_stop_with`). Then this
   processor halts too. The machine stays on, showing the message, until its button is held.
   **A reboot** writes *"Restarting."* the same way and resets.

**It is outside the async-first rule.** A blocking operation hands back a `PendingOperation`,
and `sys_power` never returns, so there is nothing to hand one to. Its flush waits in the kernel,
bounded. It returns only to refuse: a handle that is not the object (`InvalidHandle`, `NoAccess`
or `InvalidArgument`), then an `op` that names nothing (`InvalidArgument`).

**There is no power-off.** Entering S5 means evaluating the `\_S5` object, which is AML, and AML
waits for ACPICA ([`why-phased-acpi.md`](../rationale/why-phased-acpi.md)).

## Resetting

`kernel/src/arch/x86_64/reset.rs`, behind `Platform::prepare_reset` and `Platform::reset`. It
tries, in order, giving each half a second and saying on COM1 which it is:

1. **The FADT's reset register**, when the FADT advertises one (`RESET_REG_SUP`) in a space this
   kernel writes: an I/O port; physical memory, mapped by `prepare_reset` while the machine still
   runs, because `reset` must not allocate; or a PCI configuration register of bus 0, through the
   legacy `0xCF8`/`0xCFC` mechanism. q35's is I/O port `0xcf9`, value `0xf`.
2. **The 8042's reset line** — command `0xFE` on port `0x64` — on every machine, whatever
   `IAPC_BOOT_ARCH` says: firmware gets that bit wrong both ways, and a pulse sent where there is
   no 8042 does nothing.
3. **A triple fault**: an empty IDT and an exception nothing can deliver. It cannot fail, since a
   PC answers a processor shutdown by resetting, so it is what `reset` ends in rather than a step
   that might be skipped.

Each resets q35 on its own. That was checked by booting each alone, with the ones before it
removed; an inert write to the reset register was also checked, and the chain moved on to the
8042 after its half second, which is what shows the wait works with interrupts masked.

## The FADT

`kernel/src/arch/x86_64/acpi.rs` reads four things from it, each only if the table's length holds
it. A revision-1 FADT ends at byte 116, before the reset register:

| Offset | Field | Used for |
|---|---|---|
| 108 | `CENTURY` — the RTC's century register, 0 for none | the wall clock's century |
| 109 | `IAPC_BOOT_ARCH` — bit 1, an 8042 | the report |
| 112 | `Flags` — bit 10, `RESET_REG_SUP` | whether the reset register counts |
| 116, 128 | `RESET_REG` (a Generic Address Structure) and `RESET_VALUE` | the first way to reset |

The hardware report prints them as four `fadt:` lines, so the laptop's report says whether it can
reset by register. The RTC reads the century register when the FADT names one, in the chip's own
BCD or binary, and believes only 19–21; otherwise the year is 2000–2099, as before.

## What gates it

- **Host tests:**
  - the FADT parser at lengths from 244 down to 10;
  - the reset order, and which reset registers are written;
  - the PCI configuration address;
  - the RTC's century;
  - the last line's row;
  - the object's rights, which a duplicate is refused;
  - the op's decoding, and which block devices are flushed.
- **`cargo xtask test-qemu`:**
  - q35's four `fadt:` lines;
  - `init: holds the system-control object`;
  - `boot-probe` calling `sys_power` with no handle, a lookup-only namespace and a writable disk,
    each refused. A kernel that checked the right and not the type halts the machine there, and
    the run times out.
- **`cargo xtask check-report`**: the same `fadt:` lines, read off the live image's report.
- **Not yet by a gate: a halt or a reset itself.** Administration Part E.4's `check-shutdown` will
  boot one of each. E.3's were booted by hand, with a probe in `init`.
