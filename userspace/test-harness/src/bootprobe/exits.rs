//! A process's exit as its peers see it: what each thread held is let go, and none outlives it.

use super::*;
use core::sync::atomic::{AtomicBool, Ordering};
use libkern::KError;
use libos::{MapReadWrite, Memory, Spawn, thread};

/// `test-stage`'s exit children, by `arg0`.
const EXIT_WAITING: u64 = 0x4558_0001;
const EXIT_RUNNING: u64 = 0x4558_0002;
const EXIT_ALONE: u64 = 0x4558_0003;
const EXIT_SPAWNING: u64 = 0x4558_0004;

/// Every right a handle has, for a grant that gives the child what the parent holds.
pub(super) const ALL: Rights = Rights::from_bits(u64::MAX);

/// A spawn of `test-stage` with `arg0`, ready for its grants.
pub(super) fn stage<'a>(root: Root<'a>, arg0: u64) -> Option<Spawn<'a>> {
    let mut spawn = block_on(Spawn::path(root, "/bin/test-stage")).ok()?;
    spawn.arg0(arg0);
    Some(spawn)
}

/// Spawn `test-stage` with `arg0`, handing it `moved` and, before that, a copy of `page` if given.
pub(super) fn spawn_stage(
    root: Root<'_>,
    arg0: u64,
    page: Option<&Handle<Memory, MapReadWrite>>,
    moved: Handle<Channel, Only>,
) -> bool {
    let Some(mut spawn) = stage(root, arg0) else { return false };
    if page.is_some_and(|page| spawn.grant_copy(page, ALL).is_err()) {
        return false;
    }
    if spawn.grant(moved, ALL).is_err() {
        return false;
    }
    spawn.spawn().is_ok()
}

pub(super) fn channel() -> Option<Pipe> {
    Handle::pair(4).ok()
}

/// Whether `ch`'s peer has gone by `deadline`. A message still on the way is taken and dropped.
pub(super) fn peer_gone_by<'a>(ch: impl Into<Ref<'a, Channel, Only>>, deadline: u64) -> bool {
    let ch = ch.into();
    let mut msg = Msg::new();
    loop {
        match ch.recv(&mut msg) {
            Err(e) if e.code() == KError::PeerClosed => return true,
            Ok(_) => continue,
            Err(_) => {}
        }
        if clock_ns() >= deadline {
            return false;
        }
        wait_for(ch, deadline);
    }
}

/// A page of memory, mapped here, for a child to count in.
fn shared_page() -> Option<(Handle<Memory, MapReadWrite>, Mapping<MapReadWrite>)> {
    let mem = Handle::<Memory, MapReadWrite>::create(PAGE as usize).ok()?;
    let page = mem.map(PAGE as usize).ok()?;
    Some((mem, page))
}

/// The count in `page` 20 ms after an exit began, then 50 ms later: `sys_process_exit` closes the
/// handles first, so the first read waits out what stops a sibling, an IPI or the next tick.
fn counts_settled(page: &Mapping<MapReadWrite>) -> (u64, u64) {
    let count = || {
        let mut bytes = [0u8; 8];
        let _ = page.read_into(0, &mut bytes);
        u64::from_le_bytes(bytes)
    };
    nap(20 * MS);
    let first = count();
    nap(50 * MS);
    (first, count())
}

fn release_page(mem: Handle<Memory, MapReadWrite>, page: Mapping<MapReadWrite>) {
    drop(page);
    drop(mem);
}

/// **A process that exits with a thread blocked in `sys_wait` lets go of what it waited on**: the
/// channel end the thread waits on must close.
pub(super) fn exit_with_a_waiter_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit with a waiter FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    if !spawn_stage(root, EXIT_WAITING, None, its) {
        return fail(b"no child");
    }
    let blocked = receive(&mine, clock_ns() + 5_000 * MS).is_some_and(|m| m.op == 1);
    let gone = blocked && peer_gone_by(&mine, clock_ns() + 2_000 * MS);
    drop(mine);
    if !blocked {
        return fail(b"the child's waiter never blocked");
    }
    if !gone {
        return fail(b"no PeerClosed in 2 s: the waiter's references outlived its process");
    }
    log::write(b"boot-probe: a process exiting with a thread in sys_wait lets go of what it waited on ok\n");
    true
}

/// **No thread outlives its process**: a count on CPU 2 must stop once a sibling exits from CPU 3.
pub(super) fn exit_with_a_running_sibling_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit with a running sibling FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Some((mem, page)) = shared_page() else {
        return fail(b"no page");
    };
    if !spawn_stage(root, EXIT_RUNNING, Some(&mem), its) {
        release_page(mem, page);
        return fail(b"no child");
    }
    let gone = peer_gone_by(&mine, clock_ns() + 5_000 * MS);
    let (ended, later) = counts_settled(&page);
    release_page(mem, page);
    drop(mine);
    if !gone {
        return fail(b"no PeerClosed in 5 s");
    }
    if ended == 0 {
        return fail(b"the process ended before its count began");
    }
    if later != ended {
        Line::new()
            .s(b"boot-probe: exit with a running sibling FAIL: a thread on another CPU counts on after its process ended, ")
            .u(ended)
            .s(b" then ")
            .u(later)
            .end();
        return false;
    }
    log::write(b"boot-probe: a process's exit stops a sibling running on another CPU ok\n");
    true
}

/// **An exit reaches the threads created while it runs**: one it missed counts on in a page once
/// the channel they all wait on is closed.
pub(super) fn exit_while_spawning_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit while spawning FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Some((mem, page)) = shared_page() else {
        return fail(b"no page");
    };
    if !spawn_stage(root, EXIT_SPAWNING, Some(&mem), its) {
        release_page(mem, page);
        return fail(b"no child");
    }
    let gone = peer_gone_by(&mine, clock_ns() + 5_000 * MS);
    let (ended, later) = counts_settled(&page);
    release_page(mem, page);
    drop(mine);
    if !gone {
        return fail(b"no PeerClosed in 5 s");
    }
    if later != ended {
        Line::new()
            .s(b"boot-probe: exit while spawning FAIL: a thread created during the exit counts on, ")
            .u(ended)
            .s(b" then ")
            .u(later)
            .end();
        return false;
    }
    log::write(b"boot-probe: a process's exit ends the threads created while it runs ok\n");
    true
}

/// Holds [`spin`] threads on their CPUs until cleared.
static LOADED: AtomicBool = AtomicBool::new(false);

extern "C" fn spin(_: u64, _: u64, _: u64, _: u64) -> ! {
    while LOADED.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
    thread::exit(0)
}

/// **A last thread's `sys_thread_exit` closes its process's handles at once, with every CPU busy**:
/// none idles while a spinner holds each, so they close only if the exit told the reaper.
pub(super) fn thread_exit_under_load_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: thread exit under load FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    LOADED.store(true, Ordering::Relaxed);
    let loaded = (0..4u8).all(|cpu| {
        thread::Stack::new(4 * PAGE as usize).and_then(|stack| thread::start_on(spin, stack, 0, 1 << cpu)).is_ok()
    });
    let spawned = loaded && spawn_stage(root, EXIT_ALONE, None, its);
    let gone = spawned && peer_gone_by(&mine, clock_ns() + 1_000 * MS);
    LOADED.store(false, Ordering::Relaxed);
    drop(mine);
    if !spawned {
        return fail(if loaded { b"no child" } else { b"no spinner" });
    }
    if !gone {
        return fail(b"no PeerClosed in 1 s with every CPU busy");
    }
    log::write(b"boot-probe: a last thread's sys_thread_exit closes its handles with every CPU busy ok\n");
    true
}

extern "C" fn quit(_: u64, _: u64, _: u64, _: u64) -> ! {
    thread::exit(0)
}

/// **What a spawn and a thread's start hand back is still the caller's** once the call returns,
/// as a handle the kernel closed again would not be: a child that exits by itself still takes a
/// terminate request, which only queues, and a thread not suspended has no fault registers to
/// give, which a closed handle answers `InvalidHandle` instead.
pub(super) fn started_handles_are_kept_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: started handles FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Some(mut spawn) = stage(root, EXIT_ALONE) else {
        return fail(b"no stage");
    };
    if spawn.grant(its, ALL).is_err() {
        return fail(b"no grant");
    }
    let Ok(child) = spawn.spawn() else {
        return fail(b"no child");
    };
    let asked = child.terminate();
    let gone = peer_gone_by(&mine, clock_ns() + 5_000 * MS);
    drop((child, mine));
    if asked.is_err() {
        return fail(b"a spawn's handle to its child was closed");
    }
    if !gone {
        return fail(b"the child never exited");
    }
    let Ok(stack) = thread::Stack::new(4 * PAGE as usize) else {
        return fail(b"no stack");
    };
    let Ok(started) = thread::start(quit, stack, 0) else {
        return fail(b"no thread");
    };
    if !started.fault_registers().is_err_and(|e| e.code() == KError::InvalidArgument) {
        return fail(b"a thread's handle was closed, or it answered as a suspended thread's");
    }
    log::write(b"boot-probe: a spawn's and a thread's handles are the caller's once they return ok\n");
    true
}
