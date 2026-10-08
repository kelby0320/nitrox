# CLAUDE.md

Project-level instructions for Claude Code working on Nitrox.

## What this project is

Nitrox is a hobby operating system written in Rust. Successor to Latte (kelby0320/latte, an earlier Unix-like OS in C). Targets x86_64 UEFI primarily; aarch64 designed in via the architecture abstraction layer but not yet implemented. The system architecture rejects POSIX, Unix signals, ambient authority, and synchronous syscalls; it preserves Unix's composable pipelines, everything-as-a-resource philosophy, and powerful shell environment, on a foundation of capability-based access control plus per-process namespaces.

For the full architecture: read `docs/architecture/overview.md` first. For specific decisions and their rationale: `docs/rationale/`. For exact contracts (ABIs, formats): `docs/spec/`.

## Core architectural rules

These shape every decision; deviation requires explicit discussion:

- **Capability-based, not identity-based.** Authority is held in handles, not derived from a UID/GID. There is no "user identity" at the kernel level.
- **Per-process namespaces.** Different processes see different namespace contents. Sandboxing is by namespace construction, not by permission denial.
- **Async-first syscalls.** Every potentially-blocking operation returns a `PendingOperation` handle. The thread blocks on `sys_wait`, never inside another syscall.
- **No signals.** Async events are delivered via the notification queue. See `docs/rationale/why-no-signals.md`.
- **Resource servers don't self-register.** A supervisor (init, service-mgr, session-mgr) holding `BIND_NAMESPACE` does the registration. See `docs/rationale/why-supervisor-registration.md`.
- **Filesystems are userspace processes.** No filesystem code in the kernel.

## Language and toolchain rules

- **Rust throughout.** Kernel, userspace services, and runtime libraries.
- **No nightly language or library features.** No `#![feature(...)]` anywhere in
  `kernel/` or `userspace/` — enforced by `cargo xtask check-nightly` in CI. The
  `Handle<T, M>` design uses typestate markers rather than const-generic bitflags
  specifically to honour this.
- **Toolchain: stable, with one narrow exception.** The kernel and tools build on
  **stable** against the built-in `x86_64-unknown-none`. **Userspace** pins a nightly
  (`userspace/rust-toolchain.toml`) for one reason: it targets
  `x86_64-unknown-nitrox`, a custom spec, because hardware floating point needs a
  hard-float ABI and stable rustc ships no freestanding x86_64 target that has one. A
  custom spec has no precompiled sysroot, so `core`/`alloc` are rebuilt with
  `-Z build-std`, which is nightly-only. The pin is exact, not floating. This buys a
  *target*, not a licence — see `docs/decision-log.md` (2026-07-21
  floating-point).
- **Assembly is emitted from Rust**, not NASM: `core::arch::asm!`, `global_asm!`, and `#[unsafe(naked)]` + `naked_asm!` (all stable since Rust 1.88). The exception entry stubs, the GDT/TSS load, the user-memory copy routines, and the thread context switch are all in-tree Rust asm. There is no assembler in the build — `build.rs` only passes the linker script. (Earlier drafts reserved NASM for the entry stub and context switch; both turned out cleaner as Rust-emitted asm — see `docs/decision-log.md` 2026-05-13 and 2026-05-29 — so NASM is not used. Re-evaluate only if a routine genuinely cannot be expressed via `asm!`/`naked_asm!`.)
- **Cargo + cargo xtask** for builds. The `xtask` workspace provides higher-level commands (`xtask qemu`, `xtask image`, etc.).
- **Limine** as the bootloader.

## Build commands

Standard development loop:

```
cargo xtask build          # build the kernel ELF
cargo xtask image          # build + assemble the UEFI-bootable disk image
cargo xtask qemu           # build, assemble the image, and launch under QEMU
cargo xtask qemu --grab    # …with the window holding the pointer and keyboard
cargo xtask qemu --selftest # …with the boot self-tests / demos compiled in
cargo xtask qemu-debug     # launch QEMU with the GDB stub enabled
cargo xtask test           # host-side unit tests
cargo xtask test-qemu      # boot a headless self-test image; pass/fail via isa-debug-exit
cargo xtask test-interactive # boot the RELEASE image and drive a real login + shell
cargo xtask preview        # render the toolkit on the host to a PNG — no boot, no QEMU
cargo xtask tune           # …and the compositing preview: shadows and the overview's opacity
cargo xtask shot           # boot the release image and photograph the whole desktop
cargo xtask check-display  # boot + screendump; compare the screen to a libdraw render
cargo xtask check-terminal # click into nxterm, type, and check the shell's answer renders
cargo xtask check-input    # inject a key + a click over QMP; check they reach a window
cargo xtask check-input --usb # …on a machine with no i8042: a USB keyboard and mouse only
cargo xtask check-images   # test vs release initramfs and root: differ only on a short allow-list
cargo xtask check-login    # boot the RELEASE image and drive the graphical greeter to a session
cargo xtask check-login --usb # …typed and clicked on USB, with no i8042
cargo xtask check-logout   # the power menu: log out past the editor's question; restart; shut down
cargo xtask check-fbcon    # boot with NO serial port; read the boot and a panic off the screen
cargo xtask image --live   # the live image: release root as a RAM-disk module, for a USB stick
cargo xtask check-live     # boot the live image as a USB stick with no disk; mount, greeter, a write
cargo xtask check-report   # choose the live menu's hardware report, no serial port; read its pages
cargo xtask check-report --usb # …its pages turned on a USB keyboard, with no i8042
cargo xtask check-install  # install to a blank disk from the live menu, then boot that disk
cargo xtask check-recovery # reset a password on an installed disk from the live image, then boot it
cargo xtask image --live --selftest # the test live image: the live stick with the test packages
cargo xtask check-storage  # that stick beside a copy of the release disk; the host checks the disk
cargo xtask check-media    # the live desktop: a stick plugged in, shown in Files, saved to, ejected
cargo xtask check-shutdown # `with power shutdown` on a test disk; the host checks it; then a reboot
cargo xtask check-resolutions # four display gates at five screen sizes — on demand, not in CI
```

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
the kernel's rescan replaces with the watch pinged though no mount changed.

`cargo xtask test-interactive` is the serial column's gate on the **release image**. It types at
the real prompt over the serial console and matches on what comes back — 36 steps,
expect-driven rather than sleep-driven.

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

Don't run kernel code on the host. Don't run `cargo build` directly in the kernel workspace without the custom target — it will fail.

## Review workflow

Every PR gets reviewed by a **separate, fresh Claude Code session** — `/pr-review <N>`,
defined in `.claude/skills/pr-review/SKILL.md`. The point of the split is that the
reviewer does not inherit the author's context, so it cannot inherit the author's blind
spots. The reviewer reports and does not edit; findings come back to the working session.

If you are the working session, do not run it on your own work — you are the author.

## Repository layout

```
kernel/         no_std kernel; custom target x86_64-unknown-none
userspace/      userspace services and libraries; std target
tools/          host-native build utilities (xtask, image builder)
docs/           project documentation (see structure below)
```

Documentation structure under `docs/`:

```
docs/
  architecture/    subsystems that EXIST — what they do and how they relate
  design/          subsystems DESIGNED BUT NOT BUILT (today: the display arm above M1)
  spec/            exact contracts (ABIs, wire formats, schemas, the shell language)
  reference/       catalogues (today: error codes only — see deferred-decisions.md)
  rationale/       why decisions were made (read here when puzzled)
  conventions/     how to write code in this project
  planning/        phase and subproject plans, with checkboxes
  archive/         superseded artifacts, kept for the record (the v5.1 design doc)
  decision-log.md  the running record of decisions and their reasoning
```

**Which of these describe the system as it is today**, and which do not — this matters more
than it looks, because reading the wrong class as current is how you end up confidently
wrong about how something works:

- **`spec/`, `reference/`, `architecture/`, `conventions/` describe current behaviour.**
  If one disagrees with the source, the source wins and the doc is a bug — fix it in the
  same change.
- **`rationale/` explains why**, and is largely timeless.
- **`design/`, `planning/` and `archive/` do not describe current behaviour.** `design/`
  is what a subsystem *will* be. Today it holds `fault-survival.md`
  (added 2026-08-19), which is not a display document at all — it is where the kernel's
  fault-survival intent is written down — `graphical-prompt.md` (added 2026-09-29), the password
  prompt the view broker will open on the display once something needs one, and `nitrox-shell/`,
  the designed-but-not-built appearance of the desktop, which `docs/planning/desktop-refresh.md`
  adopts. What is built has moved out —
  `input-subsystem.md` and `widget-toolkit.md` graduated on 2026-08-12, `desktop-shell.md` and
  `graphical-session.md` on 2026-08-25 with Milestone 7, `ui-composition-model.md` on
  2026-08-26 with Milestone 8, and `display-substrate.md` on 2026-08-30 (owed by Milestone 9 and
  paid a milestone late) — so the rule to apply is simply "`design/` means not built".
  **Two docs graduated while still outrunning their code** — `desktop-shell.md` (its tray is
  v2) and `ui-composition-model.md` (its ports are unscheduled) — and each says in its Status
  line which sections are behaviour and which are intent. That is the pattern to copy when a
  document is mostly true, rather than one to spread.
  `planning/` is what is intended, with checkboxes for what is done. `archive/` is superseded.
  **Never conclude "the system does X" from any of them.**
- **`decision-log.md` is a dated record and is append-only.** Entries are true as of their
  date; correcting one to match today's code destroys the evidence. Append a new entry.

**A `design/` doc graduates to `architecture/` when the code lands** — the milestone that
builds it should carry that move as a checkbox, because the doc is otherwise the thing
nobody remembers to update.

**Filenames carry no version number.** `foo-design-v1.2.md` guarantees link rot on every
revision — renaming it breaks every inbound reference, which is a self-inflicted source of
exactly the drift these rules exist to prevent. The version belongs in the doc's Status
line; git carries the history. (`archive/os-design-v5.1.md` is the exception: there the
version is the artifact's identity and the file is frozen.)

Every doc under `architecture/` carries a **Status** line naming what is actually built and
when it was last checked. Trust it over the body's tense, and correct it when you find it
wrong.

`cargo xtask check-docs` (in CI) enforces the mechanical part: every relative doc link
resolves, every backticked `kernel/…`/`userspace/…`/`tools/…` path cited by a
current-behaviour doc exists, and every `architecture/` doc has a Status line. It cannot
tell whether prose is *true* — that part is on review. A deliberate reference to a path
that does not exist (an honest forward reference, or a record of a deletion) is exempted by
marking the line `<!-- check-docs: allow-missing -->`.

When uncertain why something is the way it is, check `docs/rationale/rejected-approaches.md` first — many "obvious" alternatives were considered and rejected for specific reasons.

## Subdirectory rules

Per-subdirectory `CLAUDE.md` files exist for the major workspaces. Read the relevant one before significant work:

- `tools/CLAUDE.md` — host tooling: which rules do *not* reach it, and the bar for a
  host dependency
- `kernel/CLAUDE.md` — `#![no_std]`, no external crates, unsafe policy
- `userspace/CLAUDE.md` — crate layering, async-first
- `userspace/libkern/CLAUDE.md` — `#![no_std]` + no alloc; raw syscall surface
- `userspace/init/CLAUDE.md` — critical-path code, special constraints

When working in a subdirectory, Claude Code lazily loads the subdirectory's `CLAUDE.md`. Trust those files over this one for subdirectory-specific guidance.

## Cross-cutting conventions

- **Markdown for documentation.** No Sphinx, no MkDocs. Plain `.md` files with Mermaid for diagrams where helpful. Cross-link via relative paths.
- **TOML for configuration.** `init.toml`, service declarations, profile manifests. No YAML, no JSON5.
- **All public items have doc comments.** Use `cargo doc` for code-level reference.
- **`#[repr(C)]` for any type crossing the kernel/userspace boundary.** Layout must be predictable.
- **Document `unsafe` blocks.** Every `unsafe` block needs a `// SAFETY:` comment explaining why the operation is sound.

## Forbidden patterns

Things that should not appear in code, period:

- External crates in the kernel (one planned exception: ACPICA in Phase 2; not yet active)
- Nightly Rust features
- `unsafe` blocks without `SAFETY` comments
- Sync syscalls that block (the `read()`/`write()` Unix-style pattern)
- Code that assumes a UID/GID model
- Direct `panic!()` in init or eshell — these are critical-path
- Adding "for now" code without a TODO and a tracking entry
- Referencing architecture internals (`arch::x86_64::*`, future
  `arch::aarch64::*`) from kernel code outside `kernel/src/arch/` — go
  through the neutral `crate::arch` interface. Enforced by a private arch
  submodule and `cargo xtask check-arch`. See
  `docs/conventions/arch-boundary.md`.

If you find yourself writing one of these, stop and ask.

## When to update which doc

- **Implementation produces new conventions** → `docs/conventions/`
- **Implementation reveals a subtlety in an architecture doc** → update the architecture doc; the docs are living
- **A new design decision is made** → append to `docs/decision-log.md` with date and reasoning
- **A deferred item is being implemented** → update `docs/rationale/deferred-decisions.md`
- **A spec contract changes** → update the spec doc; bump version markers as needed

## Status

The project is pre-v0.1. The syscall ABI, wire formats, and kernel internals are pre-stabilization. The `docs/spec/` documents are the canonical contracts within this pre-stabilization period; if a spec doc and the source disagree, the source wins and the spec is updated to match (filed against the decision log).

Phases 0–5 (foundation, kernel substrate, boot-to-userspace, service ecosystem, a usable windowed desktop, bare metal) are **complete** (Phase 5 closed 2026-09-17). The target laptop boots Nitrox from its own internal disk, installed by `nxinstall` from a live USB stick, to a greeter and a terminal — so "it has only ever run under an emulator" is no longer true of anything below the display arm. **Two things come before Phase 6** (2026-09-17): the **desktop refresh**
(`docs/planning/desktop-refresh.md`), adopting a polished design, and then **administration**
(`docs/planning/administration.md`) — elevation and the tools an installed system needs. The
refresh is first because the admin tools are UI surfaces. **The refresh is complete as of
2026-09-22** — all eleven parts, A–K — and **administration as of 2026-09-30**, all seven parts,
A–G, the last being an installer that runs in an ordinary session as the view broker's client.
Phases are **not renumbered**: the numbers appear throughout an append-only decision log. Then **Phase 6 — USB**; 7–9 are the
portable runtime, networking, and the browser. See `docs/decision-log.md` for the current implementation phase and `docs/planning/implementation-plan.md` for the slice-by-slice breakdown.
