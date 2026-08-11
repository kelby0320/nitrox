# Building and embedding a userspace program

How a userspace binary is built and loaded by the kernel, as established by
the first userspace process (`userspace/hello`).

## The program

A standalone userspace program that the kernel loads is `#![no_std]` +
`#![no_main]` with a hand-rolled `_start` (no crt0; the kernel sets the user
stack and jumps to the ELF entry). It issues syscalls via inline `asm!`
(`rax` = number, `rdi`/`rsi`/… = args; `syscall` clobbers `rcx`/`r11`). It
needs a `#[panic_handler]`.

## It must be a static, non-PIE `ET_EXEC`

The kernel ELF loader (`kernel/src/mm/elf.rs`) accepts only `ET_EXEC`
(rejects PIE/`ET_DYN`), with no `PT_INTERP`, and page-aligned `PT_LOAD`
segments (`p_vaddr % PAGE == p_offset % PAGE`), all below `USER_VIRT_END`.
Rust's `x86_64-unknown-none` defaults toward PIE, so the crate forces:

- `.cargo/config.toml` (in the crate dir, so it doesn't affect sibling
  crates): `relocation-model=static`, `code-model=small`,
  `link-arg=-no-pie`, `link-arg=-static`, `link-arg=-zmax-page-size=0x1000`.
- A `user.ld` linker script (fixed low base, `ENTRY(_start)`, page-aligned
  segments, `/DISCARD/` of `.interp`/`.dynamic`/`.got`), passed via a
  `build.rs` `-T` arg (mirrors `kernel/build.rs`; a relative `-T` in the
  config would not resolve from the linker's cwd).

Verify the output: `readelf -h <elf>` → `Type: EXEC`; `readelf -l <elf>` →
no `INTERP`, each `LOAD` has `VirtAddr % 0x1000 == Offset % 0x1000`.

## Build ordering, the store, and the initramfs

Programs are **not embedded in the kernel**. `cargo xtask build` builds each userspace
program (run from its crate dir, so its `.cargo/config.toml` selects the custom target);
`cargo xtask image` then places the built ELFs. Because the kernel no longer
`include_bytes!`s userspace artifacts, its compile no longer depends on them — but always
build via `cargo xtask` so the ELFs exist before the image is assembled.

Where a program is placed is the part that matters, and there are two answers.

**Almost everything goes in the content-addressed store**, at
`/store/<hash>-<package>-<version>/bin/<name>` on the ext4 root, and is projected into `/bin`
by the profile server. A spawner resolves `/bin/<name>` → a readable `MemoryObject` →
`sys_process_spawn` (see the SpawnArgs spec).

**The initramfs holds only what cannot come from a filesystem** — today `init`,
`fs-server-ext4`, `eshell` and `profile-server`, at `sbin/<name>`. The kernel boot-loads
`/sbin/init` from it. This list has drifted twice (2026-08-03 and again by 2026-08-11), both
times because adding a name to a list of names is a one-word change, so it is now a list of
`(program, why it cannot come from the filesystem)` pairs with a size ceiling behind it.

**To make a new program spawnable**, in order of what to try:

1. Add it to a store package — `SYSTEM_SERVICES` for a service, `COREUTILS` for a user
   program, `TEST_PROGRAMS` for a gate — in `tools/xtask/src/main.rs`, and have its spawner
   resolve `/bin/<name>`. This is the answer unless the next point applies.
2. Only if it must run *before there is a filesystem to read it from*: add it to
   `INITRAMFS_PROGRAMS` **with the reason**, raise `INITRAMFS_MAX_BYTES`, and resolve
   `/initramfs/sbin/<name>`. If you cannot write a reason, it is not this case.

Init's boot order follows from the same split: anything spawned from `/bin` must come up
after `bind_profile_server`, which is what provides `/bin`.
