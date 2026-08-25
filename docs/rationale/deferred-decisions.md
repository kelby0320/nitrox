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

**Re-measured 2026-08-11**, at the second scheduled revisit — and the first thing it found
is that **the trigger had already fired unobserved**. An adjudicated boot on the tree as it
stood *before* this change performs **2,778 fills**, not 43: the 2026-07-29 figure was taken
when nearly every program was initramfs-resident, and the coreutils/store work of 2026-08-03
moved the services and the coreutils onto ext4 without anyone re-reading the counters. The
scheduled revisit is what caught it, which is the argument for scheduling them.

| | before (2026-08-11) | after |
|---|---|---|
| page-cache fills | 2,778 | 3,021 |
| fill time | 307 ms, 3.8 % of boot | 466 ms, 5.2 % of boot |
| spawns | 105 | 105 |
| image materialise | 616 ms for 12.33 MB, 73 % of spawn | 656 ms for 12.37 MB, 83 % of spawn |
| adjudicated boot | 7.89 s | 8.83 s |

(One run each, so the boot-time delta is indicative rather than a measurement — the fill and
byte counts are exact.) This change moved two servers, five test programs and a 343 KiB font
from the initramfs onto ext4, which is +243 fills and +38 KB materialised.

**Still deferred, and the reason has changed.** Fills are 5 % of boot, so read-ahead is worth
something now rather than nothing — but **image materialisation is 83 % of spawn time and the
larger number by far**, and it is not a read-ahead problem: `sys_spawn` copies the whole ELF
eagerly, so a 12 MB boot pays 656 ms whether or not the pages are ever touched. Demand-paged
program text (the CoW/`MAP_PRIVATE` file mapping in the process-memory-model bundle) subsumes
most of what read-ahead would buy here and is the thing to do first. **New trigger for
read-ahead: after demand-paged program text lands**, when what remains is genuinely fill cost.
**New trigger for that:** it is now the largest single line in the boot profile, so it wants
scheduling rather than a trigger.

**TCP/IP networking.** The architecture is committed: userspace netstack server, network drivers as Tier 1 or Tier 2 modules, sockets as namespace resources. Implementation is deferred. Trigger: a concrete need (wanting to SSH into the system, wanting to download files, etc.). Implementation is a major effort (~15-50K lines depending on whether smoltcp is ported or a stack is written from scratch); deferring keeps the initial system simple while not foreclosing the work.

**Network booting (PXE) by the kernel.** Limine handles PXE before the kernel runs. The kernel itself doesn't need network for PXE. Network-mounted root filesystems can use the same userspace fs-server architecture as local mounts; this is gated on the netstack being implemented.

### Graphics

**The shape of a window's ports — `TODO(port-shape-rework)`.** A port is a path under the window
that owns it (`ui-composition-model.md` §5a), and that much is settled. Nothing else about it is: naming, whether a port carries a stream or a single message,
what a resolve does when nothing is listening, and whether the compositor is the right server for
a path an application defines.

**Filed because its justification changed underneath it.** §5a was written to describe what
durable window-to-window wiring bound into, and revision 3 cut that wiring. What keeps ports is a
different case the wiring story had been obscuring: **the command line addressing a GUI program** —
sending a file to a running editor, reading a selection out of a browser. That is the
everything-is-a-resource claim applied to windows, and it wants a path, since there is nothing for
`QueryCaps` alone to hand a shell. The pressures are not the same ones §5a was drawn against, so
the design is carried forward as an open question rather than as a decision.

The marker sits on the compositor's forwarded-resolve arm, which is where such a path would be
served and where `/dev/draw/<N>/ports/…` is currently declined. Trigger: the first client that
wants to address a window from outside it — most likely a coreutil, which is also the case that
would settle the stream-versus-message question fastest.

**GPU driver and compositor.** Architecture is sketched (GPU driver as Tier 2 LKM, compositor as userspace server, client-side rendering, Wayland-influenced protocol). Specific compositor protocol, 3D acceleration scope, window management model — all deferred. Trigger: when the project wants a GUI. Pre-compositor mode (`/dev/framebuffer` as a kernel resource server) is sufficient for early userspace, debug UI, and kernel panic screens.

**Specific compositor/client protocol.** Deferred along with the compositor itself. Likely Wayland-derived but using the resource-server protocol as the wire format. Decision when compositor work begins.

**3D acceleration, OpenGL/Vulkan equivalents, GPU compute.** All deferred. Initial scope is 2D framebuffer rendering.

~~**Text rendering, fonts**~~ — **decided 2026-08-11: `ab_glyph`, real TrueType, built in M4
Part C** (`display-substrate.md` §6). Userspace takes its first external dependencies:
`ab_glyph` + `ab_glyph_rasterizer` + `owned_ttf_parser` + `ttf-parser` + `core_maths` +
`libm`, all permissive, all verified to build for `x86_64-unknown-nitrox`. The bar every
future one has to clear is in `userspace/CLAUDE.md`.

**A popup does not follow its parent — `TODO(popup-follows-parent)`.** A popup's offset is
resolved against its parent's origin once, when it is created (M6 C1); the compositor stores
absolute origins and nothing re-places a child when its parent moves. So a manager that moves a
window leaves that window's open menus behind.

**The first version of this entry justified that by calling tracking "placement policy", and
that was mostly wrong.** Examined properly (2026-08-20): following off-screen just clips, because
the compositor already clips and never slides; a resize does not move a popup at all, because the
offset is from the parent's *origin*; and "during the drag or at the end" stops being a question
if the rule is simply the invariant `popup.origin == parent.origin + offset`. That is geometry,
not policy.

What is actually true is that the case is **narrow**. A click-initiated drag lands on the parent
and dismisses a menu, so the parent moves only when nothing was open. What is left is movement
with no click on the parent — a keyboard move, tiling, a shell rearranging windows — and there a
shell may well prefer to dismiss menus rather than slide them, which it can do by destroying them.
Only `popup` is affected: a `dialog` is not offset from its parent and is not supposed to follow
one.

The **reachable** consequence of resolving once is a different one, and it is fixed differently:
a popup created before its parent has been placed resolves against the default origin and is
composited detached. Holding a popup whose parent is held — releasing both together, so the offset
resolves against the parent's final origin — closes that without any tracking, and is what M6 C3
will do. Trigger for tracking itself: a shell that moves windows without a click on them *and*
wants open menus to survive it.

~~**`libsurface` allows one window per connection.**~~ — **done 2026-08-20, M6 C3.** `Window`
owned its `Transport`, so a client held one usable window per session while the *protocol* had
no such limit: `Connection::owned` is a list, and a popup may only name a parent its own
connection owns. `Session` owns the transport now and lends windows through `WindowRef`, so a
client can hold a parent and its menu at once.

Found by writing C1's gate probe and having the compositor correctly refuse a popup parented
across connections. Closing it also made the *compositor's* limit real: nothing bounded windows
per connection, because the API was the bound — see `MAX_WINDOWS_PER_CONNECTION`.

**Buffers per window are unbounded — `TODO(window-buffer-cap)`.** `WindowStack::attach` refuses a
duplicate buffer id and nothing else, so one window may attach any number of buffers and the
compositor maps each. Pre-existing, and not reachable by a well-behaved client: `libsurface`
attaches exactly the count a client asked for at creation, and `ui-testclient`'s churn proves the
mappings are reclaimed on destroy.

Filed now because M6 C3 changed the arithmetic. The argument for
`MAX_WINDOWS_PER_CONNECTION` was that everything else on this server is bounded — and this is
the exception, now multiplied: 29 sessions (`MAX_WAIT_HANDLES - 3`) × 64 windows = 1,856 windows,
each with an unbounded buffer list, where the old API held it to one window per connection.

The bound is not obviously a count: what matters is mapped *bytes*, and a cap on buffers alone
would still admit a few enormous ones. Trigger: the first untrusted client, or a per-connection
memory budget, whichever comes first — the second is the better answer and the reason not to
guess a number now. Raised by the PR #222 review.

**`/dev/draw/manage` gates nothing yet — `TODO(manage-ungated)`.** The manager channel's
capability is meant to be the *binding*: a supervisor puts it in the shell's namespace and nowhere
else, which is how everything else in this system is gated. In Milestone 6 that gates nothing, and
three facts compose to say so: `/dev/draw` is bound **unscoped** into init's root namespace
(`userspace/init/src/main.rs`), every graphical client is spawned with `namespace: 0` and inherits
it, and the compositor classifies resolves by **suffix alone** with no caller identity — it already
records the consequence for a different suffix, *"Any holder of `/dev/draw` can resolve `info` in a
loop."*

So any graphical client could resolve `manage`, and the only thing separating them is the
first-come rule — which makes the **race** the gate, when that rule exists to avoid one. M6's image
relies on an *ordering* (the intended manager resolves first) and this entry is the record that it
is an ordering rather than a capability.

**Not a hole to plug in M6.** Namespace-based gating needs *per-client namespaces*, and the process
that constructs them is `desktop-shell` ([`graphical-session.md`](../design/graphical-session.md)
§3, §5a). Until it exists there is one namespace and everybody inherits it, so no binding can be
given to one client and withheld from another. Trigger: **Milestone 7**, which closes it by binding
`manage` only into the shell's session namespace — at which point the first-come rule stops being
load-bearing and becomes a belt-and-braces check.

**The mechanism, since M7's plan now rests on it** (`display-arm-plan.md` Milestone 7 Part E): an
application's namespace binds `/dev/draw/new` as its own path with subtree base `/new`, not the
`/dev/draw` subtree. The exact resolve matches and forwards `new`; `/dev/draw/manage` is not a
component-boundary prefix match against that binding, so nothing answers it. The shell's session
namespace binds the subtree and reaches both. It cannot express "the subtree minus `manage`",
which is why an application that ever needs `/dev/draw/<id>/info` for ids it does not know in
advance re-opens this.

**A supervisor cannot tell its children's exits apart — `TODO(child-exit-attribution)`.**
`KIND_CHILD_EXITED` carries the child's **pid** (body bytes 0–3), and nothing in the system maps
a process **handle** back to a pid: `sys_process_spawn` returns a handle, `sys_handle_stat`'s
`HandleInfo` carries rights / object type / generation / size and no pid, there is no pid syscall,
and `/proc` exposes exactly `self/status` (the caller's *own* pid) and `sched/stats` — there is no
per-pid tree. `KIND_PEER_CLOSED` is declared in both ABIs but **never emitted**, so a control
channel's death is not a notification either.

So a supervisor with two children learns *that* one exited and never *which*. Both supervisors
already assume their way past it: `init`'s `reap_loop` attributes the first `ChildExited` to its
primary child while `parent_h != 0`, without comparing the pid — and init has six or more children
(fs-server, profile-server, compositor, input-server, tty-server, service-mgr) — and `service-mgr`'s `supervise` does the same for its single service. Neither has been
bitten because in both cases only one child is *expected* to exit, which makes this a latent bug
rather than a live one.

**It bit the moment a supervisor gained a second child that exits**, which is exactly what a
declared `boot-probe` is ([`test-path-retrofit.md`](../planning/test-path-retrofit.md) Part A):
the probe exiting was read as the supervised service exiting, closing its handle and firing its
restart policy while the service was still alive. Found on contact 2026-08-21, and demonstrable:
with the pid-blind rule in place the guest prints `'heartbeat' exited code=0` and
`restarting 'heartbeat' (attempt 1 of 3)` for a service that never stopped.

**Reproduce it under `cargo xtask check-terminal`, not `test-qemu`.** Part B made `boot-probe`
the verdict-writer, so under `test-qemu` its last act terminates the machine and `service-mgr`
never sees it exit — there is nothing to misattribute there any more. `check-terminal` boots the
same image without the `isa-debug-exit` device, so the probe runs past the verdict to `exit(0)`
and the bug is visible again. `check_service_attribution` lives on that gate for the same
reason.

**`service-mgr` no longer depends on this, and does not use the pid.** It watches each child's
**control channel** instead: the child's end is destroyed when it exits, the kernel nulls the
survivor's peer pointer and signals it (`sched::ipc_endpoint_closing` → `signal_ipc_endpoint`,
the same wake path `sys_wait` uses), so the control handles sit in the wait set and a
`sys_channel_recv` on the one that woke answers `PeerClosed` (`-13`) rather than `WouldBlock`
(`-11`). A handle cannot be recycled under its holder the way a pid can, so *which* endpoint
closed is never ambiguous — and it needed **no kernel change**: `KIND_PEER_CLOSED` is still
unemitted, and the mechanism turned out not to need it.

**It is exact given a contract the manager cannot verify**: a declared service must hold its
control endpoint until it exits. What the manager observes is the endpoint closing; that is the
child exiting only because nothing else closes it. A service that closes it early is reported
dead while it runs, and under `policy = "always"` gets a second live copy — found that way in
`boot-probe`, which closed the handle as its second instruction (PR #226 review, finding 1). The
contract is now stated in `docs/spec/service-toml-schema.md` under the `control` handle.

**What is still deferred is the exit *code*.** It arrives on `KIND_CHILD_EXITED` beside a pid the
parent cannot match, so `service-mgr` pairs codes with dead services in arrival order. With one
exit per wake — every case the system produces today — that is right; with two in the same wake
the codes can swap, which matters only to `on-failure` (`never` and `always` do not read the
code). A service found dead with no code queued is treated as a failure, since a crash that
outruns its notification is the case worth restarting.

**`init` is untouched and still has the original bug**: `reap_loop` attributes the first
`ChildExited` to its primary child without comparing the pid. Its children do not all have control
channels, so `service-mgr`'s fix does not carry over.

**The trigger named Part C has arrived, and the bug is newly *reachable* in a test image.** Part
C2 made `init::supervise` unconditional, so `reap_loop` now runs with `parent_h = service_mgr_h`
where the `selftest` path used to pass `0` — the branch could not fire at all before. It stays
latent because init's remaining children are the servers it binds, none of which is expected to
exit; the graphical clients and the demo chain are `service-mgr`'s children now, not init's (this
entry said "plus the selftest demos" until 2026-08-24). Closing it needs one of the three shapes
above, all of which touch the ABI or the notification path.

The candidate fixes for the code, none chosen: **report the pid at spawn** (the kernel writes it
back into a `SpawnArgs` field, or a new syscall reads it from a process handle) — smallest, but
`SpawnArgs`/`HandleInfo` cross the ABI and `HandleInfo`'s size has already caused one memory bug;
**emit `KIND_PEER_CLOSED`** with the exit status folded in; or **carry the parent's handle** on
`KIND_CHILD_EXITED` beside the pid, which answers the question directly and keeps the code on the
same record.

**`sys_process_spawn` drops syscaps the parent lacks, silently — `TODO(spawn-syscap-attenuation)`.**
The kernel computes `child = parent & requested` (`kernel/src/syscall/table.rs`), which is the
right rule and the wrong *report*: a spawn asking for more than the parent holds succeeds, and
neither the kernel nor the parent learns which bits went missing. The child then fails at the
point it first tries to use one, somewhere else entirely.

Nothing is broken by it today — `service-mgr` holds `BIND_NAMESPACE` and the only declaration
asking for a capability asks for that one. It became reachable when `syscaps` became declaration
data (retrofit Part C2): before that, every grant was a `SpawnArgs` literal written next to the
spawn, and now it is a name in a file.

**A supervisor cannot check it either.** Nothing reports a process its own capability set —
`/proc/self/status` carries pid and tid — so `service-mgr` cannot verify a declaration's subset
without hardcoding a second copy of what `init` granted it. `service-manager.md` claimed a
parse-time subset check for exactly this reason, and there has never been one.

Shapes, none chosen: **report the drop** (spawn returns the granted set, or logs the difference),
**refuse the spawn** on any requested bit the parent lacks (a stronger contract, and the one the
schema used to describe), or **expose a process's own syscaps** so a supervisor can check before
asking. Trigger: the first declaration wanting a capability `service-mgr` does not hold — a
device manager with `LOAD_MODULE` is the obvious one.

**An intermittent `#DF`, cause unestablished — `TODO(unexplained-df)`.** A double fault with a
kernel `rip` (`0xffffffff8001e000`), a **user** `rsp`, and the stack pointer reported "not
scannable" — the shape of a fault taken where no fault can be delivered.

**Not new, and previously untracked.** The 2026-05-29 decision log named a residual it believed
was the cause — *"an AP mutating the shared kernel-vmap page tables for its own kstacks leaves
other cores' cached paging structures stale, so init's `RSP0` push faults → `#DF`"* — put the rate
at ~15 % under KVM boot-looping after two partial fixes, and said closing it was the next step. It
never got an open-item entry, so nothing has tracked it for three months; this entry is that.

**That attribution does not survive re-checking, and this entry asserted it before checking.**
The first draft said `KernelStack::new` needs a TLB shootdown because `drop` has one. It does not:
`kstack.rs` argues, thirty lines below the install loop, that adding a mapping needs no cross-CPU
invalidation on x86 — the VA is freshly allocated, so no CPU holds a cached translation for it,
stale or otherwise. The premise holds: `kvmap`'s allocator is a bump pointer that **never reclaims
VA**. x86 does not cache not-present translations, so there is nothing to invalidate. Linux does
the same. So the install path is **ruled out**, and the earlier draft of this entry — plus a
`TODO` marker placed on that loop — was wrong twice over: wrong about the mechanism, and written
without reading the argument next to it.

**Still live under TCG, and invisible to CI — which is the part that matters.** Measured
2026-08-24:

| Configuration | Boots | `#DF` |
|---|---|---|
| Local, **TCG** (no `--kvm`) | 14 | **2** (~14 %) |
| Local, **KVM** (`--kvm`) | 6 | 0 |
| **CI**, KVM (40 workflow runs) | 83 | 0 |

So the gates cannot see it: every CI job passes `--kvm`, and 83 consecutive clean boots read as
"fine". What it degrades is **local development**, because `cargo xtask check-terminal` with no
flag is TCG — the default a session runs. A one-in-seven red on a gate that is green in CI is
also the most expensive kind of flake to diagnose, since the obvious first move is to re-run and
watch it pass.

The 2026-05-29 figure of ~15 % was attributed to KVM boot-looping. Either the two partial fixes
closed the KVM-visible path and left the TCG one, or that attribution was imprecise; six local
KVM boots is too small a sample to say which, and the CI evidence only bounds it low.

Both failures are the same fault: `#DF`, kernel `rip 0xffffffff8001e000`, a **user** `rsp`, and
the stack pointer reported "not scannable" — the shape of a fault taken where no fault can be
delivered. One landed during the demo chain's rename/move stage, the other after the terminal
interaction. An initial reading of 8 clean runs suggested the rate had dropped far below 15 %;
two more failures said otherwise, and the smaller sample was the wrong one to believe.

**It is not caused by the retrofit** — the plan changed which *process* runs these tests, not the
kernel — but the retrofit is why it is being seen: more of the gate set now boots the same image
through the same paths, so a 15 % fault has more chances to show.

**One window found and closed, and it was not enough.** The `sysretq` exit stub restored the
user `RSP` (`pop rsp`) two instructions before leaving ring 0 **without re-masking interrupts**.
An interrupt arriving there takes no privilege change, so the CPU pushes its frame onto the
*user* stack; under SMAP that push faults, and delivering that fault on the same stack is a
`#DF`. A `cli` before `pop rsp` closes it.

*(This paragraph originally continued "the syscall body runs with `IF` set — a blocked syscall
resumes that way". **Both halves were wrong**, and the second is what sent the next pass looking
in the wrong place: `switch_into` captures a thread's `IF` before the `SCHED` acquire, which
SFMASK has already made false, so a resuming syscall returns masked. What actually left `IF` set
is below.)*

**Proven by widening the window rather than waiting for the flake**: 256 `pause` instructions
between `pop rsp` and `sysretq` turn a ~14 % intermittent into a deterministic `#DF` on the next
boot; the same widened window with `cli` in front of it boots clean. That is a positive control
for the mechanism *and* for the fix, and it took two runs instead of dozens.

**The entry side already knew about this shape.** `idt.rs` puts NMI on IST2 because
"`syscall_entry` has a two-instruction window after `swapgs` where RSP is still the *user* stack",
and describes the same `#PF`-then-`#DF` chain. The hazard was understood on entry — where SFMASK
masks `IF` and IST2 covers NMI — and missed on exit, where neither applied.

**Whether it fully fixed the fault is still open, and the numbers are why.** After the `cli`:
**1 failure in 49 boots (~2 %)**, against **2 in 14 (~14 %)** before — a ~7× reduction, and the
one post-fix failure has not recurred in the 45 boots since. Fisher's exact on 2/14 vs 1/49 gives
p ≈ 0.09: suggestive, not conclusive, and *not* a demonstration that the fault is gone.

The single post-fix failure cannot be decoded retroactively — its build's `LSTAR` was not
recorded, which is exactly the gap the dump now closes. If it was `syscall_entry + 0x5C` in that
build's layout it lands on `pop rsp` (one byte later, with the `cli` in front), which interrupts
cannot reach; if the base had shifted it is something else entirely. The next occurrence will say.

**A methodological correction, and then a correction to the correction.** The `rip` was first
read as the faulting instruction, then dismissed — for `#DF` the pushed instruction pointer is
architecturally **undefined**, and this one was identical across rebuilds while decoding to a
different symbol in each, which looked like the tell.

**Dismissing it was the bigger mistake.** QEMU pushed a meaningful `rip` here; the reason it
"decoded to a different symbol" is that it was being compared against the *wrong build's* symbol
table. Read against the failing build's `IA32_LSTAR` — `0xffffffff8001dfa4` — the value
`0xffffffff8001e000` is `syscall_entry + 0x5C`, which in that layout is exactly the `sysretq`
instruction. The clue had been pointing at the answer the whole time.

So the dump now prints **`LSTAR` alongside `CR2` for vector 8**, and `rip - lstar` decodes itself.
The address is otherwise unrecoverable: every rebuild moves it, and the failing kernel is gone by
the time anyone reads the transcript. `CR2` is still worth having — it holds the address the
first fault could not translate — but for this fault the offset is the field that talks.

**Hypotheses closed with evidence, so they are not re-run.** *SFMASK not masking `IF` on some
CPU* — a runtime probe on every ring-3 descent read `sfmask = 0x40600` (IF | DF | AC): armed
correctly, and armed on APs as well as the BSP. *`enter_user`* — builds its `iretq` frame on the
kernel stack and never loads a user `RSP` in ring 0. *The kstack install path* — ruled out above.

**A dead end worth not repeating: raising the timer rate is not an accelerator.** At 20× (`TICK_NS`
500 µs) five runs were clean; at 100× (100 µs) three of six failed — but with *different*
signatures each time, a `#PF` with `cr2 = 0x10` and a `vector 0x46` with `rsp = 0`, neither
matching the fault being hunted. It manufactures its own faults rather than accelerating the one
you want, and "the probe increased failures" is not the same as "the probe increased failures of
the kind I am hunting".

**The source of the enabled `IF` — found, and it is `tlb::shootdown`.** The `cli` closed the
window; it did not answer *why interrupts were on at a syscall's exit at all*, and that question
turned out to have a specific answer. `shootdown` **enables interrupts itself** and must: it
spins waiting for peer CPUs to acknowledge an IPI, and an `IF`-masked spinner cannot service the
shootdown IPI a peer is simultaneously sending it — mask both sides and they deadlock. Having
enabled them, it restored the state it captured on the way in:

```rust
let prev_if = Cpu::interrupts_enabled();     // false — SFMASK masked IF at syscall entry
unsafe { Cpu::interrupts_enable() };          // required, per above
{ let _guard = LOCK.lock(); /* IPIs; spin for ACKs */ }
unsafe { Cpu::interrupts_restore(prev_if) };  // prev_if == false …
```

…and `interrupts_restore` was `if prev { sti }`, with an `else` arm that was a **no-op**,
commented *"leave IF clear — it already is"*. That comment held for every caller that had reached
it through `interrupts_disable`. It was false for the one caller that had enabled `IF` itself. So
the restore did nothing, and **the entire tail of every SMP `sys_memory_unmap` after the shootdown
ran with interrupts unexpectedly enabled** — through the syscall's return, and out through the
`sysretq` stub's ring-0-on-user-stack window.

**Measured before assuming.** A counter at syscall entry and exit over one boot: of **60,000**
syscalls, **647 returned with `IF` set — and all 647 were `SYS_MEMORY_UNMAP`**. Entry was `IF=0`
on all 60,000, which rules out the entry path and pins the leak to the body. This is a far wider
exposure than the two instructions the `cli` covered, and it is consistent with the `cli` moving
the rate to ~2 % rather than to zero.

**The fix is to delete the precondition, not to patch the caller.** `interrupts_restore` is now
unconditional — `if prev { sti } else { cli }` — so it does what its name says regardless of how
the caller got there. Patching `tlb.rs` alone would have left the same trap armed for the next
caller that enables interrupts on its own; the other four call sites all happen to satisfy the
old precondition, which is precisely why it survived unnoticed.

**A `debug_assert` at syscall exit keeps it closed**, and both polarities were demonstrated
rather than assumed: it fires within two `test-qemu` boots on the unfixed tree, and holds across
8/8 with the fix. (Its first run was silent, because `shootdown` has a fast path that returns
early when no other CPU is online; a forced `interrupts_enable` before the assert confirmed the
instrument worked before that silence was believed.)

Trigger: the entry stays open until a TCG sample large enough to distinguish the post-fix rate
from the ~2 % residual comes back clean. Reproduce with `cargo xtask check-terminal` **without**
`--kvm`, several times.

**A per-backend output queue in the tty server — `TODO(tty-output-queue)`.** `Tty::Output` is
sent with `SENDMODE_BLOCK`, so a terminal emulator that stops draining stalls **the whole
server** — one blocked send holds its single serve loop, so `session-mgr`'s login terminal and
the serial console shell stall with it, not just the wedged emulator's own terminals. (This
entry said "every terminal it serves", which reads as per-backend; it is per-server — PR #194
review.) It is a stall and not a deadlock: the chain is tty-server → emulator → compositor, the
compositor depends on nothing in the tty path, and the emulator's own send back is `NOBLOCK`.

**The input direction is still the un-made half of the same trade.** `Backend::typed` is
`NOBLOCK`, so on a full ring `nxterm` logs a line and drops the keystrokes — `take_outbox` has
already emptied them. It needs the ring's eight messages to back up, so it is far harder to
reach than the output side was, but it is the same silent loss and the queue should cover both
directions. The alternative it replaced was worse and is why this is not simply
left alone: a `NOBLOCK` send onto a full ring **silently discards a program's output** — no
error reaches anyone, the shell believes it printed, and the user sees a line with a hole in
it. `check-terminal` found it as an intermittently-missing character, which is exactly how it
would present in use. The answer that costs neither is a per-backend queue the serve loop
drains alongside everything else. Trigger: a second terminal emulator, or the first time a
stalled client is observed to affect another. Landed 2026-08-13 with M5 Part C
(`userspace/tty-server/src/main.rs`).

**`/dev/tty` inside a graphical application — `TODO(gui-dev-tty)`.** M5 Part C hands `nxterm`'s
shell its terminal as a *handle* at spawn, the way `libstream` already passes streams. A
`/dev/tty` resolved inside that window still reaches the session's console, because that binding
belongs to a namespace `desktop-shell` will construct and no such process exists. Inert today —
no program that could run inside a window resolves `/dev/tty` except `nxsh`, which will have been
handed one — and wrong to fix in the tty server, because it is a property of how applications get
their namespaces. (`session-mgr` resolves it for the login prompt and the test harness in the
gate; neither runs in a window.) Trigger: **Milestone 7**, the graphical session;
[`graphical-session.md`](../design/graphical-session.md) §6.1 holds the three candidate shapes.

**Concurrent serial and graphical sessions — ~~deferred~~ ANSWERED 2026-08-21: two independent
sessions.** `session-and-auth.md` deferred "one console, one session at a time"; two supervisors
able to authenticate independently fired it, and Milestone 7's details pass settled it.
`session-mgr` and `desktop-session-mgr` each authenticate and run a session, unaware of each
other — so serial staying available while a graphical session runs is governing decision 3
holding by construction rather than by care. Nothing `logind`-shaped is needed, and nothing built
for two sessions forecloses one later. See
[`graphical-session.md`](../design/graphical-session.md) §6.2 for the costs accepted.

**A scrollbar's grab offset — `TODO(scroll-grab)`.** `ScrollState::offset_at` puts the thumb's
*centre* under the cursor, so grabbing a thumb near either of its ends makes it jump by up to
half its length before the drag begins. Every toolkit that avoids this remembers where within
the thumb the press landed and subtracts it — which is interaction state retained between two
pointer events, and `widget-toolkit.md` §3 reserves retained widget state for things the
application has no opinion about. This qualifies, so it is a question of where it lives rather
than whether it is allowed. Trigger: the first time someone drags a scrollbar and complains, or
M6's window management, which needs press-relative dragging for window moves anyway and will
have to answer the same question. Landed 2026-08-12 with M5 Part B (`userspace/libui/src/widget.rs`).

~~**Antialiasing is deferred, and it is a `libdraw` item rather than a font one.**~~
**Resolved 2026-08-12, in M5 Part A.** `libdraw` composited opaque XRGB8888 and could not
blend, so glyph coverage was thresholded to 1 bit while the rasterizer produced 8. It blends
now — `Rgb::blend` and `Framebuffer::blend_pixel` — and `Font::draw_str` passes coverage
straight through.

**The trigger fired on its second clause**, "text that looks bad enough to prompt one": a
terminal is entirely text, so M5 is where one-bit coverage stops being tolerable. The first
clause was circular in hindsight ("trigger: `libdraw` growing an alpha-blend path" is the work,
not a reason to do it), which is worth noticing in a file whose value is its triggers.

**The threshold was deleted rather than kept as a fallback**, which the entry, the plan and
`display-substrate.md` §6 all said it would be. A fallback is for a caller that cannot take the
good path; thresholding is not that, it is only worse output. The one caller that could
genuinely want it is a surface that cannot be read back — and such a surface cannot blend
*anything*, so that is a `Framebuffer` capability question rather than a second glyph loop kept
alive against a case nothing has.

**No alpha channel was added**, and that is the shape worth recording: coverage is an argument
to a blend, not a fourth byte in a pixel. Surfaces stay opaque and `compose` stays a copy. What
made the threshold necessary was the absence of the *operation*, not of a channel.

**Input methods and accessibility** remain deferred, and accessibility is a **gap rather than
an oversight** — no accessible tree, no screen-reader surface, and retrofitting one is
substantially harder than designing it in (`widget-toolkit.md` §11). Trigger: neither has one
yet; both want a deliberate discussion the way the rasterizer got.

~~**Input: key repeat.**~~ **Decided 2026-08-10: generated compositor-side, built in M4
Part C.** Held keys do not repeat. The record format reserves `value == 2` for it
(`docs/architecture/input-subsystem.md` §3), so no wire change is needed.

**Two corrections to this entry.** It said repeat "wants a timer in `libinput`" — it does
not: `libinput` is pure and has no syscalls, so a timer cannot live there. The compositor
generates repeats, because it knows which window has focus and so can stop them when focus
moves, with no client involvement. The alternative is Wayland's — send clients a repeat
*rate* and let each run its own timer — which is better when clients disagree about what
should repeat, a distinction nothing here makes yet, and costs every client a timer and a
state machine. See [`widget-toolkit.md`](../architecture/widget-toolkit.md) §9.2.

And the trigger said "the first text field — M4's toolkit". **It named the right milestone
and the wrong widget.** M4 has no text field — M5's terminal grid is a custom-drawn widget by
the plan's own decision, so a generic text area would have no user — but holding a key in
that grid must still repeat. A trigger naming the *artifact* expected to embody a need rather
than the need itself reads as un-fired exactly when it has fired. Raised 2026-08-06.

**Input: USB HID, hotplug, multitouch slots, gesture recognition.** None exists; M3 builds
PS/2 only. All of them land in the `input-server` or `libinput` rather than the kernel, which
is the point of the arrangement (`docs/architecture/input-subsystem.md` §1) — so none requires a
kernel change when it arrives. Trigger: hardware, or a laptop touchpad. Multitouch
additionally needs slot semantics on top of `SYN` grouping, as evdev did. Raised 2026-08-06.

**Input: `EV_ABS` device-space → screen-space mapping.** Absolute coordinates are meaningless
without knowing the device's resolution and the screen's, and nothing decides who maps them.
Not needed until a touchscreen or a tablet. Raised 2026-08-06.

**~~Input: who owns accumulated pointer position.~~ Decided 2026-08-10 — the compositor
owns it.** The `input-server` sees every device; the compositor owns the screen. What settled
it was not a second pointing device but the *clamp*: a position is only meaningful bounded by
a screen, and `input-server` has no screen and no reason to acquire one. M3 Part C3 put the
accumulation in `compositor::input::InputRouter`, which clamps against the framebuffer
geometry the compositor acquired; `libinput` emits deltas only and says so. **A second
pointing device does not reopen this** — it merges into the same stream and moves the same
cursor. What is still open is what a second *screen* means, which is a different question.
Raised 2026-08-06, decided by PR #180.

~~**Back-pressure for compositor→client messages.**~~ **Decided 2026-08-10 (M3 Part D3):
a bounded per-session outbox in the compositor, with motion coalescing.**

Of the three candidates filed, the second was taken — *a per-session outbound queue with a
bound and a policy for overrunning it* — and the two rejected are worth recording. Blocking
the compositor lets one slow client stall every other window, which is the same objection as
before and did not get weaker. Disconnecting the client turns an infinite hang into a
definite error, but it kills a client whose ring was only transiently full, and with input
arriving continuously "transiently full" is the ordinary state of a client rendering a frame.

What changed the answer was **what a real second client did**, which is exactly what the
deferral was waiting for. Part D2's gate lost a keystroke behind twelve cursor movements:
input is a *stream* and a `Release` is not, so on a shared ring the cheap message reliably
evicts the expensive one. That reframes the problem — it was never about depth. No depth is
enough against a stream, it only moves the threshold, and a rarer permanent hang is worse to
diagnose than a reproducible one.

The shape:

- **Every unsolicited message goes through `compositor::outbox`**, releases included, so
  ordering is preserved and a refusal parks the message at the head of the queue instead of
  dropping it. `NOBLOCK`-and-forget is gone.
- **At most one motion per window is queued.** A newer motion removes the older and takes its
  place at the *back*, so a motion that happened after a keystroke is still delivered after
  it. This is what X11 and Wayland do, and it works for the same reason: a motion record
  carries an absolute window-local position, so the newest says everything the older ones did.
- **The queue is flushed every loop iteration**, head-of-line, stopping at the first refusal,
  and the compositor **wakes itself** to retry. A channel endpoint signals only when it has
  something to *read*, so a client draining its ring does not wake the compositor at all —
  a parked message with no wakeup is not "a latency bound, not a loss", it is a permanent
  hang on the one message whose loss is unrecoverable, and a *silent* one where the code it
  replaced at least logged the drop. While anything is parked the serve loop waits on a
  10 ms deadline instead of forever; with every outbox empty the wait is still infinite, so
  an idle compositor does not poll (PR #181 review, finding 1).
- **The ring is 16, not 4** — which is 4× the old threshold and *not* a different kind of
  bound. Coalescing bounds the outbox, not the ring: two motions collapse only while both are
  queued, and a compositor flushing every iteration sends each as its own message. What
  removes the cliff is the retry; the depth only decides how long a stalled client goes before
  its motion starts coalescing (PR #181 review, finding 4). The old value was never chosen —
  a literal copied into every resource server in the tree, and a quarter of the kernel's own
  `IPC_DEFAULT_QUEUE_DEPTH`. Sixteen costs 128 KiB of kernel memory per session, since a slot
  is a whole 4 KiB `IpcMsg` whatever the payload.

**Still open, and smaller than it was:** the Surface protocol has **no loss marker**. If a
session's outbox overflows — which needs a client stalled long enough to accumulate 32
*discrete* events, since motion no longer accumulates — the compositor discards the oldest
and logs it, and the client is never told. `libui` already has `WindowEvent::Dropped` for its
own queue overflowing, so the client-side vocabulary exists; what is missing is a wire record.
Trigger: the first observed outbox overflow that is not a deliberately wedged test client, or
the toolkit (M4) needing to resynchronise held-key state after one. Raised 2026-08-10.

**The `4` in the other resource servers** (`auth-service`, `fs-server-ext4`, `hello`,
`input-server`) is left alone deliberately: those channels carry request/reply traffic, where
the sender is waiting for an answer and cannot outrun the receiver. They are undersized
rather than wrong. Trigger: any of them growing a server-initiated push stream.

~~**`KeyEvent` and `PointerEvent` do not say which window they are for.**~~ — **done 2026-08-20,
M6 C3.** Both records carry a `window` at offset 0, as `Release`, `FocusEvent` and `Configure`
already did; `KeyEvent` is 12 bytes and `PointerEvent` 24. `libsurface::Window::apply_event`
filters on it, which is what the widening was for.

The trigger this entry named — "the first client with two windows" — arrived exactly where it
predicted, at Part C's menus. Filed at the PR #184 re-review on 2026-08-11 and closed nine days
later without the intervening milestones having to guess.

### Processes and memory

~~**The default user stack is 32 KiB…**~~ **Decided 2026-08-10: 8 MiB plus a guard gap,
built in M4 Part A.**

`DEFAULT_USER_STACK_SIZE` (`kernel/src/arch/x86_64/abi.rs`) **is 8 MiB** as of M4 Part A,
raised from 8 pages, which had itself been raised from 4 for the read-write fs-server. It is
the **only** stack the initial thread gets — `sys_thread_create` makes the caller supply its
own for every other thread — and it now has a 1 MiB guard gap below it, so overrunning it
faults rather than landing silently in whatever is mapped there.

**A correction to this entry, which had a false claim in it.** The original filing said
raising the number costs "eager mapping, which is measurable at spawn". That is wrong: the
stack is mapped with `map_vma_lazy` (`kernel/src/mm/elf.rs`) and there is a kernel test
asserting *"stack must be demand-paged, not eagerly backed"*. Raising it costs **address
space and nothing else** — page tables materialise only for pages actually touched. The
entry was written to inform this decision and asserted a cost the code explicitly does not
have.

**8 MiB, matching Linux**, whose `RLIMIT_STACK` default (`_STK_LIM`) is 8 MiB grown on
demand; glibc gives pthreads the same. That is 8 MiB of a 128 TiB user half, for one stack
per process.

**The guard gap is cheap, and that is a consequence of the lazy mapping.**
`MMAP_MAX` (`kernel/src/mm/addr_space.rs`) **was**
`DEFAULT_USER_STACK_TOP - DEFAULT_USER_STACK_SIZE`, so the mmap arena topped out at exactly
the stack's lowest address — adjacent, with no gap, which is why an overflow landed in mapped
memory instead of faulting. It is now `USER_STACK_GUARD_BOTTOM`, leaving addresses with no VMA
below the stack, and a touch faults cleanly. Linux learned this the hard way: its gap was a single page until
Stack Clash (CVE-2017-1000364) showed a page could be jumped, and the default is now 256
pages. Nitrox takes the post-2017 lesson rather than repeating the experiment.

**It is two changes, not one — an earlier draft of this entry said "arithmetic rather than
machinery" and that was wrong.** `MMAP_MAX` bounds exactly one caller, `find_free_range`,
whose own docstring scopes it to `sys_memory_map(hint = 0)`. The **hinted** path validated
page alignment and `end > USER_VIRT_END` and nothing else, so a hinted mapping could be placed
inside the gap and an overrun would land in mapped memory again — silently, which is the
single property the gap exists to remove.

**So `sys_memory_map` refuses a hinted range intersecting the gap** — implemented as
`AddressSpace::in_stack_guard`. One range check, and it is what makes the gap a *guarantee*
rather than a convention about where the kernel happens to place things. The alternative — document the gap as protecting only against kernel
placement, roughly where Linux lands with `MAP_FIXED` — is coherent, but it keeps the failure
this whole item is about, and keeps it silent. Nothing in userspace passes a non-zero hint
today, so the cost is a check nobody currently trips.

**What made this urgent** was M4, which is one of the triggers this entry named: a toolkit is
recursive by construction — measure, arrange, paint and diff are all tree walks — and `libui`
already documents a ~9 KiB struct held by value being *"enough to run a client off the end of
its stack, which presents as a process that dies in its prologue and prints nothing at all"*.
Silent, at 32 KiB, from one struct. The guard gap is what converts that into a diagnosable
fault, and is worth more than the size.

**Still open, and no longer blocking:** there is no user-side equivalent of the kernel's
`kstack: deepest 6520 B of 16384` watermark, so nobody can say how deep a client actually
goes. At 32 KiB that was load-bearing; at 8 MiB it is curiosity, and the reason to want it is
to notice a client doing something silly rather than to size the stack. **Growth on fault**
also stays deferred — it needs the fault path to distinguish a stack overrun from a stray
access, and 8 MiB of demand-paged reservation makes it much less interesting. Trigger for
either: a client that exhausts 8 MiB, or a stack-depth question that costs debugging time.

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

**Built 2026-08-13, and this entry described it as merely designed until 2026-08-18 (audit
D.5d).** The server exists: `userspace/tty-server` is a workspace member, `init` spawns
`/bin/tty-server` and binds its endpoint at `/dev/tty`
(`userspace/init/src/main.rs`), [console-and-tty.md](../architecture/console-and-tty.md) reads
"**Status: stages 1–4 built (2026-08-13)**", and `cargo xtask check-terminal` drives a real
`nxsh` through it every run. What the entry still said was that this was a design from
2026-08-03 — which is how a reader concludes the whole capability hole is open when half of it
is closed.

**The half that is still owed** is the one the headline names: *output*. `nxsh` prints through
`kprint` (`userspace/nxsh/src/main.rs`), so the ambient `SYS_DEBUG_KPRINT` path above is
unchanged for shell output even though input now goes through a capability. Nothing can
redirect, pipe, capture or log it, because there is still no object to redirect. The three
consequences listed above should be re-measured against the shipped server rather than trusted:
they were written when nothing was built.

Excluded from the design deliberately, and still absent: job control (needs process groups,
which do not exist, and cannot use signals), key events (need a real keyboard driver), and
terminal emulation (belongs to the compositor).

Trigger: moving `nxsh`'s output off `kprint` and onto the `/dev/tty` channel it already holds.
It also gates the rich REPL (§11) and is the trigger for `TODO(session-metadata-server)`.

**Reverse-search shows one match, not a list — `TODO(history-pager)`.**
`Ctrl-R` narrows to a single match and `Ctrl-R` again walks to the next older one, blind:
you cannot see the alternatives, only step through them. That is bash, zsh and PSReadLine's
model. **fish and nushell went the other way** — both present a navigable *list* of matching
entries and let you pick, which is plainly better when several commands share a prefix.

Not built because it needs terminal capabilities we do not have. The current redraw model is
erase-and-rewrite on a dumb terminal (`\x08 \x08` per character, no assumed width); a list
needs multi-line output, cursor addressing, and a way to know the terminal's size. That is
the same capability set the compositor terminal brings, not something the search logic is
missing — `History::search_back` already returns indices and can enumerate every match.

Trigger: the compositor terminal, or any terminal backend that can address the cursor.

**A log line assembled from several `kprint`s can be torn — `TODO(atomic-log-lines)`.**
The console is shared and `kprint` is a syscall per call, so a message built from several —
`kprint(b"… exit "); kprint_u64(n); kprint(b")\n")` — can be split down the middle by any
other process that logs in between. It is atomic *per call* and nothing more.

**This is not hypothetical.** `session-mgr`'s session-ended line came back from CI as
`session ended (shell exit tty-server: terminal closed` / `3)` — the tty server logged a
close while the status was being written. It passed locally and failed under KVM on the
runner, which is the timing signature of exactly this class. That one line now assembles
into a `String` and emits once; **the pattern is everywhere else** — roughly 33 call sites
across userspace use `kprint_u64`/`kprint_hex` mid-line, and `service-mgr` builds its
service-declaration lines the same way.

~~The fix is a small `kprintf`-style helper…~~ **Done 2026-08-10 (M3 Part D1).**
`libkern::debug::Line` formats into a 256-byte stack buffer and issues one `kprint`; every
multi-call line in userspace was swept into it, and the three hand-rolled copies of the same
buffer (`compositor`, `test-harness/inputclient`, `service-mgr`'s local `kprint_u64`) are
gone. Both triggers had fired: `check-input` was 40% flaky for a milestone because its
six-call lines arrived shredded — and was misdiagnosed as a guest bug first, which is the
debugging cost the trigger named — and Part D2's gate has to assert on a line carrying a
keycode, modifiers and coordinates together.

A truncated line is marked `...` rather than silently shortened, because these lines are what
the QEMU gates match on and a short line that reads as complete is worse than one that admits
it was cut.

**Control flow inside an expression — `TODO(control-flow-in-expression-position)`.**
`eval` returns a *value*, so a block whose value is being taken has no channel for control
flow to travel back through. Two consequences, one new and one pre-existing:

- **`break`/`continue` in expression position are refused** (Milestone 4 Part A) — `let y =
  if c { break } else { 1 }` errors, with the statement-position form named in the message.
  Legal per §9c (it may well be inside a loop), and still not expressible.
- **`return` in expression position is silently wrong.** `let x = if c { return 1 } else { 2 }`
  binds `1` to `x` and keeps going, instead of leaving the function as §5b says. That
  predates Part A; it is the same missing mechanism, seen from the other side.

Statement position is unaffected and is where these are actually written: `if done { break }`
on its own line works, because `Stmt::If` propagates `Flow`.

The fix for both is the same — a `Flow`-carrying result through expression evaluation —
which is a change to every `eval` arm and wants its own slice. Trigger: **Milestone 4 Part C**
is the one to watch, since `try`/`catch` becoming an expression puts a block in value
position where a `break` is a plausible thing to write; if it does not bite there, wait for
a real script.

**Bitwise operators, and the `0o` literal — `TODO(shell-bitwise)`.**
Shell design §8e admits hex and binary literals and justifies them by
"permissions/flags/addresses" — and the language has no way to test a bit. The gap is real; the
deferral is deliberate for two reasons, both recorded in §8e so the next pass does not rediscover
them. **Every conventional symbol is already taken by something more load-bearing**: `|` is the
pipe, `&` is §11g's background suffix, `^` is §3's force-external prefix (which §3 itself flagged
as an XOR collision when it chose it). So the C spelling is unavailable at any price and the
realistic design is *named* operators — `band`/`bor`/`bxor`/`shl`/`shr` — landing with the `0o`
literal §8e also left out. And **nothing in the system is a bit field yet**: `list` emits `kind`,
not a mode word; capability rights never surface as a number. Designing five operators against a
hypothetical is how a language acquires a vocabulary nobody uses.

Trigger: the first real bit field reaching a `Value` — file modes on `list --long`, or a rights
word made visible to a script.

**Four smaller language deferrals from the v1.2 audit.** Each is in shell design §12 with the same
trigger; recorded here because the canonical-list rule is that a deferral lives in this document,
not only in the doc that decided it.

- **Labelled `break`/`continue` — `TODO(shell-labelled-break)`.** Milestone 4 Part A adds the
  unlabelled pair; labels need their own grammar (`'outer:`) and pay for themselves at a nesting
  depth shell scripts rarely reach, and `return` already covers "get me out of all of this."
  Trigger: real scripts with loops nested deeply enough to need to escape more than one level.
- **Named regex capture groups — `TODO(regex-named-captures)`.** Part F lands positional
  `capture`; names need a name table threaded through the compiler. Trigger: a pattern whose
  groups outlive the line that wrote them — a config parser rather than a one-line filter.
- **Regex-pattern replacement — `TODO(regex-replace)`.** `replace` is literal. The pattern form is
  a separate verb and needs `capture`'s submatch slots to express `$1`-style references at all.
  Trigger: the first script that wants a substitution rather than a whole-value swap.
- **Unicode case folding — `TODO(unicode-case)`.** `upper`/`lower` are ASCII-only, said where a
  user meets it rather than only in the design doc. Trigger: the tables arriving for some other
  reason (text rendering in the compositor is the likely one), since carrying them for two shell
  operators alone is not the trade.
- **A call cannot be nested in an argument list — `TODO(shell-nested-call)`.**
  `format("add={}", add(2, 3))` and `format("x={}", utils.helper(1))` both fail with "expected
  `,` or `)` in an argument list", while a parenthesised *pipeline* in the same position
  (`format("len={}", ("hello" | count))`) parses — which is why the gap went unnoticed.
  **Two independent causes, and a fix for either leaves the other broken; both were measured
  in both directions on 2026-08-18, because the first version of this entry named one cause
  that was wrong and prescribed a fix that does not work.**
  - *Bare* `name(`: calls on a plain name are built in `command_or_ident`
    (`userspace/nxsh/src/parse.rs:980`), which needs the **name** — it selects
    `CallKind::Operator` vs `Def` by testing `OPERATORS`. `paren_args` has already consumed
    that name to see whether a `:` follows, and hands `continue_expr_from` an `Expr`, from
    which the classification can no longer be made. The fix routes bare-`Ident`-then-`(`
    through that same construction; hardcoding `CallKind::Def` instead would silently change
    dispatch for operator names like `count`.
  - *Qualified* `a.b(`: this one **is** a divergence between two copies of one tier.
    `postfix` (`:678`) has a `(` arm, but it matches exactly `Expr::Field(Ident, _)` — the
    §9h qualified-name case — and returns early for anything else. `continue_expr_from`
    (`:1138`) re-implements that tier and omits the arm. Delegating to `postfix` fixes this
    shape and *only* this shape.

  Workaround is the two-line form — `let sum = add(2, 3)` then `format("add={}", sum)` — which
  is what `test-interactive` step 7 uses, with a comment, since it otherwise reads as a detour.
  Found 2026-08-18 while fixing that step's assertion. Trigger: any script that wants a call as
  an argument, which is most of them once user functions see real use.

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

**The three handle-validation paths check the same things in different orders —
`TODO(handle-validation-order)`.**
`docs/spec/handle-encoding.md` § "Validation algorithm" gives twelve steps and says "in order".
`lookup` follows it. `close` and `restrict` do not: they fold the segment and slot bound checks
into one test, and they read the object pointer **last**, where `lookup` reads it at step 6 —
before generation and owner rather than after.

Two consequences, neither fixed. The visible one is a **disclosure difference**: for one closed
handle and a caller who is not the owner, `lookup` answers `InvalidHandle` while `close` and
`restrict` answer `NotOwner` — telling a caller holding no capability that the slot is live and
owned by someone else. The other is a trap for anyone testing this file: the input that isolates
`restrict`'s object-null guard does not isolate `lookup`'s generation guard, because `lookup`
answers at step 6 and never reads the generation. A test built on the wrong order passes against
a build with the guard deleted, which is the vacuity PR #208 existed to remove.

Not fixed there because reordering `close`/`restrict` changes observable error codes for
existing callers, and unifying on the spec's order is a bigger change than the PR's scope.
Found in review of PR #208 (2026-08-18). Trigger: the next change to any of the three ladders,
or the first caller that distinguishes `NotOwner` from `InvalidHandle`.

**Explicit grace-tracker quiescence on the syscall path — `TODO(smp)`.** The handle
table's deferred-close reclamation waits for a grace period tracked per *context id*.

**Corrected 2026-08-18 (audit D.5c): this entry rested on a premise that had been false for
seven weeks.** It said `current_ctx_id()` "returns **0 in production builds** — every CPU
shares one context", and named "a real per-CPU context id, where one CPU's quiescence no
longer implies another's" as the *future* case that would break things. That is the current
implementation. `kernel/src/handle/mod.rs` returns `crate::arch::Smp::current_cpu()`, and has
since `ef47861` (2026-06-29, Phase 3 slice 0) — while this entry was still being revised as
late as 2026-07-24, citing that date's exit-time reclamation sweep. So the hazard it sent a
reader looking for had already arrived, and the entry read as reassurance.

What remains true is the argument that does not depend on the premise: every handle syscall
routes through a `HandleTable` method that takes and drops a read guard, marking that
context quiescent on drop, so deferred closes are reclaimed on the next allocate/close. What
would break it is a syscall path touching the table *without* such a method. Audit A.5
enumerated the syscalls against exactly that question and `restrict` came out as the one that
mutates an entry outside the read guard — its guards are now pinned (PR #208) but the
enumeration is the thing to redo when a new writer appears.

Trigger: a non-method table access, or a `ctx_id` that stops being the CPU id (the Process
slice intends `Process::current().ctx_id()`).

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

### Testing and CI

**Per-case QEMU test framework (`tests/qemu-tests/`).** `cargo xtask test-qemu` adjudicates
the *whole boot* from one `isa-debug-exit` verdict — the self-test payload is the suite, and
CI runs it (2026-07-24). A per-case framework, where an individual test asserts something
the boot chain does not already exercise, remains deferred. Trigger: a case that cannot be
expressed as "the boot completes and the harness agrees". See
`docs/conventions/qemu-integration-tests.md`.

**`REAP_RESERVE` is a userspace-reachable cap that aborts when crossed.** `exit_process`
sweeps every sibling of the exiting process into `reap[this_cpu]` in one `SCHED` hold, and
since 2026-08-14 an over-full push refuses rather than growing (growing was allocation under
the rank-1 lock — F11 — so the refusal is right). `sys_thread_create` has no per-process
thread limit, so a process with more than `REAP_RESERVE` (= `READY_RESERVE`, 32) threads that
block and then exit panics the kernel. Fix shape when taken: reap in batches across several
lock holds, or cap threads per process; both are changes to the exit path rather than to a
constant. Trigger: a per-process thread cap landing, a real workload approaching 32 threads,
or the first report of this panic.

**Every device interrupt is pinned to the BSP, so a BSP park kills all I/O.**
`install_isa_irq` (`kernel/src/arch/x86_64/ioapic.rs:370`) routes each GSI to `Irq::id()` —
the boot CPU — in physical destination mode. `sched::leave_online` (2026-08-13) keeps the
scheduler correct when a CPU parks, but nothing re-routes its interrupts, so a ring-0 fault on
the BSP silently ends disk, serial and PS/2 I/O while the APs keep scheduling. Measured
2026-08-14: parking CPU 3 mid-boot leaves `test-qemu` passing; parking the BSP at the same
tick leaves the machine running — it completes the UI and input self-tests and prints
`init: harness passed` — and then stalls at the login chain, which needs the filesystem. The
parked CPU was running its *idle* thread, so this is interrupt routing and not a stranded
thread. Fix shape when taken: route to a live CPU, and re-route (or broadcast) on park; that
is an interrupt-architecture change, not a scheduler one. Trigger: real-hardware bring-up,
CPU hot-unplug, or any work that makes a ring-0 fault on the BSP something the system should
survive rather than merely diagnose.

**`libkern` mock-syscall test mode.** `userspace/libkern/CLAUDE.md` describes a feature-flagged mock that records and replays syscalls for host-side tests of layers above. The crate is a `cargo new` placeholder in Phase 0. Trigger: real syscalls are defined.

### Auditing and observability

**Comprehensive systemwide tracing infrastructure (DTrace/eBPF equivalent).** Per-CPU ring buffers for kernel tracing exist in concept. A full programmable tracing facility (DTrace probes, eBPF-style filters, etc.) is out of scope initially. Trigger: deep performance analysis needs that exceed what `kprintln!` and basic tracing handles.

### Documentation

**`docs/reference/` catalogues (`docs-reference-catalogues`).** The root `CLAUDE.md` once described `reference/` as holding catalogues for kernel objects, syscalls, errors and syscaps; only `error-codes.md` was ever written, and three documents linked to a `kernel-objects-catalogue.md` that did not exist. <!-- check-docs: allow-missing --> Those links now point at the architecture docs that actually carry the material (`handle-system.md` § "Rights model" for per-type rights, `syscall-abi.md` for the syscall set), and `CLAUDE.md` describes what is really there. A genuine per-object catalogue is still worth having — and unlike the prose docs it could be **generated from source and gated**, which is the one form of reference material that cannot drift. Trigger: the syscall or object set becoming large enough that reading `kernel/src/syscall/table.rs` stops being the faster answer, or the v1.0 ABI freeze, whichever comes first.

## Resolved (kept for the record)

Entries that have been **done**, listed here rather than left in the open sections above —
scanning "what is still owed" has to be reliable. The reasoning for each lives in the
decision log entry for the date shown.

| What was deferred | Resolved | How |
|---|---|---|
| Window titles: `Surface::SetTitle` and `WindowTitle` | 2026-08-25 | Built in M7 Part A, whose window list is the trigger the entry named. The two ops share the protocol's **first variable-length Surface record**: a 4-byte window id then UTF-8 bytes, with **no length field** — the body's own length gives it, and a second one would only be a way for the two to disagree. Over `MAX_TITLE` (256 bytes) is **truncated on a character boundary, not refused** (decided 2026-08-25): `SetTitle` is silent on success and has no reply a client reads, so refusing would need an error path built for the op specified not to have one. The boundary is the load-bearing part — cutting at 256 *bytes* can land inside a character, so a cap meant to bound memory would leave the title not UTF-8 at all. The compositor logs the first truncation and then stays quiet, for the reason a per-event log cost `check-input` on 2026-08-24. **The 256 was very nearly a lie**: the serve loop copied request bodies into a 64-byte buffer, so the real cap was 60 and a title whose 61st byte began a multi-byte character came back `Malformed` instead of shortened — the exact corruption `truncate_title` exists to prevent, reintroduced one frame up the stack, with `truncate_title`'s walk-back unreachable in the shipped binary. Caught in review because every truncation test called `dispatch` directly with a body no client could deliver; the buffer is now sized from `MAX_TITLE` with a `const` assertion tying them together. Not-UTF-8 or too short is `Malformed`; another connection's window is `NotFound`; an unchanged title produces no manager event, because that queue is bounded. |
| Thread placement still targets a CPU that has parked | 2026-08-14 | `pick_target_cpu` and `pick_wake_cpu` now require the lock-free `online_mask` bit as well as `cpu_online[]`, so a core that parked forever is never *given* work. **Not applied to the stealing paths**, deliberately: `steal_one`/`steal_available` choose whom to take work *from*, and a parked CPU is exactly the queue you want drained — gating those would strand the threads it already holds. Demonstrated end-to-end by parking a CPU mid-boot: with the fix `test-qemu` passes, without it the boot self-test fails. |
| `tlb::shootdown` waits on a CPU count that can go stale | 2026-08-14 | The countdown became a per-CPU acknowledgement **bitmask**, and the initiator stops waiting once every outstanding target has left `online_mask`. Sound because a bit is cleared only by `leave_online`, whose only caller is `halt_loop` — after which that CPU executes nothing but the halt loop forever and cannot use a stale translation. Not a timeout: a live but slow CPU stays in the mask and is still waited for. The mask also removes the count's own hazard, where a late decrement could silently satisfy the *next* request. |
| `Ctrl-C` does not reach a running pipeline stage (`interrupt-reaches-a-stage`) | 2026-08-04 | **Two bugs, and my first diagnosis was wrong about both.** I had reported that the tty server never emitted the interrupt event for a line that starts a pipeline; re-probing showed it emits it every time. The event was reaching the shell's channel *before* the shell began waiting on it — and a channel signals its waiters at the moment a message is enqueued, so a waiter that arrives afterwards never sees that edge. The shell slept on a message already sitting in its queue. Both blocking points (the capture read of the tail, and `reap`) now **poll before blocking**. The second bug was `run_line`: the interrupt checkpoint lived only in `exec_block`, so a line typed at a prompt was checked inside its loops and never between its statements — the third time that same rule has been learned in this file, after `hoist_defs` and the stale `cd` guard. |
| `try`/`catch` in expression position (`try-in-expression-position`) | 2026-08-04 | Milestone 4 Part C. The fix was the one the entry predicted — `try` joins `primary` alongside `if` and `match` — and the trigger arrived from two directions at once: Milestone 4 *is* the considered pass over §9c the entry asked to wait for, and retiring the `?` propagation operator left no way to default on failure in expression position, so the recovery form had to land in the same part. It is strictly more than `?` offered: the catch branch sees the error, so the fallback can vary by `kind`, log first, or re-raise. **One node, two entry points** — `exec_try` returns `Flow`, so statement position keeps propagating `break`/`continue`/`return` out of a `try` body (a real regression risk, now a test) while expression position reduces it and refuses control flow. Writing it also turned up that §8c's `primary` production never listed `block`, `if` or `match` either, which is why `try`'s absence had looked deliberate. |
| The shell's console loop has no automated cover (`nxsh-console-tests`) | 2026-08-03 | Resolved by testing the *whole* interactive path rather than extracting the loop. `cargo xtask test-interactive` boots the **release image** — which nothing else boots — and drives login, a wrong password, a shell prompt, a spawned program, a failing stage, cross-line interpreter state, and `exit` → log in again. (It was written because `test-qemu` ran a build in which `session-mgr` auto-logged-in and the interactive path was compiled out; retrofit Part B removed that substitution on 2026-08-21, and the gate is now what exercises the one login path there is.) Expect-driven, so the guest paces it. The rejected alternative was extracting the byte loop into the library half: the continuation *decision* is already host-tested, leaving only ~60 lines of stable byte handling, and a refactor of the critical interactive path is its own risk — while the two bugs it could never have caught (console-lookup rights, session wiring) are precisely the ones that hurt. Non-vacuity checked by reverting the login-echo fix: the run fails at `\npassword:`. |
| The initramfs carries more than boot needs (`initramfs-minimisation`) | 2026-08-03, **re-done 2026-08-11** | The boot image now carries only what is required to reach a mounted root: `init` (the kernel boot-loads it), `fs-server-ext4` (it *is* the mount, and is the only possible restart image for root), `eshell` (the recovery path *for a failed mount*, so it must not live on the filesystem it recovers), and `profile-server` — the one the original rule missed, because `/bin` does not exist until it runs. Everything else moved into a `system` store package and is spawned through `/bin` like any other program: service-mgr, session-mgr, auth-service, logging-service, heartbeat. 1,506,596 → 206,360 bytes, an 86% cut in memory held for the machine's uptime. The recovery path was verified deliberately rather than assumed — a forced mount failure still reaches `eshell>`. **It regressed within the week** and was re-done on 2026-08-11: the display arm put `compositor` and `input-server` back in, and the test build carried five more. The re-do also moved the test programs out, so the initramfs now carries **the same program list in every build mode** (680,068 test / 323 KB release → 232,668 test / 223,888 release, the remaining difference being `init`'s own selftest feature), and added two things the first pass lacked — a list of `(program, why it cannot come from the filesystem)` pairs rather than a list of names, and a size ceiling that fails the build. See the 2026-08-11 decision-log entry. |
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
| `Tty`/`Surface` rsproto category collision | 2026-08-06 | Fixed in the same slice that found it: `Tty` moved from `0x09xx` to `0x0Bxx` and gained a registry row, `Surface` kept `0x09xx`, `Input` took `0x0Axx`. It was filed for about an hour before the maintainer said to fix it rather than carry it. |
| Console DPC freeing in DPC context | 2026-08-06 | Fixed with the PS/2 driver, which turned out to have the same hazard: the DPC now signals through a *borrowed* pointer and marks the parked read `spent`, leaving its `ObjectRef`s owned by the driver until `reap_pending` drops them in thread context. The "needs a bounded parking home and an overflow policy" objection that deferred it was an artifact of assuming the DPC had to own the refs. |
| `test-qemu`'s intermittent hang | 2026-08-06 | Root-caused and fixed: `irp_complete_dpc` freed its `IrpBox` in DPC context, so a completion interrupt landing on a CPU that already held the `SlabCache` lock self-deadlocked against the frame beneath it. The box is now handed to thread context through an intrusive list drained by `reap_pending`. 64 consecutive clean boots against a ~6% base rate. Found by the QMP state dump `cmd_test_qemu` now takes on timeout. |
| `xtask test-qemu` integration harness | 2026-07-14 | Boots the `test-harness` build headless and adjudicates from `isa-debug-exit`. A per-case framework under `tests/qemu-tests/` is still open (below). |
| SMP panic path: no stop-IPI (review F8) | 2026-08-19 | `Cpu::stop_the_machine` sends an **NMI** to every other online CPU, then halts. NMI rather than a fixed vector because the CPUs most in need of stopping are the ones spinning on a lock the faulting CPU holds, and an `IrqSpinLock` holder spins with interrupts masked — a Fixed-delivery IPI reaches every CPU *except* those. Wired to `dump_and_halt` and the panic handler. Measured with a ring-0 fault injected on CPU 2 during boot: `halt_loop` leaves `test-qemu` **PASSED** (the machine boots on without that core), `stop_the_machine` leaves it with no verdict at all. The other half of F8 — interleaved emergency serial output — is **not** fixed and is now narrower: output is exclusive once the others are stopped, but the window between the fault and the NMIs landing is still shared. Decision log, 2026-08-19. |
| A deterministic gate for the i8042 recovery sweep | 2026-08-19 | `cargo xtask check-input --no-ps2-irq` boots the same image with the kernel's `no-ps2-irq` feature, which skips only the `CONFIG_KBD_IRQ \| CONFIG_AUX_IRQ` write in `arch::ps2::arm` — the controller never asserts either line, so `drivers::ps2::poll` is the only path a byte can take, and the gate's assertions are unchanged. Measured with the sweep deleted: this fails on the first injected key while `check-input`, `check-terminal`, `check-display` and `test-qemu` all still pass, so it is the only gate in the tree that can. Runs in the input workflow. Decision log, 2026-08-19. |
| Promote `check-terminal` to CI | 2026-08-18 | Runs as `check-terminal --kvm` in the QEMU integration job, unconditional — its coverage is the compositor-to-shell round trip, which `check-input` (stops at the client's event log) and `check-display` (never types) do not reach, so there is no useful path filter. The stated trigger was ~10 consecutive passes; it had 64. The blocker was never the count but the **one unreproduced failure** the audit logged at the click step: it is still unexplained, and what made promotion defensible is that the gate now asserts *where* the press landed before asserting that `nxterm` received it, so a recurrence reports coordinates rather than a bare timeout. That assertion also closed a blind spot — the old gate passed with a motion packet dropped, having never checked the cursor reached the point its arithmetic named. Decision log, 2026-08-18. |
| Debug-build lock-ordering enforcement | 2026-07-29 | `kernel/src/libkern/lockrank.rs` is the rank tracker `kernel/CLAUDE.md` promised — 777 lines, live in every image `xtask` builds, gated in CI by `cargo xtask check-irq-scope`, and (since PR #202) covered by tests that fail when its arithmetic is broken. The open entry claiming "the mechanism doesn't yet exist" was written 2026-05-19 (`b1a71f7`) and never revisited — so it outlived the mechanism (`e93d52c`, 2026-07-29) by **three weeks**, sitting 100 lines above the row below it, which only makes sense as a refinement *of the tracker it said was missing*. Found by the 2026-08 audit, D.5(a). |

## How to use this document

When you encounter something that seems unimplemented or absent, check this document first. If it's listed here, the absence is intentional; the reasoning is preserved. If it's not listed here and you think it should be, consider adding an entry — the document is append-only-with-revisions, not a static snapshot.

If you're triggering a deferred item (starting work on TCP/IP, beginning aarch64 port, etc.), update this document at the same time. The deferred entry should **move to the Resolved table**, not be left in place with a status note appended — an open section that mixes finished work with owed work cannot be scanned, which is how three deferrals went unnoticed until a consumer tripped over them (the 2026-07-24 audit).

**A deferral only exists if it is in this document.** The three gaps that cost the most to
rediscover — exit-time handle reclamation, the wall clock, file truncate — were each
"recorded" somewhere else: a sentence in an architecture doc promising a later slice, a
`TODO(...)` in the syscall table, a bullet in a crate's `CLAUDE.md`. None was in this list,
so none was ever reviewed. If you write a stub, a `TODO`, or prose that promises future
work, mirror it here.

`cargo xtask check-deferrals` enforces **both directions** of that, since 2026-08-18:

- every `TODO(<tag>)` in `kernel/src`, `userspace` or `tools/xtask/src` names an entry here;
- every tagged entry in the **open** section above has a marker somewhere in that code.

Until then it asked only the first question, and the second is where the rot was — 9 of the 28
open entries bound to no marker at all (`atomic-log-lines`, `history-pager`,
`regex-named-captures`, `regex-replace`, `shell-bitwise`, `shell-labelled-break`,
`stack-attribution`, `tty-server`, `unicode-case`), four of them appearing nowhere else in the
repository. So for a third of the tagged entries the enforcement this paragraph advertised was
an empty set, failing silently in the direction that matters: a deferral whose marker is gone is
one nobody trips over while editing the code. All nine now have markers, placed where the
limitation actually lives rather than where it was convenient.

**Open section only, and that boundary is the point.** A resolved entry has no marker *because
closing it deleted the marker* — that is the lifecycle working, and the first attempt at this
measurement got it wrong: a whole-file scrape counts the prose `TODO(<tag>)` in this section,
and counts tags named narratively inside Resolved rows. `shell-cwd` is one, in a row that
records how closing that deferral found two stale `TODO(shell-cwd)` markers in `nxsh` and
deleted them. Counting that as rot would have sent this very change to reinstate the marker
whose removal the row describes.

**If a deferral genuinely has no code site yet**, mark the entry
`<!-- check-deferrals: no-code-site -->` on the same line as its tag and the gate will accept
it. Nothing uses that today — every open entry turned out to have somewhere honest to put a
marker — so it is covered by a unit test rather than by a live example, deliberately: an escape
hatch nobody has opened is exactly the thing that does not work the first time it is needed.
Reach for it only when the alternative is a marker nobody would ever edit past; a note at the
place someone *would* change is worth more than a clean gate.

That per-entry question — *where would the marker go for a feature that does not exist yet?* —
was the reason the reverse direction was filed separately from the audit that found it. The
answer turned out to be that every one of the nine had somewhere honest: the prose describing
each limitation already existed, and only the searchable tag was missing.

The decision log (`docs/decision-log.md`) is the place to record the actual decision when a deferred item moves into active work — what triggered it, what the implementation approach is, when the decision was made.
