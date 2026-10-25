//! A process's exit as its peers see it: what each thread held is let go, and none outlives it.

use super::*;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use libkern::abi::SpawnArgs;
use libkern::{KError, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_PROCESS_SPAWN};

/// `test-stage`'s exit children, by `arg0`.
const EXIT_WAITING: u64 = 0x4558_0001;
const EXIT_RUNNING: u64 = 0x4558_0002;
const EXIT_ALONE: u64 = 0x4558_0003;
const EXIT_SPAWNING: u64 = 0x4558_0004;

const MS: u64 = 1_000_000;

/// Spawn `test-stage` with `arg0`, handing it `handles`, moved where `move_mask` has a bit.
fn spawn_stage(root_ns: u64, arg0: u64, handles: &[u64], move_mask: u32) -> bool {
    let (st, image) = ns_lookup(root_ns, b"/bin/test-stage", RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        return false;
    }
    let mut args = SpawnArgs {
        image,
        handle_count: handles.len() as u32,
        move_mask,
        arg0,
        handles: [0; 4],
        rights: [u64::MAX; 4],
        namespace: 0,
        syscaps: 0,
    };
    args.handles[..handles.len()].copy_from_slice(handles);
    // SAFETY: a valid argument block naming handles this process holds.
    let p = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const args) as u64) };
    close(image);
    close(p.max(0) as u64);
    p > 0
}

fn channel() -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let made = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 4, 0) } == 0;
    made.then_some((a, b))
}

/// Whether `ch`'s peer has gone by `deadline`. A message still on the way is taken and dropped.
fn peer_gone_by(ch: u64, deadline: u64) -> bool {
    let mut buf = [0u8; 4096];
    let mut hs = [0u64; 8];
    loop {
        let mut count = 0usize;
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(SYS_CHANNEL_RECV, ch, buf.as_mut_ptr() as u64, hs.as_mut_ptr() as u64, (&raw mut count) as u64)
        };
        if rr == KError::PeerClosed.as_i32() as i64 {
            return true;
        }
        if rr == 0 {
            hs[..count.min(8)].iter().for_each(|&h| close(h));
            continue;
        }
        if clock_ns() >= deadline {
            return false;
        }
        let handles = [ch];
        let mut results = [0u8; 24];
        // SAFETY: one handle this process holds, and room for one result.
        unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, deadline) };
    }
}

/// A page of memory, mapped read-only here, for a child to count in.
fn shared_page() -> Option<(u64, u64)> {
    // SAFETY: register-only syscalls; the second maps the object the first made.
    let (mem, page) = unsafe {
        let mem = syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0);
        (mem, if mem > 0 { syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ) } else { -1 })
    };
    if page < 0 {
        close(mem.max(0) as u64);
        return None;
    }
    Some((mem as u64, page as u64))
}

/// The count in `page` 20 ms after an exit began, then 50 ms later. `sys_process_exit` closes the
/// handles before it reaches the siblings, so the first read waits out the IPI that stops them.
fn counts_settled(page: u64) -> (u64, u64) {
    // SAFETY: `page` stays mapped until its owner unmaps it, after this.
    let count = || unsafe { &*(page as *const AtomicU64) }.load(Ordering::Relaxed);
    nap(20 * MS);
    let first = count();
    nap(50 * MS);
    (first, count())
}

fn release_page(mem: u64, page: u64) {
    // SAFETY: unmapping a page `shared_page` mapped; nothing reads it after.
    unsafe { syscall2(SYS_MEMORY_UNMAP, page, 0) };
    close(mem);
}

/// **A process that exits with a thread blocked in `sys_wait` lets go of what it waited on**: the
/// channel end the thread waits on must close.
pub(super) fn exit_with_a_waiter_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit with a waiter FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    if !spawn_stage(root_ns, EXIT_WAITING, &[its], 1) {
        close(mine);
        close(its);
        return fail(b"no child");
    }
    let blocked = receive(mine, clock_ns() + 5_000 * MS).is_some_and(|m| m.op == 1);
    let gone = blocked && peer_gone_by(mine, clock_ns() + 2_000 * MS);
    close(mine);
    if !blocked {
        return fail(b"the child's waiter never blocked");
    }
    if !gone {
        return fail(b"no PeerClosed in 2 s: the waiter's references outlived its process");
    }
    kprint(b"boot-probe: a process exiting with a thread in sys_wait lets go of what it waited on ok\n");
    true
}

/// **No thread outlives its process**: a count on CPU 2 must stop once a sibling exits from CPU 3.
pub(super) fn exit_with_a_running_sibling_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit with a running sibling FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Some((mem, page)) = shared_page() else {
        close(mine);
        close(its);
        return fail(b"no page");
    };
    if !spawn_stage(root_ns, EXIT_RUNNING, &[mem, its], 0b10) {
        close(mine);
        close(its);
        release_page(mem, page);
        return fail(b"no child");
    }
    let gone = peer_gone_by(mine, clock_ns() + 5_000 * MS);
    let (ended, later) = counts_settled(page);
    release_page(mem, page);
    close(mine);
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
    kprint(b"boot-probe: a process's exit stops a sibling running on another CPU ok\n");
    true
}

/// **An exit reaches the threads created while it runs**: one it missed counts on in a page once
/// the channel they all wait on is closed.
pub(super) fn exit_while_spawning_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: exit while spawning FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    let Some((mem, page)) = shared_page() else {
        close(mine);
        close(its);
        return fail(b"no page");
    };
    if !spawn_stage(root_ns, EXIT_SPAWNING, &[mem, its], 0b10) {
        close(mine);
        close(its);
        release_page(mem, page);
        return fail(b"no child");
    }
    let gone = peer_gone_by(mine, clock_ns() + 5_000 * MS);
    let (ended, later) = counts_settled(page);
    release_page(mem, page);
    close(mine);
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
    kprint(b"boot-probe: a process's exit ends the threads created while it runs ok\n");
    true
}

/// Holds [`spin`] threads on their CPUs until cleared.
static LOADED: AtomicBool = AtomicBool::new(false);

extern "C" fn spin() -> ! {
    while LOADED.load(Ordering::Relaxed) {
        core::hint::spin_loop();
    }
    // SAFETY: `sys_thread_exit` ends this thread.
    unsafe { syscall1(libkern::SYS_THREAD_EXIT, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Start a [`spin`] thread on CPU `cpu`, on a stack never unmapped; this process lasts the boot.
fn start_spinner(cpu: u8) -> bool {
    const STACK: u64 = 4 * PAGE;
    // SAFETY: register-only syscalls; the second maps the object the first made.
    let base = unsafe {
        let mem = syscall4(SYS_MEMORY_CREATE, STACK, 0, 0, 0);
        if mem < 0 {
            return false;
        }
        syscall4(SYS_MEMORY_MAP, mem as u64, 0, STACK, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    let args = libkern::ThreadArgs {
        entry: spin as *const () as u64,
        user_sp: base as u64 + STACK,
        arg0: 0,
        class: 0,
        rt_priority: 0,
        nice: 0,
        cpu_affinity: 1 << cpu,
        _reserved: [0; 36],
    };
    base >= 0 && libos::thread_create(&args).is_ok()
}

/// **A last thread's `sys_thread_exit` closes its process's handles at once, with every CPU busy**:
/// none idles while a spinner holds each, so they close only if the exit told the reaper.
pub(super) fn thread_exit_under_load_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: thread exit under load FAIL: ").s(why).end();
        false
    };
    let Some((mine, its)) = channel() else {
        return fail(b"no channel");
    };
    LOADED.store(true, Ordering::Relaxed);
    let loaded = (0..4u8).all(start_spinner);
    let spawned = loaded && spawn_stage(root_ns, EXIT_ALONE, &[its], 1);
    let gone = spawned && peer_gone_by(mine, clock_ns() + 1_000 * MS);
    LOADED.store(false, Ordering::Relaxed);
    close(mine);
    if !spawned {
        close(its);
        return fail(if loaded { b"no child" } else { b"no spinner" });
    }
    if !gone {
        return fail(b"no PeerClosed in 1 s with every CPU busy");
    }
    kprint(b"boot-probe: a last thread's sys_thread_exit closes its handles with every CPU busy ok\n");
    true
}
