use super::*;

/// Probe and initialise an AHCI controller `DeviceNode`, publishing its first SATA disk; returns
/// its fate (claimed, or declined with why).
pub fn init(controller: &ObjectRef) -> Outcome {
    // **One controller's disk is served**, through `AHCI`: a second is declined before it is touched.
    if AHCI.get().is_some() {
        crate::kprintln!("ahci: another controller's disk is served already; this one is declined");
        return declined("another AHCI controller is already claimed");
    }
    // SAFETY: `controller` pins a live `DeviceNode`.
    let dn: &DeviceNode = unsafe { &*(controller.as_ptr() as *const DeviceNode) };
    let desc = dn.descriptor();

    // BAR5 is the ABAR (AHCI 1.3). Map it uncached.
    let bar = desc.bars[5];
    if bar.size == 0 {
        crate::kprintln!("ahci: controller has no ABAR (BAR5)");
        return declined("the controller has no ABAR (BAR5)");
    }
    let pages = bar.size.div_ceil(crate::mm::PAGE_SIZE as u64).max(1);
    // SAFETY: `bar.base` is the controller's MMIO ABAR from PCI BAR sizing.
    let abar = match unsafe { kvmap::map_mmio(PhysAddr(bar.base), pages) } {
        Ok(va) => va.as_u64() + (bar.base & (crate::mm::PAGE_SIZE as u64 - 1)),
        Err(_) => {
            crate::kprintln!("ahci: ABAR map failed");
            return declined("its ABAR could not be mapped");
        }
    };

    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_AE);
    let pi = read32(abar, HBA_PI);
    let cap = read32(abar, HBA_CAP);
    crate::kprintln!("ahci: HBA up (CAP {:#010x}, PI {:#010x})", cap, pi);

    // The driver writes 64-bit addresses into the command list, the FIS base and every PRDT entry.
    // An HBA without `S64A` would take the low half and DMA to the wrong place, so decline it.
    if cap & CAP_S64A == 0 {
        crate::kprintln!("ahci: HBA lacks 64-bit addressing (CAP.S64A clear) — declining");
        return declined("no 64-bit addressing (CAP.S64A clear)");
    }

    let mut port = u32::MAX;
    for p in 0..32u32 {
        if pi & (1 << p) == 0 {
            continue;
        }
        let pb = abar + PORT_BASE + p as u64 * PORT_STRIDE;
        if read32(pb, PX_SSTS) & 0xF == SSTS_DET_PRESENT && read32(pb, PX_SIG) == SIG_SATA {
            port = p;
            break;
        }
    }
    if port == u32::MAX {
        crate::kprintln!("ahci: no SATA disk on any implemented port");
        return declined(NO_SATA_DISK);
    }
    let cfg = match crate::pci::Config::map(desc) {
        Ok(c) => Some(c),
        Err(e) => {
            crate::kprintln!(
                "ahci: config space unreachable ({:?}) — INTx, and DMA is left relying on \
                 whatever the firmware set",
                e
            );
            None
        }
    };

    // **Bus mastering must be enabled before anything DMAs**: `start_port` sets FRE (the HBA then
    // writes received FISes to memory) and `identify` has it fetch a command list and a PRDT and
    // write 512 bytes back. Firmware that left the bit clear would otherwise time `identify` out.
    // Say which happened: leaving it on is a property of the firmware.
    if let Some(cfg) = &cfg {
        if crate::pci::enable_bus_master(cfg) {
            crate::kprintln!("ahci: bus master already enabled by firmware");
        } else {
            crate::kprintln!("ahci: bus master enabled by the driver");
        }
    }

    let (mut disk, name, name_len) = match claim_port(abar, port) {
        Ok(claimed) => claimed,
        Err(why) => {
            crate::kprintln!("ahci: port {}: {}", port, why);
            return declined(why);
        }
    };
    let sectors = disk.sectors;
    crate::kprintln!(
        "ahci: port {} disk ready ({} sectors, {} MiB): {}",
        port,
        sectors,
        sectors * SECTOR_SIZE as u64 / (1024 * 1024),
        Printable(&name[..name_len])
    );
    // Its own line: the one above is matched by a gate.
    crate::kprintln!(
        "ahci: port {} flushes with {}",
        port,
        if disk.flush_command == ATA_FLUSH_CACHE_EXT { "FLUSH CACHE EXT" } else { "FLUSH CACHE" }
    );

    // **Every step that can fail precedes the publish**, which nothing undoes (a watch thread runs).
    let Some(node) = disk_node(controller, sectors, &name[..name_len], &disk) else {
        give_back(&mut disk);
        return declined("the disk's device node could not be allocated");
    };
    let Some(signal) = install_signal(&cfg, desc.interrupt.line as u32) else {
        give_back(&mut disk);
        return declined("no interrupt can reach the boot CPU");
    };
    let disk = match AHCI.publish(disk) {
        Ok(disk) => disk,
        Err(mut disk) => {
            give_back(&mut disk);
            return declined("another AHCI controller is already claimed");
        }
    };

    write32(abar, HBA_GHC, read32(abar, HBA_GHC) | GHC_IE);
    write32(abar, HBA_IS, read32(abar, HBA_IS)); // clear stale

    // SAFETY: adopt the creation reference into the device table.
    let r = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
    crate::device::register(r.clone(), "ahci");
    disk.counted.attach(&r);
    Outcome::Claimed { driver: DRIVER, signal }
}

/// **The controller's interrupt**: MSI, which needs no interrupt-line register (UEFI often leaves
/// it meaningless), else INTx from `gsi`; `None`, logged, if neither reaches the boot CPU.
fn install_signal(cfg: &Option<crate::pci::Config>, gsi: u32) -> Option<Signal> {
    if let Some(cfg) = cfg
        && let Some(msi) = crate::pci::read_msi(cfg)
    {
        // SAFETY: ring-0, post-Irq::init; `isr` stays valid for the kernel's
        // lifetime, and nothing can be delivered until the message is written.
        match unsafe { crate::arch::IrqInstall::install_msi(isr) } {
            // SAFETY: the handler for `msg.vector` was just registered, so
            // the device may write the message from here on.
            Some(msg) if unsafe { crate::pci::program_msi(cfg, &msi, msg.address, msg.data) } => {
                // MSI does not suppress INTx, and a device able to do both
                // would assert a line nothing is listening on.
                crate::pci::set_intx_disabled(cfg, true);
                crate::kprintln!(
                    "ahci: irq via MSI (vec {:#04x}, addr {:#x}, data {:#06x}, {}-bit cap at {:#04x})",
                    msg.vector,
                    msg.address,
                    msg.data,
                    if msi.addr64 { 64 } else { 32 },
                    msi.off
                );
                return Some(Signal::Msi { vector: msg.vector });
            }
            Some(_) => crate::kprintln!(
                "ahci: MSI declined — the capability cannot hold the message address; \
                     falling back"
            ),
            None => crate::kprintln!("ahci: MSI declined — this CPU cannot be named in a message; falling back"),
        }
    }
    // The GSI is the PCI interrupt-line register's (ACPI `_PRT` routing is deferred).
    // SAFETY: ring-0, post-IrqRouter::init; `isr` stays valid for the kernel's lifetime.
    let Some(vec) = (unsafe { crate::arch::IrqInstall::install_intx(gsi, isr) }) else {
        crate::kprintln!("ahci: GSI {} cannot be routed to the boot CPU", gsi);
        return None;
    };
    if let Some(cfg) = cfg {
        crate::pci::set_intx_disabled(cfg, false);
    }
    crate::kprintln!("ahci: irq via INTx (GSI {}, vec {:#04x})", gsi, vec);
    Some(Signal::Intx { gsi, vector: vec })
}

/// A claimed disk, and its name's bytes and length.
pub(super) type Claimed = (KBox<AhciDisk>, [u8; MAX_DEVICE_NAME], usize);

/// **Claim the SATA disk on `port`**: every allocation made first, so none fails with the port
/// running, then the port started and the disk identified; or why not, its memory freed if it stopped.
pub(super) fn claim_port(abar: u64, port: u32) -> Result<Claimed, &'static str> {
    let port_base = abar + PORT_BASE + port as u64 * PORT_STRIDE;
    // **A port that will not stop is given nothing**: a command list may be given only to a stopped
    // port, and the one running may be the firmware's.
    if stop_port(port_base).is_none() {
        return Err("its port would not stop");
    }
    let (Ok(cmd_list), Ok(fis), Ok(cmd_table), Ok(data)) = (
        DmaBuffer::alloc(1024),
        DmaBuffer::alloc(256),
        DmaBuffer::alloc(CMD_TABLE_BYTES),
        DmaBuffer::alloc(SECTOR_SIZE as usize),
    ) else {
        return Err(OUT_OF_MEMORY);
    };
    let (Ok(intr), Ok(wake)) = (InterruptObject::try_new(), InterruptObject::try_new()) else {
        return Err(OUT_OF_MEMORY);
    };
    let disk = AhciDisk {
        abar,
        port_base,
        port,
        cmd_list: Programmed::new(cmd_list),
        _fis: Programmed::new(fis),
        cmd_table: Programmed::new(cmd_table),
        sectors: 0,
        flush_command: ATA_FLUSH_CACHE,
        state: IrqSpinLock::new(LockRank::Leaf, PortState::new()),
        // SAFETY: a fresh `InterruptObject`, adopted under its own tag.
        intr: unsafe { crate::drivers::adopt(intr, KObjectType::InterruptObject) },
        counted: Counted::new(),
        // SAFETY: as above.
        wake: unsafe { crate::drivers::adopt(wake, KObjectType::InterruptObject) },
    };
    let Ok(mut disk) = KBox::try_new(disk) else {
        return Err(OUT_OF_MEMORY);
    };

    write32(port_base, PX_CLB, disk.cmd_list.phys().as_u64() as u32);
    write32(port_base, PX_CLBU, (disk.cmd_list.phys().as_u64() >> 32) as u32);
    write32(port_base, PX_FB, disk._fis.phys().as_u64() as u32);
    write32(port_base, PX_FBU, (disk._fis.phys().as_u64() >> 32) as u32);
    write32(port_base, PX_SERR, 0xFFFF_FFFF); // clear errors (write-1-to-clear)
    write32(port_base, PX_IS, 0xFFFF_FFFF);
    write32(port_base, PX_IE, PXIE_DHRE | PX_FATAL);
    start_port(port_base);

    let mut name = [0u8; MAX_DEVICE_NAME];
    // SAFETY: `disk` is the live state, not yet published: IDENTIFY is polled.
    let Some((sectors, name_len, flush)) = (unsafe { identify(&disk, Programmed::new(data), &mut name) }) else {
        give_back(&mut disk);
        return Err("IDENTIFY failed");
    };
    disk.sectors = sectors;
    disk.flush_command = flush;
    Ok((disk, name, name_len))
}
