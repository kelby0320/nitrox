# Nitrox Implementation Plan — Phase 6 — USB

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. This phase is
**planned, not built** — nothing below describes current behaviour.

---

## Phase 6: USB

**Goal:** a pointer, removable storage, and the first driver that is not on the boot path.

**Why it follows bare metal rather than preceding it.** [Phase 5](phase-5-bare-metal.md) needs
no USB: the target laptop's built-in keyboard is on the i8042, and the live image puts the root
filesystem in memory. So USB is not a bring-up blocker — it is what makes the machine
*pleasant*, and it is where three separate wants converge.

### What converges here

**A pointer.** Phase 5 ends with a keyboard and no mouse. The laptop's trackpad is `ELAN0501`
on I²C-HID, which needs a Designware I²C controller driver, the I²C-HID protocol and a HID
report-descriptor parser — a whole stack whose only consumer is that one trackpad. A USB mouse
needs xHCI plus the same HID parser, and the xHCI half is reusable for everything else USB.
**The HID parser is the shared piece**, which is the argument for doing USB first and I²C-HID
later (if ever) rather than the other way round.

**Removable storage**, which is what a thumb drive is, and which needs USB mass storage plus
**FAT32 read-write**. The read-write FAT deferral exists already with the trigger "a need to
update the bootloader from within the OS, or some other ESP-write workflow"; a thumb drive is
that workflow, arriving from a different direction.

**The first hot-pluggable device**, which is the trigger the Tier 2 module loader has been
waiting for.

### Sketch

Ordered by dependency, not yet sliced into parts:

- [ ] **xHCI host controller driver** — the controller at `00:14.0` on the target machine.
      Rings, TRBs, the event ring, port status and enumeration. MSI from
      [Phase 5](phase-5-bare-metal.md) Part A is the interrupt path; Linux uses MSI for
      `xhci_hcd` on this machine. Its capability was read during Part A's detail pass: **plain
      MSI, 64-bit, eight vectors, and no MSI-X** — so nothing here needs the MSI-X mechanism
      that Phase 5 deliberately left deferred.
- [ ] **Grow the device-interrupt vector pool.** It holds eight in total
      (`DEVICE_IRQ_BASE = 0x30`, `DEVICE_IRQ_COUNT = 8`) and `register_device_handler` panics
      when it runs out — the xHCI alone advertises eight. The comment there says the fix is to
      add stubs; this is the phase that makes it necessary.
- [ ] **USB core** — device enumeration, descriptor parsing, address assignment, configuration
      selection, endpoint management, and hot-plug as an event rather than a boot-time scan.
- [ ] **HID class + report-descriptor parser** — boot protocol first (it is a fixed layout and
      gets a keyboard and mouse working), then report descriptors properly, because boot
      protocol is a fallback that real devices are allowed to refuse.
- [ ] **USB HID keyboard and mouse** into the existing `input-server`. The seam already exists:
      `libinput` consumes device-neutral events and the i8042 driver is one producer. A second
      producer is what proves that boundary was drawn in the right place.
- [ ] **USB mass storage (bulk-only transport, SCSI)** as a block `DeviceNode`, so the existing
      block spine and GPT parsing apply unchanged.
- [ ] **`fs-server-fat` read-write** — thumb drives are FAT32, and this closes the orphaned
      Phase 2 "FAT read-only" deferral.

### Kernel modules (Tier 2 drivers)

- [ ] **The module loader**: `export!` symbol tables, ELF relocation, ABI-hash enforcement,
      `SysCaps::LOAD_MODULE`.
- [ ] **The userspace driver manager**: matching `DeviceNode`s to modules and handing a driver
      process a `Handle<DeviceNode>`.

**This is where they belong, and the reason is a rule this project applies elsewhere: build an
abstraction at its second consumer, not its first.** The Tier 1 / Tier 2 split is already
designed in [`drivers-and-irps.md`](../architecture/drivers-and-irps.md), and the loader's
deferral names the trigger exactly — "hot-pluggable or optional hardware that isn't on the boot
path". Every driver Nitrox has today is on the boot path. USB devices are the first that are
not, and they arrive as several at once (HID, mass storage, per-device), so the boundary gets
designed against real consumers rather than against a guess about them.

**The counter-argument, recorded because it is reasonable**: modules would pay for themselves
across two hardware targets (QEMU and the laptop) by keeping unused drivers out of the kernel
image. That is true and it is not urgent — the kernel image is small, and a driver compiled in
and never bound costs a few kilobytes. Designing the load boundary before knowing which drivers
cross it is the more expensive mistake.

### Definition of Done

A USB mouse moves the cursor, a USB keyboard types at the greeter, and a FAT32 thumb drive
mounts, lists, and takes a file written from `nxfiles` — with at least one of those drivers
loaded as a module rather than compiled in.

### What Phase 6 does not do

I²C-HID (so the built-in trackpad stays dead unless a consumer argues for it), USB 3
SuperSpeed streams, isochronous transfers (audio, webcams), USB hubs beyond what enumeration
needs, and USB device mode.
