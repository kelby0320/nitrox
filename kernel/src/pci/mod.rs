//! PCI(e) enumeration over ECAM.
//!
//! Walks the PCIe configuration space exposed by the firmware-discovered ECAM
//! windows ([`crate::arch::platform::ArchPlatform::pcie_ecam_regions`]) and
//! builds an architecture-independent [`DeviceNode`] per present function. PCI(e)
//! config space is a PCI-SIG standard identical across architectures, so this
//! module is neutral kernel code; only *where the ECAM window lives* is
//! arch-specific (ACPI MCFG vs. a DTB), and that already crossed the arch
//! boundary as [`EcamRegion`]s.
//!
//! This is Phase 2 slice 5, Part 1: enumeration + `DeviceNode` only. Driver
//! matching (claiming a node, marking it `Block`, routing its interrupt) is the
//! AHCI part; bridge/secondary-bus traversal beyond the ECAM bus range is
//! deferred (QEMU q35's devices sit on the buses the MCFG already covers). See
//! `docs/spec/device-node.md` and `docs/architecture/drivers-and-irps.md`.
//!
//! ## Config-space access
//!
//! Each function's 4 KiB config space is 4 KiB-aligned MMIO at
//! `region.base + ((bus - bus_start) << 20 | dev << 15 | func << 12)`. The vmap
//! allocator never reclaims VA, so rather than mapping every function (or the
//! whole multi-hundred-MiB ECAM window), enumeration reserves **one** vmap page
//! and repoints it per function with [`kvmap::remap_mmio_page`].

use crate::arch::Platform;
use crate::arch::platform::{ArchPlatform, EcamRegion};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::mm::{PhysAddr, VirtAddr, kvmap};
use crate::object::ObjectRef;
use crate::object::device_node::{
    BAR_FLAG_64, BAR_FLAG_PREFETCH, BAR_IO, BAR_MMIO, BAR_NONE, BarWindow, BlockGeometry,
    DeviceClass, DeviceIdentity, DeviceNode, InterruptSpec, ResourceDescriptor,
};

// PCI configuration-space register offsets (dword-aligned).
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

/// Read/write access to one function's configuration space. Abstracted so the
/// decoding/sizing logic is host-testable against a synthetic config space.
///
/// `pub(crate)` because a driver programming its own capabilities needs one —
/// see [`Config`], the window that outlives [`enumerate`].
pub(crate) trait Cfg {
    fn read32(&self, off: u16) -> u32;
    fn write32(&self, off: u16, val: u32);
}

/// Config access through a mapped, uncached MMIO window.
struct MmioCfg {
    base: *mut u8,
}

impl Cfg for MmioCfg {
    fn read32(&self, off: u16) -> u32 {
        // SAFETY: `base` is a live uncached 4 KiB config window and `off` is a
        // dword-aligned offset inside it. The bound that matters is the
        // window's, not the 256-byte header's: enumeration reads `< 0x100`, and
        // `read_msi` refuses a capability whose structure would run past `0x100`,
        // but either way every offset any caller forms stays well under 4 KiB.
        unsafe { core::ptr::read_volatile(self.base.add(off as usize) as *const u32) }
    }

    fn write32(&self, off: u16, val: u32) {
        // SAFETY: as `read32`; config-space dword writes are the documented way
        // to probe a BAR's size (write all-ones, read back the size mask).
        unsafe { core::ptr::write_volatile(self.base.add(off as usize) as *mut u32, val) }
    }
}

/// Physical base of `(bus, dev, func)`'s 4 KiB config space within `region`.
fn func_phys(region: &EcamRegion, bus: u8, dev: u8, func: u8) -> PhysAddr {
    let off = (((bus - region.bus_start) as u64) << 20)
        | ((dev as u64) << 15)
        | ((func as u64) << 12);
    PhysAddr(region.base.as_u64() + off)
}

/// Header-type byte, including the multi-function bit (bit 7).
fn header_type_raw<C: Cfg>(cfg: &C) -> u8 {
    ((cfg.read32(REG_HEADER) >> 16) & 0xFF) as u8
}

/// Decode the device's identity from its class/id registers.
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

/// Decode the legacy interrupt line/pin. `gsi`/`trigger`/`polarity` stay
/// unresolved until a driver routes the pin (the AHCI part).
fn read_interrupt<C: Cfg>(cfg: &C) -> InterruptSpec {
    let d = cfg.read32(REG_INTERRUPT);
    let line = (d & 0xFF) as u8;
    let pin = ((d >> 8) & 0xFF) as u8;
    InterruptSpec {
        gsi: 0,
        trigger: 0,
        polarity: 0,
        line,
        pin,
        present: (pin != 0) as u8,
        _pad: 0,
    }
}

/// Size every BAR of a header-type-0 function. Standard protocol: disable the
/// function's decode, write all-ones to a BAR and read back the size mask,
/// restore the original value, re-enable decode. A 64-bit memory BAR consumes
/// its slot plus the next.
fn size_bars<C: Cfg>(cfg: &C) -> [BarWindow; 6] {
    let mut bars = [BarWindow::ZERO; 6];
    let cmd = cfg.read32(REG_COMMAND);
    write_command(cfg, (cmd & !CMD_DECODE_BITS) as u16);

    let mut i = 0usize;
    while i < 6 {
        let off = REG_BAR0 + (i as u16) * 4;
        let orig = cfg.read32(off);
        if orig & 0x1 != 0 {
            // I/O-space BAR (always 32-bit; low two bits are flags).
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
            // 64-bit memory BAR (this slot + the next).
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
            // 32-bit memory BAR.
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

/// Build the full resource descriptor for one present function.
fn decode_function<C: Cfg>(cfg: &C, seg: u16, bus: u8, dev: u8, func: u8) -> ResourceDescriptor {
    let bars = if header_type_raw(cfg) & 0x7F == 0 {
        size_bars(cfg)
    } else {
        // Bridges (header type 1) have a different BAR layout; their traversal
        // is deferred, so leave the windows empty.
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

/// Log a discovered function.
fn log_function(desc: &ResourceDescriptor) {
    let id = &desc.identity;
    crate::kprintln!(
        "pci {:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}.{:02x}.{:02x} pin {}",
        desc.bus,
        desc.dev,
        desc.func,
        id.vendor,
        id.device,
        id.class,
        id.subclass,
        id.prog_if,
        desc.interrupt.pin
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

/// Outcome of probing one function slot.
enum Probe {
    Absent,
    Present { multifunction: bool },
}

/// Probe `(bus, dev, func)`: repoint the window, and if a device is present
/// build its `DeviceNode`, log it, and push an owning reference into `out`.
fn probe_function(
    region: &EcamRegion,
    win: VirtAddr,
    bus: u8,
    dev: u8,
    func: u8,
    out: &mut KVec<ObjectRef>,
    found: &mut usize,
) -> Probe {
    // SAFETY: `win` is the enumeration scan window reserved by `enumerate`;
    // `func_phys` is a 4 KiB-aligned config-space frame within `region`.
    if unsafe { kvmap::remap_mmio_page(win, func_phys(region, bus, dev, func)) }.is_err() {
        return Probe::Absent;
    }
    let cfg = MmioCfg {
        base: win.as_u64() as *mut u8,
    };
    if (cfg.read32(REG_ID) & 0xFFFF) as u16 == VENDOR_ABSENT {
        return Probe::Absent;
    }
    let multifunction = header_type_raw(&cfg) & HEADER_MULTIFUNCTION != 0;
    let desc = decode_function(&cfg, region.segment, bus, dev, func);
    match DeviceNode::try_new(DeviceClass::Other, desc, BlockGeometry::ZERO) {
        Ok(node) => {
            log_function(&desc);
            // SAFETY: `into_raw` yields the single creation reference; adopt it
            // as an `ObjectRef` of the matching type.
            let r = unsafe {
                ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode)
            };
            if out.try_push(r).is_ok() {
                *found += 1;
            }
        }
        Err(_) => crate::kprintln!("pci: {:02x}:{:02x}.{} node alloc failed", bus, dev, func),
    }
    Probe::Present { multifunction }
}

/// Probe every function of one device slot (function 0, then 1..8 if the slot
/// reports multi-function).
fn scan_device(
    region: &EcamRegion,
    win: VirtAddr,
    bus: u8,
    dev: u8,
    out: &mut KVec<ObjectRef>,
    found: &mut usize,
) {
    if let Probe::Present { multifunction } =
        probe_function(region, win, bus, dev, 0, out, found)
    {
        if multifunction {
            for func in 1u8..8 {
                probe_function(region, win, bus, dev, func, out, found);
            }
        }
    }
}

/// Enumerate every PCI(e) function reachable through the firmware ECAM windows,
/// returning one owning [`DeviceNode`] reference per present function. Logs each
/// function as it is discovered.
///
/// Must run after the allocators, the HHDM, the kvmap, and
/// [`ArchPlatform::init`] (so the ECAM regions are populated). Performs no I/O
/// beyond config-space reads (and the BAR-sizing read/restore writes).
pub fn enumerate() -> KVec<ObjectRef> {
    let mut out: KVec<ObjectRef> = KVec::new();
    let regions = Platform::pcie_ecam_regions();
    if regions.is_empty() {
        crate::kprintln!("pci: no ECAM regions discovered");
        return out;
    }
    let win = match kvmap::vmap_alloc_pages(1) {
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
                scan_device(region, win, bus, dev, &mut out, &mut found);
            }
        }
    }
    crate::kprintln!("pci: enumeration complete ({} function(s) found)", found);
    out
}

// --- Config space that outlives enumeration ---------------------------------
//
// `enumerate` reserves one scan window and repoints it per function, so it is
// gone by the time a driver is bound. A driver that must program its own
// capabilities (MSI) needs a window of its own, and `ResourceDescriptor` carries
// the `seg`/`bus`/`dev`/`func` the address is derived from.

/// Why a function's configuration space could not be reached.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConfigError {
    /// No firmware ECAM window covers this function's segment and bus. Either
    /// the descriptor did not come from [`enumerate`], or the firmware described
    /// a bus it does not map.
    NoEcamWindow,
    /// The window is covered but could not be mapped (out of VA or page tables).
    Unmappable(AllocError),
}

/// A function's 4 KiB configuration space, mapped uncached for as long as the
/// holder keeps it — the driver-side counterpart to [`enumerate`]'s shared scan
/// window.
///
/// **The mapping is permanent.** The vmap allocator never reclaims VA, so
/// dropping a `Config` returns nothing; one page per claimed device is the price,
/// and Tier 1 drivers are few and bound once at boot. A shared window behind a
/// lock would save that page and cost a lock on a path that runs after the
/// scheduler exists — the wrong trade at this count.
pub(crate) struct Config {
    inner: MmioCfg,
}

impl Config {
    /// Map the configuration space of the function `desc` describes.
    ///
    /// Boot-time; must run after [`ArchPlatform::init`] (the ECAM windows) and
    /// the kvmap.
    pub(crate) fn map(desc: &ResourceDescriptor) -> Result<Config, ConfigError> {
        let region = Platform::pcie_ecam_regions()
            .iter()
            .find(|r| r.segment == desc.seg && desc.bus >= r.bus_start && desc.bus <= r.bus_end)
            .ok_or(ConfigError::NoEcamWindow)?;
        let phys = func_phys(region, desc.bus, desc.dev, desc.func);
        // SAFETY: `phys` is a 4 KiB-aligned config-space frame inside a firmware
        // -described ECAM window, which is device memory and never RAM.
        let va = unsafe { kvmap::map_mmio(phys, 1) }.map_err(ConfigError::Unmappable)?;
        Ok(Config {
            inner: MmioCfg {
                base: va.as_u64() as *mut u8,
            },
        })
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

// --- The capability list ----------------------------------------------------

/// `status` bit 4: the function implements a capability list.
const STATUS_CAP_LIST: u32 = 1 << 4;
/// Config offset of the capability-list pointer (header types 0 and 1 alike).
const REG_CAP_PTR: u16 = 0x34;
/// Capabilities live in `0x40..=0xFC`, dword-aligned (PCI 3.0 §6.7).
const CAP_FIRST: u16 = 0x40;
const CAP_LAST: u16 = 0xFC;

/// Capability ID: MSI (PCI 3.0 §6.8.1).
pub(crate) const CAP_ID_MSI: u8 = 0x05;

/// Config offset of the first capability with `id`, or `None` if the function
/// has no capability list or does not implement one with that id.
///
/// The walk is **bounded by the number of dword-aligned slots the list can
/// occupy**, so a device that reports a circular or malformed chain terminates
/// the search instead of hanging the boot.
pub(crate) fn find_capability<C: Cfg>(cfg: &C, id: u8) -> Option<u16> {
    if cfg.read32(REG_COMMAND) & (STATUS_CAP_LIST << 16) == 0 {
        return None;
    }
    let mut off = (cfg.read32(REG_CAP_PTR) & 0xFC) as u16;
    for _ in 0..=((CAP_LAST - CAP_FIRST) / 4) {
        if !(CAP_FIRST..=CAP_LAST).contains(&off) {
            return None; // the chain ended (next = 0) or pointed out of range
        }
        let hdr = cfg.read32(off);
        if (hdr & 0xFF) as u8 == id {
            return Some(off);
        }
        off = ((hdr >> 8) & 0xFC) as u16;
    }
    None
}

// --- MSI --------------------------------------------------------------------
//
// The capability's *layout* is selected by Message Control bit 7, and it is not
// a widening: Message Data sits at `+0x0C` when the address is 64-bit and at
// `+0x08` when it is 32-bit. Both target machines' AHCI controllers matter here
// and they disagree — QEMU's ICH9 is 64-bit, the laptop's Sunrise Point-LP is
// 32-bit — and no QEMU boot can exercise the second. That is what the host tests
// below are for. See `docs/planning/phase-5-bare-metal.md` § "The divergence
// QEMU structurally cannot catch".

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
    /// Message Control bit 7 — **which selects the structure**, not merely the
    /// address width. See [`msi_data_offset`].
    pub addr64: bool,
    /// Message Control bit 8. When clear the capability has no Mask or Pending
    /// Bits registers at all and simply ends after Message Data; neither
    /// controller on either target machine sets it.
    pub per_vector_mask: bool,
    /// Message Control bits 3:1: log2 of the vectors the function can accept.
    /// We request one regardless, so this is recorded rather than acted on.
    pub multi_message_capable: u8,
}

/// Config offset of this capability's **Message Data** register.
///
/// `+0x0C` in the 64-bit form, where `+0x08` holds the upper address; `+0x08` in
/// the 32-bit form, where the capability then ends. Writing Data at the wrong one
/// leaves Data zero and the device signalling vector 0.
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
    // **The structure has to fit inside the 256-byte header.** `CAP_LAST` bounds
    // where a capability may *start*, not where it may end, so a function
    // advertising MSI at `0xF4` or above — malformed; the 64-bit form needs 14
    // bytes and the 32-bit form 10 — would have `program_msi` writing into
    // extended configuration space and over whatever capability begins at
    // `0x100`. Refuse it, and the caller falls back to INTx.
    if msi_data_offset(&cap) as u32 + 4 > 0x100 {
        return None;
    }
    Some(cap)
}

/// Point the function's MSI capability at `addr`/`data` and enable it with
/// **one** vector (Multiple Message Enable = 0).
///
/// `addr` and `data` are the architecture's business — see
/// [`crate::arch::msi_message`] — and the layout is this module's.
///
/// Returns `false` without writing anything if `addr` does not fit the
/// capability's form, leaving the caller to fall back to INTx.
///
/// # Safety
/// Ring-0, boot-time. On a `true` return the device may write `data` to `addr` at
/// any time, so the vector's handler must already be registered and the driver
/// ready to receive it. The caller is also responsible for disabling the
/// function's INTx (see [`set_intx_disabled`]), which MSI does not do by itself.
pub(crate) unsafe fn program_msi<C: Cfg>(cfg: &C, cap: &MsiCap, addr: u64, data: u16) -> bool {
    // **Refuse rather than truncate.** A 32-bit capability has nowhere to put the
    // upper half, and writing only the low one would point the device at a
    // different address than the caller was told it had. `install_msi` makes the
    // same choice about a destination that will not fit, and a silent truncation
    // is the exact shape of the bug this part exists to prevent.
    if !cap.addr64 && addr > u32::MAX as u64 {
        return false;
    }
    // Message Address: dword-aligned, its low two bits reserved-zero.
    cfg.write32(cap.off + 0x04, (addr & 0xFFFF_FFFC) as u32);
    if cap.addr64 {
        cfg.write32(cap.off + 0x08, (addr >> 32) as u32);
    }
    // Message Data is 16 bits and shares its dword with reserved space, so
    // read-modify-write rather than zeroing the other half.
    let data_off = msi_data_offset(cap);
    let keep = cfg.read32(data_off) & 0xFFFF_0000;
    cfg.write32(data_off, keep | data as u32);
    // Enable, one vector. The header's low half (id, next) is read-only, so
    // writing it back unchanged is a no-op on hardware.
    let hdr = cfg.read32(cap.off);
    let ctl = ((hdr >> 16) as u16 & !MSI_CTL_MME_MASK) | MSI_CTL_ENABLE;
    cfg.write32(cap.off, (hdr & 0xFFFF) | ((ctl as u32) << 16));
    true
}

// --- The command register ---------------------------------------------------

/// `command` bit 2: the function may act as a DMA bus master.
const CMD_BUS_MASTER: u32 = 1 << 2;
/// `command` bit 10: the function's INTx assertion is suppressed.
const CMD_INTX_DISABLE: u32 = 1 << 10;

/// Write the 16-bit `command` register **without disturbing `status`**, which
/// shares its dword.
///
/// `status`'s error bits (Master Abort, Target Abort, Parity Error, Signalled
/// System Error) are **write-1-to-clear**, so reading the dword and writing it
/// back silently clears whatever the function had latched — exactly the
/// diagnostics that matter on a machine we have not met. Writing zeros there
/// clears nothing, so the whole dword is written with the status half zeroed.
fn write_command<C: Cfg>(cfg: &C, command: u16) {
    cfg.write32(REG_COMMAND, command as u32);
}

/// Let the function act as a DMA bus master, returning whether it already could.
///
/// **Nothing in Nitrox used to do this**, and DMA worked anyway because the
/// firmware hands every function over with the bit already set. That is a
/// property of the firmware we boot under, not of the machine, so a driver that
/// DMAs sets it for itself.
pub(crate) fn enable_bus_master<C: Cfg>(cfg: &C) -> bool {
    let cs = cfg.read32(REG_COMMAND);
    let was = cs & CMD_BUS_MASTER != 0;
    if !was {
        write_command(cfg, (cs | CMD_BUS_MASTER) as u16);
    }
    was
}

/// Suppress or allow the function's legacy INTx assertion.
///
/// Enabling MSI does not do this, so a device left with INTx enabled can deliver
/// both — and the IOAPIC entry the INTx path routed is still live.
pub(crate) fn set_intx_disabled<C: Cfg>(cfg: &C, disabled: bool) {
    let cs = cfg.read32(REG_COMMAND);
    let next = if disabled {
        cs | CMD_INTX_DISABLE
    } else {
        cs & !CMD_INTX_DISABLE
    };
    if next != cs {
        write_command(cfg, next as u16);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::RefCell;

    /// A synthetic 256-byte config space that faithfully models BAR sizing:
    /// writing `0xFFFF_FFFF` to a dword "arms" it, and the next read returns the
    /// preset `sizing` mask instead of the stored value.
    struct FakeCfg {
        normal: RefCell<[u32; 64]>,
        sizing: [u32; 64],
        armed: RefCell<[bool; 64]>,
    }

    impl FakeCfg {
        fn new() -> Self {
            FakeCfg {
                normal: RefCell::new([0; 64]),
                sizing: [0; 64],
                armed: RefCell::new([false; 64]),
            }
        }
        fn set(&mut self, off: u16, val: u32) {
            self.normal.borrow_mut()[(off / 4) as usize] = val;
        }
        fn set_sizing(&mut self, off: u16, mask: u32) {
            self.sizing[(off / 4) as usize] = mask;
        }
    }

    impl Cfg for FakeCfg {
        fn read32(&self, off: u16) -> u32 {
            let idx = (off / 4) as usize;
            if self.armed.borrow()[idx] {
                self.sizing[idx]
            } else {
                self.normal.borrow()[idx]
            }
        }
        fn write32(&self, off: u16, val: u32) {
            let idx = (off / 4) as usize;
            if val == 0xFFFF_FFFF {
                self.armed.borrow_mut()[idx] = true;
            } else {
                self.normal.borrow_mut()[idx] = val;
                self.armed.borrow_mut()[idx] = false;
            }
        }
    }

    // --- Capability list + MSI ----------------------------------------------

    /// A config space with a capability list: `status` bit 4 set, the pointer at
    /// `0x34`, and one MSI capability at `off` carrying `ctl` as its Message
    /// Control. `next` chains onward (0 terminates).
    fn with_msi(off: u16, ctl: u16, next: u8) -> FakeCfg {
        let mut c = FakeCfg::new();
        c.set(REG_COMMAND, (STATUS_CAP_LIST << 16) | 0x0006);
        c.set(REG_CAP_PTR, off as u32);
        c.set(off, CAP_ID_MSI as u32 | ((next as u32) << 8) | ((ctl as u32) << 16));
        c
    }

    /// QEMU's ICH9 AHCI, read off a live boot: MSI at `0x80`, control `0x0080`.
    fn qemu_ahci_msi() -> FakeCfg {
        with_msi(0x80, 0x0080, 0xA8)
    }

    /// The laptop's Sunrise Point-LP AHCI, read from `lspci -vv`: MSI at `0x80`,
    /// `Count=1/1 Maskable- 64bit-` — so Message Control is `0x0000`.
    fn laptop_ahci_msi() -> FakeCfg {
        with_msi(0x80, 0x0000, 0xA8)
    }

    #[test]
    fn the_capability_walk_follows_the_chain_to_a_later_entry() {
        let mut c = FakeCfg::new();
        c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
        c.set(REG_CAP_PTR, 0x70);
        c.set(0x70, 0x01 | (0x80 << 8)); // power management -> 0x80
        c.set(0x80, CAP_ID_MSI as u32 | (0xA8 << 8));
        c.set(0xA8, 0x12); // SATA, end of chain
        assert_eq!(find_capability(&c, CAP_ID_MSI), Some(0x80));
        assert_eq!(find_capability(&c, 0x12), Some(0xA8), "the last entry is reachable");
        assert_eq!(find_capability(&c, 0x11), None, "MSI-X is not in this chain");
    }

    #[test]
    fn the_capability_walk_declines_when_the_status_bit_is_clear() {
        let mut c = qemu_ahci_msi();
        // Everything is still in place except the claim that a list exists. A
        // function without the bit may have anything at 0x34.
        c.set(REG_COMMAND, 0x0006);
        assert_eq!(find_capability(&c, CAP_ID_MSI), None);
    }

    #[test]
    fn the_capability_walk_terminates_on_a_circular_chain() {
        // Positive control first: the same two entries, chain terminated.
        let mut ok = FakeCfg::new();
        ok.set(REG_COMMAND, STATUS_CAP_LIST << 16);
        ok.set(REG_CAP_PTR, 0x40);
        ok.set(0x40, 0x01 | (0x50 << 8));
        ok.set(0x50, 0x10);
        assert_eq!(find_capability(&ok, 0x10), Some(0x50), "control: a sane chain resolves");

        // Now point the second entry back at the first. Searching for something
        // absent must return rather than walk forever.
        let mut loopy = ok;
        loopy.set(0x50, 0x10 | (0x40 << 8));
        assert_eq!(find_capability(&loopy, CAP_ID_MSI), None);
    }

    #[test]
    fn the_capability_walk_rejects_a_pointer_outside_the_capability_region() {
        let mut c = FakeCfg::new();
        c.set(REG_COMMAND, STATUS_CAP_LIST << 16);
        c.set(REG_CAP_PTR, 0x10); // inside the header — that is BAR0, not a capability
        // **Decodable on purpose, so the range check is the only thing that can
        // refuse it.** Written first without this line, where the walk stopped on
        // the zero `next` pointer instead and the test passed with the bound
        // deleted (PR #295 review, 2). A BAR whose low byte is `0x05` is not
        // far-fetched, and without the bound `read_msi` returns a capability at
        // `0x10` — after which `program_msi` writes the message address over BAR1
        // and the data over BAR2.
        c.set(0x10, CAP_ID_MSI as u32);
        assert_eq!(find_capability(&c, CAP_ID_MSI), None);
    }

    #[test]
    fn an_msi_capability_that_would_run_past_the_header_is_refused() {
        // Malformed: a 64-bit structure needs 14 bytes, so it cannot legally
        // start above 0xF0. `CAP_LAST` bounds where a capability may *start*, not
        // where it may end, so the walk finds this one and `read_msi` is what has
        // to decline it — otherwise Message Data lands at 0x100, in extended
        // configuration space, over whatever capability begins there.
        let c = with_msi(0xF4, 0x0080, 0);
        assert_eq!(find_capability(&c, CAP_ID_MSI), Some(0xF4), "the walk finds it");
        assert_eq!(read_msi(&c), None, "and read_msi refuses it");

        // The last offset that does fit, so the bound is not simply off-by-wide.
        let ok = with_msi(0xF0, 0x0080, 0);
        assert!(read_msi(&ok).is_some(), "0xF0 leaves exactly enough room");

        // The 32-bit form needs ten bytes and so fits four higher.
        let narrow = with_msi(0xF4, 0x0000, 0);
        assert!(read_msi(&narrow).is_some(), "0xF4 fits a 32-bit structure");
    }

    #[test]
    fn the_two_target_controllers_decode_to_different_shapes() {
        let qemu = read_msi(&qemu_ahci_msi()).expect("QEMU's AHCI advertises MSI");
        assert_eq!(qemu.off, 0x80);
        assert!(qemu.addr64, "QEMU's ICH9 AHCI is 64-bit");
        assert!(!qemu.per_vector_mask, "and non-maskable");
        assert_eq!(qemu.multi_message_capable, 0, "one vector");

        let laptop = read_msi(&laptop_ahci_msi()).expect("the laptop's AHCI advertises MSI");
        assert_eq!(laptop.off, 0x80, "at the same offset, which is the trap");
        assert!(!laptop.addr64, "the laptop's Sunrise Point-LP AHCI is 32-bit");
        assert!(!laptop.per_vector_mask);
        assert_eq!(laptop.multi_message_capable, 0);
    }

    /// **The test this whole part exists to make possible.** No QEMU boot can
    /// reach the 32-bit branch, because QEMU's AHCI is 64-bit; a driver shaped by
    /// the emulator writes Data at `+0x0C` and leaves the real Message Data —
    /// `+0x08` — holding whatever the upper-address write put there.
    #[test]
    fn a_32_bit_capability_takes_its_data_at_plus_8_and_nothing_beyond() {
        let c = laptop_ahci_msi();
        let cap = read_msi(&c).unwrap();
        assert_eq!(msi_data_offset(&cap), 0x88);
        // SAFETY: a synthetic config space; nothing is armed by this write.
        assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });

        assert_eq!(c.read32(0x84), 0xFEE0_0000, "message address");
        assert_eq!(c.read32(0x88) & 0xFFFF, 0x0031, "message data, at +0x08");
        assert_eq!(
            c.read32(0x8C),
            0,
            "+0x0C is past the end of a 32-bit non-maskable capability and must \
             not be written — on the laptop it is reserved space before the SATA \
             capability at 0xA8"
        );
    }

    #[test]
    fn a_64_bit_capability_takes_the_upper_address_at_plus_8_and_data_at_plus_c() {
        let c = qemu_ahci_msi();
        let cap = read_msi(&c).unwrap();
        assert_eq!(msi_data_offset(&cap), 0x8C);
        // SAFETY: a synthetic config space.
        assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });

        assert_eq!(c.read32(0x84), 0xFEE0_0000, "message address, low half");
        assert_eq!(c.read32(0x88), 0, "message address, upper half");
        assert_eq!(c.read32(0x8C) & 0xFFFF, 0x0031, "message data, at +0x0C");
    }

    #[test]
    fn programming_msi_preserves_the_reserved_half_of_the_data_dword() {
        let mut c = laptop_ahci_msi();
        c.set(0x88, 0xDEAD_0000); // reserved upper half, arbitrarily non-zero
        let cap = read_msi(&c).unwrap();
        // SAFETY: a synthetic config space.
        assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0031) });
        assert_eq!(c.read32(0x88), 0xDEAD_0031, "16-bit field, read-modify-write");
    }

    #[test]
    fn programming_msi_enables_exactly_one_vector() {
        // **Message Control arrives with MME already 3**, so clearing it is work
        // the write has to do rather than a property the fixture came with. The
        // first version of this test used 0x0086, whose MME is already zero, and
        // passed with `& !MSI_CTL_MME_MASK` deleted (PR #295 review, 3).
        // 0x00B6 = bit 7 (64-bit), bits 6:4 = 3 (enabled for eight), bits 3:1 = 3.
        let c = with_msi(0x80, 0x00B6, 0);
        let cap = read_msi(&c).unwrap();
        assert_eq!(cap.multi_message_capable, 3, "capable of 8");
        assert_ne!(
            (c.read32(0x80) >> 16) as u16 & MSI_CTL_MME_MASK,
            0,
            "precondition: the fixture has multiple messages enabled"
        );
        // SAFETY: a synthetic config space.
        assert!(unsafe { program_msi(&c, &cap, 0xFEE0_0000, 0x0030) });

        let ctl = (c.read32(0x80) >> 16) as u16;
        assert_ne!(ctl & MSI_CTL_ENABLE, 0, "enabled");
        assert_eq!(ctl & MSI_CTL_MME_MASK, 0, "multiple message enable = 1 vector");
        assert_eq!(c.read32(0x80) & 0xFF, CAP_ID_MSI as u32, "capability id preserved");
        assert_eq!((c.read32(0x80) >> 8) & 0xFF, 0, "next pointer preserved");
    }

    #[test]
    fn a_32_bit_capability_refuses_an_address_it_cannot_hold_and_writes_nothing() {
        let c = laptop_ahci_msi();
        let cap = read_msi(&c).unwrap();
        // SAFETY: a synthetic config space.
        let wrote = unsafe { program_msi(&c, &cap, 0x1_FEE0_0000, 0x0031) };
        assert!(!wrote, "refused rather than truncated to 0xFEE0_0000");
        assert_eq!(c.read32(0x84), 0, "message address untouched");
        assert_eq!(c.read32(0x88), 0, "message data untouched");
        assert_eq!((c.read32(0x80) >> 16) as u16 & MSI_CTL_ENABLE, 0, "and not enabled");

        // The same address is fine on a capability that has room for it.
        let wide = qemu_ahci_msi();
        let wide_cap = read_msi(&wide).unwrap();
        // SAFETY: a synthetic config space.
        assert!(unsafe { program_msi(&wide, &wide_cap, 0x1_FEE0_0000, 0x0031) });
        assert_eq!(wide.read32(0x88), 1, "upper half of the address");
    }

    // --- The command register ------------------------------------------------

    #[test]
    fn writing_the_command_register_never_writes_ones_into_the_status_half() {
        // `status` shares the dword and its error bits are write-1-to-clear, so
        // a read-modify-write of the whole dword clears whatever the function had
        // latched. Every write goes through `write_command`, which puts zeros
        // there — and zeros clear nothing on hardware.
        //
        // `FakeCfg` has no write-1-to-clear semantics, so what this asserts is the
        // property that matters: the *value we write* carries no ones above bit 15.
        const LATCHED: u32 = (1 << 13) | (1 << 15); // target abort, parity error
        for (what, run) in [
            ("enable_bus_master", &(|c: &FakeCfg| {
                enable_bus_master(c);
            }) as &dyn Fn(&FakeCfg)),
            ("set_intx_disabled", &(|c: &FakeCfg| set_intx_disabled(c, true))),
            ("size_bars", &(|c: &FakeCfg| {
                size_bars(c);
            })),
        ] {
            let mut c = FakeCfg::new();
            c.set(REG_COMMAND, 0x0003 | (LATCHED << 16));
            run(&c);
            assert_eq!(
                c.read32(REG_COMMAND) >> 16,
                0,
                "{what} wrote the status half back; on hardware that clears the \
                 abort and parity bits a first bring-up is diagnosed from"
            );
        }
    }

    #[test]
    fn enabling_bus_master_reports_whether_the_firmware_had_already_done_it() {
        // What both target machines' firmware actually hands over.
        let mut already = FakeCfg::new();
        already.set(REG_COMMAND, 0x0007);
        assert!(enable_bus_master(&already), "firmware had set it");
        assert_eq!(already.read32(REG_COMMAND) & 0xFFFF, 0x0007, "unchanged");

        // And a function handed over without it, which is what we must not assume
        // away: decode bits on, bus mastering off.
        let mut not_yet = FakeCfg::new();
        not_yet.set(REG_COMMAND, 0x0003);
        assert!(!enable_bus_master(&not_yet), "firmware had not");
        assert_eq!(not_yet.read32(REG_COMMAND) & 0xFFFF, 0x0007, "now it is set");
    }

    #[test]
    fn intx_disable_is_its_own_bit_and_leaves_the_decode_bits_alone() {
        let mut c = FakeCfg::new();
        c.set(REG_COMMAND, 0x0007);
        set_intx_disabled(&c, true);
        assert_eq!(c.read32(REG_COMMAND) & 0xFFFF, 0x0407, "bit 10 set, 0..2 intact");
        set_intx_disabled(&c, false);
        assert_eq!(c.read32(REG_COMMAND) & 0xFFFF, 0x0007, "and it is reversible");
    }

    /// A QEMU-ICH9-like AHCI controller header (header type 0, BAR5 = ABAR).
    fn ahci_fake() -> FakeCfg {
        let mut c = FakeCfg::new();
        c.set(REG_ID, 0x2922_8086); // vendor 8086, device 2922
        c.set(REG_COMMAND, 0x0000_0007); // I/O + mem + bus-master enabled
        c.set(REG_CLASS_REV, 0x0106_0102); // class 01, subclass 06, prog_if 01, rev 02
        c.set(REG_HEADER, 0x0000_0000); // header type 0, single-function
        // BAR5 (offset 0x24): 32-bit non-prefetchable MMIO, base 0xFEBF1000, 8 KiB.
        c.set(REG_BAR0 + 5 * 4, 0xFEBF_1000);
        c.set_sizing(REG_BAR0 + 5 * 4, 0xFFFF_E000);
        c.set(REG_INTERRUPT, 0x0000_0105); // line 5, pin 1 (INTA)
        c
    }

    #[test]
    fn decodes_ahci_identity() {
        let id = decode_identity(&ahci_fake());
        assert_eq!(id.vendor, 0x8086);
        assert_eq!(id.device, 0x2922);
        assert_eq!(id.class, 0x01);
        assert_eq!(id.subclass, 0x06);
        assert_eq!(id.prog_if, 0x01);
        assert_eq!(id.revision, 0x02);
    }

    #[test]
    fn sizes_32bit_mmio_abar() {
        let bars = size_bars(&ahci_fake());
        let abar = &bars[5];
        assert_eq!(abar.kind, BAR_MMIO);
        assert_eq!(abar.base, 0xFEBF_1000);
        assert_eq!(abar.size, 0x2000);
        assert_eq!(abar.flags, 0); // 32-bit, non-prefetchable
        // The unimplemented BARs stay absent.
        assert_eq!(bars[0].kind, BAR_NONE);
    }

    #[test]
    fn size_bars_restores_command_register() {
        let cfg = ahci_fake();
        let before = cfg.read32(REG_COMMAND);
        let _ = size_bars(&cfg);
        assert_eq!(cfg.read32(REG_COMMAND), before, "decode-enable bits not restored");
    }

    #[test]
    fn sizes_64bit_prefetchable_mmio() {
        let mut c = FakeCfg::new();
        c.set(REG_HEADER, 0); // header type 0
        // BAR0: 64-bit prefetchable memory at 0x8000_0000, 1 MiB.
        c.set(REG_BAR0, 0x8000_000C); // bits: mem(0)=0, type(2:1)=10, prefetch(3)=1
        c.set(REG_BAR0 + 4, 0x0000_0000);
        c.set_sizing(REG_BAR0, 0xFFF0_0000);
        c.set_sizing(REG_BAR0 + 4, 0xFFFF_FFFF);
        let bars = size_bars(&c);
        assert_eq!(bars[0].kind, BAR_MMIO);
        assert_eq!(bars[0].base, 0x8000_0000);
        assert_eq!(bars[0].size, 0x10_0000);
        assert_eq!(bars[0].flags, BAR_FLAG_64 | BAR_FLAG_PREFETCH);
        // The high slot was consumed, not reported as its own window.
        assert_eq!(bars[1].kind, BAR_NONE);
    }

    #[test]
    fn reads_interrupt_line_and_pin() {
        let int = read_interrupt(&ahci_fake());
        assert_eq!(int.line, 5);
        assert_eq!(int.pin, 1);
        assert_eq!(int.present, 1);
    }

    #[test]
    fn absent_function_has_no_pin() {
        let mut c = FakeCfg::new();
        c.set(REG_INTERRUPT, 0x0000_0000);
        let int = read_interrupt(&c);
        assert_eq!(int.pin, 0);
        assert_eq!(int.present, 0);
    }
}
