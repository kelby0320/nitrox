//! **Stopping the machine on purpose** (administration Part E.3): `sys_power`'s halt and reboot.
//!
//! The machine stops in three steps:
//!
//! 1. **Every disk is flushed**, so that what the filesystems wrote is on the medium and not in a
//!    drive's cache: `IoOpcode::Flush` — AHCI's `FLUSH CACHE EXT` — sent to each disk at once,
//!    and waited for until each is done or [`FLUSH_BOUND_NS`] has passed. A partition is skipped,
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
//! **Outside the async-first rule, deliberately.** A blocking operation hands back a
//! `PendingOperation`, but this never returns, so there would be nothing to hand it to; the flush
//! waits here, bounded.
//!
//! What stops the machine is `init`'s alone: the one
//! [`SystemControl`](crate::object::SystemControl) is made at boot and handed to it, and cannot be
//! duplicated or transferred. See `docs/architecture/power.md`.

use core::fmt::Write;

use crate::arch::cpu::ArchCpu;
use crate::arch::platform::ArchPlatform;
use crate::arch::timer::ArchTimer;
use crate::libkern::block::BlockKind;
use crate::libkern::handle::KObjectType;
use crate::libkern::power::PowerOp;
use crate::libkern::{KBox, KVec};
use crate::object::{DeviceNode, MAX_WAIT_HANDLES, ObjectRef, PendingOperation};

/// How long the flush may take, every disk together.
///
/// A drive's own limit for a cache flush is longer, 30 seconds for ATA, but that is for a cache
/// full of writes; by the time `init` asks, every filesystem has been written back and marked
/// clean, so a flush here moves little.
const FLUSH_BOUND_NS: u64 = 10_000_000_000;

/// The line a halt ends on.
pub const SAFE_TO_TURN_OFF: &str = "It is now safe to turn off your computer.";

/// The line a reboot shows while the platform resets.
const RESTARTING: &str = "Restarting.";

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

/// Whether a block device of `kind` is flushed: everything but a partition, whose flush is its
/// disk's.
fn flushes(kind: u32) -> bool {
    kind != BlockKind::Partition as u32
}

/// Flush every disk, waiting until each is done or [`FLUSH_BOUND_NS`] has passed, and log what
/// each did.
fn flush_every_disk() {
    // `/dev/blk/<n>` and its flush's operation.
    let mut flushing: KVec<(usize, ObjectRef)> = KVec::new();
    let mut index = 0usize;
    while let Some(device) = crate::device::find_block_device(index) {
        // SAFETY: the device table holds `DeviceNode`s, and `device` pins this one.
        let node = unsafe { &*(device.as_ptr() as *const DeviceNode) };
        if flushes(node.block_info().kind) {
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
        index += 1;
    }

    let deadline = crate::arch::Timer::read_ns().saturating_add(FLUSH_BOUND_NS);
    loop {
        let mut waiting = [0usize; MAX_WAIT_HANDLES];
        let mut n = 0;
        for (_, po) in flushing.iter() {
            if n < MAX_WAIT_HANDLES && !crate::sched::pending_op_is_signaled(po.as_ptr()) {
                waiting[n] = po.as_ptr() as usize;
                n += 1;
            }
        }
        let now = crate::arch::Timer::read_ns();
        if n == 0 || now >= deadline {
            break;
        }
        // `flushing` pins every operation waited on. Out of memory to wait with is a flush that
        // does not finish, as far as the log is concerned.
        let waited = crate::sched::wait_on(&waiting[..n], deadline, now);
        if let crate::sched::WaitResult::OutOfMemory = waited {
            break;
        }
    }

    let mut flushed = 0usize;
    for (index, po) in flushing.iter() {
        if !crate::sched::pending_op_is_signaled(po.as_ptr()) {
            let bound = FLUSH_BOUND_NS / 1_000_000_000;
            crate::kprintln!("power: /dev/blk/{index} did not finish flushing in {bound} s");
            continue;
        }
        match crate::sched::pending_op_completion(po.as_ptr()).0 {
            0 => flushed += 1,
            status => crate::kprintln!("power: /dev/blk/{index} failed to flush (status {status})"),
        }
    }
    crate::kprintln!("power: flushed {flushed} of {} disks", flushing.len());
}

/// Send one disk a flush, returning its operation, or the status it was refused with.
fn start_flush(device: &ObjectRef) -> Result<ObjectRef, i32> {
    let po = PendingOperation::try_new().map_err(|_| crate::syscall::KError::OutOfMemory as i32)?;
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    let po = unsafe {
        ObjectRef::from_raw(KBox::into_raw(po).as_ptr() as *mut (), KObjectType::PendingOperation)
    };
    crate::io::block::dispatch_block_flush(device, &po).map_err(|e| e as i32)?;
    Ok(po)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Every block device but a partition is flushed** — a partition's flush goes to its disk,
    /// which is flushed already — including one whose driver said nothing about what it is.
    #[test]
    fn every_block_device_but_a_partition_is_flushed() {
        assert!(flushes(BlockKind::Disk as u32));
        assert!(flushes(BlockKind::RamDisk as u32));
        assert!(flushes(BlockKind::Unknown as u32));
        assert!(!flushes(BlockKind::Partition as u32));
    }
}
