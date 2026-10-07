//! `fs-server-ext4` — the server `[[bin]]` (slice 7 Part 4).
//!
//! The first **userspace resource server**: a process that serves an ext4 filesystem over a
//! block device through the resource-server protocol, reached transparently via the namespace
//! (the kernel forwards `sys_ns_lookup` to it — slice 7 Part 3).
//!
//! **The protocol is `libfsserver`'s** since Phase 6 Part E.1, which serves `fs-server-fat`
//! the same way: the bootstrap, the forwarded requests, directory sessions and the control
//! channel are there, and this binary is ext4 as a [`Volume`](libfsserver::Volume) and a
//! `_start`. **Alloc-free** — fixed `.bss` buffers, no global allocator.
//!
//! It never holds `BIND_NAMESPACE` (its supervisor binds its endpoint) and receives only the
//! handles it needs at spawn — see `CLAUDE.md` and
//! `docs/rationale/why-supervisor-registration.md`.

#![no_std]
#![no_main]

use fs_server_ext4::{Ext4, ReadOnly};
use libfsserver::disk::Disk;
use libfsserver::server;

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the
/// notification channel (unused), `rsi` = the inherited root namespace (unused —
/// the server resolves nothing), `rdx` = the **control channel** endpoint its
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    // 1–2. The block device, and whether to serve it read-only, from the setup message.
    let (disk, read_only) = server::bootstrap(control, Disk::new);
    let device = disk.device();
    // 3–6. Check, Ready, serve — a read-only mount through the type that refuses every write,
    //      which also marks each file it resolves read-only.
    if read_only {
        let ro = ReadOnly(&disk);
        server::run(&Ext4(&ro), control, device)
    } else {
        server::run(&Ext4(&disk), control, device)
    }
}

/// **Say where, and exit**, so a resolve waiting on this server fails rather than waits for ever.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    server::panicked(info)
}
