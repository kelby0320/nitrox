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

/// Memory standing in for the ABAR, freed when the test is done with its disk.
struct FakeRegs(*mut [u32; 96]);

impl Drop for FakeRegs {
    fn drop(&mut self) {
        // SAFETY: `fake_disk` made the pointer with `Box::into_raw`, and nothing frees it but this.
        drop(unsafe { std::boxed::Box::from_raw(self.0) });
    }
}

/// **A disk on fake registers**: memory standing in for the ABAR, held while the disk is used.
/// The registers are written through `abar`, so it comes from `into_raw`, a pointer that may
/// write, not from a shared borrow of the box.
fn fake_disk() -> (FakeRegs, AhciDisk) {
    crate::mm::test_support::init_global_heap();
    let regs = FakeRegs(std::boxed::Box::into_raw(std::boxed::Box::new([0u32; 96])));
    let abar = regs.0 as u64;
    let disk = AhciDisk {
        abar,
        port_base: abar + PORT_BASE,
        port: 0,
        cmd_list: DmaBuffer::alloc(1024).unwrap(),
        _fis: DmaBuffer::alloc(256).unwrap(),
        cmd_table: DmaBuffer::alloc(CMD_TABLE_BYTES).unwrap(),
        sectors: 1 << 20,
        flush_command: ATA_FLUSH_CACHE_EXT,
        state: IrqSpinLock::new(LockRank::Leaf, PortState::new()),
        intr: interrupt_object(),
        counted: Counted::new(),
        wake: interrupt_object(),
    };
    (regs, disk)
}

fn interrupt_object() -> ObjectRef {
    // SAFETY: a fresh `InterruptObject`, adopted under its own tag.
    unsafe { crate::drivers::adopt(InterruptObject::try_new().unwrap(), KObjectType::InterruptObject) }
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
        assert_eq!(read32(d.port_base, PX_CI), 0, "flush {flush}: slot 0 idle when the interrupt came");
        submit(&mut irp, ctx(&d));
        assert_eq!(read32(d.port_base, PX_CI), 1, "flush {flush}: issued since, on another CPU");
        let (retired, ..) = take_retired(&d, &mut d.state.lock());
        crate::dpc::run_pending();
        assert!(retired.is_null(), "flush {flush}: retired while the HBA runs it");
        assert_eq!(irp.status, PENDING, "flush {flush}");
        assert!(core::ptr::eq(d.state.lock().inflight, &irp), "flush {flush}: still in flight");
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
    submit(&mut first, ctx(&d));
    submit(&mut second, ctx(&d));
    write32(d.port_base, PX_TFD, 0x50);
    // SAFETY: slot 0's header is the disk's; DW1 is the byte count an HBA writes back.
    unsafe { (d.cmd_list.virt() as *mut u32).add(1).write_volatile(512) };
    assert_eq!(settle_polled(&d, false), Some(true));
    crate::dpc::run_pending();
    assert_eq!((first.status, first.transferred), (IO_ERROR, 0), "failed, whatever the count said");
    assert!(core::ptr::eq(d.state.lock().inflight, &second), "the next issued");
    assert_eq!(d.state.lock().fault, CLEAR);
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
        submit(&mut first, ctx(&d));
        submit(&mut second, ctx(&d));
        write32(d.port_base, PX_IS, PXIE_TFEE);
        write32(d.port_base, PX_TFD, 0x51);
        write32(d.port_base, PX_CI, keeps as u32);
        on_interrupt(&d);
        crate::dpc::run_pending();
        assert_eq!(first.status, PENDING, "keeps {keeps}: not retired, the watch thread's");
        assert_eq!(d.state.lock().fault, FAULTED, "keeps {keeps}");
        let irp = claim(&d, 0, false).expect("a reported fault is claimed");
        assert!(core::ptr::eq(irp, &first));
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
        assert!(core::ptr::eq(d.state.lock().inflight, &second), "keeps {keeps}: the next issued");
        assert_eq!(d.state.lock().fault, CLEAR, "keeps {keeps}");
        assert_eq!(read32(d.port_base, PX_SERR), !0, "keeps {keeps}: the error latched");
        assert_eq!(read32(d.port_base, PX_IS), !0, "keeps {keeps}");
        assert_eq!(read32(d.abar, HBA_IS), 1 << d.port, "keeps {keeps}");
        assert_eq!(read32(d.port_base, PX_CMD) & (CMD_FRE | CMD_ST), CMD_FRE | CMD_ST, "keeps {keeps}: restarted");
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
    submit(&mut first, ctx(&d));
    submit(&mut second, ctx(&d));
    write32(d.port_base, PX_TFD, 0x50);
    let t = 1_000;
    d.state.lock().issued_at = t;
    assert_eq!(due_at(&d, t + 5), t + COMMAND_DEADLINE_NS + 1, "its deadline, before a deadline's length");
    assert_eq!(claim(&d, t + COMMAND_DEADLINE_NS, false), None, "at its deadline");
    let irp = claim(&d, t + COMMAND_DEADLINE_NS + 1, false).expect("past it");
    assert!(recover(&d, irp));
    crate::dpc::run_pending();
    assert_eq!(first.status, IO_ERROR);
    assert!(core::ptr::eq(d.state.lock().inflight, &second), "the next issued");
}

/// **A fault with nothing in flight holds what is submitted until the port is back**: one issued
/// onto the stopping port was forgotten when the recovery cleared slot 0.
#[test]
fn a_fault_with_nothing_in_flight_holds_what_comes_until_the_port_is_back() {
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second) = (read_irp(&frag), read_irp(&frag));
    write32(d.port_base, PX_IS, 1 << 27);
    write32(d.port_base, PX_TFD, 0x50);
    on_interrupt(&d);
    submit(&mut first, ctx(&d));
    let idle = core::ptr::null_mut();
    assert_eq!((d.state.lock().inflight, read32(d.port_base, PX_CI)), (idle, 0), "faulted: queued");
    let irp = claim(&d, 0, false).expect("a reported fault is claimed");
    assert_eq!(irp, idle, "with no command of its own");
    submit(&mut second, ctx(&d));
    drain_queue(&d);
    assert_eq!((d.state.lock().inflight, read32(d.port_base, PX_CI)), (idle, 0), "recovering: queued");
    assert!(recover(&d, irp));
    crate::dpc::run_pending();
    assert!(core::ptr::eq(d.state.lock().inflight, &first), "the first issued once the port is back");
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
    let _exclusive = crate::dpc::exclusive();
    let (_regs, d) = fake_disk();
    let frag = PhysFrag { base: 0x10_0000, len: 512 };
    let (mut first, mut second, mut third) = (read_irp(&frag), read_irp(&frag), read_irp(&frag));
    given_up(&d, &mut first, &mut second);
    crate::dpc::run_pending();
    assert!(d.state.lock().dead);
    assert_eq!(first.status, PENDING, "kept");
    assert!(core::ptr::eq(d.state.lock().inflight, &first), "and still slot 0's");
    assert_eq!(second.status, IO_ERROR, "queued: failed");
    submit(&mut third, ctx(&d));
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
    submit(&mut first, ctx(&d));
    let late_irp = &mut late as *mut Irp as usize;
    let mut q = d.state.lock();
    std::thread::scope(|s| {
        let (d, exclusive) = (&d, &exclusive);
        let waiting = s.spawn(move || {
            let _shared = exclusive.share();
            submit(late_irp as *mut Irp, ctx(d))
        });
        // Long enough for `waiting` to be at the lock.
        std::thread::sleep(std::time::Duration::from_millis(50));
        let _ = give_up(&mut q);
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
    given_up(&d, &mut first, &mut second);
    write32(d.port_base, PX_CI, 1);
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
    submit(&mut first, ctx(&d));
    submit(&mut second, ctx(&d));
    crate::dpc::take_enqueued();
    on_interrupt(&d);
    let queued = crate::dpc::take_enqueued();
    assert!(!queued.contains(&drain) && !queued.contains(&fault), "a command running yet");
    assert!(queued.contains(&intr));
    write32(d.port_base, PX_CI, 0);
    on_interrupt(&d);
    let queued = crate::dpc::take_enqueued();
    assert!(queued.contains(&drain), "a retire, with a command queued behind it");
    assert!(!queued.contains(&fault) && queued.contains(&intr));
    write32(d.port_base, PX_IS, PXIE_TFEE);
    write32(d.port_base, PX_TFD, 0x51);
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
        submit(&mut irp, ctx(&d));
        set_clock(ISSUE + ON_DEVICE);
        match way {
            "interrupt" => {
                write32(d.port_base, PX_CI, 0);
                on_interrupt(&d);
            }
            "polled" => {
                write32(d.port_base, PX_CI, 0);
                assert_eq!(settle_polled(&d, true), Some(true));
            }
            _ => {
                write32(d.port_base, PX_IS, PXIE_TFEE);
                write32(d.port_base, PX_TFD, 0x51);
                on_interrupt(&d);
                let taken = claim(&d, 0, false).expect("a reported fault is claimed");
                write32(d.port_base, PX_CI, 0);
                write32(d.port_base, PX_TFD, 0x50);
                assert!(recover(&d, taken));
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
    for stops in [true, false] {
        let regs = FakeRegs(std::boxed::Box::into_raw(std::boxed::Box::new([0u32; 96])));
        let abar = regs.0 as u64;
        if !stops {
            write32(abar + PORT_BASE, PX_CMD, CMD_CR);
        }
        let freed = crate::mm::heap::frames_freed_by_this_thread();
        let claimed = claim_port(abar, 0).map(|_| ());
        assert_eq!(claimed, Err("IDENTIFY failed"), "stops {stops}: nothing answers on fake registers");
        let gave = crate::mm::heap::frames_freed_by_this_thread() - freed;
        assert_eq!(gave, if stops { 4 } else { 0 }, "stops {stops}: its three port pages and IDENTIFY's");
    }
}
