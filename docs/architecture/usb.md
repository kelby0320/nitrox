# USB

**Status: Phase 6 Parts A.1 and A.2 built (2026-10-02)** — the xHCI host controller is claimed and
its rings proved (A.1), and **a hub thread enumerates what is attached**, at boot and after it,
logging each device and matching it against the class table (A.2). **A device is not yet in the
registry** (A.3), nothing binds a class driver (Parts B and D), and a departure frees its slot but
reaches nothing else (Part C). The design ahead is [`phase-6-usb.md`](../planning/phase-6-usb.md);
this document grows with each part.

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
will be in the registry before `device-mgr` replays it. A bound that passes is logged, and the rest
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

## The code, and what tests it

- **`drivers::xhci::ring`** — TRBs, the producer ring (the command ring; transfer rings from A.2)
  and the event ring's consumer, as arithmetic over a `Slots` trait. Host tests read a producer
  ring back with a model of the controller written from the specification, across three wraps.
- **`drivers::xhci::caps`** — the extended-capability walk, bounded by the register window and an
  entry count. Host tests on register images.
- **`drivers::xhci::context`** — input and device contexts at both entry sizes, and the speeds.
- **`drivers::xhci::desc`** — descriptors read from bytes, the class match, and strings made
  printable. Host tests on bytes QEMU's devices sent during an enumeration (captured with `pcap=`),
  and on bytes no correct device sends: lengths that run past the buffer, a zero length, a string
  longer than its own length says.
- **`pci::power_up`** — host tests on a synthetic configuration space, including that the write
  raising the function does not clear `PME_Status`.
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
