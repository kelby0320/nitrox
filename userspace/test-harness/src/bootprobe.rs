//! `boot-probe` — the in-guest checks that have no user-facing surface, and the boot
//! verdict.
//!
//! **Why this is a program and not a phase of a supervisor.** `init` and `session-mgr`
//! carry the substrate probes today — `init` the filesystem tests, `session-mgr` the SMP
//! and floating-point gates — and they carry them because *the verdict* lives there, not
//! because the checks belong to a supervisor. `sched_gate`'s own comment says so: it is
//! "checked synchronously at the single PASS point". Move the verdict to a program whose
//! job is adjudication and the probes follow it out.
//!
//! See [`docs/planning/test-path-retrofit.md`](../../../docs/planning/test-path-retrofit.md).
//! This is Part A: the program, started from a **service declaration** so a test image
//! differs from a release image by data rather than by code. Parts B and C move the checks
//! in; until then it proves the plumbing and nothing else.
//!
//! **Started by `service-mgr`** from `/initramfs/etc/services.toml`, which carries a
//! `[service.boot-probe]` table only in selftest / test-harness images. It is therefore an
//! ordinary declared service: a control channel at `rdx`, a LOOKUP-only view of the root
//! namespace at `rsi`, no syscaps, and `policy = "never"` — start once, do not restart.
//!
//! **Its exit is what `service-mgr` attributes.** Being the second child of a supervisor
//! that could previously hold only one is the whole reason `supervise` learned to tell its
//! children apart, via each child's control channel rather than the pid on
//! `KIND_CHILD_EXITED` (`TODO(child-exit-attribution)`).

#![no_std]
#![no_main]

use libkern::{SYS_HANDLE_CLOSE, exit, kprint, syscall1};

/// Bootstrap registers, as `service-mgr`'s `SPAWN_SERVICE` fills them: `rdi` = this
/// process's notification channel, `rsi` = the inherited LOOKUP-only root namespace,
/// `rdx` = the control-channel endpoint (`RECV | WAIT`), `rcx` = `arg0` (unused).
///
/// The control channel is closed rather than served. A declared service is entitled to a
/// lifecycle channel, and `service-mgr` sends `CTRL_OP_SHUTDOWN` on it to the **first**
/// declared service after a demo interval — but this probe's whole shape is "run, report,
/// exit", so there is nothing for a shutdown request to interrupt. Closing it here is also
/// what makes the exit observable: `service-mgr` learns a child is gone from that channel's
/// peer closing, which is what happens when this process exits either way.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"boot-probe: up\n");
    if control != 0 {
        // SAFETY: closing our own control-channel endpoint, handed over at spawn.
        unsafe { syscall1(SYS_HANDLE_CLOSE, control) };
    }
    // Part A carries no checks yet, and says so rather than printing a verdict it did not
    // earn. `session-mgr` still writes the boot verdict until Part B moves it here.
    kprint(b"boot-probe: no checks yet (retrofit Part A) -- exiting 0\n");
    exit(0);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"boot-probe: PANIC\n");
    exit(1);
}
