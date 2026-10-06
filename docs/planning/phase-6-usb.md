# Nitrox Implementation Plan — Phase 6 — USB

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams.

**Status: scoped 2026-10-01; Part A detailed and built 2026-10-02; Part B detailed 2026-10-03 and
built 2026-10-05; Part C detailed and built 2026-10-05; Part D detailed 2026-10-06; Parts D–H not
built.** This replaces the
sketch written on 2026-09-10, before Phase 5 and administration. The scope and the decisions below
were agreed with the maintainer on 2026-10-01. Each part gets its own detail pass when it is next,
as administration's parts did; what is here is the phase's shape, the design each part builds to,
and the gate that closes it. **Nothing below describes current behaviour.**

## Scope

| | |
|---|---|
| **In** | **An xHCI host-controller driver**, one for both machines. **USB enumeration** at boot and after it, with devices that **arrive and depart**. **HID keyboards and mice** in boot protocol. **USB mass storage** as block devices, with **MBR** partition tables and whole-disk filesystems beside GPT. **`fs-server-fat`**, read-write. **Removable media** a session can use and eject. **Formatting and partitioning** (`disk --format`, `disk --partition`). **Copy throughput**, measured on the laptop and then fixed where it is worst. |
| **Out** | **Kernel modules** (Tier 2), and with them module matching. I²C-HID. USB tablets and other absolute pointers. HID report descriptors beyond a mouse's (revised 2026-10-03, Part B). USB 3 streams and UAS. Isochronous transfers (audio, webcams). External hubs. USB device mode. exFAT. |
| **Before it** | Five small items from the laptop install, in [their own plan](laptop-polish.md), the grace period for `with` among them. |

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
  loaded at runtime, or drivers in userspace processes that hold an `InterruptObject` (and, a
  capability question of its own, their device's registers) — is open:
  [`drivers-and-irps.md`](../architecture/drivers-and-irps.md) describes both. It is recorded in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md), not decided here.
- **HID: boot protocol, decoded in the kernel.** The USB keyboard and mouse drivers decode the
  fixed boot-protocol reports into the `InputEvent` records the PS/2 driver emits, as PS/2 turns
  scancodes into keycodes in the kernel ([`input-subsystem.md`](../architecture/input-subsystem.md)
  §4). `input-server` gains only real arrivals and departures. **Report descriptors are deferred**
  until a device needs them. *(Revised 2026-10-03 at the maintainer's request: a mouse's is read,
  for its wheel — § Part B in detail.)*
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
- **"Grow the device-interrupt vector pool"** — not needed yet. The pool holds eight, and a
  release boot takes at most five: AHCI, COM1 (taken whether or not a UART answers), the i8042's
  two and the RAM disk's software completion vector, as `kernel/src/io/ramdisk.rs` lists them. A
  self-test boot adds the PIT's, for six. The xHCI's eight MSI vectors are what it *offers*; the
  driver uses one interrupter and asks for one, which makes seven at most. USB's devices take
  none — they are all behind the controller. The pool grows when the next device wants a vector,
  which is Phase 8's network card at the latest, since `register_device_handler` panics when it
  is empty. (The scoping first said six for a QEMU boot and blamed the laptop's five on its missing
  COM1; the PIT's vector is the self-test's, and COM1's is taken regardless — PR #350 review.)
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
- **Kernel threads exist, and so does a deadline wait.** `sched::spawn` creates a kernel thread
  running a function, and `sched::wait_on` blocks the current thread on kernel objects until one
  signals or an absolute deadline passes; `park_briefly` in `kernel/src/object/file_object.rs`
  sleeps with it, and the page-cache fill waits on a `PendingOperation` with it. **What has not
  happened is a long-lived kernel thread blocking in `wait_on`**: the self-test workers never call
  it, and the reaper parks by hand, for a reason of its own. DPCs run at the interrupt tail and
  must not block (`kernel/src/dpc.rs`). (The scoping first said no kernel thread could wait; the
  search that would have found `spawn` printed it and was misread — PR #350 review.)
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

### The hub thread

**A kernel thread from `sched::spawn` that waits with `sched::wait_on`** — on a transfer's
`PendingOperation`, on the controller's port-change signal, or on nothing until a deadline.
Enumeration is a sequence of steps with waits between them — 100 ms for a newly connected device
to settle, a port reset, 10 ms of recovery, then control transfers that each complete on an
interrupt. Written as a thread it reads top to bottom. The alternative, a state machine advanced by
DPCs and timer callbacks, was weighed and set aside: it puts the same sequence into a dozen states,
and a host test of it tests the machine rather than the sequence. **It is the first long-lived
kernel thread to block in `wait_on`**, so Part A's gate is also that path's first exercise.

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

*(Revised 2026-10-05, in Part C's detail pass, with the maintainer: a node to read,
`/dev/registry/changes`, in place of the notification. The kernel cannot name the manager to send
it one — § Part C in detail.)*

### HID, boot protocol

The driver sets each HID interface to boot protocol and polls its interrupt-IN endpoint. A
keyboard's eight-byte report becomes key presses and releases — HID usage to keycode through a
table beside the scancode table — and a mouse's report becomes button events, `REL_X` and `REL_Y`,
each stamped at the interrupt. They are served at `/dev/input/raw/<n>`, like the i8042's two, and
reach `input-server` through the manager. The keyboard's Caps Lock and Num Lock lights are a
`SET_REPORT`, sent when the compositor's modifier state changes; whether that is in scope is Part
B's detail pass. *(It is, with Caps Lock and Num Lock made locks and the keypad mapped, for both
keyboards — § Part B in detail.)*

**Boot protocol has no wheel.** HID 1.11 defines a boot mouse's report as buttons, X and Y, and
anything after them is the device's own. The wheel needs report descriptors, so it is a cost of the
boot-protocol call, deferred with them (PR #350 review). *(Revised 2026-10-03: a mouse's report
descriptor is read and the mouse run in report protocol, with boot protocol as the fallback.)*

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
| The hub thread: the first long-lived kernel thread blocking in `wait_on` | enumeration; partition scans after boot | no |
| The xHCI driver, USB enumeration, the class table | Parts A–D | no |
| HID boot keyboard and mouse, a mouse's report descriptor | Part B | no |
| A one-byte write to a raw input node: a keyboard's lights | Part B | no — `IoOpcode::Write` exists, and a char node now takes it |
| `DeviceKind::UsbDevice`; a departed flag and a generation in the registry | Parts A and C | no — not a hash input; `abi-sync-check` guards `libkern::device` |
| `/dev/registry/changes`, a node to read, in place of the registry-changed notification the scoping planned (Part C's detail pass, the maintainer's call) | Part C | no — a node and a path, where a notification kind would have been ABI |
| Bulk-only transport and SCSI as a block device | Part D | no |
| MBR partition tables, whole-disk filesystems, partition scans at runtime | Part D | no |
| Limine's file media fields | Part D | no |
| `BOOT` in a registry record's `flags`, and `/dev/devices`' `boot` column (Part D's detail pass) | Part D | no — not a hash input; `abi-sync-check` guards the flag |

## Parts — sketched

Ordered by dependency. Each has its detail pass before it is built.

| Part | What | Gate |
|---|---|---|
| **A** | **xHCI and enumeration, reported.** The hub thread; the xHCI driver; enumeration at boot and on later port changes; `UsbDevice` records; the hardware report's USB page. | `test-qemu` boots with `usb-kbd`, `usb-mouse` and `usb-storage` attached and asserts each enumerated with its IDs and class. `check-report` asserts the live stick listed. On the laptop, the report's USB page is the survey this plan lacks. |
| **B** | **HID keyboard and mouse, at boot.** Boot protocol, the usage table, the nodes, `input-server` taking them from the manager's replay. | A gate on a machine with **`i8042=off`**: a key and a click from USB reach a window, and a login at the greeter goes through. `test-interactive` and the other release gates unchanged with the i8042 on. |
| **C** | **Arrivals and departures.** Departed records, retired indices, `PeerClosed`, the generation, the change node (a notification until Part C's detail pass), the manager's diff, `input-server` taking and retiring devices. | B's gate plugs a second `usb-kbd` in over QMP and types on it, then unplugs it, and the input server retires its slot. Host tests on the manager's diff. |
| **D** | **Mass storage.** Bulk-only and SCSI; MBR, whole-disk and runtime partition scans; the storage service mounting late arrivals and tearing down departures; the boot medium passed over; `nxinstall` refusing it. | A storage gate plugs in an ext4 stick over QMP: it auto-mounts writable, takes a file, ejects (through the admin `Unmount` until F), and the host checks it with `e2fsck` and `debugfs`. Then a stick unplugged while mounted, torn down. `check-live` and `check-install` see the boot stick passed over and refused. *(Detailed 2026-10-06: `check-storage` gains these steps, a live boot's read-only auto-mount remounted writable until Part F, pending the maintainer's call — § Part D in detail.)* |
| **E** | **`fs-server-fat`**, read-write, and the storage service spawning by kind. | Host tests against `mformat` images, `fsck.fat -n` clean after every write. The storage gate with a FAT stick: mounted, written, unmounted, and the host reads the file back with `mcopy`. |
| **F** | **Removable media for a session**: writable auto-mount on a live boot too, `Eject`, `disk --eject`, the mount watch, Files' Drives and eject button. | A desktop gate: a FAT stick plugged in appears in Files, a file saved onto it from `nxedit`, ejected from Files; the host reads it back. |
| **G** | **Formatting and partitioning**: `disk --partition`, `disk --format`. | A blank stick partitioned and formatted FAT through `with admin`, then mounted and written; refused while mounted. |
| **H** | **Copy throughput**: `time`, the measurements on the laptop, the fix for what dominates, and the number. | The number, measured on the laptop and recorded. No QEMU gate holds a time, since TCG's is not the machine's. |

**Parts A–C give the input half of the Definition of Done, D–G the storage half, and H the
number.** The order puts the first boot of the laptop with a USB driver as early as it can be:
Part A's report is the survey of what is attached.

## Part A in detail *(2026-10-02)*

### The spike: what exists, and what is missing

**The kernel:**
- **Drivers bind before the scheduler runs.** `drivers::probe` runs with interrupts masked, and
  AHCI's bring-up is polled through it. The scheduler, the APs, the hardware report and `init`
  follow, in that order (`kernel/src/main.rs`). So the controller's bring-up fits in `probe`, but
  enumeration does not: it waits for interrupts and for time to pass, so it needs a thread, and a
  thread needs the scheduler.
- **A kernel thread can wait on what a DPC signals.** `sched::spawn` makes the thread, and
  `sched::wait_on` blocks it with a deadline on any of:
  - a `PendingOperation` the DPC completes with `complete_pending_op`;
  - an `InterruptObject` the DPC signals with `signal_interrupt`. It is a latching counter, so a
    port change that lands while the thread is busy is not lost. A kernel caller consumes one with
    `interrupt_consume`, as `sys_wait` does for a process;
  - nothing at all, to sleep until the deadline.

  The reaper parks by hand only because what wakes it is a queue inside the scheduler, not an
  object. A wake on the CPU that took the interrupt is prompt when that CPU is idle: the
  device-interrupt tail calls `resched_if_idle` (2026-07-23).
- **The registry only grows, and nothing has added to it after boot.** `device::register*` take a
  `SpinLock` that allocates while held, so the thread registers, never a DPC. Every reader so far
  has read a table finished before `init`, and `device-mgr` replays it once.
- **A record is PCI-shaped.** The table keeps beside each node what the node cannot say about
  itself: kind, served index, parent, driver. A non-block node's name is a word from its kind
  (`keyboard`), so a USB device's product name has nowhere to go yet. `DeviceRecord` has three
  reserved bytes (`_pad`). `device-mgr` matches on `DeviceKind` exhaustively in four places.
- **What a driver needs exists**: 64-bit BAR sizing (`kernel/src/pci/mod.rs`), `map_mmio`,
  `DmaBuffer` (page-aligned and a power of two, so no ring segment crosses 64 KiB), `read_msi`
  and `program_msi`, `enable_bus_master`, and an outcome recorded per function.
- **The command line has one flag**, `hwreport` (`kernel/src/cmdline.rs`, host-tested). The
  installed system's `limine.conf` has `timeout: 0`, so it shows no menu and its command line
  cannot be changed. The live stick's menu can.

**QEMU 11.0.2, asked rather than remembered:**
- `qemu-xhci` is `1b36:000d`, with a 64-bit BAR0, MSI and MSI-X. **`p2=4` and `p3=4` are four
  connectors, not eight**: each has a USB 2 and a USB 3 port number (port *n* and *4+n*), and a
  device takes whichever its speed needs. Four devices fill the root, and a fifth lands behind a
  hub if one is attached. (The first version of this pass said eight ports — PR #353 review.)
- `info usb` puts `usb-kbd` and `usb-mouse` at 480 Mb/s, and `usb-storage` at 5000 Mb/s on a USB
  3 port. `usb_version=1` attaches a device at 12 Mb/s. So a gate covers high, full and
  SuperSpeed. **Low speed, which most real mice use, no QEMU USB device takes a setting for**: the
  laptop is its first test.
- Every USB device takes `pcap=`, which writes its traffic for Wireshark. That is the tool for a
  build that disagrees with a device.
- The live gates already attach `qemu-xhci` and the stick. `test-qemu` attaches no USB, but has a
  QMP socket.

**The laptop** (the 2026-09-10 dump: `lspci -vv` and `/proc/interrupts` under Linux):
- `00:14.0`, BAR0 64 KiB and 64-bit, MSI with eight vectors, no MSI-X: as the plan says.
- **It was in D3 when the dump was taken** (`Status: D3 NoSoftRst+`), because Linux suspends an
  idle controller. What the firmware leaves it in at `ExitBootServices` is unknown, and in D3hot
  its registers read as all ones. So the driver puts it in D0 through its power-management
  capability before anything else, and **waits the 10 ms** PCI PM 1.2 requires before touching it.
  `NoSoftRst+` says the transition keeps the BARs and the command register; a function without it
  is reset by the transition, and the driver restores both, as Linux's `pci_power_up` does. QEMU
  never takes this path.
- **No USB input is built in**: the keyboard is the i8042's and the touchpad is I²C (`ELAN0501`).
  Linux still took about 2,000 xHCI interrupts, so something internal is probably attached, such
  as a webcam or Bluetooth. The report will say.
- **From Linux's `xhci` driver** (checked against its source by the PR #353 review):
  - **Intel hosts need 1 ms between setting `HCRST` and the next register access**, which Linux's
    comment says can otherwise hang the machine, rarely. The pause comes *before* the first poll —
    the read it guards is the poll — so no bound on the loop can stand in for it.
  - **This controller is on Linux's missing Cold Attach Status list**, which Linux applies only
    after resume, citing an Intel PCH erratum. That a USB 3 device attached at boot could leave its
    port in compliance mode is this plan's guess, not Linux's; the report prints every port's
    state at start, so a stuck port would show, and a warm reset clears one.

### The shape

**Bring-up comes in two halves.**
- **In `probe`, polled**, in this order:
  - the controller into D0, and bus mastering on;
  - the BIOS handoff through the USB Legacy Support capability: OS-owned, a bounded wait for the
    firmware to let go, and its SMIs off;
  - halt; then set `HCRST`, **pause 1 ms**, wait for `HCRST` to clear, and wait for Controller
    Not Ready to clear (xHCI §5.4.1, in Linux's order);
  - the structures: the device context base array, the scratchpad buffers the controller asks
    for, the command ring, and one event-ring segment with its table, on interrupter 0;
  - MSI, and the outcome *claimed*.

  **Every wait is bounded, and a failed one declines the function with its reason.** No USB is a
  diagnosable failure; a hang on the laptop is not.
- **After the APs are up**, a new `drivers::start` spawns **the hub thread**. It sets the
  controller running, reads every port and enumerates. It starts after the APs rather than before
  them, though that would overlap its waits with theirs: AP bring-up is the scheduler's most
  delicate moment, and the only cost is the wait below.

**The boot waits for the first round**, bounded, before the report and `init`. That way the
report lists what is attached, and a device present at boot is in the registry before
`device-mgr` replays it, which Part B needs. A bound that passes is logged, and whatever is still
enumerating carries on as a later arrival. **The first round:**
1. the USB 2 debounce, 100 ms, taken once for every port, which also lets USB 3 links train;
2. then each port that reports a connection, **one at a time**, since only one device may answer
   at address 0.

On a machine with nothing attached, the round costs about 100 ms.

**Per device:**
1. A USB 2 port is reset; a USB 3 port is already enabled by link training.
2. *Enable Slot*, then *Address Device*.
3. The device descriptor's first eight bytes, then *Evaluate Context* if the default endpoint's
   maximum packet size differs from the speed's default. **At SuperSpeed `bMaxPacketSize0` is an
   exponent** — 9 means 512 — and a byte count everywhere else. Compared literally, every
   SuperSpeed device would get a maximum packet of 9, and QEMU would not notice: its xHCI reads
   the field only for a debug message. The laptop's first USB 3 device would.
4. The whole device descriptor, then the configuration descriptor: its nine bytes, then
   `wTotalLength`.
5. String descriptor 0, then the product and serial strings.
6. **The class match, logged and not acted on.** There is no `SET_CONFIGURATION`: the class
   driver that binds sets the configuration, in Parts B and D.

**Every command and control transfer has a deadline.** A device that misses one is logged, and
its slot disabled.

**Later port changes take the same path**: coldplug is the first round of hot-plug. A disconnect
disables the slot and is logged. **Its record stays** until Part C gives the registry departures,
which is the one place Part A is knowingly incomplete.

**The record:**
- `DeviceKind::UsbDevice` (8). The record's `vendor` and `device` are `idVendor` and
  `idProduct`. **The node's own descriptor stays PCI's**: vendor `0xFFFF`, as for every node that
  is not a PCI function, because `pci_parent` (`kernel/src/device.rs`) reads any other vendor as
  "has a PCI address", and a USB node's zero address would find the host bridge. The USB IDs live
  in the table's entry, beside the name, and fill the record from there.
- The class triple is the device descriptor's, or the first interface's when the device's is zero,
  as most are.
- `driver` is `xhci`, and `parent` is the controller.
- The name is the product string and the serial, as a disk's is its model and serial. With no
  strings, it is the IDs.
- **`_pad`'s first two bytes become `port` and `speed`.** They are zero for every other kind, so
  no reader changes and `REGISTRY_VERSION` stays 1.
- The table keeps the name in the entry, beside the kind.
- **One controller.** A second is declined, as AHCI drives one.

**`device-mgr`** names a USB device **`usb-<id>`**, after its registry id. A port is reused when a
device leaves and another arrives, and its record stays, so two records could share a port; a
connector also has two port numbers. Every other name for a node that comes and goes is keyed on
an index never reused, for the reason the Departure section gives. (The first version of this
pass said `usb-<port>` — PR #353 review.) Its kind is `usb`, it has no path, its description is
the name, IDs, port and speed, and its parent is the controller's function. So
`list /dev/devices` shows them.

**The log is the report.** One line per controller fact, one per port at start, one per device,
and one when the first round ends. Illustratively, with the IDs left for the first boot to read:
```
xhci: 00:14.0 up: 16 ports (USB 2: 1-12, USB 3: 13-16), 32 slots, 64-byte contexts, 4 scratchpads
xhci: port 1 at start: connected, high-speed
usb: port 1: vvvv:pppp class 03/01/01, high-speed, "QEMU USB Keyboard": HID boot keyboard
usb: port 4: vvvv:pppp class 09/00/00, full-speed, "QEMU USB Hub": a hub, not supported
usb: first round: 4 device(s) in 310 ms
```

**`usb=off`** on the command line declines the controller with that reason. The installed system
has no menu to pass it, but the first boot of a USB build on the laptop is the live stick's report
entry, whose menu has an editor. It is the way past a bring-up that hangs anyway.

### Pieces

- **A.1 The controller.** `drivers::xhci`'s bring-up in `probe`; the event ring's interrupt and
  DPC; the extended capabilities read, with Supported Protocol giving which ports are USB 2 and
  which USB 3; each port's state at start, logged; `usb=off`. **Built 2026-10-02**, with the
  controller running from `probe` so a No Op proves the rings, and the per-port log moved to
  A.2's first round. QEMU's controller is configured as the laptop's, with MSI and no MSI-X,
  because its default offers MSI-X alone (decision log).
- **A.2 The hub thread and enumeration.** `drivers::start`; the first round and the boot's bounded
  wait; the per-device sequence through the strings; the class table; later connects and
  disconnects. **Built 2026-10-02**: the first boot enumerated QEMU's four devices in 173 ms, and
  the hot-plug worked as planned. `test-qemu` gained a **fifth device, `usb-ccid`**, because none
  of the four takes Evaluate Context: each keeps its speed's default packet size, and QEMU ignores
  the size anyway. The reader is full-speed with 64-byte packets, and is also a truer "nothing
  matches" than the hub, which matches as a hub.
- **A.3 The records.** `UsbDevice`, `port` and `speed`, names in the table's entries, and
  `device-mgr`'s names. **Built 2026-10-02.** `boot-probe` finds five records, not four, with
  A.2's reader. **The hot-plug races `device-mgr`'s one read** of the registry: under KVM the
  manager read 19 records and `boot-probe` 21. So `boot-probe` holds the manager to the registry's
  first records, with every record after them a USB device, rather than to the whole table. That
  is the gap Part C closes, made visible (decision log).
- **A.4 Docs.** Below. **Done 2026-10-02**, with A.1–A.3, each part's in its own change.

### Gates

- **`test-qemu`, TCG and KVM**, attaches `qemu-xhci,p2=8,p3=8` — eight connectors, so the
  hot-plug below reaches a root port rather than the hub — with:
  - `usb-kbd`, at high speed;
  - `usb-mouse,usb_version=1`, at full speed;
  - `usb-storage` over a blank image, at SuperSpeed;
  - `usb-hub`, which nothing matches.
- **On the host**, `test-qemu` asserts:
  - the controller claimed over MSI;
  - each device's line: its port, speed, IDs, class, name and match;
  - the first round finished within its bound.
- **`boot-probe`** finds four `UsbDevice` records whose parent is the controller's id, carrying
  the IDs, class, port and speed the log gave. A log line can be printed for a device the table
  never got.
- **A hot-plug, from the host:** once the first round is logged, `device_add usb-kbd` over QMP
  and its arrival line, then `device_del` and its disconnect line. With the default four
  connectors the four devices above fill the root, and the keyboard would land behind the hub,
  where Part A does not look: the gate would wait for a line that cannot come (QEMU, `info usb`).
- **`check-report`**: the live stick's line on a report page.
- **Host tests, in the kernel crate:**
  - TRBs, and a ring's enqueue across its link TRB with the cycle bit toggled;
  - the event ring's dequeue by cycle;
  - the extended-capability walk, with Legacy Support and the Supported Protocol port ranges;
  - 32- and 64-byte contexts, and the scratchpad array;
  - descriptor parsing from bytes real devices sent, and from bytes no correct device sends: a
    short `bLength`, or a `wTotalLength` past the buffer;
  - a SuperSpeed device descriptor with `bMaxPacketSize0 = 9`, read as 512;
  - the class table, including a hub and a composite device;
  - a UTF-16 string made printable;
  - `usb=off` on the command line.
- **The laptop:** the live stick's report entry, with its USB lines photographed. That is the
  survey this plan lacks. It is a step for the maintainer, not a gate.

Adding a PCI function to `test-qemu` shifts every registry id after it. `boot-probe` finds
records by kind and name, but that is to be checked when it is built.

### Not in Part A

- Binding a class driver (Parts B and D), and with it `SET_CONFIGURATION` and every endpoint but
  the default one.
- Departure records (Part C).
- External hubs, a second controller, and MSI-X or INTx: both machines have MSI, and a controller
  without it is declined.
- Link power management, and suspend.

### Docs Part A owes

- **`docs/architecture/usb.md`**, new. <!-- check-docs: allow-missing -->
- **`device-node.md`**: the kind, `port` and `speed`, a sixth group in the registry's order,
  published by a thread after boot, and **what `vendor` and `device` mean now**: a PCI function's
  or a USB device's IDs, and `0xFFFF` for neither.
- **`drivers-and-irps.md`**: the hub thread's waits.
- **`device-manager.md`**: the `usb-<id>` names.
- **The root `CLAUDE.md`**: `test-qemu`'s USB devices.

## Part B in detail *(2026-10-03)*

### What exists, and what is missing (checked 2026-10-03)

- **Part A enumerates and stops.** Each device is addressed, described, matched against the class
  table and registered as a `UsbDevice`. Nothing sets a configuration, and no endpoint but the
  default one exists. `desc::class_match` reports the first matching interface only.
- **The xHCI driver knows one kind of transfer**, the control transfer's three stages, and the
  commands that touch the default endpoint: Address Device, Evaluate Context, Reset Endpoint and
  Set TR Dequeue Pointer. There is no Configure Endpoint and no Normal TRB, and the DPC routes a
  Transfer Event only to the hub thread's one wait, on the default endpoint (`on_event` in
  `kernel/src/drivers/xhci/mod.rs`).
- **The input path above the kernel needs nothing new.** `input-server` takes up to eight
  devices of any origin from the device manager; the compositor merges them and repeats a held
  key in software (`REPEAT_DELAY_NS`), so a USB keyboard, which sends no typematic repeat, repeats
  like a PS/2 one. Keycodes are evdev's numbering, and translation is the kernel driver's
  (`input-subsystem.md` §4).
- **What a raw input node is lives inside the PS/2 driver**: the event ring that drops whole
  records and announces it, the one parked read, the DPC's hand-off of a finished read and its
  reclaim in thread context (`kernel/src/drivers/ps2/mod.rs`, `ring.rs`). The hand-off was a
  use-after-free once (PR #178 review). A second producer must share it, not copy it.
- **Served indices are the i8042's own**: the keyboard 0 and the mouse 1. On a machine without an
  i8042 there are no input nodes at all.
- **Caps Lock and Num Lock do nothing, and the keypad types nothing.** `libinput` knows Shift, Ctrl,
  Alt and Meta. Its US keymap was written in M3 Part C2 as the least a terminal needed, and the
  keypad was left out without a record. Keypad Enter was patched into `libterm`'s encoder alone (PR
  #191), so it is Enter in `nxterm` and nowhere else: about seventy references to `KEY_ENTER` across
  `libui`, the shell, the greeter and the applications never see it. No keyboard light is driven,
  and a char node takes reads only — `sys_io_submit` refuses a write to one.
- **`modifiers` is compared exactly.** The compositor matches a hotkey with `h.mods == modifiers`,
  so a lock carried as a modifier bit would break every chord whenever Num Lock was on.
- **The hardware report turns its pages on the PS/2 driver's key count** (`kernel/src/report.rs`).
  A USB keyboard does not reach it.
- **Taking the controller from the firmware ended the firmware's PS/2 emulation of a USB
  keyboard.** Since Part A, a machine whose only keyboard is USB has none once the kernel is up —
  the greeter included — until Part B. The laptop's keyboard is a real i8042, so it was not hit.

**The spike** (2026-10-03, the release image of `06b4707`, local QEMU 11): `-machine
i8042=off` beside `q35`, `nec-usb-xhci`, `usb-kbd` and `usb-mouse`. The FADT said `8042 absent`,
the PS/2 driver published nothing, both devices enumerated as HID boot devices, `input-server`
served with **0 devices**, and the greeter came up. So the machine Part B's gates need boots
today, with no input. QEMU 8.2, CI's, takes `i8042=off` on q35 too.

### The shape

**The binding is a step of enumeration.** After a device's class match, the hub thread binds
**every interface whose triple is a boot keyboard (`03/01/01`) or a boot mouse (`03/01/02`)**, not
only the first: a wireless receiver has one of each. Each bound interface becomes one input node.
A HID interface without the boot subclass is not bound: report protocol for anything but a mouse
is deferred (*Not in Part B*). In order:
1. **The endpoints.** Each bound interface's interrupt-IN endpoint, read from the configuration
   descriptor after the interface (a HID descriptor sits between them, and a SuperSpeed endpoint's
   companion follows it). **Configure Endpoint** adds them all in one command: the slot context's
   entry count raised to the highest Device Context Index, and per endpoint the type *Interrupt
   IN*, three retries, its maximum packet, its interval, a transfer ring, **its Max ESIT Payload**
   — the maximum packet times the burst, plus one — and **an Average TRB Length** of the TRB it is
   given, the maximum packet. QEMU reads only the packet size and the interval
   (`hcd-xhci.c`), so no gate can tell a wrong value in the other two; they are checked against
   xHCI 1.2 §6.2.3 when built, and the laptop is where a wrong one would show.
   - **The interval is encoded by speed**, as the endpoint context's Interval field asks: at full
     and low speed `bInterval` is in milliseconds, so the exponent of 125 µs is three more than
     ⌊log₂ `bInterval`⌋, clamped to 3–10; at high speed and above it is `bInterval` − 1. QEMU's
     devices give 10 at full speed and 7 at high speed, and both encode to 6: 8 ms.
   - The Device Context Index is twice the endpoint number, plus one for IN.
2. **`SET_CONFIGURATION`** with the descriptor's `bConfigurationValue`, after Configure Endpoint,
   which is Linux's order: the controller has accepted the bandwidth before the device is told.
3. **A mouse's report descriptor** (B.3): `GET_DESCRIPTOR` of type Report to the interface, for
   the length its HID descriptor gives, and parsed (below).
4. **`SET_PROTOCOL`**: boot (`0`) for a keyboard, and for a mouse whose descriptor does not describe
   a plain mouse; report (`1`) for one whose does. A keyboard that refuses boot protocol is not
   bound, since its report-protocol reports may be anything. A mouse that refuses report protocol
   is already in it — a device's state after a reset — so that refusal is passed over.
5. **`SET_IDLE` (`0`) to keyboards**, so a held key sends no reports until something changes.
   Harmless if refused: an unchanged report decodes to nothing. A stall is recovered and passed
   over, as a string's is.
6. **One Normal TRB per endpoint**, for the endpoint's maximum packet, Interrupt On Completion and
   Interrupt on Short Packet, into a report buffer, and the endpoint's doorbell.

A mouse whose report descriptor cannot be read falls back to boot protocol, and the refusals above
are passed over. **A failure in steps 1 or 2**, which are the device's, ends its binding, logged:
the device stays enumerated and registered, with no input node, and its slot stays. **A failure in
steps 3 to 6**, which are an interface's, ends that interface's alone: its endpoint is configured
and never polled, and the device's other interfaces go on, so a receiver whose mouse refuses keeps
its keyboard. The new control request is the first with no
data stage, so the hub thread gains `control_out` beside `control_in`.

**Polling is the DPC's.** A Transfer Event for an endpoint other than the default one goes to a
table of bound endpoints, by slot and Device Context Index. On success or Short Packet:
- the report's length is the request less the residual;
- it is decoded against the endpoint's previous report into events, stamped as the DPC takes the
  event, at the MSI's interrupt tail, in a report already quantised to the endpoint's interval;
- the events go to the node's ring, a parked read is completed as PS/2's is, and the same TRB is
  queued again with a doorbell.

**QEMU's device NAKs an IN token when it has nothing to report** (`hw/usb/dev-hid.c`), so its
controller completes the TRB only when there is input; the laptop's device answers on the
interval. The driver re-queues on every completion and depends on neither.

**Any other completion halts the endpoint.** The DPC cannot issue a command and wait, so it marks
the endpoint and wakes the hub thread, which resets it — Reset Endpoint, then Set TR Dequeue
Pointer — and queues its TRB again. A third halt in a device's life leaves it stopped, logged.
**No `SYN_DROPPED` follows a recovery.** A HID report is the device's state, so the first report
after it, decoded against the last one before, delivers whatever changed unseen. `SYN_DROPPED`
would instead make the interpreter forget every held modifier, and a key held across the recovery
sends no new event: a transfer error with Shift held would type lowercase until Shift was pressed
again (PR #357 review, probed on today's interpreter). A drop the ring itself announces is
PS/2's mechanism, unchanged.

**The reports.** HID 1.11 Appendix B:
- **A keyboard's eight bytes** are a modifier bitmap, a reserved byte and six key usages. Against
  the previous report, released keys come first, then pressed ones, then `SYN_REPORT`. The eight
  modifier bits are the eight modifier keys. **A report of `ErrorRollOver`** (usage `0x01` in every
  slot) means more keys are down than the device can say, and is ignored, keeping the previous
  state: decoding it would release every held key. A usage is translated by **a 256-entry table
  beside the scancode table**, to evdev keycodes — Linux's `hid_keyboard[]` is the reference, and
  a usage with no keycode is dropped.
- **A mouse in report protocol** (B.3) is read by what its report descriptor says. A small parser
  — short items only, bounded by the descriptor's length — walks the first application collection
  whose usage is *Mouse* or *Pointer*, and finds in one input report the buttons, X and Y
  (relative), and the wheel (Generic Desktop `0x38`) if there is one: each field's bit offset, size
  and sign, and the report ID that prefixes the report when the descriptor uses IDs. Buttons 1–3
  become `BTN_LEFT`, `BTN_RIGHT` and `BTN_MIDDLE` on change, then `REL_X`, `REL_Y` and `REL_WHEEL`
  when non-zero, then `SYN_REPORT`. **Axes of 12 and 16 bits**, common on modern mice in report
  protocol, decode as what they are. HID's Y is positive downward, as `REL_Y` is. **HID's wheel is
  positive away from the user and `REL_WHEEL` is not**: this system's is positive toward the user,
  the screen's direction, deliberately not Linux's (`kernel/src/libkern/input.rs`,
  `rsproto-input-ops.md`). So the USB decoder negates the wheel, where PS/2's wire already agrees
  and passes it through. QEMU shows both: one `wheel-down` is `+1` on the PS/2 wire and `-1` in the
  HID report.
- **A mouse whose descriptor does not parse, or describes something else**, runs in boot protocol:
  its first three bytes are buttons, X and Y, decoded as above with no wheel, and what follows them
  is the device's own. Which a mouse got is logged, so the laptop's report says.
- **Buttons past the third, and a horizontal wheel** (Consumer *AC Pan*), are found and not
  emitted: nothing above the kernel carries them yet.
- **QEMU's `usb-mouse`** describes five buttons, then X, Y and the wheel as 8-bit relative fields,
  with no report ID (`qemu_mouse_hid_report_descriptor` in `hw/usb/dev-hid.c`), and sends those four
  bytes whatever the protocol (`hw/input/hid.c`). So its wheel arrives through the parser.

**The node.** The ring, the parked read, the DPC's hand-off and the reclaim move out of the PS/2
driver into **`drivers::input`**, which both producers use. PS/2 keeps one lock over its two nodes,
since they share a controller, and each USB node has its own. `sched::reap_pending` reclaims every
input node's finished reads, not the PS/2 driver's alone. **The key count moves there too**, so
the hardware report turns a page on a key from any keyboard and drains every keyboard's ring when
it ends. **So does the report's presence check.** Today it asks whether an i8042 keyboard answered
(`ps2::keyboard_present`), and on a machine without one it holds no page at all: "no keyboard
answered … not holding" (PR #357 review, booted on `i8042=off`). It becomes "a keyboard node
exists", which a USB keyboard bound in the first round satisfies, since the report runs after
`drivers::settle`.

**The served index is the next after every input node's.** The i8042 keeps 0 and 1; a USB
keyboard and mouse on the laptop are 2 and 3, and on a machine without an i8042 they are 0 and 1.
A count of input nodes would collide when the i8042 has a mouse and no keyboard. **Indices are
never reused**, which Part C relies on.

**The records.** A USB node is a `Keyboard` or `Mouse` record, served at its index, whose parent is
its `UsbDevice` record and whose driver is `usb-hid`. Its name is the kind word, as the i8042's
are; the device's own name is its parent's. The registry's sixth group becomes "the USB devices
and their keyboards and mice", each node after its device. `device-mgr` needs no change: a
keyboard is `input`'s, by kind, and named `input-<n>` by its served index.

**Departure.** Part C retires a node. Until then the hub thread removes a departing device's
endpoints from the DPC's table, under its lock, **before Disable Slot**, so the DPC cannot touch a
report buffer that `release` is about to free. The node stays, with no producer; a parked read on it
waits for a report that will not come. **A lights write to it completes at once with `PeerClosed`**,
the departure's error (§ *Departure*), so `input-server`, which writes to every keyboard it holds,
is never left waiting on one that left. The rings and report buffers are part of the device's
memory, so a slot that does not disable keeps them, as it keeps the rest.

**Which device a key came from is not tracked.** `input-server` merges by time, and the
compositor's modifier state is the merged stream's: Shift on the laptop's keyboard shifts a key on
a USB one, as it would on Linux.

**The lock keys and the keypad** (B.5), for both keyboards:
- **Caps Lock and Num Lock are locks.** A press toggles one in `libinput`'s interpreter, and the
  compositor holds the state. **A held lock key toggles once**: the interpreter tracks it as down
  until its release. A PS/2 keyboard's typematic repeat arrives as further presses with no release
  between, since the driver emits press and release only, and the interpreter remembers held keys
  for the eight modifiers alone; without that, holding Caps Lock past the typematic delay would
  flip it at the repeat rate (PR #357 review). **`SYN_DROPPED` clears no lock**: a lock is not a
  held key, and its light goes on showing it. **It reaches a client in `KeyEvent`'s reserved
  field, renamed `locks`**, not in `modifiers`: the field is spare, the layout does not change, and
  every exact comparison of `modifiers` keeps working. Caps Lock gives a letter its other case,
  which Shift then reverses, and touches nothing else.
- **Num Lock is on at boot**, so the keypad types digits from the start. `unverified:` a laptop
  with no numpad may have an embedded keypad overlaid on its letters that follows the state `0xED`
  sets, which would make Num Lock on turn letters into digits there. The target laptop has a
  numpad; such a machine would want the default to be a setting.
- **The keypad**, in `libinput`'s keymap and its interpreter:
  - with Num Lock on, its digits and `.` are text, as the main keys' are;
  - with it off, the interpreter delivers 7, 8, 9, 4, 6, 1, 2, 3, 0 and `.` as Home, Up, Page Up,
    Left, Right, End, Down, Page Down, Insert and Delete, so every client that acts on those
    keycodes takes them unchanged; 5 delivers nothing;
  - `/`, `*`, `-` and `+` are text whatever Num Lock says;
  - **keypad Enter is delivered as Enter**, always, which every check for `KEY_ENTER` then sees.
    `libterm`'s special case goes, and with it its test that the keymap has no keypad Enter;
  - **a release is the press's keycode**, so a key held while Num Lock changes releases what it
    pressed.
- **The lights.** Each change sends the lock state to `input-server` as a new request on the
  compositor's consumer channel, **`Lights` (`0x0A01`)**. The server writes it to every keyboard it
  holds, and to each that arrives, so a keyboard plugged in later shows the state at once.
  - **A raw input node takes a one-byte write**: the lights in HID's order, bit 0 Num Lock, bit 1
    Caps Lock and bit 2 Scroll Lock, which is never set. It goes through a new `submit_write`
    beside `CharBackend`'s `submit_read`, and completes when the keyboard has acknowledged it.
  - **PS/2** sends `0xED` and the mask in the i8042's order, each answered by `0xFA` through the
    same byte stream as keys. **While a command is in flight, `0xFA` and `0xFE` are taken before
    the decoder's state machine**, as the command's answers: `0xFE` asks for the byte again, a
    bounded number of times. Outside a command they reach the decoder as today, which already
    turns them into silence. **The hazard is an answer between an `E0` prefix and its code**: the
    decoder would take it as the prefix's code, so `E0 FA 48`, Up with an acknowledgement in the
    middle, would be keypad 8 pressed and never released, and with Num Lock on the compositor's
    repeat would type `8` until another key (PR #357 review, probed on today's decoder). It needs a
    light to change while an `E0` key is in flight, such as an arrow held when Caps Lock is
    pressed. A keyboard that does not answer within a bound fails the write, logged. On the laptop
    the embedded controller lights the key.
  - **USB** sends `SET_REPORT`, a one-byte output report to the keyboard's interface, from the hub
    thread, which owns the default endpoint: the write leaves the mask and wakes it. Writes faster
    than the thread are coalesced to the latest mask. **A stall** recovers the default endpoint, as
    a string's does, and fails the write. **A timeout** leaves the request on the default
    endpoint's ring, which Part A answers only by abandoning a device, so the device takes no more
    lights — its keys go on arriving, on their own endpoint — and the write fails, logged.
- **Scroll Lock is not a lock**, and its light stays off.

### The maintainer's calls, 2026-10-03

- **The mouse wheel is in**, if it is not much more work: a mouse's report descriptor, with boot
  protocol as the fallback.
- **Caps Lock's light is in**, and with it Caps Lock and Num Lock as locks and the keypad mapped:
  the laptop's keyboard has a numpad. Scroll Lock is dropped.
- **Two-finger scrolling on the trackpad is later**, with the trackpad's native interface.

### Calls made in this pass, without the maintainer

- **Every boot interface is bound**, not only the first, so a combined receiver gives both a
  keyboard and a mouse.
- **The shared input node is extracted from PS/2**, not copied, because the hand-off it carries
  was a use-after-free once.
- **The served index is the next free one**, so an i8042-less machine's first USB keyboard is
  `input-0`.
- **The wheel comes from the descriptor**, not from a fourth byte, which boot protocol does not
  define and real mice fill differently.
- **No horizontal wheel or extra buttons**: the parser finds them, and nothing above the kernel
  carries them.
- **`SET_IDLE (0)` to keyboards only.** Mice report on change anyway, and some stall it.
- **The hardware report counts USB key presses**, since a desktop whose only keyboard is USB
  would otherwise be unable to turn its pages.
- **Locks travel in `KeyEvent`'s spare field**, not as modifier bits, since the compositor compares
  modifiers exactly.
- **Num Lock is on at boot.**
- **Keypad Enter is delivered as Enter**, and with Num Lock off the keypad's navigation keys as the
  keys they stand for, so no client has to learn the keypad.
- **A light's toggle is gated on PS/2 only.** QEMU traces a PS/2 keyboard's lights and not a USB
  one's, so a USB keyboard is held to acknowledging the lights it is given at start — the request a
  toggle sends.

### Pieces

- **B.1 The shared input node.** `drivers::input`: the ring, the parked read, the hand-off, the
  reclaim and the key count, moved out of `drivers::ps2` with their tests. No behaviour changes:
  `check-input`, with and without `--no-ps2-irq`, and `check-fbcon`, `check-report` and
  `check-login` hold PS/2 as they do today. **Built 2026-10-05.** The hand-off, which had no host
  test while it was PS/2's, has four now, each failing its control; `input.yml`'s path filter gained
  `drivers/input/**` with it, since the ring left `drivers/ps2/**`.
- **B.2 Endpoints and the binding.** Configure Endpoint and Normal TRBs; the endpoint and
  companion descriptors; `control_out`; the binding steps; the DPC's endpoint table and the
  re-queue; the halt recovery; the boot report decoders and the usage table; the nodes, their
  served indices and records; departure removing endpoints first; the hardware report's presence
  check and key count.
- **B.3 The wheel.** `GET_DESCRIPTOR` of a report descriptor, the parser, report protocol for a
  mouse it describes, the decoder by its fields, and the boot fallback.
- **B.4 The gates.** `check-input --usb`, `check-login --usb` and `check-report --usb`, below, and
  CI's jobs for them.
- **B.2–B.4 built 2026-10-05**, together, since B.4's gates were what showed B.2 and B.3 working.
  Calls on the way:
  - **Report decoding is `drivers::hid`, bus-neutral**, so the trackpad's I²C-HID could use it;
  - the mouse decoder takes a **layout**, boot protocol's being one, so B.3 adds only the parser;
  - **`input-server`'s device count is asserted as a bound, not a value**: under TCG the manager
    read the registry after the hot-plugged keyboard was bound, and was handed five, not four;
  - `check-report --usb` asserts the FADT's 8042 absent and a first round of three, where the
    i8042 run asserts present and one.
- **B.5 Lock keys and the keypad.** The interpreter's locks and keypad; `KeyEvent.locks`; the
  keymap's keypad and Caps Lock; `Lights` and `input-server`'s fan-out; `submit_write`; PS/2's
  `0xED` exchange, its answers taken ahead of the decoder; USB's `SET_REPORT`; `libterm`'s special
  case removed.
- **B.5 built 2026-10-05.** Calls on the way:
  - **`check-terminal`'s Num Lock step is keypad 8, not keypad 4.** `tty-server`'s line discipline
    recognises Left and drops it, so `xy`, Left, `z` shows `xyz` and the step could never pass.
    Keypad 8 with Num Lock off is Up, which the discipline answers by recalling the line before —
    something a digit could not do. The keypad step types `12 + 3` and keypad Enter, and the shell's
    `15` is the proof the Enter was one; the Caps Lock step types a quote with Caps Lock on and a
    shifted letter, so a Caps Lock that touched more than letters fails;
  - **the trace is read exactly**, each step the whole sequence since the kernel's reset, which is
    found by tracing `ps2_reset_keyboard` beside the lights. Num Lock's light is asserted **before
    anything is injected**: the compositor also sends after every input pass whose locks changed,
    so a compositor that skipped its first send passes the same check made after any typing;
  - **the PS/2 exchange ends inside the driver's leaf lock, and is completed and logged outside
    it** — completing takes the scheduler's lock and logging the serial port's, both ranked above
    a leaf. The DPC marks a write *completing* while it does so, so thread context cannot drop it
    first. Caught reading the code, before a boot ran it;
  - `input-server`'s keyboard filter is one comparison in `main.rs`, not a host test: a mouse's
    node refuses the write anyway, `Unsupported`. The request's parse is the lib's, tested;
  - the `SET_REPORT`'s data byte is the node's, unchanged, since HID's order is the wire's — so
    its tests are the lights' (`libinput`) and the node's (`drivers::input`), and the setup bytes
    have their own.
- **B.6 Docs.** Below.

### Gates

- **`check-input --usb`** boots the self-test image on `-machine q35,i8042=off`, with the gates'
  controller, `usb-kbd` and `usb-mouse` — **no PS/2, so a key reaching the client came through USB
  or not at all**. It asserts what `check-input` asserts, **the wheel included**, which comes
  through the report descriptor (B.3), less one step: there is **no `ps2-hold-gate` walk**, which
  holds the i8042's drain, since QEMU queues a USB device's input itself. With B.5 it also asserts
  that the USB keyboard acknowledged the lights it was given at start, Num Lock's.

  The motion sum across a stalled consumer stays: QEMU clamps each report's axes to ±127 and keeps
  the remainder for the next (`hid_pointer_poll`), so the sum is still exact. CI runs it under KVM
  in `input.yml`, whose path filter gains `kernel/src/drivers/xhci/**` and
  `kernel/src/drivers/input/**`.
- **`check-login --usb`**: the same machine, the release image, and a wrong password, a right one
  and a session, all typed on the USB keyboard. In CI's QEMU job under KVM. It is the Definition
  of Done's "types at the greeter".
- **`check-report --usb`**: the live image on the same machine, with the stick as today. Its pages
  turn on USB key presses. Without B.2's presence check the report holds no page and the gate fails
  with "the report did not hold"; without its count a page waits out its 120 s, and the gate, which
  allows 60 s for the next, fails. **Limine's menu takes Down and Enter from `usb-kbd` on
  `i8042=off`** — the firmware's own USB keyboard support, before the kernel takes the controller —
  which the gate depends on (PR #357 review, booted).
- **`check-terminal`**, on the i8042 as today, gains B.5's steps:
  - Caps Lock, then `ab` shows `AB` in the grid; Caps Lock again;
  - keypad `1` and `2` show `12`, since Num Lock is on, and keypad Enter submits the line;
  - Num Lock off, then keypad 4 moves the cursor left: `xy`, keypad 4, `z` shows `xzy`;
  - **QEMU's `ps2_set_ledstate` trace**, written to a file. A boot with no lights code already
    traces `ledstate 0` several times, from the keyboard's resets (PR #357 review), so the
    sequence is matched after the kernel's own reset of the keyboard: Num Lock (`2`), then Caps
    Lock and Num Lock (`6`), then Num Lock (`2`), then none (`0`). Both QEMU versions have the
    event.
- **`test-qemu`** keeps the i8042 on beside its USB keyboard and mouse:
  - each device's binding line, with its node: `usb: port 9: keyboard at /dev/input/raw/2`;
  - `input-server` given **four** devices;
  - `boot-probe` holding the i8042's keyboard and mouse at 0 and 1 and the USB ones at 2 and 3,
    each with its `UsbDevice` parent and driver `usb-hid`;
  - `boot-probe`'s devices check allowing a hot-plugged device's keyboard after the manager's
    read, beside the device itself;
  - the hot-plugged keyboard bound, and its endpoints removed when it leaves.
- **Host tests**, in the kernel crate, `libinput`, `librsproto` and `input-server`:
  - the usage table, including that no usage maps to a keycode outside evdev's range;
  - a keyboard report against its predecessor: a press, a release, both at once, a modifier
    alone, the same report twice, a key moving slots, and `ErrorRollOver`;
  - a boot mouse report: each button, both axes signed, a fourth byte ignored, and a report that
    changes nothing;
  - the report-descriptor parser, on QEMU's descriptor, on one with report IDs, on 12- and 16-bit
    axes, on one with no wheel, on one with no X, and on one that runs past its length — each to
    its field layout or to the boot fallback — and a report decoded by a layout at each width;
  - the interpreter's locks: a press toggles, and a held lock key's typematic **press, press,
    release** toggles once; `SYN_DROPPED` clears no lock; Caps Lock against Shift; the keypad with
    Num Lock on and off; a release after Num Lock changed mid-press; keypad Enter as Enter;
  - a USB keyboard's report after a halt recovery, decoded against the last one, with Shift held
    across it, and no `SYN_DROPPED` pushed;
  - the USB wheel's sign: a report's `-1` is `REL_WHEEL` `+1`, toward the user;
  - `KeyEvent`'s `locks` written and read back;
  - the PS/2 driver's command exchange: `0xFA` taken as the command's while one is in flight, **an
    acknowledgement between `E0` and its code** (`E0 FA 48` is Up), `0xFE` resending, scancodes
    around it, and an acknowledgement that never comes;
  - `SET_REPORT`'s setup bytes **and its data byte** for each lock state, since QEMU accepts any
    byte and traces none, and `input-server`'s fan-out reaching keyboards only;
  - the endpoint and companion descriptors, with a HID descriptor between, from bytes QEMU's
    devices sent, and from bytes that run past the configuration;
  - the interval encoding at each speed, at both ends of its clamp;
  - the Configure Endpoint input context at both entry sizes;
  - the served index with and without the i8042's two;
  - `drivers::input`'s ring and hand-off, moved with their tests.
- **The laptop:** a USB mouse and keyboard on the live stick's session. A step for the maintainer,
  not a gate.

The gate set grows from 36 to **42**: each new gate under TCG and KVM. B.5 adds steps to two
gates, and no gate.

### Not in Part B

- **A device plugged in after the manager's read reaching `input-server`** (Part C). Part B binds
  it and registers its node, and nothing hands the node over.
- **Scroll Lock**, as a lock or a light.
- **Report descriptors for anything but a mouse**: consumer keys, a keyboard's report protocol,
  and any HID device that is not a boot device.
- **A horizontal wheel and buttons past the third**: the parser finds them, and nothing above the
  kernel carries them.
- **Two-finger scrolling on the laptop's trackpad.** It is an ELAN0501 on I²C, which the firmware
  presents on the i8042 as a relative mouse, so no finger reaches the system. Scrolling with two
  fingers needs its native interface — the Designware I²C controller, I²C-HID and HID multitouch —
  and B.3's parser is the first piece of that. Later, as its own item.
- **Absolute pointers**: `usb-tablet`, and the CLAUDE.md note that the guest has a relative pointer
  only stays true.
- **Remote wakeup, suspend, and selective suspend.**

### Docs Part B owes

- **`input-subsystem.md`**: the second producer in §2's diagram and table, the usage table beside
  the scancode table in §4, `drivers::input`, the served index, the locks and the keypad, and the
  lights' write path.
- **`rsproto-input-ops.md`**: `Lights`. **`rsproto-surface-ops.md`**: `KeyEvent.locks`.
- **`usb.md`**: the binding, the endpoints, the DPC's polling, the halt recovery, and departure
  removing endpoints first.
- **`device-node.md`**: the sixth group's keyboards and mice, and the served index rule.
- **`drivers-and-irps.md`**: the DPC's second kind of transfer completion, if it says what the DPC
  completes.
- **Statements Part B makes false** (PR #357 review):
  - `io-operation.md`: a char device accepts a `Read` only, and `Write` is `Unsupported`;
  - `device-node.md`: `Char` "accepts byte-stream Read IoOps";
  - `console-and-tty.md`: "`CharBackend` has only `submit_read`";
  - `boot-flow.md`: the report turning "on an i8042 key press", and holding nothing without one;
  - `qemu-integration-tests.md`: the `--usb` variants, and which gates carry the hold-gate walk.
- **The root `CLAUDE.md`**: the three `--usb` gates, `check-terminal`'s lock steps, and
  `test-qemu`'s bound devices.

## Part C in detail *(2026-10-05)*

**Arrivals and departures.** A device plugged in after boot reaches the owner of its class, and one
unplugged leaves it:
- the registry says a device went;
- the manager follows the registry rather than reading it once;
- the input server takes a device and lets it go;
- a departed keyboard's held keys are released.

### What exists, and what is missing (checked 2026-10-05)

**The kernel:**
- **The table only grows, and never says a device left** (`kernel/src/device.rs`). Each entry is
  an owning reference kept for the boot. A disconnected USB device's record stays and looks
  present, and its paths — `/dev/input/raw/<n>`, `/dev/registry/<id>` — go on resolving to it.
- **Served indices are already never reused.** An input node takes the index after every input
  node's (Part B.2), and a departed one would keep its own.
- **The snapshot has no generation, and its header no room for one.** `RegistryHeader` is magic,
  version, count and record size: sixteen bytes. **A record has one spare byte**, `_pad` at
  offset 39.
- **Nothing tells anyone that the table changed.** The notification queue has no kind for it, and
  **the kernel has no way to name the device manager** to queue one for: a notification goes to a
  process the kernel already has a reason to tell, as a parent is told `ChildExited`. Nor can a
  kernel server leave a lookup waiting: `OpStatus::Pending` belongs to the forwarding arm of
  `sys_ns_lookup` (`kernel/src/syscall/table.rs`) alone.
- **The hub thread's departure** (`depart`, `kernel/src/drivers/xhci/hub.rs`) takes the device's
  HID endpoints out of the DPC's table and disables its slot. It touches no record and no node.
- **A USB input node's state is one of sixteen statics** (`NODES` in
  `kernel/src/drivers/xhci/hid.rs`), taken in order and never given back. Its `CharBackend`
  context is the index.
- **A node's reader** (`drivers::input::Reader`) holds one parked read, and has no state for a
  device that has gone.
- **A char node's `Read` never sees its `offset`** (PR #360 review). `sys_io_submit` passes a char
  backend's `submit_read` the buffer, its offset, the length and the context, and drops `offset`
  (`kernel/src/syscall/table.rs`); `io-operation.md` says a stream ignores it.
- **A departed keyboard's held keys are never released.** The last report decoded holds them
  down, and no report follows it. The compositor repeats a held key until something else stops
  the run.

**Userspace:**
- **`device-mgr` reads the registry once** (`userspace/device-mgr/src/main.rs`), and keeps each
  class device's node by id. It encodes `Departed` and sends none.
- **`input-server` takes arrivals after `Settled`, and retires a slot on `Departed`.** Both are
  host-tested and neither has run.
- **A read that fails is logged and armed again** (`userspace/input-server/src/main.rs`), and
  which line it logs depends on how it fails (PR #360 review). A read that *completes* with an
  error logs `read completed with an error; events lost` in `harvest` and is armed again at the
  top of the loop, so a node whose every read completed `PeerClosed` would be read in a spin. A
  submit *refused* logs `read submit FAILED -- device may stall` and leaves the device out of the
  wait set, and the next pass refuses it again and logs it again.
- **`input-server` closes a node at once on `Departed`**, whatever its ring still holds.
- **Most readers find block devices through `DeviceRecord::block_index`**
  (`userspace/libkern/src/device.rs`): `libsession`, `eshell`'s `lsblk`, the test harness and some
  of `boot-probe`. A departed record that answers `None` there is right for each of those at once.
  **`boot-probe`'s registry test does not** (PR #360 review): it resolves `/dev/registry/<id>`
  for every record, and `/dev/blk` and `/dev/input/raw` at each record's `served`, which a departed
  record would fail on every boot `test-qemu`'s hot-plug leaves one in.
- **`boot-probe` holds the manager to a prefix of the registry** (Part A.3), because the manager
  missed whatever arrived after its one read.
- **`test-qemu`'s hot-plug begins at the first round's line**, before `init` (Part A.2), so the
  keyboard arrives, is swapped for the mouse, and the mouse arrives while userspace is still
  starting: on either side of the manager's first read, as `check_usb_listed` records.

**QEMU** — `hw/input/hid.c`, `hw/usb/dev-hid.c` and `ui/input.c`, read at 8.2.2 and 11.0:
- **A USB keyboard becomes the target of injected keys when it is plugged in.** `hid_init`
  activates its handler, which moves it to the head of the list `input-send-event` searches.
  Unplugged, the next keyboard is the target again.
- **A USB mouse becomes the target of injected motion on its first poll**: `hid_pointer_activate`,
  from its interrupt IN, once.
- **Nothing is sent for a key held when its device goes.** QEMU unregisters the handler, and the
  guest is left with the last report.

### The shape

**A departure is a state of a record, not its removal.** An id is a record's place, and
`/dev/registry/<id>`, `device-mgr`'s `usb-<id>` and an owner's `Departed` all name a device by it.
- **A departed record keeps its fields, its id and its served index**, which no later device takes.
- **It says so.** The spare byte becomes `flags`, and `DEPARTED` (`0x01`) is its first bit. The
  layout is unchanged.
- **Its children depart with it**: every record whose parent chain reaches it. That is a USB
  device's keyboard and mouse, and in Part D a stick's disk and that disk's partitions.
- **Its paths stop resolving.** `/dev/input/raw/<n>`, `/dev/blk/<n>` and `/dev/registry/<id>`
  answer `NotFound`. A handle already held stays valid, and what it does is its driver's (below).
- **`DeviceRecord::block_index` answers `None` for a departed record**, and a new `input_index`
  beside it does the same. A reader that asks either is right without knowing departures exist.

**The table has a generation**, bumped once per change: a registration, or a departure with its
children. **The snapshot carries it.** `REGISTRY_VERSION` becomes 2, and the header 24 bytes, with
`generation: u64` after `record_size`. Every reader parses through `libkern::device::records`,
which is the one place to change; version 1 is refused, as a mismatched version is today.

**The event source is a node to read, not a notification.** `/dev/registry/changes` is a char node.
A `Read` on it **waits until the generation is past the read's `offset`**, then completes with the
current generation, eight bytes. A reader that is behind is answered at once.
- **The offset has to reach it.** A char backend's `submit_read` gains the `offset` (PR #360
  review), which the console, the i8042's nodes and USB's ignore: it is a signature change in
  three backends, and the change node is the first to read the field. Without it the node could
  only mean "the next change", and a keyboard plugged in between the manager's snapshot and its
  read would wait for some other change to be handed over.
- **No change can be missed**: the manager reads a snapshot, then waits past the snapshot's
  generation. A change in between answers the wait at once.
- **It is the model the system already has**: `sys_io_submit`, a `PendingOperation`, and `sys_wait`
  beside whatever else the reader waits on — as `input-server` waits on its devices.
- **Authority is the binding**, as for `/dev/registry`: the root namespace, and nothing else.
- **Four reads may wait at once** — the manager's and `boot-probe`'s with room to spare — and a
  fifth is refused, `WouldBlock`.
- **They are answered from the thread that changed the table**, after its lock is let go:
  completing takes the scheduler's lock.

**Why not the notification the scoping planned** (§ *The event source*): the kernel cannot name the
manager. Telling it would need the manager to register — "notify me" — which is a watch by
another name, and a notification kind besides, which is ABI. A lookup left waiting until the next
change was weighed too: no kernel server can leave a lookup pending today, and the node needs no
new path through `sys_ns_lookup`. **The maintainer agreed to the node**, below.

**A USB device departs in the hub thread**, in this order:
1. **What each bound HID endpoint holds is released**, and the events pushed to its node, then
   `SYN_REPORT`. **A keyboard's last report is decoded against an empty one**: its keys, then its
   modifiers. **A mouse's held buttons are released from its decoder's `buttons`**, not by decoding
   an empty report, which for a layout with a report ID would not carry the ID and decode to
   nothing (PR #360 review). A drag in progress ends.
2. **Its endpoints leave the DPC's table**, as `unbind` does now.
3. **Its nodes retire.** A read already parked is answered with the releases, or `PeerClosed` if
   there were none to send. **A read submitted after that drains what the ring holds, and once it
   is empty is refused at submission, `PeerClosed`**, with no `PendingOperation`: a reader learns
   the device has gone at once, and cannot spin on reads that each complete with an error. A
   lights write still waiting is completed `PeerClosed`, as a new one is refused.
4. **Its records depart**: the device, its keyboards and its mice, as one generation. The reads
   waiting on `/dev/registry/changes` are answered.
5. **Its slot is disabled and its memory freed**, as now.

**A USB node's static slot is given back** once its node has retired and its last read has been
dropped in thread context. **A slot carries an epoch**, in its `CharBackend` context beside the
index, so a handle to a node retired from a slot that has since been reused is refused
`PeerClosed`, rather than served the next device's ring. The sixteen then bound the devices
*attached*, not every device ever attached.

**`device-mgr` follows the table:**
- after its first read it keeps a read waiting on `/dev/registry/changes`, at the snapshot's
  generation;
- when that completes, it reads the snapshot again and **diffs it against the records it holds**.
  A record that is new and present is an `Arrived` to its class's owner, carrying a duplicate of
  its node. A record that was present and has departed is a `Departed`, and the manager closes its
  own handle to the node. **A record that arrived and departed between two reads is told to no
  one**;
- a class with no owner yet keeps its arrivals for the replay, which leaves departed records out;
- `/dev/devices` lists present devices, and a departed device's `usb-<id>.tsm` is gone.

The diff is a function of two lists of records, and host-tested as one.

**`input-server`:**
- **`PeerClosed` is its device leaving**, whether a read completes with it or a submit is refused
  with it. The device is not armed again, and `<kind> <id> left` is logged once — not `read
  completed with an error` and not `read submit FAILED`;
- **`Departed` retires a slot only once its node has answered `PeerClosed`.** Until then the slot
  keeps being read, so the releases still in its ring are delivered whatever order the manager's
  message and the node's reads arrive in (PR #360 review: a mouse unplugged mid-drag, more than a
  harvest's thirty-two events behind, would otherwise lose its button's release). A slot whose
  node has already answered retires at once. A small state machine, host-tested in the library;
- a device arriving after `Settled` takes a free slot and is written the lights (Part B.5).

**Nothing changes in the compositor**: the releases are ordinary events, and a held key's release
stops its repeat as any release does.

### The maintainer's calls, 2026-10-05

- **The event source is the node**, `/dev/registry/changes`, not the notification the scoping
  planned. The node changes no ABI; the notification would have changed the hash, and needed a
  registration as well.
- **Departed records stay for the boot.** A replug costs a record per node — two for a keyboard or
  a mouse, its device and its input node — at 144 bytes each in every snapshot after it, and
  dropping records would break "an id is its place". Recorded under `usb-departed-records`, with
  the snapshot's size as the trigger.

### Calls made in this pass, without the maintainer

- **A departed path answers `NotFound`**, not a node that fails every request: the path names
  something that is no longer there, and a holder of the old handle learns it from the handle.
- **The releases come from the driver.** The driver holds the last report, which is the only place
  a device's held keys are known. `input-server` merges the devices into one stream, and the
  compositor's interpreter sees only that stream, so neither can tell which keys were the
  departed device's.
- **Slots are recycled, with an epoch**, rather than allocated per binding: the DPC stays free of
  allocation, and a stale handle cannot reach a new device.
- **The generation goes in the snapshot, not only in the change node's answer**, so a snapshot says
  which state of the table it is.

### Pieces

- **C.1 The registry.** `flags` and `DEPARTED`; the generation and the version 2 header; a
  departure with its children; departed paths answering `NotFound`; `block_index` and
  `input_index`; `/dev/registry/changes`, and the `offset` reaching a char backend's
  `submit_read`. Host tests: a departure marks every descendant and only them; a departed served
  index is never reissued; the snapshot marks rather than omits; each path refuses a departed
  device; the generation counts every change; a waiting read is answered by the change past its
  offset and not before, and at once when behind.
- **C.2 USB departure.** The releases, the nodes retired, the records departed, the slots
  recycled with epochs. Host tests:
  - a keyboard's last report against an empty one releases every key and then every modifier;
  - a mouse's held buttons are released from its decoder's state, **with a layout that has a report
    ID** — QEMU's mouse has none, so no gate holds that case;
  - a retired reader is answered with what its ring holds, and a read after that is refused at
    submission, `PeerClosed`;
  - a handle whose epoch is stale is refused.
- **C.3 The manager follows.** The waiting read, the diff, the owners and the tables. Host tests on
  the diff: an arrival, a departure, both in one read, a record that came and went unseen, and a
  departure for a class with no owner.
- **C.4 `input-server`.** `PeerClosed` as a departure, whether a read completes with it or a
  submit is refused with it; `Departed` retiring a slot only once its node has answered. The
  slot's states are the library's, host-tested: a `Departed` before the node's `PeerClosed`, and
  after it.
- **`boot-probe`** (with C.1): its registry test holds a present record as now, and a departed
  one to its paths answering `NotFound` — `/dev/registry/<id>`, and `/dev/input/raw` or
  `/dev/blk` at its served index.
- **C.5 The gates.** Below.
- **C.6 Docs.** Below.
- **C.1–C.6 built 2026-10-05.** Calls on the way:
  - **`MemoryObject::copy_in`** writes a read's bytes into its buffer, replacing the console's and
    the input nodes' copies of the same helper, since the change node would have been a third;
  - **a refused or failed change read stops the manager following**, logged, rather than
    retrying: it keeps the table it has and serves it;
  - **`test-qemu`'s devices check cannot see a manager that never follows** when the hot-plug has
    come and gone before the manager's one read — the control passed it — so `check-input --usb`,
    which orders its own plugs, is the gate that holds following, and fails the same control;
  - **the first host test of a departed index never reissued could not fail**: the departed index
    sat below a present one, so a reissue would have taken the next anyway. It departs the device
    with the highest index now, the case a reissue shows in, and fails its control.

### Gates

- **`check-input --usb` gains its last steps**:
  1. **The boot keyboard unplugged** over QMP: the manager sends `Departed`, and `input-server`
     retires its slot.
  2. **A keyboard plugged in**, which is then the only one: the manager sends `Arrived`,
     `input-server` reads it, its lights are acknowledged, and a key typed on it reaches the test
     client's window.
  3. **A key held down on it** — `input-send-event`, down only — is seen pressed by the window; the
     keyboard is unplugged; and **the window sees the release.** The slot is retired.
  4. **`input-server` logged one `left` line for each keyboard unplugged, and neither `read
     completed with an error` nor `read submit FAILED`.** Today's `input-server` prints the first
     for a read that completes `PeerClosed` and the second at each pass for a refused submit, so a
     regression to arming a departed node again prints one of them (PR #360 review: asserting on
     the failed submit alone could not see the spin).

  The order matters: the boot keyboard leaving first makes every key in step 2 the new
  keyboard's, since there is no other.
- **`test-qemu`'s hot-plug stays where it is**, before `init`: it is Part A's enumeration test, and
  moving it after `input-server` settles would race `boot-probe`'s verdict, which ends the run. So
  it lands on either side of the manager's first read, and a keyboard swapped out before that read
  is rightly told to no one (PR #360 review). What `test-qemu` holds is what holds wherever it
  lands: **`boot-probe` holds the manager to the registry's present records** — waiting, within a
  bound, until the manager has caught up — where it held it to a prefix; and a departed record's
  paths answer `NotFound`. A hot-plugged keyboard's `Arrived` and `Departed` are `check-input
  --usb`'s to hold, where the gate decides the order.
- **Controls**, planned:
  - a departure that does not bump the generation: step 1 times out;
  - no releases: step 3's release never arrives;
  - `input-server` arming a `PeerClosed` node again: step 4 fails, on `read submit FAILED`;
  - `Departed` retiring a slot before its node has answered: its host test fails;
  - an `Arrived` sent for a departed record, and the epoch check removed: their host tests fail.

The gate set stays at 42: these are steps in gates that exist.

### Not in Part C

- **Disks arriving and departing** (Part D). The registry's departure carries them, and what the
  storage service does with one is Part D's.
- **Compacting the table**, which the second call above declines.
- **External hubs**, out of the phase.
- **Suspend and resume.**

### Docs Part C owes

- **`device-node.md`**: `flags` and `DEPARTED`, the generation and the version 2 header, departed
  paths, `/dev/registry/changes`, served indices never reused, and the `offset` a char backend's
  `submit_read` now takes.
- **`io-operation.md`**: the change node's `Read`, and what its `offset` means — the one char node
  that reads the field.
- **`namespace-and-resource-servers.md`**: the registry server's leaves, which gain `changes`.
- **`device-manager.md`**: the manager following the table and the diff, and §9's gap closed.
- **`rsproto-devices-ops.md`**: its Status — `Departed` is sent, and so are later arrivals.
- **`input-subsystem.md`**: the hotplug source exists, a departure releases what was held, and
  `input-server` takes `PeerClosed` as a departure.
- **`usb.md`**: a departure's order, a node retiring, slots given back.
- **`deferred-decisions.md`**: `usb-departed-records`, with records retired in place and nodes
  given back, and the table's growth what remains.
- **`qemu-integration-tests.md` and the root `CLAUDE.md`**: `check-input --usb`'s new steps and
  `test-qemu`'s.

## Part D in detail *(2026-10-06)*

**Mass storage.** A USB stick plugged in before boot or after it becomes a disk:
- its partition table — GPT, MBR, or none — is read where it arrives;
- the storage service mounts what it holds, and lets it go when it leaves;
- the stick a live boot started from is passed over, and `nxinstall` refuses it.

### What exists, and what is missing (checked 2026-10-06)

**The kernel:**
- **The class table already knows bulk-only storage** (`08/06/50`, `desc::Match::BulkOnlyStorage`)
  and logs it. Nothing binds it. QEMU's stick enumerates in `test-qemu`, and as the live image's
  stick in `check-live`, `check-install`, `check-report` and `check-storage`.
- **Only interrupt-IN endpoints are configured** (`context::configure_endpoints`). Every transfer
  event for an endpoint other than the default one goes to `hid::on_transfer`
  (`kernel/src/drivers/xhci/mod.rs`).
- **`hid::bind` configures the device itself**: one Configure Endpoint for its HID endpoints, then
  `SET_CONFIGURATION`. A second class binding the same way would send a second
  `SET_CONFIGURATION`, which resets every endpoint the configuration has (USB 2.0 §9.1.1.5) —
  the keyboard's included.
- **The hub thread's waits share one record.** `Xhci::waiting` holds the one command or
  default-endpoint transfer the thread waits for, and the DPC completes it. Nothing else can wait
  on the controller.
- **A block device is a `BlockBackend`** (`kernel/src/io/block.rs`). Its `submit`, from any CPU,
  must not block, and the IRP completes through its DPC. AHCI keeps one command in flight and up
  to 32 queued behind it. `max_frags` bounds one transfer, and a larger one is refused rather than
  split (`TODO(block-transfer-split)`). The largest any client submits is `nxinstall`'s 256 KiB.
- **Partition tables are read once, at boot, polled** (`kernel/src/drivers/gpt.rs`). `gpt::init`
  reads through `read_blocking`, which calls the backend's `poll` with interrupts masked. It reads
  GPT only, assumes 512-byte sectors, and does not keep the disk's own GUID.
- **A disk's parent is found by its PCI address** (`Registry::add_block`), and a partition's by its
  disk's node.
- **`/dev/disk/by-partlabel` and `by-partuuid` are bound once**, into `init`'s namespace, from every
  partition `gpt::init` recorded (`bind_partition_names`). That happens after the hub thread's first
  round, so a stick's partitions read there would be bound too.
- **Limine's module records are bound as revision, address, size and path**
  (`kernel/src/limine.rs`). The media fields after them are not read.
- **A failed page-cache fill ends the faulting process** (`kernel/src/arch/x86_64/idt.rs`), as a
  fault on nothing does.

**Userspace:**
- **The storage service reads every device synchronously**, in its one loop: `DeviceIo::read_at`
  waits on each read with no deadline. A device that never answered would stop the service.
- **Arrivals and departures after `Settled` are logged and ignored** (`serve_subscription`).
- **`fs-server-ext4` answers `IoError` when its device fails, and runs on.** It exits on
  `Meta::Unmount` or a failed setup. The service notices a server's exit by its control channel
  closing.
- **The service passes over the installer's source** by its partition name, auto-mounts read-only
  on a live boot, and mounts at most eight filesystems (`MAX_MOUNTS`).
- **`InUse` names every mounted device and its disk**, and the `disks` grant withholds them.
  `nxinstall` says why a disk is withheld from `/dev/devices` and `/dev/storage`: the running
  system if `init` mounted on it, else the command that unmounts it.
- **`/dev/devices`' columns**: `name`, `kind`, `path`, `size`, `description`, `parent`, `driver`.

**QEMU** — measured for this pass, at 8.2.2 (in `ubuntu:24.04`, CI's) and 11.0.2:
- **`blockdev-add` then `device_add usb-storage,bus=xhci.0,drive=<node>`** plugs a stick into a
  running machine. **`device_del` unplugs it and the node survives**, so the same image plugs in
  again. A `-drive if=none` drive would be deleted with its device.
- `usb-storage` sets INQUIRY's removable bit only with `removable=on`. Removable means "behind USB
  mass storage" in this plan (§ *Removable media*), so nothing reads that bit.

**Limine** — measured by printing every module's record, on a live-stick boot and a disk boot:
- **Each record names the volume it was loaded from**: `media_type` 0, `partition_index` 1, and
  **the GPT disk GUID, in the byte order the GPT header stores it**. On the stick boot that was the
  stick's `54E3387F-…`, and on the disk boot the disk's `5BC793C1-…`, as `sgdisk -p` reads each. So
  the boot disk is the one whose header's GUID equals the bytes in module 0's record.

### The shape

**A bulk-only interface binds as a disk**, in the hub thread, during enumeration:
1. **One configuration per device.** HID and storage interfaces are gathered first. One Configure
   Endpoint adds every endpoint they bind, and one `SET_CONFIGURATION` follows. Then each class's
   own requests run. `hid::bind`'s device steps move to the hub.
2. **The bulk endpoints**: a ring each, bulk endpoint contexts with the maximum packet and a
   SuperSpeed companion's burst, and a page for the command and status wrappers.
3. **`GET MAX LUN`** (a stall means one). For each logical unit:
   - `INQUIRY`: a direct-access device, or the unit is left alone;
   - `TEST UNIT READY`, with `REQUEST SENSE` between tries, for up to five seconds: a stick still
     becoming ready, or one reporting the unit attention its reset raised;
   - `READ CAPACITY(10)`.

   A unit that never becomes ready is logged and not published: an empty card reader's slot, say.
4. **A `Disk` record under the `UsbDevice` record.** It is named by INQUIRY's vendor and product
   and the device's serial string, with the block size and count `READ CAPACITY` gives. A unit of
   2 TiB or more, which `READ CAPACITY(10)` cannot describe, is logged and not published.
5. **Its partition table is read there and then** (below), and its GPT disk GUID compared with the
   boot disk's.

**The I/O path is the DPC's**, as AHCI's is its interrupt's:
- **`submit` queues the IRP behind the one in flight** and starts it if none is. Bulk-only runs one
  command at a time, and a device's units share that one.
- **A command goes on the rings whole.** The command wrapper goes on bulk OUT. The data follows
  as one Normal TRB per IRP fragment, on OUT for a write and IN for a read. The status wrapper
  goes on bulk IN, and only its completion interrupts. One doorbell rings for each endpoint used.
- **The DPC completes the IRP at the status wrapper's event**, and starts the next. The wrapper's
  signature, tag and status are checked, and its residue must be zero. Every transfer event goes
  to the storage table or the HID table, by slot and endpoint.
- **`max_frags` is 64**: 256 KiB a command, the largest any client submits.
- **Reads and writes are `READ(10)` and `WRITE(10)`.** A flush is `SYNCHRONIZE CACHE(10)`. A device
  that refuses it as an illegal request has no cache to flush, as many sticks have none, and the
  flush completes.
- **Everything off the fast path is the hub thread's**: a stalled endpoint, a bad status wrapper, a
  phase error, a command past its deadline. The DPC marks the device and wakes the thread, as a HID
  halt does. The thread runs bulk-only's reset recovery: the class's reset request, then each
  endpoint's halt cleared (Reset Endpoint, Set TR Dequeue Pointer, `CLEAR_FEATURE`). It fails the
  command `IoError` and starts the next. **A device that does not recover is ended**: its disk
  departs as on an unplug, and its slot is disabled.
- **Every command has a deadline**: thirty seconds, what Linux's `sd` gives one. The hub thread
  keeps it, sleeping until the earliest deadline while any command is in flight. **This is what
  bounds a stick that stops answering**: the storage service and `fs-server-ext4` wait on their
  reads with no deadline of their own.

**A departure** joins the hub thread's `depart`, before the records:
1. the disk leaves the DPC's table;
2. **its command in flight and its queue complete `PeerClosed`**, and so does any later submit,
   at once;
3. its records depart: the device, its disks and their partitions, as one change;
4. its slot is disabled and its memory freed, as now.

**A disk's state is one of eight slots, given back under an epoch** as a USB input node's is (Part
C), so a handle to a departed stick's node is refused rather than served the next stick's. **The
slots are a value a host test drives** — take every slot, retire one, take it again, submit through
the old context. That is PR #361's lesson: the epoch's first guard tested only its predicate.
A partition's window (`io::block::Partition`) is leaked per publish, as at boot. A departed stick
keeps its records for the boot, so `usb-departed-records` gains the windows.

**Partition tables are read where a disk arrives:**
- **In thread context**: the hub thread dispatches an IRP and waits on its operation, bounded. Not
  `read_blocking`, which polls with interrupts masked and is the boot's.
- **GPT** as today, keeping the header's disk GUID.
- **MBR**: its four primary entries. Each one in use and not extended becomes a partition, named as
  an unlabelled GPT partition is, `partition <n> (unlabelled)`. A protective entry (`0xEE`) means
  GPT. An extended one (`0x05`, `0x0F`, `0x85`) is passed over and said.
- **No table**: no partitions. The storage service probes the disk itself, as it probes a RAM disk
  now, and a filesystem at its start mounts as the disk.
- **One parser for both paths**: the table's sectors are read into bytes and parsed by a function
  over them, host-tested. The boot's polled read and the hub thread's waited one both feed it, so a
  SATA disk with an MBR gains its partitions too.
- **A stick's partitions get no `/dev/disk` names** (a call for the maintainer, below).
- **Blocks of 512 bytes only.** A disk with another block size is published with its table unread,
  and the log says why.

**The boot medium:**
- **The kernel keeps the boot disk's GPT GUID** from module 0's Limine record. That means binding
  the record's media fields. It compares the GUID with every disk's header, at boot and at arrival.
- **A match is flagged in the disk's record**: `BOOT` (`0x02`) in `flags`, beside `DEPARTED`, and
  `DeviceRecord::is_boot_medium`. On a live boot that is the stick. On an installed machine it is
  the internal disk, which `init`'s root already makes in use.
- **`/dev/devices` gains a `boot` column**, a bool.
- **The storage service passes the boot disk over**: nothing on it is auto-mounted, and `InUse`
  names it, so `disks` withholds it. An administrator's `Mount` of one of its partitions stays an
  administrator's choice.
- **`nxinstall` refuses it**: a withheld disk flagged `boot` holds the running system.

**The storage service follows its devices:**
- **An `Arrived` after `Settled` is read as at boot**: what the device holds, and a disk's table.
  It is mounted by the boot's rules, with clashing labels numbered as at boot. That means ext4, not
  on the boot medium, not the installer's source, and read-only on a live boot (Part F makes a
  removable one writable). It is logged as at boot, a line per device. Past `MAX_MOUNTS`, the
  device is said and left unmounted.
- **A `Departed`** takes the device out of the table and closes the service's node for it. **A
  mount on it is torn down**: its label leaves `fs`, its server is terminated, and its namespace is
  closed. The log says the device left while mounted, and that what had not been written back is
  lost. Not `Meta::Unmount`: the server would try to mark the filesystem clean on a device that is
  gone, fail, and exit anyway.

**A stick pulled while mounted, end to end:**
1. Its I/O fails `PeerClosed` in the kernel at once.
2. The server answers its clients `IoError`.
3. The manager's `Departed` reaches the storage service, which tears the mount down.
4. A program with one of its files mapped, touching a page not yet in memory, is ended, as on any
   failed fill (`design/fault-survival.md` is where that would change).

### Calls for the maintainer

- **The storage gate extends `check-storage`**, rather than adding a forty-third. That gate already
  boots a stick beside a disk, logs in on serial, writes with `test-pattern`, and checks a disk on
  the host. **On its live boot a stick auto-mounts read-only**, so the gate remounts it writable
  with `with admin disk`, as it does `nitrox-root` today. Part F, making a removable stick writable
  on a live boot too, removes that step. The alternative is a new gate on an installed-style boot,
  where the auto-mount is writable as the sketch's gate says: another boot in every CI run, for a
  rule Part F changes anyway.
- **A stick's partitions get no `/dev/disk` names.** Those names are how `init` finds its critical
  path, and they are bound from what the boot's probe found. A stick read in the first round would
  otherwise put its partitions there — a `nitrox-root` among them. It would lose to the internal
  disk's only because AHCI is probed first and the first binding wins. The storage service finds a
  stick through the device manager, as it finds every disk, so nothing needs the names.

### Calls made in this pass, without the maintainer

- **The I/O path is the DPC's, and recovery the hub thread's.** A thread per disk would read top to
  bottom, as the hub thread does. But every command would cost two context switches. The
  controller's one wait record would have to become many, since the hub thread and the disk thread
  would both use the default endpoint. And Part H is about throughput. The DPC path is the shape
  AHCI and HID already have. Recovery is rare and needs commands and the default endpoint, which
  is where the hub thread's sequential style pays.
- **One configuration per device**, so a composite device's second class cannot reset its first.
- **The ten-byte commands only**: units under 2 TiB of 512-byte blocks, which is every stick this
  phase meets.
- **A departed mount's server is terminated**, not asked to unmount (above).
- **The boot medium is a flag the kernel sets**, from a record only it reads, so the storage
  service, `nxinstall` and the hardware report all read the same fact.

### Pieces

- **D.1 Partition tables and the boot medium.** Covers:
  - one parser for GPT and MBR over the table's bytes, through which the boot's read goes;
  - a disk registered under a parent by id;
  - Limine's media fields bound, and `BOOT` set on the disk whose GUID matches, in both copies of
    `libkern::device`;
  - `/dev/devices`' `boot` column.

  Host tests:
  - GPT as today, with the disk GUID;
  - MBR's entries: unused, protective, extended, and a sector without the signature;
  - the flag set for the matching GUID and no other.
- **D.2 Bulk-only transport.** Covers:
  - one configuration per device, and bulk endpoint contexts;
  - the storage table, beside HID's in the DPC's routing;
  - the command and status wrappers, and the queue and its fast path;
  - the SCSI commands at bind, the disk's record, and its table read in thread context.

  Host tests:
  - the bulk contexts' words;
  - each CDB's bytes;
  - a status wrapper with a wrong signature, a wrong tag, status 1, status 2, and a residue;
  - `INQUIRY` and `READ CAPACITY` answers, including a unit of 2 TiB.
- **D.3 Recovery and departure.** Covers:
  - deadlines kept by the hub thread, reset recovery, and a device ended when it does not recover;
  - a departure completing the queue `PeerClosed`;
  - the eight slots under an epoch, as a value.

  Host tests: a slot taken, retired and taken again, with a submit through the old context refused
  — the bump, the check and the queue each with a control that fails it.
- **D.4 The storage service follows.** Covers:
  - an arrival read and mounted by the boot's rules;
  - a departure tearing its mount down;
  - the boot medium passed over and named by `InUse`;
  - `nxinstall` naming a `boot` disk as holding the running system.

  Host tests: the mount plan for an arrival; the teardown plan for a departure; the boot medium
  never planned and always in use; `withheld` naming it.
- **D.5 The gates.** Below.
- **D.6 Docs.** Below.

### Gates

- **`test-qemu`'s stick stops being blank.** It becomes **an MBR stick with one FAT partition**,
  which is what a stick from a shop holds. Its table is read in the first round, and:
  - the kernel logs its disk and partition;
  - `boot-probe`'s registry test holds the disk under its `UsbDevice` record, the partition under
    the disk, and `boot` on the disk the test image boots from and on no other;
  - the storage service reports the partition as FAT, and mounts nothing it did not mount before.
- **`check-storage` gains a stick that comes and goes** (the first call above):
  1. **The boot stick**: the service reports it as the boot medium and passes it over.
  2. **An MBR stick holding one ext4 partition, plugged in** over QMP. The kernel logs its disk and
     partition. The service mounts it read-only, the boot being a live one. `with admin disk`
     unmounts it and mounts it writable. `test-pattern --write` writes through a mapping without a
     sync. `with admin disk --unmount` ejects it: until Part F, that is what an eject is.
     `device_del` unplugs it, its records depart, and the service drops the device. **The host
     carves the partition out by the MBR**: `e2fsck -fn` clean, `s_state` clean, and the pattern
     read with `debugfs`.
  3. **A whole-disk ext4 stick, pulled while mounted.** It is plugged in, mounted, remounted
     writable, written without a sync, and `device_del`'d. The service says it left while
     mounted, its label is gone from the table, and a command typed after it runs. The host finds
     the filesystem still marked in use, which is what an unplug without an eject leaves.
  4. **The same stick plugged in again** — the node survives, as measured. It is a new disk at a
     new index, and is mounted again: served indices are not reused for disks either.
- **`check-live`**: the stick is a disk flagged `boot`, and the service passes it over.
- **`check-install`**: `with admin nxinstall` names the stick as holding the running system. The
  gate names it as the target first, and the refusal is in the log, before the install goes on to
  the SATA disk.
- **`check-report`**: the USB page lists the stick's disk beside the device.
- **Controls**, planned:
  - the GUID comparison removed: `check-install`'s refusal and `check-live`'s line fail;
  - a departure that leaves its queue uncompleted: step 3's teardown never comes, since the
    server's read never answers;
  - the service ignoring `Departed`: the label stays, and step 3 fails;
  - no table read at arrival: step 2 finds a disk holding nothing;
  - the status wrapper's tag unchecked: its host test fails.

The gate set stays at 42.

**On the laptop**, for the maintainer to try:
- the hardware report lists the live stick's disk, flagged as the boot medium;
- an ext4 stick plugged in after boot is in `disk --list`, mounted read-only;
- `with admin disk` makes it writable, a file goes onto it, and it unmounts clean.

### Not in Part D

- **FAT** (Part E). **A writable auto-mount on a live boot, `Eject`, the mount watch and Files'
  Drives** (Part F). **Formatting** (Part G). **Throughput** (Part H).
- **Media change.** A unit not ready at attach is not looked at again, so a card put into a reader
  later is not found.
- **A write-protect switch.** `MODE SENSE` is not read. A write to a protected stick fails
  `IoError`, and a writable mount of one fails at its server's first write.
- **Logical partitions** inside an MBR extended partition.
- **Units of 2 TiB or more, and blocks of other than 512 bytes.**
- **UAS and streams**, as for the phase.
- **`/dev/disk` names for a stick** (the second call).

### Docs Part D owes

- **`usb.md`**: mass storage — the binding, the I/O path, recovery, a departure, and the boot
  medium.
- **`device-node.md`**: `BOOT`, a USB disk's record, a table read at arrival, MBR.
- **`storage.md`**: arrivals and departures, the boot medium, and a stick pulled while mounted.
  §12's line about arrivals is closed.
- **`device-manager.md`**: the `boot` column.
- **`drivers-and-irps.md`**: a block driver behind a bus, driven by its controller's DPC.
- **`io-operation.md`**: block I/O on a departed disk completing `PeerClosed`.
- **`rsproto-storage-ops.md`**: `InUse` naming the boot medium.
- **`deferred-decisions.md`**: `usb-departed-records` with disk slots and partition windows, and
  `block-transfer-split`'s bound for a USB disk.
- **`qemu-integration-tests.md` and the root `CLAUDE.md`**: `check-storage`'s new steps, and
  `test-qemu`'s stick.

## Definition of Done

On the laptop and in the gates:
- **A USB mouse moves the cursor and a USB keyboard types at the greeter**, plugged in before boot
  or after it, and each is let go when unplugged.
- **A FAT32 thumb drive plugged in after boot mounts writable, appears in Files, takes a file saved
  from `nxedit`, and ejects from Files**; unplugged without ejecting, it is torn down cleanly.
- **A copy to the stick meets the number Part H sets.**

## What Phase 6 does not do

- **Kernel modules**, and matching devices to modules (the decision above).
- **I²C-HID**: the trackpad already works through the i8042. Its native interface is what
  two-finger scrolling needs, and Part B's report-descriptor parser is the first piece of it: later,
  as its own item.
- **Absolute pointers, and report descriptors beyond a mouse's**: a tablet, a touchscreen, extra
  keys, and a horizontal wheel.
- **USB 3 streams and UAS**: one bulk-only command at a time.
- **Isochronous transfers**: audio, webcams.
- **External hubs**: devices on the controller's own ports only.
- **USB device mode.**
- **exFAT.**
- **Per-session visibility under `/storage`**: a stick is reachable by every session, as every
  mount is today.

## Before Phase 6

Five items found by the laptop install, done first at the maintainer's direction, are planned in
[`laptop-polish.md`](laptop-polish.md): the grace period for `with`, blank table cells, the disk's
model in `disk --list`, the build's commit on the screen, and which output is a diagnostic.

## Docs this phase owes

- **`docs/architecture/usb.md`**, new with Part A <!-- check-docs: allow-missing -->
  and grown by each part after it.
- **`device-manager.md`**: the event source (C). **`input-subsystem.md`**: the second producer
  (B, C). **`storage.md`**: FAT, removable media, eject and the watch (D–F).
- **`drivers-and-irps.md`**: the hub thread's waits (A), and the Tier 2 question recorded rather
  than answered.
- **`deferred-decisions.md`**: read-write FAT resolved (E), `TODO(fs-throughput)` resolved or
  narrowed (H), the module loader's trigger restated (this pass).
- **The root `CLAUDE.md`**: each new gate.
