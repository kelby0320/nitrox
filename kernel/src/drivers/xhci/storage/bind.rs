//! Binding a bulk-only interface: its units, read and published, each with its partition table.

use super::*;

/// A bulk-only interface prepared for its device's one configuration: its endpoints' rings and its
/// pages, which [`bind`] puts in the table.
pub(in crate::drivers::xhci) struct Prepared {
    interface: u8,
    in_ep: super::desc::Endpoint,
    out_ep: super::desc::Endpoint,
    bulk_in: Ring,
    bulk_out: Ring,
    wrap_virt: u64,
    wrap_phys: u64,
    data_virt: u64,
    data_phys: u64,
}

impl Prepared {
    /// The endpoints Configure Endpoint adds for it.
    pub(in crate::drivers::xhci) fn endpoints(&self) -> [context::Bulk; 2] {
        let (i, o) = (&self.in_ep, &self.out_ep);
        [
            context::Bulk {
                dci: i.dci(),
                input: true,
                max_packet: i.max_packet,
                burst: i.burst,
                ring: self.bulk_in.phys,
                cycle: self.bulk_in.producer.cycle(),
            },
            context::Bulk {
                dci: o.dci(),
                input: false,
                max_packet: o.max_packet,
                burst: o.burst,
                ring: self.bulk_out.phys,
                cycle: self.bulk_out.producer.cycle(),
            },
        ]
    }
}

/// **The device's bulk-only interface, prepared**: its rings, a wrapper page and the binding's data
/// buffer, all the device's memory. `None` when it has none, or memory ran out, which is said.
pub(in crate::drivers::xhci) fn prepare(port: u8, mem: &mut DeviceMem, config: &[u8]) -> Option<Prepared> {
    let s = super::desc::storage_interface(config)?;
    let (Ok(rin), Ok(rout), Ok(wrap), Ok(data)) = (
        DmaBuffer::alloc(RING_TRBS * 16),
        DmaBuffer::alloc(RING_TRBS * 16),
        DmaBuffer::alloc(crate::mm::PAGE_SIZE),
        DmaBuffer::alloc(BIND_DATA),
    ) else {
        crate::kprintln!("usb: port {port}: no memory for its storage endpoints; not bound");
        return None;
    };
    let ring = |b: &DmaBuffer| {
        // SAFETY: `b` goes into the device's memory, freed only once its slot is disabled and it is
        // out of the table, after which nothing uses its rings.
        let (mut mem, phys) = (unsafe { RingMem::kept(b) }, b.phys().as_u64());
        Ring { producer: Producer::new(&mut mem, phys), mem, phys }
    };
    let p = Prepared {
        interface: s.number,
        in_ep: s.bulk_in,
        out_ep: s.bulk_out,
        bulk_in: ring(&rin),
        bulk_out: ring(&rout),
        wrap_virt: wrap.virt() as u64,
        wrap_phys: wrap.phys().as_u64(),
        data_virt: data.virt() as u64,
        data_phys: data.phys().as_u64(),
    };
    if mem.class.try_reserve(4).is_err() {
        crate::kprintln!("usb: port {port}: no memory for its storage endpoints; not bound");
        return None;
    }
    for b in [rin, rout, wrap, data] {
        let _ = mem.class.push_within_capacity(Programmed::new(b));
    }
    Some(p)
}

/// **Bind the bulk-only interface `p` of the device in `slot`**, its configuration set: put it in
/// the table, then for each logical unit that is a ready direct-access device, read its partition
/// table and publish it — a `Disk` under the `UsbDevice` record `parent`, named by its INQUIRY
/// strings and `serial`, and its partitions under it.
pub(in crate::drivers::xhci) fn bind(
    x: &Xhci,
    port: u8,
    slot: u8,
    mem: &mut DeviceMem,
    p: Prepared,
    parent: Option<u32>,
    serial: &[u8],
) {
    let dev = Dev {
        slot,
        interface: p.interface,
        in_dci: p.in_ep.dci(),
        out_dci: p.out_ep.dci(),
        in_address: p.in_ep.address,
        out_address: p.out_ep.address,
        in_mps: p.in_ep.max_packet,
        out_mps: p.out_ep.max_packet,
        bulk_in: p.bulk_in,
        bulk_out: p.bulk_out,
        wrap_virt: p.wrap_virt,
        wrap_phys: p.wrap_phys,
        data_virt: p.data_virt,
        data_phys: p.data_phys,
        db: x.db,
        tag: 0,
        luns: [Lun::default(); MAX_LUNS],
        inflight: None,
        fault: None,
        hub_result: None,
        // **Held while it binds**, as a rescan holds it: a unit published before the next is bound
        // takes submits, and they queue rather than start under the binding's commands. Held
        // across every unit, `TODO(usb-bind-hold-per-unit)`.
        recovering: true,
        stranded: None,
        rescan: None,
        queue: Queue::new(),
        counted: [const { Counted::new() }; MAX_LUNS],
    };
    let Some((i, epoch)) = DISKS.take(dev) else {
        crate::kprintln!("usb: port {port}: {MAX_DEVICES} storage devices are bound already; not bound");
        return;
    };
    let mut b = Binding { x, port, i, epoch, mem, parent, serial };
    let max_lun = match hub::control_in_byte(x, slot, b.mem, [0xA1, 0xFE, 0, 0, p.interface, 0, 1, 0]) {
        Ok(n) => n.min(MAX_LUNS as u8 - 1),
        // A stall means one unit (bulk-only §3.2); its endpoint is recovered.
        Err(Failed::Code(code::STALL)) => {
            let _ = hub::recover_ep0(x, slot, b.mem);
            0
        }
        Err(e) => {
            crate::kprintln!("usb: port {port}: GET MAX LUN failed: {e}; not bound");
            retire(i);
            return;
        }
    };
    let mut published = 0;
    for lun in 0..=max_lun {
        match b.unit(lun) {
            Ok(Some(u)) => {
                if b.publish(lun, &u) {
                    published += 1;
                }
            }
            Ok(None) => {}
            Err(why) => {
                crate::kprintln!("usb: port {port}: LUN {lun}: {why}; not bound, nor any unit after it");
                break;
            }
        }
    }
    if published == 0 {
        retire(i);
    } else {
        act(DISKS.let_go(i, crate::arch::Timer::read_ns()), Some(x));
    }
}

struct Unit {
    inquiry: scsi::Inquiry,
    blocks: u64,
    block_len: u32,
}

struct Binding<'a> {
    x: &'a Xhci,
    port: u8,
    i: usize,
    epoch: u32,
    mem: &'a mut DeviceMem,
    parent: Option<u32>,
    serial: &'a [u8],
}

impl Binding<'_> {
    /// **What unit `lun` is**: a ready direct-access device's INQUIRY, block count and size, or
    /// `None` for one left alone — another kind, or no medium — which is said. `Err` when the
    /// device stopped answering, which ends the binding. **A unit of 2³² blocks or more**, which
    /// `READ CAPACITY(10)` cannot describe, is asked again with `READ CAPACITY(16)`.
    fn unit(&mut self, lun: u8) -> Result<Option<Unit>, &'static str> {
        let port = self.port;
        let mut buf = [0u8; scsi::INQUIRY_LEN];
        let inquiry = match self.run(lun, &scsi::inquiry(), &mut buf) {
            Ok(Status::Passed { .. }) => scsi::inquiry_answer(&buf).ok_or("INQUIRY's answer is short")?,
            Ok(_) => return Err("INQUIRY failed"),
            Err(_) => return Err("INQUIRY went unanswered"),
        };
        if !inquiry.direct_access {
            crate::kprintln!("usb: port {port}: LUN {lun}: not a direct-access block device; left alone");
            return Ok(None);
        }
        // **Ready**: a stick still starting up, or reporting the unit attention its reset raised, is
        // asked again; one with no medium is left alone.
        let start = crate::arch::Timer::read_ns();
        loop {
            match self.run(lun, &scsi::test_unit_ready(), &mut []) {
                Ok(Status::Passed { .. }) => break,
                Ok(Status::Failed) => {
                    let (key, asc, _) = self.sense(lun)?;
                    if key == scsi::NOT_READY && asc == scsi::MEDIUM_NOT_PRESENT {
                        crate::kprintln!("usb: port {port}: LUN {lun}: no medium; left alone");
                        return Ok(None);
                    }
                }
                Ok(_) | Err(_) => return Err("TEST UNIT READY went wrong"),
            }
            if crate::arch::Timer::read_ns().wrapping_sub(start) > READY_NS {
                crate::kprintln!("usb: port {port}: LUN {lun}: not ready after five seconds; left alone");
                return Ok(None);
            }
            hub::sleep(100_000_000);
        }
        let mut cap = [0u8; scsi::CAPACITY_LEN];
        let (blocks, block_len) = match self.run(lun, &scsi::read_capacity(), &mut cap) {
            Ok(Status::Passed { .. }) => match scsi::capacity_answer(&cap) {
                Some(c) => c,
                None => {
                    let mut cap16 = [0u8; scsi::CAPACITY16_LEN];
                    match self.run(lun, &scsi::read_capacity16(), &mut cap16) {
                        Ok(Status::Passed { .. }) if let Some(c) = scsi::capacity16_answer(&cap16) => {
                            crate::kprintln!(
                                "usb: port {port}: LUN {lun}: 2 TiB or more: {} blocks, \
                                 read with the sixteen-byte commands",
                                c.0
                            );
                            c
                        }
                        _ => {
                            crate::kprintln!(
                                "usb: port {port}: LUN {lun}: 2 TiB or more, and READ CAPACITY(16) went \
                                 unanswered; left alone"
                            );
                            return Ok(None);
                        }
                    }
                }
            },
            _ => return Err("READ CAPACITY went wrong"),
        };
        Ok(Some(Unit { inquiry, blocks, block_len }))
    }

    fn sense(&mut self, lun: u8) -> Result<(u8, u8, u8), &'static str> {
        let mut buf = [0u8; scsi::SENSE_LEN];
        match self.run(lun, &scsi::request_sense(), &mut buf) {
            Ok(Status::Passed { .. }) => Ok(scsi::sense_answer(&buf)),
            _ => Err("REQUEST SENSE went wrong"),
        }
    }

    fn run(&mut self, lun: u8, cdb: &[u8], out: &mut [u8]) -> Result<Status, Failed> {
        run(self.x, self.i, lun, cdb, out, self.mem)
    }

    /// **Publish unit `lun`**: its table read through the binding's commands, then its node under
    /// the device's record and its partitions under the node. Whether it was published.
    fn publish(&mut self, lun: u8, u: &Unit) -> bool {
        let port = self.port;
        let Some(parent) = self.parent else {
            crate::kprintln!("usb: port {port}: its device has no record; its disk is not published");
            return false;
        };
        let mut name = [0u8; MAX_DEVICE_NAME];
        let name_len = disk_name(&u.inquiry, self.serial, &mut name);
        let unit = Lun { blocks: u.blocks, block_len: u.block_len, ..Lun::default() };
        {
            let mut d = DISKS.devs[self.i].lock();
            if let Some(dev) = d.as_mut() {
                dev.luns[lun as usize] = unit;
            }
        }
        // **The table first**, before the disk can be reached: nothing else waits on it yet.
        let table = read_table(self.x, self.i, port, lun, unit, self.mem);
        let backend = BlockBackend { submit, poll, ctx: context(self.i, lun, self.epoch), max_frags: MAX_FRAGS };
        let geometry = BlockGeometry { logical_block_size: u.block_len, block_count: u.blocks };
        let Ok(node) =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, &name[..name_len], backend)
        else {
            crate::kprintln!("usb: port {port}: no memory for its disk's node");
            return false;
        };
        let node = crate::drivers::adopt_node(node);
        let Some(id) = crate::device::register_block_under(node.clone(), parent, "usb-storage") else {
            return false;
        };
        // Kept with the unit, for a rescan to depart the disk's partitions by, and its counters.
        if let Some(dev) = DISKS.devs[self.i].lock().as_mut() {
            dev.luns[lun as usize].record = Some(id);
            dev.counted[lun as usize].attach(&node);
        }
        crate::kprintln!(
            "usb: port {port}: LUN {lun}: disk \"{}\", {} blocks of {} bytes, record {id}",
            Printable(&name[..name_len]),
            u.blocks,
            u.block_len
        );
        if let Some(table) = table {
            crate::drivers::gpt::publish(&node, table, crate::drivers::gpt::Names::None);
        }
        true
    }
}

/// **Read unit `lun`'s partition table** through the hub thread's commands, as at its disk's
/// arrival and again for a rescan: `None` for a unit of other than 512-byte blocks, which is said,
/// since the table reader takes those alone.
pub(super) fn read_table(
    x: &Xhci,
    i: usize,
    port: u8,
    lun: u8,
    unit: Lun,
    mem: &mut DeviceMem,
) -> Option<Result<crate::drivers::partitions::Table, crate::drivers::partitions::Unread>> {
    use crate::drivers::partitions::{self, BLOCK};
    if unit.block_len as usize != BLOCK {
        crate::kprintln!("usb: port {port}: LUN {lun}: {}-byte blocks; its table is not read", unit.block_len);
        return None;
    }
    Some(partitions::read(unit.blocks, &mut |lba, count, out: &mut [u8]| {
        let len = count as usize * BLOCK;
        if len > BIND_DATA || out.len() < len || count > u16::MAX as u64 {
            return false;
        }
        let cdb = rw_cdb(unit.blocks, false, lba, count as u16);
        matches!(run(x, i, lun, cdb.as_slice(), &mut out[..len], mem), Ok(Status::Passed { residue: 0 }))
    }))
}

/// **A USB disk's name**: its INQUIRY vendor and product, their padding trimmed, then the device's
/// serial in brackets, as a SATA disk's is its model and serial. The length written into `out`.
pub(super) fn disk_name(inquiry: &scsi::Inquiry, serial: &[u8], out: &mut [u8]) -> usize {
    let trim = |b: &[u8]| -> usize { b.iter().rposition(|&c| c != b' ' && c != 0).map_or(0, |p| p + 1) };
    let mut w = NameBuf::new(out);
    let vendor = &inquiry.vendor[..trim(&inquiry.vendor)];
    let product = &inquiry.product[..trim(&inquiry.product)];
    let _ = core::fmt::Write::write_fmt(&mut w, format_args!("{} {}", Printable(vendor), Printable(product)));
    if !serial.is_empty() {
        let _ = core::fmt::Write::write_fmt(&mut w, format_args!(" ({})", Printable(serial)));
    }
    w.len()
}
