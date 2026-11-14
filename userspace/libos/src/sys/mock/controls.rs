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

/// Complete the next file's or mount's flush with `status`.
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

/// Refuse the next `channel_send` with `status`, taking nothing, as `OutOfMemory` is.
pub fn fail_next_send(status: i32) {
    with(|m| m.fail_send = Some(status));
}

/// Complete the next send's operation at once with `status`, as a kernel answering it so would.
pub fn answer_next_send(status: i32) {
    with(|m| m.answer_send = Some(status));
}

/// Refuse the next `memory_create` with `status`.
pub fn fail_next_create(status: i32) {
    with(|m| m.fail_create = Some(status));
}

/// Let the next truncate leave the file's size as it was, as a server ignoring it would.
pub fn ignore_next_truncate() {
    with(|m| m.ignore_truncate = true);
}

/// Answer the next successful rename with a handle, as a server answering it so would.
pub fn rename_with_handle() {
    with(|m| m.rename_handle = true);
}

/// Every `IoOp` submitted, in order.
pub fn submitted() -> Vec<IoOp> {
    with(|m| m.submitted.clone())
}

/// Answer a flush of the mount at `path` with `written`, and a held with `held`.
pub fn mount(path: &str, written: i64, held: i64) {
    with(|m| m.mounts.insert(path.as_bytes().to_vec(), (written, held)));
}

/// Refuse the next `n` flushes `WouldBlock`, as the kernel refuses one past its process's share.
pub fn busy_flushes(n: u32) {
    with(|m| m.busy_flushes = n);
}

/// Let every flush from now on start and never finish, as one on a device that stopped does.
pub fn flushes_never_finish() {
    with(|m| m.flush_never = true);
}

/// Let the next `reads` entropy reads find the pool unseeded.
pub fn unseeded(reads: u32) {
    with(|m| m.unseeded = reads);
}

/// Let the next `reads` unseeded reads find every place the kernel keeps for a reader waiting on
/// the seeding taken, as it answers past `SEED_WAITERS_MAX`: a PO done `WouldBlock`, to read again.
pub fn seed_waiters_full(reads: u32) {
    with(|m| m.seed_full = reads);
}

/// Set the wall clock, which reads `Unsupported` until then.
pub fn set_realtime(ns: u64) {
    with(|m| m.realtime = Some(ns));
}

/// Answer the next clock set in range with `r`, a refusal or `CLOCK_SET_THIS_BOOT`.
pub fn answer_next_clock_set(r: i64) {
    with(|m| m.clock_answer = Some(r));
}

/// A handle to a device that `io_submit` takes, with `READ | WRITE`.
pub fn device() -> u64 {
    with(|m| m.mint(Object::Device, Rights::READ | Rights::WRITE) as u64)
}

/// The system-control object, with `WRITE | INSPECT` and neither `DUPLICATE` nor `TRANSFER`, as
/// the kernel hands it to `init`.
pub fn system_control() -> u64 {
    with(|m| m.mint(Object::SystemControl, Rights::WRITE | Rights::INSPECT) as u64)
}

/// Each test verdict reported, in order.
pub fn verdicts() -> Vec<u64> {
    with(|m| m.verdicts.clone())
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
        let ns = m.object(Object::Namespace(vec![Binding::new(path.as_bytes(), server, u64::MAX)]));
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
        let ns = m.object(Object::Namespace(vec![Binding::new(path.as_bytes(), server, u64::MAX)]));
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

/// Whether `a` and `b` are handles on one object, which no call reports.
pub fn same_object(a: u64, b: u64) -> bool {
    with(|m| matches!((m.table.get(&a), m.table.get(&b)), (Some(x), Some(y)) if x.0 == y.0))
}

/// Make the next spawn's child exit before anything reaches it: it holds none of its grants.
pub fn exit_next_child() {
    with(|m| m.exit_next_child = true);
}

/// A handle on what the `n`th spawn granted its child at `i`, with the rights it was installed
/// with, for a test that plays the child.
pub fn granted(n: usize, i: usize) -> u64 {
    with(|m| {
        let (obj, rights) = m.children[n][i];
        m.install(obj, rights)
    })
}

/// The base `path` was bound in `ns` with, which no enumerate reports; `None` if nothing is bound
/// there.
pub fn bound_base(ns: u64, path: &str) -> Option<Vec<u8>> {
    with(|m| {
        let ns = m.namespace(ns, Rights::empty()).ok()?;
        m.bindings(ns).iter().find(|b| b.path == path.as_bytes()).map(|b| b.base.clone())
    })
}

/// Every thread's arguments, in order.
pub fn threads() -> Vec<ThreadArgs> {
    with(|m| m.threads.clone())
}

/// Waits that slept until their deadline.
pub fn sleeps() -> u32 {
    with(|m| m.sleeps)
}

/// `sys_ns_enumerate` calls since the reset.
pub fn enumerates() -> u32 {
    with(|m| m.enumerates)
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

/// Let every file read from now on start and never finish, as one on a device that stopped does.
pub fn reads_never_finish() {
    with(|m| m.reads_never = true);
}

/// Answer the next file read `by` bytes short, as one of a file truncated since its stat is.
pub fn next_read_short(by: u64) {
    with(|m| m.read_short = by);
}
