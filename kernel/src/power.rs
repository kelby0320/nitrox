//! **Stopping the machine on purpose**: the power node's halt and reboot.
//!
//! The machine stops in three steps:
//!
//! 1. **Every disk is flushed**, so what the filesystems wrote is on the medium and not in a
//!    drive's cache: `IoOpcode::Flush` (AHCI's `FLUSH CACHE EXT`) sent to each disk at once, and
//!    waited for until each is done or [`FLUSH_BOUND_NS`] has passed. A partition is skipped,
//!    since its flush is its disk's. A disk that does not finish is named on the log and the stop
//!    goes on: a machine that cannot stop is worse than a drive that took too long.
//! 2. **Every other processor stops**, by the path a panic takes (`stop_other_cpus`).
//! 3. A **halt** says [`SAFE_TO_TURN_OFF`] on COM1 and as the screen's last line, and halts this
//!    processor too; the machine stays on, showing it, until its button is held. A **reboot**
//!    runs the platform's reset chain.
//!
//! **There is no power-off.** Entering S5 needs AML, which waits for ACPICA
//! (`docs/rationale/why-phased-acpi.md`).
//!
//! **Outside the async-first rule, deliberately.** A write to the power node acts within
//! `sys_io_submit`, before an operation is made for it ([`CharBackend::act`]), and never returns,
//! so there would be nothing to hand a `PendingOperation` to; the flush waits here, bounded.
//!
//! What stops the machine is `init`'s alone: the one power node is made at boot and handed to it
//! with [`INIT_RIGHTS`], which it can neither duplicate nor transfer, and bound in no namespace.
//! See `docs/architecture/power.md`.

use core::fmt::Write;

use crate::arch::cpu::ArchCpu;
use crate::arch::platform::ArchPlatform;
use crate::arch::timer::ArchTimer;
use crate::libkern::block::BlockKind;
use crate::libkern::handle::Rights;
use crate::libkern::power::PowerOp;
use crate::libkern::{ArrayVec, KVec};
use crate::object::device_node::CharBackend;
use crate::object::header::Ref;
use crate::object::{DeviceNode, MAX_WAIT_HANDLES, MemoryObject, ObjectRef, PendingOperation};
use crate::syscall::error::KError;

/// How long the flush may take, every disk together. A drive's own limit is longer (30 seconds for
/// ATA), but that is for a cache full of writes; by the time `init` asks, every filesystem is
/// written back and marked clean.
const FLUSH_BOUND_NS: u64 = 10_000_000_000;

/// The line a halt ends on.
pub const SAFE_TO_TURN_OFF: &str = "It is now safe to turn off your computer.";

/// The line a reboot shows while the platform resets.
const RESTARTING: &str = "Restarting.";

/// The rights `init`'s handle to the power node carries: `WRITE`, to stop the machine, and
/// `INSPECT`; neither `DUPLICATE` nor `TRANSFER`, so it can give the handle to no one.
pub const INIT_RIGHTS: Rights = Rights::WRITE.union(Rights::INSPECT);

/// The power node's backend: no read, and a write that acts at once.
pub const BACKEND: CharBackend =
    CharBackend { submit_read: no_read, submit_write: None, ctx: core::ptr::null_mut(), act: Some(write) };

fn no_read(_: &Ref<MemoryObject>, _: &Ref<PendingOperation>, _: u64, _: u64, _: u64, _: *mut ()) -> Result<(), KError> {
    Err(KError::Unsupported)
}

/// **A write to the power node**: one byte, `POWER_HALT` or `POWER_REBOOT`, flushes, stops and
/// halts or resets the machine. Any other length or byte is refused `InvalidArgument`, the only
/// way this returns.
fn write(buffer: &MemoryObject, buf_offset: u64, len: u64) -> KError {
    let Some(op) = op_in(buffer, buf_offset, len) else { return KError::InvalidArgument };
    let (pid, what) = (crate::sched::current_owner_pid(), if op == PowerOp::Halt { "halt" } else { "reboot" });
    crate::kprintln!("power: pid {pid} asks to {what}");
    power(op)
}

/// The stop a write of `len` bytes from `buffer` at `buf_offset` names: one byte naming one.
fn op_in(buffer: &MemoryObject, buf_offset: u64, len: u64) -> Option<PowerOp> {
    let mut byte = [0u8; 1];
    (len == 1 && buffer.copy_out(buf_offset as usize, &mut byte) == 1).then_some(())?;
    PowerOp::from_u64(u64::from(byte[0]))
}

/// Flush every disk, stop every processor, then halt or reset. Never returns.
pub fn power(op: PowerOp) -> ! {
    flush_every_disk();
    if op == PowerOp::Reboot {
        crate::arch::Platform::prepare_reset();
    }
    let to = if op == PowerOp::Halt { "halt" } else { "reset" };
    crate::kprintln!("power: stopping every processor, to {to}");
    crate::arch::Cpu::stop_other_cpus();
    // From here, nothing takes a lock or allocates: a processor stopped above may hold either.
    match op {
        PowerOp::Halt => {
            last_words(SAFE_TO_TURN_OFF);
            crate::arch::Cpu::halt_loop()
        }
        PowerOp::Reboot => {
            last_words(RESTARTING);
            // SAFETY: ring 0, after `prepare_reset`, with interrupts masked and every other
            // processor stopped — `stop_other_cpus` did both.
            unsafe { crate::arch::Platform::reset() }
        }
    }
}

/// Say `line` on COM1 and as the screen's last line. The serial port is written without its
/// lock, which a stopped processor may hold; the screen is taken back once, as a panic takes it.
fn last_words(line: &str) {
    let mut w = crate::arch::serial::emergency_writer();
    let _ = writeln!(w, "{line}");
    crate::fbcon::reclaim_for_stop_with(line.as_bytes());
}

/// Whether a block device of `kind` is flushed: all but a partition, whose flush is its disk's.
fn flushes(kind: u32) -> bool {
    kind != BlockKind::Partition as u32
}

/// Flush every disk, waiting until each is done or [`FLUSH_BOUND_NS`] has passed, logging each.
fn flush_every_disk() {
    // `/dev/blk/<n>` and its flush's operation.
    let mut flushing: KVec<(usize, Ref<PendingOperation>)> = KVec::new();
    let mut next = 0usize;
    while let Some((index, device)) = crate::device::find_block_device_from(next) {
        next = index + 1;
        if flushes(device.block_info().kind) {
            match start_flush(&device) {
                Ok(po) => {
                    if flushing.try_push((index, po)).is_err() {
                        crate::kprintln!("power: /dev/blk/{index} not flushed: out of memory");
                    }
                }
                Err(status) => {
                    crate::kprintln!("power: /dev/blk/{index} not flushed (status {status})")
                }
            }
        }
    }

    let deadline = crate::arch::Timer::read_ns().saturating_add(FLUSH_BOUND_NS);
    loop {
        let mut waiting = ArrayVec::<&ObjectRef, MAX_WAIT_HANDLES>::new();
        for (_, po) in flushing.iter() {
            if !crate::sched::pending_op_is_signaled(po) {
                let _ = waiting.try_push(po.as_object());
            }
        }
        let now = crate::arch::Timer::read_ns();
        if waiting.is_empty() || now >= deadline {
            break;
        }
        // Out of memory to wait with is a flush that does not finish, as far as the log is concerned.
        let waited = crate::sched::wait_on_io(&waiting, deadline, now);
        if let crate::sched::WaitResult::OutOfMemory = waited {
            break;
        }
    }

    let mut flushed = 0usize;
    for (index, po) in flushing.iter() {
        if !crate::sched::pending_op_is_signaled(po) {
            let bound = FLUSH_BOUND_NS / 1_000_000_000;
            crate::kprintln!("power: /dev/blk/{index} did not finish flushing in {bound} s");
            continue;
        }
        match crate::sched::pending_op_completion(po).0 {
            0 => flushed += 1,
            status => crate::kprintln!("power: /dev/blk/{index} failed to flush (status {status})"),
        }
    }
    crate::kprintln!("power: flushed {flushed} of {} disks", flushing.len());
}

/// Send one disk a flush, returning its operation, or the status it was refused with.
fn start_flush(device: &Ref<DeviceNode>) -> Result<Ref<PendingOperation>, i32> {
    let po = PendingOperation::try_new().map_err(|_| crate::syscall::KError::OutOfMemory as i32)?;
    let po = Ref::new(po);
    crate::io::block::dispatch_block_flush(device, &po).map_err(|e| e as i32)?;
    Ok(po)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every block device but a partition is flushed**, including one whose driver said nothing
    /// about what it is.
    #[test]
    fn every_block_device_but_a_partition_is_flushed() {
        assert!(flushes(BlockKind::Disk as u32));
        assert!(flushes(BlockKind::RamDisk as u32));
        assert!(flushes(BlockKind::Unknown as u32));
        assert!(!flushes(BlockKind::Partition as u32));
    }

    #[test]
    fn a_write_names_a_stop_only_by_one_byte_that_is_one() {
        crate::mm::test_support::init_global_heap();
        let page = MemoryObject::try_new(4096).unwrap();
        page.copy_in(0, &[0, 1, 2, 0xFF]);
        assert_eq!(op_in(&page, 0, 1), Some(PowerOp::Halt));
        assert_eq!(op_in(&page, 1, 1), Some(PowerOp::Reboot));
        assert_eq!(op_in(&page, 2, 1), None, "the byte past the two");
        assert_eq!(op_in(&page, 3, 1), None);
        assert_eq!(op_in(&page, 0, 2), None, "two bytes");
        assert_eq!(write(&page, 2, 1), KError::InvalidArgument, "refused, and the machine goes on");
        assert_eq!(write(&page, 0, 2), KError::InvalidArgument);
    }
}
