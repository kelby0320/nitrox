# USB

**Status: Phase 6 Part A.1 built (2026-10-02)** — the xHCI host controller is claimed, its rings
are proved, and its interrupt drains the event ring. **Nothing is enumerated yet**: a port change
is counted, and nothing acts on it until Part A.2's hub thread. There are no USB device records
(A.3), class drivers (Parts B and D), or departures (Part C). The design ahead is
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
   way. The interrupt acknowledges and queues a DPC; the DPC drains the event
   ring and writes the dequeue pointer back. It counts Port Status Change Events, which no one
   acts on yet.

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

**No A.1 gate takes an xHCI interrupt.** QEMU posts port events only while a controller runs, and
`test-qemu`'s devices are attached before the reset, so nothing posts one after the controller
starts: the gates prove the MSI is programmed and the controller claimed over it, not that an
event interrupts. **A.2's enumeration is the first exercise of the path** — every command it sends
completes through the interrupt and the DPC — and its gate is the one that holds it. The race
below was found by a reviewer with port resets forced into the window, and its fix is held the same
way, by that experiment, recorded in the decision log.

**The ports' USB versions come from the Supported Protocol capabilities**, never from an assumed
order. QEMU's controller numbers its USB 3 ports first: with `p2=8,p3=8`, USB 3 is 1–8 and USB 2
is 9–16. A connector with a USB 2 and a USB 3 side has a port number in each range.

**What every boot logs** (and so what the hardware report shows): the power state found, whether
the firmware handed the controller over and how fast, then one line of facts —
`xhci: 00:03.0 up: xHCI 1.0, 16 ports (USB 2: 9-16, USB 3: 1-8), 64 slots, 32-byte contexts, 0
scratchpad(s)` on `test-qemu` — the No Op's answer, and the MSI vector.

## The code, and what tests it

- **`drivers::xhci::ring`** — TRBs, the producer ring (the command ring; transfer rings from A.2)
  and the event ring's consumer, as arithmetic over a `Slots` trait. Host tests read a producer
  ring back with a model of the controller written from the specification, across three wraps.
- **`drivers::xhci::caps`** — the extended-capability walk, bounded by the register window and an
  entry count. Host tests on register images.
- **`pci::power_up`** — host tests on a synthetic configuration space, including that the write
  raising the function does not clear `PME_Status`.
- **`test-qemu`** boots the controller with a keyboard at high speed, a mouse at full speed, a
  stick at SuperSpeed and a hub, and asserts on the host the controller's facts, the No Op's answer
  and the claim over MSI. A control with no doorbell write is declined, and fails it.
