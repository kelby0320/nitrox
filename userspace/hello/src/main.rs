//! `hello` — a freestanding ring-3 program that exercises the syscall surface through libos and
//! reports each step on the kernel's log; the boot gate counts on the lines it prints.
//!
//! Built as a static, non-PIE `ET_EXEC` at a low user address; see `user.ld` and
//! `.cargo/config.toml`.

#![no_std]
#![no_main]

use libkern::{KError, KOBJ_MEMORY_OBJECT, RIGHT_MAP_READ, Rights};
use libos::log::write as kprint;
use libos::{Carried, Channel, Deadline, Handle, MapReadWrite, Memory, Msg, Only, Timer, WaitSet, time};

const PAGE: u64 = 4096;

/// The payload `hello` sends to itself over IPC.
const IPC_PING: &[u8] = b"ipc: ping\n";

const MSG: &[u8] = b"hello from ring 3 (pid 1)\n";

/// Whether `result` is `code`.
fn refused<T>(result: &libos::Result<T>, code: KError) -> bool {
    matches!(result, Err(e) if e.code() == code)
}

/// Entry point: runs each demo step, then faults on purpose.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kprint(MSG);

    let Ok(h) = Handle::<Memory, MapReadWrite>::create(PAGE as usize) else {
        kprint(b"memory: create FAIL\n");
        libos::exit(1);
    };

    let Ok(mut map) = h.map(PAGE as usize) else {
        kprint(b"memory: map FAIL\n");
        libos::exit(1);
    };

    // 3. Round-trip a byte through the mapped page.
    let mut got = [0u8];
    let stored = map.write_from(0, &[0xA5]).is_ok() && map.read_into(0, &mut got).is_ok();
    if stored && got[0] == 0xA5 {
        kprint(b"memory: roundtrip ok\n");
    } else {
        kprint(b"memory: roundtrip FAIL\n");
    }

    // 4. Exercise the handle-operation syscalls on the memory handle. Each
    //    step prints its own failure so a regression is easy to localise.
    let mut handle_ops_ok = true;

    match h.stat() {
        Ok(info) if info.object_type == KOBJ_MEMORY_OBJECT && (info.rights & RIGHT_MAP_READ) != 0 => {}
        _ => handle_ops_ok = false,
    }

    match h.duplicate(Rights::INSPECT | Rights::DUPLICATE) {
        Err(_) => handle_ops_ok = false,
        Ok(dup) => {
            // The duplicate closes as it drops, which libos does not report.
            match dup.without_duplicate() {
                Ok(restricted) if restricted.extra_rights().bits() & Rights::DUPLICATE.bits() == 0 => {}
                _ => handle_ops_ok = false,
            }
        }
    }

    if handle_ops_ok {
        kprint(b"handle-ops ok\n");
    } else {
        kprint(b"handle-ops FAIL\n");
    }

    // 4b. Read the monotonic clock twice with work in between; it must advance.
    let t1 = time::now();
    let mut spin = 0u64;
    for _ in 0..200_000 {
        spin = spin.wrapping_add(core::hint::black_box(1));
    }
    let _ = spin;
    let t2 = time::now();
    if matches!((t1, t2), (Ok(t1), Ok(t2)) if t2 > t1) {
        kprint(b"clock: monotonic advancing\n");
    } else {
        kprint(b"clock: monotonic FAIL\n");
    }

    let Ok(th) = Handle::<Timer, Only>::create() else {
        kprint(b"timer: create FAIL\n");
        libos::exit(1);
    };

    // A failed clock read leaves 0.
    let t0 = time::now().unwrap_or(0);
    let fire_at = t0 + 50_000_000; // +50 ms
    let sr = th.set(fire_at);
    let mut set = WaitSet::<1>::new();
    let _ = set.add(&th);
    // Generous 5 s overall deadline so the timer (not the deadline) wakes us.
    let waited = set.wait(Deadline::at(t0 + 5_000_000_000));
    let t_after = time::now().unwrap_or(0);
    let woke = matches!(&waited, Ok(s) if s.len() == 1 && s.contains(0));
    if sr.is_ok() && woke && t_after >= fire_at {
        kprint(b"timer: waited and woke ok\n");
    } else {
        kprint(b"timer: wait FAIL\n");
    }

    // 4d. Poll an unset timer — nothing ready.
    if let Ok(th2) = Handle::<Timer, Only>::create() {
        let mut set = WaitSet::<1>::new();
        let _ = set.add(&th2);
        if matches!(set.wait(Deadline::POLL), Ok(s) if s.is_empty()) {
            kprint(b"wait: poll empty as expected\n");
        } else {
            kprint(b"wait: poll FAIL\n");
        }

        // 4e. Pure timeout: arm far in the future, wait with a near deadline.
        let now = time::now().unwrap_or(0);
        let _ = th2.set(now + 10_000_000_000); // +10 s
        if matches!(set.wait(Deadline::at(now + 30_000_000)), Ok(s) if s.is_empty()) {
            kprint(b"wait: timed out as expected\n");
        } else {
            kprint(b"wait: timeout FAIL\n");
        }
    }
    drop(th);

    if map.unmap().is_ok() {
        kprint(b"memory: unmap ok\n");
    } else {
        kprint(b"memory: unmap FAIL\n");
    }

    // 5b. IPC round-trip. Create a channel, hold both endpoints, and send a
    //     message end0→end1, then receive it on end1 — a full ring-3
    //     create/send/wait/recv/peer-closed exercise of `sys_channel_*`.
    //     (Cross-process IPC + handle transfer arrive with process spawn.)
    let mut ipc_ok = true;
    if let Ok((e0, e1)) = Handle::<Channel, Only>::pair(4) {
        // Send on end0 (NoBlock). The message lands in end1's inbox.
        let mut ping = Msg::new();
        let _ = ping.set(IPC_PING);
        if e0.send(&ping, &mut Carried::new()).is_err() {
            ipc_ok = false;
        }

        // Polling end0 finds nothing — the message went to end1's inbox, not end0's.
        let mut recv = Msg::new();
        if refused(&e0.recv(&mut recv), KError::WouldBlock) {
            kprint(b"ipc: empty-endpoint would-block ok\n");
        } else {
            ipc_ok = false;
        }

        let mut set = WaitSet::<1>::new();
        let _ = set.add(&e1);
        let waited = set.wait(Deadline::NEVER);
        let rr = e1.recv(&mut recv).is_ok();
        if matches!(waited, Ok(s) if s.len() == 1) && rr && recv.sender_pid() == 1 && recv.payload() == IPC_PING {
            kprint(b"ipc: received message from pid 1 ok\n");
        } else {
            ipc_ok = false;
        }

        if refused(&e1.recv(&mut recv), KError::WouldBlock) {
            kprint(b"ipc: drained would-block ok\n");
        } else {
            ipc_ok = false;
        }

        drop(e0);
        if refused(&e1.recv(&mut recv), KError::PeerClosed) {
            kprint(b"ipc: peer-closed detected ok\n");
        } else {
            ipc_ok = false;
        }
    } else {
        ipc_ok = false;
    }

    if ipc_ok {
        kprint(b"ipc: ok\n");
    } else {
        kprint(b"ipc: FAIL\n");
    }

    // 6. Deliberately fault. With notifications, a userspace fault is delivered
    //    to the process's supervisor as a SegFault notification and this thread
    //    is terminated — the kernel survives (it used to halt). The supervisor
    //    (the kernel boot thread, this slice) reports the catch. We write to a
    //    never-mapped low address (page 1) → #PF (not-present, write).
    kprint(b"hello: triggering a deliberate fault\n");
    // SAFETY: a deliberate fault: page 1 is never mapped, so the store faults and the kernel terminates this thread
    unsafe { core::ptr::write_volatile(0x1000 as *mut u64, 0xDEAD) };
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}
