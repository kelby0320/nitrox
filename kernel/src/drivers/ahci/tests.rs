use super::*;

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
    // SAFETY: `words` is a full 256-word IDENTIFY result, which is the contract.
    let n = unsafe { format_identity(words.as_ptr(), &mut out) };
    (out, n)
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
    use crate::io::irp::{IrpBuffer, IrpOp};
    crate::mm::test_support::init_global_heap();
    crate::dpc::init().unwrap();
    for op in [IrpOp::Rescan, IrpOp::Eject] {
        let mut irp = Irp::new_block(op, core::ptr::null(), 0, 0, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        submit(&mut irp, core::ptr::null_mut());
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

/// **A disk on fake registers**: memory standing in for the ABAR, held while the disk is used.
fn fake_disk() -> (std::boxed::Box<[u32; 96]>, AhciDisk) {
    crate::mm::test_support::init_global_heap();
    crate::dpc::init().unwrap();
    let regs = std::boxed::Box::new([0u32; 96]);
    let abar = regs.as_ptr() as u64;
    let disk = AhciDisk {
        abar,
        port_base: abar + PORT_BASE,
        port: 0,
        cmd_list: DmaBuffer::alloc(1024).unwrap(),
        _fis: DmaBuffer::alloc(256).unwrap(),
        cmd_table: DmaBuffer::alloc(CMD_TABLE_BYTES).unwrap(),
        sectors: 1 << 20,
        flush_command: ATA_FLUSH_CACHE_EXT,
        inflight: AtomicPtr::new(core::ptr::null_mut()),
        pending: IrqSpinLock::new(LockRank::Leaf, PendingRing::new()),
        intr: core::ptr::null_mut(),
        issued_at: AtomicU64::new(0),
        counted: Counted::new(),
        fault: AtomicU8::new(CLEAR),
        wake: core::ptr::null_mut(),
        dead: AtomicBool::new(false),
    };
    (regs, disk)
}

/// A read of one sector into `frag`.
fn read_irp(frag: &PhysFrag) -> Irp {
    use crate::io::irp::{IRP_BUF_FRAGS, IrpBuffer, IrpOp};
    let buffer = IrpBuffer { kind: IRP_BUF_FRAGS, count: 1, frags: frag as *const PhysFrag as u64 };
    Irp::new_block(IrpOp::Read, core::ptr::null(), 0, 512, buffer, core::ptr::null_mut(), 0)
}

fn ctx(d: &AhciDisk) -> *mut () {
    d as *const AhciDisk as *mut ()
}

const IO_ERROR: i32 = crate::syscall::error::KError::IoError as i32;
const PENDING: i32 = IrpStatus::Pending as i32;

/// **A command a polled wait gave up on is failed, not retired**: it retired as done while it may
/// have been running yet, and the next was built over its slot.
#[test]
fn a_command_a_polled_wait_gave_up_on_is_failed_not_retired() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    submit(&mut first, ctx(&d));
    submit(&mut second, ctx(&d));
    write32(d.port_base, PX_TFD, 0x50);
    // SAFETY: slot 0's header is the disk's; DW1 is the byte count an HBA writes back.
    unsafe { (d.cmd_list.virt() as *mut u32).add(1).write_volatile(512) };
    assert_eq!(settle_polled(&d, false), Some(true));
    crate::dpc::run_pending();
    assert_eq!((first.status, first.transferred), (IO_ERROR, 0), "failed, whatever the count said");
    assert_eq!(d.inflight.load(Ordering::Acquire), &mut second as *mut Irp, "the next issued");
    assert_eq!(d.fault.load(Ordering::Acquire), CLEAR);
}

/// **A task file error is recovered, its command failed and the next issued**: a real HBA keeps the
/// command's bit in PxCI, so the request waited for ever; QEMU clears it, so both are taken.
#[test]
fn a_task_file_error_is_recovered_its_command_failed_and_the_next_issued() {
    for keeps in [true, false] {
        let (_regs, d) = fake_disk();
        let frag = PhysFrag { base: 0x10_0000, len: 512 };
        let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
        submit(&mut first, ctx(&d));
        submit(&mut second, ctx(&d));
        write32(d.port_base, PX_IS, PXIE_TFEE);
        write32(d.port_base, PX_TFD, 0x51);
        write32(d.port_base, PX_CI, keeps as u32);
        on_interrupt(&d);
        crate::dpc::run_pending();
        assert_eq!(first.status, PENDING, "keeps {keeps}: not retired, the watch thread's");
        assert_eq!(d.fault.load(Ordering::Acquire), FAULTED, "keeps {keeps}");
        let irp = claim(&d, 0, false).expect("a reported fault is claimed");
        assert_eq!(irp, &mut first as *mut Irp);
        assert_eq!(claim(&d, 0, true), None, "and only once");
        write32(d.port_base, PX_CI, 0);
        write32(d.port_base, PX_TFD, 0x50);
        on_interrupt(&d);
        crate::dpc::run_pending();
        assert_eq!(first.status, PENDING, "keeps {keeps}: an interrupt while it is recovered");
        write32(d.port_base, PX_SERR, 1 << 26);
        write32(d.port_base, PX_IS, 0);
        write32(d.abar, HBA_IS, 0);
        assert!(recover(&d, irp));
        crate::dpc::run_pending();
        assert_eq!(first.status, IO_ERROR, "keeps {keeps}");
        assert_eq!(d.inflight.load(Ordering::Acquire), &mut second as *mut Irp, "keeps {keeps}: the next issued");
        assert_eq!(d.fault.load(Ordering::Acquire), CLEAR, "keeps {keeps}");
        assert_eq!(read32(d.port_base, PX_SERR), !0, "keeps {keeps}: the error latched");
        assert_eq!(read32(d.port_base, PX_IS), !0, "keeps {keeps}");
        assert_eq!(read32(d.abar, HBA_IS), 1 << d.port, "keeps {keeps}");
        assert_eq!(read32(d.port_base, PX_CMD) & (CMD_FRE | CMD_ST), CMD_FRE | CMD_ST, "keeps {keeps}: restarted");
    }
}

/// **A command past its deadline is recovered and failed**, held at the deadline's neighbours.
#[test]
fn a_command_past_its_deadline_is_recovered_and_failed() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    assert_eq!(due_at(&d, 7), 7 + COMMAND_DEADLINE_NS, "idle: a deadline's length on");
    submit(&mut first, ctx(&d));
    submit(&mut second, ctx(&d));
    write32(d.port_base, PX_TFD, 0x50);
    let t = 1_000;
    d.issued_at.store(t, Ordering::Relaxed);
    assert_eq!(due_at(&d, t + 5), t + COMMAND_DEADLINE_NS + 1, "its deadline, before a deadline's length");
    assert_eq!(claim(&d, t + COMMAND_DEADLINE_NS, false), None, "at its deadline");
    let irp = claim(&d, t + COMMAND_DEADLINE_NS + 1, false).expect("past it");
    assert!(recover(&d, irp));
    crate::dpc::run_pending();
    assert_eq!(first.status, IO_ERROR);
    assert_eq!(d.inflight.load(Ordering::Acquire), &mut second as *mut Irp, "the next issued");
}

/// **A fault with nothing in flight holds what is submitted until the port is back**: one issued
/// onto the stopping port was forgotten when the recovery cleared slot 0.
#[test]
fn a_fault_with_nothing_in_flight_holds_what_comes_until_the_port_is_back() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    write32(d.port_base, PX_IS, 1 << 27);
    write32(d.port_base, PX_TFD, 0x50);
    on_interrupt(&d);
    submit(&mut first, ctx(&d));
    let idle = core::ptr::null_mut();
    assert_eq!((d.inflight.load(Ordering::Acquire), read32(d.port_base, PX_CI)), (idle, 0), "faulted: queued");
    let irp = claim(&d, 0, false).expect("a reported fault is claimed");
    assert_eq!(irp, idle, "with no command of its own");
    submit(&mut second, ctx(&d));
    drain_queue(&d);
    assert_eq!((d.inflight.load(Ordering::Acquire), read32(d.port_base, PX_CI)), (idle, 0), "recovering: queued");
    assert!(recover(&d, irp));
    crate::dpc::run_pending();
    assert_eq!(d.inflight.load(Ordering::Acquire), &mut first as *mut Irp, "the first issued once the port is back");
    assert_eq!(read32(d.port_base, PX_CI), 1);
    assert_eq!((first.status, second.status), (PENDING, PENDING));
}

/// A disk on a port that will not stop, its first command kept: the second was queued behind it.
fn given_up(d: &AhciDisk, first: &mut Irp, second: &mut Irp) {
    submit(first, ctx(d));
    submit(second, ctx(d));
    write32(d.port_base, PX_CMD, CMD_ST | CMD_FRE | CMD_CR | CMD_FR);
    let irp = claim(d, 0, true).expect("a forced claim");
    assert!(!recover(d, irp), "the port did not stop");
}

/// **A port that will not stop keeps its command and fails the rest**: the command's buffer may yet
/// be written, so it is never completed; what was queued, and what comes after, fails.
#[test]
fn a_port_that_will_not_stop_keeps_its_command_and_fails_the_rest() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second, mut third) = (read_irp(&frag), read_irp(&frag), read_irp(&frag));
    given_up(&d, &mut first, &mut second);
    crate::dpc::run_pending();
    assert!(d.dead.load(Ordering::Acquire));
    assert_eq!(first.status, PENDING, "kept");
    assert_eq!(d.inflight.load(Ordering::Acquire), &mut first as *mut Irp, "and still slot 0's");
    assert_eq!(second.status, IO_ERROR, "queued: failed");
    submit(&mut third, ctx(&d));
    crate::dpc::run_pending();
    assert_eq!(third.status, IO_ERROR, "after: refused");
}

/// **A request meeting the give-up at the port lock is refused**, not queued behind a kept one.
#[test]
fn a_request_meeting_the_give_up_at_the_port_lock_is_refused() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut late) = (read_irp(&frag), read_irp(&frag));
    submit(&mut first, ctx(&d));
    let late_irp = &mut late as *mut Irp as usize;
    let mut q = d.pending.lock();
    std::thread::scope(|s| {
        let d = &d;
        let waiting = s.spawn(move || submit(late_irp as *mut Irp, ctx(d)));
        // Long enough for `waiting` to be at the lock.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = give_up(d, &mut q);
        drop(q);
        waiting.join().unwrap();
    });
    crate::dpc::run_pending();
    assert_eq!(late.status, IO_ERROR);
}

/// **A poll of a disk given up waits for nothing**: the command it keeps will not finish.
#[test]
fn a_poll_of_a_disk_given_up_waits_for_nothing() {
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    given_up(&d, &mut first, &mut second);
    write32(d.port_base, PX_CI, 1);
    let before = clock();
    assert_eq!(poll_disk(&d), None);
    assert!(clock() - before < POLL_TIMEOUT_NS, "waited {} ms", (clock() - before) / 1_000_000);
}
