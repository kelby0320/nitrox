# USB

**Status: Phase 6 Part A built (2026-10-02), and Part B's keyboards and mice (B.1–B.4, 2026-10-05)**
— the xHCI host controller is claimed and its rings proved (A.1); **a hub thread enumerates what
is attached**, at boot and after it, logging each device and matching it against the class table
(A.2); **each device is a `UsbDevice` record in the registry**, which `device-mgr` names
`usb-<id>` (A.3); and **a boot keyboard or mouse is bound**, polled by the DPC, and served at
`/dev/input/raw/<n>` like the i8042's, a mouse's wheel read through its report descriptor (B.2,
B.3). The lock keys and their lights are B.5's, not built. No class driver binds anything else
(Part D). A departure frees the device's slot and leaves its records, and nothing tells
`device-mgr` of a device plugged in after it read the registry (Part C). The design ahead is
[`phase-6-usb.md`](../planning/phase-6-usb.md); this document grows with each part.

## The controller

**One Tier 1 driver, `drivers::xhci`** (`kernel/src/drivers/xhci/`), matched by PCI class
`0C/03/30`, serves QEMU's xHCI (the gates attach `nec-usb-xhci`) and the laptop's Sunrise Point-LP
controller (`8086:9d2f`) alike, as one AHCI driver serves both machines' disks. It takes **one
controller**; a second is declined.

**Bring-up is in `drivers::probe`**, polled, with interrupts masked and before the scheduler runs:
1. **Power.** `pci::power_up` puts the function in D0 through its power-management capability,
   having saved its BARs and command register. From D3hot the driver waits the 10 ms PCI PM 1.2
   requires, and restores what the transition reset when the function lacks `No_Soft_Reset`. The
   laptop's controller was in D3 when Linux was asked about it; QEMU's has no power management.
2. **Bus mastering** on.
3. **The firmware's handoff**, through the USB Legacy Support extended capability: OS-owned set,
   up to a second for the firmware to clear BIOS-owned — then it is taken anyway, as Linux takes
   it — and the firmware's SMIs turned off.
4. **Ready, then halt, then reset.** Controller Not Ready is waited out before any operational
   register is written (xHCI 1.2 §4.2), since a function just raised from D3 can still be setting
   up. Then `HCRST` is set, **1 ms passes before any register is read**, and `HCRST` and Controller
   Not Ready are waited out. Linux pauses on every Intel host, against a
   rare hang on that read; a bound on the loop cannot stand in for it.
5. **What the controller needs from memory**: the device context base array, the scratchpad
   buffers it asks for, a one-page command ring with a Link TRB back to its start, and one
   event-ring segment on interrupter 0. Each is a `DmaBuffer` page or more, so none crosses the
   64 KiB boundary a ring may not.
6. **MSI**, programmed before the controller runs, so a failure leaves nothing running.
7. **Running, and a No Op through the command ring**, polled on the event ring. It proves the
   ring, the doorbell and the event ring before anything depends on them, and the function is
   claimed only if it completes with Success.
8. **The interrupter on — without writing Interrupt Pending — then one more drain.** An event
   posted after the No Op's drain sets IP and Event Handler Busy with the interrupter still off,
   and raises nothing. IP is write-one-to-clear, so enabling with IP written as one threw that
   interrupt away, and with EHB still set the interrupter raised nothing again: only the DPC clears
   EHB, and only an interrupt runs the DPC. Enabling without IP keeps the pending interrupt, and
   the drain after consumes whatever landed and clears EHB, so the next event interrupts either
   way. The interrupt acknowledges and queues a DPC; the DPC drains the event ring a batch at a
   time, writing the dequeue pointer back, and acts on each event with the ring's lock released: a
   port change wakes the hub thread, and a completion ends the wait it is in (below).

**Every wait is bounded, and a failure declines the function with its reason** — no USB is a
diagnosable failure, a hang on a machine with no serial port is not. A decline after the
controller started halts and resets it first, since it would otherwise go on writing to memory the
driver frees. `usb=off` on the command line declines the controller outright; the installed system
has no boot menu to type it at, but the live stick's does.

**MSI only.** The laptop's controller has MSI with eight vectors and no MSI-X, and the driver
takes that. **Every gate's controller is QEMU's `nec-usb-xhci` with `msi=on,msix=off`**
(`XHCI_DEVICE` in `tools/xtask/src/main.rs`), so the gates' controller signals the way the laptop's
does. It is the same xHCI core as `qemu-xhci`, with the NEC µPD720200's identity (`1033:0194`).
`qemu-xhci` offers MSI-X alone on q35, and QEMU 8.2 — CI's — hard-codes that, with no `msi`
property; the NEC model takes `msi` and `msix` in 8.2 and 11 alike.

**Enumeration is what exercises the interrupt.** QEMU posts port events only while a controller
runs, and `test-qemu`'s devices are attached before the reset, so bring-up alone takes no xHCI
interrupt: it proves the MSI programmed and the controller claimed over it. Every command and
transfer the hub thread sends (below) completes through the interrupt and the DPC, so
`test-qemu`'s enumeration facts are what hold the path. The race in step 8 above is held otherwise:
a reviewer found it with port resets forced into the window, and that experiment, run against the
code before and after the fix, is recorded in the decision log.

**The ports' USB versions come from the Supported Protocol capabilities**, never from an assumed
order. QEMU's controller numbers its USB 3 ports first: with `p2=8,p3=8`, USB 3 is 1–8 and USB 2
is 9–16. A connector with a USB 2 and a USB 3 side has a port number in each range.

**What every boot logs** (and so what the hardware report shows): the power state found, whether
the firmware handed the controller over and how fast, then one line of facts —
`xhci: 00:03.0 up: xHCI 1.0, 16 ports (USB 2: 9-16, USB 3: 1-8), 64 slots, 32-byte contexts, 0
scratchpad(s)` on `test-qemu` — the No Op's answer, and the MSI vector.

## Enumeration: the hub thread

**A kernel thread** (`drivers/xhci/hub.rs`), spawned by `drivers::start` once the scheduler and
the APs are up — after them rather than during AP bring-up, the scheduler's most delicate moment.
It is **the first long-lived kernel thread to block in `sched::wait_on`**, on three kinds of thing:
- **a command's or a control transfer's `PendingOperation`**, which the DPC completes when the
  event for it arrives;
- **the port-change `InterruptObject`**, which the DPC signals on every Port Status Change Event.
  It latches, so a change that lands while the thread is enumerating is counted, not lost;
- **nothing, until a deadline**, to sleep through the waits USB requires.

**One wait at a time, handed over safely.** The thread records what it awaits — a command by its
TRB's address, a transfer by its slot and its Status stage's address — *before* the TRB is written,
so no completion can arrive first. The DPC takes a matching record out under a lock and completes
its operation after releasing it, since completing takes the scheduler's lock and no leaf lock may
be held across that. A transfer is ended by its Status stage, or by an error on any stage; a short
Data stage is passed over, since its Status stage follows. **On a timeout the thread takes the
record back**; if the DPC already took it, its completion is on the way and is waited for, so the
DPC never completes an operation the thread has dropped.

**Coldplug is the first round of hot-plug.** The first round takes the USB 2 debounce once for
every port (100 ms, which also lets USB 3 links train), logs each connected port's state, then
enumerates the connected ports one at a time — only one device may answer at address 0 — and
completes the operation **the boot waits on**: `drivers::settle`, before the hardware report and
`init`, bounded at two seconds. So the report lists what is attached, and a device present at boot
is in the registry before `device-mgr` reads it. A bound that passes is logged, and the rest
arrive later. Then the thread sleeps until a port changes: a connection is debounced and
enumerated; a disconnection disables the device's slot and frees its memory.

**Per device**, each step logged if it fails, and the slot disabled again:
1. a USB 2 port is reset and given its 10 ms recovery; a USB 3 port is enabled by link training,
   which is waited for;
2. *Enable Slot*, then *Address Device* with the speed's default packet size, then 10 ms for
   SET_ADDRESS to settle (USB 2.0 allows a device 2 ms; Linux waits 10);
3. the device descriptor's first eight bytes, then *Evaluate Context* if `bMaxPacketSize0` differs
   from that default — **an exponent at SuperSpeed** (9 is 512), a byte count below it;
4. the device descriptor, the configuration descriptor (nine bytes, then `wTotalLength`), string
   descriptor 0, and the product and serial strings. **A string the device stalls is passed over**
   — a bad index or language is a common stall — and the default endpoint recovered with *Reset
   Endpoint* and *Set TR Dequeue Pointer*, since a stall halts it until reset; any other failure
   on a string ends the device, as on every step;
5. **the class match, logged and not acted on**: HID boot keyboard `03/01/01`, HID boot mouse
   `03/01/02`, bulk-only mass storage `08/06/50`, a hub (listed, not supported), or nothing this
   kernel drives. No `SET_CONFIGURATION`: the class driver that binds sets the configuration.

**Every command and transfer has a one-second deadline.** A device that misses a transfer's is
abandoned and its slot disabled. A command that goes unanswered leaves the command ring in doubt —
aborting it is not built — so the controller is marked wedged and nothing more is asked of it, not
even to disable a slot.

**A slot is disabled before its memory is freed, and only then.** The controller reads and writes
a device's contexts and rings until its slot is disabled — QEMU's *Disable Slot* itself writes the
default endpoint's output context — so a failed enumeration and a departure both disable first. A
slot that does not disable, or that a wedged controller cannot be asked about, **keeps its memory
for good**: a few pages leaked is the price of not handing the controller memory it may yet write.

**A connect change on a port that has a device is a departure**, then an arrival if something is
there: a device pulled and another plugged in while the thread was busy — or a contact that bounced
— leaves the port connected, and only the change bit says anything happened.

**The log**, per device: `usb: port 9: 0627:0001 class 03/01/01, high-speed, "QEMU USB Keyboard
(…)": HID boot keyboard`; the evaluation when it happens; `usb: first round: 5 device(s) in 209 ms`;
and `usb: port 14: disconnected; slot 6 disabled`. The names are each device's product and serial
strings, printable ASCII, the serial in brackets as a disk's is.

## The record

**Each device enumerated is put in the registry** (A.3), by the hub thread once its class match is
logged — not by the DPC, since the table allocates under its lock. Its node is a bare `Other` with
the zero descriptor, and what it is lives in the device table's entry beside the node, filling its
`UsbDevice` record:
- its IDs, in the fields a PCI function's go in;
- its class triple: the device's, or its first interface's when that is zero, as most are;
- its root port and its speed, in two bytes that were padding;
- its product and serial as its name, or `vvvv:pppp` when it has no strings;
- `xhci` as its driver, and **the controller's PCI function as its parent**, found by the
  controller's address. The node's own zero address would find the host bridge.

[`device-node.md`](../spec/device-node.md) § *The registry* has the fields.

**A departure leaves the record**, and an id is never reused, which is why `device-mgr` names a USB
device `usb-<id>` and not by its port ([`device-manager.md`](device-manager.md) §5). The port goes
to the next device when this one leaves, and a connector has two port numbers. Until Part C the
manager reads the registry once, at its start. A device plugged in after that is in `/dev/registry`
and not in `/dev/devices`. A device the table cannot take — no memory for its node, or the table
full — stays attached, and the log says it is missing from the registry.

## Keyboards and mice: HID (Part B)

**Binding is a step of enumeration** (`kernel/src/drivers/xhci/hid.rs`). After a device's record
is made, the hub thread binds every interface whose class triple is a boot keyboard (`03/01/01`) or
a boot mouse (`03/01/02`) — not only the first, so a receiver with both gives both. Each bound
interface becomes an input node. In order:
1. **Configure Endpoint** adds every bound interface's interrupt-IN endpoint in one command. Each
   is read from the configuration descriptor after its interface: a HID descriptor sits between,
   and a SuperSpeed companion after it, giving the burst. Its context holds:
   - its maximum packet and burst, and three retries;
   - its interval, as an exponent of 125 µs: three more than ⌊log₂ `bInterval`⌋ at full and low
     speed, where `bInterval` is in milliseconds, and `bInterval` − 1 above;
   - its Max ESIT Payload, and an Average TRB Length of the TRB it is given;
   - its transfer ring.

   The slot's context entries are raised to the highest Device Context Index (twice the endpoint
   number, plus one for IN).
2. **`SET_CONFIGURATION`**, after Configure Endpoint, which is Linux's order: the controller has
   the bandwidth before the device is told. It is the first request with no data stage, which
   `control_out` sends.
3. **A mouse's report descriptor** (B.3), from the interface, for the length its HID descriptor
   gives.
4. **`SET_PROTOCOL`**: report protocol for a mouse whose descriptor describes a plain mouse; boot
   protocol for every keyboard and any other mouse. A keyboard that refuses boot protocol is not
   bound. A mouse that refuses report protocol is in it already, a device's state after a reset.
5. **`SET_IDLE (0)`** to a keyboard, so it reports a change and not a held state. A refusal is
   passed over: an unchanged report decodes to nothing.
6. **A node**, registered under the device's record, and **one Normal TRB** queued, for the
   endpoint's maximum packet, interrupting on completion and on a short packet.

**What ends a binding.** A failure in step 1 or 2 is the device's, and ends every interface's. In
steps 3 to 6:
- **a stall is that interface's**: its endpoint stays configured and is never polled, and the
  others go on. A stalled report descriptor runs the mouse in boot protocol;
- **any other failure on the default endpoint ends the binding there**, since the request may still
  be on the endpoint's ring (`control_in`'s rule, which enumeration keeps by ending the device). So
  nothing more is asked of it: that interface is not bound, nor any after it, and those before it
  stay bound, on their own endpoints. A keyboard whose `SET_IDLE` fails so is past its last request,
  and is bound, the last (PR #358 review);
- running out of nodes or table room is that interface's alone.

**Polling is the DPC's.** A Transfer Event for any endpoint but the default one goes to the table
of bound endpoints, by slot and Device Context Index. On success or a short packet:
- the report — the TRB's length less the residual — is copied out of its buffer;
- it is decoded against the endpoint's last report (`kernel/src/drivers/hid/`), and the events go
  to the node's ring, stamped at the MSI's interrupt tail;
- the same TRB is queued again with a doorbell;
- a read parked on the node is completed, as the PS/2 driver's are, through `drivers::input`.

QEMU's device NAKs an IN token with nothing to report, so its controller completes a TRB only when
there is input; a real device answers on its interval. Re-queueing on every completion serves both.

**Any other completion halts the endpoint**, and the DPC cannot issue a command and wait, so it
marks the endpoint and wakes the hub thread, **after letting the table's lock go**: waking takes the
scheduler's lock, which ranks above it (PR #358 review). The thread resets it — Reset Endpoint, then
Set TR Dequeue Pointer to the ring's enqueue point — and queues its TRB again, saying so once the
lock is let go. A third halt leaves it stopped, logged, and so does a reset that fails. Each halt
is counted once, when the DPC sees it. **No `SYN_DROPPED` follows a recovery**: a HID report is
the device's whole state, so the first report after it, decoded against the last before, delivers
what changed. A `SYN_DROPPED` would make the input layer forget every held modifier (PR #357
review).

**The reports** (`drivers::hid`, which knows nothing of USB):
- **A keyboard's** eight bytes, against the last report: releases first, keys then modifiers, then
  presses, modifiers then keys, then `SYN_REPORT`. Usages become keycodes through a table beside
  the PS/2 driver's scancode table, evdev's numbering; a usage with no keycode is dropped. A report
  of `ErrorRollOver` is not kept, since decoding it would release every held key.
- **A mouse's**, by its layout: where its buttons, X, Y and wheel are. Button changes, then
  `REL_X`, `REL_Y` and `REL_WHEEL` when non-zero, then `SYN_REPORT` — the PS/2 driver's shape. In
  boot protocol the layout is fixed: three buttons, then X and Y as signed bytes, and no wheel. **In
  report protocol it is the report descriptor's** (B.3), read by a small parser:
  - it reads short items only, within the descriptor's length, and finds the first application
    collection whose usage is *Mouse* or *Pointer*;
  - in one input report it finds the buttons, relative X and Y, and the wheel, with the report
    ID that prefixes the report when the device numbers them;
  - fields are read at their own width and sign, so a 12- or 16-bit axis decodes as what it is;
  - a descriptor that describes anything else, or does not parse, leaves boot protocol.
- **The wheel is negated.** HID's is positive away from the user, and `REL_WHEEL` here is positive
  toward the user, the screen's direction (`kernel/src/libkern/input.rs`).
- **Not carried**: buttons past the third and a horizontal wheel, which the parser finds and
  nothing above the kernel carries, and a fourth byte in boot protocol, which is the device's own.

**The node** is one of sixteen statics, its `CharBackend` context the index, as the PS/2 driver's
two are. Its ring, its parked read and the hand-off of a finished read are `drivers::input`'s,
shared with PS/2 (B.1). **Its served index is the next after every input node's**:
- beside the i8042's keyboard and mouse, at 0 and 1, a USB keyboard and mouse are 2 and 3;
- on a machine without an i8042 they are 0 and 1;
- an index is never reused.

It is a `Keyboard` or `Mouse` record under its `UsbDevice`, with driver `usb-hid` and the kind word
as its name, and `device-mgr` hands it to `input-server` as it hands the i8042's.

**The hardware report counts any keyboard's presses**, and holds its pages when any keyboard node
exists. It asked the i8042 alone until B.2, and on a machine without one it held no page. It drains
every keyboard's ring when it ends.

**Departure**: the hub thread takes the device's endpoints out of the DPC's table **before Disable
Slot**, so the DPC cannot touch a report buffer the release is about to free. Rings and buffers
are the device's memory, freed only after its slot is disabled. The node stays, with no producer,
until Part C retires it.

**The log** adds a line per bound interface: `usb: port 9: keyboard at /dev/input/raw/2, boot
protocol`, and `usb: port 10: mouse at /dev/input/raw/3, report protocol, with a wheel`.

## The code, and what tests it

- **`drivers::xhci::ring`** — TRBs, the producer ring (the command ring; transfer rings from A.2)
  and the event ring's consumer, as arithmetic over a `Slots` trait. Host tests read a producer
  ring back with a model of the controller written from the specification, across three wraps.
- **`drivers::xhci::caps`** — the extended-capability walk, bounded by the register window and an
  entry count. Host tests on register images.
- **`drivers::xhci::context`** — input and device contexts at both entry sizes, and the speeds.
- **`drivers::xhci::desc`** — descriptors read from bytes, the class match, strings made printable,
  and the `vvvv:pppp` name of a device with none. Host tests on bytes QEMU's devices sent during an
  enumeration (captured with `pcap=`), and on bytes no correct device sends: lengths that run past
  the buffer, a zero length, a string longer than its own length says.
- **`pci::power_up`** — host tests on a synthetic configuration space, including that the write
  raising the function does not clear `PME_Status`.
- **`device::Registry`** — a host test that a USB record carries its entry's IDs, class, port,
  speed, name and driver, under the controller found by its address, and no parent when the table
  has no function there.
- **`test-qemu`** boots the controller with a keyboard at high speed, a mouse at full speed, a
  stick at SuperSpeed, a hub, and a smart-card reader — the device nothing matches, and the one
  whose default endpoint is not its speed's, so its enumeration evaluates it. On the host it
  asserts the controller's facts, the No Op's answer, the claim over MSI, each device's line, the
  evaluation, and the first round **ending before `init` is spawned**. Over QMP it plugs a keyboard
  in once the first round is logged; once it has arrived, **pauses the machine and swaps it for a
  mouse on the same port**, so the guest finds a connect change on a port that still has a device;
  and pulls the mouse out once it has arrived. Each arrival and departure is asserted, in order, on
  one port. Controls each fail it: no doorbell write; a boot that does not wait; no evaluation; a
  DPC that ignores Transfer Events — every device then times out, the boot's wait gives up at two
  seconds and says so, and the boot goes on; a hub loop that ignores its wake; a disconnect that
  does nothing; a connect change on an occupied port that keeps the old device. Two paths no gate
  reaches are held by experiments recorded in the decision log: that QEMU's *Disable Slot* writes
  the output context, which is why memory outlives the slot, and that a stalled request leaves the
  endpoint answering nothing until it is recovered.
- **The records, in `test-qemu`** (A.3): `boot-probe` prints a line per `UsbDevice` record it reads
  through `/dev/registry`, after checking it is under a claimed `0c/03/30` function and carries a
  port, a speed, a name and its driver. The host holds each of the five to its port, IDs, class,
  speed and name. `boot-probe` also holds `device-mgr` to the registry: the manager's devices are
  the registry's first records, every record after them is a USB device, and each USB device the
  manager read is listed as `usb-<id>.tsm`. Where the hot-plug lands against the manager's read
  varies from run to run, under TCG and KVM alike — before either device, between them, and after
  both have all been seen — and the check holds wherever it lands. Controls each fail it:
  - a hub thread that registers nothing fails on the host. `boot-probe` checks only the records it
    finds and passes, which is why the host asserts the lines;
  - a record whose parent is looked up by the node's own descriptor fails in `boot-probe`.
- **`check-report`** reads the live stick's enumeration line off a report page: port 1,
  SuperSpeed, mass storage. It is the line a photograph of the laptop's report will be compared
  with.
- **`drivers::hid`** — host tests:
  - the usage table against the system's keycodes, with nothing outside evdev's key range;
  - a keyboard report against its predecessor: a press, a release, a key moving slots, modifiers
    pressed first and released last, `ErrorRollOver`, six keys, a short report, and the worst case;
  - a mouse report by a layout: each button, signed axes, a report ID, 12- and 16-bit fields, a
    boot report's fourth byte unread, and the wheel's sign;
  - the report-descriptor parser on QEMU's mouse and keyboard and on three shapes real mice have,
    with each refusal.
- **`drivers::xhci::{ring, context, desc}`** — the Normal TRB and Configure Endpoint, the interval
  at each speed and both ends of its clamp, the input context at both entry sizes, and the HID
  interfaces of QEMU's devices and of a combined receiver.
- **`device`** — the served index beside the i8042's two, without them, and where the i8042 has only
  a mouse.
- **The `--usb` gates** (B.4) boot q35 with `i8042=off`, the gates' controller, `usb-kbd` and
  `usb-mouse`, so a key or click that arrives came through USB:
  - `check-input --usb` asserts what `check-input` does — the stalled-consumer motion sum, a key, a
    click, the wheel in both directions, the chords and the routing — less the i8042's
    held-release step;
  - `check-login --usb` logs in at the greeter and runs the session;
  - `check-report --usb` turns the report's pages on USB key presses.
- **`test-qemu`** asserts each bound interface's line and `boot-probe`'s record of it, and that
  `input-server` was handed the first round's keyboard and mouse beside the i8042's. Its hot-plug
  asserts each arrival bound.
- **Controls**, each failing its gate:
  - a DPC that does not re-queue;
  - the report's presence check asking the i8042 alone;
  - no USB key count;
  - the wheel not negated;
  - the descriptor ignored.
