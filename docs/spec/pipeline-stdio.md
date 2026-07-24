# Pipeline stdio & the process setup message

How a shell-spawned command receives its standard streams (`stdin`/`stdout`/`stderr`)
and command-line arguments (`argv`), and the wire contract for the **setup message**
that carries them.

**Status:** Pre-stabilization. This is the C3 CLI-substrate prereq
(`docs/planning/shell-coreutils-plan.md`); the ABI call is recorded in the decision
log (2026-07-23, "C3 ABI call"). It adds **no kernel or ABI-hash change** — it is a
userspace convention layered on the existing register bootstrap
(`process-spawn-args.md`). Exact byte offsets are canonical in `libos`/`libstream`;
this document is the contract those pin.

## Why a message, not more registers

A pipeline stage needs `{notif, namespace, stdin, stdout, stderr, argv}` (and later
`env`). The spawn bootstrap delivers four registers and **no strings** — it cannot
carry `argv`, and coreutils need it from the first milestone. Rather than grow the
register/handle budget (still can't carry strings) or introduce a kernel-marshaled
stack block (a hash-invalidating, rebuild-the-world change), the extra capabilities
and arguments arrive as **one IPC message** the parent sends after spawn — the same
pattern init already uses to hand `fs-server` its block device. The kernel already
provides every hard part: bounded-queue backpressure and `PeerClosed` on the IPC
channel (`kernel/src/object/ipc_channel.rs`), inline handle transfer (`IPC_HANDLE_MAX
= 8`), and `sys_channel_create(end0, end1, queue_depth)` to mint pipes.

## Two tiers

Bootstrap is **pay-per-use**. The register bootstrap is the universal, zero-syscall
floor; the setup message is opt-in.

- **Tier 0 — register-only, zero syscalls.** `{notif, namespace, arg0, one endpoint}`
  from registers, exactly as today. `init` (kernel-spawned, no parent to message it)
  and every existing service/leaf-shell stay here **unchanged**. Selected by
  `arg0 == 0` — the value every current spawner already passes.
- **Tier 1 — register + one setup recv (opt-in).** A stage calls
  `libos::bootstrap().setup()`, which receives + parses the setup message on its
  bootstrap endpoint. The parent sends it before/as the stage starts, so the recv is
  normally **pre-queued and non-blocking**.

## The `arg0` bootstrap descriptor

`rcx` (`SpawnArgs.arg0`) **is the bootstrap descriptor — system-wide, the one
meaning it has.** This is the single process-spawn convention: no program repurposes
`arg0` (or `endpoint`, or any register) for private payload. A program that needs
parameters gets them from `argv` in the setup message (Tier 1), never from `arg0`.
(This governs `sys_process_spawn`; `sys_thread_create`'s `arg0` is a separate,
in-process thread argument and is unaffected.)

`arg0 == 0` is **Tier 0** — no descriptor, register-only. A non-zero descriptor:

| Bits | Field | Meaning |
|---|---|---|
| `[7:0]` | `version` | Bootstrap-descriptor version, for format evolution. `0` ⇒ no descriptor (Tier 0). This ABI is version `1`. |
| `[8]` | `SETUP_PENDING` | A setup message is queued on the bootstrap endpoint (`rdx`); the runtime should receive it. |
| `[63:9]` | reserved | Must be `0`. |

A Tier-1 stage is spawned with `arg0 = 0x0000_0000_0000_0101` (`version = 1`,
`SETUP_PENDING`). A generic `_start` runtime reads `arg0` for *any* program: if
`SETUP_PENDING` is set it recvs the setup message; otherwise it returns the
register-only view and makes no syscall. Because `arg0` has exactly one meaning,
that detection is unambiguous — no magic marker is needed.

> The legacy `parent`/`child` demo programs predate this convention and abuse `arg0`
> as a private role/seed field. They are the only violators; every real service
> already passes `arg0 == 0`. They are slated to be rewritten as a conforming test
> harness (decision log, 2026-07-24) and are not part of normal bringup.

## The setup message

One IPC message, parent → child, on the child's bootstrap endpoint (the handle the
kernel installed at spawn and surfaced in `rdx`).

### Transferred handles

The present stream endpoints ride in `IpcMsg.handles[]`, packed contiguously in the
canonical order **stdin, stdout, stderr** — only those present are included. The
`streams` bitmap (below) says which are present, so the *k*-th set bit maps to the
*k*-th transferred handle. Room remains (`IPC_HANDLE_MAX = 8`) for later additions
(a working-directory handle, extra streams) appended after `stderr`.

- **stdin** — the read end of the upstream pipe (a `sys_channel_create` endpoint).
  Absent for a *source* stage (no upstream).
- **stdout** — the write end of the downstream pipe. Absent for a *sink* stage.
- **stderr** — a **shared diagnostic sink** (design §1: "separate from the pipe,
  surfaces to display/log"), *not* a per-adjacency pipe. The parent typically passes
  the same shared send-end (a `DUPLICATE`) to every stage in a pipeline.

### Payload

A TSM1 `Record` value (the C2 codec, `docs/spec/typed-stream-format.md`) — the setup
message is one structured value, not a data stream:

| Field | Type | Meaning |
|---|---|---|
| `streams` | `Int` | Presence bitmap: bit 0 `stdin`, bit 1 `stdout`, bit 2 `stderr`. Set bits, ascending, index the packed transferred handles. |
| `argv` | `List<String>` | Command-line arguments; `argv[0]` is the program name by convention. |

`env` is reserved as a future field; readers tolerate extra fields (C2 subset-match),
so it can be added without a version bump. The exact record schema is pinned in
`libos` (the sender) and consumed by `libos::bootstrap().setup()` (the receiver).

## Pipe semantics (kernel-provided)

The setup message only *wires* the endpoints; the stream behaviour is the IPC
channel's:

- **Backpressure** — each pipe is a `sys_channel_create` channel with a bounded queue
  (`queue_depth`). A blocking send on a full peer ring is held until the consumer
  drains it; no shell-level flow control.
- **Early-consumer close** (`yes | head -1`) — when a consumer closes its read end,
  the producer's next send surfaces `PeerClosed`. Library code
  (`libstream`/`libos`) treats that as "stop producing, exit cleanly." No signals.
- **Fatal stage failure** — the parent (which spawned every stage) observes
  `ChildExited` on its notification channel; stage lifecycle is not in-band.

## Not this

- **No kernel stack-resident bootstrap block.** Considered and deferred — its only
  win is "zero syscalls at entry," at the cost of a hash-invalidating kernel change
  and a changed `_start` for every binary. See the decision log (2026-07-23).
- **No `SPAWN_MAX_HANDLES` / register-count change.** Streams are *transferred in a
  message*, not installed at spawn, so the spawn ABI is untouched (`SPAWN_MAX_HANDLES`
  stays 4).
- **No `/dev/stdin`-style namespace plumbing.** Streams are handles delivered as data,
  not namespace-bound resources.

## See also

- `docs/spec/process-spawn-args.md` — the register bootstrap this layers on.
- `docs/spec/typed-stream-format.md` — TSM1 (`argv` is a `List<String>`).
- `docs/spec/ipc-message-format.md` — the IPC message envelope + handle transfer.
- `docs/history/nitrox-shell-design-v1.1.md` §1 — the pipeline execution model.
