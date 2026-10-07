//! `fs-server-fat` — the server `[[bin]]` (Phase 6 Part E.4).
//!
//! A FAT12, FAT16 or FAT32 filesystem served over a block device, as `fs-server-ext4` serves
//! ext4: **the protocol is `libfsserver`'s**, and this binary is the library's [`Fat`] as a
//! [`Volume`](libfsserver::Volume) and a `_start`. Its disk is `libfsserver`'s
//! [`SectorDisk`], sector-granular and up to 64 KiB a submit, since a FAT's structures are
//! sector-aligned and its partition's last sectors need not fill a 4 KiB block. **Alloc-free.**
//!
//! It never holds `BIND_NAMESPACE` (its supervisor, the storage service, binds its endpoint) and
//! receives only the handles it needs at spawn — see
//! `docs/rationale/why-supervisor-registration.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use fs_server_fat::{Fat, ReadOnly};
use libfsserver::disk::SectorDisk;
use libfsserver::server;

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the notification
/// channel (unused), `rsi` = the inherited root namespace (unused — the server resolves nothing),
/// `rdx` = the **control channel** endpoint its supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    // 1–2. The block device, and whether to serve it read-only, from the setup message.
    let (disk, read_only) = server::bootstrap(control, SectorDisk::new);
    let device = disk.device();
    // 3–6. Check, Ready, serve — a read-only mount through the type that refuses every write,
    //      which also marks each file it resolves read-only.
    if read_only {
        let ro = ReadOnly(&disk);
        server::run(&Fat::new(&ro), control, device)
    } else {
        server::run(&Fat::new(&disk), control, device)
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
