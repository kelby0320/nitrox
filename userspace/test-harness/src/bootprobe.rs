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

use libkern::{exit, kprint};

/// Bootstrap registers, as `service-mgr`'s `SPAWN_SERVICE` fills them: `rdi` = this
/// process's notification channel, `rsi` = the inherited LOOKUP-only root namespace,
/// `rdx` = the control-channel endpoint (`RECV | WAIT`), `rcx` = `arg0` (unused).
///
/// **The control endpoint is held until exit, and closing it early is a bug.** An earlier
/// version of this file closed it as its second instruction, reasoning that a probe with no
/// lifecycle protocol to serve has no use for it. It does have one use, and it is not the
/// probe's: `service-mgr` reads *this handle's* closure as "the child is gone"
/// (`supervise`), because a pid on `KIND_CHILD_EXITED` cannot be matched to a process
/// handle. Closing it early therefore reports a death that has not happened — observed as
/// `'boot-probe' exited code=unknown` printed before this function's own next line, and
/// under `policy = "always"` as a *second copy of a live service*, which is the exact
/// failure this program was added to prove is gone (PR #226 review, finding 1).
///
/// So: hold it, and let process teardown close it. That is what makes "peer closed" mean
/// "child exited", and it is a contract on every declared service rather than a quirk of
/// this one — see `docs/spec/service-toml-schema.md`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"boot-probe: up\n");
    let _ = control;
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
