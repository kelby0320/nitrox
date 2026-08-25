//! `desktop-shell` — the graphical session's leader.
//!
//! **A stub, deliberately, and Part E is what makes it a shell.** Part D needs a leader to
//! spawn so that a graphical login can be observed end to end; what that leader *is* belongs
//! to the part that builds it. Standing this up empty is what lets Part D ship a login gate
//! that Parts E and F then land against, rather than a gate written after the fact.
//!
//! What it does prove, which is not nothing: `desktop-session-mgr` authenticated a user, built
//! a session namespace without `/dev/console`, spawned an unprivileged process into it, and
//! will reap it.
//!
//! It does not draw. Its namespace has no `/dev/draw` — `libsession::build_namespace` binds
//! the serial column's set, and the graphical bind arrives with `/dev/desktop` in Part E.

#![no_std]
#![no_main]

use libkern::*;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Wait set for the setup channel.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// One 24-byte `IoResult`.
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _ns: u64, setup: u64, _arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");

    // **Blocks on the setup channel, never spins.** A spinning leader keeps a run queue
    // non-empty, so the idle thread never runs and deferred handle reclamation — which lives
    // there — stops for the whole machine. That is the 2026-07-31 `logging-service` bug, and
    // it is worth getting right in a stub because the stub is what runs until Part E.
    //
    // The session lasts as long as this process, so this is also what makes the graphical
    // session persist: `desktop-session-mgr` is blocked reaping it.
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
        let waited = unsafe {
            WAIT_HANDLES[0] = setup;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited != 1 {
            // The wait itself failed — a handle that cannot be waited on will never become
            // waitable, so retrying is a spin. Exit and let the supervisor draw its greeter.
            kprint(b"desktop-shell: setup channel unwaitable; exiting\n");
            break;
        }
        // Drain, so the channel does not stay signalled. Part E reads `argv` and the
        // environment from here; a stub that ignored them would still have to consume them.
        // SAFETY: valid recv out-params.
        let _ = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                setup,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
    }
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 0, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Recv buffers for the setup message.
static mut RECV_MSG: [u8; 4096] = [0; 4096];
/// See [`RECV_MSG`].
static mut RECV_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut RECV_COUNT: usize = 0;

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-shell: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
