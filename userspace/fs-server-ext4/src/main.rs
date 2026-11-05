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
use libfsserver::cache::{MetaCache, OnceStore};
use libfsserver::disk::Disk;
use libfsserver::server;
use libos::boot::Start;
use libos::{Channel, Only, exit, log};

/// The metadata cache's memory.
static STORE: OnceStore = OnceStore::new();

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the
/// notification channel (unused), `rsi` = the inherited root namespace (unused —
/// the server resolves nothing), `rdx` = the **control channel** endpoint its
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: `_start`'s four registers as the kernel set them, wrapped here and nowhere else.
    let mut start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    // `start` keeps the two unused handles for the server's life, as serving never returns.
    let Some(control) = start.take_handle0::<Channel, Only>() else {
        log::write(b"fs-server: setup recv failed\n");
        exit(1)
    };
    // 1–2. The block device, and whether to serve it read-only, from the setup message.
    let (disk, read_only) = server::open_disk(&control, Disk::over);
    // The device through a metadata cache.
    let store = STORE.take().expect("`_start` runs once");
    let cached = MetaCache::new(&disk, store);
    // 3–6. Check, Ready, serve — a read-only mount through the type that refuses every write,
    //      which also marks each file it resolves read-only.
    if read_only {
        let ro = ReadOnly(&cached);
        server::serve_volume(&Ext4(&ro), control, disk.handle())
    } else {
        server::serve_volume(&Ext4(&cached), control, disk.handle())
    }
}

/// **Say where, and exit**, so a resolve waiting on this server fails rather than waits for ever.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    server::panicked(info)
}
