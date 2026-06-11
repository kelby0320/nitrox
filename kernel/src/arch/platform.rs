//! Architecture-neutral platform-discovery contract.
//!
//! [`ArchPlatform`] is the kernel's neutral view of the hardware the firmware
//! describes. The firmware mechanism is arch-specific — ACPI static tables on
//! x86_64, a Device Tree Blob on aarch64 — and lives entirely behind this
//! boundary. Only facts that mean the same thing on every architecture cross
//! it.
//!
//! Today the sole neutral fact is the set of **PCIe ECAM regions**: PCI(e)
//! configuration space is a PCI-SIG standard identical across architectures, so
//! the neutral PCI enumerator (the storage slice) consumes [`EcamRegion`]s and
//! builds architecture-independent `DeviceNode`s — only *where the ECAM window
//! lives* is arch-specific (ACPI MCFG vs. the DTB).
//!
//! Arch-specific routing facts deliberately do **not** cross this boundary. On
//! x86_64 the ACPI MADT yields IOAPIC bases, GSI bases, and interrupt source
//! overrides — concepts with no aarch64 (GIC) analogue. The x86 implementation
//! caches them internally and the x86 IOAPIC code consumes them directly; they
//! never appear in neutral names. See `docs/architecture/drivers-and-irps.md`
//! and the decision log (2026-06-11).
//!
//! The active architecture's implementation is re-exported from `crate::arch`
//! as `Platform` (see `kernel/src/arch/mod.rs`), mirroring `ArchIrq` → `Irq`.

use crate::libkern::AllocError;
use crate::mm::PhysAddr;

/// A PCIe ECAM (Enhanced Configuration Access Mechanism) window: an MMIO region
/// through which a span of PCI buses' configuration space is read. Discovered
/// from firmware (ACPI MCFG on x86_64); consumed by the architecture-neutral
/// PCI enumerator.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EcamRegion {
    /// Physical base of the ECAM window. The config space of bus `b`, device
    /// `d`, function `f` begins at `base + ((b - bus_start) << 20 | d << 15 | f << 12)`.
    pub base: PhysAddr,
    /// PCI segment group this window covers.
    pub segment: u16,
    /// First PCI bus number covered (inclusive).
    pub bus_start: u8,
    /// Last PCI bus number covered (inclusive).
    pub bus_end: u8,
}

impl EcamRegion {
    /// All-zero region, for fixed-size static-array initialisation.
    pub const ZERO: EcamRegion = EcamRegion {
        base: PhysAddr(0),
        segment: 0,
        bus_start: 0,
        bus_end: 0,
    };
}

/// Neutral platform-discovery operations, backed by the active architecture's
/// firmware parser.
pub trait ArchPlatform {
    /// Discover platform hardware from the firmware tables and cache the
    /// results. Sources its own firmware pointer (from the bootloader), so it
    /// takes no arguments — like [`ArchIrq::init`](crate::arch::irq::ArchIrq::init)
    /// and [`ArchTimer::init`](crate::arch::timer::ArchTimer::init). Missing or
    /// malformed tables are logged and treated as "no devices of that kind"
    /// rather than a hard failure at this stage; the consumers that require a
    /// given table report a precise error when they run.
    ///
    /// # Safety
    /// Ring-0 only; call once during boot after the HHDM is available (the
    /// firmware tables are read through it). Reads firmware-owned physical
    /// memory; performs no allocation.
    unsafe fn init() -> Result<(), AllocError>;

    /// The PCIe ECAM windows discovered from firmware. Empty if the platform
    /// exposes no PCIe segments (or [`init`](ArchPlatform::init) has not run).
    /// The slice is stable for the lifetime of the kernel (written once at
    /// boot, read-only thereafter).
    fn pcie_ecam_regions() -> &'static [EcamRegion];
}
