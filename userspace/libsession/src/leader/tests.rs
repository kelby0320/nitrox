use super::*;
use crate::testing::{endpoint, lookup_only, stop_flag};
use crate::{drain_for_stop, drain_stop_requests, stop_asked};
use libkern::KIND_CHILD_EXITED;
use libos::{MapReadWrite, Memory, NsMutable, NsReadOnly, mock};
use std::string::String;

type Ns = Handle<Namespace, NsMutable>;

fn note(kind: u32, code: i32) -> Notification {
    let mut body = [0u8; 60];
    body[8..12].copy_from_slice(&code.to_le_bytes());
    Notification { kind, body }
}

/// A supervisor's root with `/bin/nxsh` in it, a session namespace, and a queue holding `queued`.
struct World {
    root: Ns,
    session: Ns,
    notif: Handle<Notify, Only>,
    _image: Handle<Memory, MapReadWrite>,
}

fn world(queued: &[Notification]) -> World {
    mock::reset();
    let root = Ns::create().unwrap();
    let image = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
    root.bind("/bin/nxsh", &image).unwrap();
    // SAFETY: a queue the mock has just made, which nothing else holds.
    let notif = unsafe { Handle::from_raw(RawHandle(mock::notify(queued)), Rights::empty()) };
    World { root, session: Ns::create().unwrap(), notif, _image: image }
}

fn lead(w: &World, program: &str, syscaps: SysCaps, extras: &[Option<Ref<'_, Channel, Only>>]) -> i32 {
    run_leader(lookup_only(&w.root), &w.session, &w.notif, program, &["/home/alice"], syscaps, extras)
}

/// **An extra that cannot be passed on leaves an empty message in its place**, as a missing one
/// does, so each later extra keeps its slot: a duplicate refused, or a send refused, which leaves
/// the duplicate to close.
#[test]
fn an_extra_that_cannot_go_leaves_an_empty_message_in_its_place() {
    mock::reset();
    let (setup, leader) = Handle::<Channel, Only>::pair(4).unwrap();
    let (extra, _peer) = Handle::<Channel, Only>::pair(4).unwrap();
    // SAFETY: a handle the mock has just made for the test, which nothing else holds.
    let fixed: Handle<Channel, Only> =
        unsafe { Handle::from_raw(RawHandle(mock::with_rights(extra.raw().0, Rights::TRANSFER)), Rights::empty()) };
    let open = mock::open_handles();
    send_extra(setup.as_ref(), Some(fixed.as_ref()));
    mock::fail_next_send(libkern::KError::OutOfMemory.as_i32());
    send_extra(setup.as_ref(), Some(extra.as_ref()));
    for _ in 0..2 {
        assert!(leader.recv(&mut Msg::new()).unwrap().is_empty(), "an empty message for each");
    }
    assert!(leader.recv(&mut Msg::new()).is_err(), "and nothing more");
    assert_eq!((mock::open_handles(), mock::stray_closes()), (open, 0), "the duplicate that did not go, closed");
    assert_eq!(
        mock::logged(),
        b"libsession: could not duplicate the leader's extra handle\n\
          libsession: an endpoint the leader expects is absent (placeholder sent)\n\
          libsession: the leader's extra handle was not delivered\n"
    );
}

/// The leader is spawned into the session namespace with the setup channel as its one moved
/// grant, the caps it was given and the bootstrap descriptor that says a setup is pending.
#[test]
fn the_leader_is_spawned_into_the_session_with_its_setup_channel() {
    let w = world(&[note(KIND_CHILD_EXITED, 0), note(KIND_CHILD_EXITED, 0)]);
    let before = mock::open_handles();
    lead(&w, "nxsh", SysCaps::BIND_NAMESPACE, &[]);
    let args = mock::spawned().pop().unwrap();
    assert_eq!((args.handle_count, args.move_mask), (1, 1));
    assert_eq!((args.arg0, args.namespace), (bootstrap_arg0(true), w.session.raw().0));
    assert_eq!((args.syscaps, args.rights[0]), (SysCaps::BIND_NAMESPACE.bits(), u64::MAX));
    assert!(!mock::is_open(args.handles[0]), "moved to the leader");
    assert_eq!(mock::open_handles(), before, "the image, the channel's end and the process are closed");
    assert_eq!(mock::stray_closes(), 0);
    lead(&w, "nxsh", SysCaps::empty(), &[]);
    assert_eq!(mock::spawned()[1].syscaps, 0, "the serial leader holds none");
}

/// **A set-up leader reads its setup, then one message an extra, in order**, a missing one empty
/// and each other a duplicate of the caller's, which it keeps; the leader's exit code is returned.
#[test]
fn a_set_up_leader_gets_its_arguments_then_each_extra_in_order() {
    let w = world(&[note(KIND_CHILD_EXITED, 5)]);
    let (one, _one_peer) = Handle::<Channel, Only>::pair(4).unwrap();
    let (two, _two_peer) = Handle::<Channel, Only>::pair(4).unwrap();
    assert_eq!(lead(&w, "nxsh", SysCaps::empty(), &[Some(one.as_ref()), None, Some(two.as_ref())]), 5);
    // SAFETY: a handle the mock has just made for the test, on the leader's end.
    let leader = unsafe { Handle::<Channel, Only>::from_raw(RawHandle(mock::granted(0, 0)), Rights::empty()) };
    let setup = libstream::setup::receive_setup(&leader, Deadline::NEVER).unwrap().unwrap();
    assert_eq!((setup.argv, setup.env), (["nxsh", "/home/alice"].map(String::from).to_vec(), session_env()));
    for source in [Some(&one), None, Some(&two)] {
        let mut carried = leader.recv(&mut Msg::new()).unwrap();
        let Some(source) = source else {
            assert!(carried.is_empty(), "a placeholder for the missing one");
            continue;
        };
        let extra: Handle<Channel, Only> = carried.take(0).unwrap();
        assert!(mock::same_object(extra.raw().0, source.raw().0), "the caller's endpoint, duplicated");
    }
    assert!(leader.recv(&mut Msg::new()).is_err(), "nothing after the extras");
    assert!(mock::is_open(one.raw().0) && mock::is_open(two.raw().0), "the caller's, kept for later sessions");
    assert_eq!(mock::stray_closes(), 0);
}

/// A leader whose setup message cannot be sent is asked to stop and waited for, and its exit is
/// `-1` whatever its code.
#[test]
fn a_leader_that_cannot_be_set_up_is_stopped_and_its_exit_is_failure() {
    let w = world(&[note(KIND_CHILD_EXITED, 7)]);
    mock::exit_next_child();
    assert_eq!(lead(&w, "nxsh", SysCaps::empty(), &[None]), -1);
    assert_eq!(mock::terminated(), 1);
    assert!(mock::logged().starts_with(b"libsession: setup message FAIL; asking the leader to stop\n"));
}

/// Past the time it has to stop, a leader is given up on rather than waited for.
#[test]
fn a_leader_that_will_not_stop_is_given_up_on() {
    let _flag = stop_flag();
    let w = world(&[note(KIND_TERMINATE_REQUESTED, 0)]);
    let before = mock::open_handles();
    assert_eq!(lead(&w, "nxsh", SysCaps::empty(), &[]), -1);
    assert_eq!(mock::terminated(), 1);
    assert!(mock::logged().ends_with(b"libsession: nxsh is still running after it was asked to stop\n"));
    assert_eq!(mock::open_handles(), before, "its process is let go");
}

/// A program the root has no image of is not spawned, and nothing is left open.
#[test]
fn a_missing_image_is_not_spawned() {
    let w = world(&[]);
    let before = mock::open_handles();
    assert_eq!(lead(&w, "absent", SysCaps::empty(), &[]), -1);
    assert!(mock::spawned().is_empty());
    assert!(mock::logged().ends_with(b"libsession: absent image not found\n"));
    assert_eq!(mock::open_handles(), before);
}

/// A spawn the kernel refuses leaves neither end of the setup channel open: a login is retried.
#[test]
fn a_refused_spawn_closes_both_ends_of_the_setup_channel() {
    let w = world(&[]);
    let not_a_namespace = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
    // SAFETY: the borrow ends with this test, which holds the memory open.
    let session = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(not_a_namespace.raw(), Rights::empty()) };
    let before = mock::open_handles();
    assert_eq!(run_leader(lookup_only(&w.root), session, &w.notif, "nxsh", &[], SysCaps::empty(), &[]), -1);
    assert!(mock::spawned().is_empty());
    assert!(mock::logged().ends_with(b"libsession: nxsh spawn FAIL\n"));
    assert_eq!(mock::open_handles(), before);
    assert_eq!(mock::stray_closes(), 0);
}

/// The raw form is the same leader on the caller's own handles, which it leaves open.
#[test]
fn the_raw_form_spawns_the_same_leader_and_closes_nothing() {
    let w = world(&[note(KIND_CHILD_EXITED, 4)]);
    let (extra, _peer) = endpoint(50);
    let code = spawn_leader(
        w.root.raw().0,
        w.session.raw().0,
        w.notif.raw().0,
        "nxsh",
        &[],
        SysCaps::BIND_NAMESPACE.bits(),
        &[extra.raw().0, 0],
    );
    assert_eq!(code, 4);
    let args = mock::spawned().pop().unwrap();
    assert_eq!((args.namespace, args.syscaps), (w.session.raw().0, SysCaps::BIND_NAMESPACE.bits()));
    for h in [w.root.raw().0, w.session.raw().0, w.notif.raw().0, extra.raw().0] {
        assert!(mock::is_open(h));
    }
    assert_eq!(mock::stray_closes(), 0);
}

/// A terminate request that reaches the leader's wait is passed on to it once, and remembered, so
/// the supervisor exits rather than prompting again.
#[test]
fn a_stop_asked_while_a_leader_runs_is_passed_on_and_remembered() {
    let _flag = stop_flag();
    let stop = note(KIND_TERMINATE_REQUESTED, 0);
    let w = world(&[stop, stop, note(KIND_CHILD_EXITED, 3)]);
    assert!(!stop_asked());
    assert_eq!(lead(&w, "nxsh", SysCaps::empty(), &[]), 3, "its own exit, as it was set up");
    assert!(stop_asked());
    assert_eq!(mock::terminated(), 1, "asked once");
    assert!(mock::logged().ends_with(b"libsession: asked to stop; passing it on to nxsh\n"));
}

/// A drain between sessions says whether a stop was asked, and remembers it.
#[test]
fn a_drain_between_sessions_finds_a_stop() {
    let _flag = stop_flag();
    mock::reset();
    // SAFETY: queues the mock has just made, which nothing else holds.
    let queue =
        |q: &[Notification]| unsafe { Handle::<Notify, Only>::from_raw(RawHandle(mock::notify(q)), Rights::empty()) };
    assert!(!drain_stop_requests(&queue(&[])));
    assert!(!drain_stop_requests(&queue(&[note(KIND_CHILD_EXITED, 0)])));
    assert!(!stop_asked());
    let stop = queue(&[note(KIND_CHILD_EXITED, 0), note(KIND_TERMINATE_REQUESTED, 0), note(KIND_CHILD_EXITED, 0)]);
    assert!(drain_stop_requests(&stop));
    assert!(stop_asked());
    assert!(stop.recv().unwrap().is_none(), "drained to the end");
}

/// The raw drain reads the caller's queue and leaves it open.
#[test]
fn the_raw_drain_leaves_the_queue_open() {
    let _flag = stop_flag();
    mock::reset();
    let raw = mock::notify(&[note(KIND_TERMINATE_REQUESTED, 0)]);
    assert!(drain_for_stop(raw));
    assert!(mock::is_open(raw));
}
