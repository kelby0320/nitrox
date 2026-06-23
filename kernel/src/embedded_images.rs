//! Kernel-embedded userspace executables for `sys_process_spawn`.
//!
//! Phase 1 has no filesystem, so the images a parent can spawn are compiled
//! into the kernel (`include_bytes!`) and selected by an
//! [`ImageId`](crate::libkern::ImageId). Phase 2 replaces this with images
//! served from the initramfs (the selector becomes a path / `MemoryObject`
//! handle). The boot `parent`/`hello` images are embedded in the `main` binary;
//! only the spawn-able `child` lives here (the lib needs it for the syscall).

use crate::libkern::ImageId;

/// The Phase-1 IPC-demo child (`userspace/child`), built by `xtask` before the
/// kernel.
static CHILD_ELF: &[u8] =
    include_bytes!("../../userspace/target/x86_64-unknown-none/release/child");

/// The bootstrapping init (`userspace/init`), built by `xtask` before the kernel.
/// Spawnable via [`ImageId::Init`] (slice 4 Part 3); becomes the boot pid-1 image
/// in Part 5. The path-based-spawn / initramfs-relocation end state is slice 7.
static INIT_ELF: &[u8] =
    include_bytes!("../../userspace/target/x86_64-unknown-none/release/init");

/// The embedded ELF bytes for an [`ImageId`].
pub fn image_bytes(image: ImageId) -> &'static [u8] {
    match image {
        ImageId::Child => CHILD_ELF,
        ImageId::Init => INIT_ELF,
    }
}
