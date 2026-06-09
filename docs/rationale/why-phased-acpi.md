# Why Phased ACPI

Nitrox handles ACPI in two phases. Phase 1 is pure-Rust ACPI table parsing, covering MADT/FADT/HPET/MCFG and the other static tables sufficient to bring up SMP, timers, PCIe, and IOMMU on modern hardware. No AML interpreter. Phase 2, deferred until specifically needed, integrates ACPICA via FFI as a documented exception to the "no external Rust crates" rule. This document explains the phased approach and the eventual ACPICA decision.

## What ACPI is and what it isn't

ACPI is the modern firmware standard for describing hardware to operating systems and providing power management. It has two distinct components:

**Static tables** — flat data structures with documented byte-level layouts. RSDP points to XSDT, which lists the addresses of all the other tables. MADT describes the interrupt controllers and CPUs. FADT describes fixed hardware features and registers. HPET describes the high-precision event timer. MCFG describes PCIe enhanced configuration space. DMAR/IVRS describe IOMMUs. SRAT describes NUMA topology.

**The ACPI Machine Language (AML)** — bytecode embedded in the DSDT and SSDT tables, executed by an interpreter the OS provides. AML is what implements ACPI methods like `_PRT` (interrupt routing), `_CRS` (current resource settings), `_PSx` (device power states), `_SRS` (set resource settings), and the entire dynamic device enumeration, power management, and sleep state machinery.

These are separable in implementation. You can parse the static tables without an AML interpreter; static tables are plain data. AML, however, is a small dynamic language with concurrency, mutexes, registers, region operations, and method calls. An AML interpreter is a real piece of software — ACPICA, the de facto reference implementation, is roughly 100K lines of C.

## What you can do with just the static tables

Surprisingly much.

**SMP boot.** MADT lists CPUs and APIC addresses. With this, you can program the local APIC and IOAPIC, bring up additional cores via Limine, and run a multiprocessor kernel.

> **Note — the *local* APIC needs no ACPI at all.** The current CPU's local-APIC register base is in the `IA32_APIC_BASE` MSR, so the OS-Phase-1 "architecture trait completion" slice brings the local APIC up (in xAPIC/MMIO mode) straight from the MSR, with no ACPI table parsing. ACPI's MADT is needed for the *IOAPIC* (external-device IRQ routing) and for enumerating the *other* CPUs — both deferred to OS Phase 2. (xAPIC rather than x2APIC because the QEMU/TCG dev loop does not emulate x2APIC; see the decision log.)

**Timers.** FADT and HPET tell you the timer addresses. TSC calibration via HPET works.

> **Note — Phase-1 timekeeping needs no ACPI either.** The OS-Phase-1 "timers and clocks" slice builds the monotonic clock and the per-CPU timer from the **local-APIC timer + TSC, calibrated against the legacy PIT** (channel 2, at its fixed legacy ports) — no HPET, no FADT. HPET (which needs ACPI/FADT to locate) stays deferred to OS Phase 2; the LAPIC timer runs in count-down mode rather than TSC-deadline mode because the QEMU/TCG dev loop does not emulate the TSC-deadline timer (see the decision log).

**PCIe enumeration.** MCFG gives you the ECAM (Enhanced Configuration Access Mechanism) base address. From there, PCIe configuration space is just MMIO at predictable offsets. You can walk the bus tree, identify devices, size BARs, build the device tree.

**IOMMU programming.** DMAR (Intel) or IVRS (AMD) describes the IOMMU. Programming the IOMMU is straightforward MMIO once you know its address.

**Modern interrupt routing.** PCIe devices use MSI/MSI-X by default — the device writes to a magic memory address to trigger an interrupt, with the address and vector configured via MSI capability registers in PCI config space. No ACPI involvement.

**Reboot.** FADT contains a `ResetReg` and `ResetValue`. Writing the value to the register triggers a hardware reset. Works on essentially all modern hardware; no AML required.

This combination is enough to run a complete kernel on modern desktop, server, and QEMU hardware. The kernel can boot all CPUs, handle interrupts, enumerate PCIe devices including AHCI/NVMe storage and network controllers, program IOMMUs for DMA isolation, and reboot when asked. Storage works, networking works, displays work (via the framebuffer Limine provides), and reboot works. This is everything a hobby OS needs for years of development.

## What requires AML

The things AML is genuinely necessary for:

**Laptop ecosystem.** Battery state, lid switch, thermal zones, AC adapter detection, embedded controller communication. All of this is AML-mediated. There's no static table that says "battery percentage is 73%" — there's an AML method that, when called, queries the embedded controller and returns the value.

**Graceful shutdown (ACPI S5 state transition).** Requires evaluating `_S5` to get the SLP_TYP value and writing to the PM1 register in a defined sequence. The `_S5` evaluation is AML.

**Sleep states (S1-S4).** Suspend-to-RAM, hibernation, etc., require AML method evaluation throughout.

**Dynamic device power management.** `_PSx` methods control individual device power states. Without these, devices stay at full power until the OS halts.

**ACPI-only motherboard devices.** TPM, some embedded sensors, some legacy motherboard chipset devices that aren't on PCIe and aren't in any static table. The DSDT enumerates them via AML.

**CPU power management beyond `hlt`.** C-states and P-states (idle states and frequency scaling) are AML-mediated. Without AML, the kernel can issue `hlt` for idle but can't request deeper sleep or lower CPU frequencies.

**General Purpose Events (GPEs).** ACPI's mechanism for delivering hardware events (lid close, power button, thermal alerts, button presses on weird hardware) requires AML to handle.

**Long-tail firmware quirks.** The hardware ecosystem is full of machines with `_INI` methods that must be evaluated for the machine to work right, AML workarounds for hardware bugs, vendor-specific fixups. ACPICA has accumulated decades of experience with these.

## What was considered

Three options for handling this:

### Option 1: Build a Rust AML interpreter from scratch

Pros: preserves single-language purity. Stays smaller than ACPICA — a focused Rust AML interpreter could probably land at 5-10K lines, given that AML's runtime model is fixed and well-specified. Better fit for the kernel's safety story.

Cons: real engineering work — weeks to months. The AML spec is approximately 200 pages of detail. Real-world firmware AML has edge cases that have eaten ACPICA contributors for years; a from-scratch interpreter would inherit none of that experience and would have to discover the issues. Existing prior art (the `aml` Rust crate) is incomplete and not maintained at the level a kernel needs.

The honest assessment: this is the path the "pure Rust everywhere, no exceptions" instinct wants. It is also the path that would consume large amounts of project time on something that isn't novel and isn't differentiating. For a hobby project pursuing a novel OS architecture, spending months writing a Rust AML interpreter is a poor use of effort. The AML interpretation problem is not the problem this project exists to solve.

### Option 2: Integrate ACPICA via FFI

Pros: battle-tested across decades and millions of real firmware blobs. Handles the long tail of firmware quirks. Mature, well-documented, BSD-licensed (compatible).

Cons: 100K-line C dependency in the kernel — violates "no external crates" as originally stated. The ACPICA OS Services Layer (OSL) requires implementing approximately 30 callback functions binding ACPICA to kernel facilities (memory, locks, interrupts, I/O, threading). Significant `unsafe` FFI surface — the "approximately 10% unsafe" claim shifts to "approximately 10-15% unsafe" once integrated.

### Option 3: Phased approach (chosen)

Phase 1: pure Rust ACPI table parsing. No AML. No external crates. Sufficient for the development phase of the project, which will run for years on modern desktop and server hardware.

Phase 2: ACPICA integration when the project actually needs AML. Deferred until a concrete need surfaces (laptop targeting, graceful shutdown requirement, AML-only device that must be supported, CPU power management beyond `hlt`).

The decision between Options 1 and 2 was made up front: when Phase 2 happens, it will be ACPICA, not a from-scratch Rust interpreter. The reasoning is the same as the cost/benefit analysis above — the AML interpretation problem is not where this project should spend effort. The phased approach defers the *timing* of integrating ACPICA, not the choice of *what* to integrate when the time comes.

## Why phasing is the right approach

A few things drove this:

**Phase 1 is enough for years.** Most hobby OS development happens on desktop, server, or QEMU hardware. None of these scenarios require AML for the development experience to be smooth. By the time Phase 2 becomes necessary, the kernel will be much more mature, and the OSL implementation will benefit from that maturity (real synchronization primitives, real interrupt subsystem, real thread management for ACPICA's threading callbacks).

**Phase 1 is not wasted work.** When Phase 2 happens, ACPICA will need to parse the same static tables Phase 1 already parses. Phase 1's table parsing remains the path for tables that don't need AML interpretation. The Phase 1 code continues to be used.

**The decision can be revisited if circumstances change.** If at some point a Rust AML interpreter of acceptable quality emerges in the broader ecosystem (well-maintained, complete enough, used in serious projects), Phase 2 can adopt it instead of ACPICA. The phasing keeps options open. Committing to ACPICA up front would foreclose this.

**Documenting Phase 2 as a planned exception is honest.** The "no external crates" claim, as written for Phase 1, is literally true. Phase 2 introduces an exception with documented reasoning. This is better than pretending the exception won't ever exist or being surprised by it later.

## When Phase 2 happens

The trigger for Phase 2 is any of:

- **First laptop target.** Battery, lid, thermal management, embedded controller — these all want AML.
- **Graceful S5 shutdown requirement.** When "I want to issue a shutdown command and have the machine actually power off cleanly" becomes important.
- **An ACPI-only device that must be supported.** Some motherboard chipset device that's not on PCIe and is enumerated via DSDT.
- **CPU power management beyond `hlt`.** When measurable power consumption matters.
- **GPE handling.** When the system needs to receive ACPI hardware events.

When triggered, Phase 2 is a focused integration task:

1. Vendor a specific ACPICA release at `kernel/vendor/acpica/`. Reproducible builds — pin a version, don't track upstream live.
2. Implement OSL callbacks in `kernel/src/kacpi/osl/` — approximately 30 functions, mostly thin wrappers. ~800-1500 lines of Rust.
3. Build via `build.rs`, with `-ffreestanding`, no libc, ACPICA's debugger and disassembler components disabled.
4. `bindgen` generates the FFI declarations as a host-side build tool. The generated bindings are committed; normal builds don't run bindgen.
5. The `kernel/src/kacpi/osl/` directory carries `#![allow(unsafe_op_in_unsafe_fn)]` and is documented as the ACPICA boundary.
6. The kernel's "approximately 10% unsafe" claim is revised to "approximately 10-15%."

## Interim: shutdown and reset in Phase 1

The most user-visible Phase 1 limitation is shutdown. The kernel can boot, run, do everything useful, and then... can't actually power off when the user types `shutdown`. The interim is:

**Reset works.** FADT's `ResetReg` and `ResetValue` are static, no AML needed. Reboot is fully functional.

**Shutdown is awkward but acceptable.** On QEMU, writing to port `0x604` (Q35 chipset) triggers ACPI shutdown — this is QEMU-specific and works without firmware AML execution. On real hardware without AML, the kernel halts the machine via `hlt` loop and the user holds the power button. Not ideal, but not a blocker for development.

Both paths are wrapped behind `ArchPower::shutdown()` and `ArchPower::reboot()`, so Phase 2 can swap in ACPICA-backed implementations without touching callers.

## Where to read more

- [Power management architecture](../architecture/power-management.md) — full power management subsystem, both phases
- [Boot flow architecture](../architecture/boot-flow.md) — where ACPI initialization happens during boot
- [ACPICA project documentation](https://www.intel.com/content/www/us/en/developer/articles/tool/acpi-component-architecture-downloads.html) — for when Phase 2 happens
