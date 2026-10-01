# Nitrox Implementation Plan — Phase 6 — USB

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams.

**Status: scoped 2026-10-01; nothing built.** This replaces the sketch written on 2026-09-10,
before Phase 5 and administration. The scope and the decisions below were agreed with the
maintainer on 2026-10-01. Each part gets its own detail pass when it is next, as administration's
parts did; what is here is the phase's shape, the design each part builds to, and the gate that
closes it. **Nothing below describes current behaviour.**

## Scope

| | |
|---|---|
| **In** | **An xHCI host-controller driver**, one for both machines. **USB enumeration** at boot and after it, with devices that **arrive and depart**. **HID keyboards and mice** in boot protocol. **USB mass storage** as block devices, with **MBR** partition tables and whole-disk filesystems beside GPT. **`fs-server-fat`**, read-write. **Removable media** a session can use and eject. **Formatting and partitioning** (`disk --format`, `disk --partition`). **Copy throughput**, measured on the laptop and then fixed where it is worst. |
| **Out** | **Kernel modules** (Tier 2), and with them module matching. I²C-HID. USB tablets and other absolute pointers. HID report descriptors. USB 3 streams and UAS. Isochronous transfers (audio, webcams). External hubs. USB device mode. exFAT. |
| **Before it** | Five small items from the laptop install (*Before Phase 6*, below), the grace period for `with` among them. |

## Decisions, 2026-10-01

The maintainer's calls, after a discussion of the sketch and a check of it against the code:

- **No kernel modules in this phase.** The sketch's Definition of Done wanted "at least one of
  those drivers loaded as a module". USB does not need one:
  - **A device that arrives needs a driver that can bind when it does, not code that arrives with
    it.** The deferral's trigger, "hot-pluggable hardware", ran those two together. A HID or
    mass-storage driver compiled in and binding at arrival meets the whole Definition of Done.
  - **Both machines need the same drivers.** xHCI is a standard register interface, like AHCI:
    `qemu-xhci` and the laptop's Sunrise Point-LP controller (`8086:9d2f`) are one driver, as the
    one AHCI driver already serves both. The USB classes are standard too.
  - **The first case of a machine needing a driver the other does not is networking** — virtio-net
    under QEMU, RTL8111/8168 on the laptop ([Phase 8](phase-8-networking.md)) — and even two NIC
    drivers can be compiled in and decline where their device is absent, as AHCI does.

  Modules wait for a driver that should not be in every image (Wi-Fi is the likely first), a driver
  restarted without a reboot, or one built outside this tree. **Which Tier 2 is** — kernel modules
  loaded at runtime, or drivers in userspace processes holding their device's registers and an
  `InterruptObject` — is open: [`drivers-and-irps.md`](../architecture/drivers-and-irps.md)
  describes both. It is recorded in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md), not decided here.
- **HID: boot protocol, decoded in the kernel.** The USB keyboard and mouse drivers decode the
  fixed boot-protocol reports into the `InputEvent` records the PS/2 driver emits, as PS/2 turns
  scancodes into keycodes in the kernel ([`input-subsystem.md`](../architecture/input-subsystem.md)
  §4). `input-server` gains only real arrivals and departures. **Report descriptors are deferred**
  until a device needs them.
- **A USB tablet is deferred.** It would give QEMU an absolute pointer and end the `--grab` dance,
  but it needs report descriptors and the deferred `EV_ABS` screen mapping, so it is its own work.
- **A removable stick is the session's.** One the storage service auto-mounted is **writable, on a
  live boot too**, and the session can **eject** it, from Files or `disk --eject`, without a
  password. Internal disks keep today's rules: read-only on a live boot, and unmounted through the
  `storage` grant. This keeps the graphical prompt's trigger from arriving here.
- **Throughput is measured first.** A part times a copy on the laptop, fixes what dominates, and
  only then sets the number the Definition of Done holds.
- **Five small items come before this phase**, the grace period for `with` among them.

## What the sketch said, and what changed

- **"Phase 5 ends with a keyboard and no mouse"** — false since the laptop's first boot
  (2026-09-15): the firmware puts the trackpad on the i8042's auxiliary port, and the PS/2 driver
  drives it ([`phase-5-bare-metal.md`](phase-5-bare-metal.md) § *No pointer — wrong*). The sketch's
  first argument for USB fell with it. A USB mouse and keyboard stay in the Definition of Done for
  other reasons: they are the first devices that come and go, and the second input producer, which
  is what proves `libinput`'s boundary.
- **The module loader** is out (above).
- **"Grow the device-interrupt vector pool"** — not needed yet. The pool holds eight, and a QEMU
  boot with a RAM disk takes six: the PIT, the RAM disk's software completion vector, AHCI, COM1,
  the keyboard and the mouse (`0x30`–`0x35`). A live boot on the laptop, with no COM1, takes
  five. The xHCI's eight MSI vectors are what it *offers*; the driver uses one interrupter and asks
  for one, which makes seven. USB's devices take none — they are all behind the controller. The
  pool grows when the next device wants a vector, which is Phase 8's network card at the latest,
  since `register_device_handler` panics when it is empty.
- **`fs-server-fat` read-write** stays, and the sketch understated what reaching a stick takes:
  thumb drives are formatted with an MBR partition table or none at all, and the kernel reads GPT
  only.
- **The throughput deferral** (`TODO(fs-throughput)`) names this phase as its trigger, and the
  sketch did not mention it.

## What exists to build on — and what does not (checked 2026-10-01)

**The kernel:**
- **Drivers are matched once, at boot.** `drivers::probe` walks the device table, brings up AHCI,
  publishes the RAM disks, then reads each disk's GPT (`kernel/src/drivers/mod.rs`). Nothing
  probes later.
- **The device table never loses an entry**: "a registered node lives for the kernel's lifetime"
  (`kernel/src/device.rs`). `/dev/blk/<n>` and `/dev/input/raw/<n>` resolve through each entry's
  served index; `/dev/registry` serves a snapshot.
- **The GPT reader runs at boot, polled, with interrupts masked** (`read_blocking`), and reads GPT
  only. There is no MBR partition parsing.
- **The kernel has two threads of its own**, the idle thread and the reaper, each with a bespoke
  park. There is no general kernel thread and no way for one to sleep until a deadline. DPCs run
  at the interrupt tail and must not block (`kernel/src/dpc.rs`). The PS/2 driver's lost-byte
  sweep rides the timer tick.
- **What a driver needs exists**: `DmaBuffer` (physically contiguous, zeroed, page- or
  block-aligned, from the buddy allocator), `map_mmio` for uncached BARs, and MSI from Phase 5
  Part A. No IOMMU is programmed; DMA is to physical addresses.
- **`DeviceKind`** has `PciFunction`, `Disk`, `Partition`, `RamDisk`, `Keyboard`, `Mouse` and
  `Console`, and a `DeviceRecord` carries PCI-shaped identity: vendor, device, class, subclass,
  prog-if, and a bus address.
- **Errors**: nothing says "the device is gone". `PeerClosed` — the other end is gone — is the
  nearest.
- **Limine's file record** is bound as revision, address, size and path (`kernel/src/limine.rs`).
  The media fields after them, which carry the GPT disk and partition GUIDs of the volume a file
  came from, are not read.

**Userspace:**
- **The device manager speaks `Arrived`, `Settled` and `Departed`**
  ([`device-manager.md`](../architecture/device-manager.md)). It replays the registry once at
  start (coldplug). Nothing sends a later `Arrived` or any `Departed`.
- **`input-server` takes devices as they arrive and depart**, up to eight, each `Departed`
  retiring its slot — host-tested, never yet exercised by a real departure.
- **The storage service owns `block`**, recognises FAT from its boot sector and never mounts it,
  spawns `/bin/fs-server-ext4` by a constant, and logs and ignores arrivals after `Settled`
  ([`storage.md`](../architecture/storage.md) §12).
- **A live boot auto-mounts read-only**; unmounting needs the `storage` grant; every mounted
  filesystem is writable by whoever reaches it.
- **Files' Places** are Home, the home folders and Root (`libfs::places`). A drive is not one.
- **`nxsh` cannot time a command.**

**Testing:**
- **QEMU already has a USB controller**: the live gates attach `qemu-xhci` with `usb-storage`
  (`tools/xtask/src/main.rs`). `usb-kbd` and `usb-mouse` exist, and QMP's `device_add` and
  `device_del` plug and unplug at runtime.
- **QEMU 11 can turn the i8042 off** (`-machine q35,i8042=off`), so a gate can boot a machine
  whose only keyboard and mouse are USB: a key reaching the greeter came through USB or nowhere.
- **The host has `mformat`, `mcopy`, `mkfs.fat` and `fsck.fat`** for building FAT fixtures and
  checking what the guest wrote, as `mke2fs`, `e2fsck` and `debugfs` serve ext4.

**The laptop:** the xHCI is at `00:14.0`, `8086:9d2f`, plain MSI with eight vectors and no MSI-X
([`phase-5-bare-metal.md`](phase-5-bare-metal.md)). **What is attached to it is not surveyed** —
built-in devices such as a webcam or Bluetooth often sit on a laptop's USB ports. Part A's hardware
report finds out.

## The design

### One xHCI driver, one interrupter

A Tier 1 driver over the xHCI register interface, matched by PCI class `0C/03/30`, serving
`qemu-xhci` and the laptop alike, with quirks taken as the laptop shows them rather than copied in
advance. It owns the command ring, one event ring on **interrupter 0 with one MSI vector**, the
device context base array and the scratchpad buffers the controller asks for, and a transfer ring
per endpoint. Every ring segment is a `DmaBuffer` page, so none crosses the 64 KiB boundary the
specification forbids. The interrupt's DPC drains the event ring and completes what each event
finishes; port status changes become work for the hub thread.

### A kernel thread that can wait

**The reaper is the first kernel thread that waits; the USB hub thread is the second**, so this is
where a general facility is built. A kernel thread can be created with a body, and it can block
until **an event is signalled or a deadline passes**. Enumeration is a sequence of steps with
waits between them — 100 ms for a newly connected device to settle, a port reset, 10 ms of
recovery, then control transfers that each complete on an interrupt. Written as a thread it reads
top to bottom. The alternative, a state machine advanced by DPCs and timer callbacks, was weighed
and set aside: it puts the same sequence into a dozen states, and a host test of it tests the
machine rather than the sequence.

The same thread scans a newly arrived disk's partitions, which today's GPT reader can only do at
boot with interrupts masked.

### Enumeration, at boot and after

At start the driver resets the controller and the hub thread handles every port that reports a
device, exactly as it handles one that reports a device later: **coldplug is the first round of
hot-plug**. For each device: reset the port, enable a slot, address it, read its device and
configuration descriptors, choose its first configuration, and match its interfaces against the
built-in class table — HID boot keyboard, HID boot mouse, mass storage bulk-only. A device nothing
matches is **listed and left alone**: registered, reported, unbound.

A USB device is a registry record of a new kind, **`UsbDevice`**, with its vendor and product IDs
in the vendor and device fields, its class triple in the class fields, and its controller as its
parent. A keyboard or mouse it provides is a `Keyboard` or `Mouse` record with the device as its
parent. A disk is a `Disk` with the device as its parent.

### Departure

A device unplugged is **departed**, not deleted:
- **Its served index is retired, never reused**, as session ids are never reused: a program still
  holding `/dev/blk/7` must not find a different disk there.
- **I/O to a departed node completes `PeerClosed`**: the other end is gone. In-flight requests are
  completed with it as the driver tears the device down; no new error code.
- **The registry carries a generation**, bumped on every arrival and departure, and a snapshot
  marks departed records rather than omitting them, so a reader comparing two snapshots sees what
  went.
- **Its children depart with it**: a disk's partitions, a device's keyboard.

### The event source

The kernel tells the device manager that the registry changed through **a notification** on the
manager's notification queue — the same mechanism that delivers `ChildExited`. The manager reads
the snapshot, compares generations and records, and sends `Arrived` and `Departed` to each class's
owner. No new channel from the kernel; one new notification kind. The manager stays the one reader
of the registry and the one place that hands devices out.

### HID, boot protocol

The driver sets each HID interface to boot protocol and polls its interrupt-IN endpoint. A
keyboard's eight-byte report becomes key presses and releases — HID usage to keycode through a
table beside the scancode table — and a mouse's report becomes `REL_X`, `REL_Y`, the wheel and
button events, each stamped at the interrupt. They are served at `/dev/input/raw/<n>`, like the
i8042's two, and reach `input-server` through the manager. The keyboard's Caps Lock and Num Lock
lights are a `SET_REPORT`, sent when the compositor's modifier state changes; whether that is in
scope is Part B's detail pass.

### Mass storage

**Bulk-only transport with SCSI**: `INQUIRY` for the vendor, product and the removable bit, `READ
CAPACITY`, `READ(10)` and `WRITE(10)`, `TEST UNIT READY`, `REQUEST SENSE` on failure, and
`SYNCHRONIZE CACHE` for `IoOpcode::Flush`. Each logical unit becomes a block `DeviceNode`, so the
block spine and everything above it apply unchanged. The disk is named for its INQUIRY strings and
its serial.

**Partition tables:** GPT as today; **MBR**, its four primary entries, an extended partition left
for later; and **no table at all**, where a filesystem starts at sector 0 — the "superfloppy" many
sticks ship as, which the storage service probes on the disk itself.

### `fs-server-fat`

A library and a server, as `fs-server-ext4` is: **FAT12, FAT16 and FAT32, with long file names**,
read and write. A long name gets a generated 8.3 short name beside it. No exFAT, which large
sticks increasingly ship with; reformatting one is what `disk --format` is for. The library is
host-tested against images `mformat` and `mcopy` build, and what it writes is checked with
`fsck.fat -n` and read back with `mcopy`. **Its write path batches from the first version** — runs
of clusters per request and the FAT's sectors cached — so the throughput part does not have to
retrofit it. The storage service spawns the server for what a device holds, by kind, rather than
by a constant.

### Removable media

**Removable** means a disk behind USB mass storage; a SATA disk is never one, whatever its bay.
For such a disk:
- **It auto-mounts writable on any boot**, a live boot included, at `/storage/<label>`.
- **`Eject`** on the session endpoint runs the unmount chain on a removable mount the service made
  — write back, flush, unmount — and answers when it is safe to remove. `disk --eject LABEL` is
  the command; Files has an eject button. It is the only unmount a session can ask for: an
  internal disk still needs the `storage` grant.
- **A session can follow mounts**: a watch on the session endpoint that sends a message per mount
  and unmount, so Files' **Drives** in Places update without polling.
- **Surprise removal** is a departure: the server's I/O fails, it exits, the mount is torn down,
  and the log says the device left while mounted. What had not been written back is lost, which is
  what *eject* exists to prevent.

**The boot medium on a live boot.** Once mass storage works, the stick a live image booted from
appears as a USB disk. It holds the ESP the machine started from, not the person's files. The kernel
reads the boot volume's GPT disk GUID from Limine's file record; the storage service passes that
disk over, as it passes over `nitrox-source`, and `nxinstall` refuses it: it holds the running
system.

### Formatting and partitioning

`disk --partition DEVICE` writes one partition spanning the disk, and `disk --format DEVICE fat|ext4
[LABEL]` makes a filesystem on a disk or partition — thin over `libgpt`, MBR for a stick, and each
filesystem library's `mkfs`. Both take the raw device, so they run through the `disks` grant
(`with admin disk --format …`), and both refuse a device in use.

### Measuring a copy

`nxsh` gains **`time`**, the wall-clock time of a pipeline, printed when it ends, so a copy can be
timed on the laptop's screen. The part times an ext4 copy under `/home` and a copy to a stick,
finds what dominates — the deferral lists the suspects and guesses at none — fixes it, and writes
the number the Definition of Done then holds.

## Kernel work in this phase

| Change | Why | ABI hash |
|---|---|---|
| Kernel threads that block on an event or a deadline | the hub thread; partition scans after boot | no |
| The xHCI driver, USB enumeration, the class table | Parts A–D | no |
| HID boot keyboard and mouse | Part B | no |
| `DeviceKind::UsbDevice`; a departed flag and a generation in the registry | Parts A and C | **yes** — `libkern::device` is shared |
| A registry-changed notification kind | Part C | **yes** — a notification kind is ABI |
| Bulk-only transport and SCSI as a block device | Part D | no |
| MBR partition tables, whole-disk filesystems, partition scans at runtime | Part D | no |
| Limine's file media fields | Part D | no |

## Parts — sketched

Ordered by dependency. Each has its detail pass before it is built.

| Part | What | Gate |
|---|---|---|
| **A** | **xHCI and enumeration, reported.** Kernel threads that wait; the xHCI driver; enumeration at boot and on later port changes; `UsbDevice` records; the hardware report's USB page. | `test-qemu` boots with `usb-kbd`, `usb-mouse` and `usb-storage` attached and asserts each enumerated with its IDs and class. `check-report` asserts the live stick listed. On the laptop, the report's USB page is the survey this plan lacks. |
| **B** | **HID keyboard and mouse, at boot.** Boot protocol, the usage table, the nodes, `input-server` taking them from the manager's replay. | A gate on a machine with **`i8042=off`**: a key and a click from USB reach a window, and a login at the greeter goes through. `test-interactive` and the other release gates unchanged with the i8042 on. |
| **C** | **Arrivals and departures.** Departed records, retired indices, `PeerClosed`, the generation, the notification, the manager's diff, `input-server` taking and retiring devices. | B's gate plugs a second `usb-kbd` in over QMP and types on it, then unplugs it, and the input server retires its slot. Host tests on the manager's diff. |
| **D** | **Mass storage.** Bulk-only and SCSI; MBR, whole-disk and runtime partition scans; the storage service mounting late arrivals and tearing down departures; the boot medium passed over; `nxinstall` refusing it. | A storage gate plugs in an ext4 stick over QMP: it auto-mounts writable, takes a file, ejects (through the admin `Unmount` until F), and the host checks it with `e2fsck` and `debugfs`. Then a stick unplugged while mounted, torn down. `check-live` and `check-install` see the boot stick passed over and refused. |
| **E** | **`fs-server-fat`**, read-write, and the storage service spawning by kind. | Host tests against `mformat` images, `fsck.fat -n` clean after every write. The storage gate with a FAT stick: mounted, written, unmounted, and the host reads the file back with `mcopy`. |
| **F** | **Removable media for a session**: writable auto-mount on a live boot too, `Eject`, `disk --eject`, the mount watch, Files' Drives and eject button. | A desktop gate: a FAT stick plugged in appears in Files, a file saved onto it from `nxedit`, ejected from Files; the host reads it back. |
| **G** | **Formatting and partitioning**: `disk --partition`, `disk --format`. | A blank stick partitioned and formatted FAT through `with admin`, then mounted and written; refused while mounted. |
| **H** | **Copy throughput**: `time`, the measurements on the laptop, the fix for what dominates, and the number. | The number, measured on the laptop and recorded. No QEMU gate holds a time, since TCG's is not the machine's. |

**Parts A–C give the input half of the Definition of Done, D–G the storage half, and H the
number.** The order puts the first boot of the laptop with a USB driver as early as it can be:
Part A's report is the survey of what is attached.

## Definition of Done

On the laptop and in the gates:
- **A USB mouse moves the cursor and a USB keyboard types at the greeter**, plugged in before boot
  or after it, and each is let go when unplugged.
- **A FAT32 thumb drive plugged in after boot mounts writable, appears in Files, takes a file saved
  from `nxedit`, and ejects from Files**; unplugged without ejecting, it is torn down cleanly.
- **A copy to the stick meets the number Part H sets.**

## What Phase 6 does not do

- **Kernel modules**, and matching devices to modules (the decision above).
- **I²C-HID**: the trackpad already works through the i8042.
- **Absolute pointers and HID report descriptors**: a tablet, a touchscreen, extra keys.
- **USB 3 streams and UAS**: one bulk-only command at a time.
- **Isochronous transfers**: audio, webcams.
- **External hubs**: devices on the controller's own ports only.
- **USB device mode.**
- **exFAT.**
- **Per-session visibility under `/storage`**: a stick is reachable by every session, as every
  mount is today.

## Before Phase 6

Five items found by the laptop install, done first at the maintainer's direction:
- **The grace period for `with`** (`TODO(view-grace)`), which needs a way for the broker to tell
  two terminal handles are one terminal.
- **Empty table cells** drawn blank rather than as `null`.
- **A model and serial column in `disk --list`**.
- **The build's commit** shown at boot and in the shell's banner, so a stale stick shows itself.
- **Which output is a diagnostic**: a program's usage and progress are drawn in the error colour
  because they go to `stderr`.

## Docs this phase owes

- **`docs/architecture/usb.md`**, new with Part A <!-- check-docs: allow-missing -->
  and grown by each part after it.
- **`device-manager.md`**: the event source (C). **`input-subsystem.md`**: the second producer
  (B, C). **`storage.md`**: FAT, removable media, eject and the watch (D–F).
- **`drivers-and-irps.md`**: kernel threads (A), and the Tier 2 question recorded rather than
  answered.
- **`deferred-decisions.md`**: read-write FAT resolved (E), `TODO(fs-throughput)` resolved or
  narrowed (H), the module loader's trigger restated (this pass).
- **The root `CLAUDE.md`**: each new gate.
