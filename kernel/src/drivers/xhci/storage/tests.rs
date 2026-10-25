use super::*;
use crate::io::irp::IrpBuffer;
use crate::mm::test_support::init_global_heap;

#[test]
fn a_command_wrapper_is_laid_out_as_bulk_only_says() {
    let w = cbw(0x1234_5678, 4096, true, 3, &scsi::rw10(false, 0x0102_0304, 8));
    assert_eq!(&w[0..4], b"USBC");
    assert_eq!(le32(&w, 4), 0x1234_5678);
    assert_eq!(le32(&w, 8), 4096);
    assert_eq!((w[12], w[13], w[14]), (0x80, 3, 10));
    assert_eq!(&w[15..25], &[0x28, 0, 1, 2, 3, 4, 0, 0, 8, 0]);
    assert!(w[25..].iter().all(|&b| b == 0));
    assert_eq!(cbw(1, 0, false, 0, &scsi::test_unit_ready())[12], 0, "OUT, or no data");
}

fn status(sig: &[u8; 4], tag: u32, residue: u32, status: u8) -> [u8; CSW_LEN] {
    let mut b = [0u8; CSW_LEN];
    b[0..4].copy_from_slice(sig);
    b[4..8].copy_from_slice(&tag.to_le_bytes());
    b[8..12].copy_from_slice(&residue.to_le_bytes());
    b[12] = status;
    b
}

#[test]
fn a_status_wrapper_is_this_commands_or_invalid() {
    assert_eq!(csw(&status(b"USBS", 7, 0, 0), 7), Status::Passed { residue: 0 });
    assert_eq!(csw(&status(b"USBS", 7, 512, 0), 7), Status::Passed { residue: 512 });
    assert_eq!(csw(&status(b"USBS", 7, 0, 1), 7), Status::Failed);
    assert_eq!(csw(&status(b"USBS", 7, 0, 2), 7), Status::Phase);
    assert_eq!(csw(&status(b"USBS", 7, 0, 3), 7), Status::Invalid, "no status 3");
    assert_eq!(csw(&status(b"USBC", 7, 0, 0), 7), Status::Invalid, "a command's signature");
    assert_eq!(csw(&status(b"USBS", 8, 0, 0), 7), Status::Invalid, "another command's tag");
    assert_eq!(csw(&status(b"USBS", 7, 0, 0)[..12], 7), Status::Invalid, "short");
}

#[test]
fn each_cdb_is_as_scsi_gives_it() {
    assert_eq!(scsi::test_unit_ready(), [0; 6]);
    assert_eq!(scsi::request_sense(), [0x03, 0, 0, 0, 18, 0]);
    assert_eq!(scsi::inquiry(), [0x12, 0, 0, 0, 36, 0]);
    assert_eq!(scsi::read_capacity()[0], 0x25);
    assert_eq!(scsi::rw10(true, 0xAABB_CCDD, 0x0102), [0x2A, 0, 0xAA, 0xBB, 0xCC, 0xDD, 0, 0x01, 0x02, 0]);
    assert_eq!(scsi::synchronize_cache(), [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(scsi::eject(), [0x1B, 0, 0, 0, 0x02, 0], "LoEj is bit 1 of byte 4, Start bit 0");
}

#[test]
fn a_failed_commands_sense_comes_to_its_status() {
    let (flush, eject, read) = (IrpOp::Flush as u32, IrpOp::Eject as u32, IrpOp::Read as u32);
    assert_eq!(sense_outcome(scsi::ILLEGAL_REQUEST, flush, false), Some(0), "no cache to flush");
    let unsupported = Some(KError::Unsupported as i32);
    assert_eq!(sense_outcome(scsi::ILLEGAL_REQUEST, eject, false), unsupported, "a stick that cannot eject");
    assert_eq!(sense_outcome(scsi::ILLEGAL_REQUEST, read, false), Some(KError::IoError as i32));
    assert_eq!(sense_outcome(scsi::UNIT_ATTENTION, eject, false), None, "sent again once");
    assert_eq!(sense_outcome(scsi::UNIT_ATTENTION, eject, true), Some(KError::IoError as i32));
}

#[test]
fn the_answers_read_as_scsi_gives_them() {
    let mut inq = [0u8; 36];
    inq[8..16].copy_from_slice(b"QEMU    ");
    inq[16..32].copy_from_slice(b"QEMU HARDDISK   ");
    let a = scsi::inquiry_answer(&inq).unwrap();
    assert!(a.direct_access);
    assert_eq!(&a.vendor, b"QEMU    ");
    inq[0] = 0x05;
    assert!(!scsi::inquiry_answer(&inq).unwrap().direct_access, "a CD-ROM");
    inq[0] = 0x20;
    assert!(!scsi::inquiry_answer(&inq).unwrap().direct_access, "qualifier 1: not connected");
    assert_eq!(scsi::inquiry_answer(&inq[..35]), None);

    assert_eq!(scsi::capacity_answer(&[0, 0, 0x7F, 0xFF, 0, 0, 2, 0]), Some((0x8000, 512)));
    assert_eq!(scsi::capacity_answer(&[0xFF, 0xFF, 0xFF, 0xFE, 0, 0, 2, 0]), Some((0xFFFF_FFFF, 512)));
    assert_eq!(scsi::capacity_answer(&[0xFF, 0xFF, 0xFF, 0xFF, 0, 0, 2, 0]), None, "2 TiB or more");

    let mut sense = [0u8; 18];
    sense[2] = 0x06;
    sense[12] = 0x28;
    assert_eq!(scsi::sense_answer(&sense), (scsi::UNIT_ATTENTION, 0x28, 0));
}

#[test]
fn an_irp_fits_its_unit_or_is_refused() {
    let unit = Lun { blocks: 1024, block_len: 512, ..Lun::default() };
    let r = IrpOp::Read as u32;
    let (cdb, len, input) = irp_command(r, 4096, 8192, unit).unwrap();
    assert_eq!((cdb, len, input), (Cdb::of(&scsi::rw10(false, 8, 16)), 8192, true));
    assert_eq!(irp_command(IrpOp::Rescan as u32, 0, 0, unit), Err(KError::InvalidArgument), "not a command");
    assert!(!irp_command(IrpOp::Write as u32, 0, 512, unit).unwrap().2, "a write moves out");
    assert_eq!(irp_command(r, 100, 512, unit), Err(KError::InvalidArgument), "unaligned");
    assert_eq!(irp_command(r, 0, 0, unit), Err(KError::InvalidArgument), "empty");
    assert!(irp_command(r, 1023 * 512, 512, unit).is_ok(), "the last block");
    assert_eq!(irp_command(r, 1023 * 512, 1024, unit), Err(KError::InvalidArgument), "past the end");
    assert_eq!(irp_command(IrpOp::Flush as u32, 0, 0, unit), Ok((Cdb::of(&scsi::synchronize_cache()), 0, false)));
    assert_eq!(irp_command(IrpOp::Flush as u32, 0, 0, Lun::default()), Err(KError::InvalidArgument));
    assert_eq!(irp_command(IrpOp::Eject as u32, 0, 0, unit), Ok((Cdb::of(&scsi::eject()), 0, false)));
}

#[test]
fn the_sixteen_byte_commands_are_as_scsi_gives_them() {
    assert_eq!(scsi::read_capacity16(), [0x9E, 0x10, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 0, 0]);
    assert_eq!(
        scsi::rw16(true, 0x0102_0304_0506_0708, 0x0A0B_0C0D),
        [0x8A, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0x0A, 0x0B, 0x0C, 0x0D, 0, 0]
    );
    assert_eq!(scsi::rw16(false, 0, 1)[0], 0x88);
    let blocks: u64 = 3 << 31; // 3 TiB of 512-byte blocks
    let mut answer = [0u8; scsi::CAPACITY16_LEN];
    answer[..8].copy_from_slice(&(blocks - 1).to_be_bytes());
    answer[8..12].copy_from_slice(&512u32.to_be_bytes());
    assert_eq!(scsi::capacity16_answer(&answer), Some((blocks, 512)));
    assert_eq!(scsi::capacity16_answer(&answer[..11]), None, "too short");
    let mut ten = [0u8; scsi::CAPACITY_LEN];
    ten[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    ten[4..].copy_from_slice(&512u32.to_be_bytes());
    assert_eq!(scsi::capacity_answer(&ten), None, "what sends a unit to READ CAPACITY(16)");
}

/// At exactly 2³² blocks the last block still fits in 32 bits, so it is a `READ(10)`.
#[test]
fn a_unit_past_two_to_the_thirty_two_blocks_is_read_with_the_sixteen_byte_commands() {
    let r = IrpOp::Read as u32;
    let at = |blocks: u64| Lun { blocks, block_len: 512, ..Lun::default() };
    let last = (1u64 << 32) - 1;
    assert_eq!(irp_command(r, last * 512, 512, at(1 << 32)).unwrap().0, Cdb::of(&scsi::rw10(false, u32::MAX, 1)));
    assert_eq!(irp_command(r, 0, 512, at((1 << 32) + 1)).unwrap().0, Cdb::of(&scsi::rw16(false, 0, 1)));
    let past = 1u64 << 32;
    assert_eq!(irp_command(r, past * 512, 512, at((1 << 32) + 1)).unwrap().0, Cdb::of(&scsi::rw16(false, past, 1)));
    let beyond = irp_command(r, (past + 1) * 512, 512, at((1 << 32) + 1));
    assert_eq!(beyond, Err(KError::InvalidArgument), "past its end");
    assert_eq!(rw_cdb(3 << 31, true, 5, 8).as_slice(), &scsi::rw16(true, 5, 8));
}

#[test]
fn a_td_size_counts_the_packets_left() {
    assert_eq!(packets(0, 512), 0);
    assert_eq!(packets(4096, 512), 8);
    assert_eq!(packets(4097, 512), 9);
    assert_eq!(packets(1 << 20, 512), 31);
}

#[test]
fn a_usb_disk_is_named_by_its_model_and_serial() {
    let inq = scsi::Inquiry { direct_access: true, vendor: *b"QEMU    ", product: *b"QEMU HARDDISK   " };
    let mut out = [0u8; MAX_DEVICE_NAME];
    let n = disk_name(&inq, b"1-0000:00:04.0-1", &mut out);
    assert_eq!(&out[..n], b"QEMU QEMU HARDDISK (1-0000:00:04.0-1)");
    let n = disk_name(&inq, b"", &mut out);
    assert_eq!(&out[..n], b"QEMU QEMU HARDDISK");
}

/// Heap memory standing in for a device's rings, wrapper page, binding buffer and the doorbell
/// array: its "physical" addresses are its virtual ones.
struct Fake {
    rings: [crate::libkern::KVec<u64>; 2],
    wrap: crate::libkern::KVec<u64>,
    data: crate::libkern::KVec<u64>,
    db: crate::libkern::KVec<u32>,
}

fn zeroed<T: Copy + Default>(n: usize) -> crate::libkern::KVec<T> {
    let mut v = crate::libkern::KVec::new();
    v.try_reserve(n).unwrap();
    while v.len() < n {
        v.try_push(T::default()).unwrap();
    }
    v
}

impl Fake {
    fn new() -> Fake {
        Fake {
            rings: [zeroed(RING_TRBS * 2), zeroed(RING_TRBS * 2)],
            wrap: zeroed(512),
            data: zeroed(BIND_DATA / 8),
            db: zeroed(32),
        }
    }

    fn dev(&mut self, slot: u8) -> Dev {
        let ring = |v: &mut crate::libkern::KVec<u64>| {
            let at = v.as_mut_ptr() as u64;
            Ring { producer: Producer::new(&mut RawSlots(at), at), virt: at, phys: at }
        };
        let (r0, r1) = self.rings.split_at_mut(1);
        let wrap = self.wrap.as_mut_ptr() as u64;
        let data = self.data.as_mut_ptr() as u64;
        Dev {
            slot,
            interface: 0,
            in_dci: 3,
            out_dci: 4,
            in_address: 0x81,
            out_address: 0x02,
            in_mps: 512,
            out_mps: 512,
            bulk_in: ring(&mut r0[0]),
            bulk_out: ring(&mut r1[0]),
            wrap_virt: wrap,
            wrap_phys: wrap,
            data_virt: data,
            data_phys: data,
            db: self.db.as_mut_ptr() as u64,
            tag: 0,
            luns: {
                let mut l = [Lun::default(); MAX_LUNS];
                l[0] = Lun { blocks: 2048, block_len: 512, record: Some(7), ..Lun::default() };
                l
            },
            inflight: None,
            fault: None,
            hub_result: None,
            recovering: false,
            stranded: None,
            rescan: None,
            queue: Queue::new(),
            counted: [const { Counted::new() }; MAX_LUNS],
        }
    }

    fn answer(&mut self, tag: u32, status_byte: u8) {
        self.answer_short(tag, 0, status_byte);
    }

    fn answer_short(&mut self, tag: u32, residue: u32, status_byte: u8) {
        let b = status(b"USBS", tag, residue, status_byte);
        let w = self.wrap.as_mut_ptr() as *mut u8;
        for (k, v) in b.iter().enumerate() {
            // SAFETY: the fake wrapper page is 4 KiB.
            unsafe { w.add(CSW_AT as usize + k).write(*v) };
        }
    }

    fn trb(&self, r: usize, i: usize) -> Trb {
        RawSlots(self.rings[r].as_ptr() as u64).read(i)
    }
}

fn irp(op: IrpOp, offset: u64, length: u64, frags: &[PhysFrag]) -> Irp {
    let buffer = if frags.is_empty() {
        IrpBuffer::NONE
    } else {
        IrpBuffer { kind: crate::io::irp::IRP_BUF_FRAGS, count: frags.len() as u32, frags: frags.as_ptr() as u64 }
    };
    Irp::new_block(op, core::ptr::null(), offset, length, buffer, core::ptr::null_mut(), 0)
}

fn csw_trb(d: &Disks, i: usize) -> u64 {
    match d.devs[i].lock().as_ref().unwrap().inflight.unwrap().stage {
        Stage::Status { csw_trb, .. } => csw_trb,
        s => panic!("the status is not asked for yet: {s:?}"),
    }
}

/// **A read's status is asked for when its data stage ends, and it completes at its status
/// wrapper**; the IRP queued meanwhile starts when the first completes.
#[test]
fn a_reads_status_is_asked_for_when_its_data_ends_and_the_next_starts_at_its_status() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(9)).unwrap();
    let ctx = context(i, 0, epoch);
    let frags = [PhysFrag { base: 0x10_000, len: 4096 }, PhysFrag { base: 0x20_000, len: 4096 }];
    let mut first = irp(IrpOp::Read, 8 * 512, 8192, &frags);
    assert_eq!(disks.submit(&mut first, ctx, 0), Action::None);
    let cbw_trb = fake.trb(1, 0);
    assert_eq!(cbw_trb.0[2] & 0x1_FFFF, CBW_LEN as u32, "the command wrapper on OUT");
    assert_eq!((fake.trb(0, 0).0[0], fake.trb(0, 1).0[0]), (0x10_000, 0x20_000), "the data on IN");
    assert_ne!(fake.trb(0, 0).0[3] & (1 << 4), 0, "chained to the next fragment");
    assert_eq!(fake.trb(0, 1).0[3] & (1 << 5), 1 << 5, "the last interrupts");
    assert_eq!(fake.trb(0, 2), Trb::default(), "and nothing behind it yet");
    assert_eq!(fake.db[9], 3, "the last doorbell rung was IN's");
    let w = fake.wrap.as_ptr() as *const u8;
    // SAFETY: the fake wrapper page.
    let wrapper: [u8; CBW_LEN] = core::array::from_fn(|k| unsafe { w.add(k).read() });
    assert_eq!(&wrapper[15..25], &scsi::rw10(false, 8, 16));

    let mut second = irp(IrpOp::Write, 0, 512, &frags[..1]);
    assert_eq!(disks.submit(&mut second, ctx, 0), Action::None, "queued");
    assert_eq!(disks.on_transfer(9, 4, code::SUCCESS, 0x99, 1), Some(Action::None), "OUT's event is not the data's");
    assert_eq!(fake.trb(0, 2), Trb::default(), "so no status yet");
    assert_eq!(disks.on_transfer(9, 3, code::SUCCESS, 0x10_000, 1), Some(Action::None), "the data stage ends");
    assert_eq!(fake.trb(0, 2).0[2] & 0x1_FFFF, CSW_LEN as u32, "and the status is asked for");
    fake.answer(1, 0);
    let at = csw_trb(&disks, i);
    assert_eq!(
        disks.on_transfer(9, 3, code::SUCCESS, at, 1),
        Some(Action::Complete { irp: &mut first, status: 0, transferred: 8192 })
    );
    assert_eq!(fake.trb(1, 1).0[2] & 0x1_FFFF, CBW_LEN as u32, "the next command's wrapper");
    assert_eq!(fake.trb(1, 2).0[0], 0x10_000, "and its data, on OUT for a write");
    assert_eq!(disks.on_transfer(8, 3, code::SUCCESS, at, 1), None, "another slot's event is not this driver's");
}

/// A queued write is timed from when it starts, not from its submit; a recovered error counts
/// with none of the recovery's time.
#[test]
fn each_command_is_counted_on_its_disk_with_its_time_on_the_device() {
    static FIRST: crate::io::stats::IoStats = crate::io::stats::IoStats::new();
    static OTHER: crate::io::stats::IoStats = crate::io::stats::IoStats::new();
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(9)).unwrap();
    disks.devs[i].lock().as_ref().unwrap().counted[0].attach_static(&FIRST);
    let ctx = context(i, 0, epoch);
    let frags = [PhysFrag { base: 0x10_000, len: 4096 }, PhysFrag { base: 0x20_000, len: 4096 }];
    let mut read = irp(IrpOp::Read, 0, 8192, &frags);
    disks.submit(&mut read, ctx, 100);
    let mut write = irp(IrpOp::Write, 0, 512, &frags[..1]);
    disks.submit(&mut write, ctx, 150);
    disks.on_transfer(9, 3, code::SUCCESS, 0x20_000, 300);
    fake.answer(1, 0);
    let done = disks.on_transfer(9, 3, code::SUCCESS, csw_trb(&disks, i), 400);
    assert!(matches!(done, Some(Action::Complete { status: 0, .. })));
    let r = FIRST.record_for(0, 0);
    assert_eq!((r.reads, r.read_bytes, r.busy_ns), (1, 8192, 300), "issued at 100, its status at 400");
    // The write started at 400, as the read completed: its 600 are from there, not from 150.
    disks.on_transfer(9, 4, code::SUCCESS, 0x10_000, 700);
    fake.answer(2, 0);
    disks.on_transfer(9, 3, code::SUCCESS, csw_trb(&disks, i), 1000);
    let r = FIRST.record_for(0, 0);
    assert_eq!((r.writes, r.written_bytes, r.busy_ns), (1, 512, 300 + 600));

    let mut flush = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut flush, ctx, 2000);
    disks.on_transfer(9, 4, code::SUCCESS, 0, 2100);
    fake.answer(3, 1);
    assert_eq!(disks.on_transfer(9, 3, code::SUCCESS, csw_trb(&disks, i), 2200), Some(Action::Wake));
    assert_eq!(FIRST.record_for(0, 0).errors, 0, "not yet: the hub thread has it");
    let (_, cmd, _) = disks.begin_recovery(i).unwrap();
    disks.end_recovery(i, cmd, End::Complete(KError::IoError as i32), 9000);
    let r = FIRST.record_for(0, 0);
    assert_eq!((r.flushes, r.errors, r.busy_ns), (0, 1, 900), "an error, and no recovery time");

    let (j, e2) = disks.take(fake.dev(3)).unwrap();
    disks.devs[j].lock().as_ref().unwrap().counted[0].attach_static(&OTHER);
    let mut other = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut other, context(j, 0, e2), 10_000);
    disks.on_transfer(3, 4, code::SUCCESS, 0, 10_050);
    fake.answer(1, 0);
    disks.on_transfer(3, 3, code::SUCCESS, csw_trb(&disks, j), 10_100);
    assert_eq!((OTHER.record_for(0, 0).flushes, OTHER.record_for(0, 0).busy_ns), (1, 100));
    assert_eq!(FIRST.record_for(0, 0).flushes, 0);
}

/// **A rescan is held for the hub thread**: it wakes the thread rather than going on the rings,
/// and **while it waits the device runs as ever**, so what the disk's windows forwarded before
/// they were retired reaches the disk at the old table's offsets, never after the new table.
#[test]
fn a_rescan_takes_the_device_only_once_its_queue_has_drained() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let frags = [PhysFrag { base: 0x10_000, len: 512 }];
    let mut first = irp(IrpOp::Read, 0, 512, &frags);
    assert_eq!(disks.submit(&mut first, ctx, 0), Action::None, "started");
    let mut later = irp(IrpOp::Write, 512, 512, &frags);
    assert_eq!(disks.submit(&mut later, ctx, 0), Action::None, "queued behind the read");
    let mut rescan = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(disks.submit(&mut rescan, ctx, 0), Action::Wake, "held, and the hub thread woken");
    assert_eq!(disks.due(1)[i], Some(5), "the hub thread's to look at");
    let mut again = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(
        disks.submit(&mut again, ctx, 0),
        Action::Complete { irp: &mut again, status: KError::WouldBlock as i32, transferred: 0 }
    );
    assert_eq!(disks.begin_rescan(i), BeginRescan::Busy, "a command in flight, and one queued");
    disks.on_transfer(5, 3, code::SUCCESS, 0x10_000, 1);
    fake.answer(1, 0);
    let at = csw_trb(&disks, i);
    let ended = disks.on_transfer(5, 3, code::SUCCESS, at, 1);
    assert_eq!(ended, Some(Action::Complete { irp: &mut first, status: 0, transferred: 512 }));
    {
        let d = disks.devs[i].lock();
        let dev = d.as_ref().unwrap();
        assert!(dev.inflight.is_some() && dev.queue.len == 0, "the queued write started while the rescan waits");
    }
    assert_eq!(disks.begin_rescan(i), BeginRescan::Busy, "the write is in flight");
    disks.on_transfer(5, 4, code::SUCCESS, 0x10_000, 2);
    fake.answer(2, 0);
    let at = csw_trb(&disks, i);
    let ended = disks.on_transfer(5, 3, code::SUCCESS, at, 2);
    assert_eq!(ended, Some(Action::Complete { irp: &mut later, status: 0, transferred: 512 }));
    // **A command queued keeps it waiting by itself**, with nothing in flight: no path here leaves
    // one so, and a rescan taking the device over it would issue it after the new table.
    let mut stuck = irp(IrpOp::Read, 2048, 512, &frags);
    {
        let mut d = disks.devs[i].lock();
        let dev = d.as_mut().unwrap();
        let c = dev.command_for(&mut stuck, 0).unwrap();
        assert!(dev.queue.push(c));
    }
    assert_eq!(disks.begin_rescan(i), BeginRescan::Busy, "a command queued, nothing in flight");
    assert!(disks.devs[i].lock().as_mut().unwrap().queue.pop().is_some());
    let unit = Lun { blocks: 2048, block_len: 512, record: Some(7), ..Lun::default() };
    assert_eq!(disks.rescan_unit(i), Some(unit), "the unit the hub thread retires the windows of");
    assert_eq!(disks.begin_rescan(i), BeginRescan::Ready { irp: &mut rescan, lun: 0, unit });
    assert!(disks.devs[i].lock().as_ref().unwrap().recovering, "the device is held");
    assert_eq!(disks.begin_rescan(i), BeginRescan::Nothing, "taken");
    let mut held = irp(IrpOp::Read, 1024, 512, &frags);
    assert_eq!(disks.submit(&mut held, ctx, 3), Action::None);
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none(), "queued while the rescan runs");
    assert_eq!(disks.end_rescan(i, 4), Action::None);
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_some(), "and started at its end");
    let mut stray = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(
        disks.submit(&mut stray, context(i, 1, epoch), 0),
        Action::Complete { irp: &mut stray, status: KError::Unsupported as i32, transferred: 0 }
    );
    let mut waiting = irp(IrpOp::Rescan, 0, 0, &[]);
    disks.submit(&mut waiting, ctx, 0);
    let (_, held) = disks.depart(i);
    assert_eq!(held[2], Some(&mut waiting as *mut Irp));
}

/// A rescan waiting on a device a failed recovery stranded ends the device.
#[test]
fn a_rescan_on_a_stranded_device_ends_it() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let frags = [PhysFrag { base: 0x10_000, len: 512 }];
    let mut read = irp(IrpOp::Read, 0, 512, &frags);
    disks.submit(&mut read, ctx, 0);
    let mut rescan = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(disks.submit(&mut rescan, ctx, 0), Action::Wake);
    // The command wrapper stalls, and reset recovery fails.
    assert_eq!(disks.on_transfer(5, 4, code::STALL, 0, 1), Some(Action::Wake));
    let (_, cmd, _) = disks.begin_recovery(i).unwrap();
    assert_eq!(disks.end_recovery(i, cmd, End::Stranded, 2), Action::None);
    assert_eq!(disks.begin_rescan(i), BeginRescan::Ended, "to be ended, not waited for");
    let (_, held) = disks.depart(i);
    assert_eq!(held[1], Some(&mut read as *mut Irp), "the departure completes the stranded read");
    assert_eq!(held[2], Some(&mut rescan as *mut Irp), "and the rescan");
}

/// **An eject goes as `START STOP UNIT`, and a unit that stopped is gone**; another runs on.
#[test]
fn a_unit_that_stopped_refuses_everything_after_and_says_so_once() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let mut dev = fake.dev(5);
    dev.luns[1] = Lun { blocks: 2048, block_len: 512, record: Some(8), ..Lun::default() };
    let (i, epoch) = disks.take(dev).unwrap();
    let (ctx, other) = (context(i, 0, epoch), context(i, 1, epoch));
    let mut eject = irp(IrpOp::Eject, 0, 0, &[]);
    assert_eq!(disks.submit(&mut eject, ctx, 0), Action::None);
    let w = fake.wrap.as_ptr() as *const u8;
    // SAFETY: the fake wrapper page.
    let wrapper: [u8; CBW_LEN] = core::array::from_fn(|k| unsafe { w.add(k).read() });
    assert_eq!(&wrapper[15..21], &scsi::eject());
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    fake.answer(1, 0);
    let done = disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), 2);
    assert_eq!(done, Some(Action::Stopped { irp: &mut eject }));

    let mut read = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
    assert_eq!(disks.submit(&mut read, ctx, 3), Action::Ejected { irp: &mut read, said: Some((5, 0)) });
    let mut rescan = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(disks.submit(&mut rescan, ctx, 3), Action::Ejected { irp: &mut rescan, said: None }, "said once");
    let mut flush = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut flush, other, 3), Action::None, "the other unit runs on");
}

/// **What waited behind an eject on its unit is never started**, another unit's read starting in
/// its place, and the hub thread answers it: a flush done, the rest refused, as after.
#[test]
fn what_waited_behind_an_eject_is_never_started_and_answered_by_the_hub_thread() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let mut dev = fake.dev(5);
    dev.luns[1] = Lun { blocks: 2048, block_len: 512, record: Some(8), ..Lun::default() };
    let (i, epoch) = disks.take(dev).unwrap();
    let (ctx, other) = (context(i, 0, epoch), context(i, 1, epoch));
    let mut rescan = irp(IrpOp::Rescan, 0, 0, &[]);
    assert_eq!(disks.submit(&mut rescan, ctx, 0), Action::Wake, "held for the hub thread");
    let mut eject = irp(IrpOp::Eject, 0, 0, &[]);
    disks.submit(&mut eject, ctx, 0);
    let frag = [PhysFrag { base: 0x1000, len: 512 }];
    let (mut read, mut flush, mut theirs) =
        (irp(IrpOp::Read, 0, 512, &frag), irp(IrpOp::Flush, 0, 0, &[]), irp(IrpOp::Read, 0, 512, &frag));
    for (r, c) in [(&mut read, ctx), (&mut flush, ctx), (&mut theirs, other)] {
        assert_eq!(disks.submit(r, c, 0), Action::None, "queued behind the eject");
    }
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    fake.answer(1, 0);
    let done = disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), 2);
    assert_eq!(done, Some(Action::Stopped { irp: &mut eject }));
    let inflight = disks.devs[i].lock().as_ref().unwrap().inflight.unwrap().owner;
    assert!(matches!(inflight, Owner::Irp { cmd, .. } if core::ptr::eq(cmd.irp, &theirs)), "the other unit's read");
    assert!(disks.due(2)[i].is_some(), "the hub thread looks at it");
    assert_eq!(disks.take_stopped(i), Some(Action::Ejected { irp: &mut read, said: Some((5, 0)) }), "said once");
    assert_eq!(disks.take_stopped(i), Some(Action::Complete { irp: &mut flush, status: 0, transferred: 0 }));
    assert_eq!(disks.take_stopped(i), Some(Action::Ejected { irp: &mut rescan, said: None }), "the held rescan");
    assert_eq!(disks.take_stopped(i), None);
    assert_eq!(disks.begin_rescan(i), BeginRescan::Nothing, "the rescan went with them");
    let mut after = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut after, ctx, 3), Action::Complete { irp: &mut after, status: 0, transferred: 0 });
}

/// **An eject the unit refuses leaves it running**, completed by the hub thread from the sense.
#[test]
fn an_eject_that_fails_leaves_the_unit_running() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let mut eject = irp(IrpOp::Eject, 0, 0, &[]);
    disks.submit(&mut eject, ctx, 0);
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    fake.answer(1, 1);
    assert_eq!(disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), 2), Some(Action::Wake));
    let (_, cmd, _) = disks.begin_recovery(i).unwrap();
    disks.end_recovery(i, cmd, End::Complete(KError::Unsupported as i32), 3);
    let mut read = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
    assert_eq!(disks.submit(&mut read, ctx, 4), Action::None, "started, not refused");
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_some());
}

#[test]
fn a_command_that_goes_wrong_is_left_for_the_hub_thread() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(2)).unwrap();
    let ctx = context(i, 0, epoch);
    let mut a = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut a, ctx, 0);
    assert_eq!(fake.trb(1, 0).0[3] & (1 << 5), 1 << 5, "with no data, the command wrapper interrupts");
    assert_eq!(disks.on_transfer(2, 4, code::SUCCESS, 0, 1), Some(Action::None), "and its event asks for the status");
    fake.answer(1, 1);
    assert_eq!(disks.on_transfer(2, 3, code::SUCCESS, csw_trb(&disks, i), 1), Some(Action::Wake));
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().fault, Some(Fault::Status(Status::Failed)));
    let mut b = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut b, ctx, 0), Action::None);
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().queue.len, 1, "waits behind the recovery");

    {
        let mut d = disks.devs[i].lock();
        let dev = d.as_mut().unwrap();
        dev.fault = None;
        dev.inflight = None;
        dev.recovering = true;
    }
    let mut during = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut during, ctx, 0), Action::None);
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().queue.len, 2, "queued, not started under the recovery");
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none());

    let (j, e2) = disks.take(fake.dev(3)).unwrap();
    let mut c = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
    disks.submit(&mut c, context(j, 0, e2), 0);
    assert_eq!(disks.on_transfer(3, 4, code::STALL, 0, 1), Some(Action::Wake), "a stall on the command wrapper");
    assert_eq!(disks.devs[j].lock().as_ref().unwrap().fault, Some(Fault::Transfer { dci: 4, code: code::STALL }));
}

#[test]
fn the_hub_thread_sleeps_no_longer_than_a_deadline_while_a_disk_is_bound() {
    init_global_heap();
    let disks = Disks::new();
    let (mut fake, mut other) = (Fake::new(), Fake::new());
    assert_eq!(disks.bound(0), None, "no disk: no bound");
    let (i, ei) = disks.take(fake.dev(5)).unwrap();
    let slept = 100;
    let until = disks.bound(slept);
    assert_eq!(until, Some(slept + DEADLINE_NS), "idle: a deadline's length");

    let mut a = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut a, context(i, 0, ei), 150), Action::None, "no wake");
    let woke = until.unwrap();
    assert!(woke <= 150 + DEADLINE_NS, "the thread wakes before the command is due");
    assert_eq!(disks.due(woke)[i], None, "and finds it not yet due");
    assert_eq!(disks.bound(woke), Some(150 + DEADLINE_NS), "so sleeps until its deadline");
    assert_eq!(disks.due(150 + DEADLINE_NS + 1)[i], Some(5), "and marks it then");

    let (j, ej) = disks.take(other.dev(6)).unwrap();
    let mut b = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut b, context(j, 0, ej), 120);
    assert_eq!(disks.bound(130), Some(120 + DEADLINE_NS), "the earliest command's deadline");
}

#[test]
fn a_deadline_marked_as_its_command_ends_is_not_charged_to_the_next() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let (mut a, mut b) = (irp(IrpOp::Flush, 0, 0, &[]), irp(IrpOp::Flush, 0, 0, &[]));
    disks.submit(&mut a, ctx, 0);
    disks.submit(&mut b, ctx, 0);
    assert_eq!(disks.due(DEADLINE_NS + 1)[i], Some(5), "past its deadline: marked");
    assert_eq!(disks.on_transfer(5, 4, code::SUCCESS, 0, DEADLINE_NS + 2), Some(Action::None));
    fake.answer(1, 0);
    let done = disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), DEADLINE_NS + 2);
    assert_eq!(done, Some(Action::Complete { irp: &mut a, status: 0, transferred: 0 }), "it made it");
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().fault, None, "the mark went with it");
    assert!(disks.begin_recovery(i).is_none(), "nothing for the hub thread to recover");
    let running = disks.devs[i].lock().as_ref().unwrap().inflight.map(|c| c.owner);
    let b_ptr = &mut b as *mut Irp;
    assert!(matches!(running, Some(Owner::Irp { cmd, .. }) if cmd.irp == b_ptr), "the next left running");
    assert_eq!(disks.due(DEADLINE_NS + 3)[i], None, "inside its own deadline");
}

#[test]
fn a_command_left_for_the_hub_thread_ignores_its_later_events() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let mut r = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
    disks.submit(&mut r, context(i, 0, epoch), 0);
    assert_eq!(disks.on_transfer(5, 4, code::STALL, 0, 1), Some(Action::Wake), "the command wrapper stalls");
    let data_trb = fake.rings[0].as_ptr() as u64;
    assert_eq!(disks.on_transfer(5, 3, code::SUCCESS, data_trb, 1), Some(Action::None), "the data's, ignored");
    assert_eq!(fake.trb(0, 1), Trb::default(), "no status asked for");
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().fault, Some(Fault::Transfer { dci: 4, code: code::STALL }));
}

#[test]
fn a_retry_is_in_flight_with_the_submit_that_waited_behind_it() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let mut a = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut a, ctx, 0);
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    fake.answer(1, 1);
    assert_eq!(disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), 1), Some(Action::Wake));
    let (fault, cmd, retried) = disks.begin_recovery(i).unwrap();
    assert_eq!((fault, cmd.irp, retried), (Fault::Status(Status::Failed), &mut a as *mut Irp, false));
    let mut b = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut b, ctx, 2), Action::None);
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none(), "it waits behind the recovery");
    assert_eq!(disks.end_recovery(i, cmd, End::Retry, 3), Action::None);
    let running = disks.devs[i].lock().as_ref().unwrap().inflight.map(|c| c.owner);
    let a_ptr = &mut a as *mut Irp;
    assert!(matches!(running, Some(Owner::Irp { cmd, retried: true }) if cmd.irp == a_ptr), "the retry runs");
    let (q, held) = disks.depart(i);
    assert_eq!((q[0], q[1]), (Some(&mut b as *mut Irp), None), "the one that waited, handed back");
    assert_eq!(held, [Some(&mut a as *mut Irp), None, None], "and the retry");
}

#[test]
fn a_stranded_command_waits_for_the_departure() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let mut a = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
    disks.submit(&mut a, ctx, 0);
    disks.due(DEADLINE_NS + 1);
    let (fault, cmd, _) = disks.begin_recovery(i).unwrap();
    assert_eq!(fault, Fault::Deadline);
    assert_eq!(disks.end_recovery(i, cmd, End::Stranded, DEADLINE_NS + 2), Action::None, "not completed");
    let mut b = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut b, ctx, DEADLINE_NS + 3);
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none(), "nothing starts on a device being ended");
    let (q, held) = disks.depart(i);
    assert_eq!(q[0], Some(&mut b as *mut Irp));
    let want = [None, Some(&mut a as *mut Irp), None];
    assert_eq!(held, want, "the stranded command, for after the slot is disabled");
}

#[test]
fn a_hub_command_the_dpc_reached_is_waited_for() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, _) = disks.take(fake.dev(5)).unwrap();
    let po = 0x1234 as *mut ();
    assert!(disks.issue_hub(i, po, 0, &scsi::test_unit_ready(), 0, false, 0));
    assert_eq!(disks.take_back(i), TakeBack::Taken, "before the DPC reached it");
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_none());
    assert!(disks.issue_hub(i, po, 0, &scsi::test_unit_ready(), 0, false, 0));
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    fake.answer(2, 0);
    assert_eq!(disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, i), 1), Some(Action::Hub { po }));
    assert_eq!(disks.take_back(i), TakeBack::OnItsWay, "after it: its completion is on its way");
    let result = disks.devs[i].lock().as_ref().unwrap().hub_result;
    assert_eq!(result, Some(HubResult::Status(Status::Passed { residue: 0 })));
}

#[test]
fn a_transfer_that_would_straddle_the_link_goes_after_it() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let mut dev = fake.dev(5);
    while dev.bulk_in.producer.room_before_link(RING_TRBS) > 2 {
        dev.bulk_in.producer.push(&mut RawSlots(dev.bulk_in.virt), Trb::no_op());
    }
    let before = dev.bulk_in.producer.next_slot();
    let (i, epoch) = disks.take(dev).unwrap();
    let frags = [0x1000, 0x2000, 0x3000].map(|base| PhysFrag { base, len: 512 });
    let mut r = irp(IrpOp::Read, 0, 1536, &frags);
    disks.submit(&mut r, context(i, 0, epoch), 0);
    assert_eq!((fake.trb(0, before).kind(), fake.trb(0, before + 1).kind()), (kind::NO_OP, kind::NO_OP), "padding");
    assert_eq!([0, 1, 2].map(|k| fake.trb(0, k).0[0]), [0x1000, 0x2000, 0x3000], "the read, whole, after the Link");
    assert_ne!(fake.trb(0, 0).cycle(), fake.trb(0, before).cycle(), "on the ring's next pass");
}

/// **The status is its wrapper's own event**: a read whose first TRB comes up short raises an
/// event there and another at its last TRB, which arrives on IN once the status is asked for
/// and is not the status.
#[test]
fn a_short_reads_last_data_event_is_not_its_status() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(9)).unwrap();
    let frags = [PhysFrag { base: 0x10_000, len: 4096 }, PhysFrag { base: 0x20_000, len: 4096 }];
    let mut r = irp(IrpOp::Read, 0, 8192, &frags);
    disks.submit(&mut r, context(i, 0, epoch), 0);
    let in_ring = fake.rings[0].as_ptr() as u64;
    assert_eq!(disks.on_transfer(9, 3, code::SHORT_PACKET, in_ring, 1), Some(Action::None), "short at the first");
    fake.answer_short(1, 4096, 0);
    let last = in_ring + 16;
    assert_eq!(disks.on_transfer(9, 3, code::SHORT_PACKET, last, 1), Some(Action::None), "the last TRB's own");
    assert!(matches!(disks.devs[i].lock().as_ref().unwrap().inflight.map(|c| c.stage), Some(Stage::Status { .. })));
    assert_eq!(
        disks.on_transfer(9, 3, code::SUCCESS, csw_trb(&disks, i), 1),
        Some(Action::Wake),
        "the status wrapper's"
    );
    let fault = disks.devs[i].lock().as_ref().unwrap().fault;
    assert_eq!(fault, Some(Fault::Status(Status::Passed { residue: 4096 })));
}

/// **A short read fails, whatever its status says**: the pages it did not fill would read as the
/// file. A hub command's answer shorter than asked for is the device's to give.
#[test]
fn a_short_read_whose_status_says_nothing_is_missing_fails() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(9)).unwrap();
    let frags = [PhysFrag { base: 0x10_000, len: 4096 }, PhysFrag { base: 0x20_000, len: 4096 }];
    let mut r = irp(IrpOp::Read, 0, 8192, &frags);
    disks.submit(&mut r, context(i, 0, epoch), 0);
    let in_ring = fake.rings[0].as_ptr() as u64;
    assert_eq!(disks.on_transfer(9, 3, code::SHORT_PACKET, in_ring, 1), Some(Action::None));
    fake.answer(1, 0);
    assert_eq!(disks.on_transfer(9, 3, code::SUCCESS, csw_trb(&disks, i), 1), Some(Action::Wake));
    assert_eq!(disks.devs[i].lock().as_ref().unwrap().fault, Some(Fault::Short), "left for reset recovery");

    let (j, _) = disks.take(fake.dev(5)).unwrap();
    let po = 0x1234 as *mut ();
    assert!(disks.issue_hub(j, po, 0, &scsi::inquiry(), 36, false, 0));
    disks.on_transfer(5, 4, code::SUCCESS, 0, 1);
    assert_eq!(disks.on_transfer(5, 3, code::SHORT_PACKET, 0, 1), Some(Action::None));
    fake.answer(1, 0);
    assert_eq!(disks.on_transfer(5, 3, code::SUCCESS, csw_trb(&disks, j), 1), Some(Action::Hub { po }));
    let result = disks.devs[j].lock().as_ref().unwrap().hub_result;
    assert_eq!(result, Some(HubResult::Status(Status::Passed { residue: 0 })));
}

#[test]
fn a_departed_devices_node_is_refused_and_its_slot_reused_under_a_new_epoch() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let mut taken = [(0, 0); MAX_DEVICES];
    for (k, t) in taken.iter_mut().enumerate() {
        *t = disks.take(fake.dev(10 + k as u8)).unwrap();
    }
    assert!(disks.take(fake.dev(99)).is_none(), "every slot bound");
    let (i, old_epoch) = taken[4];
    let old = context(i, 0, old_epoch);
    let mut inflight = irp(IrpOp::Flush, 0, 0, &[]);
    let mut queued = irp(IrpOp::Flush, 0, 0, &[]);
    disks.submit(&mut inflight, old, 0);
    disks.submit(&mut queued, old, 0);
    let (q, f) = disks.depart(disks.slot_of(14).unwrap());
    assert_eq!(q[0], Some(&mut queued as *mut Irp), "the queue handed back");
    assert_eq!(f, [Some(&mut inflight as *mut Irp), None, None], "and the command in flight");
    let mut late = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(
        disks.submit(&mut late, old, 0),
        Action::Complete { irp: &mut late, status: KError::PeerClosed as i32, transferred: 0 }
    );

    let (again, new_epoch) = disks.take(fake.dev(50)).unwrap();
    assert_eq!(again, i, "the departed device's slot");
    assert_ne!(new_epoch, old_epoch, "under a new epoch");
    let mut stale = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(
        disks.submit(&mut stale, old, 0),
        Action::Complete { irp: &mut stale, status: KError::PeerClosed as i32, transferred: 0 },
        "the old node reaches nothing of the new device"
    );
    let mut fresh = irp(IrpOp::Flush, 0, 0, &[]);
    assert_eq!(disks.submit(&mut fresh, context(i, 0, new_epoch), 0), Action::None, "the new node is served");
    assert!(disks.devs[i].lock().as_ref().unwrap().inflight.is_some());
}

/// **A departure completes its commands only once the slot is disabled**: a slot kept, its memory
/// leaked because the controller may still own it, keeps them too, their buffers maybe still
/// written; only a rescan, which moves no data, completes. They completed whatever became of it.
#[test]
fn a_departure_completes_its_commands_only_once_the_slot_is_disabled() {
    init_global_heap();
    crate::dpc::init().unwrap();
    let peer_closed = KError::PeerClosed as i32;
    let pending = crate::io::irp::IrpStatus::Pending as i32;
    for (slot, moving) in
        [(super::super::hub::Released::Freed, peer_closed), (super::super::hub::Released::Kept("wedged"), pending)]
    {
        let mut a = irp(IrpOp::Read, 0, 512, &[PhysFrag { base: 0x1000, len: 512 }]);
        let mut b = irp(IrpOp::Write, 0, 512, &[PhysFrag { base: 0x2000, len: 512 }]);
        let mut c = irp(IrpOp::Rescan, 0, 0, &[]);
        finish(Departed([Some(&mut a), Some(&mut b), Some(&mut c)]), &slot);
        crate::dpc::run_pending();
        assert_eq!((a.status, b.status), (moving, moving), "the command in flight and the stranded one");
        assert_eq!(c.status, peer_closed, "a rescan moves nothing, so it completes either way");
    }
}
