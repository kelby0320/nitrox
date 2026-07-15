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

## Build ordering and the initramfs

Programs are **packed into the initramfs** at `sbin/<name>`, not embedded in the
kernel. `cargo xtask build` builds each userspace program (run from its crate dir,
with `--target x86_64-unknown-none`, so its `.cargo/config.toml` applies); `cargo
xtask image` then packs the built ELFs into the initramfs CPIO
(`build_initramfs`). The kernel boot-loads `/sbin/init` from the initramfs, and
each spawner resolves its children by path (`/initramfs/sbin/<name>` → a readable
`MemoryObject` → `sys_process_spawn`; see the SpawnArgs spec). Because the kernel no
longer `include_bytes!`s userspace artifacts, its compile no longer depends on them
— but always build via `cargo xtask` so the ELFs exist before the image is
assembled.

To make a new program spawnable: add it to the `programs` list in `build_initramfs`
(`tools/xtask`), and have its spawner resolve `/initramfs/sbin/<name>`.
