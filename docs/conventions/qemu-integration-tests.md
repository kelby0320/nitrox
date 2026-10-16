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
| Every substrate check passes | PASS (`0x10` → exit **33**) | **`boot-probe`**: `sched_gate`, `fp_gate`, and the four filesystem checks + `subtree_bind_test` |
| Any one of them fails | FAIL (`0x11` → exit **35**) | `boot-probe`, same call |
| A critical-path boot failure | FAIL | `init` (`emergency`) |
| Kernel `panic!` | FAIL | the kernel panic handler (`main.rs`) |
| Kernel triple-fault | (nonzero) | QEMU itself, via `-no-reboot` |
| Hang (no verdict) | timeout → FAIL | the runner's wall-clock timeout |

**PASS moved to `boot-probe` on 2026-08-21** (retrofit Part B); before that it was
`session-mgr`, after its auto-login proved. The row above said `init` fired it, which had not
been true for longer than that — `init` has no `test_exit(true)` call and only ever fires FAIL.

**The ordering is the declarations file's**, since retrofit Part C2 (2026-08-24). `boot-probe`
declares `after = ["test-harness"]`, so `service-mgr` does not start it until the demo chain has
exited; everything else is started in file order. That is the property `fp_gate` needs — it
completed in 2 of 15 KVM runs when it lived in the demo `parent`, because whoever owns the
verdict races the demo chain. `init::supervise` enforced it by running the chain synchronously
until C2; it does not any more, and it no longer reads the chain's exit code either.

**Which is why more of the adjudication is on the host now.** A one-shot declared with
`policy = "never"` has its exit code logged and read by nobody, so anything that used to be "init
saw a non-zero exit" is a transcript check below.

The runner treats exit **33** as the only pass; everything else (35, 124 timeout,
triple-fault, signal) is a failure. `isa-debug-exit` can never produce exit `0`
(the low bit is always set), so "pass" is a chosen odd code, not zero.

### The exit code is not the only assertion

A verdict is one bit, and some defects do not reach it. `test-qemu` therefore also matches
against the **captured serial transcript** after a PASS, and a transcript check failing fails
the run.

`test-qemu` has ten (the list is `cmd_test_qemu`'s PASS arm, and it wins over this count); the ones with a story worth
keeping are below, and `check_ahci_msi_path`, `check_block_read_selftest` and
`check_oversize_refused` each carry theirs in a doc comment. **`check_login_chain`** requires
`session-mgr: received fs + profile endpoints; auth resolved from /svc/auth` (the wording changed
in M7 Part C, when the auth channel stopped being couriered and became a namespace resolve). It replaces a `verdict(false)`
`session-mgr` used to fire when its endpoint handoff failed — a session supervisor adjudicating
a test run — and without it a broken login chain reaches PASS, because nothing else in
`test-qemu` reads it. It asserts the chain came *up*, not that anyone logged in: nothing types
a password here.

**`check_hardware_facts`** (Phase 5 Part D) requires the lines of the hardware report every boot
logs that describe QEMU's machine as it is: q35's ECAM window, four enabled MADT CPUs and one IOAPIC,
QEMU's ACPI tables by signature and OEM, the framebuffer's geometry with its padding, the AHCI
controller **claimed** over MSI, COM1 present, and the pinned Limine. The boot passes with any of
them wrong, which is why it is a transcript check; `check-report` asserts the other half — a
declined controller, no COM1 — off the screen of a boot with no serial port.

**`check_demo_chain`** requires `test-harness: all smoke tests passed`, **`check_display_selftest`**
requires `display-selftest: PASSED`, and **`check_every_service_started`** forbids
`service-mgr: image not found` and `service-mgr: spawn FAIL`. All three replace adjudication
`init` used to do by reading an exit code, and all three were added because the run passed
without them: the demo chain stopped at `session user bind FAIL` with half the spawns missing, a
forced display-selftest failure printed `FAILED` and `exited code=1`, and a service pointed at a
nonexistent path simply never started.

**`check-terminal` has one of its own: `check_service_attribution`**, and *why it is there* is
the useful part. `service-mgr` supervises seven children in a test image, and a supervisor that mixes
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

### Why `isa-debug-exit` stays, when the transcript could carry everything

Re-examined at the end of the test-path retrofit (2026-08-24), with `test-qemu` now carrying
four transcript checks. Everything the exit code reports is **also printed**: a failed gate
prints `boot-probe: … FAIL`, a critical-path failure prints `init: critical-path failure`, a
panic prints `*** KERNEL PANIC ***`. An expect-only `test-qemu` is possible.

Three things keep the device:

- It **cannot be produced by accident**. A log line can be; `SYS_TEST_EXIT` is served only by a
  kernel built with its own `test-harness` feature.
- It **terminates the run** at the decision point, instead of leaving the runner to match a
  final line and then kill QEMU.
- The **kernel panic handler** fires it from a context where further output may not survive —
  exactly where a transcript is least trustworthy.

**What changed is the balance.** The device used to be the whole adjudication and is now one
signal of five. The rule that follows: *the device is the verdict, the transcript is the
coverage.* New adjudication goes to the transcript — it says which of several things went wrong,
where an exit code says only that something did.

### Why `test-qemu` and `test-interactive` are two gates

They cannot merge, and the reason is structural. `boot-probe` fires PASS and QEMU terminates, so
a self-adjudicating image cannot afterwards be typed at. `test-interactive` could be pointed at
the test image with the device absent — that is what `check-terminal` does — but then it would
not boot the **release** image, which is its whole justification.

That justification survives the retrofit, and the measurement is `cargo xtask check-images`: of the
initramfs's five files, four are byte-identical between the two images, and the one that differs is
data — the mount manifest, whose test copy adds two `[[bind]]`s. The declaration file and the
profile manifest differ too, **on the root** since administration Part E.1c, which `check-images`
compares as well: the two roots are identical but for those two files, the test package, and **one
program — `nxterm`**, built with `test-harness` in a test-harness image so that it reports each
completed row for `check-terminal` (PR #194). Every other program is byte-identical, the store's as
well as the initramfs's (`sbin/init` differed until Phase 5 Part C.1); `nxterm` went unseen until
E.1c, because the initramfs was all that was compared. The kernel differs (`test-harness`), and the
store carries a package a release image does not. The gates also assert different things:
`test-qemu` adjudicates the substrate, `test-interactive` drives the path a person takes.

## The `test-harness` feature

`test-qemu` builds the kernel with the **`test-harness`** cargo feature
(`= ["selftest", "ps2-hold-gate"]`),
which is distinct from `selftest` because it changes *terminal* behavior. No userspace program
takes either: `init` was the last, until Phase 5 Part C.1 turned its one test-only branch into
manifest data. What a test image's userspace does differently comes from its data — declarations,
the mount manifest, and the `test` store package.

**And its machine has one more disk.** Since administration Part C.5b a test image's ESP carries
`scratch.img`, an 8 MiB ext4 labelled `nitrox-scratch` holding a `README` — and since Phase 6
Part H.4 `ahead`, a file of 64 pages nothing opens before `boot-probe` — as a second Limine
module, which the kernel publishes as a RAM disk. It is what the storage service finds to
auto-mount, since the boot disk holds only `init`'s root and the FAT ESP, and so what lets
`boot-probe` mount, read and write through `/svc/storage` on every run — and, since Phase 6 Part
H.2, hold the I/O counters to a write: a file of 64 pages synced there must move the scratch disk's
written bytes and the page cache's write-backs by at least the file's in `io.tsm`, and the disk
`init`'s root is on must have read something — and since Part H.4 move the scratch disk by no more
than 40 writes, the data a run of pages a write-back IRP and the zeroes 64 KiB a write, where a page
a command took 145, and by no read a page, 32 reads in all, since what a grow added fills as zero
without a read and the server reads its metadata through a cache. Then `ahead` is read back through
a mapping, as written, in at most eight reads: each fill reads up to 31 pages ahead of its own. `test-interactive` holds `nxsh`'s
`time` on the release image: its line, of its shape, before the value it passes through. The
programs are the release programs, and a release boot simply has one disk fewer. **The cost is the
block devices' order**: a RAM disk is published before any partition, so a test image's ESP is
`/dev/blk/2` where a release image's is `/dev/blk/1`. Nothing may find a device by its index for
that reason; the registry says what each one is. The two modes differ in:

|  | `selftest` (`xtask qemu --selftest`) | `test-harness` (`xtask test-qemu`) |
|---|---|---|
| Boot self-tests / demos | run | run |
| After the demos | drop to the interactive `eshell` | fire the PASS verdict, exit QEMU |
| On a kernel panic | print + halt (inspect in GDB) | fire the FAIL verdict, exit QEMU (and halt if no device is attached) |
| Display / serial | interactive | headless, serial captured |

`test-harness` is compiled out of production kernels entirely: `SYS_TEST_EXIT`,
`arch::debug_exit`, and the panic-handler exit path only exist under it — there is
no emulator-exit backdoor in a shipping build, and it is not in the ABI hash.

**It also implies `ps2-hold-gate`**, a gate hook rather than a mode: an F9 press makes the PS/2
driver read nothing from the i8042 for 300 ms. `check-input` uses it to build, on purpose, the
state behind the `lost-release` flake — QEMU holding a mouse release its sixteen-byte queue had no
room for, until the next injected event — and to prove that the gates' flush delivers it
(`expect_after_pointer` in `tools/xtask`). Nothing else presses F9, and a kernel without the
feature has no hold at all. `check-input --no-ps2-irq` does not run that step: with only the tick
reading the controller, a click from an unknown position is not reliable enough to build on.
`check-input --usb` does not either: it holds the i8042's drain, and that machine has no i8042.

**`--usb` boots a machine with no i8042** (Phase 6 Part B): q35 with `i8042=off`, the gates' xHCI
controller, a `usb-kbd` and a `usb-mouse`, so a key or a click that arrives came through USB or not
at all. `check-input`, `check-login` and `check-report` take it, each asserting what it does on the
i8042 less what is the i8042's own: `check-input` the held-release step, and `check-report` the
FADT's 8042 and the first round's count, for which it asserts the machine's instead.
`check-input --usb` adds what is USB's own: that the keyboard acknowledged its lights (Phase 6 Part
B.5), and **keyboards that come and go** (Part C). Over QMP it unplugs the boot keyboard, plugs
another in and types on it, then holds a key down on that one and unplugs it — and the test client's
window sees the release, which QEMU cannot have sent: its HID device sends nothing for a key held
when it goes. Each answer is matched from the QMP command that caused it (`Session::line_since`),
whichever process prints first. A last keyboard goes in for the rest of the gate. At its end the
transcript must hold one `left` line per unplugged keyboard and neither line an input server prints
for a node it goes on reading after its device has gone.

**`test-qemu`'s hot-plug cannot hold the device manager to following the registry.** It lands
before `init`, so on either side of the manager's first read, and when it has come and gone before
that read, a manager that never follows still agrees with the registry. `check-input --usb` orders
its own plugs, so it is the gate that does.

**A stick is QEMU's `usb-storage`, and what it holds is the host's to check** (Phase 6 Part D).
`test-qemu`'s is an MBR naming one FAT16 partition (`mbr_fat_stick`), as a shop sells one, so the
first round reads a table and the storage service reports a filesystem it does not mount — with
512-byte clusters, which `fs-server-fat` refuses, so the boot's set of mounts is the same with the
FAT server as without it (Phase 6 Part E). `check-storage`'s FAT stick is the one that is served: a
300 MiB FAT32 formatted in place in a sparse image by `mformat -i <file>@@<offset>`, so the gate
writes no 300 MiB file, and carved back sparsely for `fsck.fat`. Its data region is off a 4 KiB
boundary, the case files mapped in sectors exist for, and `xtask` asserts that from the boot sector
before the boot, since a formatter that aligned it would leave the gate proving less. **The serial
line carries ASCII alone** — the line discipline drops every byte past it — so a Unicode name the
guest writes is one it took from a listing, never one typed.
`check-storage` plugs its sticks in after the login: each is a node added with `blockdev-add` and a
`usb-storage` device on it, and `device_del` pulls the device and leaves the node, so the same image
goes in again as a new device. Each is a file the host reads once the machine has stopped — carved
by its MBR where it has one — as it reads the SATA disk. A command in flight at the moment of an
unplug is the driver's host tests' alone: no gate can place an unplug inside one command's few
milliseconds without depending on timing.

**A drive past 2 TiB is a sparse file** (Phase 6 Part G). `test-qemu`'s sixth USB device is a 3 TiB
image, `set_len` and nothing written, attached **last** so every other device keeps its port and
every fact naming one stays true. It costs nothing on the host's disk: `boot-probe` writes one
sector at block 2³² + 7, and the host reads that sector back out of the image after the run and
then removes the file. **The host's read is the assertion**: the guest reading back what it wrote
proves only that the write and the read named the same block, and a block number cut to 32 bits
names block 7 for both. `boot-probe`'s test passes, saying so, on any machine without such a drive,
since every `--selftest` gate runs it; `test-qemu` holds its line.

**A stick the guest formats is checked twice** (Phase 6 Part G). `check-storage`'s blank stick is
formatted ext4 whole, then partitioned again and formatted FAT, all in one boot, so the host copies
the image once the ext4 is ejected — before the MBR replaces its GPT — and checks the copy and the
final image after the machine stops, as it checks a stick pulled while mounted. **Which partition
is the boot stick's is asked of `/dev/devices`** (`filter parent == "blk-<n>"`), not read off the
report lines: the report gives a partition's reason, and the live stick's ESP's is its clusters, not
that it is on the disk the machine started from.

**`check-media` plugs a stick into a desktop and reads the result off the screen** (Phase 6 Part
F). It boots the **release** live image beside a copy of the release disk and logs in at the
graphical greeter, so what it drives is what a person has: Files, the editor, the eject button.
Its one claim no log line could carry is that **a stick appears in Files with nothing typed or
moved after the plug** — any input would wake Files on its compositor channel and draw the row
whether the storage service's watch had woken it or not — so after the `device_add` it only takes
screendumps, until the row's label and its eject glyph have ink where the browser's metrics put the
second drive row. Where it aims inside a dialog it does not write down: the editor's chooser is laid
out on the host by `libui`, with the theme the image stages, and the gate clicks where that layout
puts the Up button. **A cursor is drawn into the screen a gate reads**, so a check for something
gone looks where the pointer is not.

**A gate that boots a test image and types meets `boot-probe` unless it waits for it.** `boot-probe`
starts once the test harness's chain has finished, and some of its tests change what a session
leans on: they install a policy, and open view-broker clients until one is refused. `check-shutdown`
logs in after its verdict, and `check-storage` after its exit — whatever its verdict, since on the
live stick's machine it takes the SATA disk's `nitrox-root` for its root and fails. Before Part D
`check-storage` ended before `boot-probe` began; the sticks made it long enough to meet it.

**A gate that plugs and unplugs must not do it under `boot-probe`'s registry test**, which reads the
device table once and looks up each present record's paths. `check-input --usb` waits for that test
to report before its keyboards come and go (`await_registry_probe`): a kept `--kvm` transcript had
it reading the table between one keyboard's unplug and the next one's plug, and finding none. It
waits for that test rather than the whole verdict, because the test client gives up after twenty
seconds without an event; measured, the wait is under a second under KVM and nothing under TCG.

**QEMU's trace is a gate's window onto a device the guest cannot report on** (Phase 6 Part B.5).
`check-terminal` boots with `-trace ps2_set_ledstate -trace ps2_reset_keyboard -D <file>` and reads
the PS/2 keyboard's lights from that file as it types: Num Lock's alone before anything is injected,
then Caps Lock's added and taken away, then Num Lock off and on — each step the *exact* sequence
since the kernel's reset, so a write too many fails as surely as one missing. The trace is QEMU's
log backend, written as it happens, and the same flags work on CI's 8.2. Only what follows the last
`ps2_reset_keyboard` is the system's: the firmware resets and sets the keyboard first. QEMU's USB
keyboard traces nothing of its lights, which is why `--usb` asserts the acknowledgement instead.

## Running it

```
cargo xtask test-qemu        # exit 0 = pass, nonzero = fail; serial echoed to stdout
cargo xtask test-qemu --kvm  # same, under hardware virtualisation
```

It builds a `test-harness` image, runs QEMU with `-smp 4` (so the SMP
distribution/affinity self-tests are meaningful), `-display none`,
`-serial stdio`, `-no-reboot`, and the `isa-debug-exit` device, all under a
`timeout(1)` ceiling.

### The screen a gate boots

**`test-qemu` and `test-interactive` boot QEMU's default screen, 1280×800; every gate that looks at
or clicks on a screen boots 1360×768** — `check-display`, `check-input`, `check-terminal`,
`check-login`, `check-logout`, `check-fbcon`, `check-live`, `check-report`, `check-install`,
`check-storage`, `check-media`, `check-shutdown`, `check-recovery`, and the `shot` and
`bench-compose` tools (Phase 5 Part E). So every CI run boots two sizes, and a size written back
into a client fails somewhere. `--size WxH` boots one of those at another size; `qemu_base_args`
takes the screen from every caller, so a new boot cannot forget to say.

- **The size is QEMU's, not the image's**: `-vga none -device VGA,xres=…,yres=…` makes QEMU's
  EDID prefer the mode, and OVMF and Limine boot into it.
- **The width must be a multiple of 8.** QEMU's VGA rounds the width down while OVMF reports the
  one it asked for, so the guest's rows and QEMU's disagree and the picture shears — while the
  guest's own `framebuffer:` line and the self-test verdict, which a wrong stride cannot fail, still
  read correct. `DisplaySize::parse` refuses such a width. 1360×768 is the laptop's 1366×768 as near
  as QEMU shows it.
- **A gate aims at a size it derives, not one it reads.** The bottom bar's click height, its
  task buttons, show-desktop and the desktop's name, the sidebar, the pointer's pin corner and the
  screen's centre are `DisplaySize` methods over the chrome metrics the gate writes down a second time (M11 decision 2); the
  expected wallpaper line is `libdraw::scale::fill` at the size.
- **Pointer travel is sized from the screen too.** A pin over-drives the longer side
  (`DisplaySize::pin_motions`), and a drag to an edge travels a screen's width, paced. Both were
  fixed numbers that worked at 1280 and not at 2560 or 1920.

`cargo xtask check-resolutions` runs `check-display`, `check-terminal`, `check-login` and
`check-fbcon` at five sizes, each as a child with its transcript kept under
`build-cache/resolutions/`, and prints a table — **by hand, not in CI**. It reports, beside each
result, whether a boot's demo chain finished, since that is where a size-dependent failure outside
the gates has shown up (the dead-log-source check at 1024×768, decision log 2026-09-15).

### A key goes where the keyboard is, not where the log says

**Type at a dialog only after its keyboard receipt.** A dialog's owner announces it *before*
asking for the window — `nxedit: choosing a file in …`, `nxfiles: showing properties` — because
that is the only order that cannot race the shell's placement line to the console (PR #267). So
that line says nothing about where the next key goes: the window is created, held for the manager,
placed, and only then handed the keyboard, and a key that reaches the compositor before that goes
to the parent window. Every dialog therefore prints a second line when the keyboard first reaches
it — `nxedit: the chooser has the keyboard`, `nxedit: the question has the keyboard`,
`nxfiles: properties has the keyboard`, `nxfiles: the question has the keyboard` — from
`libui::window::Child::took_keyboard`, and a gate that types waits for it. `check-login` once
typed a Save As backspace into the document by missing this.

A click needs no such receipt: it is routed by position, and the shell's `placed dialog` line
is printed only after the compositor has placed the window.

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

## The gates, one by one

What each `cargo xtask` gate boots, drives and asserts, and why; the root `CLAUDE.md` lists them a
line each.

### What each gate alone catches, and what it costs

The gates CI runs, each with what no other gate catches and its duration on a CI runner, measured
2026-10-16. **CI's QEMU job is five runners**, a group each, every runner building the images its
own gates need; a group's first gate pays that build, about 85 s, which the second figure leaves
out. Each gate prints its time (`xtask: <gate> took N s`), so the groups are rebalanced from the
log. A gate whose second column is empty is merged or retired — the maintainer's call each time.

| Gate | Runs in | Seconds | What it alone catches |
|---|---|---|---|
| `check-images` | `images` | 147 | A test or live image differing from the release one in anything new: a build-mode feature changing a program's bytes, or the live root not the release root. |
| `check-shutdown` | `images` | 76 | `with power shutdown`'s sequence, the root left clean on the host, and the clock set and kept across a reboot. |
| `test-qemu` | `harness` | 125 / 33 | The self-test boot adjudicated whole by `isa-debug-exit`; USB enumeration and hot-plug, a disk written past 2 TiB, the I/O counters, AHCI claimed over MSI. |
| `check-terminal` | `harness` | 54 | The compositor-to-shell round trip — a click, keys to `nxsh`, the answer rendered — `OSC 7` back to `nxterm`, and the PS/2 locks and lights read from QEMU's trace. |
| `check-fbcon` | `harness` | 35 | A machine with no serial port: the boot and a panic read off the screen with the kernel's own glyphs. |
| `test-interactive` | `release` | 120 / 32 | The release image's serial login and shell, its 37 steps through the real `login()` and `tty` layer. |
| `check-login` | `release` | 61 | The graphical greeter on PS/2, then the desktop driven step by step: menus, windows, the overview, Files, the editor, the clipboard, dialogs. |
| `check-login --usb` | `release` | 70 | The greeter and that session on a machine with no i8042, every key and click USB's. |
| `check-logout` | `release-live` | 140 / 56 | Ending a session: log out past the editor's question, restart and shut down, in three boots. |
| `check-live` | `release-live` | 23 | The live stick: its RAM-disk root, the greeter within 1.5 s of the mount, the boot medium flagged and passed over. |
| `check-report` | `release-live` | 23 | The live menu's hardware report with no serial port, its pages turned on PS/2: AHCI declined with no disk, no COM1. |
| `check-report --usb` | `release-live` | 30 | The same pages turned on a USB keyboard, before `init`. |
| `check-storage` | `live-test` | 137 / 63 | Disks the host reads afterwards: an unmount's writes, sticks plugged and pulled while mounted, FAT, partitioning and formatting. |
| `check-media` | `live-test` | 46 | Removable media on the desktop: Files' Drives updated with nothing typed, Save As onto a stick, Files' eject. |
| `check-display` | `display.yml` | 120 | The guest's screen against the host's render — base address, stride, channels — and the overview's round trip. |
| `check-input` | `input.yml` | 104 | A PS/2 key and click reaching a client window through the compositor's router. |
| `check-input --no-ps2-irq` | `input.yml` | 29 | Input with the tick's `ps2::poll` sweep as its only path. |
| `check-input --usb` | `input.yml` | 33 | USB-only input, and devices that come and go: a keyboard unplugged, another typed on, a held key released. |

**Two pairs are the nearest to overlapping**, and are kept for what the second column says:
`check-report` and `check-report --usb` read the same pages and differ in the keyboard turning them;
`check-login --usb` and `check-input --usb` both put USB input on the desktop, one through the
greeter and a session, the other through hot-plug.

`cargo xtask test-qemu` boots the self-test build (`test-harness` feature)
headless and adjudicates the whole boot (kernel → init → mount → userspace demos)
from QEMU's exit code: the guest writes a verdict to the `isa-debug-exit` device
(init on success, the kernel panic handler on failure), a hang is caught by a
wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`. Since Phase 6 Part A it
boots with an **xHCI controller and five USB devices** — a keyboard at high speed, a mouse at full
speed, a stick at SuperSpeed, a hub, and a smart-card reader nothing matches — asserts on the host
what the controller and the hub thread's first round report, that the round ended before `init`,
that `boot-probe` finds each device's `UsbDevice` record under the controller (Part A.3), that
the keyboard and mouse are bound and served after the i8042's two and handed to `input-server`
(Part B.2),
and **over QMP plugs a keyboard in, swaps it for a mouse on the same port while the machine is
paused, and pulls that out**; every gate's controller is configured as the
laptop's, with MSI and no MSI-X — `nec-usb-xhci`, since CI's QEMU 8.2 cannot give `qemu-xhci` MSI
(`docs/architecture/usb.md`). Since Part D **the stick is an MBR with one FAT16 partition**: it is
bound as a disk under its `UsbDevice`, its table read, the partition reported FAT and not mounted —
since Part E **with the reason, its 512-byte clusters**, which `fs-server-fat` refuses — and the
SATA disk the image boots from is the one record flagged `boot`. Since Part G **a sixth device, a
3 TiB sparse drive**, last so the others keep their ports: read with the sixteen-byte commands,
published with its size, and written past 2 TiB by `boot-probe` — the host finds the sector in the
image, where a block number cut to 32 bits would have put it 2 TiB lower; and `boot-probe`'s
`Reread`s, refused for a mounted device and the boot disk, and taken for the stick, whose partition
the kernel's rescan replaces with the watch pinged though no mount changed. Since Part H.2
`boot-probe` holds the **I/O counters** to a write: a file synced on the scratch disk moves its
written bytes and the page cache's write-backs in `/dev/devices/io.tsm` by at least the file's —
and, since Part H.4, by no more than 40 writes, its data a run of pages an IRP and its zeroes
64 KiB a write, with no read a page: what a grow added fills as zero without one, and the server
reads its metadata once. A file the build put there reads back as written in at most eight reads,
each fill reading up to 31 pages ahead.

`cargo xtask test-interactive` is the serial column's gate on the **release image**. It types at
the real prompt over the serial console and matches on what comes back — 37 steps,
expect-driven rather than sleep-driven. Since Phase 6 Part H.2 one of them is `nxsh`'s `time`: its
line, of its shape, before the value it passes through.

**Why it exists, in the past tense since 2026-08-21.** `session-mgr` used to auto-log-in and run
a fixed script under `test-harness`, so the `login:` prompt, the typed password, the real shell
prompt and the whole `tty_*` layer were `#[cfg(not(feature = "test-harness"))]` code that CI
compiled and never ran. Retrofit Part B deleted that: `session-mgr` has **one** `login()` in
every build and zero test cfgs, and its login proof lives here as steps 5a–5c.

**Prefer this shape for anything user-facing**: a service should behave in a test image the way
it behaves in a release one. `docs/planning/test-path-retrofit.md` is the plan that made that
true — `session-mgr` went from 31 build-mode `cfg` sites to zero and `init` from 41 to zero — and
it is complete. The last was `init`'s `/subtreetest` binding, which needed a **bind-mount concept
in `init.toml`** and got one in Phase 5 Part C.1: a `[[bind]]` in the test image's manifest
(`docs/spec/init-toml-schema.md`). `init` takes no cargo feature in any mode, so a test image and
a release image carry the same `init`.

`cargo xtask check-images` is what keeps the property: it fails if a test image and a release image
start differing in anything new — in their initramfs, and since administration Part E.1c in their
roots, where the service declarations and the profile manifest now are. It holds the **live image**
to the same rule: its initramfs may differ from the release one only in `etc/init.toml`, and the
filesystem inside its `root.img` must be the release root partition's, file for file, as must the
one inside the install entry's `install-root.img`, the pristine root the installer copies. The
**test live image** `check-storage` boots is held the same way to a `--selftest` image.

`cargo xtask check-terminal` is the **compositor-to-shell round trip** — a click that raises
`nxterm`, keys travelling to `nxsh` and echoing back into the grid, and the shell's answer
rendered there. It runs unconditionally in CI's QEMU job (promoted 2026-08-18); `check-input`
stops at the test client's event log and `check-display` never types, so nothing else covers it.
Since the desktop refresh's Part K it also holds the **shell-to-terminal** direction: `nxsh`
writes its working directory as `OSC 7`, `libterm` reads it and `nxterm` shows it beside the
window's name — three crates whose host tests cannot see each other, so a boot is what makes them
agree on the bytes.
Since Phase 6 Part B.5 it holds the **locks and the keyboard's lights**: it types under Caps Lock
and on the keypad with Num Lock on and off, and reads what the PS/2 keyboard's lights were set to
from QEMU's own trace of them (`-trace ps2_set_ledstate`), exactly, step by step — the one place a
gate learns a device's state from the emulator rather than from the guest.

`cargo xtask check-login` is the **graphical login gate**, on the release image as
`test-interactive` and `check-logout` are. It drives the greeter with the PS/2 injection
`check-input` and `check-terminal` use — a wrong password, then a right one, then a session — and it
and `check-logout` are the gates where the display arm exists for a person rather than for a test:
every other display gate boots `--selftest`. It runs unconditionally in CI's QEMU job, and so does
**`check-login --usb`**, the same login on a machine with no i8042, typed and clicked on a USB
keyboard and mouse (Phase 6 Part B). Landed with
M7 Part D, deliberately *before* the shell it will eventually show, so Parts E and F land against a
gate that exists.

**It must boot the release image**, not the test one. In a `--selftest` boot the greeter is
bottom-most — `service-mgr` brings the login chain up after the servers and before every other
declared service, which is what keeps `check-display`'s reference windows undisturbed — so it holds
no keyboard and nothing typed reaches it.

`cargo xtask check-logout` is the **ending a session gate** (administration Parts F.2 and F.3), on
the release image for `check-login`'s reason, and in CI's QEMU job. From the desktop's power menu,
in one QEMU run of **three boots**: a **Log out** that asks every window to close and waits on the
editor's "discard?" question, with the shell's waiting dialog naming it; **Cancel**, and the
session goes on; Log out again and discard, and **the greeter comes back**; a logout with nothing
to ask, which ends at once; a **restart typed at a terminal**, which asks the windows too and has
the shell gone inside its supervisor's bound; the menu's **Restart**, the editor ended anyway and
the view broker asked for `shutdown --reboot`; and its **Shut down**, with "It is now safe to turn
off your computer." read off the screen with `check-fbcon`'s decoder. It boots with a reset allowed
to reboot, unlike every other release boot. It aims at the power button and the dialogs' buttons
from `chrome`, and chooses the menu's rows by the keyboard.

`cargo xtask check-fbcon` is the **no-serial-port gate** (Phase 5 Part B): the laptop Phase 5
targets has no COM1, so the kernel draws everything COM1 receives on the screen until a client is
handed `/dev/framebuffer`, and again when the machine stops (`kernel/src/fbcon/`,
`docs/architecture/framebuffer-console.md`). The gate boots with `-serial none` and **reads the
screen back into text** with the kernel's own glyph and layout code, compiled into `xtask` by
path — so it asserts on lines, not pixels, and learns nothing from serial. It boots the release
userspace over a kernel built with `fbcon-gate`, a gate-only feature that **holds the screen still
for a second** after the timer and at the handout — a sampled scrolling console is otherwise a
flake, since a line may be up for milliseconds — and **panics on F10**, the only way to stop a
working machine after the desktop is up (QEMU's `inject-nmi` does not reach this kernel). It runs
in CI's QEMU job.

`cargo xtask check-live` boots the **live image** (`tools/build-cache/nitrox-live.img`, Phase 5
Part C) — the release kernel and initramfs with the release root filesystem riding along as a
second Limine module, which the kernel publishes as a RAM disk — attached as a **USB stick** with
the AHCI controller empty, so no storage driver is involved: the laptop's first boot, before there
is a USB storage driver. It asserts the module became a disk named `nitrox-live`, that `init` mounted and
read through it, that the greeter came up within 1.5 s of the mount (a RAM disk completing on the
timer tick instead of its own interrupt takes 3 s or more), that a serial login writes under
`/home`, and that the session reaches no disk — none does on any entry since administration Part
G.3. Since Phase 6 Part D the stick is **a disk too**, flagged as the one the machine started from
by the GUID Limine loaded the modules from, and the storage service passes it over. It runs in CI's
QEMU job.

`cargo xtask check-install` is the **installer gate** (Phase 5 Parts H.1–H.2), on demand like
`check-resolutions`: two boots, and a 512 MiB disk image. **It is a reinstall** (administration Part
G.3): the disk is a copy of the release disk, grown to 512 MiB, so it holds an install. The first
boot boots the live image's third menu entry with that disk attached, and drives the path a person
takes on the laptop — Limine's menu, the **graphical** greeter, a terminal from the Applications
menu, and `with admin nxinstall` typed at the shell in it. Nothing reads the terminal's grid (a
release image deliberately does not narrate it), so what it asserts on **in the guest** is the
kernel log: the ESP module that entry alone loads with the pristine root beside it, which the
storage service leaves unmounted and the installer copies from; the disk's older install
auto-mounted read-only; a session that holds no disk, and a view the `disks` grant filled; the
target refused as in use, the stick refused as holding the running system (Phase 6 Part D),
`with admin disk --unmount nitrox-root` freeing it, and the installer
asking before it writes — a `no` that writes nothing, then a `yes`; its questions for the new
machine's first account answered (Part G.2); and the milestones a destructive operation records.
It also aims the installer at the pristine root, a RAM disk, named correctly, and asserts nothing
was installed to it.

**Then it carves the root partition off the written disk and checks it on the host**, which is where
H.2's claims live — a boot proves the filesystem works and says nothing about its size, and H.1's
install booted perfectly with 24 MiB on a 477 MiB partition. Three claims: `e2fsck -fn` finds it
clean, its superblock's block count is the *partition's*, and **a write lands past block group 0**,
done with the allocator the guest runs because a size assertion passes just as well with allocation
confined to the first group. The file goes to a copy, so the disk that boots is the one the
installer made. **And the account** (Part G.2): `/system/users` holds the new account alone, the
policy makes it the one administrator, and `/home` holds its home alone, with its three folders. The
second boot is that disk alone, with no stick: its greeter refuses the live stick's `alice` and logs
the new account in, and neither boot's transcript holds either password.

`cargo xtask check-recovery` is the **recovery gate** (administration Part D.5), on demand like
`check-install`: two boots and a copy of the release disk. It is the one account path nobody
exercises until they need it, when there is no administrator to ask. The first boot is the live
image beside that copy, logged in on serial as the live image's own `alice`: `with admin disk`
remounts the disk's root writable, and `account --password alice --users
/storage/nitrox-root/system/users` sets a new password in **that disk's** file — `libusers` on the
file, no view and no service — before an unmount leaves it clean. The host then carves the
partition out: `e2fsck -fn` clean, and `/system/users` changed in `alice`'s line alone, under a
fresh salt. The second boot is the disk alone: the old password is refused at the login and the
new one taken. Neither boot may print either password.

`cargo xtask check-storage` is the **storage gate** (administration Part C.8), and the one whose
verdict is a disk. It boots the **test live image** (`image --live --selftest`: the live stick with
the test packages on its root) as a USB stick beside **a copy of the release disk** on the AHCI
controller — the laptop with Nitrox installed and a stick in it, and the one topology with a second
disk the host can read afterwards. On serial, the disk's `nitrox-root` is auto-mounted read-only
and refuses a write — as `disk --eject` of it is refused, it not being removable — `with admin disk`
remounts it writable, and `test-pattern` writes a pattern through a mapping and **exits without a
sync**. The host reads the disk mid-run and finds the file without the pattern, so what it finds
after `with admin disk --unmount`, the unmount put there. With the machine stopped, the host carves
the partition out: `e2fsck -fn` clean, the superblock's `s_state` clean, and the file holding the
pattern, read with `debugfs` rather than the library that wrote it. **Since Phase 6 Part D it plugs
sticks in over QMP** after that: the boot stick passed over; an MBR stick with two ext4 partitions,
each auto-mounted **writable** (Part F) and written without a sync, an eject of the first refused
while `test-pattern --eject-held` holds a file on the second, both left mounted, then both ejected
by one `disk --eject` with no password, and pulled; a
whole-disk ext4 stick **pulled while mounted**, whose teardown's I/O must all come back at once —
the kernel letting the dirty file go, the server unable to record the filesystem clean, the label
gone and the shell still answering; and that stick again, at a new index. The host then carves the
first's partitions out by its MBR and requires each clean with its pattern, and finds the second —
copied as the pull left it — still marked in use. **Since Part E it holds `fs-server-fat`**: the
disk copy carries a third partition, an internal FAT reported not removable and left unmounted,
beside an ESP refused for its clusters; and a 300 MiB FAT32 stick whose data region is off a 4 KiB
boundary — asserted before the boot — is auto-mounted writable, its host names (Unicode among them)
listed, written to (a directory, a copy to a long Unicode name, a rename, a removal, a file through
a mapping), ejected with `disk --eject` and pulled. On the host `fsck.fat -n` finds it clean and
mtools reads what the guest wrote. **Since Part G it formats**, through `with admin`: `disk
--list`'s `note` naming the internal FAT's reason, a partition of the boot stick refused before any
write, then a blank 2 GiB stick formatted ext4 whole (a GPT, its partition published by the kernel's
rescan, mounted, written, ejected and copied), partitioned again (an MBR, the GPT's partition
departed, a new one holding nothing) and that partition formatted FAT (mounted, written, the same
command refused while mounted, ejected). The host reads the GPT and its ext4 in the copy, and the
MBR, no GPT header at either end, and the FAT in the stick. **It logs in only once `boot-probe` has
exited**, whatever its verdict, which on that machine is a FAIL by the machine's shape: its later
tests install a policy and fill the view broker's clients, and the sticks made the gate long enough
to meet them. It runs in CI's QEMU job.

`cargo xtask check-media` is the **removable-media gate** (Phase 6 Part F), on the desktop a person
uses: the **release** live image as the boot stick beside a copy of the release disk, logged in at
the graphical greeter. Files lists the internal `nitrox-root` in its sidebar's Drives with no eject
button; a FAT stick plugged in over QMP is auto-mounted writable and **its row appears on the screen
with nothing typed after the plug** — read off screendumps, since any input would wake Files whether
the storage service's watch had or not; the editor saves onto it through Save As, Up to `/` aimed
where `libui` lays the chooser out for the staged theme; **Files' eject button ejects it** with no
password and its row goes; and on the host `fsck.fat -n` finds it clean and `mtype` reads back what
was typed. It runs in CI's QEMU job.

`cargo xtask check-shutdown` is the **shutdown gate** (administration Part E.4d), and the second
whose verdict is a disk. It boots a copy of a `--selftest` disk image and, on serial once
`boot-probe`'s verdict is in, logs in as `alice`: `test-pattern` writes a pattern under `/home`
through a mapping and exits without a sync, the host finds the file mid-run without it, and
`with power shutdown` — no password, the seeded policy's `power` view for everyone — takes the
machine down. It asserts the sequence on COM1 (the sessions ended when asked rather than after their
bound, every service stopped but the test image's known clients, `init` left `/` clean) and **reads
"It is now safe to turn off your computer." off the screen** with `check-fbcon`'s decoder, since
the laptop has no serial port. With the machine stopped the host carves the root out: `e2fsck -fn`
clean, `s_state` clean, and the file holding the pattern. Then a fresh copy sets the clock to 2031
with `with admin date --set` and runs `with power shutdown --reboot`: the second boot must anchor
its clock to the time set, read back from the RTC the set wrote (administration Part E.5), and
mount its root clean. It runs in CI's QEMU job.
**Waiting for `boot-probe` is load-bearing**: `boot-probe` takes and lets go of a power endpoint to
test its refusals, and the broker holds another for the boot.

`cargo xtask check-report` is the **hardware report gate** (Phase 5 Part D). Every boot logs what it
found — the bootloader handoff, the CPU, every ACPI table and MADT entry, each PCI function's
capabilities and what its driver did with it — and the live image's boot menu has a second entry,
`Nitrox — hardware report`, whose `cmdline: hwreport` makes the kernel hold that log on the screen a
page at a time before `init`, turning a page per key press (`kernel/src/report.rs`). The gate boots
the live image as `check-live` does but with `-serial none`, finds Limine's menu with a colour
detector of its own, chooses the report, reads every page with the console's decoder, and asserts
the facts that machine has: an AHCI controller **declined** for having no disk, the module disk,
and `console: no UART at COM1`. `test-qemu` asserts the other half on its own boot — the controller
**claimed** over MSI, COM1 present — so between them both outcomes and both kinds of COM1 are
gated. It runs in CI's QEMU job, and so does **`check-report --usb`**, on a machine with no i8042
whose pages turn on a USB keyboard (Phase 6 Part B). **`check-input --usb`** is the input gate's
on that machine, in `input.yml` beside `--no-ps2-irq`. Since Phase 6 Part C it is also the gate for
**devices that come and go**: it unplugs the boot keyboard, types on another plugged in, and holds a
key down on that one as it is unplugged — the window must see the release, which QEMU never sends.
It does so only once `boot-probe`'s registry test has reported, since an unplug in the middle of
that test fails it (PR #363's gate run).

**Every gate that boots a screen boots 1360×768** (Phase 5 Part E) — the laptop's 1366×768 as near
as QEMU can show it, since its VGA shears any width that is not a multiple of 8 — while `test-qemu`
and `test-interactive` keep QEMU's 1280×800, so every CI run boots two sizes. `--size WxH` boots a
screen gate at another size. The clients take the size from `/dev/draw/screen` and the gates from
`DisplaySize`; neither writes one down. See `docs/conventions/qemu-integration-tests.md`.
`cargo xtask check-resolutions` runs `check-display`, `check-terminal`, `check-login` and
`check-fbcon` at 1024×768, 1280×800, 1360×768, 1920×1080 and 2560×1440 and prints a table — **by
hand, from time to time, not in CI**: twenty boots is what confirming resolution independence
costs, and no PR should pay it.

`cargo xtask shot` is the other half of that: it **photographs** rather than renders, booting the
release image and driving it to nine moments — the greeter, the bare desktop, the Applications
menu, two real windows, then the terminal, the pointer resting on its close button, the file
browser and the editor each in the state the design draws it in, and the overview — then writing what QEMU says is on the display to
`tools/build-cache/shot-*.png`. It costs a boot, and it is the only way to see the things
`preview` cannot: the cursor, the window frames, the ground between windows, and how two windows
sit next to each other. A tool rather than a gate — it asserts only enough to know the picture is
of a working desktop.

`cargo xtask tune` is the same idea aimed at what `preview` structurally cannot show, and it
exists because judging a *composited* effect used to cost a boot each time. It writes
`tune-shadow.png` and `tune-overview.png` in about a second, taking `--radius`, `--strength`,
`--drop`, `--ground` and `--side` and defaulting each to the value that ships — so a bare run is a
picture of the current screen. **The overview half composites over the real screendump** when a
`cargo xtask shot` has left one in `build-cache`, which is what makes an opacity judgement
trustworthy: the question is how much of the actual desktop should show through. The shadow half
draws a mock scene instead, because the screendump already has the shipped shadow baked into it
and layering a second one would be comparing two shadows and calling the sum an answer. It is a
tool for *relative* judgements between candidates; confirm the pick with `shot`.

`cargo xtask preview` writes `tools/build-cache/preview-{ui,term}.png` — the same renders
`check-display` compares the guest against, drawn here and made viewable — and `preview-ui-dark.png`,
the toolkit in the dark scheme, which no gate compares against because no guest draws it. **It exists so that a
judgement about how something looks costs a glance rather than a boot** (M11 Part A), which is
what makes a polish loop affordable at all. It shows the *toolkit's* surfaces only: anything the
compositor draws — the cursor, the drag outline, the background between windows — and the
arrangement of real windows are composed in the guest and still need one.

`cargo xtask check-display` is the display arm's **smoke gate**, not a per-commit one:
it boots an image and compares the guest's screen against a `libdraw` render over QMP
`screendump`. It catches what a self-hash structurally cannot — a wrong base address,
a wrong stride, or swapped channels — because the guest stays consistent with itself.
CI runs it automatically via `.github/workflows/display.yml`, path-filtered to the
files that could break it.

## Running QEMU by hand

`cargo xtask qemu`, for a person at the window rather than a gate.

**Use `--grab` whenever you are going to touch the mouse or press a chord.** The guest has a
**relative** pointing device and no absolute one — a PS/2 mouse reports movement, and there is no
USB or virtio input driver for a tablet device to talk to — so nothing ever tells it where the
host's pointer is. Ungrabbed, the guest's cursor and yours are two independent cursors whose
offset is permanent and *cannot* be corrected by pushing into a corner: your pointer leaves the
window, and stops producing motion, before the guest's cursor reaches the edge. Every chord is
`Super`-something, and GNOME, KDE and COSMIC all bind `Super` at the host compositor, so those
keystrokes are your desktop's rather than the guest's. `--grab` confines the pointer and takes
the keyboard (Ctrl-Alt-G releases it); on a Wayland session it also runs the window through
XWayland, because a grab is an X operation. The chords are `Super+A`
(the Applications menu, and a second one closes it), `Super+H`, `Super+1..4`,
`Super+Shift+1..4`, `Super+R`. `Super` alone is unbound and **reserved for a full launcher**,
should one be built.

When a chord seems dead, the debug console says which half is at fault: the compositor logs
`Super down` / `Super up` per transition (the modifier only — never the key beside it, which at a
password prompt would be the password). No line means the keystroke never left your desktop.

## Adding coverage

Today: add an assertion to the existing self-test payload — a kernel check in
`kernel/src/boot_selftest.rs`, or a userspace check in the `test-harness` crate (`boot-probe`'s
gates, or the demo chain `check_demo_chain` adjudicates). A check that fails should `panic!`
(kernel) or fail the verdict `boot-probe` fires (userspace); either fails the run. (This said
"init's `selftest` block" until 2026-09-14; init has had none since the retrofit.) Keep additions deterministic and free of
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
