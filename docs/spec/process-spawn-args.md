# SpawnArgs — `sys_process_spawn` argument block

This document specifies `SpawnArgs`, the `#[repr(C)]` block a parent passes to
[`sys_process_spawn`](syscall-abi.md) by `UserPtr<SpawnArgs>` to describe a child
process.

**Status:** Pre-stabilization. Phase 1 implements the form below with the
deferrals noted; the image selector and register bootstrap ABI change in Phase 2
(filesystem + a real init handoff).

## Layout

```rust
#[repr(C)]
pub struct SpawnArgs {
    pub image:        u32,            // offset 0  — ImageId selector
    pub handle_count: u32,            // offset 4  — valid entries in handles/rights (≤ 4)
    pub move_mask:    u32,            // offset 8  — bit i: move (1) vs duplicate (0) handle i
    pub _pad:         u32,            // offset 12
    pub arg0:         u64,            // offset 16 — opaque user data, delivered to the child
    pub handles:      [RawHandle; 4], // offset 24 — parent-side handles to install in the child
    pub rights:       [u64; 4],       // offset 56 — per-handle attenuation bound
}
```

Total size 88 bytes, 8-byte aligned. `SPAWN_MAX_HANDLES = 4`. The offsets are
pinned by compile-time asserts in `kernel/src/libkern/spawn.rs`.

## Fields

- **`image`** — an [`ImageId`](#imageid) selecting which executable the child
  runs. Phase 1: kernel-embedded; an unrecognised value returns `InvalidArgument`.
- **`handle_count`** — number of valid `handles`/`rights` entries (`≤ 4`; larger
  returns `TooLarge`).
- **`move_mask`** — for each `i < handle_count`, bit `i` selects **move** (the
  parent loses access; default) or **duplicate** (the parent keeps its handle).
- **`arg0`** — an opaque word delivered to the child at entry (in `rdx`). The
  Phase-1 demo uses it as a role selector.
- **`handles[i]`** — a handle in the **parent's** table to give the child. Each
  must carry the `TRANSFER` right. Installed in the child with
  `source_rights & rights[i]`.
- **`rights[i]`** — the attenuation bound for `handles[i]`.

## Handle install semantics

For each `i < handle_count` the kernel looks up `handles[i]` in the parent's
table (requiring `TRANSFER`), and installs a handle to the same object in the
child's table with the attenuated rights. **Duplicate** keeps the parent's
handle; **move** closes it once the spawn commits. The install is atomic-or-fail:
any failure before the child's first thread is enqueued rolls back every
child-side allocation and leaves the parent's handles untouched.

The child also receives a fresh **notification channel** (a handle in its table),
where its own `ChildExited` is *not* delivered (that goes to the parent) but
where the kernel delivers events addressed to the child (faults; later, peer
closes).

## Bootstrap (how the child receives its handles)

Phase 1 has no argc/argv/auxv. The kernel seeds three argument registers at the
child's first ring-3 entry, read directly as the `extern "C"` parameters of the
child's `_start`:

| Register | Value |
|---|---|
| `rdi` | the child's notification-channel handle |
| `rsi` | the child's first installed handle (`handles[0]`), or `0` if none |
| `rdx` | `args.arg0` |

(Phase 2 replaces this with a stack-resident bootstrap block carrying the full
initial handle set, matching the real init handoff.)

## ImageId

```rust
#[repr(u32)]
pub enum ImageId {
    Child = 0,   // userspace/child — the Phase-1 IPC-demo worker
}
```

A Phase-1 stand-in for a filesystem path: the kernel `include_bytes!`s the
spawn-able images and selects one by id. Phase 2 replaces this with an initramfs
path / a `MemoryObject` handle holding the ELF.

## ABI

`SpawnArgs` crosses the kernel/userspace boundary, so its layout is a kernel-ABI
version-hash input (like `IpcMsg` / `Notification`). The hash is not yet computed
in code, so nothing is enforced today — the compile-time asserts pin the offsets.

## Deferred to Phase 2

- `args.namespace` (per-process namespaces are Phase 2) — not present yet.
- Filesystem/`MemoryObject` image sourcing (replacing `ImageId`).
- The stack-resident bootstrap block (replacing the register ABI).
