use super::*;
use crate::io::irp::Irp;

std::thread_local! {
    static NOW: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// The port's clock under a host test: each test its own, as each runs on a thread of its own.
pub(super) fn clock() -> u64 {
    NOW.with(|now| {
        now.set(now.get() + 1_000_000);
        now.get()
    })
}

/// Set this test's port clock, which its next read moves on from.
fn set_clock(ns: u64) {
    NOW.with(|now| now.set(ns));
}

#[test]
fn every_op_has_its_command_and_nothing_else_does() {
    use crate::io::irp::IrpOp;
    assert_eq!(ata_command(IrpOp::Read as u32, 0xEA), Some((ATA_READ_DMA_EXT, false)));
    assert_eq!(ata_command(IrpOp::Write as u32, 0xEA), Some((ATA_WRITE_DMA_EXT, true)));
    assert_eq!(ata_command(IrpOp::Flush as u32, 0xEA), Some((0xEA, false)), "the drive's flush");
    assert_eq!(ata_command(IrpOp::Flush as u32, 0xE7), Some((0xE7, false)));
    assert_eq!(ata_command(3, 0xEA), None, "not a read, which a count of 0 makes 65,536 sectors");
}

#[test]
fn a_drive_with_flush_cache_ext_is_flushed_with_it() {
    assert_eq!(flush_command(1 << 13), ATA_FLUSH_CACHE_EXT);
    assert_eq!(flush_command(0xFFFF), ATA_FLUSH_CACHE_EXT);
    assert_eq!(flush_command(!(1 << 13)), ATA_FLUSH_CACHE, "every other bit set, not this one");
    assert_eq!(flush_command(0), ATA_FLUSH_CACHE);
}

#[test]
fn the_prdt_capacity_is_what_fits_in_a_command_table() {
    assert_eq!(MAX_PRDT_ENTRIES, 248);
    assert!(PRDT_OFFSET + MAX_PRDT_ENTRIES as usize * PRDT_ENTRY <= CMD_TABLE_BYTES);
    assert!(PRDT_OFFSET + (MAX_PRDT_ENTRIES as usize + 1) * PRDT_ENTRY > CMD_TABLE_BYTES);
}

/// **A command's fragments stop at the table's room**: the last entry the page holds is written,
/// and one fragment more stops before a byte of the table is.
#[test]
fn a_command_past_the_tables_room_stops() {
    let (_regs, d) = fake_disk();
    let frag = |i: u64| PhysFrag { base: 0x10_0000 + i * 0x1000, len: 512 };
    let frags: std::vec::Vec<PhysFrag> = (0..=MAX_PRDT_ENTRIES as u64).map(frag).collect();
    let (full, over) = (&frags[..MAX_PRDT_ENTRIES as usize], &frags[..]);
    build_command(&d.state.lock(), ATA_READ_DMA_EXT, 0, 8, full, false);
    let last = PRDT_OFFSET + (MAX_PRDT_ENTRIES as usize - 1) * PRDT_ENTRY;
    let entry = d.state.lock().cmd_table.read::<u64>(last);
    assert_eq!(entry, frag(MAX_PRDT_ENTRIES as u64 - 1).base, "the page's last entry");
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        build_command(&d.state.lock(), ATA_WRITE_DMA_EXT, 0, 8, over, true);
    }));
    assert!(built.is_err(), "one fragment past the table's room");
    let fis = d.state.lock().cmd_table.read::<[u8; 3]>(0);
    assert_eq!(fis, [0x27, 0x80, ATA_READ_DMA_EXT], "the table untouched");
}

/// **A command is laid out as the HBA reads it**: the FIS's command, LBA and count, a PRDT entry a
/// fragment, and slot 0's header naming the FIS's length, the direction, the entries and the table.
#[test]
fn a_command_is_laid_out_as_the_hba_reads_it() {
    let (_regs, d) = fake_disk();
    let frags = [PhysFrag { base: 0x1_2345_6000, len: 4096 }, PhysFrag { base: 0x8000, len: 512 }];
    let p = d.state.lock();
    build_command(&p, ATA_WRITE_DMA_EXT, 0x0605_0403_0201, 0x0102, &frags, true);
    let fis: [u8; 14] = p.cmd_table.read(0);
    assert_eq!(fis, [0x27, 0x80, ATA_WRITE_DMA_EXT, 0, 1, 2, 3, 0x40, 4, 5, 6, 0, 2, 1]);
    assert_eq!(p.cmd_table.read::<[u32; 4]>(PRDT_OFFSET), [0x2345_6000, 1, 0, 4095 | 1 << 31]);
    assert_eq!(p.cmd_table.read::<[u32; 4]>(PRDT_OFFSET + PRDT_ENTRY), [0x8000, 0, 0, 511 | 1 << 31]);
    let table = p.cmd_table.phys().as_u64();
    assert_eq!(p.cmd_list.read::<[u32; 4]>(0), [5 | 1 << 6 | 2 << 16, 0, table as u32, (table >> 32) as u32]);
}

/// What that capacity is worth as a transfer, pinning the figure in `deferred-decisions.md`.
#[test]
fn one_command_covers_just_under_a_megabyte() {
    let bytes = MAX_PRDT_ENTRIES as usize * crate::mm::PAGE_SIZE;
    assert_eq!(bytes, 1_015_808);
    assert_eq!(bytes / 1024, 992);
}

/// Lay `text` into an IDENTIFY field as a drive does: byte-swapped within each word,
/// space-padded.
fn put_field(words: &mut [u16; 256], first: usize, width_words: usize, text: &[u8]) {
    let mut bytes = [b' '; 40];
    bytes[..text.len()].copy_from_slice(text);
    for i in 0..width_words {
        words[first + i] = u16::from(bytes[i * 2]) << 8 | u16::from(bytes[i * 2 + 1]);
    }
}

fn identity_of(model: &[u8], serial: &[u8]) -> ([u8; MAX_DEVICE_NAME], usize) {
    let mut words = [0u16; 256];
    put_field(&mut words, 27, 20, model);
    put_field(&mut words, 10, 10, serial);
    let mut out = [0u8; MAX_DEVICE_NAME];
    let n = format_identity(&words, &mut out);
    (out, n)
}

/// **A sector count past LBA48 is refused**: read whole, a non-zero word 103 gave 2^55 sectors or
/// more, and the boot log's size in MiB overflowed and stopped the boot.
#[test]
fn a_sector_count_past_lba48_is_refused() {
    let (mut words, mut name) = ([0u16; 256], [0u8; MAX_DEVICE_NAME]);
    words[100..=103].copy_from_slice(&[0xFFFF, 0xFFFF, 0xFFFF, 0]);
    assert_eq!(identity(&words, &mut name).map(|i| i.0), Some((1 << 48) - 1), "the most LBA48 addresses");
    words[100..=103].copy_from_slice(&[0, 0, 0, 1]);
    assert_eq!(identity(&words, &mut name), None, "2^48, one past it");
    words[103] = 0x0080;
    assert_eq!(identity(&words, &mut name), None);
    words[100..=103].fill(0);
    assert_eq!(identity(&words, &mut name), None, "no sectors");
}

fn assert_identity(model: &[u8], serial: &[u8], want: &[u8]) {
    let (out, n) = identity_of(model, serial);
    assert_eq!(
        &out[..n],
        want,
        "model {:?} serial {:?} produced {:?}",
        core::str::from_utf8(model),
        core::str::from_utf8(serial),
        core::str::from_utf8(&out[..n])
    );
}

/// **Padding is trimmed at both ends**, because the standard does not say which end a drive
/// pads.
#[test]
fn a_right_justified_field_loses_its_padding_too() {
    assert_identity(b"ST1000LM035-1RK172", b"            WDELGVZ3", b"ST1000LM035-1RK172 (WDELGVZ3)");
    assert_identity(b"QEMU HARDDISK", b"QM00001", b"QEMU HARDDISK (QM00001)");
    // Padding on both ends of both fields, interior spaces kept: `QEMU HARDDISK` has one.
    assert_identity(b"  A B  ", b"   C D  ", b"A B (C D)");
}

/// A field that is nothing but padding is empty, and a disk with no serial is named by its
/// model alone.
#[test]
fn an_all_padding_field_comes_back_empty() {
    assert_identity(b"ST1000LM035", b"                    ", b"ST1000LM035");
    assert_identity(b"                                        ", b"", b"");
}

#[test]
fn a_rescan_and_an_eject_are_refused_without_touching_a_disk() {
    let _exclusive = crate::dpc::exclusive();
    use crate::io::irp::{IrpBuffer, IrpOp};
    crate::mm::test_support::init_global_heap();
    for op in [IrpOp::Rescan, IrpOp::Eject] {
        let mut irp = Irp::new_block(op, core::ptr::null(), 0, 0, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        Port.submit(fly(&mut irp));
        crate::dpc::run_pending();
        assert_eq!(irp.status, crate::syscall::error::KError::Unsupported as i32, "{op:?}");
    }
}

/// A command the HBA says moved less than its length fails, and its pages count as nothing moved:
/// one byte short is a failure, the whole length a success, and a flush moves nothing and asks for
/// nothing.
#[test]
fn a_command_that_moved_less_than_its_length_fails() {
    let io_error = crate::syscall::error::KError::IoError as i32;
    let ok = IrpStatus::Success as i32;
    assert_eq!(completion(false, 8192, 8192), (ok, 8192));
    assert_eq!(completion(false, 8192, 8191), (io_error, 0), "a read stopped a byte early");
    assert_eq!(completion(false, 8192, 0), (io_error, 0));
    assert_eq!(completion(false, 0, 0), (ok, 0), "a flush");
    assert_eq!(completion(true, 8192, 8192), (io_error, 0), "the drive's error, whatever moved");
}

/// Memory standing in for the ABAR, port 0's registers in it.
fn fake_abar() -> Mmio<'static> {
    Mmio::over(std::boxed::Box::leak(std::boxed::Box::new([0u32; 96])))
}

/// **A test's disk, which keeps what its port still holds when the test ends**, as a disk given up
/// does: so only a request the driver let go of unfinished fails the test.
struct Fake(AhciDisk);

impl core::ops::Deref for Fake {
    type Target = AhciDisk;
    fn deref(&self) -> &AhciDisk {
        &self.0
    }
}

impl core::ops::DerefMut for Fake {
    fn deref_mut(&mut self) -> &mut AhciDisk {
        &mut self.0
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let (slot, mut queue) = give_up(&mut self.0.state.lock());
        if let Some(slot) = slot {
            slot.keep();
        }
        while let Some(irp) = queue.pop() {
            irp.keep();
        }
    }
}

/// **A disk on fake registers**, and the ABAR they are in.
fn fake_disk() -> (Mmio<'static>, Fake) {
    crate::mm::test_support::init_global_heap();
    let abar = fake_abar();
    let disk = AhciDisk {
        abar,
        port_base: port_regs(abar, 0).unwrap(),
        port: 0,
        _fis: Programmed::new(DmaBuffer::alloc(256).unwrap()),
        sectors: 1 << 20,
        flush_command: ATA_FLUSH_CACHE_EXT,
        state: IrqSpinLock::new(
            LockRank::Leaf,
            PortState::new(
                Programmed::new(DmaBuffer::alloc(1024).unwrap()),
                Programmed::new(DmaBuffer::alloc(CMD_TABLE_BYTES).unwrap()),
            ),
        ),
        intr: interrupt_object(),
        counted: Counted::new(),
        wake: interrupt_object(),
    };
    (abar, Fake(disk))
}

fn interrupt_object() -> Ref<InterruptObject> {
    Ref::new(InterruptObject::try_new().unwrap())
}

/// `irp`, in flight: dropped by the driver, it fails the test.
fn fly(irp: *mut Irp) -> InFlight {
    // SAFETY: the test's IRP, which outlives the disk it is submitted to.
    unsafe { InFlight::for_test(irp) }
}

/// Whether slot 0's command is `irp`.
fn in_slot(d: &AhciDisk, irp: &Irp) -> bool {
    d.state.lock().inflight.as_ref().is_some_and(|f| f.is(irp))
}

/// A read of one sector into `frag`.
fn read_irp(frag: &PhysFrag) -> Irp {
    use crate::io::irp::{IRP_BUF_FRAGS, IrpBuffer, IrpOp};
    let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: 1, frags: frag as *const PhysFrag as u64 };
    Irp::new_block(IrpOp::Read, core::ptr::null(), 0, 512, buffer, core::ptr::null_mut(), 0)
}

const IO_ERROR: i32 = crate::syscall::error::KError::IoError as i32;
const PENDING: i32 = IrpStatus::Pending as i32;

/// **A command issued after the interrupt read slot 0 idle is not retired**: the ISR read PxCI
/// outside the port lock, and took the command another CPU had just issued.
#[test]
fn a_command_issued_after_the_slot_read_idle_is_not_retired() {
    let _exclusive = crate::dpc::exclusive();
    use crate::io::irp::{IRP_BUF_NONE, IrpBuffer, IrpOp};
    for flush in [false, true] {
        let (_regs, d) = fake_disk();
        let frag = PhysFrag { base: 0x10_0000, len: 512 };
        let none = IrpBuffer { kind: IRP_BUF_NONE, count: 0, frags: 0 };
        let mut irp = match flush {
            false => read_irp(&frag),
            true => Irp::new_block(IrpOp::Flush, core::ptr::null(), 0, 0, none, core::ptr::null_mut(), 0),
        };
        assert_eq!(d.port_base.read32(PX_CI), 0, "flush {flush}: slot 0 idle when the interrupt came");
        submit(&d, fly(&mut irp));
        assert_eq!(d.port_base.read32(PX_CI), 1, "flush {flush}: issued since, on another CPU");
        let retired = take_retired(&d, &mut d.state.lock());
        crate::dpc::run_pending();
        assert!(retired.is_none(), "flush {flush}: retired while the HBA runs it");
        assert_eq!(irp.status, PENDING, "flush {flush}");
        assert!(in_slot(&d, &irp), "flush {flush}: still in flight");
    }
}

/// **A command a polled wait gave up on is failed, not retired**: it retired as done while it may
/// have been running yet, and the next was built over its slot.
#[test]
fn a_command_a_polled_wait_gave_up_on_is_failed_not_retired() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    submit(&d, fly(&mut first));
    submit(&d, fly(&mut second));
    d.port_base.write32(PX_TFD, 0x50);
    // Slot 0's header DW1: the byte count an HBA writes back.
    d.state.lock().cmd_list.write::<u32>(4, 512);
    assert_eq!(settle_polled(&d, false), Some(true));
    crate::dpc::run_pending();
    assert_eq!((first.status, first.transferred), (IO_ERROR, 0), "failed, whatever the count said");
    assert!(in_slot(&d, &second), "the next issued");
    assert_eq!(d.state.lock().fault, CLEAR);
}

/// **A retired command is done by the bytes slot 0's header counted**: its whole length a success,
/// a byte short a failure.
#[test]
fn a_retired_command_is_done_by_the_bytes_its_header_counted() {
    let _exclusive = crate::dpc::exclusive();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    for (counted, done) in [(512, (IrpStatus::Success as i32, 512)), (511, (IO_ERROR, 0))] {
        let (_regs, d) = fake_disk();
        let mut r = read_irp(&frag);
        submit(&d, fly(&mut r));
        d.port_base.write32(PX_CI, 0);
        d.port_base.write32(PX_TFD, 0x50);
        d.state.lock().cmd_list.write::<u32>(4, counted);
        assert_eq!(settle_polled(&d, true), Some(true), "retired");
        crate::dpc::run_pending();
        assert_eq!((r.status, r.transferred), done, "{counted} bytes counted");
    }
}

/// **A task file error is recovered, its command failed and the next issued**: a real HBA keeps the
/// command's bit in PxCI, so the request waited for ever; QEMU clears it, so both are taken.
#[test]
fn a_task_file_error_is_recovered_its_command_failed_and_the_next_issued() {
    let _exclusive = crate::dpc::exclusive();
    for keeps in [true, false] {
        let (_regs, d) = fake_disk();
        let frag = PhysFrag { base: 0x10_0000, len: 512 };
        let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
        submit(&d, fly(&mut first));
        submit(&d, fly(&mut second));
        d.port_base.write32(PX_IS, PXIE_TFEE);
        d.port_base.write32(PX_TFD, 0x51);
        d.port_base.write32(PX_CI, keeps as u32);
        on_interrupt(&d);
        crate::dpc::run_pending();
        assert_eq!(first.status, PENDING, "keeps {keeps}: not retired, the watch thread's");
        assert_eq!(d.state.lock().fault, FAULTED, "keeps {keeps}");
        assert!(claim(&d, 0, false), "a reported fault is claimed");
        assert!(in_slot(&d, &first), "for its command");
        assert!(!claim(&d, 0, true), "and only once");
        d.port_base.write32(PX_CI, 0);
        d.port_base.write32(PX_TFD, 0x50);
        on_interrupt(&d);
        crate::dpc::run_pending();
        assert_eq!(first.status, PENDING, "keeps {keeps}: an interrupt while it is recovered");
        d.port_base.write32(PX_SERR, 1 << 26);
        d.port_base.write32(PX_IS, 0);
        d.abar.write32(HBA_IS, 0);
        assert!(recover(&d));
        crate::dpc::run_pending();
        assert_eq!(first.status, IO_ERROR, "keeps {keeps}");
        assert!(in_slot(&d, &second), "keeps {keeps}: the next issued");
        assert_eq!(d.state.lock().fault, CLEAR, "keeps {keeps}");
        assert_eq!(d.port_base.read32(PX_SERR), !0, "keeps {keeps}: the error latched");
        assert_eq!(d.port_base.read32(PX_IS), !0, "keeps {keeps}");
        assert_eq!(d.abar.read32(HBA_IS), 1 << d.port, "keeps {keeps}");
        assert_eq!(d.port_base.read32(PX_CMD) & (CMD_FRE | CMD_ST), CMD_FRE | CMD_ST, "keeps {keeps}: restarted");
    }
}

/// **A command past its deadline is recovered and failed**, held at the deadline's neighbours.
#[test]
fn a_command_past_its_deadline_is_recovered_and_failed() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    assert_eq!(due_at(&d, 7), 7 + COMMAND_DEADLINE_NS, "idle: a deadline's length on");
    submit(&d, fly(&mut first));
    submit(&d, fly(&mut second));
    d.port_base.write32(PX_TFD, 0x50);
    let t = 1_000;
    d.state.lock().issued_at = t;
    assert_eq!(due_at(&d, t + 5), t + COMMAND_DEADLINE_NS + 1, "its deadline, before a deadline's length");
    assert!(!claim(&d, t + COMMAND_DEADLINE_NS, false), "at its deadline");
    assert!(claim(&d, t + COMMAND_DEADLINE_NS + 1, false), "past it");
    assert!(recover(&d));
    crate::dpc::run_pending();
    assert_eq!(first.status, IO_ERROR);
    assert!(in_slot(&d, &second), "the next issued");
}

/// **A fault with nothing in flight holds what is submitted until the port is back**: one issued
/// onto the stopping port was forgotten when the recovery cleared slot 0.
#[test]
fn a_fault_with_nothing_in_flight_holds_what_comes_until_the_port_is_back() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    d.port_base.write32(PX_IS, 1 << 27);
    d.port_base.write32(PX_TFD, 0x50);
    on_interrupt(&d);
    submit(&d, fly(&mut first));
    let idle = || (d.state.lock().inflight.is_none(), d.port_base.read32(PX_CI));
    assert_eq!(idle(), (true, 0), "faulted: queued");
    assert!(claim(&d, 0, false), "a reported fault is claimed");
    assert!(d.state.lock().inflight.is_none(), "with no command of its own");
    submit(&d, fly(&mut second));
    drain_queue(&d);
    assert_eq!(idle(), (true, 0), "recovering: queued");
    assert!(recover(&d));
    crate::dpc::run_pending();
    assert!(in_slot(&d, &first), "the first issued once the port is back");
    assert_eq!(d.port_base.read32(PX_CI), 1);
    assert_eq!((first.status, second.status), (PENDING, PENDING));
}

/// A disk on a port that will not stop, its first command kept: the second was queued behind it.
fn given_up(d: &AhciDisk, first: InFlight, second: InFlight) {
    submit(d, first);
    submit(d, second);
    d.port_base.write32(PX_CMD, CMD_ST | CMD_FRE | CMD_CR | CMD_FR);
    assert!(claim(d, 0, true), "a forced claim");
    assert!(!recover(d), "the port did not stop");
}

/// **A port that will not stop keeps its command and fails the rest**: the command's buffer may yet
/// be written, so it is never completed; what was queued, and what comes after, fails.
#[test]
fn a_port_that_will_not_stop_keeps_its_command_and_fails_the_rest() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second, mut third) = (read_irp(&frag), read_irp(&frag), read_irp(&frag));
    given_up(&d, fly(&mut first), fly(&mut second));
    crate::dpc::run_pending();
    assert!(d.state.lock().dead);
    assert_eq!(first.status, PENDING, "kept");
    assert!(d.state.lock().inflight.is_none(), "for good, out of slot 0");
    assert_eq!(second.status, IO_ERROR, "queued: failed");
    submit(&d, fly(&mut third));
    crate::dpc::run_pending();
    assert_eq!(third.status, IO_ERROR, "after: refused");
}

/// **A request meeting the give-up at the port lock is refused**, not queued behind a kept one.
#[test]
fn a_request_meeting_the_give_up_at_the_port_lock_is_refused() {
    let exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut late) = (read_irp(&frag), read_irp(&frag));
    submit(&d, fly(&mut first));
    let late_irp = &mut late as *mut Irp as usize;
    let mut q = d.state.lock();
    std::thread::scope(|s| {
        let (d, exclusive) = (&d, &exclusive);
        let waiting = s.spawn(move || {
            let _shared = exclusive.share();
            submit(d, fly(late_irp as *mut Irp))
        });
        // Long enough for `waiting` to be at the lock.
        crate::mm::test_support::settle();
        if let (Some(kept), _) = give_up(&mut q) {
            kept.keep();
        }
        drop(q);
        waiting.join().unwrap();
    });
    crate::dpc::run_pending();
    assert_eq!(late.status, IO_ERROR);
}

/// **A poll of a disk given up waits for nothing**: the command it keeps will not finish.
#[test]
fn a_poll_of_a_disk_given_up_waits_for_nothing() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    given_up(&d, fly(&mut first), fly(&mut second));
    d.port_base.write32(PX_CI, 1);
    let before = clock();
    assert_eq!(poll_disk(&d), None);
    assert!(clock() - before < POLL_TIMEOUT_NS, "waited {} ms", (clock() - before) / 1_000_000);
}

/// **An interrupt queues the fault DPC on a new fault and the drain DPC on a retire**, and neither
/// otherwise: the first wakes the watch thread, the second issues the command queued behind. Every
/// interrupt queues the one that signals the disk's interrupt object.
#[test]
fn an_interrupt_queues_the_fault_dpc_on_a_fault_and_the_drain_dpc_on_a_retire() {
    let _exclusive = crate::dpc::exclusive();
    let addr = |dpc: &Dpc| dpc as *const Dpc as usize;
    let (fault, drain, intr) = (addr(&AHCI_FAULT_DPC), addr(&AHCI_DRAIN_DPC), addr(&AHCI_INTR_DPC));
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    submit(&d, fly(&mut first));
    submit(&d, fly(&mut second));
    crate::dpc::take_enqueued();
    on_interrupt(&d);
    let queued = crate::dpc::take_enqueued();
    assert!(!queued.contains(&drain) && !queued.contains(&fault), "a command running yet");
    assert!(queued.contains(&intr));
    d.port_base.write32(PX_CI, 0);
    on_interrupt(&d);
    let queued = crate::dpc::take_enqueued();
    assert!(queued.contains(&drain), "a retire, with a command queued behind it");
    assert!(!queued.contains(&fault) && queued.contains(&intr));
    d.port_base.write32(PX_IS, PXIE_TFEE);
    d.port_base.write32(PX_TFD, 0x51);
    on_interrupt(&d);
    let queued = crate::dpc::take_enqueued();
    assert!(queued.contains(&fault), "a task file error");
    assert!(!queued.contains(&drain) && queued.contains(&intr), "and nothing retired");
    on_interrupt(&d);
    assert!(!crate::dpc::take_enqueued().contains(&fault), "a fault reported already");
    crate::dpc::run_pending();
}

/// **A command's device time runs from its issue to its retire**, whichever way it retires: by an
/// interrupt, a polled wait or a recovery. Issued at 1,000 s and retired 5 s on.
#[test]
fn a_commands_device_time_runs_from_its_issue_to_its_retire() {
    let _exclusive = crate::dpc::exclusive();
    use crate::io::stats::IoStats;
    const ISSUE: u64 = 1_000_000_000_000;
    const ON_DEVICE: u64 = 5_000_000_000;
    for way in ["interrupt", "polled", "recovered"] {
        static STATS: IoStats = IoStats::new();
        let before = STATS.record_for(0, 0).busy_ns;
        let (_regs, d) = fake_disk();
        d.counted.attach_static(&STATS);
        let frag = PhysFrag { base: 0x10_0000, len: 512 };
        let mut irp = read_irp(&frag);
        set_clock(ISSUE);
        submit(&d, fly(&mut irp));
        set_clock(ISSUE + ON_DEVICE);
        match way {
            "interrupt" => {
                d.port_base.write32(PX_CI, 0);
                on_interrupt(&d);
            }
            "polled" => {
                d.port_base.write32(PX_CI, 0);
                assert_eq!(settle_polled(&d, true), Some(true));
            }
            _ => {
                d.port_base.write32(PX_IS, PXIE_TFEE);
                d.port_base.write32(PX_TFD, 0x51);
                on_interrupt(&d);
                assert!(claim(&d, 0, false), "a reported fault is claimed");
                d.port_base.write32(PX_CI, 0);
                d.port_base.write32(PX_TFD, 0x50);
                assert!(recover(&d));
            }
        }
        crate::dpc::run_pending();
        let busy = STATS.record_for(0, 0).busy_ns - before;
        assert!((ON_DEVICE..ON_DEVICE + 1_000_000_000).contains(&busy), "{way}: {busy} ns on the device");
    }
}

/// **A disk declined after its port started frees its memory only once the port stopped**: a
/// command list still running may yet write the received-FIS area or move IDENTIFY's data.
#[test]
fn a_declined_disk_frees_its_memory_only_once_its_port_stopped() {
    crate::mm::test_support::init_global_heap();
    let freed = crate::mm::heap::frames_freed_by_this_thread();
    let claimed = claim_port(fake_abar(), 0).map(|_| ());
    assert_eq!(claimed, Err("IDENTIFY failed"), "nothing answers on fake registers");
    let gave = crate::mm::heap::frames_freed_by_this_thread() - freed;
    assert_eq!(gave, 4, "its three port pages and IDENTIFY's, the port stopped");
    for stops in [true, false] {
        let (_regs, mut d) = fake_disk();
        if !stops {
            d.port_base.write32(PX_CMD, CMD_CR);
        }
        let freed = crate::mm::heap::frames_freed_by_this_thread();
        let data = Programmed::new(DmaBuffer::alloc(SECTOR_SIZE as usize).unwrap());
        // SAFETY: the fake disk, polled, with no interrupt.
        assert!(unsafe { identify(&d, data, &mut [0; MAX_DEVICE_NAME]) }.is_none());
        give_back(&mut d);
        drop(d);
        let gave = crate::mm::heap::frames_freed_by_this_thread() - freed;
        assert_eq!(gave, if stops { 4 } else { 0 }, "stops {stops}: IDENTIFY's page and the port's three");
    }
}

/// **A port whose registers lie past the ABAR is not claimed**: the fake ABAR holds port 0's alone.
#[test]
fn a_port_past_the_abar_is_not_claimed() {
    crate::mm::test_support::init_global_heap();
    let abar = fake_abar();
    assert!(port_regs(abar, 0).is_some_and(|p| p.len() == PORT_STRIDE), "port 0, at the ABAR's end");
    assert!(port_regs(abar, 1).is_none(), "port 1, past it");
    assert_eq!(claim_port(abar, 1).map(|_| ()), Err("its registers lie past the ABAR"));
}

/// **A port that will not stop is given nothing**: its command list was programmed and IDENTIFY
/// issued on a command engine still running, which a list may be given only once it has stopped.
#[test]
fn a_port_that_will_not_stop_is_given_no_command_list() {
    crate::mm::test_support::init_global_heap();
    let abar = fake_abar();
    let pb = port_regs(abar, 0).unwrap();
    pb.write32(PX_CMD, CMD_CR);
    assert_eq!(claim_port(abar, 0).map(|_| ()), Err("its port would not stop"));
    assert_eq!((pb.read32(PX_CLB), pb.read32(PX_FB), pb.read32(PX_CI)), (0, 0, 0), "nothing given");
}

/// **A submit past a full queue is refused `IoError`**: slot 0 busy, the queue full behind it.
#[test]
fn a_submit_past_a_full_queue_is_refused() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let mut irps: std::vec::Vec<Irp> = (0..PENDING_DEPTH + 2).map(|_| read_irp(&frag)).collect();
    for r in irps.iter_mut() {
        submit(&d, fly(r));
    }
    crate::dpc::run_pending();
    let (last, held) = irps.split_last().unwrap();
    assert!(held.iter().all(|r| r.status == PENDING), "issued, then queued");
    assert_eq!(last.status, IO_ERROR, "past the queue: refused");
}

/// **A request the port cannot take is refused `InvalidArgument`**: more fragments than a command
/// table holds, or an op with no ATA command, at an idle slot 0 or reaching it from the queue.
#[test]
fn a_request_the_port_cannot_take_is_refused() {
    let _exclusive = crate::dpc::exclusive();
    use crate::io::irp::{IRP_BUF_FRAGS, IrpBuffer, IrpOp};
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let many = std::vec![frag; MAX_PRDT_ENTRIES as usize + 1];
    let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: many.len() as u32, frags: many.as_ptr() as u64 };
    let len = 512 * many.len() as u64;
    let mut wide = Irp::new_block(IrpOp::Read, core::ptr::null(), 0, len, buffer, core::ptr::null_mut(), 0);
    let rescan = || Irp::new_block(IrpOp::Rescan, core::ptr::null(), 0, 0, IrpBuffer::NONE, core::ptr::null_mut(), 0);
    let (mut idle, mut first, mut queued) = (rescan(), read_irp(&frag), rescan());
    for r in [&mut wide as *mut Irp, &mut idle, &mut first, &mut queued] {
        submit(&d, fly(r));
    }
    crate::dpc::run_pending();
    let invalid = crate::syscall::error::KError::InvalidArgument as i32;
    assert_eq!((wide.status, idle.status), (invalid, invalid), "at submit");
    assert_eq!(queued.status, PENDING, "behind slot 0's read");
    d.port_base.write32(PX_CI, 0);
    d.port_base.write32(PX_TFD, 0x50);
    d.state.lock().cmd_list.write::<u32>(4, 512);
    assert_eq!(settle_polled(&d, true), Some(true));
    crate::dpc::run_pending();
    assert_eq!(queued.status, invalid, "reaching slot 0 from the queue");
}

/// **A request with no disk published is refused `PeerClosed`**: the port names the disk AHCI
/// publishes, and a host test publishes none.
#[test]
fn a_request_with_no_disk_published_is_refused() {
    let _exclusive = crate::dpc::exclusive();
    crate::mm::test_support::init_global_heap();
    assert!(AHCI.get().is_none());
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let mut r = read_irp(&frag);
    Port.submit(fly(&mut r));
    crate::dpc::run_pending();
    assert_eq!(r.status, crate::syscall::error::KError::PeerClosed as i32);
}
