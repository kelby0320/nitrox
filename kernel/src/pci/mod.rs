//! PCI(e) enumeration over ECAM: walks the firmware-discovered ECAM windows
//! ([`crate::arch::platform::ArchPlatform::pcie_ecam_regions`]) and builds an
//! architecture-independent [`DeviceNode`] per present function. Only *where the ECAM window
//! lives* is arch-specific ([`EcamRegion`]s). Bridge traversal beyond the ECAM bus range is
//! not done. See `docs/spec/device-node.md` and `docs/architecture/drivers-and-irps.md`.
//!
//! Each function's 4 KiB config space is at
//! `region.base + ((bus - bus_start) << 20 | dev << 15 | func << 12)`. Enumeration reserves **one**
//! vmap page, a [`MmioWindow`], and repoints it per function.

use crate::arch::Platform;
use crate::arch::platform::{ArchPlatform, EcamRegion};
use crate::libkern::{AllocError, KVec};
use crate::mm::kvmap::{self, MmioWindow};
use crate::mm::{Mmio, PAGE_SIZE, PhysAddr};
use crate::object::device_node::{
    BAR_FLAG_64, BAR_FLAG_PREFETCH, BAR_IO, BAR_MMIO, BAR_NONE, BarWindow, BlockGeometry, DeviceClass, DeviceIdentity,
    DeviceNode, InterruptSpec, ResourceDescriptor,
};
use crate::object::header::Ref;

const REG_ID: u16 = 0x00; // vendor_id | device_id << 16
const REG_COMMAND: u16 = 0x04; // command | status << 16
const REG_CLASS_REV: u16 = 0x08; // revision | prog_if << 8 | subclass << 16 | class << 24
const REG_HEADER: u16 = 0x0C; // cache | latency << 8 | header_type << 16 | bist << 24
const REG_BAR0: u16 = 0x10; // first of six BAR slots
const REG_INTERRUPT: u16 = 0x3C; // int_line | int_pin << 8

/// `command`/`status` bit 0 (I/O space enable) + bit 1 (memory space enable).
const CMD_DECODE_BITS: u32 = 0b11;
/// Vendor id read from an absent function.
const VENDOR_ABSENT: u16 = 0xFFFF;
/// Header-type bit 7: the device is multi-function.
const HEADER_MULTIFUNCTION: u8 = 0x80;

/// Read/write access to one function's configuration space, abstract so decoding/sizing is
/// host-testable. `pub(crate)` because a driver programming its own capabilities needs one.
pub(crate) trait Cfg {
    fn read32(&self, off: u16) -> u32;
    fn write32(&self, off: u16, val: u32);
}

/// A function's 4 KiB configuration space, each access held to it.
struct MmioCfg<'a>(Mmio<'a>);

impl Cfg for MmioCfg<'_> {
    fn read32(&self, off: u16) -> u32 {
        self.0.read32(off as u64)
    }

    fn write32(&self, off: u16, val: u32) {
        self.0.write32(off as u64, val)
    }
}

fn func_phys(region: &EcamRegion, bus: u8, dev: u8, func: u8) -> PhysAddr {
    let off = (((bus - region.bus_start) as u64) << 20) | ((dev as u64) << 15) | ((func as u64) << 12);
    PhysAddr(region.base.as_u64() + off)
}

fn header_type_raw<C: Cfg>(cfg: &C) -> u8 {
    ((cfg.read32(REG_HEADER) >> 16) & 0xFF) as u8
}

fn decode_identity<C: Cfg>(cfg: &C) -> DeviceIdentity {
    let id = cfg.read32(REG_ID);
    let class = cfg.read32(REG_CLASS_REV);
    DeviceIdentity {
        vendor: (id & 0xFFFF) as u16,
        device: ((id >> 16) & 0xFFFF) as u16,
        revision: (class & 0xFF) as u8,
        prog_if: ((class >> 8) & 0xFF) as u8,
        subclass: ((class >> 16) & 0xFF) as u8,
        class: ((class >> 24) & 0xFF) as u8,
    }
}

/// Decode the legacy interrupt line/pin. `gsi`/`trigger`/`polarity` stay unresolved until a
/// driver routes the pin.
fn read_interrupt<C: Cfg>(cfg: &C) -> InterruptSpec {
    let d = cfg.read32(REG_INTERRUPT);
    let line = (d & 0xFF) as u8;
    let pin = ((d >> 8) & 0xFF) as u8;
    InterruptSpec { gsi: 0, trigger: 0, polarity: 0, line, pin, present: (pin != 0) as u8, _pad: 0 }
}

/// Size every BAR of a header-type-0 function: disable decode, write all-ones and read back the
/// size mask, restore, re-enable decode. A 64-bit memory BAR consumes its slot plus the next.
fn size_bars<C: Cfg>(cfg: &C) -> [BarWindow; 6] {
    let mut bars = [BarWindow::ZERO; 6];
    let cmd = cfg.read32(REG_COMMAND);
    write_command(cfg, (cmd & !CMD_DECODE_BITS) as u16);

    let mut i = 0usize;
    while i < 6 {
        let off = REG_BAR0 + (i as u16) * 4;
        let orig = cfg.read32(off);
        if orig & 0x1 != 0 {
            cfg.write32(off, 0xFFFF_FFFF);
            let mask = cfg.read32(off) & 0xFFFF_FFFC;
            cfg.write32(off, orig);
            if mask != 0 {
                bars[i] = BarWindow {
                    base: (orig & 0xFFFF_FFFC) as u64,
                    size: (!mask).wrapping_add(1) as u64,
                    kind: BAR_IO,
                    flags: 0,
                };
            }
            i += 1;
        } else if (orig >> 1) & 0x3 == 0x2 {
            let orig_hi = cfg.read32(off + 4);
            cfg.write32(off, 0xFFFF_FFFF);
            cfg.write32(off + 4, 0xFFFF_FFFF);
            let mask_lo = cfg.read32(off);
            let mask_hi = cfg.read32(off + 4);
            cfg.write32(off, orig);
            cfg.write32(off + 4, orig_hi);
            let mask = ((mask_hi as u64) << 32) | ((mask_lo & 0xFFFF_FFF0) as u64);
            if mask != 0 {
                let mut flags = BAR_FLAG_64;
                if (orig >> 3) & 0x1 != 0 {
                    flags |= BAR_FLAG_PREFETCH;
                }
                bars[i] = BarWindow {
                    base: ((orig_hi as u64) << 32) | ((orig & 0xFFFF_FFF0) as u64),
                    size: (!mask).wrapping_add(1),
                    kind: BAR_MMIO,
                    flags,
                };
            }
            i += 2;
        } else {
            cfg.write32(off, 0xFFFF_FFFF);
            let mask = cfg.read32(off) & 0xFFFF_FFF0;
            cfg.write32(off, orig);
            if mask != 0 {
                let mut flags = 0;
                if (orig >> 3) & 0x1 != 0 {
                    flags |= BAR_FLAG_PREFETCH;
                }
                bars[i] = BarWindow {
                    base: (orig & 0xFFFF_FFF0) as u64,
                    size: (!mask).wrapping_add(1) as u64,
                    kind: BAR_MMIO,
                    flags,
                };
            }
            i += 1;
        }
    }

    write_command(cfg, cmd as u16);
    bars
}

fn decode_function<C: Cfg>(cfg: &C, seg: u16, bus: u8, dev: u8, func: u8) -> ResourceDescriptor {
    let bars = if header_type_raw(cfg) & 0x7F == 0 {
        size_bars(cfg)
    } else {
        // Bridges (header type 1) have a different BAR layout; leave the windows empty.
        [BarWindow::ZERO; 6]
    };
    ResourceDescriptor {
        identity: decode_identity(cfg),
        bars,
        interrupt: read_interrupt(cfg),
        seg,
        bus,
        dev,
        func,
        _pad: [0; 3],
    }
}

fn log_function(desc: &ResourceDescriptor, caps: &Capabilities) {
    let id = &desc.identity;
    crate::kprintln!(
        "pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}.{:02x} pin {} {}",
        desc.bus,
        desc.dev,
        desc.func,
        id.vendor,
        id.device,
        id.class,
        id.subclass,
        id.prog_if,
        desc.interrupt.pin,
        caps
    );
    let mut i = 0;
    while i < desc.bars.len() {
        let bar = &desc.bars[i];
        if bar.kind != BAR_NONE {
            let kind = if bar.kind == BAR_MMIO { "mmio" } else { "io" };
            crate::kprintln!("  bar{} {} base {:#x} size {:#x}", i, kind, bar.base, bar.size);
        }
        i += 1;
    }
}

enum Probe {
    Absent,
    Present { multifunction: bool },
}

type Found = KVec<Ref<DeviceNode>>;

/// Probe `(bus, dev, func)`: repoint the window and, if a device is present, build, log and push
/// its `DeviceNode`.
fn probe_function(
    region: &EcamRegion,
    win: &mut MmioWindow,
    bus: u8,
    dev: u8,
    func: u8,
    out: &mut Found,
    found: &mut usize,
) -> Probe {
    // SAFETY: `func_phys` is a 4 KiB-aligned config-space frame within `region`.
    let Ok(regs) = (unsafe { win.repoint(func_phys(region, bus, dev, func)) }) else {
        return Probe::Absent;
    };
    let cfg = MmioCfg(regs);
    if (cfg.read32(REG_ID) & 0xFFFF) as u16 == VENDOR_ABSENT {
        return Probe::Absent;
    }
    let multifunction = header_type_raw(&cfg) & HEADER_MULTIFUNCTION != 0;
    let desc = decode_function(&cfg, region.segment, bus, dev, func);
    let caps = Capabilities::read(&cfg);
    match DeviceNode::try_new(DeviceClass::Other, desc, BlockGeometry::ZERO) {
        Ok(node) => {
            log_function(&desc, &caps);
            *found += out.try_push(Ref::new(node)).is_ok() as usize;
        }
        Err(_) => crate::kprintln!("pci: {:02x}:{:02x}.{} node alloc failed", bus, dev, func),
    }
    Probe::Present { multifunction }
}

fn scan_device(region: &EcamRegion, win: &mut MmioWindow, bus: u8, dev: u8, out: &mut Found, n: &mut usize) {
    if let Probe::Present { multifunction } = probe_function(region, win, bus, dev, 0, out, n)
        && multifunction
    {
        for func in 1u8..8 {
            probe_function(region, win, bus, dev, func, out, n);
        }
    }
}

/// Enumerate every PCI(e) function reachable through the firmware ECAM windows, returning one
/// owning [`DeviceNode`] reference per present function.
///
/// Must run after the allocators, HHDM, kvmap and [`ArchPlatform::init`]. Performs no I/O
/// beyond config reads and BAR sizing.
pub fn enumerate() -> KVec<Ref<DeviceNode>> {
    let mut out = KVec::new();
    let regions = Platform::pcie_ecam_regions();
    if regions.is_empty() {
        crate::kprintln!("pci: no ECAM regions discovered");
        return out;
    }
    let mut win = match MmioWindow::new() {
        Ok(v) => v,
        Err(_) => {
            crate::kprintln!("pci: scan-window allocation failed; skipping enumeration");
            return out;
        }
    };
    let mut found = 0usize;
    for region in regions {
        for bus in region.bus_start..=region.bus_end {
            for dev in 0u8..32 {
                scan_device(region, &mut win, bus, dev, &mut out, &mut found);
            }
        }
    }
    crate::kprintln!("pci: enumeration complete ({} function(s) found)", found);
    out
}

/// Why a function's configuration space could not be reached.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// No firmware ECAM window covers this function's segment and bus.
    NoEcamWindow,
    /// The window is covered but could not be mapped (out of VA or page tables).
    Unmappable(AllocError),
}

/// A function's 4 KiB configuration space, mapped uncached for as long as the holder keeps it.
///
/// **The mapping is permanent**: a device mapping never gives its VA back, so dropping a `Config`
/// returns nothing. A page per claimed device is cheap (Tier 1 drivers are few, bound once at
/// boot); a shared window behind a lock would cost a lock on a post-scheduler path.
pub(crate) struct Config {
    inner: MmioCfg<'static>,
}

impl Config {
    /// Map the configuration space of the function `desc` describes. Boot-time.
    pub(crate) fn map(desc: &ResourceDescriptor) -> Result<Config, ConfigError> {
        let region = Platform::pcie_ecam_regions()
            .iter()
            .find(|r| r.segment == desc.seg && desc.bus >= r.bus_start && desc.bus <= r.bus_end)
            .ok_or(ConfigError::NoEcamWindow)?;
        let phys = func_phys(region, desc.bus, desc.dev, desc.func);
        // SAFETY: `phys` is a 4 KiB-aligned config-space frame inside a firmware
        // -described ECAM window, which is device memory and never RAM.
        let regs = unsafe { kvmap::map_mmio(phys, PAGE_SIZE as u64) }.map_err(ConfigError::Unmappable)?;
        Ok(Config { inner: MmioCfg(regs) })
    }
}

impl Cfg for Config {
    fn read32(&self, off: u16) -> u32 {
        self.inner.read32(off)
    }
    fn write32(&self, off: u16, val: u32) {
        self.inner.write32(off, val)
    }
}

/// `status` bit 4: the function implements a capability list.
const STATUS_CAP_LIST: u32 = 1 << 4;
/// Config offset of the capability-list pointer (header types 0 and 1 alike).
const REG_CAP_PTR: u16 = 0x34;
/// Capabilities live in `0x40..=0xFC`, dword-aligned (PCI 3.0 §6.7).
const CAP_FIRST: u16 = 0x40;
const CAP_LAST: u16 = 0xFC;

/// Capability ID: MSI (PCI 3.0 §6.8.1).
pub(crate) const CAP_ID_MSI: u8 = 0x05;

/// Config offset of the first capability with `id`, or `None`.
///
/// The walk is **bounded by the number of dword-aligned slots the list can occupy**, so a
/// circular or malformed chain terminates instead of hanging the boot.
pub(crate) fn find_capability<C: Cfg>(cfg: &C, id: u8) -> Option<u16> {
    capability_list(cfg).find(|&(cap, _)| cap == id).map(|(_, off)| off)
}

/// Every capability in the function's list as `(id, offset)`, in chain order; ends at a zero
/// next pointer, one outside the capability region, or after as many entries as it has slots.
fn capability_list<C: Cfg>(cfg: &C) -> impl Iterator<Item = (u8, u16)> + '_ {
    let mut off = if cfg.read32(REG_COMMAND) & (STATUS_CAP_LIST << 16) == 0 {
        0 // no list: an offset outside the region ends the walk before it reads anything
    } else {
        (cfg.read32(REG_CAP_PTR) & 0xFC) as u16
    };
    let mut slots = (CAP_LAST - CAP_FIRST) / 4 + 1;
    core::iter::from_fn(move || {
        if slots == 0 || !(CAP_FIRST..=CAP_LAST).contains(&off) {
            return None;
        }
        slots -= 1;
        let here = off;
        let hdr = cfg.read32(here);
        off = ((hdr >> 8) & 0xFC) as u16;
        Some(((hdr & 0xFF) as u8, here))
    })
}

/// Capability ID: PCI Express (PCIe base spec §7.5.3).
const CAP_ID_PCIE: u8 = 0x10;
/// Capability ID: MSI-X (PCI 3.0 §6.8.2).
const CAP_ID_MSIX: u8 = 0x11;

/// What a function's capability list holds, as a hardware report names it: its MSI form, MSI-X,
/// PCI Express, and how many entries of any other kind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Capabilities {
    /// `None` without an MSI capability, `Some(None)` with one [`read_msi`] refuses.
    msi: Option<Option<MsiCap>>,
    msix: bool,
    pcie: bool,
    other: u32,
}

impl Capabilities {
    fn read<C: Cfg>(cfg: &C) -> Capabilities {
        let mut caps = Capabilities { msi: None, msix: false, pcie: false, other: 0 };
        for (id, _) in capability_list(cfg) {
            match id {
                CAP_ID_MSI => caps.msi = Some(read_msi(cfg)),
                CAP_ID_MSIX => caps.msix = true,
                CAP_ID_PCIE => caps.pcie = true,
                _ => caps.other += 1,
            }
        }
        caps
    }
}

impl core::fmt::Display for Capabilities {
    /// `caps msi64 pcie +2`: the MSI form (`msi32`/`msi64`, `/mask` with per-vector masking,
    /// `msi-malformed` for one the driver would refuse), `msix`, `pcie`, then `+N` other
    /// entries — or `caps none`.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("caps")?;
        let mut any = false;
        match self.msi {
            Some(Some(m)) => {
                let width = if m.addr64 { 64 } else { 32 };
                write!(f, " msi{width}{}", if m.per_vector_mask { "/mask" } else { "" })?;
                any = true;
            }
            Some(None) => {
                f.write_str(" msi-malformed")?;
                any = true;
            }
            None => {}
        }
        if self.msix {
            f.write_str(" msix")?;
            any = true;
        }
        if self.pcie {
            f.write_str(" pcie")?;
            any = true;
        }
        if self.other > 0 {
            write!(f, " +{}", self.other)?;
            any = true;
        }
        if !any {
            f.write_str(" none")?;
        }
        Ok(())
    }
}

// --- MSI: the layout is selected by Message Control bit 7 (Message Data at `+0x0C` when the
// address is 64-bit, `+0x08` when 32-bit). QEMU's ICH9 AHCI is 64-bit, the laptop's Sunrise
// Point-LP 32-bit, which no QEMU boot exercises (`docs/planning/phase-5-bare-metal.md`). ---

/// Message Control bit 0: MSI enable.
const MSI_CTL_ENABLE: u16 = 1 << 0;
/// Message Control bits 3:1: log2 of the vectors the function can be given.
const MSI_CTL_MMC_SHIFT: u16 = 1;
/// Message Control bits 6:4: log2 of the vectors it is actually given.
const MSI_CTL_MME_MASK: u16 = 0b111 << 4;
/// Message Control bit 7: the message address is 64-bit.
const MSI_CTL_ADDR64: u16 = 1 << 7;
/// Message Control bit 8: per-vector masking, and hence Mask/Pending Bits.
const MSI_CTL_PVM: u16 = 1 << 8;

/// What a driver needs in order to program a function's MSI capability.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) struct MsiCap {
    /// Config offset of the capability header.
    pub off: u16,
    /// Message Control bit 7 — **which selects the structure**, not merely the address width.
    /// See [`msi_data_offset`].
    pub addr64: bool,
    /// Message Control bit 8. When clear the capability has no Mask or Pending Bits registers
    /// and ends after Message Data.
    pub per_vector_mask: bool,
    /// Message Control bits 3:1: log2 of the vectors the function can accept; we request one.
    pub multi_message_capable: u8,
}

/// Config offset of this capability's **Message Data** register: `+0x0C` in the 64-bit form,
/// `+0x08` in the 32-bit form. Writing it at the wrong one leaves Data zero (vector 0).
pub(crate) fn msi_data_offset(cap: &MsiCap) -> u16 {
    if cap.addr64 { cap.off + 0x0C } else { cap.off + 0x08 }
}

/// Decode the function's MSI capability, or `None` if it has none.
pub(crate) fn read_msi<C: Cfg>(cfg: &C) -> Option<MsiCap> {
    let off = find_capability(cfg, CAP_ID_MSI)?;
    let ctl = (cfg.read32(off) >> 16) as u16;
    let cap = MsiCap {
        off,
        addr64: ctl & MSI_CTL_ADDR64 != 0,
        per_vector_mask: ctl & MSI_CTL_PVM != 0,
        multi_message_capable: ((ctl >> MSI_CTL_MMC_SHIFT) & 0b111) as u8,
    };
    // **The structure has to fit inside the 256-byte header.** `CAP_LAST` bounds where a
    // capability may *start*: MSI at `0xF4` or above would have `program_msi` writing into
    // extended configuration space. Refuse it, and the caller falls back to INTx.
    if msi_data_offset(&cap) as u32 + 4 > 0x100 {
        return None;
    }
    Some(cap)
}

/// Point the function's MSI capability at `addr`/`data` and enable it with **one** vector
/// (Multiple Message Enable = 0); `addr` and `data` are the architecture's business
/// ([`crate::arch::msi_message`]). Returns `false` without writing anything if `addr` does not
/// fit the capability's form, leaving the caller to fall back to INTx.
///
/// # Safety
/// Ring-0, boot-time. On a `true` return the device may write `data` to `addr` at
/// any time, so the vector's handler must already be registered and the driver
/// ready to receive it. The caller is also responsible for disabling the
/// function's INTx (see [`set_intx_disabled`]), which MSI does not do by itself.
pub(crate) unsafe fn program_msi<C: Cfg>(cfg: &C, cap: &MsiCap, addr: u64, data: u16) -> bool {
    // **Refuse rather than truncate**: a 32-bit capability has nowhere to put the upper half.
    if !cap.addr64 && addr > u32::MAX as u64 {
        return false;
    }
    // Message Address: dword-aligned, its low two bits reserved-zero.
    cfg.write32(cap.off + 0x04, (addr & 0xFFFF_FFFC) as u32);
    if cap.addr64 {
        cfg.write32(cap.off + 0x08, (addr >> 32) as u32);
    }
    // Message Data is 16 bits and shares its dword with reserved space: read-modify-write.
    let data_off = msi_data_offset(cap);
    let keep = cfg.read32(data_off) & 0xFFFF_0000;
    cfg.write32(data_off, keep | data as u32);
    // Enable, one vector. The header's low half (id, next) is read-only.
    let hdr = cfg.read32(cap.off);
    let ctl = ((hdr >> 16) as u16 & !MSI_CTL_MME_MASK) | MSI_CTL_ENABLE;
    cfg.write32(cap.off, (hdr & 0xFFFF) | ((ctl as u32) << 16));
    true
}

/// `command` bit 2: the function may act as a DMA bus master.
const CMD_BUS_MASTER: u32 = 1 << 2;
/// `command` bit 10: the function's INTx assertion is suppressed.
const CMD_INTX_DISABLE: u32 = 1 << 10;

/// Write the 16-bit `command` register **without disturbing `status`**, whose error bits
/// (Master Abort, Target Abort, Parity Error, Signalled System Error) are **write-1-to-clear**:
/// the whole dword is written with the status half zeroed.
fn write_command<C: Cfg>(cfg: &C, command: u16) {
    cfg.write32(REG_COMMAND, command as u32);
}

/// Let the function act as a DMA bus master, returning whether it already could (the firmware
/// hands every function over with the bit set, which is not a property of the machine).
pub(crate) fn enable_bus_master<C: Cfg>(cfg: &C) -> bool {
    let cs = cfg.read32(REG_COMMAND);
    let was = cs & CMD_BUS_MASTER != 0;
    if !was {
        write_command(cfg, (cs | CMD_BUS_MASTER) as u16);
    }
    was
}

/// Suppress or allow the function's legacy INTx; enabling MSI does not do this.
pub(crate) fn set_intx_disabled<C: Cfg>(cfg: &C, disabled: bool) {
    let cs = cfg.read32(REG_COMMAND);
    let next = if disabled { cs | CMD_INTX_DISABLE } else { cs & !CMD_INTX_DISABLE };
    if next != cs {
        write_command(cfg, next as u16);
    }
}

// --- Power management: a function in D3 answers configuration cycles but not memory ones (its
// registers read as all ones); the laptop's xHCI was found in D3. ---

/// Capability ID: PCI Power Management (PCI PM 1.2 §3.2).
const CAP_ID_PM: u8 = 0x01;
/// PMCSR, at the capability's `+4`: bits 1:0 are the power state.
const PMCSR_STATE: u32 = 0b11;
/// PMCSR bit 3, `No_Soft_Reset`: a D3hot to D0 transition keeps the function's configuration.
const PMCSR_NO_SOFT_RESET: u32 = 1 << 3;
/// PMCSR bit 15, `PME_Status`: write one to clear, so a write that changes the state carries zero.
const PMCSR_PME_STATUS: u32 = 1 << 15;

/// What [`power_up`] found and did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Power {
    /// No power-management capability, so the function is always in D0.
    NoCapability,
    /// It was in D0 already.
    AlreadyD0,
    /// It was in D`from` and is asked for D0. **The caller waits 10 ms before touching it** (PCI
    /// PM 1.2 §5.4.1), and restores with [`Saved::restore`] when `kept` is false.
    Raised { from: u8, kept: bool },
}

/// The registers a D3hot-to-D0 transition resets on a function without `No_Soft_Reset`.
pub(crate) struct Saved {
    command: u32,
    bars: [u32; 6],
}

impl Saved {
    /// Write the BARs back, then the command register, which enables decoding them.
    pub(crate) fn restore<C: Cfg>(&self, cfg: &C) {
        for (i, &bar) in self.bars.iter().enumerate() {
            cfg.write32(REG_BAR0 + 4 * i as u16, bar);
        }
        write_command(cfg, self.command as u16);
    }
}

/// Put the function in D0, having saved what a resetting transition would lose (configuration
/// cycles reach a function in D3hot, so the saved values are live).
pub(crate) fn power_up<C: Cfg>(cfg: &C) -> (Power, Saved) {
    let mut saved = Saved { command: cfg.read32(REG_COMMAND), bars: [0; 6] };
    for (i, bar) in saved.bars.iter_mut().enumerate() {
        *bar = cfg.read32(REG_BAR0 + 4 * i as u16);
    }
    let Some(cap) = find_capability(cfg, CAP_ID_PM) else {
        return (Power::NoCapability, saved);
    };
    let pmcsr = cfg.read32(cap + 4);
    let from = (pmcsr & PMCSR_STATE) as u8;
    if from == 0 {
        return (Power::AlreadyD0, saved);
    }
    cfg.write32(cap + 4, pmcsr & !PMCSR_STATE & !PMCSR_PME_STATUS);
    (Power::Raised { from, kept: pmcsr & PMCSR_NO_SOFT_RESET != 0 }, saved)
}

#[cfg(test)]
mod tests;
