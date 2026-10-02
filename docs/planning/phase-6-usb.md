# Nitrox Implementation Plan — Phase 6 — USB

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams.

**Status: scoped 2026-10-01; Part A detailed 2026-10-02; nothing built.** This replaces the
sketch written on 2026-09-10, before Phase 5 and administration. The scope and the decisions below
were agreed with the maintainer on 2026-10-01. Each part gets its own detail pass when it is next,
as administration's parts did; what is here is the phase's shape, the design each part builds to,
and the gate that closes it. **Nothing below describes current behaviour.**

## Scope

| | |
|---|---|
| **In** | **An xHCI host-controller driver**, one for both machines. **USB enumeration** at boot and after it, with devices that **arrive and depart**. **HID keyboards and mice** in boot protocol. **USB mass storage** as block devices, with **MBR** partition tables and whole-disk filesystems beside GPT. **`fs-server-fat`**, read-write. **Removable media** a session can use and eject. **Formatting and partitioning** (`disk --format`, `disk --partition`). **Copy throughput**, measured on the laptop and then fixed where it is worst. |
| **Out** | **Kernel modules** (Tier 2), and with them module matching. I²C-HID. USB tablets and other absolute pointers. HID report descriptors. USB 3 streams and UAS. Isochronous transfers (audio, webcams). External hubs. USB device mode. exFAT. |
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

### HID, boot protocol

The driver sets each HID interface to boot protocol and polls its interrupt-IN endpoint. A
keyboard's eight-byte report becomes key presses and releases — HID usage to keycode through a
table beside the scancode table — and a mouse's report becomes button events, `REL_X` and `REL_Y`,
each stamped at the interrupt. They are served at `/dev/input/raw/<n>`, like the i8042's two, and
reach `input-server` through the manager. The keyboard's Caps Lock and Num Lock lights are a
`SET_REPORT`, sent when the compositor's modifier state changes; whether that is in scope is Part
B's detail pass.

**Boot protocol has no wheel.** HID 1.11 defines a boot mouse's report as buttons, X and Y, and
anything after them is the device's own. The wheel needs report descriptors, so it is a cost of the
boot-protocol call, deferred with them (PR #350 review).

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
| HID boot keyboard and mouse | Part B | no |
| `DeviceKind::UsbDevice`; a departed flag and a generation in the registry | Parts A and C | no — not a hash input; `abi-sync-check` guards `libkern::device` |
| A registry-changed notification kind | Part C | **yes** — a notification kind is ABI |
| Bulk-only transport and SCSI as a block device | Part D | no |
| MBR partition tables, whole-disk filesystems, partition scans at runtime | Part D | no |
| Limine's file media fields | Part D | no |

## Parts — sketched

Ordered by dependency. Each has its detail pass before it is built.

| Part | What | Gate |
|---|---|---|
| **A** | **xHCI and enumeration, reported.** The hub thread; the xHCI driver; enumeration at boot and on later port changes; `UsbDevice` records; the hardware report's USB page. | `test-qemu` boots with `usb-kbd`, `usb-mouse` and `usb-storage` attached and asserts each enumerated with its IDs and class. `check-report` asserts the live stick listed. On the laptop, the report's USB page is the survey this plan lacks. |
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
  `device-mgr`'s names.
- **A.4 Docs.** Below.

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
- **Absolute pointers and HID report descriptors**: a tablet, a touchscreen, extra keys, and a
  mouse's wheel, which boot protocol does not carry.
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
