//! [`SpawnArgs`] — the argument block `sys_process_spawn` reads from userspace.
//!
//! A parent describes a child process: which executable image to run, an
//! optional user data word handed to the child at entry, and the set of handles
//! to install in the child's table (with per-handle rights attenuation and a
//! move-or-duplicate choice). `docs/spec/process-spawn-args.md` is the normative
//! source; this module is its in-kernel embodiment (the value type only).
//!
//! ## Phase 1 limitations (deferred to Phase 2)
//!
//! - **Image selection is a kernel-embedded enum** ([`ImageId`]), not a
//!   filesystem path / `MemoryObject` handle — there is no filesystem yet. The
//!   kernel `include_bytes!`s the child ELF; the selector picks it.
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

/// Which kernel-embedded executable image to run (Phase 1 stand-in for a
/// filesystem path). The kernel maps each variant to an embedded ELF; an
/// unrecognised selector is rejected with `InvalidArgument`.
#[repr(u32)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ImageId {
    /// The Phase-1 IPC demo child (`userspace/child`).
    Child = 0,
}

impl ImageId {
    /// Decode an image selector, or `None` if unrecognised.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Child),
            _ => None,
        }
    }
}

/// The spawn argument block, passed by `UserPtr<SpawnArgs>`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SpawnArgs {
    /// Executable image selector ([`ImageId`]) (offset 0).
    pub image: u32,
    /// Number of valid entries in `handles`/`rights`; `≤ SPAWN_MAX_HANDLES` (offset 4).
    pub handle_count: u32,
    /// Bit `i` set ⇒ **move** `handles[i]` to the child (the parent loses it);
    /// clear ⇒ **duplicate** (the parent keeps its handle) (offset 8).
    pub move_mask: u32,
    /// Padding to 8-byte-align `arg0` (offset 12).
    pub _pad: u32,
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
}

const _: () = assert!(core::mem::size_of::<SpawnArgs>() == 24 + 16 * SPAWN_MAX_HANDLES + 8);
const _: () = assert!(core::mem::align_of::<SpawnArgs>() == 8);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, image) == 0);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, handle_count) == 4);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, move_mask) == 8);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, arg0) == 16);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, handles) == 24);
const _: () = assert!(core::mem::offset_of!(SpawnArgs, namespace) == 24 + 16 * SPAWN_MAX_HANDLES);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_args_layout_is_stable() {
        assert_eq!(core::mem::size_of::<SpawnArgs>(), 24 + 16 * 4 + 8);
        assert_eq!(core::mem::align_of::<SpawnArgs>(), 8);
        assert_eq!(core::mem::offset_of!(SpawnArgs, image), 0);
        assert_eq!(core::mem::offset_of!(SpawnArgs, handle_count), 4);
        assert_eq!(core::mem::offset_of!(SpawnArgs, move_mask), 8);
        assert_eq!(core::mem::offset_of!(SpawnArgs, arg0), 16);
        assert_eq!(core::mem::offset_of!(SpawnArgs, handles), 24);
        assert_eq!(core::mem::offset_of!(SpawnArgs, rights), 24 + 8 * 4);
        assert_eq!(core::mem::offset_of!(SpawnArgs, namespace), 24 + 16 * 4);
    }

    #[test]
    fn image_id_round_trips() {
        assert_eq!(ImageId::from_u32(0), Some(ImageId::Child));
        assert_eq!(ImageId::from_u32(1), None);
        assert_eq!(ImageId::Child as u32, 0);
    }
}
