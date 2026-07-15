//! [`SpawnArgs`] — the argument block `sys_process_spawn` reads from userspace.
//!
//! A parent describes a child process: which executable image to run, an
//! optional user data word handed to the child at entry, and the set of handles
//! to install in the child's table (with per-handle rights attenuation and a
//! move-or-duplicate choice). `docs/spec/process-spawn-args.md` is the normative
//! source; this module is its in-kernel embodiment (the value type only).
//!
//! ## Image source
//!
//! - **The image is a [`MemoryObject`](crate::object::MemoryObject) handle**
//!   (`image`) holding the program's ELF bytes. The spawner resolves the executable
//!   path in userspace (`sys_ns_lookup` → a readable `MemoryObject`) and passes the
//!   handle; `sys_process_spawn` reads its bytes and loads the ELF. No filesystem
//!   code enters the kernel. (init itself is loaded from the initramfs by the kernel
//!   at boot — see `run_first_userspace`.)
//! - The child receives its installed handle *values* via a register bootstrap
//!   ABI (see `sys_process_spawn`), not a stack-resident handle block.
//!
//! ## ABI
//!
//! `SpawnArgs` crosses the kernel/userspace boundary, so its layout is a
//! kernel-ABI-hash input (like [`IpcMsg`](crate::libkern::IpcMsg) /
//! [`Notification`](crate::libkern::Notification)). The hash is not yet computed
//! in code, so nothing is enforced today — the compile-time asserts pin the
//! offsets.

use crate::libkern::handle::RawHandle;

/// Maximum initial handles a parent can install in a child at spawn.
pub const SPAWN_MAX_HANDLES: usize = 4;

/// The spawn argument block, passed by `UserPtr<SpawnArgs>`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SpawnArgs {
    /// A [`MemoryObject`](crate::object::MemoryObject) handle holding the program's
    /// ELF image; the spawner resolves the executable path (userspace) and passes the
    /// handle, which `sys_process_spawn` reads (requires `MAP_READ`) and loads (offset 0).
    pub image: RawHandle,
    /// Number of valid entries in `handles`/`rights`; `≤ SPAWN_MAX_HANDLES` (offset 8).
    pub handle_count: u32,
    /// Bit `i` set ⇒ **move** `handles[i]` to the child (the parent loses it);
    /// clear ⇒ **duplicate** (the parent keeps its handle) (offset 12).
    pub move_mask: u32,
    /// Opaque user data handed to the child at entry (in `rdx`) (offset 16).
    pub arg0: u64,
    /// Parent-side handles to install in the child's table (offset 24).
    pub handles: [RawHandle; SPAWN_MAX_HANDLES],
    /// Per-handle rights attenuation bound; the installed rights are
    /// `source_rights & rights[i]` (offset 24 + 8·N).
    pub rights: [u64; SPAWN_MAX_HANDLES],
    /// The child's root namespace (offset 24 + 16·N). `RawHandle::NULL` (`0`) ⇒
    /// **inherit** a `LOOKUP`-only handle to the parent's namespace; non-null ⇒ a
    /// namespace the parent holds a `LOOKUP`-righted handle to (typically a
    /// more-restricted one the parent constructed) — the child receives a
    /// `LOOKUP`-only handle to it. See
    /// `docs/architecture/namespace-and-resource-servers.md` (sandbox-by-construction).
    pub namespace: RawHandle,
    /// The ambient [`SysCaps`](crate::libkern::SysCaps) to grant the child, as a raw
    /// bit pattern (offset 24 + 16·N + 8). The kernel installs
    /// `parent.syscaps & syscaps` — a parent can never grant a capability it does not
    /// hold. `0` ⇒ an unprivileged child. See `docs/architecture/syscaps.md`.
    pub syscaps: u64,
}

const _: () = assert!(core::mem::size_of::<SpawnArgs>() == 24 + 16 * SPAWN_MAX_HANDLES + 16);
const _: () = assert!(core::mem::align_of::<SpawnArgs>() == 8);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, image) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, handle_count) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, move_mask) == 12);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, arg0) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, handles) == 24);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, namespace) == 24 + 16 * SPAWN_MAX_HANDLES);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, syscaps) == 24 + 16 * SPAWN_MAX_HANDLES + 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_args_layout_is_stable() {
        assert_eq!(core::mem::size_of::<SpawnArgs>(), 24 + 16 * 4 + 16);
        assert_eq!(core::mem::align_of::<SpawnArgs>(), 8);
        assert_eq!(core::mem::offset_of!(SpawnArgs, image), 0);
        assert_eq!(core::mem::offset_of!(SpawnArgs, handle_count), 8);
        assert_eq!(core::mem::offset_of!(SpawnArgs, move_mask), 12);
        assert_eq!(core::mem::offset_of!(SpawnArgs, arg0), 16);
        assert_eq!(core::mem::offset_of!(SpawnArgs, handles), 24);
        assert_eq!(core::mem::offset_of!(SpawnArgs, rights), 24 + 8 * 4);
        assert_eq!(core::mem::offset_of!(SpawnArgs, namespace), 24 + 16 * 4);
        assert_eq!(core::mem::offset_of!(SpawnArgs, syscaps), 24 + 16 * 4 + 8);
    }
}
