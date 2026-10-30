//! The hub thread's side of a storage device: its own commands, recovery, a rescan, a departure.

use super::*;

/// **Rescan a USB disk's table**, for the `Rescan` IRP device `i` holds, from the hub thread with
/// the device's memory:
/// 1. **the disk's partitions' windows retired first**, so nothing more comes through them, and
///    waited out until no submit is between a window's check and its forward ([`window_busy`]);
/// 2. **the device left to drain** — what those windows forwarded before, queued or in flight,
///    reaches the disk at the old table's offsets now, never after the new one is published — a
///    fault recovered on the way; then taken, every later submit queued;
/// 3. the partitions' records departed, the table read again as at the disk's arrival and
///    published, and the IRP completed with how many partitions it found — or `IoError` if the
///    table would not read.
///
/// **`Ended` for a device to end as on an unplug**: one whose recovery failed meanwhile, which
/// nothing will make quiet. Its departure completes the rescan.
///
/// [`window_busy`]: crate::io::block::window_busy
pub(in crate::drivers::xhci) fn rescan(x: &Xhci, i: usize, port: u8, mem: &mut DeviceMem) -> Recovered {
    use crate::io::block::{retire_window, window_busy};
    let start = crate::arch::Timer::read_ns();
    let late = || crate::arch::Timer::read_ns().wrapping_sub(start) > DEADLINE_NS + BIND_NS;
    let Some(unit) = DISKS.rescan_unit(i) else { return Recovered::Done };
    let gone = unit.record.map(crate::device::children).unwrap_or_default();
    let mut windows = KVec::new();
    for &child in gone.iter() {
        if let Some(node) = crate::device::node(child)
            && retire_window(&node)
        {
            // The window is retired whether or not it is kept to be waited out.
            let _ = windows.try_push(node);
        }
    }
    // **Giving up** — a device that never went quiet — leaves the retired windows' records
    // departed, since they refuse everything: the disk lists no partition until a rescan succeeds.
    let give_up = |why: &str| {
        if let Some(irp) = DISKS.drop_rescan(i) {
            crate::kprintln!("usb: port {port}: its disk {why}; not rescanned");
            complete(irp, KError::IoError as i32, 0);
        }
        for &child in gone.iter() {
            crate::device::depart(child);
        }
        Recovered::Done
    };
    while windows.iter().any(window_busy) {
        if late() {
            return give_up("kept a window busy");
        }
        hub::sleep(1_000_000);
    }
    let (irp, lun, unit) = loop {
        match DISKS.begin_rescan(i) {
            BeginRescan::Nothing => return Recovered::Done,
            BeginRescan::Ended => return Recovered::Ended,
            BeginRescan::Ready { irp, lun, unit } => break (irp, lun, unit),
            BeginRescan::Busy => {
                // A fault the DPC recorded is recovered first; a device that does not recover is
                // ended by the caller, and its departure completes the rescan. An ejected unit's first.
                drain_stopped(i);
                if let Recovered::Ended = recover(x, i, mem) {
                    return Recovered::Ended;
                }
                if late() {
                    return give_up("never went quiet");
                }
                hub::sleep(1_000_000);
            }
        }
    };
    let found = match unit.record.and_then(crate::device::node) {
        Some(disk) => {
            for &child in gone.iter() {
                crate::device::depart(child);
            }
            use crate::drivers::partitions::Unread;
            let table = read_table(x, i, port, lun, unit, mem);
            let found = match table {
                Some(Err(Unread::Io | Unread::NoMemory)) => None,
                Some(t) => Some(crate::drivers::gpt::publish(&disk, t, crate::drivers::gpt::Names::None)),
                None => Some(0),
            };
            match found {
                Some(n) => crate::kprintln!(
                    "usb: port {port}: LUN {lun}: rescanned: {} partition(s) departed, {n} published",
                    gone.len()
                ),
                None => crate::kprintln!(
                    "usb: port {port}: LUN {lun}: rescanned: {} partition(s) departed, and its table would not read",
                    gone.len()
                ),
            }
            Some(found)
        }
        None => None,
    };
    act(DISKS.end_rescan(i, crate::arch::Timer::read_ns()), Some(x));
    match found {
        Some(Some(n)) => complete(irp, 0, n as u64),
        Some(None) => complete(irp, KError::IoError as i32, 0),
        None => complete(irp, KError::PeerClosed as i32, 0),
    }
    Recovered::Done
}

/// **One of the hub thread's own commands** on device `i`: `cdb` for unit `lun`, its data read into
/// `out` — none when `out` is empty — waited for within [`BIND_NS`]. What its status wrapper said, or
/// why there is none. **A stall is recovered here**, as bulk-only §6.7 says: a data stage's halt
/// cleared and the status read; anything else, a timeout included, by reset recovery, which also
/// skips whatever the command left on the rings.
pub(super) fn run(
    x: &Xhci,
    i: usize,
    lun: u8,
    cdb: &[u8],
    out: &mut [u8],
    mem: &mut DeviceMem,
) -> Result<Status, Failed> {
    let len = out.len().min(BIND_DATA) as u32;
    let result = match run_once(i, lun, cdb, len, false) {
        Err(e) => Err(e),
        Ok(HubResult::Status(s)) => Ok(s),
        Ok(HubResult::Fault(Fault::Transfer { dci, code: code::STALL }))
            if len > 0 && dci == dev_field(i, |d| d.in_dci) =>
        {
            clear_halt(x, i, dci, mem)?;
            match run_once(i, lun, cdb, 0, true) {
                Ok(HubResult::Status(s)) => Ok(s),
                Ok(HubResult::Fault(_)) => Err(Failed::Device("the status after a stall did not read")),
                Err(e) => Err(e),
            }
        }
        Ok(HubResult::Fault(Fault::Transfer { code, .. })) => Err(Failed::Code(code)),
        Ok(HubResult::Fault(_)) => Err(Failed::Device("a command went wrong")),
    };
    match result {
        Ok(Status::Phase | Status::Invalid) | Err(_) => {
            reset_recovery(x, i, mem)?;
        }
        Ok(_) => {}
    }
    if len > 0 && matches!(result, Ok(Status::Passed { .. })) {
        let d = DISKS.devs[i].lock();
        if let Some(dev) = d.as_ref() {
            for (k, b) in out[..len as usize].iter_mut().enumerate() {
                // SAFETY: the binding buffer holds BIND_DATA bytes, which the controller finished
                // writing with the command just waited for.
                *b = unsafe { core::ptr::read_volatile((dev.data_virt as *const u8).add(k)) };
            }
        }
    }
    result
}

fn dev_field<T: Default>(i: usize, f: impl FnOnce(&Dev) -> T) -> T {
    DISKS.devs[i].lock().as_ref().map(f).unwrap_or_default()
}

/// **Put one hub command on device `i`'s rings and wait for it**, within [`BIND_NS`]: its data, `len`
/// bytes, into the binding buffer. `status_only` reads a status wrapper alone, after a data stage's
/// stall was cleared, for the command last sent.
fn run_once(i: usize, lun: u8, cdb: &[u8], len: u32, status_only: bool) -> Result<HubResult, Failed> {
    let po = hub::new_operation()?;
    let now = crate::arch::Timer::read_ns();
    if !DISKS.issue_hub(i, po.as_ptr(), lun, cdb, len, status_only, now) {
        return Err(Failed::Device("its device has gone"));
    }
    let at = crate::arch::Timer::read_ns();
    let signalled = matches!(
        // SAFETY: `po` pins the `PendingOperation` waited on.
        unsafe { crate::sched::wait_on(&[po.as_ptr() as usize], at + BIND_NS, at) },
        crate::sched::WaitResult::Signaled(_)
    );
    if !signalled {
        match DISKS.take_back(i) {
            TakeBack::Taken => return Err(Failed::Timeout),
            TakeBack::Gone => return Err(Failed::Device("its device has gone")),
            // **The DPC took it first**, and completes `po` only once it has let the device's lock
            // go: so `po` is not dropped until then, as `hub::wait` waits.
            TakeBack::OnItsWay => {
                use crate::sched::{WaitResult, wait_on};
                // SAFETY: `po` pins the `PendingOperation` waited on, and outlives the closure.
                let done =
                    || matches!(unsafe { wait_on(&[po.as_ptr() as usize], u64::MAX, 0) }, WaitResult::Signaled(_));
                while !done() {
                    hub::sleep(1_000_000);
                }
            }
        }
    }
    let mut d = DISKS.devs[i].lock();
    let Some(dev) = d.as_mut() else { return Err(Failed::Device("its device has gone")) };
    dev.hub_result.take().ok_or(Failed::Timeout)
}

/// **Clear endpoint `dci`'s halt** on device `i`: Reset Endpoint — or Stop Endpoint, for one still
/// running — then its dequeue pointer past everything queued, then `CLEAR_FEATURE(ENDPOINT_HALT)` to
/// the device, which resets its side's data toggle.
fn clear_halt(x: &Xhci, i: usize, dci: u8, mem: &mut DeviceMem) -> Result<(), Failed> {
    let (slot, next, address) = {
        let d = DISKS.devs[i].lock();
        let Some(dev) = d.as_ref() else { return Err(Failed::Device("its device has gone")) };
        let input = dci == dev.in_dci;
        let ring = if input { &dev.bulk_in } else { &dev.bulk_out };
        (dev.slot, ring.next(), if input { dev.in_address } else { dev.out_address })
    };
    match hub::command(x, Trb::endpoint_command(kind::RESET_ENDPOINT, slot, dci)) {
        Ok(_) => {}
        Err(Failed::Code(code::CONTEXT_STATE_ERROR)) => {
            match hub::command(x, Trb::endpoint_command(kind::STOP_ENDPOINT, slot, dci)) {
                Ok(_) | Err(Failed::Code(code::CONTEXT_STATE_ERROR)) => {}
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    }
    hub::command(x, Trb::set_dequeue(next.0, next.1, slot, dci))?;
    hub::control_out(x, slot, mem, [0x02, 0x01, 0, 0, address, 0, 0, 0])
}

/// **Reset recovery** (bulk-only §5.3.4): the class's reset request, then both endpoints' halts
/// cleared.
fn reset_recovery(x: &Xhci, i: usize, mem: &mut DeviceMem) -> Result<(), Failed> {
    let (slot, interface, in_dci, out_dci) = {
        let d = DISKS.devs[i].lock();
        let Some(dev) = d.as_ref() else { return Err(Failed::Device("its device has gone")) };
        (dev.slot, dev.interface, dev.in_dci, dev.out_dci)
    };
    hub::control_out(x, slot, mem, [0x21, 0xFF, 0, 0, interface, 0, 0, 0])?;
    clear_halt(x, i, in_dci, mem)?;
    clear_halt(x, i, out_dci, mem)
}

pub(super) fn retire(i: usize) {
    let _ = DISKS.depart(i);
}

/// **How long the hub thread may sleep** at `now` ([`Disks::bound`]).
pub(in crate::drivers::xhci) fn bound(now: u64) -> Option<u64> {
    DISKS.bound(now)
}

/// **The devices the hub thread must look at** ([`Disks::due`]).
pub(in crate::drivers::xhci) fn due(now: u64) -> [Option<u8>; MAX_DEVICES] {
    DISKS.due(now)
}

/// **Answer what waited on device `i`'s ejected units**, from the hub thread.
pub(in crate::drivers::xhci) fn drain_stopped(i: usize) {
    while let Some(a) = DISKS.take_stopped(i) {
        act(a, None);
    }
}

/// What [`recover`] did.
pub(in crate::drivers::xhci) enum Recovered {
    /// The command was finished or failed, and the device goes on.
    Done,
    /// The device did not recover: it is to be ended, as on an unplug.
    Ended,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum End {
    /// The IRP completes with this status, and the next queued starts.
    Complete(i32),
    /// A unit attention: the command again, once.
    Retry,
    /// **Reset recovery failed**, so the bulk endpoints may never have stopped and the command's
    /// TRBs may still move the IRP's data. It is held for the departure, which completes it once
    /// the slot is disabled, as it completes a command in flight.
    Stranded,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum TakeBack {
    /// Taken back before the DPC reached it: nothing will complete its operation.
    Taken,
    /// The DPC took it first: its completion of the operation is on its way.
    OnItsWay,
    /// The device has gone.
    Gone,
}

impl Disks {
    /// **How long the hub thread may sleep at `now`**: until the earliest deadline of an IRP's
    /// command in flight, and **never longer than a deadline's length while any disk is bound**, so
    /// a command started while it sleeps is seen by its deadline with nothing waking the thread:
    /// one wake a deadline's length on an idle machine with a stick in, rather than one a command.
    /// `None` with no disk bound.
    pub(super) fn bound(&self, now: u64) -> Option<u64> {
        let mut bound = None;
        for s in &self.devs {
            let d = s.lock();
            let Some(dev) = d.as_ref() else { continue };
            let mut until = bound.unwrap_or(now.saturating_add(DEADLINE_NS));
            if let Some(c) = dev.inflight
                && matches!(c.owner, Owner::Irp { .. })
            {
                until = until.min(c.deadline);
            }
            bound = Some(until);
        }
        bound
    }

    /// **The devices the hub thread must look at**: by table slot, the xHCI slot of each with a
    /// fault the DPC recorded, an IRP's command past its deadline at `now`, which is marked here,
    /// or a rescan waiting.
    pub(super) fn due(&self, now: u64) -> [Option<u8>; MAX_DEVICES] {
        core::array::from_fn(|i| {
            let mut d = self.devs[i].lock();
            let dev = d.as_mut()?;
            if dev.fault.is_none()
                && let Some(c) = dev.inflight
                && matches!(c.owner, Owner::Irp { .. })
                && now > c.deadline
            {
                dev.fault = Some(Fault::Deadline);
            }
            let stopped = dev.queue.ring.iter().flatten().any(|c| dev.ejected(c.lun));
            (dev.fault.is_some() || dev.rescan.is_some() || stopped).then_some(dev.slot)
        })
    }

    /// **Begin recovering device `i`**: its fault and its IRP's command taken, so the hub thread's own
    /// commands can use the rings, and every submit queued until [`Disks::end_recovery`]. `None` when
    /// there is nothing to recover: no fault, or the command it was for has completed.
    pub(super) fn begin_recovery(&self, i: usize) -> Option<(Fault, IrpCmd, bool)> {
        let mut d = self.devs[i].lock();
        let dev = d.as_mut()?;
        match (dev.fault.take(), dev.inflight.take()) {
            (Some(f), Some(Command { owner: Owner::Irp { cmd, retried }, .. })) => {
                dev.recovering = true;
                Some((f, cmd, retried))
            }
            (_, inflight) => {
                dev.inflight = inflight;
                None
            }
        }
    }

    /// **End device `i`'s recovery of `cmd`** as `end` says, in the one lock scope that lets
    /// submits start again: so none can start between the recovery's end and a retry, and have its
    /// command replaced by the retry's. A stranded command keeps the device recovering, so nothing
    /// starts on a device about to be ended. What to do after, with no lock held.
    pub(super) fn end_recovery(&self, i: usize, cmd: IrpCmd, end: End, now: u64) -> Action {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else {
            return Action::Complete { irp: cmd.irp, status: KError::PeerClosed as i32, transferred: 0 };
        };
        match end {
            End::Stranded => {
                dev.stranded = Some(cmd.irp);
                Action::None
            }
            End::Retry => {
                dev.recovering = false;
                dev.start(cmd, true, now);
                Action::None
            }
            End::Complete(status) => {
                dev.recovering = false;
                if dev.inflight.is_none()
                    && dev.fault.is_none()
                    && let Some(next) = dev.next()
                {
                    dev.start(next, false, now);
                }
                let transferred = if status == 0 { cmd.len as u64 } else { 0 };
                // Counted, with none of the recovery's time: that was the hub thread's commands.
                dev.counted[cmd.lun as usize % MAX_LUNS].record(cmd.op, status == 0, transferred, 0);
                Action::Complete { irp: cmd.irp, status, transferred }
            }
        }
    }

    /// **Put one of the hub thread's commands on device `i`'s rings**, its operation at `po`: `cdb`
    /// for unit `lun`, its data, `len` bytes, into the binding buffer — or, `status_only`, a status
    /// wrapper alone, for the command last sent. Whether the device is still there.
    #[allow(clippy::too_many_arguments, reason = "a command's device, operation, unit, CDB, length and stage")]
    pub(super) fn issue_hub(
        &self,
        i: usize,
        po: *mut (),
        lun: u8,
        cdb: &[u8],
        len: u32,
        status_only: bool,
        now: u64,
    ) -> bool {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return false };
        dev.hub_result = None;
        let owner = Owner::Hub { po };
        if status_only {
            let tag = dev.tag;
            dev.inflight =
                Some(Command { owner, tag, stage: Stage::Data { dci: 0 }, deadline: now + BIND_NS, started: now });
            dev.ask_status(false);
        } else {
            let frags = [PhysFrag { base: dev.data_phys, len: len as u64 }];
            let frags = if len == 0 { &frags[..0] } else { &frags[..] };
            dev.issue(owner, lun, cdb, len, len > 0, frags, now);
        }
        true
    }

    /// **The unit device `i`'s waiting rescan names**, for the hub thread to retire its partitions'
    /// windows by before it waits for the device to drain; `None` with no rescan waiting.
    pub(super) fn rescan_unit(&self, i: usize) -> Option<Lun> {
        let d = self.devs[i].lock();
        let dev = d.as_ref()?;
        let (_, lun) = dev.rescan?;
        dev.luns.get(lun as usize).copied()
    }

    /// **Begin device `i`'s rescan**: once nothing is in flight, queued or recovering, its `Rescan`
    /// taken and the device held — every submit queued — until [`Disks::end_rescan`]. **`Ended` for
    /// a device a failed recovery stranded**, which nothing will make quiet: the hub thread ends
    /// it, and the departure completes the rescan.
    pub(super) fn begin_rescan(&self, i: usize) -> BeginRescan {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return BeginRescan::Nothing };
        if dev.rescan.is_none() {
            return BeginRescan::Nothing;
        }
        if dev.stranded.is_some() {
            return BeginRescan::Ended;
        }
        if dev.inflight.is_some() || dev.fault.is_some() || dev.recovering || dev.queue.len > 0 {
            return BeginRescan::Busy;
        }
        let Some((irp, lun)) = dev.rescan.take() else { return BeginRescan::Nothing };
        dev.recovering = true;
        BeginRescan::Ready { irp, lun, unit: dev.luns.get(lun as usize).copied().unwrap_or_default() }
    }

    /// **End device `i`'s rescan**: submits start again, and the first queued behind it now.
    pub(super) fn end_rescan(&self, i: usize, now: u64) -> Action {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return Action::None };
        dev.recovering = false;
        if dev.inflight.is_none()
            && dev.fault.is_none()
            && let Some(next) = dev.next()
        {
            dev.start(next, false, now);
        }
        Action::None
    }

    /// **One request that waited on an ejected unit of device `i`**, answered as one sent after the
    /// eject is ([`Disks::submit`]): each queued command, then a rescan held for the unit.
    pub(super) fn take_stopped(&self, i: usize) -> Option<Action> {
        let mut d = self.devs[i].lock();
        let dev = d.as_mut()?;
        let luns = dev.luns;
        let (irp, op, lun) = match dev.queue.pop_where(|c| luns.get(c.lun as usize).is_some_and(|u| u.ejected)) {
            Some(c) => (c.irp, c.op, c.lun),
            None => {
                let (irp, lun) =
                    dev.rescan.take_if(|&mut (_, lun)| luns.get(lun as usize).is_some_and(|u| u.ejected))?;
                (irp, IrpOp::Rescan as u32, lun)
            }
        };
        if op == IrpOp::Flush as u32 {
            return Some(Action::Complete { irp, status: 0, transferred: 0 });
        }
        let said = dev.luns.get_mut(lun as usize).is_some_and(|u| !core::mem::replace(&mut u.refusal_said, true));
        Some(Action::Ejected { irp, said: said.then_some((dev.slot, lun)) })
    }

    /// **Give up device `i`'s waiting rescan**: its IRP, for the caller to complete.
    fn drop_rescan(&self, i: usize) -> Option<*mut Irp> {
        self.devs[i].lock().as_mut().and_then(|dev| dev.rescan.take()).map(|(irp, _)| irp)
    }

    /// **A hub command's wait ran out**: taken back if the DPC has not reached it, so nothing
    /// completes an operation the hub thread is about to drop.
    pub(super) fn take_back(&self, i: usize) -> TakeBack {
        let mut d = self.devs[i].lock();
        let Some(dev) = d.as_mut() else { return TakeBack::Gone };
        if matches!(dev.inflight, Some(Command { owner: Owner::Hub { .. }, .. })) {
            dev.inflight = None;
            return TakeBack::Taken;
        }
        TakeBack::OnItsWay
    }
}

/// **What a failed command's sense key comes to**, or `None` to send it again: an illegal request
/// to flush is no cache to flush, and one to eject a stick that cannot.
pub(super) fn sense_outcome(key: u8, op: u32, retried: bool) -> Option<i32> {
    match key {
        scsi::ILLEGAL_REQUEST if op == IrpOp::Flush as u32 => Some(0),
        scsi::ILLEGAL_REQUEST if op == IrpOp::Eject as u32 => Some(KError::Unsupported as i32),
        scsi::UNIT_ATTENTION if !retried => None,
        _ => Some(KError::IoError as i32),
    }
}

/// **Recover device `i`**, whose command went wrong (bulk-only §6.7): a failed command's sense
/// read and judged by [`sense_outcome`] — a unit attention sends the command
/// again once — a data stage's stall cleared and its status read, and reset recovery for anything
/// else, a deadline included. The command is completed, and the next started. From the hub thread,
/// with the device's memory.
pub(in crate::drivers::xhci) fn recover(x: &Xhci, i: usize, mem: &mut DeviceMem) -> Recovered {
    let Some((fault, cmd, retried)) = DISKS.begin_recovery(i) else {
        return Recovered::Done;
    };
    let mut sense = [0u8; scsi::SENSE_LEN];
    let outcome = match fault {
        Fault::Status(Status::Failed) => match run(x, i, cmd.lun, &scsi::request_sense(), &mut sense, mem) {
            Ok(Status::Passed { .. }) => Ok(sense_outcome(scsi::sense_answer(&sense).0, cmd.op, retried)),
            _ => Err(()),
        },
        Fault::Transfer { dci, code: code::STALL }
            if dci == dev_field(i, |d| if cmd.input { d.in_dci } else { d.out_dci }) =>
        {
            match clear_halt(x, i, dci, mem).and_then(|_| run_once(i, cmd.lun, &[], 0, true)) {
                Ok(_) => Ok(Some(KError::IoError as i32)),
                Err(_) => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
            }
        }
        _ => reset_recovery(x, i, mem).map(|_| Some(KError::IoError as i32)).map_err(|_| ()),
    };
    let end = match outcome {
        Ok(Some(status)) => End::Complete(status),
        Ok(None) => End::Retry,
        Err(()) => End::Stranded,
    };
    act(DISKS.end_recovery(i, cmd, end, crate::arch::Timer::read_ns()), Some(x));
    if end == End::Stranded { Recovered::Ended } else { Recovered::Done }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum BeginRescan {
    /// No rescan is waiting, or the device has gone.
    Nothing,
    /// One is waiting, behind a command in flight, one queued, or a recovery.
    Busy,
    /// One is waiting on a device a failed recovery stranded, which is to be ended.
    Ended,
    /// It is the hub thread's, for unit `lun`, and the device is held.
    Ready { irp: *mut Irp, lun: u8, unit: Lun },
}

/// What a departure leaves for after the slot is disabled: the command that was in flight, one a
/// failed recovery stranded, and a rescan waiting.
pub(in crate::drivers::xhci) struct Departed(pub(super) [Option<*mut Irp>; 3]);

/// **The device in xHCI slot `slot` has gone**: out of the table, so the DPC touches none of its
/// memory; its queue completed `PeerClosed` now, and any later submit refused so by its slot's
/// epoch. The command in flight is handed back, for [`finish`] once the slot is disabled: until
/// then the controller may still move its data. From the hub thread.
pub(in crate::drivers::xhci) fn depart(slot: u8) -> Departed {
    let Some(i) = DISKS.slot_of(slot) else {
        return Departed([None; 3]);
    };
    let (queued, held) = DISKS.depart(i);
    for irp in queued.into_iter().flatten() {
        complete(irp, KError::PeerClosed as i32, 0);
    }
    Departed(held)
}

/// **Complete what a departure handed back**, `PeerClosed`, by what became of the slot: all of it
/// once disabled; if it was kept, its commands are kept with its memory, never completed, as the
/// controller may still move their data, and only the rescan, which moves none, completes.
pub(in crate::drivers::xhci) fn finish(d: Departed, slot: &super::hub::Released) {
    let [inflight, stranded, rescan] = d.0;
    let disabled = matches!(slot, super::hub::Released::Freed);
    for irp in [inflight, stranded].into_iter().flatten().filter(|_| disabled).chain(rescan) {
        complete(irp, KError::PeerClosed as i32, 0);
    }
}
