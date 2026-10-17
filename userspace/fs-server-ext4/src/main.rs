//! `fs-server-ext4` — the server `[[bin]]`.
//!
//! The first **userspace resource server**: it serves an ext4 filesystem over a block device
//! through the resource-server protocol, reached via the namespace.
//!
//! **The protocol is `libfsserver`'s**, shared with `fs-server-fat`; this binary is ext4 as a
//! [`Volume`](libfsserver::Volume) and a `_start`. **Alloc-free**: fixed `.bss` buffers, no global
//! allocator. It never holds `BIND_NAMESPACE` (its supervisor binds its endpoint); see
//! `docs/rationale/why-supervisor-registration.md`.

#![no_std]
#![no_main]

use fs_server_ext4::{Ext4, ReadOnly};
use libfsserver::cache::{MetaCache, SERVER_BLOCKS, Store};
use libfsserver::disk::Disk;
use libfsserver::server;

/// The metadata cache's memory: all zeroes, so `.bss`.
static mut STORE: Store<SERVER_BLOCKS> = Store::new();

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the
/// notification channel (unused), `rsi` = the inherited root namespace (unused —
/// the server resolves nothing), `rdx` = the **control channel** endpoint its
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    // 1–2. The block device, and whether to serve it read-only, from the setup message.
    let (disk, read_only) = server::bootstrap(control, Disk::new);
    let device = disk.device();
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // The device through a metadata cache.
    // SAFETY: `_start` runs once, and this is the only reference `STORE` ever has.
    let cached = MetaCache::new(&disk, unsafe { &mut *(&raw mut STORE) });
    // 3–6. Check, Ready, serve — a read-only mount through the type that refuses every write,
    //      which also marks each file it resolves read-only.
    if read_only {
        let ro = ReadOnly(&cached);
        server::run(&Ext4(&ro), control, device)
    } else {
        server::run(&Ext4(&cached), control, device)
    }
}

/// **Say where, and exit**, so a resolve waiting on this server fails rather than waits for ever.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    server::panicked(info)
}
