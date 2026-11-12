//! The queue's order, and the requests a device refuses.

use super::*;

/// **A command taken from the queue leaves the rest in their order**, whichever slot the ring's head
/// is at and whichever command is taken: a write queued behind another to the same block must not
/// overtake it.
#[test]
fn a_command_taken_from_the_queue_leaves_the_rest_in_their_order() {
    let mut irps: std::vec::Vec<Irp> = (0..4).map(|_| irp(IrpOp::Flush, 0, 0, &[])).collect();
    let at: std::vec::Vec<*mut Irp> = irps.iter_mut().map(|r| r as *mut Irp).collect();
    let cmd = |k: usize, lun| IrpCmd { irp: was(at[k]), lun, cdb: Cdb::of(&[]), len: 0, input: false, op: 0 };
    for head in 0..QUEUE {
        for taken in 0..4 {
            let mut q = Queue::new();
            q.head = head;
            for k in 0..4 {
                assert!(q.push(cmd(k, (k == taken) as u8)).is_ok());
            }
            assert!(q.pop_where(|c| c.lun == 1).is_some_and(|c| c.irp.is(at[taken])));
            let rest: std::vec::Vec<*mut Irp> = core::iter::from_fn(|| q.pop()).map(|c| c.irp.into_raw()).collect();
            let want: std::vec::Vec<*mut Irp> = (0..4).filter(|&k| k != taken).map(|k| at[k]).collect();
            assert_eq!(rest, want, "head {head}, taking {taken}");
        }
    }
}

/// **A submit past a full queue is refused `IoError`**, its request handed back to complete.
#[test]
fn a_submit_past_a_full_queue_is_refused() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let ctx = context(i, 0, epoch);
    let mut irps: std::vec::Vec<Irp> = (0..QUEUE + 2).map(|_| irp(IrpOp::Flush, 0, 0, &[])).collect();
    let (last, held) = irps.split_last_mut().unwrap();
    for r in held {
        assert_eq!(disks.submit(fly(r), ctx, 0).lax(), Action::None, "started, then queued");
    }
    let refused = Action::Complete { irp: was(last), status: KError::IoError as i32, transferred: 0 };
    assert_eq!(disks.submit(fly(last), ctx, 0).lax(), refused);
}

/// **A submit the device cannot take is refused with its error**: a slot past the table, and a
/// command its unit refuses.
#[test]
fn a_submit_the_device_cannot_take_is_refused_with_its_error() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let frags = [PhysFrag { base: 0x1000, len: 512 }];
    let (mut past, mut unaligned) = (irp(IrpOp::Read, 0, 512, &frags), irp(IrpOp::Read, 100, 512, &frags));
    let invalid = |irp| Action::Complete { irp: was(irp), status: KError::InvalidArgument as i32, transferred: 0 };
    let no_slot = context(MAX_DEVICES, 0, 0);
    assert_eq!(disks.submit(fly(&mut past), no_slot, 0).lax(), invalid(&raw mut past), "no such slot");
    let off = disks.submit(fly(&mut unaligned), context(i, 0, epoch), 0).lax();
    assert_eq!(off, invalid(&raw mut unaligned), "off a block");
}

/// **A departure completes its queue `PeerClosed` at once**, and hands back the command in flight,
/// which the controller may still move.
#[test]
fn a_departure_completes_its_queue_at_once() {
    let _exclusive = crate::dpc::exclusive();
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, epoch) = disks.take(fake.dev(5)).unwrap();
    let (mut a, mut b) = (irp(IrpOp::Flush, 0, 0, &[]), irp(IrpOp::Flush, 0, 0, &[]));
    for r in [&mut a as *mut Irp, &mut b] {
        assert_eq!(disks.submit(fly(r), context(i, 0, epoch), 0).lax(), Action::None);
    }
    let Departed(held, _) = departed(&disks, 5);
    crate::dpc::run_pending();
    assert_eq!(b.status, KError::PeerClosed as i32, "queued: completed");
    assert_eq!(held.lax(), [Some(was(&raw mut a)), None, None], "in flight: handed back");
    assert_eq!(a.status, crate::io::irp::IrpStatus::Pending as i32);
}

/// **A departure ends an armed hub command's wait**, completing it, so its waiter finds the device
/// gone rather than wait for a DPC that will not come.
#[test]
fn a_departure_ends_a_hub_commands_wait() {
    init_global_heap();
    let disks = Disks::new();
    let mut fake = Fake::new();
    let (i, _) = disks.take(held(fake.dev(5))).unwrap();
    let po = hub::new_operation().unwrap();
    let issue = |c| disks.issue_hub(i, c, 0, &scsi::test_unit_ready(), 0, false, 0);
    // Its wait, the hub thread's, needs a scheduler.
    core::mem::forget(armed::arm(&po, HubCommand { disks: &disks, i }, issue).unwrap());
    let _ = departed(&disks, 5);
    assert!(crate::sched::pending_op_is_signaled(&po), "its waiter ended");
    assert_eq!(disks.take_back(i), None, "nothing left to take back");
}
