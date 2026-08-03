# Deferred Decisions

This document tracks design decisions that have been deliberately deferred — known concerns that aren't being addressed in the initial implementation, with the reasoning for why deferral is acceptable. The goal is to make the deferrals explicit rather than implicit, so that future readers (and future-you) understand what's been knowingly omitted versus what's been overlooked.

There's a distinction between **non-goals** (things that won't be done at all) and **deferrals** (things that will be done, but not now). This document covers both, marking which is which.

For the reasoning behind specific architectural choices, see the topical rationale documents (e.g., `why-capabilities.md`, `why-no-signals.md`). This document covers what *isn't* being built, where the previous documents cover what *is*.

## Non-goals (permanent)

These are not going to be done. Architecture is structured to not require them.

**POSIX compatibility as a primary goal.** Nitrox does not aim to be a POSIX-compliant operating system. Programs written for POSIX do not, in general, work on Nitrox without modification. A POSIX compatibility shim may be added later as a pragmatic concession for ported software (see deferrals below), but it's not a constraint on the native interface design. Native Nitrox programs use the handle-based interface with typed data, async-first I/O, and capability discipline.

**Global ambient authority (UID/GID).** Authority is held in handles, never derived from process identity. There is no "user" concept at the kernel level. The session manager and authentication service handle the human-facing user model, but the kernel doesn't know what a user is.

**Unix signals.** The notification queue replaces signals. See [`why-no-signals.md`](why-no-signals.md). There's no plan to add signals later.

**A global VFS tree in the kernel.** The kernel does not maintain a global mount table or filesystem dentry cache. Per-process namespaces and resource servers replace this entirely. There's no plan to add a kernel VFS later.

**A monolithic kernel with filesystem code inside it.** Filesystems are userspace. There's no plan to move them into the kernel.

**Synchronous syscalls that block.** Every potentially-blocking operation returns a `PendingOperation` handle. See [`why-async-syscalls.md`](why-async-syscalls.md).

**KPTI / Meltdown-class speculative execution mitigations.** Nitrox is a hobby OS where the security model is capability-based sandboxing, not "protect against speculative execution side channels in untrusted userspace running on untrusted hardware." This is a reasoned choice for the project's scope, not an oversight. Adding KPTI would significantly impact syscall performance and complicates the higher-half kernel design.

**Legacy (pre-2014) x86 hardware.** There is no requirement to run on old machines. The kernel already requires SMEP and SMAP (it enables and asserts them; the dev loop passes `+smep,+smap`) — SMAP is Broadwell, so the de-facto x86 floor is **≈ 2014**. The baseline is roughly **x86-64-v2 ISA plus SMEP/SMAP**; on any CPU meeting it, an invariant TSC and x2APIC are also guaranteed. This is a deliberate "no legacy" scope choice, not an oversight, and it is what lets the kernel assume modern features rather than carry fallback paths for ancient hardware. (BIOS/legacy-boot is separately out of scope — the project is UEFI + Limine only.)

## Open deferrals

These will eventually be done, but aren't in initial scope. Each entry documents what's
deferred and what triggers it. **Everything below is still open** — resolved entries move
to the [Resolved](#resolved-kept-for-the-record) table at the bottom, so this section can
be read as the list of what is actually owed.

### Hardware support

**aarch64.** The architecture abstraction layer is designed in from the start. Every arch-specific concern (paging, interrupts, FPU, user memory access, power) is behind a trait. The `kernel/src/arch/aarch64/` directory exists as stubs. Initial implementation targets x86_64; aarch64 implementation comes after the x86_64 system is mature. Trigger: when there's a specific aarch64 target system to support, or when the x86_64 implementation is stable enough to make the porting effort worthwhile.

**5-level paging on x86_64 (57-bit virtual addresses).** Ice Lake and later support 5-level paging, allowing virtual address spaces up to 128 PiB. Nitrox uses 4-level paging (canonical 48-bit). The address space is plenty for any conceivable workload. Trigger: a use case that requires it. None foreseen.

**KASLR (kernel image ASLR).** The kernel image is loaded at a fixed higher-half address. User-space ASLR is implemented (28 bits of entropy for ELF, stack, and mmap arena). Kernel ASLR is a defense-in-depth measure against kernel-mode exploits. Not initially. Trigger: a security hardening pass after the system is mature.

### Power management

**Phase 2 ACPI (ACPICA integration via FFI).** Phase 1 (pure Rust ACPI table parsing, no AML) ships first. ACPICA integration is deferred until needed. See [`why-phased-acpi.md`](why-phased-acpi.md). Triggers: laptop targeting, graceful S5 shutdown requirement, AML-only device support, CPU power management beyond `hlt`, GPE handling.

**Full laptop power management.** Battery, lid switch, thermal zones, AC adapter — all gated on Phase 2 ACPI.

**CPU C-state and P-state management.** Power saving via deep CPU sleep states and frequency scaling. Gated on Phase 2 ACPI.

**Suspend-to-RAM (S3), hibernation (S4), runtime device power management.** All gated on Phase 2 ACPI plus additional design work specific to each.

### Kernel module infrastructure

**Module signing.** LKMs are not cryptographically signed in the initial design. `SysCaps::LOAD_MODULE` is required to load modules; the capability is the access control. Signing would add a trust hierarchy on top. Trigger: production deployment scenarios where unsigned modules are unacceptable. Not foreseen for hobby use.

**ABI-compatible module loading across kernel versions.** Modules must be rebuilt against the running kernel. The build-hash ABI version refuses cross-version loading. ABI compatibility would impose stable interface constraints on the kernel that limit evolution. Trigger: a stable kernel release where ABI compatibility is worth the constraint.

**Live kernel patching / hot upgrade.** Not in scope.

### Drivers and interrupts

See `docs/architecture/drivers-and-irps.md` for the framework these defer from.

**Tier 2 (runtime-loadable) drivers.** Phase 2 ships only Tier 1 drivers
(compiled into the kernel ELF via Cargo features: `pci`, `ahci`, `gpt`). The
userspace driver manager — matching `DeviceNode`s to loadable modules and
handing a driver process a `Handle<DeviceNode>` — needs the kernel-module
loader (`export!` table, ELF relocation, ABI-hash enforcement) which is itself
deferred (see "Kernel module infrastructure" above). Trigger: hot-pluggable or
optional hardware that isn't on the boot path.

**MSI / MSI-X (message-signalled interrupts).** Phase 2 routes device
interrupts through the IOAPIC (legacy line interrupts), which is sufficient for
the QEMU AHCI controller. MSI/MSI-X (and the per-vector affinity they enable)
land when a device needs them. Trigger: NVMe, multi-queue NICs, or performance
work on interrupt-heavy devices.

**A dedicated arch trait for the device-interrupt *installation* facility.**
`install_pci_irq` (the composite that registers a handler in the arch vector
table + routes a GSI to it — Part 3) is currently a **neutral free function**
(`crate::arch::install_pci_irq`), not a method on `ArchIrqRouter`: it spans three
hardware abstractions (the handler registry, the local controller, and the
router) and belongs to none. When the device-interrupt *family* grows a second
member — **MSI/MSI-X install**, **shared-INTx chaining**, or **IRQ teardown**
(Tier 2 module unload drains + unhooks an IRQ) — promote the family into its own
arch trait (e.g. `ArchIrqInstall`), distinct from `ArchIrqRouter` (pure routing)
and `ArchIrq` (the local controller). One method + one consumer today does not
justify the trait (the project builds an abstraction at its second consumer); the
`TODO(msi)` on the function marks the trigger.

**Shared PCI INTx interrupt chaining.** The "chain of handlers, each returning
*mine* / *not mine*" model for shared legacy interrupt lines is deferred; Phase 2
assumes each handled GSI has one owner. MSI/MSI-X are never shared, so this only
matters for legacy INTx sharing. Trigger: real hardware where INTx lines are
shared across functions.

**IOMMU programming and userspace drivers.** Granting a `DeviceNode` /
`InterruptObject` to a userspace driver process safely requires programming the
IOMMU (VT-d / AMD-Vi / SMMU) to constrain the device's DMA to memory the driver
legitimately holds. Phase 2 has only in-kernel drivers, so neither the IOMMU
programming nor userspace driver hosting is built. Trigger: a userspace driver
(e.g. a userspace NIC or GPU driver).

**IRP cancellation and the completion timeout.** The IRP framework lands without
request cancellation or the 30-second force-complete timeout. Phase 2 stacks are
shallow and the boot-path block driver completes promptly. `sys_io_cancel` is
defined (number reserved) but returns `Unsupported` until this lands. Trigger:
long-running or cancellable I/O (network, user-abortable operations) and Tier 2
module unload (which drains in-flight IRPs).

**Async-I/O surface subset.** The [`IoOp`](../spec/io-operation.md) descriptor
ships with only `Read`/`Write` opcodes and no `flags` modifiers; `Flush`/`Trim`,
force-unit-access / no-cache flags, and multi-buffer scatter/gather in one `IoOp`
are deferred to their first consumer (RW filesystems, SSD trim). The block
namespace ships **enumeration-order** whole-disk names (`/dev/blk/0..`);
content-stable `/dev/disk/by-partuuid/*` / `by-partlabel/*` names are slice 6
(they need GPT metadata). The `/dev/blk` binding is **read-only** in Phase 2
(RO `fs-server-ext4`); RW block access lands with RW filesystems (Phase 3).

**Filter drivers.** Transparent insertion of a driver into a stack (encryption,
compression, logging, LUKS, LVM) is part of the IRP design but unimplemented.
Phase 2 has single- and two-layer stacks only (AHCI; GPT-over-block). Trigger:
the first filter use case (encrypted root / LVM, both already deferred under
"Filesystems").

**NVMe.** Phase 2's first storage driver is AHCI (simpler than NVMe). The `nvme`
Tier 1 feature follows. Trigger: NVMe hardware or a faster boot device matters.

**AHCI driver scope.** The Phase 2 AHCI driver (Part 3) supports a **single
controller, single SATA disk, one command *issued* at a time** (slot 0). Multi-port /
multi-disk, multiple controllers, and port multipliers are deferred to when a
configuration needs them. It resolves the controller's GSI from the **PCI
interrupt-line register** (firmware-programmed on QEMU); proper ACPI `_PRT` routing
(which needs AML) is deferred — see `device-node.md`. The read self-test brings up
against the existing AHCI boot disk; the dedicated `xtask build-disk` + ext4 test disk
arrive with the fs-server (slice 7).

> **Concurrent submits are queued, not dropped (2026-07-20).** The driver drives one
> command at a time, but *concurrent* block submits from different clients (a page-fault
> fill, a `sys_io_submit`, another CPU) are now serialised through a small software FIFO
> in front of slot 0 (`PendingRing`, `IrqSpinLock`-guarded) — a second submit while slot 0
> is busy queues and issues when the slot retires, rather than clobbering the in-flight
> IRP (which orphaned its waiter → a hang). This was a real correctness bug found bringing
> up the auth/session login chain concurrently with the demo chain. **Full NCQ — letting
> the controller run up to 32 commands at once across the command list — remains a future
> item** (the queue depth is already `PENDING_DEPTH = 32`, so the software queue converts
> to NCQ slots cleanly when we build it; the trigger is a workload that is I/O-latency
> bound, e.g. an SSD or many concurrent readers).

**Concurrent same-page faults.** When a file page fault misses, the fault path reserves
the frame (`Loading`), starts the producer fill, and parks the faulting thread on a
per-*fault* `PendingOperation`. A *second* thread faulting the **same** page has no handle
on that in-flight fill's PO, so it `yield_now`s and retries until the page is `Ready` — a
spin, not a block. This was unreachable when written (single CPU, one faulter per
`FileObject`); **under SMP it is reachable today**, and `std::thread` makes it ordinary.
The fix — store the fill PO in the cache page so a second faulter blocks on it (one wakeup,
no spin) — is scheduled as **B3 of the pre-CLI substrate-hardening pass**
(`docs/planning/phase-4-desktop.md`).

**Kernel log buffer is keep-early, not keep-recent (slice 9 Part 5).** `klog`
(`/dev/log`) is a **linear append** buffer: it captures kernel `kprint!` output from
boot until its 16 KiB fills, then drops later output. This keeps the early boot /
failure context (what an emergency inspection wants) and comfortably holds a full
boot log. The trade-off: on a long-running or verbose system it stops capturing
recent messages — the opposite of what `dmesg` usually wants for "what just happened."
A keep-recent **ring** (overwrite-oldest, linearised on snapshot) is the refinement;
deferred until the system runs long enough to overflow 16 KiB (eshell/services beyond
boot). The snapshot fill (`copy_into_frames`) already handles the segmented copy a
ring would need.

**AHCI NCQ (multiple in-flight commands).** The driver issues **one command at a time**
(slot 0). The *contention* this used to cause is resolved — concurrent submits from
different clients queue in a software FIFO in front of slot 0 (`PendingRing`, 2026-07-20)
instead of clobbering the in-flight IRP, and the demo/login sequencing that worked around
it has been lifted. What remains is **throughput**: letting the controller run up to 32
commands at once across the command list. The software queue's depth is already
`PENDING_DEPTH = 32`, so it converts to NCQ slots cleanly. Trigger: an I/O-latency-bound
workload (an SSD, or many concurrent readers).

**Stateless `File::ReadRange` fill — Model B only, no shipping consumer.** Every
filesystem shipping today is Model A (the kernel reads the device directly from a block
map), so this costs nothing at present; it applies when a **non-block** filesystem exists —
a network or synthetic server. Recorded because the shape is real when it arrives. A
page-cache fill names its
file by re-sending the path `suffix` on every `ReadRange` (the same suffix the lazy
`Resolve` used), so the fs-server re-resolves the path per fill rather than handing
back an open-file cookie at resolve time. Simple and correct for the milestone; the
re-resolve cost hides behind the IPC round-trip. A server-side open-file handle
(resolve returns a cookie; `ReadRange` carries it) is the obvious Phase-3
optimization — defer until a profiling case or a stateful fs (RW, where the open
handle anchors writeback) forces it. See `docs/spec/rsproto-file-ops.md`.

**Page-cache scope (slice 8).** The first file page cache (slice 8, the **Model-B**
range-read fill — see the decision log, 2026-06-25) is deliberately minimal on three
axes. **(1) Per-file, not global.** Each `FileObject` owns a sparse page table; two
processes that independently resolve the same path get separate caches. Global,
inode-keyed sharing (one physical page shared across every mapping of a file) needs a
stable file identity the fs-server exposes and is deferred — trigger: a workload that
maps the same file hot from many processes. **(2) No eviction/reclaim.** The cache
grows to the mapped extent and is freed only on unmap / `FileObject` drop; the
clock-algorithm reclaim daemon + `Notification::MemoryPressure` is Phase 3+ — trigger:
caches that can grow past comfortable bounds (large files, many mappings). **(3)
Stateless fill protocol.** `File::ReadRange(suffix, …)` re-sends the path suffix per
range and the fs-server re-resolves suffix→inode each time (cacheable internally); a
stateful `file_id` / open-file table + a `close` op is a later optimization — trigger:
per-fault re-resolution showing up in profiles. The page cache is built behind a
**fill-producer seam** so the **Model-A** extent fill (Phase 3, zero-copy block reads)
slots in *alongside* `ReadRange` without a redesign.

**Demand-fault fill: one page per fault, no read-ahead.** The kernel fills **exactly one
4 KiB page per fault** (`FileObject::fault_in_page` reserves a single index). What that
costs depends on which data path the filesystem uses, and the two are very different:

- **Model A — every filesystem shipping today (ext4).** The fs-server hands the kernel the
  file's `BlockRun` map at resolve, and a fault reads the page **zero-copy straight from
  the device** with one block IRP. There is **no fs-server IPC in the fill path at all**.
  Read-ahead here is therefore a *purely kernel-side* change: the run map is already in
  hand, so filling N contiguous pages is one IRP covering N × PAGE bytes (the AHCI driver
  issues a multi-sector transfer as a single command), turning *pages* IRPs and wakes into
  *⌈pages/N⌉*.
- **Model B — no shipping consumer.** A non-block filesystem fills via a stateless
  `File::ReadRange` per page, and the server re-resolves the path per range. That is the
  expensive shape the slice-8/9 notes describe.

**Measured 2026-07-29** (the old ~325 ms-per-page figure was Model B, before the 4 KiB
block batching and the device-IRQ scheduling fix; it does not describe this system). A
whole adjudicated boot performs **43 fills** at **~137–204 µs** each — **0.5 % of boot** —
with **zero** concurrent-faulter spins. Nearly every program is spawned from the initramfs,
which resolves to a `MemoryObject` copy and never touches the page cache; the 43 are the
ext4-backed reads. Read-ahead would therefore optimise half a percent of boot, so it is
**deferred on measurement rather than on assumption**.

The counters behind those numbers are permanent (`file_object::fill_stats`,
`syscall::table::spawn_stats`, printed at the end of a `test-harness` run), so the trigger
is observable: **fill count climbing out of the tens**, which is what large binaries or
file-backed program text would cause. That is the same inflection as CoW and dynamic
linking — see the process-memory-model bundle.

**Two scheduled revisits**, rather than waiting for a profile to volunteer: **after the
typed shell + coreutils subproject** (a shell spawns per pipeline stage and drives real
filesystem traffic from userspace, unlike init's fixed boot sequence), and **after the
desktop UI MVP** (large binaries, many concurrent instances, fonts and images loaded from
files). At each, re-read the counters from a `test-qemu` boot: if fills have climbed out of
the tens, or image materialisation past a few milliseconds, this stops being deferred.

**TCP/IP networking.** The architecture is committed: userspace netstack server, network drivers as Tier 1 or Tier 2 modules, sockets as namespace resources. Implementation is deferred. Trigger: a concrete need (wanting to SSH into the system, wanting to download files, etc.). Implementation is a major effort (~15-50K lines depending on whether smoltcp is ported or a stack is written from scratch); deferring keeps the initial system simple while not foreclosing the work.

**Network booting (PXE) by the kernel.** Limine handles PXE before the kernel runs. The kernel itself doesn't need network for PXE. Network-mounted root filesystems can use the same userspace fs-server architecture as local mounts; this is gated on the netstack being implemented.

### Graphics

**GPU driver and compositor.** Architecture is sketched (GPU driver as Tier 2 LKM, compositor as userspace server, client-side rendering, Wayland-influenced protocol). Specific compositor protocol, 3D acceleration scope, window management model — all deferred. Trigger: when the project wants a GUI. Pre-compositor mode (`/dev/framebuffer` as a kernel resource server) is sufficient for early userspace, debug UI, and kernel panic screens.

**Specific compositor/client protocol.** Deferred along with the compositor itself. Likely Wayland-derived but using the resource-server protocol as the wire format. Decision when compositor work begins.

**3D acceleration, OpenGL/Vulkan equivalents, GPU compute.** All deferred. Initial scope is 2D framebuffer rendering.

**Text rendering, fonts, input methods, accessibility.** Downstream of the compositor.

### Filesystems

**A separate interrupt stack — `TODO(irq-stack)`.** Only `#DF` runs on a dedicated stack
(IST1, per-CPU, set up in `gdt.rs`). Every other interrupt — the timer, the TLB-shootdown
and reschedule IPIs, and every registered device handler — is configured with **IST0, i.e.
no IST**, so it nests onto whichever kernel stack is current. Linux x86_64, at the *same*
16 KiB thread-stack size, keeps interrupts off it entirely with a dedicated 16 KiB IRQ
stack (plus ISTs for NMI/MCE/debug), so Nitrox's 16 KiB is carrying strictly more than the
number it is matched against.

Measurement (2026-07-29, via the stack watermark below): the deepest any kernel stack has
gone during a full integration boot is **6264 B of 16384 — 38%**, leaving ~10 KiB for a
nested interrupt on top of the deepest syscall. That figure is only that comfortable
*because* measuring it immediately found 4 KiB of avoidable stack in the IPC paths — the
first reading was 58%, and 83% once a fourth `KBox::try_new(StoredMsg::zeroed())` landed on
the sync path, each of those materialising a 4 KiB message in the caller's frame before
copying it to the heap. With those converted to `KBox::try_new_zeroed`, the headroom is
real rather than notional. The guard page turns an overflow into a loud fault rather than
corruption either way. The fix here is a **per-CPU** IRQ stack, which
costs 16 KiB × CPUs rather than × threads — much cheaper than growing every thread's stack,
and it is what the precedent actually is. Trigger: the watermark climbing further (device
drivers with real interrupt work are the likely cause — the current device set is thin), or
the first guard-page fault.

**Attributing the watermark to a call path — `TODO(stack-attribution)`.** The watermark
records how deep and *whose* thread, and that was enough to learn something: the pid varies
run to run while the depth is bit-for-bit constant, so the deep path is something every
spawned process does rather than one pathological thread. It does not record *where*, which
is the next question and needs a return-address capture at the moment the record is set.
Trigger: wanting to actually reduce the figure above, rather than just watch it.

**Resource-server fan-out beyond the `sys_wait` width — `TODO(server-fanout)`.** A server
that holds a channel per client waits on its serving endpoint plus one slot per client, so
`MAX_WAIT_HANDLES` is the number of clients it can serve at once — for *every* server, not
one of them. Slice C3 (2026-07-29) raised it 8 → 32, taking both fan-out servers
(`fs-server-ext4`'s directory sessions, `logging-service`'s per-principal sources) from 7
concurrent clients to 31, and made both derive their cap from the constant rather than
restate it. That is a bigger number, not a different shape, and three things are unchanged:
the ceiling is still fixed; a client that opens a session and then stalls pins a slot for
as long as it lives, which no cap fixes; and the kernel arrays are on the **kernel stack**,
so the limit cannot simply keep growing (a compile-time budget check in `thread.rs` fails
the build at roughly a quarter of the 16 KiB stack).

The shape that removes the limit is a **readiness mechanism**: register a channel so that
becoming readable posts a notification carrying a server-chosen key, and the server waits
on **one** handle — its notification queue — regardless of client count. The notification
queue already exists and is the architecture's answer to async events, so this is a natural
extension rather than a new concept. The sharp edge is that the queue drops on overflow
(`KIND_NOTIFICATIONS_DROPPED`), and a dropped readiness wakeup is a permanently stuck
client unless the server treats that notification as "rescan everything" — which means the
mechanism has to be designed with the fallback, not have it bolted on. Trigger: a server
that genuinely needs more than ~31 concurrent clients, or the first time a stalled client
starving others is observed rather than theorised — most likely the desktop, where a
compositor holds a channel per window rather than per short-lived listing.

**Per-page dirty tracking — `TODO(page-dirty-tracking)`.** `FileObject::writeback` flushes
every *resident* page, not every *modified* one — the kernel keeps no per-page dirty bit,
so it cannot tell the difference. Two consequences, both currently benign. Writeback does
more device I/O than it needs to on a file that was mostly read. And `File::Touch`
(Slice C4) therefore stamps `mtime` on **sync**, not on write: a caller that maps a file
`MAP_WRITE`, changes nothing, and syncs anyway will move the timestamp. In practice the two
coincide — `sys_file_sync` requires `MAP_WRITE` and callers reach for it precisely after
writing — so this is a fidelity gap, not a wrong answer. The fix is to harvest the PTE
dirty bits (and re-protect pages read-only after each writeback so re-dirtying is
observable), which is the same machinery a periodic writeback daemon needs. Trigger: a
writeback daemon, a file large enough that flushing clean pages costs real time, or a
consumer that actually depends on `mtime` meaning "content changed".

**Per-mount write authority in a namespace binding — `TODO(mount-write-authority)`.** A
namespace binding to a userspace filesystem carries the rights of the **endpoint handle**
it was bound with (`sys_ns_bind` takes them from the target), so they describe the IPC
channel, not the files behind it — which is why the forwarding path in `sys_ns_lookup`
ignores `binding_rights` entirely. The consequence: **no mutating resolve is rights-checked
by the kernel.** `RESOLVE_CREATE`/`GROW`/`TRUNCATE`/`RENAME` all gate on nothing but
`LOOKUP` on the namespace handle, so a process that can resolve a path in a mount can
write it. Today that is contained by *namespace construction* — a sandbox that must not
write a filesystem is not given a binding to it at all, which is the architecture's
intended mechanism (`docs/architecture/namespace-and-resource-servers.md`) — so this is a
missing *refinement*, not an open hole. It surfaced writing `sys_file_rename` (2026-07-29),
where an attempt to check `MAP_WRITE` on both bindings failed against a live mount because
the bits are not what the name suggests. Doing it properly means deciding where per-mount
authority lives: rights on the binding independent of the endpoint handle's, a read-only
mount flag, or an explicit attenuation at bind time. Trigger: a read-only mount of a
writable filesystem — the first one is likely a sandboxed profile or a shared `/store`.

**Per-stage attribution in `PipelineStatus` — `TODO(pipeline-stage-attribution)`.**
§1 makes `PipelineStatus` a headline: a composite exit status with one `StageStatus` per stage, in
pipeline order, so a script can ask *which* stage failed rather than making do with a bash-style
scalar. `nxsh` builds it (Milestone 3 Part C) and it is exact for a **one-stage** chain, which is
every pipeline the standard coreutils can form. For a longer chain the per-stage entries are not
trustworthy, and the reason is an ABI gap rather than an implementation shortcut.

`sys_process_spawn` returns a **handle**. `ChildExited` carries a **ProcessId**. Nothing maps one
to the other, and `sys_wait` does not accept a process handle (`Timer`, `NotificationChannel`,
`IpcChannel`, `PendingOperation`, `InterruptObject` only). So a shell holding four child handles and
receiving four exit notifications cannot say which notification belongs to which stage.

The shell therefore reports the aggregate truthfully — a failure anywhere fails the pipeline, which
is what §1's fail-loud default actually needs — and does not claim per-stage attribution it cannot
support. An incomplete report beats a confidently wrong one.

Fixing it is small but it is an **ABI change**: either a `pid` field on `HandleInfo` for a process
handle, or a handle alongside the pid in the `ChildExited` body. Making `sys_wait` accept a process
handle would also do it and is arguably the better shape, since it lets a supervisor await a
specific child rather than draining a queue — but it is a bigger change to the wait path.

Trigger: the first pipeline with more than one external stage that anyone wants a per-stage report
from. Not urgent — §10a dissolved every classic filter into an in-process generic operator, so with
the shipped programs a pipeline has at most one external stage — but it should be batched into the
next ABI pass rather than rediscovered.

**`try`/`catch` in expression position — `TODO(try-in-expression-position)`.**
§9a makes blocks expressions and §9f shows `try`/`catch` composing with `match`, so
`let msg = try { … } catch (e) { e.message }` reads as though it should work. It does not:
`try` is parsed as a statement (§9c's `statement` production lists `try_stmt`), so it is
only usable at statement level, and a caught value reaches the outside through a `mut`.

The grammar as written is self-consistent — §9c really does put `try` among the statements
— so this is a gap between what the grammar says and what §9a's "blocks are expressions"
spirit implies, not a bug against the spec. A block *ending* in `try` already yields its
value, which covers the function-return case §9a actually argues for.

Trigger: the first script that wants to bind a caught value directly. The fix is small —
add `try` to `primary` alongside `if` and `match`, which are already expressions there —
but it is a grammar change and belongs with a considered pass over §9c rather than as a
side effect of Part E.


**A profile's contents are fixed at server startup — `TODO(profile-generation-refresh)`.**
`profile-server` reads its manifest once, at startup, from the initramfs, and never looks
again. Installing a package therefore cannot become visible to a running session: the new
generation's manifest exists on disk, and every process already holding `/bin` keeps
resolving against the old package list until the server is restarted — which in practice
means logging out and back in.

The intended behaviour is the opposite: install a package, bump the profile's generation,
and see it in `/bin` **without** logout/login. Nothing needs it today, so it is not built.

Two halves, and the second is the harder one:

- **The index.** Whatever readdir caches must be *rebuildable and swappable* rather than
  scattered through the resolve path, and must carry the generation it was built from. This
  costs nothing to honour now and is why the readdir work keeps a single owned index serving
  both resolve and readdir.
- **The trigger.** The server has to be *told*. It holds no `BIND_NAMESPACE` and should not
  be watching the filesystem for manifest changes — that would be a resource server
  reaching for authority over its own registration, which is the thing
  `why-supervisor-registration.md` refuses. So the shape is a control op on its endpoint,
  sent by whichever supervisor installed the package. That in turn implies the profile
  server keeps a control channel past startup, which today it does not.

Note the store's immutability does **not** make this stale-cache problem: a package's
contents can never change under a fixed store path, so a cached listing for a given package
is correct forever. What changes is *which packages the profile names* — membership, not
contents.

Trigger: a package manager that can install at runtime, or the first time a user has to log
out to see a program they just installed.
**A crashed fs-server takes the system with it — `TODO(fs-server-restart)`.**
`init` spawns `fs-server-ext4` once and never retains it for supervision, so nothing notices
if it dies and nothing restarts it. Every process holding a forwarded endpoint or an open
directory session would see `PeerClosed` and have no way back.

The **root** fs-server has a bootstrap constraint that is easy to get wrong: its restart
image can only ever come from the initramfs. `/store/<hash>-system-…/bin/fs-server-ext4`
exists and is the right thing for a *non-root* mount, but it is unreadable without the very
server being restarted. That is why the boot image keeps a copy even though the store has
one, and why the initramfs must stay resident (nothing releases it today —
`sys_release_initramfs` is referenced in the docs but does not exist).

Restarting the server is the smaller half. The harder half is that a remount invalidates
every handle derived from the old one: forwarded endpoints, open sessions, mapped file
pages in the page cache. Clients would need to re-resolve, and nothing in the RS protocol
says how. Until that is designed, a restart would hand processes stale handles that fail in
new ways.

Trigger: a second filesystem (where the non-root case is real and the blast radius is
small), or the first time an fs-server crash is observed in practice.

**Shell output bypasses the namespace — `TODO(tty-server)`.**
`/dev/console` is a char `DeviceNode` a process must hold a handle to, so console *input* is a
capability. Output is not: `CharBackend` has only `submit_read`, and every byte any program
prints goes through `SYS_DEBUG_KPRINT`, an ambient debug syscall taking no handle. A process
with an empty namespace can still write to the console, and nothing can redirect, pipe,
capture or log a shell's output because there is no object to redirect.

Three further consequences of the same gap: line editing is implemented separately in
`eshell`, `session-mgr` and `nxsh` (and has already diverged — the `alicepassword:` prompt bug
came from two of them disagreeing about echoing CR/LF); password entry is echo-suppression by a
parameter each caller must remember; and the driver's single-reader assumption is now
maintained only by session-mgr and nxsh happening not to read at the same time.

Designed 2026-08-03 in [console-and-tty.md](../architecture/console-and-tty.md): a userspace
resource server owning the line discipline and the raw device, handing each session an IPC
channel bound at `/dev/tty`, so a session cannot reach `/dev/console` at all. Deliberately
excludes job control (needs process groups, which do not exist, and cannot use signals), key
events (need a real keyboard driver), and terminal emulation (belongs to the compositor).

Trigger: it gates the rich REPL (§11) and is the trigger for
`TODO(session-metadata-server)`; and the ambient output path is a hole in the capability story
independent of either.

**Read-write FAT.** Initial FAT support is read-only. The ESP rarely changes after install; reading it is sufficient. Trigger: a need to update the bootloader from within the OS, or some other ESP-write workflow.

**Bulk directory creation is O(N²) block reads.** `dir_insert` scans every existing block
of a directory for a record with enough slack before appending a new block, and the server
caches nothing between calls — so filling a directory with N entries re-reads its blocks N
times. Fine at the scales anything currently does (16 entries costs ~2 s of a TCG boot),
but it is quadratic: a sweep of 40 entries plus a lookup per entry pushed a `test-qemu` run
past its 90 s ceiling (2026-07-29), which is what sized the in-guest check down to
crossing the block boundary rather than stressing it. The fixes are the standard ones —
an htree directory index, or simply a per-directory "first block with room" hint carried
across inserts. Trigger: bulk directory creation on a real path — `copy -r` of a large
tree, or unpacking a package into the store.

**ext4 write-path gaps (`fs-server-ext4`).** The write path is deliberately partial, and
these are the pieces still missing. They were previously recorded only in the crate's
`CLAUDE.md` — which is precisely how three other deferrals went unreviewed (the 2026-07-24
audit) — so they are mirrored here.

- **Extent-tree splitting / index nodes (depth > 0).** `i_block` holds four inline leaf
  extents; a file or directory needing a fifth non-contiguous extent gets `Unsupported`.
  Measured boundary on 4 KiB blocks (2026-07-29): creating **files** in one directory is
  unbounded in practice (2000+ tested — the parent's growth blocks stay contiguous, so one
  extent covers them), while creating **subdirectories** stops at **~814**, because each
  `mkdir` allocates the child's own block between the parent's and so fragments it. Both
  are far past anything the shell or desktop needs. Trigger: a directory or file that
  genuinely needs a deeper tree — very large files, or a directory of thousands of
  subdirectories.
- **Cross-group inode/block allocation.** Creation is group 0 only. Trigger: a filesystem
  large or full enough that group 0 runs out.
- **`metadata_csum` checksums** and **jbd2 journaling + replay.** The fixtures are built
  `^has_journal`; a crash mid-mutation is not recoverable by replay. Trigger: running on
  media where an unclean shutdown matters.

**btrfs, NTFS, XFS, ZFS, etc.** Each is a userspace fs-server binary. None are in initial scope. Trigger: specific deployment needs.

**Encrypted root (LUKS).** Architecture accommodates this — LUKS is a block device filter driver in initramfs; init invokes it before spawning fs-server. Not in initial scope. Trigger: encrypted-root deployment.

**LVM / software RAID at early boot.** Same architectural accommodation as LUKS. Initial scope is direct partition mounts.

**`mtime` on an in-place overwrite.** The fs-server stamps inodes on create, grow, mkdir,
unlink, rmdir and rename — every operation that passes through it. It is **not** told about
a plain overwrite: under Model A the kernel writes file data straight to the device, so
editing a file's existing bytes leaves its `mtime` at the last size change. This is the
timestamp gap users actually notice ("I saved the file and the date didn't change"). Needs
a writeback notification to the server — the natural hook is `sys_file_sync`, which already
crosses into the kernel with the file's identity in hand, though a notification per sync
has its own cost to weigh. Trigger: a text editor, or anything that rewrites a file in
place without changing its length.

**Runtime reconfiguration of critical-path mounts.** Currently requires reboot through eshell. Live remounting of `/`, `/home`, etc., is not supported. Trigger: deployment scenarios where it matters.

### Userspace

**A resource server behind `/session/*` — `TODO(session-metadata-server)`.** Nitrox has no kernel
user identity: authority is held in handles, so there is nothing for the kernel to report and
identity is a *session* concept. `session-mgr` authenticates a login and then constructs the
session's namespace, so it is both the component that knows and the component that already tells a
process about its world by construction — the shell does not ask where home is, it sees `/home`.
`whoami` therefore reads `/session/user`, a binding, rather than calling a service (which would be
closer to ambient lookup than to capabilities) or a syscall (which has nothing to return).

**Today that binding is a direct handle to a memory object — a snapshot.** That is correct for a
fact immutable for the session's lifetime, and a session's user qualifies: changing user means a new
session, not a mutated one. It is *silently wrong* for anything mutable, because a client would read
stale bytes with no indication that they are stale. Hence the rule, which is a checkable condition
rather than a judgement call:

> Direct-handle binds are for facts immutable for the session. **The first genuinely mutable
> `/session/*` member is the trigger** to put a resource server behind the prefix.

**The migration does not touch any client.** A userspace-server binding answers a resolve with
`OBJECT_KIND_MEMOBJ`, which the kernel cross-context-installs — so `lookup + map + read` is
byte-identical whether the path is a direct handle or a server. The namespace exists precisely to
hide that difference (it is the same reason `/home` can be a server subtree while `/dev/console` is a
direct handle, and no client cares). The cost of the migration falls entirely on `session-mgr`.

**Watch the coupling with B3 (env), which arrives first.** The shell subproject's open question B3 —
*"env vars as namespace-scoped resources"* — is the same problem one milestone earlier: mutable,
namespace-scoped values, due in Milestone 3 when the interpreter needs env. Whatever mechanism B3
builds for that is very likely what `/session/*` should migrate onto, rather than a bespoke session
server. Designing B3 without `/session/*` in view risks two mechanisms for one problem and two
migrations instead of one.

Expected timing, from the roadmap: the *hard* trigger is the console/tty server and the compositor
terminal (stepping-stones 5-6), where a tty and job control are mutable session state with several
consumers needing one consistent answer — the rich REPL is already recorded as gated on exactly
that. So the snapshot has roughly two milestones of runway, and B3 in Milestone 3 is the design
checkpoint.



**Where a Tier-0 program's output goes — `TODO(tier0-output-sink)`.** A coreutil spawned without a
shell has no `stdout` (stream handles arrive in the setup message, which the shell sends), so it
currently falls back to plain text via `kprint` — i.e. into the **kernel log**. That is fine as
debugging scaffolding and wrong as a design: `kprint` is a kernel *diagnostic* facility, the klog
is a bounded ring (8 KiB boot prefix + 8 KiB recent, Slice D1), and program output written there
**evicts kernel diagnostics**. It is also a layering inversion — a userspace program's normal
output should not travel through a kernel debug path.

Two candidate shapes, and the choice is about whose job it is to supply a sink:

1. **The program falls back to `/dev/console`** instead of `kprint`, opening it through its own
   namespace. Minimal change, keeps "no stdout" as a state programs handle, and the machinery
   already exists — the console is a kernel server bound at `/dev/console` in the root namespace,
   and session-mgr already re-binds it into session namespaces.
2. **The spawner always supplies streams**, so Tier 0 stops meaning "no streams" and starts meaning
   "streams the spawner chose". Init would construct a minimal setup message with `stdout` bound to
   a console handle (or explicitly to nothing, meaning *discard*). This removes the fallback path
   entirely rather than redirecting it, leaving one output path instead of two — at the cost of
   giving every spawner that job.

**Option 2, on the Linux precedent (checked 2026-07-30, empirically and against the kernel source).**
Linux coreutils have **no fallback at all**. `ls` with fd 1 closed prints
`ls: write error: Bad file descriptor` and exits 2 — the same when exec'd directly with 0/1/2 closed
and no shell involved. The program writes to fd 1; if it is not open, `write()` returns `EBADF` and
the program fails loudly. It never invents a destination.

The guarantee lives entirely outside the program, in four places: the kernel opens `/dev/console`
and dups it to 0/1/2 for **PID 1** (`console_on_rootfs()` in `init/main.c`); every other process
**inherits** 0/1/2 across fork/exec; detaching daemons explicitly redirect to `/dev/null`; and for a
**setuid** exec the kernel plugs `/dev/null` into any unallocated 0/1/2. That last one is a security
measure rather than a convenience — a setuid program that opens a file would otherwise be handed
fd 1 and could be tricked into writing to it — and note it plugs `/dev/null`, not a console: the
kernel ensures the descriptor *exists* and never decides the output should be seen.

So the precedent endorses option 2, and pushes it further than "fall back to something better": the
end state is that a stage **has no fallback** and fails when it has no `stdout`, exactly as `ls`
does. "Discard" stays expressible as an explicit `/dev/null` equivalent, which is also the daemon
idiom.

**The one place the analogy breaks, and the actual cost of option 2 here.** Linux gets "the spawner
supplies" almost free, because descriptors are *inherited* — a spawner that does nothing still
passes its own along. Nitrox has no inheritance: authority is explicit handles and a spawned process
gets exactly what the setup message carries. So what Linux obtains by default, init and every future
spawner must do deliberately. That, rather than the choice of sink, is the part to design.

Trigger: raised 2026-07-30 while planning coreutils Milestone 2 — acceptable while init and the test
harness are the only spawners, but it should not survive the shell (Milestone 3), which is the point
at which Tier 0 stops being the common case and the current behaviour would start filling the klog
during ordinary use.

**Shell grammar specification.** The shell's data model is committed (typed structured streams, port-based wiring, the display verb, model-view decomposition). The exact syntax is deferred to shell implementation. Trigger: when shell implementation begins.

**`std` port for Nitrox target (now a Phase 4 target, 2026-07-20).** Reframed from "deferred
indefinitely" to a serious, faithful compatibility target — the portable API for *application*
code, riding the native ABI (libos/libstream stay the capability-native API for system code).
`std::fs` resolves paths through the process's root namespace (bounded ambient, capability-safe);
`std::io` blocking maps to `sys_io_submit` + `block_on`; the kernel stays pure. Placement: **FP/AVX2
+ XSAVE lands early** (also unblocks `no_std + alloc` ecosystem crates); the **full cluster**
(thread-local storage, real `std::thread` → the slice-3b deschedule IPI, the `std::{fs,io,sync,thread}`
subset, `x86_64-unknown-nitrox.json`) is **consumer-driven** — it lands with portable programs / the
browser, not as a desktop-MVP gate. See the decision log (2026-07-20; supersedes 2026-07-13).

**Dynamic linking.** Everything is a **static, non-PIE `ET_EXEC`** today: the kernel ELF
loader rejects `ET_DYN` and `PT_INTERP` outright, and each program carries its own copy of
everything it uses. At present that is *right* — the shipping binaries are 13–73 KB
(`no_std` + `alloc`, hand-rolled runtime), so a loader would cost complexity and save
nothing.

**It stops being right at the GUI toolkit**, for a reason worth stating precisely: static
linking defeats page sharing exactly where sharing would pay. Shared file-backed text
(planned as B4a) shares pages across *instances of one program*, because it maps the same
`FileObject`. Two different statically-linked apps that both embed the toolkit hold
byte-identical code in **different files**, so they share nothing — not through B4a, not
through CoW, and not through the content-addressed store, which dedupes whole files. Five
apps each embedding a 2–5 MB toolkit is 10–25 MB of duplicated, unshareable text on a
256 MB machine. Dynamic linking is what extends B4a's sharing *across* programs; B4a is
what makes a loaded library shareable at all (without file-backed mapping, loading a `.so`
would eagerly copy it per process — the complexity with none of the benefit).

What it needs, roughly in order:

1. **Thread-local storage first.** The dynamic TLS models need `FS_BASE` /
   `sys_thread_set_tls`, which is itself deferred in the `std` cluster. Hard prerequisite.
2. **Kernel: accept `ET_DYN` + `PT_INTERP`** — map the interpreter and enter it instead of
   the program. User-space ASLR already supplies the base randomization PIE wants (28 bits
   for ELF/stack/mmap).
3. **A userspace `ld.so`**: map segments (over B4a's file-backed path), walk the dependency
   graph, process relocations (`RELATIVE` / `GLOB_DAT` / `JUMP_SLOT`), resolve symbols, run
   init/fini arrays.
4. **An ABI answer, because Rust has no stable ABI.** Two viable shapes: a **C ABI seam**
   at the library boundary (`extern "C"`, `#[no_mangle]`) — stable across compiler versions
   but awkward for Rust-to-Rust calls; or **whole-system build coherence** — one pinned
   compiler, everything in a generation rebuilt together, Rust ABI used freely within it.
   The **content-addressed store + generations makes the second genuinely viable here** in a
   way it is not on a conventional distro: a generation already *is* a coherent closure, and
   a toolchain bump is just a new generation. Recommended shape: coherence as the default,
   with a deliberate C seam only where a plugin boundary needs to outlive a rebuild.

The store and profile layers already anticipate this — `content-addressed-store.md` reserves
`lib/<library>` and `profiles-and-namespace-projection.md` designs `/lib`, both explicitly
"once dynamic linking exists".

**Scheduled with the GUI toolkit milestone**, alongside the process-memory-model bundle.
Sequencing note: do not build the loader speculatively, but **decide the toolkit's ABI seam
when the toolkit is designed** — retrofitting three shipped apps is far worse than starting
them right. Natural trigger: the second or third GUI app, i.e. the point where one library
would be resident more than once.

**POSIX compatibility shim.** Optional future. Translates POSIX calls to handle-based equivalents. Enables ported C software without native rewrites. Not a design constraint; the native interface design doesn't bend to accommodate POSIX. Trigger: a must-have C dependency (target the pure-Rust ecosystem first — see the 2026-07-20 std stance).

### Resource servers (in-kernel)

**`/dev` directory stub (enumerable placeholder).** Slice 5 gives `DeviceNode` a
real kernel struct (PCI-discovered nodes; block disks resolve via
`KernelServerId::BlockDevice` at `/dev/blk`), but there is still **no enumeration
syscall** (`ENUMERATE` is defined but unused) and **no listable `/dev` directory**
— lookups resolve a known path to a node; nothing enumerates the children of
`/dev`. A directory-listing surface was deferred until a device manager or a real enumeration
consumer existed. **The consumer now exists**: `list` (coreutils Milestone 1) makes
`list /dev` a day-one shell command, and `sys_ns_enumerate` is built but has no user. The
open design question is how a listing tool chooses between *namespace enumeration* (what
`/dev` needs — it is kernel-served) and an *fs-server directory session* (what `list` uses
today). Scheduled as **D3 of the pre-CLI substrate-hardening pass**
(`docs/planning/phase-4-desktop.md`). See the decision log (2026-06-22, 2026-06-23).

### Runtime libraries

**TypedRecord support for enums.** The `#[derive(TypedRecord)]` macro initially supports primitive scalars, `String`, `Vec<T>` of TypedRecord, nested structs, `Option<T>`, and `RawHandle`. Enums (tagged unions) are deferred; they require wire-format extensions and more complex derive code. Trigger: a concrete need; not foreseen as urgent.

**TypedRecord support for generics beyond `Vec<T>`.** Same reasoning. Deferred until a concrete need.

**Lifetimes beyond `'static` in TypedRecord.** Same.

**iovec-style scatter/gather user access.** All current copy primitives operate on contiguous buffers. Scatter/gather (vectored I/O equivalents) isn't initially needed. Trigger: a syscall whose performance benefits from it.

**Per-thread CPU accounting (`ProcessCpu` / `ThreadCpu` clocks) — `TODO(sched-acct)`.**
`sys_clock_read` services `Monotonic` and `Realtime`; the two CPU-time clocks return
`Unsupported` because the scheduler does not accumulate per-thread run time. The natural
home is the context switch (charge elapsed monotonic time to the outgoing thread) plus a
per-`Process` roll-up on reap. Trigger: profiling or accounting that needs CPU time rather
than wall time — a `time` builtin, per-process scheduler statistics beyond the existing
`/proc/sched/stats`, or rlimits (which need CPU-time accounting to enforce a CPU limit).

**vDSO-equivalent for `sys_clock_read`.** On modern Linux, `clock_gettime` is implemented in vDSO — userspace reads TSC directly, no syscall. Nitrox initially does one syscall per `sys_clock_read`. The API shape leaves room for this optimization later (the `ClockId` enum can map to fixed memory locations) without changing call sites.

### Concurrency primitives

**SMP panic path: unsynchronized emergency serial, no stop-IPI (review F8).** The
panic/exception handlers write through `serial::emergency_writer()` — lock-free by
design (a fault while `SERIAL` is held must not deadlock) — which was sound
single-CPU but under SMP can interleave with another CPU's locked serial writes
(garbled diagnostics, not corruption). And a panicking CPU does not stop the
others: no halt/stop IPI exists, so the other CPUs keep scheduling and mutating
state while the panic prints (and, under `test-harness`, while the verdict is
written — a fail verdict still terminates QEMU promptly, so adjudication is
unaffected). Fix shape when taken: a panic-broadcast NMI/IPI parking other CPUs
(`cli; hlt`), then unsynchronized output is genuinely exclusive. Deferred: purely
a diagnostics-quality issue today; revisit with real-hardware bring-up or when a
flaky-boot investigation is hampered by garbled panic output. From the 2026-07-21
substrate review (decision log).

**Explicit grace-tracker quiescence on the syscall path — `TODO(smp)`.** The handle
table's deferred-close reclamation waits for a grace period tracked per *context id*, and
`current_ctx_id()` returns **0 in production builds** — every CPU shares one context. Today
nothing depends on the distinction: every handle syscall routes through a `HandleTable`
method that takes and drops a read guard, marking the context quiescent on drop, so
deferred closes are reclaimed on the next allocate/close. The `TODO(smp)` marks the case
that would break it — a syscall path that touches the table *without* going through such a
method, or a real per-CPU context id where one CPU's quiescence no longer implies
another's. **Worth an explicit look during the pre-CLI hardening pass** rather than
carrying as a comment: the sweep added for exit-time handle reclamation (2026-07-24) is a
new writer on this path. Trigger: that review, a per-CPU `ctx_id`, or a non-method table
access.

**Priority inheritance for userspace synchronization.** Userspace mutex/condvar implementations built on `sys_wait` don't initially address priority inversion. Trigger: a real-time workload where priority inversion is a problem.

**Deadline scheduling (EDF) as a fourth scheduler class.** RealTime class uses fixed priority, not EDF. Adding EDF is possible without architectural changes — fourth scheduler class. Trigger: a workload that benefits.

### Memory management

**The process memory model — one pass, four parts.** These extend the same AS/fault
machinery and should land together rather than as four independent entries each waiting on
the others:

1. **Copy-on-write, private file-backed data.** `MappingKind` has only
   `Anonymous`/`FileBacked` — no private/CoW kind — so a writable `PT_LOAD` cannot be
   mapped from the image. Needs a CoW fault path plus refcounted shared frames.
2. **Lazy `MemoryObject` backing.** `sys_memory_create` allocates and zeroes **every**
   frame eagerly, which is why `MemoryObject::MAX_SIZE` (16 MiB) exists at all — a DoS
   guard, not a designed ceiling. The `#PF` half of the gate is already closed
   (`AddressSpace::fault_in` + `map_vma_lazy`, 2026-06-12); what remains is a sparse
   per-page frame table allocated on fault.
3. **Per-process resource limits (rlimits).** CPU time, process count, committed memory —
   none are bounded today. Lifting `MAX_SIZE` *requires* this: the cap is what currently
   stands in for accounting.
4. **User-stack guard page + grow-down stacks.** The loader reserves a fixed 4-page stack
   with **no guard page**, so an overflow runs into whatever VMA sits beneath it. Kernel
   thread stacks already have the discipline (vmap: 16 KiB + 1 guard page); this is the
   userspace counterpart.

**Shared read-only text is *not* in this bundle** — it needs no CoW (the existing
`FileBacked` kind suffices) and is scheduled as **B4a of the pre-CLI substrate-hardening
pass** (`docs/planning/phase-4-desktop.md`). One design constraint it exposes and this
bundle inherits: every resolve mints a **fresh `FileObject` with its own page cache**, so
sharing across instances requires the spawner to reuse one image handle per program (or,
later, inode-keyed global caching).

**Trigger for the bundle:** the GUI toolkit / desktop-apps milestone — several apps linking
one toolkit, with real `.data`/`.bss` and enough concurrent instances that private copies
and eager allocation start to matter — or, earlier, a profile showing spawn latency or RSS
bound. Without `fork`, CoW's only consumer is the ELF data segment, which after B4a is a
few KB per process; that is why this is scheduled rather than urgent.

**NUMA-aware scheduling and memory allocation.** Architecture does not preclude NUMA but does not exploit topology. Single buddy allocator zones, scheduler treats all CPUs as uniform, work stealing ignores topology. Trigger: NUMA hardware where the lack of awareness is producing measurable problems.

**Per-CPU slab caching.** Phase 1's slab allocator uses a single global spinlock per
cache. SLUB's per-CPU optimisation (a `current_slab` pointer per CPU, the cache lock taken
only on slow paths) is structurally compatible with the existing state machine. **Its
original trigger — "SMP bring-up introduces per-CPU areas" — has fired** (SMP landed in
Phase 3 and per-CPU areas exist), and it was not done, so this is now carried on a
performance trigger instead: allocator contention showing up in a profile, or a workload
where several CPUs allocate hard concurrently.

**Empty-slab reclamation back to the buddy.** Once a slab cache grows by one page, that page stays with the cache forever. Production kernels reclaim wholly-empty slabs after a watermark; Nitrox doesn't yet. Trigger: long-running workloads where slab churn produces visible memory bloat, or memory-pressure handling (the OOM daemon) needs a hook to drain caches.

**DMA / Normal zone split in the buddy.** The buddy treats every Usable frame above 1 MiB as a single pool — `DmaBuffer` returns whatever block the buddy gives, with no address-range constraint. A below-16 MiB (ISA-DMA) or below-4 GiB (32-bit-only PCI) zone would only matter for a device that **cannot** do 64-bit DMA, which the project's **no-legacy ≈2014 / x86-64-v2 baseline excludes** (modern AHCI advertises `CAP.S64A`); the dev loop's 256 MiB of RAM is sub-4 GiB regardless. Trigger: a real driver that genuinely needs an address-constrained zone (none foreseen). When it lands, `DmaBuffer::alloc` grows a max-physical-address (DMA-mask) parameter and the buddy a zoned free-list. See the `TODO:` comment in `kernel/src/mm/buddy.rs`.

**Partial / splitting `sys_memory_unmap` — `TODO(mm)`.** The syscall takes `(addr, size)`
but **ignores `size`**: it unmaps the whole VMA covering `addr`. So an unmap of part of a
mapping silently unmaps all of it — the caller asked for less and got more, with no error.
Every caller today maps and unmaps whole objects, so it has not bitten, but it is a
footgun rather than a limitation. Honouring `size` means splitting a VMA (and, for a
file-backed one, adjusting the cache-page range), which the VMA tree does not do yet.
Trigger: any caller that unmaps a sub-range — an allocator returning part of an arena, or a
`std`-style `munmap`. Until then the argument should arguably be rejected when it does not
cover the whole VMA, rather than ignored.

**Reclaiming empty intermediate page tables on unmap.** `ArchPaging::unmap_page` clears the leaf entry but leaves the PDPT/PD/PT frames it walked through allocated, even when an unmap empties one. Reclaiming them needs a per-table populated-entry count (or a 512-slot scan on every unmap). Phase 1 runs a single address space with little mapping churn, so the leak is negligible. See the `TODO:` comment in `kernel/src/arch/x86_64/paging.rs`. Trigger: address-space teardown (process exit) or `munmap`-heavy workloads make the retained tables a measurable cost.

**Range TLB invalidation (`flush_tlb_range`).** `ArchPaging` exposes `flush_tlb_page`
(one page) and `flush_tlb_all` (a CR3 reload), so a bulk unmapper issues one
`flush_tlb_page` per page. The **cross-CPU shootdown is built** (substrate hardening Parts
B/C, 2026-07-21: `tlb::shootdown`, IF-robust, broadcast on user-page unmap) — only the
*range* form is missing. Trigger: an unmap path whose per-page flush loop is measurable
(large `munmap`, address-space teardown).

**Debug-build lock-ordering enforcement.** `kernel/CLAUDE.md` documents that debug builds will track acquisition order and panic on violations. The mechanism doesn't yet exist; the only lock-ordering enforcement today is code review and `kernel/docs/lock-ordering.md`. Trigger: enough locks exist that the cost of building the rank-tracker outweighs the cost of a missed bug.

### Testing and CI

**`xtask test-qemu` integration harness.** ~~A QEMU integration test today would amount to a single "did the kernel reach the end of `kernel_main`?" smoke via `isa-debug-exit`.~~ **Implemented 2026-07-14** (Phase 3, after the storage/fs/SMP stack gave real regression surface — the SMP migration hazard was caught by a boot loop, not a unit test). `cargo xtask test-qemu` boots the `test-harness` build headless and adjudicates the whole boot from QEMU's `isa-debug-exit` exit code. The self-test payload (`selftest` feature) *is* the suite; a per-case framework under `tests/qemu-tests/` remains deferred (trigger: a test that needs to assert something the boot chain doesn't already exercise). See `docs/conventions/qemu-integration-tests.md`.

**Per-case QEMU test framework (`tests/qemu-tests/`).** `cargo xtask test-qemu` adjudicates
the *whole boot* from one `isa-debug-exit` verdict — the self-test payload is the suite, and
CI runs it (2026-07-24). A per-case framework, where an individual test asserts something
the boot chain does not already exercise, remains deferred. Trigger: a case that cannot be
expressed as "the boot completes and the harness agrees". See
`docs/conventions/qemu-integration-tests.md`.

**`libkern` mock-syscall test mode.** `userspace/libkern/CLAUDE.md` describes a feature-flagged mock that records and replays syscalls for host-side tests of layers above. The crate is a `cargo new` placeholder in Phase 0. Trigger: real syscalls are defined.

### Auditing and observability

**Comprehensive systemwide tracing infrastructure (DTrace/eBPF equivalent).** Per-CPU ring buffers for kernel tracing exist in concept. A full programmable tracing facility (DTrace probes, eBPF-style filters, etc.) is out of scope initially. Trigger: deep performance analysis needs that exceed what `kprintln!` and basic tracing handles.

## Resolved (kept for the record)

Entries that have been **done**, listed here rather than left in the open sections above —
scanning "what is still owed" has to be reliable. The reasoning for each lives in the
decision log entry for the date shown.

| What was deferred | Resolved | How |
|---|---|---|
| The shell's console loop has no automated cover (`nxsh-console-tests`) | 2026-08-03 | Resolved by testing the *whole* interactive path rather than extracting the loop. `cargo xtask test-interactive` boots the **release image** — which nothing else boots; `test-qemu` runs the `test-harness` build, where session-mgr auto-logs-in and runs a fixed script — and drives login, a wrong password, a shell prompt, a spawned program, a failing stage, cross-line interpreter state, and `exit` → log in again. Expect-driven, so the guest paces it. The rejected alternative was extracting the byte loop into the library half: the continuation *decision* is already host-tested, leaving only ~60 lines of stable byte handling, and a refactor of the critical interactive path is its own risk — while the two bugs it could never have caught (console-lookup rights, session wiring) are precisely the ones that hurt. Non-vacuity checked by reverting the login-echo fix: the run fails at `\npassword:`. |
| The initramfs carries more than boot needs (`initramfs-minimisation`) | 2026-08-03 | The boot image now carries only what is required to reach a mounted root: `init` (the kernel boot-loads it), `fs-server-ext4` (it *is* the mount, and is the only possible restart image for root), `eshell` (the recovery path *for a failed mount*, so it must not live on the filesystem it recovers), and `profile-server` — the one the original rule missed, because `/bin` does not exist until it runs. Everything else moved into a `system` store package and is spawned through `/bin` like any other program: service-mgr, session-mgr, auth-service, logging-service, heartbeat. 1,506,596 → 206,360 bytes, an 86% cut in memory held for the machine's uptime. The recovery path was verified deliberately rather than assumed — a forced mount failure still reaches `eshell>`. |
| Process teardown deferred wholesale (`exit-context-teardown`) | 2026-07-31 | `sys_process_exit` now closes the calling process's handle table in its own syscall context — Linux's `do_exit` → `exit_files` position: no locks held, free to allocate and drop. `exit_process` cannot, because it takes `SCHED` and never returns from `finish_exit`. Closing a handle is what other processes wait on (dropping an IPC endpoint's last reference nulls its peer and wakes whoever is blocked there), so this moves `PeerClosed` from "the reaper's next turn" to the moment of exit. The reaper still queues and closes the pid afterwards, finding an empty table: that path stays for processes killed externally, which never run the syscall. Measured rather than assumed — `exit_closed=` in `/proc/sched/stats`, 421 handles per self-test boot against 89 pids the reaper now finds already empty. |
| No regression cover for reclamation starvation (`idle-starvation-test`) | 2026-07-31 | Built, after the first attempt was thrown away for passing with the bug reinstated. The failed version asserted a low **switch rate**; a lone spinner is preempted back to itself, which barely moves that counter. The working version asserts **occupancy** — `/proc/sched/stats` reports `idle=` per CPU, and a spin permanently costs one. Sampled ten times across a second, taking the best sample (one busy instant is not a spin), and compared against `cpus_online - 1` because the sampling thread's own CPU never reads idle. Verified both ways: 3 of 4 CPUs idle with the fix on three consecutive runs, 2 of 4 and a failed run with the fix disabled. |
| A login session can see no programs (`session-program-namespace`) | 2026-07-31 | Both halves, because either alone is inert. The store gained a `coreutils` package (the ten coreutils plus `nxsh`) and the system profile a second `[[package]]` — before that `/bin` was bound and projected exactly `heartbeat`. Then init retains the profile server's endpoint and hands it down to session-mgr, which binds it at `/bin` in each session namespace, sharing init's registration exactly as `/home` shares the fs-server's. Binding `/initramfs/sbin` was the quick alternative and was refused: it hands a session the boot image instead of a profile. Handing a *second* endpoint down needed a channel — only `handles[0]` reaches a child — so service-mgr's `rdx` is now a handoff channel rather than one endpoint. |
| `nxsh` could not parse `list /` | 2026-07-31 | Found by typing it at a real prompt, one command after the `/bin` bind made programs reachable. A lone `/` after a command head was read as division; `/system` was never affected because it lexes as a single path word, which is why every test passed. A lone `Slash` in argument position means the next thing is whitespace or a closer, so there is no right operand and division is impossible — the root path is the only reading left. |
| Cross-mount `move` of a directory, and the missing second mount (`cross-mount-move`) | 2026-07-30 | The blocker was the fixture, not the feature: with one writable mount, no cross-mount move could be exercised end to end at all, so the recursive case was refused rather than written blind. Binding the one fs-server a second time with base `/scratch` gives a destination the kernel classifies as another mount while staying writable — the kernel's rename test is `same server && same subtree base`. The recursive case then landed on shared `fs::copy_tree`/`fs::remove_tree` walks, hoisted out of `copy` and `remove` so there is one of each rather than three. |
| `cd` as a shell-state builtin (`shell-cwd`) | 2026-07-31 | Milestone 3.5. The answer was not a shell-side string: the kernel gives every child a **LOOKUP-only** namespace handle unconditionally, so no non-supervisor can rebind its own root, and `cd`-as-rebinding was never possible. `PWD` is a conventional entry in the environment `Record`, carried on the Tier-1 setup message; relative paths are expanded by `librsproto::path::resolve` before any syscall, and the kernel still refuses `.`/`..` by name. The shell does not rewrite a spawned stage's arguments — it hands over the same `PWD`, so both sides resolve identically. **Closing the tag is what found the rest of it:** `check-deferrals` failed on two `TODO(shell-cwd)` markers still in `nxsh`'s REPL loop, guarding a hardcoded refusal of `cd` that predated the implementation. Scripts called `run_line` and worked; the interactive path never reached it, so `cd` at a prompt answered "`cd` is not implemented" while `cd` in a script changed directory. Driving it after the deletion then found `cd` refused every *binding* — `cd /` and `cd /bin` both — because `Host::exists` knew only two of the three ways a namespace path can be real: it resolves to an object, or a directory session opens it. The third is that it names a binding or sits above one, which is what `list` walks (`SYS_NS_ENUMERATE`) and what makes `/` and `/bin` visible in the first place. `cd` now asks the namespace the same question `list` does. |
| Filesystem errors collapsed into `InvalidArgument` (`fs-error-granularity`) | 2026-07-30 | `KError` gained `AlreadyExists` (-14) and `NotEmpty` (-15), and the batched ABI pass found the collapse was not the fs-server's alone: `sys_ns_bind` on an occupied path had the identical one, which is what makes these kernel errors rather than filesystem ones. Three further arms of `fs_kerror` were also reaching for a vaguer error than existed — `TooLarge`→`OutOfMemory`, `Io`→`KernelError`. Separately, `libkern`'s `from_i32` had never decoded `IoError`, so every device error read as `KernelError`; `abi-sync-check` now derives the decode table from the kernel's enum. See [error-codes.md](../reference/error-codes.md). |
| `cargo xtask abi-sync-check` | 2026-07-29 | Built (Slice D2) and wired into CI. Compares the four hand-mirrored ABI families — syscall numbers, `KError`/`KObjectType` discriminants, `Rights` bits — plus individually-paired shared limits (`MAX_WAIT_HANDLES`, `IPC_HANDLE_MAX`), 91 values in all. `#[repr(C)]` layouts stay out of it: both sides already assert their own offsets and sizes at compile time, which is stronger and fails earlier. |
| x2APIC | 2026-06-26 | Built — and **x2APIC-only**, not dual-mode: the ≈2014 baseline guarantees it, so no xAPIC fallback is carried. The dev loop runs QEMU ≥ 9.0. |
| Concurrent direct-block + forwarded-lookup hang | 2026-07-20 | Not the block/forwarding path — a missing cross-CPU wake; fixed by the reschedule IPI. |
| Writeback IRPs | Phase 3 | Dirty-page writeback landed with read-write `fs-server-ext4` (`FileObject::writeback` / `sys_file_sync`). |
| File-size discovery via `sys_handle_stat` | 2026-06-27 | `HandleInfo` gained `size: u64`; the lazy resolve grants `INSPECT`. |
| Forwarded-lookup concurrency (N = 1) | Phase 2/3 | `US_PENDING_MAX = 8` outstanding forwarded lookups, correlated by `request_id`. |
| File truncate | 2026-07-24 | `sys_file_truncate` → `RESOLVE_TRUNCATE` → `ext4::truncate_file`, kernel-forwarded so the page cache stays coherent. |
| Reclaiming a process's handles at exit | 2026-07-24 | Marked thread + a batched sweep in `reap_pending`; `next_owned` stays unbuilt (the sweep scans). |
| Wall-clock time | 2026-07-24 | `CLOCK_REALTIME` anchored from the CMOS RTC at boot; the fs-server stamps inodes. Setting the clock stays unbuilt — see the open entry. |
| Numeric `/proc/self/{pid,tid}` | Phase 3 | The capture → format → synthesize primitive landed with `/proc/sched/stats`; `/proc/self/status` was its second consumer. |
| General deferred object reclamation from `SCHED`/IRQ context | 2026-07-21 | `SchedState::deferred_drops`, drained by `reap_pending` in thread context (review fix F2). |
| `kmalloc` alignment > `SLAB_SIZE` | 2026-06-12 | Resolved by taking the other path: `mm::dma::DmaBuffer` allocates from the buddy. No remaining client wants it from `kmalloc`. |
| Kernel host-side unit tests | Phase 1 | The kernel is `lib + bin`; `cargo xtask test` runs the host suite (780+ tests across kernel and userspace). |
| `cargo xtask test` subcommand | Phase 1 | Exists and runs the whole host suite. |
| fs-server block I/O in 4 KiB blocks | 2026-07-23 | `DiskReader`'s transfer unit is the 4 KiB block — ~8× fewer device round trips. |
| Image assembly + QEMU smoke in CI | 2026-07-24 | A second CI job runs `xtask test-qemu` (which builds the image), with OVMF/gdisk/mtools/e2fsprogs installed. It had been deferred "until there is meaningful regression surface"; every Milestone 1 regression was caught by this gate and none would have failed the other jobs. |
| Per-interrupt-context lock-order tracking | 2026-07-29 | `lockrank::enter_interrupt` gives every interrupt handler a fresh view of the held-rank stack, so the order restarts at an interrupt boundary as it actually does. This was a *prerequisite* for the tracker, not a refinement — flat ranking panicked about one boot in three. `tlb::LOCK` is now ranked normally and needs no exemption; `cargo xtask check-irq-scope` keeps every entry stub scoped. |
| `xtask test-qemu` integration harness | 2026-07-14 | Boots the `test-harness` build headless and adjudicates from `isa-debug-exit`. A per-case framework under `tests/qemu-tests/` is still open (below). |

## How to use this document

When you encounter something that seems unimplemented or absent, check this document first. If it's listed here, the absence is intentional; the reasoning is preserved. If it's not listed here and you think it should be, consider adding an entry — the document is append-only-with-revisions, not a static snapshot.

If you're triggering a deferred item (starting work on TCP/IP, beginning aarch64 port, etc.), update this document at the same time. The deferred entry should **move to the Resolved table**, not be left in place with a status note appended — an open section that mixes finished work with owed work cannot be scanned, which is how three deferrals went unnoticed until a consumer tripped over them (the 2026-07-24 audit).

**A deferral only exists if it is in this document.** The three gaps that cost the most to
rediscover — exit-time handle reclamation, the wall clock, file truncate — were each
"recorded" somewhere else: a sentence in an architecture doc promising a later slice, a
`TODO(...)` in the syscall table, a bullet in a crate's `CLAUDE.md`. None was in this list,
so none was ever reviewed. If you write a stub, a `TODO`, or prose that promises future
work, mirror it here. `cargo xtask check-deferrals` enforces the `TODO(tag)` half of that
mechanically.

The decision log (`history/decision-log.md`) is the place to record the actual decision when a deferred item moves into active work — what triggered it, what the implementation approach is, when the decision was made.
