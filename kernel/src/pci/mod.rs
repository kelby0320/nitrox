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
use crate::libkern::{KBox, KVec};
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
trait Cfg {
    fn read32(&self, off: u16) -> u32;
    fn write32(&self, off: u16, val: u32);
}

/// Config access through a mapped, uncached MMIO window.
struct MmioCfg {
    base: *mut u8,
}

impl Cfg for MmioCfg {
    fn read32(&self, off: u16) -> u32 {
        // SAFETY: `base` is a live uncached 4 KiB config window; `off` is a
        // dword-aligned offset within it (all callers pass `< 0x100`).
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
    cfg.write32(REG_COMMAND, cmd & !CMD_DECODE_BITS);

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

    cfg.write32(REG_COMMAND, cmd);
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
