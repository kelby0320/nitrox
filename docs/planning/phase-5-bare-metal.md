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

## Part A — MSI, because INTx may not be routable at all ⬜

- [ ] **MSI for PCI devices**, replacing the IOAPIC path for AHCI.

**This is the part most likely to decide whether the disk works.** The AHCI driver currently
takes its interrupt line straight from the PCI Interrupt Line register
(`kernel/src/drivers/ahci.rs`, with the comment "`_PRT` routing is deferred") and uses it as a
GSI. QEMU's firmware programs that register to match the IOAPIC, so it works there. Real UEFI
frequently leaves it meaningless, because the authoritative routing lives in the ACPI DSDT's
`_PRT` — **which requires an AML interpreter**, i.e. vendoring ACPICA.

**MSI is the way out, and it is far smaller than the alternative.** An MSI-capable device is
told a vector and a LAPIC address and writes it itself; no `_PRT`, no AML, no IOAPIC entry.
Every PCIe function on Sunrise Point-LP supports it, and Linux uses it on this exact machine.
That reclassifies the existing MSI deferral — filed under "performance work on interrupt-heavy
devices" — as **the real-hardware unblocker**.

Two deferrals resolve with it. The MSI/MSI-X entry closes, and **shared INTx chaining**
(trigger: "real hardware where INTx lines are shared across functions") becomes moot for
anything MSI-capable, because MSI vectors are never shared.

- [ ] The device-interrupt install family gains its **second member**, which is the trigger the
      existing deferral names for promoting `install_pci_irq` into an `ArchIrqInstall` trait.
      Build the abstraction now, at the second consumer, not before.
- [ ] Keep the IOAPIC path: the i8042 is on GSI 1 on this machine and is not going anywhere
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

- [ ] **The form is selected by reading Message Control bit 7**, and both branches exist from the
      first commit — not a 64-bit driver with a 32-bit fixup added after the laptop refuses to
      boot.
- [ ] **The 32-bit branch is the one under host test.** `pci/mod.rs`'s existing `FakeCfg` can
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

- [ ] **Config-space access that outlives enumeration.** This does not exist today.
      `kernel/src/pci/mod.rs` reserves *one* vmap page and repoints it per function inside
      `enumerate()`; when that returns, no driver can read its own config space. MSI is entirely
      config-space writes, so this is the first deliverable rather than a detail of a later one.
      `ResourceDescriptor` already carries `seg`/`bus`/`dev`/`func`, so the address is
      re-derivable. What needs deciding is the mapping's **lifetime**: a page per claimed device,
      or one shared window behind a lock. The shared window is a mutual-exclusion problem the
      enumeration-time code never had, because it ran before the scheduler existed.
- [ ] **A capability-list walk** in the neutral PCI module: Status bit 4, the pointer at `0x34`,
      the `id`/`next` chain, with a cycle bound. Capability *layout* is PCI-SIG and identical on
      every architecture, so this is neutral kernel code by the same argument that puts ECAM
      enumeration there.
- [ ] **The arch seam, chosen before the code is written.** The MSI message *layout* is
      x86-specific: the address encodes a LAPIC destination (`0xFEE0_0000 | dest << 12`) and the
      data encodes a vector and delivery mode. The capability *write* is not. Two shapes are
      available and they are not equivalent — (1) `arch` exposes `msi_message(vector, cpu) ->
      (addr, data)` and neutral PCI code writes the capability, or (2)
      `ArchIrqInstall::install_msi(cfg, handler)` takes a config-space accessor and does the
      whole thing inside `arch/`. **Prefer (1):** it keeps PCI knowledge out of `arch/`,
      mirroring the split already made between `IrqRouter` (routing) and `Platform` (firmware
      facts), and it leaves the capability walk host-testable against `FakeCfg`, which is where
      the 32-bit form above gets its coverage. `cargo xtask check-arch` enforces whichever is
      picked, so picking after the code is written is the expensive order.
- [ ] **The trait promotion.** `install_pci_irq` is a neutral free function carrying a
      `TODO(msi)` that names exactly this moment. The family gains its second member, so it
      becomes `ArchIrqInstall` with the INTx and MSI installs side by side.
- [ ] **The driver switch, with INTx kept as the fallback.** AHCI prefers MSI when the capability
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

- [ ] The driver sets bus-master enable explicitly rather than inheriting it.

**Enabling MSI means disabling INTx.** The Interrupt Disable bit (command bit 10) is clear on
every function at handoff. A device left able to deliver both, with the IOAPIC entry we routed
still live, is a spurious-interrupt source that would present as a driver bug.

- [ ] Setting MSI Enable also sets Interrupt Disable; taking the INTx fallback leaves it clear.

### The gate, and the control that makes it mean something

**`test-qemu` passing on the MSI path proves nothing on its own** — the identical transcript is
produced by a driver that silently fell back to INTx, which is this part's most likely real
failure. So the adjudication is on *which path was taken*, not on the boot succeeding:

- [ ] The driver says which path it took — it already prints a line of the form
      `ahci: INTx GSI10 -> vec 0x30` — and an `xtask` check reads that line. PR #293 spent a
      branch on self-tests nobody read; this one is written with its reader.
- [ ] **Match the path token (`INTx` versus `MSI`), never the vector**, which is not stable
      across builds: the selftest image's `IrqRouter::self_test` registers `pit_tick` and never
      releases the slot, so AHCI lands at `0x31` there and at `0x30` in a release image. All
      eight captured transcripts in `tools/build-cache/` split on exactly that line. A matcher
      written against the literal passes `test-qemu` — Part A's named gate — and fails the first
      time it is pointed at `check-login` or `test-interactive`. Part A's own switch moves the
      number again, which is the section's thesis restated: the claim under test is the path, not
      the vector.
- [ ] A **forced-INTx probe** still boots, since that path stays as the fallback.
- [ ] **The negative control to run before calling it done:** make the capability walk fail to
      find MSI. The boot must still pass *and* the adjudicated line must change. A gate that
      reports the same thing either way is the defect, not the test.
- [ ] The **`CAP.S64A` assert** belongs in this change (flagged in PR #293). The HBA reports
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

## Part B — the machine can tell you why it failed ⬜

- [ ] **An early framebuffer console**: `kprint` renders to the screen as well as to COM1.

**Without this the first boot is undiagnosable.** `kprint` writes to the serial port and tees
into an in-memory ring (`klog.rs`) that a userspace reader can map. The laptop **has no serial
port**, and the ring is only reachable once userspace is running — so a boot that dies before
`init` produces a black screen and nothing else. Every gate we own drives the guest over
COM1; none of that survives contact with this machine.

The pieces exist: `kernel/src/font.rs` and `kernel/src/framebuffer.rs` already draw text. What
is needed is a scrolling character console over the Limine framebuffer, live from the moment
the framebuffer request is answered — i.e. before ACPI, PCI, or anything that can fail.

- [ ] It must survive the compositor taking the framebuffer over. Simplest rule: the kernel
      console owns the screen until userspace first commits a frame, and a later panic takes it
      back.
- [ ] Gate it under QEMU with `-serial none`, so the assertion is "the boot is legible with no
      serial port at all" rather than "the code compiles".

## Part C — the live image ⬜

- [ ] **A third image mode whose root filesystem is the initramfs**, so the first boot on real
      hardware needs no storage driver and no partitioning.

**Why it exists.** Booting from a USB stick does not give us a root filesystem on that stick.
Limine reads the kernel and the initramfs through **UEFI Boot Services** — the firmware's own
USB stack — and then exits boot services before entering the kernel. From that moment we have
no USB driver, so an ext4 partition on the stick is unreachable. A live image sidesteps
storage entirely: Limine loads roughly 5 MB (the release program set plus the fonts) into
memory as a module, and 6 GB of RAM makes that a non-issue.

**It is a diagnostic instrument, not a product.** Its whole value is that it removes storage
from the equation for the first attempt: if the screen stays black, "did AHCI work?" is not one
of the variables. Once Part F has booted the machine, Part G installs to the disk and the live
mode has done its job.

### The discipline: the live-ness is data, not code

**This is the constraint that makes a temporary stage safe** (maintainer's call, 2026-09-10):
*no production code may change to enable it.* No `#[cfg(feature = "live")]` anywhere in
`kernel/` or `userspace/`. The live configuration is a different `init.toml`, a different
`services.toml`, and a fatter initramfs — all of which are build-tool concerns.

That is the same rule [`test-path-retrofit.md`](test-path-retrofit.md) established and proved:
*the software under test is the software that ships*. It took `session-mgr` from 31 build-mode
`cfg` sites to zero on exactly this argument, and `cargo xtask check-images` is what keeps it
true.

- [ ] **`check-images` learns the third mode**, and the claim it enforces is that the live
      image's **programs are byte-identical** to the release image's; only the data differs. A
      live-only `cfg` then fails the build rather than surviving to be forgotten.
- [ ] `INITRAMFS_MAX_BYTES` (384 KB) becomes per-mode. The ceiling exists to catch drift in a
      *boot* image and must not be silently raised for it — an image deliberately carrying
      everything needs its own, larger number and its own reason.

### What it needs from `init`, and why that is not throwaway either

- [ ] **A bind-mount concept in `init.toml`** — "bind an already-available endpoint at another
      path, scoped to a subtree" — so `/bin` can be `/initramfs/bin`.

**This is a feature the project already owes.** It is the blocker on `/subtreetest`, the last
build-mode `cfg` in `init`, deferred from the test-path retrofit on 2026-08-24. That box says
it plainly: *"the manifest needs 'bind an already-mounted server's endpoint at another path,
with a subtree base' — which is not a test accommodation: `session-mgr` does exactly that for
`/home` on every login, and it is what `mount --bind` is everywhere else."*

So the live image does not want temporary code; it wants a general mechanism we want anyway,
and building it **closes** an open item. Doing it here also finishes the retrofit.

Nothing else is needed from the kernel: `init` already spawns from `/initramfs/sbin/…`, and the
in-kernel `/initramfs` server is already namespace-visible.

### Accepted limitations of the live mode

Stated so they are not mistaken for bugs: **no writable `/home`**, so nothing can be saved, and
no persistence across a reboot. Both are properties of the configuration and disappear the
moment root is on a disk.

## Part D — the hardware report ⬜

- [ ] **A boot mode that says what it found**: firmware handoff values, the ACPI tables located,
      the ECAM windows, every PCI function with class and BAR layout, the framebuffer geometry
      including pitch, CPU features, the MADT's CPUs and IOAPICs, and which drivers bound.

Under QEMU this is a convenience. On a machine with no serial port and no debugger it is the
only way to learn anything from a boot that gets partway. It is deliberately Part D rather than
Part F's first task: **it must be built and read under QEMU first**, where we know what the
right answers look like, so that on the laptop the *differences* stand out.

## Part E — the resolution this machine actually has ⬜

- [ ] Boot QEMU at **1366×768 with a padded pitch** and pass every display gate.

`check-display` compares the guest's screen against a `libdraw` render, and the whole display
arm has only ever run at the QEMU default. A padded stride and an odd width are exactly the
conditions under which a compositor's damage arithmetic goes wrong by a few pixels a row.
`libdraw`'s half is a host test; the guest half is a gate run at the laptop's geometry.

## Part F — the first boot ⬜

- [ ] Write the live image to a USB stick, boot the laptop, and fix what breaks.

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
