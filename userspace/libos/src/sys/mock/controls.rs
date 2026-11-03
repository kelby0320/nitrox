//! The test controls: what a test sets the model up with, and reads back from it.

use super::*;

/// Start the model again, empty.
pub fn reset() {
    MOCK.with(|m| *m.borrow_mut() = fresh());
}

/// Arm the next `io_submit` with a completion. `ready_now` = the PO is signaled
/// immediately (a poll finds it); otherwise it completes on the next wait with a deadline.
pub fn script_next(status: i32, result: u64, ready_now: bool) {
    with(|m| m.script.push_back((status, result, ready_now, false)));
}

/// Arm the next `io_submit` with a PO that never signals.
pub fn script_never() {
    with(|m| m.script.push_back((0, 0, false, true)));
}

/// Fail the next wait with a deadline with `status`.
pub fn fail_next_wait(status: i64) {
    with(|m| m.fail_wait = Some(status));
}

/// Force the next `io_submit` to fail with `status`.
pub fn fail_next_submit(status: i32) {
    with(|m| m.fail_submit = Some(status));
}

/// Refuse the next `ns_bind` past its namespace with `status`, as a missing `BIND_NAMESPACE` is.
pub fn fail_next_bind(status: i32) {
    with(|m| m.fail_bind = Some(status));
}

/// Fail the next `file_sync` or `ns_sync` with `status`.
pub fn fail_next_sync(status: i32) {
    with(|m| m.fail_sync = Some(status));
}

/// Refuse the next `thread_create` with `status`, as a `RealTime` one without `REAL_TIME` is.
pub fn fail_next_thread(status: i32) {
    with(|m| m.fail_thread = Some(status));
}

/// Refuse the next `channel_recv` with `status`, its message left queued, as `OutOfMemory` is.
pub fn fail_next_recv(status: i32) {
    with(|m| m.fail_recv = Some(status));
}

/// Answer the next successful rename with a handle, as a server answering it so would.
pub fn rename_with_handle() {
    with(|m| m.rename_handle = true);
}

/// Every `IoOp` submitted, in order.
pub fn submitted() -> Vec<IoOp> {
    with(|m| m.submitted.clone())
}

/// Answer `ns_sync` of `path` with `written` and `ns_held` with `held`.
pub fn mount(path: &str, written: i64, held: i64) {
    with(|m| m.mounts.insert(path.as_bytes().to_vec(), (written, held)));
}

/// Let the next `reads` entropy reads find the pool unseeded.
pub fn unseeded(reads: u32) {
    with(|m| m.unseeded = reads);
}

/// Set the wall clock, which reads `Unsupported` until then.
pub fn set_realtime(ns: u64) {
    with(|m| m.realtime = Some(ns));
}

/// A handle to a device that `io_submit` takes, with `READ | WRITE`.
pub fn device() -> u64 {
    with(|m| m.mint(Object::Device, Rights::READ | Rights::WRITE) as u64)
}

/// A namespace whose server at `path` answers each lookup at once with one end of a channel, and
/// the other end, for the test to read what is sent.
pub fn answering(path: &str) -> (u64, u64) {
    let mut ends = [0; 2];
    channel_create(4, &mut ends);
    with(|m| {
        let (ours, _) = m.table[&ends[0]];
        m.table.remove(&ends[0]);
        let server = m.object(Object::Answers(ours));
        let ns = m.object(Object::Namespace(vec![(path.as_bytes().to_vec(), server, u64::MAX)]));
        (m.install(ns, Rights::LOOKUP.bits()), ends[1])
    })
}

/// A server's forwarding endpoint, on which the kernel takes a reply within the send.
pub fn forwarding() -> u64 {
    with(|m| {
        let at = m.objects.len();
        let ring = VecDeque::new();
        m.object(Object::Channel { peer: at, ring, held: VecDeque::new(), depth: 1, forward: true });
        m.install(at, (Rights::SEND | Rights::RECV | Rights::WAIT).bits())
    })
}

/// A namespace with a server bound at `path`, whose lookups complete when time passes.
pub fn served(path: &str) -> u64 {
    with(|m| {
        let server = m.object(Object::Server);
        let ns = m.object(Object::Namespace(vec![(path.as_bytes().to_vec(), server, u64::MAX)]));
        m.install(ns, Rights::LOOKUP.bits())
    })
}

/// A handle to a notification queue holding `queued`.
pub fn notify(queued: &[Notification]) -> u64 {
    with(|m| m.mint(Object::Notify(queued.iter().copied().collect()), Rights::WAIT) as u64)
}

/// A handle to `h`'s object with exactly `rights`, as one a process was given would be.
pub fn with_rights(h: u64, rights: Rights) -> u64 {
    with(|m| {
        let obj = m.table[&h].0;
        m.install(obj, rights.bits())
    })
}

/// Whether `h` names something in the table.
pub fn is_open(h: u64) -> bool {
    with(|m| m.table.contains_key(&h))
}

/// How many handles are open.
pub fn open_handles() -> usize {
    with(|m| m.table.len())
}

/// How many mappings are in place.
pub fn mappings() -> usize {
    with(|m| m.maps.values().map(|(_, n)| n).sum())
}

/// What was logged.
pub fn logged() -> Vec<u8> {
    with(|m| m.log.clone())
}

/// Every spawn's arguments, in order.
pub fn spawned() -> Vec<SpawnArgs> {
    with(|m| m.spawned.clone())
}

/// Every thread's arguments, in order.
pub fn threads() -> Vec<ThreadArgs> {
    with(|m| m.threads.clone())
}

/// Waits that slept until their deadline.
pub fn sleeps() -> u32 {
    with(|m| m.sleeps)
}

/// How many POs were closed before they signalled.
pub fn closed_in_flight() -> u32 {
    with(|m| m.closed_in_flight)
}

/// How many processes were asked to exit.
pub fn terminated() -> u32 {
    with(|m| m.terminated)
}

/// How many closes named a handle that was not there.
pub fn stray_closes() -> u32 {
    with(|m| m.stray_closes)
}

/// How many handles the last `wait` was handed.
pub fn last_wait_count() -> usize {
    with(|m| m.last_wait_count)
}

/// `(submits, waits, closes)` issued since the last `reset`.
pub fn counts() -> (u32, u32, u32) {
    with(|m| (m.submits, m.waits, m.closes))
}
