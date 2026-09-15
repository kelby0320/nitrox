# Nitrox Implementation Plan — Phase 5 — Bare metal

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. Phases 0–4 are
complete; Phase 5 is active.

---

## Phase 5: Bare metal

**Goal:** Nitrox boots and runs on a real machine, installed on its own disk.

**Why this comes before the portable runtime, networking and the browser.** Everything built
through Phase 4 has only ever executed under QEMU. That is not a small asterisk: an emulator
is a *model* of a machine, and every model omits something. The omissions we already know
about are written down in [`deferred-decisions.md`](../rationale/deferred-decisions.md). **Two
name "real hardware" as their trigger** — shared INTx, and framebuffer cache attributes. The
third, `_PRT` interrupt routing, is the one this phase found *misfiled*: it sits under MSI/MSI-X,
whose stated trigger is "NVMe, multi-queue NICs, or performance work on interrupt-heavy
devices", and Part A is the argument that it is a correctness unblocker instead. The ones we do
not know about are the reason to go.

The ordering argument is about **debuggability**, not enthusiasm. A `std::thread` bug and an
interrupt-routing bug look identical from userspace. Every phase built on an untested
foundation inherits that ambiguity, and the cost of resolving it grows with the amount of code
standing on top. Going now means the foundation is checked while it is still small enough to
check.

### Definition of Done

**The desktop runs on the target laptop, booted from its own internal disk, with a keyboard, a
compositor, and a terminal running `nxsh`.** Specifically:

- The machine boots from a Nitrox-written GPT disk with no other operating system present.
- The root filesystem is ext4 on the internal SATA disk, reached through AHCI.
- The greeter accepts a login from the built-in keyboard, and a `nxterm` runs a real shell.
- Kernel diagnostics are visible on the machine's own screen, with no serial cable.

Explicitly **not** in scope: a pointer (there is none without USB or I²C-HID — see Phase 6),
networking, sound, suspend, or any power management.

---

## The target machine

Recorded here because a bring-up plan without the machine's actual details is a wish list. All
of this was read off the laptop running Debian on 2026-09-10.

**Acer Aspire A315-51**, BIOS V1.06, Core i5-7200U (Kaby Lake-U, 2 cores / 4 threads), 6 GB
DDR4, Seagate ST1000LM035 1 TB SATA (5400 rpm, 512e/4 KiB physical).

| Facet | What the machine reports | What Nitrox has |
|---|---|---|
| Firmware | UEFI, **Secure Boot disabled**, platform in Setup Mode | Limine `BOOTX64.EFI` (unsigned) — boots |
| PCIe config | `MCFG` present; ECAM at `0xe0000000`, bus 00–ff | ECAM enumeration — works |
| Local APIC | `x2apic enabled` | x2APIC-only, enabled from CPUID — works |
| Framebuffer | GOP linear at `0xa0000000`, **1366×768×32, pitch 5504** | `Geometry::with_pitch` — already honours a padded pitch |
| Storage | Sunrise Point-LP SATA in **AHCI mode** at `00:17.0` | AHCI Tier 1 driver — the right driver |
| Keyboard | `AT Translated Set 2 keyboard` on `platform/i8042/serio0` | i8042 driver — works |
| Pointer | `ELAN0501` **I²C-HID** on the Designware controller at `00:15.0` | **nothing** — Phase 6 |
| USB | Sunrise Point-LP xHCI at `00:14.0` | **nothing** — Phase 6 |
| Ethernet | Realtek RTL8111/8168 | **nothing** — Phase 8 |
| Serial port | **none** | `kprint` writes to COM1 — Part B |

**Two of those rows changed the plan**, and both are worth stating as findings rather than as
facts:

**The pitch has padding.** 1366 × 4 = 5464, and the firmware reports 5504. A framebuffer
whose stride is not `width × bpp` is the commonest way a first bare-metal boot produces a
sheared picture. Nitrox is already correct here — `libdraw::acquire` uses the reported pitch
and refuses one too small to hold a row — but this will be the first time that code has
mattered, so it belongs on the list of things to *verify* rather than assume.

**Linux drives this machine's AHCI over MSI**, not INTx: `/proc/interrupts` shows
`IR-PCI-MSI-0000:00:17.0` for `ahci` and `IR-PCI-MSI-0000:00:14.0` for `xhci_hcd`. Only the
i8042 is on the IOAPIC. See Part A.

---

## Part A — MSI, because INTx may not be routable at all ✅

- [x] **MSI for PCI devices**, replacing the IOAPIC path for AHCI.

> **Landed 2026-09-11.** AHCI acquires its interrupt over MSI on both target machines' shape of
> controller, with INTx kept as the fallback for a function advertising no capability. The
> boxes below are ticked and **the prose is past tense to match** — everything from here to
> Part B describes what was built, not what was intended. `cargo xtask test-qemu` adjudicates
> which path was taken (`check_ahci_msi_path`), and the control for that gate is in
> § "The gate".

**This was the part most likely to decide whether the disk works.** The AHCI driver used to
take its interrupt line straight from the PCI Interrupt Line register
(`kernel/src/drivers/ahci.rs`, with the comment "`_PRT` routing is deferred") and use it as a
GSI. QEMU's firmware programs that register to match the IOAPIC, so it worked there. Real UEFI
frequently leaves it meaningless, because the authoritative routing lives in the ACPI DSDT's
`_PRT` — **which requires an AML interpreter**, i.e. vendoring ACPICA.

**MSI is the way out, and it is far smaller than the alternative.** An MSI-capable device is
told a vector and a LAPIC address and writes it itself; no `_PRT`, no AML, no IOAPIC entry.
Every PCIe function on Sunrise Point-LP supports it, and Linux uses it on this exact machine.
That reclassifies the existing MSI deferral — filed under "performance work on interrupt-heavy
devices" — as **the real-hardware unblocker**.

Two deferrals moved with it, though not the two this section first predicted. The MSI half of
the MSI/MSI-X entry closed and **MSI-X kept an entry of its own** (see § "Scope"); the
`ArchIrqInstall` entry closed outright. **Shared INTx chaining** (trigger: "real hardware where
INTx lines are shared across functions") is not closed but is further from mattering: it now
covers only a function with no MSI capability at all.

- [x] The device-interrupt install family gains its **second member**, which is the trigger the
      existing deferral names for promoting `install_pci_irq` into an `ArchIrqInstall` trait.
      Build the abstraction now, at the second consumer, not before.
- [x] Keep the IOAPIC path: the i8042 is on GSI 1 on this machine and is not going anywhere
      (`interrupts.txt`: `1: … IR-IO-APIC 1-edge i8042`).

### What the emulator and the machine actually report

**Detail pass, 2026-09-11.** A throwaway capability-list walk was added to `pci::probe_function`,
run under `cargo xtask test-qemu`, read, and reverted. The laptop's side comes from `lspci -vv`
in `~/Documents/Acer Laptop HW Info/`. Part A's claim to be testable before the machine is
plugged in rests on this rather than on a datasheet.

| Function | MSI capability | MSI-X |
|---|---|---|
| QEMU q35 `00:1f.2` ICH9 AHCI `8086:2922` | at `0x80`, **64-bit**, 1 vector, non-maskable | none (SATA cap at `0xa8`) |
| Laptop `00:17.0` Sunrise Point-LP AHCI `8086:9d03` | at `0x80`, **32-bit**, 1 vector, non-maskable | none (SATA cap at `0xa8`) |
| Laptop `00:14.0` Sunrise Point-LP xHCI `8086:9d2f` | at `0x80`, 64-bit, **8 vectors**, non-maskable | none |
| QEMU `00:02.0` e1000e `8086:10d3` | at `0xd0`, 64-bit | at `0xa0`, 5 vectors, BIR 3 |

QEMU's AHCI HBA reports `CAP 0xc0141f05` — `S64A` set, 32 command slots, 6 ports.

**The gate exists**: QEMU's AHCI offers exactly the capability Part A programs. But the table
also contains the one divergence that matters, and it is not the one the plan expected.

### The divergence QEMU structurally cannot catch

**The laptop's AHCI is 32-bit MSI; QEMU's is 64-bit.** That is not a detail of degree — it
changes the capability structure's *layout*. Message Control bit 7 selects between two:

| Offset | 64-bit form (QEMU's AHCI) | 32-bit form (the laptop's AHCI) |
|---|---|---|
| `+0x02` | Message Control | Message Control |
| `+0x04` | Message Address | Message Address |
| `+0x08` | Message Upper Address | **Message Data** |
| `+0x0C` | **Message Data** | *(end of the capability)* |

**Neither controller has Mask or Pending Bits**: those exist only when Message Control bit 8
(per-vector masking) is set, and both report `Maskable-`. So the laptop's structure is ten bytes
— `0x80` id/next, `0x82` control, `0x84` address, `0x88` data — and stops.

Now trace a driver shaped by QEMU against that. Its `+0x08` write puts the *upper address* —
zero, since `0xFEE0_0000` fits in 32 bits — straight into **Message Data**. Its `+0x0C` write
lands at config offset `0x8C`, reserved space between the MSI capability at `[80]` and the SATA
capability at `[a8]`, where it does nothing. The device is left signalling vector 0, which is not
a device vector at all, so the disk's completion interrupt never arrives. The boot hangs on the
first read, on the machine, with every gate green.

- [x] **The form is selected by reading Message Control bit 7**, and both branches exist from the
      first commit — not a 64-bit driver with a 32-bit fixup added after the laptop refuses to
      boot.
- [x] **The 32-bit branch is the one under host test.** `pci/mod.rs`'s existing `FakeCfg` can
      model a 32-bit, non-maskable capability, so the laptop's shape is covered by `cargo xtask
      test` even though no QEMU boot can exercise it. Model it **without** Mask Bits, as the real
      devices are: a fake that carries them turns the misdirected write into a modelled register
      instead of into nothing, which is a tamer failure than the target's. This is the inversion
      worth keeping — the emulator tests the branch the target does *not* take, so the host test
      is not a nicety here.

### Scope: MSI, not MSI-X

The plan's first draft said "and MSI-X where a device offers it". **Drop that.** MSI is a
config-space capability and nothing more; MSI-X is a vector table and a pending-bit array living
in a device BAR, which means mapping a BAR, an allocation policy for the table, and per-vector
masking rules — a distinct mechanism wearing a similar name.

The evidence is that it has **no consumer in either phase that would use it**: neither the
laptop's AHCI nor its xHCI advertises MSI-X, and the xHCI's eight vectors are plain MSI. Those
two functions are the ones Part A and [Phase 6](phase-6-usb.md) depend on, and they are also the
only two the laptop capture covers — the Realtek NIC and the Designware I²C controller were
never capability-walked, so this says nothing about them. In the QEMU machine, where every
function *was* walked, the only MSI-X device is the e1000e, which has no driver until
[Phase 8](phase-8-networking.md). So MSI-X here would be built at its *zeroth* consumer.

### The pieces, in dependency order

- [x] **Config-space access that outlives enumeration.** It did not exist:
      `kernel/src/pci/mod.rs` reserves *one* vmap page and repoints it per function inside
      `enumerate()`, so when that returned, no driver could read its own config space. MSI is
      entirely config-space writes, which made this the first deliverable rather than a detail
      of a later one. `ResourceDescriptor` already carried `seg`/`bus`/`dev`/`func`, so the
      address was re-derivable. **Decided: a page per claimed device** (`pci::Config`), not one
      shared window behind a lock. A shared window would be a mutual-exclusion problem the
      enumeration-time code never had, because it ran before the scheduler existed, and it would
      buy back one page per Tier 1 driver — the wrong trade at this count. The mapping is
      permanent, because vmap never reclaims VA.
- [x] **A capability-list walk** in the neutral PCI module: Status bit 4, the pointer at `0x34`,
      the `id`/`next` chain, with a cycle bound. Capability *layout* is PCI-SIG and identical on
      every architecture, so this is neutral kernel code by the same argument that puts ECAM
      enumeration there.
- [x] **The arch seam, chosen before the code was written.** The MSI message *layout* is
      x86-specific: the address encodes a LAPIC destination (`0xFEE0_0000 | dest << 12`) and the
      data encodes a vector and delivery mode. The capability *write* is not. Two shapes are
      available and they are not equivalent — (1) `arch` exposes `msi_message(vector, cpu) ->
      (addr, data)` and neutral PCI code writes the capability, or (2)
      `ArchIrqInstall::install_msi(cfg, handler)` takes a config-space accessor and does the
      whole thing inside `arch/`. **Took (1):** it keeps PCI knowledge out of `arch/`,
      mirroring the split already made between `IrqRouter` (routing) and `Platform` (firmware
      facts), and it leaves the capability walk host-testable against `FakeCfg`, which is where
      the 32-bit form above gets its coverage. `cargo xtask check-arch` enforces whichever is
      picked, so picking after the code is written is the expensive order.
- [x] **The trait promotion.** `install_pci_irq` was a neutral free function carrying a
      `TODO(msi)` that named exactly this moment. The family gained its second member, so it
      became `ArchIrqInstall` with the INTx and MSI installs side by side; both the function
      and the marker are gone.
- [x] **The driver switch, with INTx kept as the fallback.** AHCI prefers MSI when the capability
      is present and falls back to the interrupt-line GSI when it is not. Keeping both is not a
      test accommodation: it is what a device with no MSI capability needs, and the i8042 stays
      on the IOAPIC regardless.

### Two things the plan did not account for

**Nothing in Nitrox enables PCI bus mastering.** Enumeration clears and restores the I/O and
memory decode bits to size BARs and never touches bit 2; the AHCI driver never reads the command
register at all. A positive-controlled sweep of `kernel/` finds two mentions of bus mastering: a
doc comment in `kernel/src/mm/dma.rs`, and the *test fixture* in `kernel/src/pci/mod.rs`, which
presets `0x0007` and labels it "bus-master enabled". The probe explains why everything works
anyway — **the firmware hands every function over with BME already set**, host bridge included.

That is the class of assumption this phase exists to find: something the emulator's firmware did
for us that we have therefore never done for ourselves. It is the same config-space plumbing MSI
needs, so it costs almost nothing here; met at Part F instead, it presents as "the disk does
nothing" with the entire boot as the suspect list.

- [x] The driver sets bus-master enable explicitly rather than inheriting it.

**Enabling MSI means disabling INTx.** The Interrupt Disable bit (command bit 10) is clear on
every function at handoff. A device left able to deliver both, with the IOAPIC entry we routed
still live, is a spurious-interrupt source that would present as a driver bug.

- [x] Setting MSI Enable also sets Interrupt Disable; taking the INTx fallback leaves it clear.

### The gate, and the control that makes it mean something

**`test-qemu` passing on the MSI path proves nothing on its own** — the identical transcript is
produced by a driver that silently fell back to INTx, which is this part's most likely real
failure. So the adjudication is on *which path was taken*, not on the boot succeeding:

- [x] The driver says which path it took — `ahci: irq via MSI (vec 0x31, addr 0xfee00000, data
      0x0031, 64-bit cap at 0x80)` or `ahci: irq via INTx (GSI 10, vec 0x31)` — and
      `check_ahci_msi_path` in `tools/xtask/src/main.rs` reads that line from `test-qemu`'s
      transcript. PR #293 spent a branch on self-tests nobody read; this one landed with its
      reader in the same commit.
- [x] **Match the path token (`INTx` versus `MSI`), never the vector**, which is not stable
      across builds: the selftest image's `IrqRouter::self_test` registers `pit_tick` and never
      releases the slot, so AHCI lands at `0x31` there and at `0x30` in a release image. All
      eight captured transcripts in `tools/build-cache/` split on exactly that line. A matcher
      written against the literal passes `test-qemu` — Part A's named gate — and fails the first
      time it is pointed at `check-login` or `test-interactive`. Part A's own switch moves the
      number again, which is the section's thesis restated: the claim under test is the path, not
      the vector.
- [x] A **forced-INTx probe** still boots, since that path stays as the fallback.
- [x] **The negative control to run before calling it done:** make the capability walk fail to
      find MSI. The boot must still pass *and* the adjudicated line must change. A gate that
      reports the same thing either way is the defect, not the test.
- [x] The **`CAP.S64A` assert** belongs in this change (flagged in PR #293). The HBA reports
      `S64A` under QEMU and the decision log treats 64-bit DMA as a considered call, but the
      kernel reads `CAP` and only prints it — a violated assumption is silent corruption rather
      than a refusal. Same shape as the rest of Part A: read a capability register and act on it.

### Assumed, and worth confirming at Part D

Linux drives this machine through **interrupt remapping** (`IR-PCI-MSI`), which is Linux's own
doing: it enables VT-d and writes remappable-format MSI addresses. Nitrox will write
compatibility-format addresses straight to `0xFEE0_0000`, which is correct **provided nothing
has enabled remapping before us**. Firmware normally has not. Part D's hardware report is where
that stops being an assumption.

**The compatibility format carries eight bits of destination id** (address bits 19:12), while
`Irq::id()` returns the full 32-bit x2APIC id. So `msi_message(vector, cpu)` narrows, and it
should say so at the seam rather than have it discovered. The bound is not new: `hw_apic_id`
already reads the 8-bit initial xAPIC id and documents "sufficient while `MAX_CPUS <= 255`". The
two assumptions are the same one and they fail together, which is the argument for stating it
once where the truncation happens. Harmless on a 4-thread i5-7200U and under QEMU.

### Left alone

The device-vector pool is eight vectors (`DEVICE_IRQ_BASE = 0x30`, `DEVICE_IRQ_COUNT = 8` in
`kernel/src/arch/x86_64/idt.rs`), and `register_device_handler` panics when it runs out. One MSI
vector for AHCI fits with room to spare. The laptop's xHCI advertises eight, so Phase 6 will want
more — and the comment there already says the fix is to add stubs.

## Part B — the machine can tell you why it failed ✅

- [x] **An early framebuffer console**: `kprint` renders to the screen as well as to COM1.

**Without this the first boot is undiagnosable.** `kprint` writes to the serial port and tees
into an in-memory ring (`klog.rs`) that a userspace reader can map. The laptop **has no serial
port**, and the ring is only reachable once userspace is running — so a boot that dies before
`init` produces a black screen and nothing else. Every gate we own drives the guest over
COM1; none of that survives contact with this machine.

The pieces exist: `kernel/src/font.rs` and `kernel/src/framebuffer.rs` already draw text. What <!-- check-docs: allow-missing -->
is needed is a scrolling character console over the Limine framebuffer, live from the moment
the framebuffer request is answered — i.e. before ACPI, PCI, or anything that can fail.

- [x] It must survive the compositor taking the framebuffer over. Simplest rule: the kernel
      console owns the screen until userspace first commits a frame, and a later panic takes it
      back.
- [x] Gate it under QEMU with `-serial none`, so the assertion is "the boot is legible with no
      serial port at all" rather than "the code compiles".

### What shipped, and where it differs from the above (2026-09-14)

`kernel/src/fbcon/`, described in [`framebuffer-console.md`](../architecture/framebuffer-console.md);
the boot banner it replaced is gone.

- **The pieces did not exist.** The font had uppercase letters, digits and four punctuation
  marks, and every kernel message is lowercase. The console draws Terminus Font, embedded as the
  PSF1 file console-setup ships and read in place (SIL OFL, licence beside it), picked from a
  rendered comparison of four candidates.
- **It mirrors COM1, not just `kprint`.** `sys_kprint` is teed too: a laptop whose `init` fails
  to mount a root says so through userspace's syscall, and a kernel-only console would stop at
  the hand-off to `init`.
- **The hand-over is the first `/dev/framebuffer` handout, not the first committed frame**, which
  the kernel cannot see. The yield happens under the console's lock before the handle exists, so
  no kernel paint can race a client's first frame. The take-back is in `stop_the_machine`, after
  the stop NMIs, so it covers a fatal fault as well as a panic.
- **The gate is `cargo xtask check-fbcon`**, and it reads the screen back into *text* with the
  kernel's own glyph and layout code, off frames a gate-only `fbcon-gate` kernel feature holds
  still — sampling a scrolling console was a flake (PR #296 review). The same feature panics on
  F10 for the third claim, since QEMU's `inject-nmi` arrives through LINT1 and does nothing to this
  kernel under TCG or KVM. Every claim was failed on purpose first.

## Part C — the live image ✅

- [x] **A third image mode whose root filesystem arrives with the kernel**, so the first boot on
      real hardware needs no storage driver and no partitioning.

> **Landed 2026-09-14**, in the four pieces below and against the detail pass unchanged in design.
> `cargo xtask image --live` writes `tools/build-cache/nitrox-live.img`; `cargo xtask check-live`
> boots it as a USB stick with no disk. The notes under C.2 and C.4 record where the measurements
> and the controls came out differently from what the pass predicted.

**Why it exists.** Booting from a USB stick does not give us a root filesystem on that stick.
Limine reads the kernel and its modules through **UEFI Boot Services** — the firmware's own USB
stack — and then exits boot services before entering the kernel. From that moment we have no USB
driver, so an ext4 partition on the stick is unreachable. A live image sidesteps storage
entirely: everything the release root holds travels as a Limine module, and 6 GB of RAM makes
that a non-issue.

**It is a diagnostic instrument, not a product.** Its whole value is that it removes storage from
the equation for the first attempt: if the screen stays black, "did AHCI work?" is not one of the
variables. Once Part F has booted the machine, Part H installs to the disk and the live mode has
done its job.

> **Detail pass, 2026-09-14.** Written before any code, like Part A's, and it changed the design:
> the first draft below assumed the in-kernel `/initramfs` server could stand in for the root, and
> it cannot. The sections from here to Part D are the corrected plan. Decisions marked
> *(maintainer's call)* were put to the maintainer with the alternatives.

### The first draft's root does not boot

The first draft made `/bin` "a subtree bind of the in-kernel `/initramfs` endpoint" and put the
rest of the root in a fatter initramfs. **That server answers one question — "give me this file"
— with a copied `MemoryObject`** (`initramfs_server`, `kernel/src/object/kernel_server.rs`). It
has no directory listing, no file metadata beyond a mapping's size, and no writes. What the
release root is used for needs all three, starting on the critical path:

- **`profile-server` lists `/store/<package>/bin/`** to build the `/bin` index (`build_index`,
  `userspace/profile-server/src/main.rs`). `init` treats a failed `/bin` as critical-path, so a
  root without directories drops to `eshell` before a single service starts.
- **`libfs::list_dir`** — `nxfiles`, `nxsh`'s `list` and completion, the file chooser — opens a
  directory by resolving it to a **session endpoint the serving process mints**
  (`librsproto::session::Dir::open`) and speaks the directory protocol over that. The initramfs
  server returns a `MemoryObject` or `NotFound`; it has no session to mint.
- **Anything that writes** — a saved file, a theme change — has nowhere to go.

Building those into a new server would be a read-only cpio filesystem that only the live image
runs: exactly the "code for one mode" the discipline below rules out, with a second
implementation of the filesystem protocol to keep in step with the first.

### The root is the release root, in RAM *(maintainer's call)*

**The live image carries the release root filesystem as an ext4 disk image, loaded by Limine as a
second module; the kernel exposes that module as a RAM-backed block device; and the same
`fs-server-ext4` mounts `/` from it.** Every program, every path and every protocol is the one the
release image runs. What differs is three pieces of data:

| | Release image | Live image |
|---|---|---|
| `boot/limine/limine.conf` | one `module_path` (the initramfs) | a second: `boot():/boot/root.img` |
| `etc/init.toml` (in the initramfs) | `device = "gpt-partlabel:nitrox-root"` | `device = "gpt-partlabel:nitrox-live"` |
| where the root partition is | the disk's second partition | inside `root.img`, a GPT image with one partition |

**A distinct label, not a reused one.** Labelling the live partition `nitrox-root` would make the
live initramfs byte-identical to the release one, but a live stick booted on a machine that has
Nitrox installed would then find two partitions answering to the same name. `nitrox-live` costs
one line of manifest and is never ambiguous; `check-images` proves that line is the whole
difference.

**Consequence for the limitations below:** `/home` is **writable**, in RAM. The first draft's "no
writable `/home`" was a property of the initramfs design, not of a live boot. What remains is no
persistence.

**Measured against the emulator before any code (2026-09-14).** The current release image,
attached to QEMU as a USB stick (`qemu-xhci` + `usb-storage`) with nothing on the AHCI controller:
OVMF boots Limine from the stick, Limine loads the kernel and the 234,648-byte initramfs through
the firmware, the kernel enumerates the xHCI controller (`class 0c.03.30`) and has no driver for
it, AHCI reports `no SATA disk on any implemented port`, and `init` fails
`/dev/disk/by-partlabel/nitrox-root not found` and drops to `eshell`. That is the laptop's live
boot today, reproduced — and it is the shape the gate below boots.

### The discipline: the live-ness is data, not code

**This is the constraint that makes a temporary stage safe** (maintainer's call, 2026-09-10):
*no production code may change for the live mode alone.* No `#[cfg(feature = "live")]` anywhere
in `kernel/` or `userspace/`. The live configuration is a different `limine.conf`, a one-line
different `init.toml`, and a root image — all build-tool concerns.

That is the same rule [`test-path-retrofit.md`](test-path-retrofit.md) established and proved:
*the software under test is the software that ships*. It took `session-mgr` from 31 build-mode
`cfg` sites to zero on exactly this argument, and `cargo xtask check-images` is what keeps it
true.

**What the rule permits** is general mechanism the live image happens to be the first to use.
The one kernel change below qualifies: a module after the first becoming a block device is what
an initrd has always been, and nothing in it names the live image.

### The pieces, in dependency order

**C.1 — a bind-mount concept in `init.toml`, and the last `cfg` leaves `init`** *(maintainer's
call: kept in Part C although the live image no longer needs it)*

- [x] `[[bind]]` entries: `path` (where to bind), `source` (the `mount_point` of a `[[mount]]` in
      the same manifest) and `subtree` (the base the lookups are scoped to). Processed after every
      mount, with the source mount's forwarding endpoint and `sys_ns_bind`'s existing base
      argument. A `source` naming no mount is a manifest error; a failed bind is critical-path, as
      a failed mount is — a missing bind is how a later test fails for a reason nobody can see.
- [x] **`init` retains every mount's endpoint until the binds are done.** Today it keeps only the
      root's (`FS_ENDPOINT`, handed on to `service-mgr`) and closes each other mount's the moment
      it is bound, so "the source mount's endpoint" exists only for `/`. The non-root endpoints
      are closed after the bind pass instead; the root's is still handed on. (Restricting `source`
      to `/` was the alternative, and would be a rule nothing needs.)
- [x] `/subtreetest` (subtree `/system`) and `/scratch` (subtree `/scratch`) become `[[bind]]`
      entries in the **test** image's `init.toml`, and the `#[cfg(feature = "selftest")]` block in
      `mount_one` is deleted.
- [x] **`init` is built with no features in any mode.** Deleting the last `cfg` is not enough on
      its own: `cmd_build` passes `--features test-harness` to `init`, and the retrofit's own note
      says most byte-identity is cargo not rebuilding a crate whose feature set did not change. So
      the features go from `userspace/init/Cargo.toml` and from the build, and "is this a test
      image" becomes a build-mode predicate rather than `mode.features().is_some()`.
- [x] `check-images`' allow-list loses `sbin/init` and gains `etc/init.toml`. The list is
      one-directional, so this prune is by hand.
- [x] `docs/spec/init-toml-schema.md` gains the table; the retrofit's box and the `init` line in
      `CLAUDE.md` close.

**Why first:** it touches `init`'s manifest, which C.3 also changes, and it is independently
verifiable — `test-qemu`'s `subtree_bind_test` and the demo harness's case 8 are what consume the
binding, so they are its gate.

**C.2 — a Limine module after the first is a RAM-backed block device**

- [x] `init_initramfs` keeps module 0 as the initramfs. Every further module is published as a
      block `DeviceNode` over the module's own memory — no copy — before `drivers::probe` runs its
      GPT pass, so the partition scan and the `/dev/disk/by-partlabel/` names come from the code
      AHCI disks already use. The kernel logs the module's path and size.
- [x] The backing generalises the existing bring-up `RamDisk` (`kernel/src/io/ramdisk.rs`), which
      today owns a 64 KiB pattern-filled `KVec` for the I/O spine self-test; that self-test keeps
      working unchanged.
- [x] **A completion must not wait for the tick** (PR #297 review, measured). Today's
      `ramdisk_submit` copies, then `dpc::enqueue`s the completion — and DPCs drain only at an
      interrupt tail. A RAM disk raises no interrupt, so every completion would wait for the next
      10 ms timer tick: the waiter parks (`block_on_po` for page-cache fills, `sys_wait` for the
      fs-server's metadata I/O), the CPU `hlt`s, and the tick wakes it. The bring-up disk never
      showed this because `io::self_test` drains synchronously. The review measured the same wait
      on AHCI, by deleting the device-tail drain: `init: mounted fs-server-ext4 at /` to
      `desktop-session-mgr: greeter presented` went from **0.10 s to 4.85 s** at `-smp 4` and to
      **8.43 s** at `-smp 1`, under KVM. That is the fs-server "I/O hang" of 2026-07-23 again, and
      on a laptop with no serial port it is a long silent stall right after the mount — the
      ambiguity the live image exists to remove.
- [x] **So the RAM disk raises its own completion interrupt.** It registers a device vector like
      any driver and, after the copy and the enqueue, sends that vector to its own CPU. The
      completion then runs where AHCI's does: `device_irq_dispatch` drains the DPC in a fresh
      interrupt lock scope and calls `resched_if_idle`, the scheduling point that fixed the I/O
      hang. **Completing inline in `submit` was the alternative, and is rejected:** the completion
      takes `SCHED` and `submit` runs in whatever lock context its caller holds, which the
      interrupt tail's fresh scope never has to reason about. A self-IPI needs a neutral way to
      raise a vector on the current CPU; `send_ipi` is arch-internal today.
- [x] **A concurrency model, stated.** The bring-up `RamDisk`'s `Sync` rests on "accessed only on
      the single CPU that services it", which a root disk breaks — the fs-server and page-cache
      fills submit from any CPU. AHCI serialises through its port lock and one in-flight slot. The
      RAM disk takes a lock around each transfer, so a read racing a write of the same block sees
      one or the other, never a torn block, which is what a real disk guarantees.
- [x] **The GPT pass names what it found.** `gpt::init` logs `N partition(s)` and each partition's
      index and LBAs, never its label, so no serial line can say `nitrox-live` was found. It logs
      the label too — the gate needs it here, and Part D's hardware report wants it on the
      laptop's own disk.
- [x] **Writable.** The module lives in `MEMMAP_KERNEL_AND_MODULES` memory, which the kernel never
      reclaims and reaches through the HHDM like the initramfs. To confirm in C.2 rather than
      assumed here: that the HHDM mapping of module memory is writable on both targets. If it is
      not, the fallback is to copy the module into buddy frames at boot, which costs its size in
      RAM once.
      **Confirmed under QEMU (TCG and KVM), 2026-09-14**: `check-live` writes a file under `/home`
      and reads it back off the module's memory. The laptop's firmware is Part F's to confirm.
- [x] Host tests for the backing's bounds and block arithmetic; the in-guest proof is C.4's gate.

> **Landed 2026-09-14.** Measured through C.4's gate with the self-interrupt deleted: mount to
> greeter 3,082–3,311 ms under KVM and 4,363 ms under TCG, against 39 ms and 180 ms with it —
> the review's stand-in, reproduced on the RAM disk itself. The self-interrupt needed two neutral
> operations, `ArchIrqInstall::install_software` and `ArchIrq::raise_on_self` (the x2APIC SELF IPI
> register). Limine reports a module's path without the `boot():` prefix, so the kernel logs
> `module 1 (/boot/root.img)`.

**C.3 — the live image, built by `cargo xtask image --live`**

- [x] A separate file (`tools/build-cache/nitrox-live.img`), so a live build never clobbers the
      image every other gate boots. One GPT partition — an ESP sized to its contents — holding
      Limine, the **release** kernel, the release initramfs with the live `init.toml`, the
      Terminus licence, and `boot/root.img`.
- [x] `root.img` is a GPT image with one partition, `nitrox-live`, holding an ext4 filesystem
      built from **the release root's staging tree** — the same function, not a copy of its steps.
      Sized to its contents plus slack for writes, and under a ceiling of its own with its own
      reason: it is what firmware reads off a USB stick before the kernel runs.
- [x] `INITRAMFS_MAX_BYTES` stays one number. The first draft made it per-mode because the live
      initramfs was going to carry the whole system; in this design it is the release initramfs
      with one manifest line changed, so the release ceiling still describes it.

**C.4 — the gates**

- [x] **`check-images` learns the third mode**: the live initramfs may differ from the release
      one in `etc/init.toml` and nothing else, and **the files inside the built `root.img` must be
      the files inside the release root partition** — names, kinds, sizes and contents, read back
      out of both ext4 images with `debugfs` (e2fsprogs, which already provides `mke2fs`). Compared
      at the output, not at the staging function (PR #297 review): C.3 has to extract that function
      from `assemble_image` anyway, and a check that compared its output for release against its
      output for live would miss a live build that called it and then wrote one file more.
- [x] **Its control is exactly that file**: have `image --live` add one to the staging tree before
      `mke2fs`, and the check must fail naming it.
- [x] **`cargo xtask check-live`** boots `nitrox-live.img` as a **USB stick**, with nothing on the
      AHCI controller, and asserts over serial:
  1. `ahci: no SATA disk on any implemented port` — no storage driver carried the boot;
  2. the module became a block device — its log line, with `boot():/boot/root.img` — and the GPT
     pass found a partition **labelled** `nitrox-live` on it (the label line C.2 adds);
  3. `init` mounted `/` from `gpt-partlabel:nitrox-live`, and the greeter was presented **within a
     time bound** from the mount — set in C.4 from measurements under both accelerators, and
     below what a tick-bound completion can reach (4.85 s under KVM in the review's stand-in);
  4. a login on the serial column can **write a file under `/home` and read it back** — the claim
     the first draft could not make.
- [x] **Its controls**, run before it is trusted, each aimed at the step it must fail:
  - drop the second `module_path` — **step 2** fails, since with AHCI empty there is then no block
    device for the GPT pass to scan (the first draft predicted step 3, which this never reaches);
  - keep the module but label its partition `nitrox-root` — **step 3** fails at the mount, which is
    the control that shows the mount assertion can fail at all;
  - delete the RAM disk's self-interrupt — **step 3's time bound** fails, which is the control for
    the completion path above;
  - attach the image on AHCI as well — **step 1** fails, since the gate would notice a boot that
    had a disk after all.
- [x] In CI's QEMU job, unconditionally: the image exists for Part F, and a live image that
      stopped booting in the meantime is the failure this gate is for.

> **Landed 2026-09-14, and the controls moved two assertions.**
>
> - **The `nitrox-root`-labelled module failed step 2, not step 3**, because step 2 asserts the label
>   and so sees it first. The control the pass wanted for step 3 is a partition that is found and
>   named but **holds no filesystem** — and that exposed something the gate had assumed: with the
>   partition zeroed, `init: mounted fs-server-ext4 at /` still printed. `fs-server-ext4` sends Ready
>   before it reads the superblock, and checks the ext4 magic only when a lookup arrives; the boot then
>   fails three steps later as `image not found: /bin/auth-service`. So step 3 also requires `init`'s
>   first read through the new root, `/system/current-generation`, and the zeroed partition fails
>   there. The server's behaviour is unchanged here — it predates Part C and is recorded in the
>   decision log.
> - **The time bound is 1.5 s, timed by when lines reach the host.** Timing `expect` returns read 0 ms
>   under KVM, because both lines arrive in one burst; the Session now stamps each chunk on arrival.
>   Good path 39 ms (KVM) and 180 ms (TCG); with the self-interrupt deleted, 3,082–3,311 ms (KVM) and
>   4,363 ms (TCG). A first guess of 3 s would have passed that control by 311 ms.
> - Every control failed its step: no module (step 2), the wrong label (step 2), no filesystem
>   (step 3), no completion interrupt (the bound), a disk on AHCI as well (step 1). `check-images`'
>   live half failed on a file staged only in the live build (`system/live-only: only in the live
>   root`) and on a live-only line in the service declarations.

### Accepted limitations of the live mode

Stated so they are not mistaken for bugs: **no persistence** — `/home` is writable, in RAM, and
forgotten at power-off — and a longer firmware stage, since `root.img` is read off the stick
before the kernel starts. Both disappear the moment root is on a disk.

### Left alone

- **The in-kernel `/initramfs` server.** Still the bootstrap path for `init`, `eshell` and the
  fs-server, and still file lookups only. Nothing here asks more of it.
- **A USB driver.** The whole point. Phase 6.
- **Choosing the root by anything but `init.toml`.** A kernel command line naming the root, or a
  scan that prefers whichever label it finds, would make the live-ness something the kernel
  decides; one line of manifest keeps it data.

## Part D — the hardware report ✅

- [x] **A boot mode that says what it found**: firmware handoff values, the ACPI tables located,
      the ECAM windows, every PCI function with class and BAR layout, the framebuffer geometry
      including pitch, CPU features, the MADT's CPUs and IOAPICs, and which drivers bound.

> **Landed 2026-09-14**, in the four pieces below and against the detail pass unchanged in design.
> Every boot logs the report; the live image's menu has `Nitrox — hardware report`;
> `cargo xtask check-report` chooses it with no serial port and reads the pages back, and
> `test-qemu` asserts the facts its own boot has. Where the result differed from the pass:
>
> - **The screen cost is about a tenth of a second under TCG**: `check-fbcon`'s first text to the
>   handout went from 1466 / 1373 ms to 1554 / 1520 / 1506 ms for about thirty new lines.
> - **The AHCI vector is not pinned.** The pass wrote `ahci, MSI vec 0x30`; the test image's is
>   `0x32` (its PIT self-test keeps a vector), so `test-qemu` asserts the claim and the path.
> - **The ACPI list is pinned by signature and OEM**: QEMU's FACP, APIC, HPET, MCFG and WAET as
>   `BOCHS BXPC`. The BGRT beside them is OVMF's boot logo, not QEMU's to promise, and the
>   addresses belong to whichever OVMF build runs.
> - **The logical CPU count is CPUID's, per package**; the MADT lines and `smp:` already count what
>   the boot launched.
> - **The MADT count leaves out a CPU listed twice** — a type-9 entry below id 255 beside usable
>   type-0 entries, as Linux does — and entries whose id names no CPU; their lines say why (PR #300
>   review).
> - **Limine draws the entry's em dash as a hyphen.** Legible, and the detector reads colours, not
>   text.
> - **The report fits QEMU's boot in two pages** at 160×49 rows; page 2 ends at the framebuffer
>   line, the last before the report.

Under QEMU this is a convenience. On a machine with no serial port and no debugger it is the
only way to learn anything from a boot that gets partway. It is deliberately Part D rather than
Part F's first task: **it must be built and read under QEMU first**, where we know what the
right answers look like, so that on the laptop the *differences* stand out.

> **Detail pass, 2026-09-14.** Written before any code, as Parts A and C were. Decisions marked
> *(maintainer's call)* were put to the maintainer with the alternatives.

### What a boot already says, and why that is not a report

More of the list is logged today than the one-line box suggests. Read off a release boot's serial
transcript:

| The box asks for | Logged today | Missing |
|---|---|---|
| Firmware handoff values | the initramfs size | the bootloader and its version, the firmware type, the HHDM offset, the memory map, the command line |
| ACPI tables located | a one-line summary (`RSDP rev 2 (XSDT); 1 IOAPIC, 5 src-override, 4 CPU; 1 ECAM region`) | the tables themselves — signature, OEM, revision, length, address |
| ECAM windows | ✓ `acpi: ECAM0 @0xe0000000 seg 0 bus 0-255` | — |
| Every PCI function, class and BARs | ✓ one line per function, one per BAR | each function's capabilities (MSI 32/64-bit, MSI-X) |
| Framebuffer geometry including pitch | ✓ `framebuffer: 1280x800 pitch 5120 bpp 32` | the padding, stated (`pitch − width × 4`) — the laptop's is 40 bytes |
| CPU features | two incidental lines (vector width, an invariant-TSC warning) | vendor, brand, family/model/stepping; the features the kernel requires and uses |
| The MADT's CPUs and IOAPICs | ✓ IOAPICs; a CPU **count** | each CPU entry (processor UID, APIC id, enabled / online-capable); the LAPIC NMI entries |
| Which drivers bound | AHCI's own lines; COM1's loopback self-test; the i8042 | a line per function saying which driver claimed it, or that none did |

**The facts are the smaller half of the problem.** Under QEMU the kernel's log to the start of
`init` is 2,195 bytes, about sixty lines; the laptop's screen holds 48 rows and scrolls a quarter at
a time as userspace starts printing, and the compositor takes the screen within a second. On the
machine this is for, nobody can read even the lines that exist.

### The shape *(maintainer's calls)*

**The facts go in the kernel log, on every boot; a report boot holds that log on the screen.**

- **Every boot logs the missing facts** (D.1). Then every CI transcript *is* a hardware report of
  the emulator, `test-qemu` can assert the answers we know, and the report boot has nothing of its
  own to gather. The rejected alternative is a separate renderer that re-queries every subsystem at
  report time — a second statement of each fact, and some (the COM1 loopback result, the MSI vector
  a driver got) exist only during bring-up.
- **The report is selected from the boot menu** (D.2, maintainer's call): a second Limine entry,
  `Nitrox — hardware report`, passes `cmdline: hwreport`. Selection is data in `limine.conf`, which
  is the discipline Part C kept. **Only the live image carries the menu** — `timeout: 5` and two
  entries — because it is the stick a person boots a machine with; the release and test images keep
  `timeout: 0` and one entry, so no gate but the live ones pays for a countdown.
- **A report boot stops before `init`, pages the kernel log on the framebuffer console, holds each
  page until a key, then boots on** (D.3, maintainer's call). The same boot then shows whether
  userspace comes up. The keys that page it are consumed and never reach userspace.

**Measured before any of this was written (2026-09-14).** A copy of the release image given a
two-entry `limine.conf` (`timeout: 10`, the second entry pointing at a kernel path that does not
exist), booted under OVMF with `-display none`: Limine 12.2.0 draws the menu with its countdown,
QMP `screendump` captures it, and injecting **Down** then **Enter** over QMP selects the second
entry — Limine panics on the missing path. So a gate can drive the real menu. The protocol's
command-line request is `LIMINE_EXECUTABLE_CMDLINE_REQUEST`
(`0x4b161536e598651e, 0xb390ad4a2f1f303a`, response `{ revision, cmdline }`) and the configuration
key is `cmdline:`, both read off Limine's upstream protocol specification (the limine-protocol
repository) and its v12.x configuration reference; `timeout: 0` boots without
drawing the menu, which is why the live image needs a nonzero one.

### The pieces, in dependency order

**D.1 — the missing facts, logged on every boot**

- [x] **The handoff**: the bootloader's name and version and the firmware type (Limine's
      bootloader-info and firmware-type requests), the base revision accepted, the HHDM offset, the
      date at boot, the executable command line, and the memory map summarised by type (entries,
      and bytes usable / reclaimable / reserved / ACPI / framebuffer).
- [x] **The CPU**: CPUID vendor, brand string, family/model/stepping, logical CPU count, and three
      groups of feature bits — what the kernel **requires** (x2APIC, RDTSCP, NX, SMEP/SMAP), what it
      **uses when present** (XSAVE/AVX, RDRAND/RDSEED), and what it **warns about** (invariant TSC).
      TSC-deadline is reported as present or absent and **not** as used: the LAPIC timer runs in
      count-down mode (`kernel/src/arch/x86_64/timer.rs`), and a line implying otherwise would
      mislead Part F. The hypervisor bit, since a transcript should say which it came from.
- [x] **Compact**: one line per table, per MADT entry, per PCI function — not one per field. Every
      line the console draws costs screen time before the compositor, and on the laptop, whose
      framebuffer has no write-combining until Part G, more than under QEMU. D.1 measures `check-fbcon`'s
      first-line-to-handout time before and after, under TCG.
- [x] **ACPI**: every table the XSDT lists — signature, OEM ID, OEM table ID, revision, length,
      physical address. The MADT's entries individually: each local APIC / x2APIC (processor UID,
      APIC id, enabled or online-capable), each IOAPIC, each source override, each LAPIC NMI entry.
      **Today's parser reads less than it looks** (`parse_madt`): it matches only type-0 local APIC
      entries with the *enabled* bit set, skips type-9 x2APIC entries and online-capable ones
      entirely, and the `cpu_apic_ids()` it fills has no caller. So on firmware that lists its CPUs
      as type-9 entries the existing summary would read `0 CPU` while these new lines show every
      entry; D.1 extends the parser to both types and both flags, and the summary counts from the
      same walk.
- [x] **PCI**: each function's capabilities as the walk Part A added finds them (MSI with its form,
      MSI-X, PCI Express), and — after `drivers::probe` — a line per function saying what became of
      it, in **three states** (PR #299 review):
  - **claimed**, and how — `ahci, MSI vec 0x30`;
  - **matched but declined**, and why — `ahci declined: no SATA disk on any implemented port`.
    `ahci::init` returns before it maps config space, enables bus mastering or takes MSI when no
    port answers, and `drivers::probe` ignores its result; recorded only on success, that function
    would read "no driver", exactly like the xHCI — and on the laptop an undetected disk would point
    away from AHCI port detection, which is the "the disk does nothing" case Part A exists for;
  - **none** — no driver in the table matched.

  The device table records none of this today; drivers report their outcome into it.
- [x] **The framebuffer's padding, as a number.** The laptop reports pitch 5504 for 1366 × 4 =
      5464; the line should say `padding 40` rather than leave the reader to subtract.
- [x] **COM1 is reported as present or absent**, not as a failing test. `console::init` prints
      `console: RX loopback self-test FAIL` for any failure today, which is what a machine with no
      UART produces. Detect the UART first — the 16550 scratch register holds what is written to it,
      and a floating bus reads back `0xFF` — and log `console: no UART at COM1` when there is none,
      keeping `FAIL` for a UART that exists and fails.
- [x] Parsers that can be host-tested are: the ACPI table list and MADT decoding (against captured
      bytes, as Part A tested MSI), the memory-map summary, and the command line.

**D.2 — the kernel command line, and the live image's menu**

- [x] The executable command-line request, read once at boot and parsed into flags; unknown words
      are logged and ignored, never fatal. `hwreport` is the only flag.
- [x] The live image's `limine.conf` gains the menu: `timeout: 5`, the default `Nitrox` entry, and
      `Nitrox — hardware report` with `cmdline: hwreport`. Generated by `image --live`, beside the
      second `module_path` it already adds — `limine.conf` is live-image data today and
      `check-images` does not compare it, so nothing there changes. `check-live` then waits out the
      five-second countdown and boots the default entry, which is also a check that the default is
      still the ordinary boot.

**D.3 — report mode**

- [x] **Where**: after `drivers::probe`, AP bring-up and `record_framebuffer` — so drivers have
      bound, every CPU has come online or failed to, and the framebuffer facts exist — and before
      `run_first_userspace`.
- [x] **What**: the kernel log so far, read back out of `klog` (which needs a read-into-a-buffer
      counterpart to `copy_into_frames`), paged to the console's rows: clear, draw a page, a prompt
      line (`— page 2/3 — any key —`). Drawn on the framebuffer console only; COM1 already has every
      line. With no framebuffer console there is nothing to hold, and the boot does not wait.
- [x] **The read walks `Klog::runs`** — prefix, elision notice, ring, in order — which is the one
      definition of the snapshot's layout. The laptop's longer log can spill past the 8 KiB prefix
      into the ring; nothing is lost before 16 KiB, and an elision notice on a page is worth seeing.
- [x] **A held page is not painted over.** The console paints every write at once while the kernel
      owns the screen, and a line at the bottom row jumps the grid a quarter — which would scroll a
      page's top rows, the handoff facts, off the photograph. Kernel lines do arrive here: in five of
      nine transcripts an `smp: cpu N online (AP)` line follows `smp: 4 CPU(s) online`, because an AP
      counts itself online before it prints (PR #299 review). So the pager holds the console in a
      state of its own, like `Owner::Userspace`: writes still reach the grid, COM1 and `klog`, and
      nothing is drawn until the report ends and the console repaints.
- [x] **Held until a key**: the i8042 driver counts key presses as it decodes them; the pager waits
      for the count to move. **The keys that paged the report are drained from the keyboard's ring
      before userspace starts.** That is defence in depth rather than a gated guarantee: from reading
      `input-server`, events it takes before any client subscribes go nowhere, so a control that
      skipped the drain would likely pass too, and the plan does not claim a gate for it. (A release
      of the last key can arrive after the drain; the greeter acts only on presses.)
- [x] **A timeout ends the report, not a page.** If a page waits 120 s with no key, the remaining
      pages are not held — a dead keyboard otherwise costs 120 s per page, minutes on the laptop's
      longer log, which reads as the hang the bound exists to prevent. **With no i8042 device
      answering** (`ps2::init` already knows) the report is not held at all, and says so on COM1.
- [x] **The bound is data**: `hwreport=<seconds>` overrides the 120 s, so a gate control can run the
      timeout path in seconds; a bare `hwreport` means the default.

**D.4 — the gates**

- [x] **`test-qemu` asserts the answers we know**, from the D.1 lines on serial, on its disk image
      with the disk on AHCI: ECAM `0xe0000000` bus 0–255; four MADT CPU entries with APIC ids 0–3;
      the IOAPIC at `0xfec00000`; the framebuffer at 1280×800, pitch 5120, padding 0; `00:1f.2`
      `8086:2922` **claimed** by `ahci` over MSI; COM1 present. The ACPI table list is pinned from
      the first run rather than guessed here.
- [x] **`cargo xtask check-report`** boots the **live image as a USB stick with the AHCI controller
      empty** — the only way the live image boots, and the laptop's shape — with **`-serial none`**.
      It finds the Limine menu, presses Down and Enter, reads each report page off the screen with
      Part B's decoder, and asserts the facts that boot has: the same ECAM, MADT, IOAPIC and
      framebuffer lines; `00:1f.2` **matched but declined** by `ahci` with `no SATA disk`; the module
      disk and its `nitrox-live` partition; and **`console: no UART at COM1`**, since `-serial none`
      is exactly that machine. It presses a key per page and asserts the boot reaches the handover.
      Between them the two gates cover a claimed and a declined function, and COM1 both ways.
- [x] **Finding the menu needs a detector of its own.** Part B's decoder reads only the kernel
      console's cells — its glyphs, its palette, its grid — and Limine draws its menu in its own font,
      grey, cyan and green on black, centred. The detector is calibrated on the menu screenshot this
      pass captured (the title's cyan, the highlighted entry's grey bar), with a control that a
      firmware screen and a kernel console screen never match. Pressing Down on a timer instead is
      the blind sampling `check-fbcon` already paid for once.
- [x] **Its controls**: boot the default entry instead — no report page appears and the boot does
      not hold; withhold the key with `hwreport=5` — the page stays up past the draw time and the
      report ends after five seconds, without holding the next page; drop one fact's line from the
      log — the page assertion names it.

### What to compare on the day

The laptop's answers, from the Debian capture of 2026-09-10, so Part F reads the report for
differences: ECAM `0xe0000000` bus 00–ff; x2APIC enabled; 2 cores / 4 threads; framebuffer
1366×768, pitch 5504, **padding 40**; AHCI at `00:17.0` claimed by `ahci` over **32-bit** MSI; xHCI
at `00:14.0`, the Designware I²C controller at `00:15.0` and the RTL8111 with **no driver**; **no
COM1**, which D.1 reports as `console: no UART at COM1` rather than as a failing self-test. An AHCI
line reading **matched but declined** is the one to stop at: the controller was found and no port
answered.

### Left alone

- **Names for PCI IDs.** A vendor/device database is a large table for a readability gain a phone
  search covers.
- **Keeping the report.** The photograph is the record; the live root is writable but forgotten at
  power-off.
- **ACPI beyond the static tables.** No AML, no `_PRT`: Part A made them unnecessary for the disk.
- **SMBIOS.** Useful, not on the list, and a table walk of its own.

## Part E — the resolution this machine actually has ✅

- [x] Boot QEMU at **the laptop's screen, as near as QEMU can show it**, and pass every display gate
      there, with every client and every gate taking the screen's size from the screen.

`check-display` compares the guest's screen against a `libdraw` render, and the whole display
arm has only ever run at the QEMU default. A padded stride and an odd width are exactly the
conditions under which a compositor's damage arithmetic goes wrong by a few pixels a row.
`libdraw`'s half is a host test; the guest half is a gate run at the laptop's geometry.

> **Landed 2026-09-15**, in the four pieces below and against the revised detail pass. Clients read
> `/dev/draw/screen`; the gates that boot a screen boot 1360×768 and aim from `DisplaySize`;
> `cargo xtask check-resolutions` runs four of them at five sizes. Where the result differed:
>
> - **A shell that cannot read the leaf stops**, rather than falling back to `QueryLayout`: the
>   manager channel is opened only after the bars exist (PR #242's deadlock), and the leaf resolves
>   through the binding the shell's connection just used. The greeter falls back to the origin as
>   planned, and its control saw the fallback line.
> - **`check-report`'s page count is read, not pinned**: two pages at 1360×768, taken from the
>   prompts, since a pinned count would fail on the next unrelated kernel line.
> - **The plan's list of gate sites missed two**: check-login's overview sidebar at x = 1180, and
>   check-input's corner-pinned burst. And `tune` stretched its preview wallpaper to the screen,
>   which no desktop ever did.
> - **`desktop-shell`'s library tests had never run in CI**, though its doc said `cargo xtask test`
>   ran them; the layout arithmetic's tests needed them to.
> - **`check-resolutions` earned its keep on its first two runs**, all of it gate assumptions sized
>   for 1280: the pointer's pin (2000 px of travel) could not cross 2560; the right-half snap drag
>   stopped 176 px short of 1920's edge, and made far enough for 2560 it overran the input ring until
>   paced; and `touch ./` typed unpaced into a starting terminal lost its batch at 1024×768. Every
>   gate passed at every size once those were fixed, but one boot:
> - **The 1024×768 dead-log-source failure reproduced** — `a closed log source left a CPU spinning`,
>   in one of the tool's two chain-running boots at that size, beside `test-qemu`'s 2 of 3. Still
>   unexplained, and not fixed on a guess.

> **Detail pass, 2026-09-15.** Written before any code, as Parts A, C and D were. Decisions marked
> *(maintainer's call)* were put to the maintainer with the alternatives. **Revised in review
> (PR #301)**: the first draft measured 1366×768 by the serial line alone, and a screendump showed
> QEMU cannot display it — § What QEMU can show.

### What QEMU can show (measured 2026-09-15)

**A mode chosen by QEMU's command line reaches the guest with no image changed.**
`-vga none -device VGA,xres=…,yres=…` makes QEMU's EDID prefer that mode; OVMF adds it to its mode
list and boots into it, and Limine hands it over.

**But not a width that is not a multiple of 8.** QEMU's standard VGA rounds `XRES` and
`VIRT_WIDTH` down to a multiple of 8 when they are written (`hw/display/vga.c`, `vbe_fixup_regs`),
and OVMF's GOP still reports the width it asked for. At `xres=1366` the guest logs
`framebuffer: 1366x768 pitch 5464`, draws 5464-byte rows, and QEMU scans out 1360-wide 5440-byte
ones: every row is sheared 6 px further than the last. **The first draft of this pass read the
`framebuffer:` line and a `verdict PASS` and called that a 1366×768 boot** — but the verdict and
the display self-test are self-hashes, which a wrong stride cannot fail, as the root `CLAUDE.md`
says of them. Read off the screen, the draft's claim does not hold: `check-report` at 1366×768 found
Limine's menu and then never decoded a console frame. `bochs-display`, which stores the width
unrounded, shears too at 1366×768 (its dump is 1366 wide, the picture drifts about two thirds of a
pixel a row) and is clean at 1360×768 and 1280×800; why was not found, and nothing here depends on
it.

**1360×768 is the laptop's screen as near as QEMU shows it.** The same 768 rows, and a console of
the laptop's `170x48 cells at scale 1` — without the 6-pixel right margin 1366 leaves. On the
standard VGA, `check-report` at 1360×768 found the menu and read both report pages off the screen,
and failed only where it should, on the `framebuffer: 1280x800` fact.

**Nor a padded stride.** OVMF's QEMU video driver sets `PixelsPerScanLine = HorizontalResolution` for
every mode (`OvmfPkg/QemuVideoDxe/Gop.c`), and Limine derives the pitch from it, so the padding
under QEMU is always 0 where the laptop's is 40.

**Userspace has 1280×800 written into it.** `desktop-shell` uses `SCREEN_W` and `SCREEN_H` 56 times —
both bars, the wallpaper window, the entry capacity, the placement bounds, and the overview: its
full-screen popup, its sidebar at `SCREEN_W − SIDE_W`, the wallpaper miniature, and the scale of
each window preview — and places the window list at `800 − 24`; `desktop-session-mgr` centres the
greeter on the same constants. Both say the compositor cannot report the screen's size, which
stopped being true for a manager with M9 Part B's `QueryLayout` and was never provided to anything
else. **At 1360×768**, the top bar stops 80 px short of the right edge, the window list is placed at
`y = 776`, wholly below the last row, the wallpaper window overhangs the bottom by 32 px, the
overview leaves an undimmed strip on the right with its sidebar 80 px in from the edge, and the
greeter sits 40 px left of centre and 16 px low. **These are the laptop's bugs**, found before the
laptop.

**So do the gates**, in `tools/xtask/src/main.rs`: `check-login` clicks the window list at
`y = 788` and at `x = 1200`, clicks the empty desktop at its centre `(640, 400)`, and expects
`desktop-shell: bottom bar placed at 0,776` and `wallpaper 1920x1200 drawn 1280x800 at 0,0`; `shot`
moves to the same centre, expects the same placement line, and clicks the overview at
`(1200, 788)`; `move_pointer_to` pins the pointer to `(1279, 799)`; `tune` composites at
`(1280u32, 800u32)` and ignores a screendump of any other size; `EMULATED_MACHINE_FACTS` holds
`framebuffer: 1280x800 …`, shared by `test-qemu` and `check-report`; and `bench-compose` boots a
screen too.

**One thing seen and not explained.** At 1024×768 `test-qemu`'s demo chain failed its
dead-log-source check (`only 2 of 4 CPUs ever went idle`) in 2 runs of 3; at 1366×768 it finished in
4 of 4 and at the default in 2 of 2. Nothing connects screen size to that check, and nothing here is
a fix for it — it is written down for E.4, where `check-resolutions` makes it cheap to reproduce.

### The pitch: the host tests, not a kernel knob *(maintainer's call)*

A padded stride in the guest would need the kernel to report a narrower width than the mode — a
command-line word such as `fbwidth=1366` on a 1376-wide mode, carried in the gate images'
`limine.conf`. **Not built**, because the stride is already exercised where it can go wrong, and a
knob would re-run the same code:

- **Every pixel offset is one function**, `Geometry::offset_of` (`y × pitch + x × bpp`), and every
  `Framebuffer` method is a trait default shared by the aperture-backed `RawFramebuffer` and the
  host tests' `MemFramebuffer`, which differ only in where their bytes live.
- **The compositor's host tests draw on padded screens by default** — 32 px in a 35 px stride and
  96 in 100 — and `copy_damage`, the shadow-to-display copy, finds each row through `offset_of` and
  copies `width × 4` bytes of it.
- **`check-display`'s reference scene is padded**: 64 px in a 268-byte stride.
- **The console's host tests paint a padded screen** and check the padding bytes are never written.
- **The kernel passes Limine's pitch through unchanged**, and `libdraw::acquire` has a test that a
  padded report survives into the geometry.

What stays unseen until Part F is the laptop's own padding and its width that is not a multiple of
8 — both of which the D.1 lines report on the day.

### The shape *(maintainer's calls)*

- **The display gates move to 1360×768 in CI; `test-qemu` stays at QEMU's default.** Every CI run
  then boots two sizes, so a size written back into a client fails somewhere, and CI's time does
  not change.
- **Nothing is resolution-specific, and a check across sizes runs on demand, not in CI.**
  Theoretically the system is resolution-independent; `cargo xtask check-resolutions` is how that
  gets confirmed from time to time without making every PR pay for it.
- **A client learns the screen's size from a read-only leaf**, `/dev/draw/screen`, in the same
  shape as `/dev/draw/<N>/info`. The greeter has no manager channel, and knowing the screen's size
  grants no placement authority.
- **The wallpaper fills the screen**, cropping the picture's overhang, rather than fitting it with
  bars down the sides: the shipped picture is 16:10 and the laptop is 16:9. **As a second
  `wallpaper_mode` value**, which is what M12 decision 7 designed the key for — and **scaling down
  only**: covering a screen larger than the picture needs the upscaler that decision deferred, so a
  fill that would have to scale up draws the picture at its own size, centred, and says so.

### The pieces, in dependency order

**E.1 — the screen leaf**

- [x] The compositor classifies `screen` beside `new`, `manage` and `<N>/info`, and answers a
      resolve with a `MemoryObject` holding the screen's `width` and `height` (`u32`, little-endian)
      and room reserved for what a later screen may add. A read shorter than the object is refused,
      as `info`'s is.
- [x] `docs/spec/rsproto-surface-ops.md` gains the section, beside `/dev/draw/<N>/info`.

**E.2 — the clients ask, and the constants go**

- [x] **`SCREEN_W` and `SCREEN_H` are deleted** from `desktop-shell` and `desktop-session-mgr`, and
      `BAR_PITCH` and `OVER_PITCH` with them, so every site that used one fails to compile rather
      than being found by reading. The size is read from the leaf once at startup and passed down.
- [x] **`desktop-shell`** sizes both bars, the wallpaper window, the placement bounds and the entry
      capacity from it, and places the window list at `height − BAR_H`. **The overview** too:
      `render_overview`'s popup and sidebar, `open_overview`, `present_overview` and
      `close_overview`'s buffers, `mini_wallpaper`'s reading of the wallpaper buffer — which, left at
      the old width over a 1360-wide buffer, would read every row 320 bytes (80 px) out of step and
      log nothing — and `desktop_preview`'s scaling of window origins. The work area still comes from
      `QueryLayout`.
- [x] **The greeter** centres on the leaf's size, and logs the origin it asked for, so a gate can
      check the centring rather than look at it.
- [x] **No size to fall back on.** A greeter that cannot resolve the leaf logs it and asks for the
      origin; a shell that cannot logs it and takes the size from `QueryLayout`, and without a
      manager either, draws no bars. Each is a visible wrong in a session that still starts, and
      none needs a size written down.
- [x] **`fill` becomes a legal `wallpaper_mode`**: `docs/spec/theme-toml-schema.md` names both values,
      `libdraw::theme` parses it (the refusal test keeps refusing a value that is neither, by name),
      and the theme `xtask` stages names `fill`.
- [x] **`libdraw::scale::fill`** beside `fit`: the smallest scale that covers the screen, capped at
      1, centred, the overhang cropped. Its plan's origin is signed — `Fit::origin`'s "never
      negative" stays true of `fit` and is not borrowed. Host tests: 16:10 into 16:9 (at 1360×768,
      `drawn 1360x850 at 0,-41`), 16:9 into 16:10, and a picture smaller than the screen, drawn at
      its own size and centred.
- [x] **`TODO(wallpaper-fill)` narrows to the upscaler** in `docs/rationale/deferred-decisions.md`:
      filling by scaling down exists, and a picture that would need scaling up to cover the screen
      is the trigger that remains.

**E.3 — the gates stop knowing the size, and CI's move**

- [x] **One place says what size a gate's screen is**: `qemu_display_args(size)` adds
      `-vga none -device VGA,xres=…,yres=…`, and every gate that boots a screen — `check-display`,
      `check-input`, `check-terminal`, `check-login`, `check-fbcon`, `check-live`, `check-report`,
      and the `shot` and `bench-compose` tools — takes a size that defaults to 1360×768. A size
      whose width is not a multiple of 8 is refused with the reason, since QEMU would shear it.
      `test-qemu` and `test-interactive` keep QEMU's default.
- [x] **Every coordinate a gate uses comes from the size or from the guest**: the window list's
      and its right-hand click points and the desktop's centre from the screen size; the bottom
      bar's placement line from the size; the pointer's pin corner from the size; the wallpaper line
      computed with `scale::fill` on the host — one source for the expected answer, as
      `tools/CLAUDE.md` asks — and `tune` composing at the size of the screendump it reads. The sites
      are the ones § What QEMU can show lists, and a search for `1280`, `800`, `1279`, `799`, `788`,
      `776`, `1200`, `640` and `400` as substrings, not words, finds any others; each hit left is
      one somebody read.
- [x] **The framebuffer fact splits**: `test-qemu` keeps `1280x800 pitch 5120`, `check-report`
      asserts `1360x768 pitch 5440 padding 0`.
- [x] **`check-fbcon`'s handout frame is re-measured**: at 48 rows it shows at least the last 36
      lines, and the kernel's last line was 29 lines before the handout when Part B measured it.
- [x] **`check-report`'s page count is read from the gate**: two pages at 1360×768 in the
      measurement above, pinned from the first real run.
- [x] **`check-login` asserts the layout it can now see**: the greeter's logged origin is centred
      for the size, the window-list click lands on the bottom bar, and the wallpaper line matches
      the host's `fill`.
- [x] Controls, each at 1360×768: the window list placed from the old height fails `check-login` at
      its click; the greeter's old centring fails its assertion; the leaf's resolve refused makes the
      greeter log its fallback and fail the same assertion; `wallpaper_mode` put back to `fit` fails
      the wallpaper line.

**E.4 — `cargo xtask check-resolutions`**, on demand

- [x] Runs `check-display`, `check-terminal`, `check-login` and `check-fbcon` at each of 1024×768,
      1280×800, 1360×768, 1920×1080 and 2560×1440 — every width a multiple of 8, for the reason
      above — one boot at a time, and prints a table of which passed. Not in CI.
- [x] **What it is allowed to find**: the 1024×768 demo-chain failure above is reproduced or not,
      with the transcript kept; a size that cannot boot in the guest's 256 MiB says so rather than
      timing out; 2560×1440 draws the wallpaper at its own size, centred, since filling that screen
      needs the upscaler. Anything it finds gets a decision-log entry; a class of failure gets a CI
      gate.

### What to compare on the day

Part F's laptop, against this: `framebuffer: 1366x768 pitch 5504 padding 40`, the console at
`170x48 cells at scale 1`, both bars spanning the screen, the greeter centred, and the wallpaper
filling it. Two numbers QEMU never showed the guest — the padding, and a width that is not a
multiple of 8 — so an error found only there is most likely in the arithmetic they touch.

### Left alone

- **A padded stride, or a 1366-pixel width, in QEMU** — see § What QEMU can show and § The pitch.
- **The leaf in an application's namespace.** Applications are bound `/dev/draw/new` alone (M7
  Part E), and no application reads the screen's size, so widening that bind would be unchecked:
  `verify_app_namespace` checks only that `new` resolves and `manage` does not. It is widened, with a
  check of its own, when an application first reads the leaf — the reasoning the shell already
  applies to `/applications`.
- **Scaling the wallpaper up.** `TODO(wallpaper-fill)`, narrowed above.
- **Scaling the interface for a dense screen.** At 2560×1440 the desktop is small; that is a
  resolution-*dependent* feature, not the independence this part checks.
- **Changing modes after boot.** The screen is the size Limine handed over for the life of the
  boot, and nothing here announces a change.

## Part F — the first boot ⬜

- [ ] Write the live image (`cargo xtask image --live` → `tools/build-cache/nitrox-live.img`) to a
      USB stick, boot the laptop, and fix what breaks.

The honest content of this part is unknown, which is why it is a part and not a checklist. What
is known is the order to look in: firmware handoff → framebuffer → ACPI tables → CPU/APIC
bring-up → SMP → i8042 → userspace. Part B is what makes each of those observable.

- [ ] Whatever this finds gets a decision-log entry and, where it is a class rather than an
      instance, a QEMU-side gate — because a bug found once on hardware and not gated will be
      found again.

## Part G — framebuffer cache attributes ⬜

- [ ] A cache-attribute on `MemoryObject`, a way for the namespace server to set it, and a PAT
      (or MTRR) story.

The deferral for this says it outright: under QEMU it is harmless and measured to cost nothing,
but **on real hardware it is a correctness problem** — a PCI framebuffer BAR wants
write-combining or uncached, and a write-back mapping can leave writes sitting in cache or
reorder them in ways a device does not expect. Its stated trigger is "the first boot on real
hardware, which is also the first time anybody could observe it."

Deliberately **after** Part F: the trigger is observation, and doing it blind would mean
guessing at which attribute this framebuffer wants.

## Part H — the installer ⬜

- [ ] A sized disk image (the current one is fixed at 128 MiB) and a way to write Nitrox to the
      internal disk: partition, format, populate, install the bootloader.

The machine is a test machine and its Debian install has no value (maintainer, 2026-09-10), so
this **takes the whole disk** — no shrinking, no dual boot, no probing for free space. That
removes the hardest part of writing an installer.

Open question for when we get here: whether the installer is a Nitrox program run from the live
image (self-hosting, and the better story) or an `xtask` that writes a stick from the
development machine (much less work). The live image makes the second sufficient, so the first
is not on the critical path.

---

## What Phase 5 does not do

**No pointer.** The trackpad is I²C-HID, which needs a Designware I²C controller driver, the
I²C-HID protocol, and a HID report-descriptor parser — a stack with no consumer but this one
trackpad. A USB mouse is the cheaper pointer and comes with thumb drives attached, so it is
[Phase 6](phase-6-usb.md).

**No networking** ([Phase 8](phase-8-networking.md)), **no sound**, **no power management** —
suspend, lid, battery and thermal all need AML, which means ACPICA, which is its own project
with its own trigger.

**No kernel modules.** The Tier 1 / Tier 2 driver split is designed
([`drivers-and-irps.md`](../architecture/drivers-and-irps.md)) and the loader is deferred with
the trigger "hot-pluggable or optional hardware that isn't on the boot path". Everything in
this phase is on the boot path. USB devices are the first things that are not, so the loader
belongs in Phase 6 — at its first honest consumer rather than one phase early.
