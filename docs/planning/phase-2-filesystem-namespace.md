# Nitrox Implementation Plan — Phase 2 — Filesystem and namespace

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. Phases 0–3 are
complete; Phase 4 is active.

---

## Phase 2: Filesystem and namespace — **COMPLETE (2026-06-26)**

**Goal:** the namespace subsystem, the resource server protocol, the first real filesystem. Init runs, processes its bootstrap manifest, mounts ext4, reads files.

> **Status: complete.** The prerequisite band + slices 1–9 are all done and the
> milestone below is met and QEMU-proven (Limine → kernel/PCI → init from
> initramfs → spawn fs-server-ext4 → mount ext4 `/` → read `/system/current-generation`
> → reaping loop, now also dropping to `eshell` on a critical-path failure).
> **Slice 10 (FAT, read-only) is deferred to Phase 3** — parity-only, not on the
> boot path. The one quality issue surfaced at close (single-page demand-fault
> latency, ~325 ms/page) is a documented Phase-3 optimization, mitigated for now
> by trimming the `large.bin` fixture 64 → 8 pages. See the decision log
> (2026-06-26, Phase 2 close).

### Tasks (in suggested execution order)

> **2026-06-11 re-sequencing (stock-take after Phase 1).** The original
> Phase 2 ordering silently assumed several pieces of infrastructure that do
> not yet exist, and had one internal ordering inversion. A dependency audit
> (see the decision log entry of 2026-06-11) found:
>
> - **The "async-I/O slice" is referenced but never defined.** The Phase 1
>   status note and the IPC slice both defer `PendingOperation` + blocking IPC
>   send "to the async-I/O slice," but no slice built it. Every block-device
>   read (AHCI → fs-server → page cache) is an async operation that needs it.
> - **Device IRQs need an IOAPIC, which needs ACPI MADT parsing** — Phase 1
>   shipped LAPIC-only and deferred IOAPIC "to Phase 2" without giving it a
>   slice. PCI ECAM likewise needs the ACPI MCFG table. (This is the small
>   pure-Rust *table-parsing* layer, distinct from the ACPICA/AML work that is
>   correctly deferred to its own trigger — see `why-phased-acpi.md`.)
> - **The DPC/softirq queue** was deferred in Phase 1 "until a device-IRQ
>   consumer exists" — storage drivers are that consumer.
> - **The page cache needs a demand-paging `#PF` handler** (not-present →
>   VMA lookup → fault-in) and the `MappingKind::FileBacked` VMA variant —
>   **both now landed** (`phase-2/demand-paging`): `AddressSpace::fault_in`
>   resolves not-present user faults and the `FileBacked` variant + dispatch
>   arms await the page cache's producer.
> - **Entropy was listed both as its own slice and as an item inside the
>   in-kernel-RS slice** (`/dev/entropy`), a forward self-reference.
> - **FAT was justified as "required to boot Limine"** — false; UEFI/Limine
>   read the ESP, not Nitrox. Nothing in the Phase 2 milestone consumes it.
>
> The missing infrastructure is now scheduled explicitly as a **prerequisite
> band** ahead of slice 1, the slices are reordered, and the misleading notes
> are corrected. (These prerequisites are genuine Phase 2 feature work; they
> are distinct from the Phase 1.5 code-quality hardening pass also recorded
> in the decision log on 2026-06-11.)

#### Phase 2 prerequisites (land before the namespace slice)

These were implicit in the original plan; each gates one or more later slices.
Author the two missing architecture docs first — slices 1 and 5 implement
*against* contracts that have not been written.

- [x] **Architecture docs.** `docs/architecture/drivers-and-irps.md` (the IRP /
  completion-routine / `InterruptObject` contract the storage slice implements)
  is **done** (`phase-2/drivers-irps-doc`). `docs/architecture/namespace-and-resource-servers.md`
  (the namespace data model + resolution + async-lookup contract + the
  resource-server model — `KernelServer`/`UserspaceServer`/`OpStatus`/registry) is **done**
  (`phase-2/namespace-design`) — it gates slice 1.
- [x] **ACPI table parser** (pure-Rust RSDP → XSDT/RSDT → MADT + MCFG; no AML).
  Enables IOAPIC (MADT) and PCI ECAM (MCFG). No external crate. Gates the
  IOAPIC and storage slices. **Done** (`phase-2/acpi-tables`): behind a new
  arch-neutral `ArchPlatform` trait (`arch/platform.rs`) — the x86 ACPI parser
  (`arch/x86_64/acpi.rs`) exposes only the PCIe ECAM regions neutrally; the
  MADT interrupt-routing facts (IOAPIC/GSI/source-overrides) stay arch-internal
  for the IOAPIC item. See the decision log (2026-06-11).
- [x] **IOAPIC bring-up + external IRQ routing.** The Phase-1 `ArchIrq`
  deferral (LAPIC-only). Without it no device interrupt is deliverable, so
  AHCI cannot signal completion. **Done** (`phase-2/ioapic`): a new
  arch-neutral `ArchIrqRouter` trait (`arch::IrqRouter`, x86 impl `X86IoApic`,
  distinct from `ArchIrq` the per-CPU local controller) + IDT device-IRQ vectors
  (0x30..) with a handler registry; brings up the IOAPIC from the cached MADT
  facts, masks the 8259s, and a PIT self-test proves GSI→IOAPIC→vector→ISR→EOI
  end-to-end. See the decision log (2026-06-11). (The `IrqSpinLock` audit for
  new IRQ-reachable locks lands with the DPC item / real device handlers.)
- [x] **DPC / softirq queue** (the Phase-1 "DPC integration for wakeup"
  deferral). Device IRQ handlers defer their real work here (no allocation /
  unbounded work in IRQ context). **Done** (`phase-2/dpc`): `kernel/src/dpc.rs`
  — an inline `Dpc { handler, ctx, queued }` + a pre-reserved global queue
  (single-CPU stand-in, per-CPU at SMP); `enqueue` from an ISR, `run_pending`
  drained at the interrupt-dispatch tail (a leaf `IrqSpinLock`). The timer's own
  deadline-firing stays inline (timekeeping work, not migrated — a correction to
  `drivers-and-irps.md`); the queue serves device ISRs. Proven by the PIT
  self-test driving a DPC end-to-end. See the decision log (2026-06-12).
- [x] **Demand-paging `#PF` handler** (not-present fault → active-AS VMA
  lookup → fault-in) **+ `MappingKind::FileBacked`** VMA variant. **Done**
  (`phase-2/demand-paging`): `pf_dispatch` offers a not-present ring-3 fault to
  `AddressSpace::fault_in` (VMA lookup → access check → alloc-zero-map-flush)
  before the fatal SegFault path; `map_vma_lazy` reserves anonymous ranges
  unbacked and the ELF loader reserves user stacks this way (PT_LOAD stays
  eager — file bytes). `MappingKind::FileBacked` + its dispatch arms exist for
  the page cache (no producer yet). Proven by a boot smoke test + the userspace
  demo running on a demand-faulted stack. Unblocks lazy `MemoryObject` paging
  (the `MAX_SIZE` cap — needs a sparse object frame table + accounting) and the
  page cache. See the decision log (2026-06-12).
- [x] **`PendingOperation` kernel object + `sys_wait` I/O-completion
  integration** (the long-promised "async-I/O slice"). **Done**
  (`phase-2/pending-operation`): a one-shot waitable `PendingOperation`
  (`object/pending_op.rs`) wired into the generic wait/wake machinery (3 sched
  dispatch arms + `signal_pending_op`); `sys_wait` reports its completion status
  via `IoResult.status`. First consumer: the IPC **`Block`** send mode — a full
  ring holds the message in a per-endpoint pending-sender queue and returns a PO
  that completes (the message delivered) when the peer next receives; close
  completes held senders `PeerClosed`. Proven by host tests + a parent demo
  (`blocking send completed via PendingOperation`). **`BlockBounded`** (the
  deadline-bounded variant) is carved out to its own follow-up (it needs the
  deadline-heap kind extension + a `sys_channel_send` deadline arg) — still
  `Unsupported`. `IoRing` lands with the rsproto transport when needed. Gates the
  storage, fs-server, and page-cache slices. See the decision log (2026-06-12).
- [x] **IPC `BlockBounded` send mode** (follow-up to the above). **Done**
  (`phase-2/block-bounded`): the deadline-heap `Entry` grew a 3-way kind
  (`Thread`/`Timer`/`PendingSend`) + channel back-pointer; a timer-tick arm cancels
  a held send whose delivery deadline elapsed (PO completes `TimedOut`); a 6th
  `sys_channel_send` arg carries the deadline. Timed-out sends are reclaimed
  outside `SCHED` via **reclaim-on-recv** (swept on the next recv / at close).
  Proven by host tests + a parent demo (`blocking send timed out via
  PendingOperation`). See the decision log (2026-06-12).
- [x] **DMA-capable allocation** (page-multiple alignment / a `dma_alloc`
  path; the `align > SLAB_SIZE` deferral). **Done** (`phase-2/dma-alloc`):
  `mm::dma::DmaBuffer` — an RAII, zeroed, physically-contiguous, page-aligned
  block from the buddy allocator (order-`k` blocks are `2^k × PAGE_SIZE`-aligned)
  exposing both a CPU/HHDM pointer and its `phys()` address, for AHCI command
  lists / FIS / PRDTs. DMA **zones** stay deferred (no address-constrained device
  on the no-legacy baseline). Proven by host tests + a boot smoke test. See the
  decision log (2026-06-12).

> **The Phase 2 prerequisite band is complete.** All seven prerequisites —
> drivers-and-IRPs doc, ACPI tables, IOAPIC, DPC queue, demand paging,
> `PendingOperation`/async-I/O + IPC `Block`/`BlockBounded`, and DMA-capable
> allocation — have landed. Phase 2 proper (the storage slice → fs-server → page
> cache) can begin.

#### 1. Namespace foundation (the per-process name-resolution substrate)

Design: [`docs/architecture/namespace-and-resource-servers.md`]. Broken into a
docs-first design pass (**done**) + three code parts, each its own PR. The
`UserspaceServer` trait / `OpStatus` / registry / IPC-forwarded lookup are *designed*
here but **implemented with slice 3** (resource servers) — there are no servers to
route to until then. Lookup is a `PendingOperation` from the start (a real lookup
forwards over IPC → async); slice 1 binds **direct handles** and returns a
pre-signalled PO carrying the resolved handle via `IoResult.result`.

- [x] **Part A — design doc** (`phase-2/namespace-design`): the model, path grammar,
  longest-prefix resolution, binding kinds, async-lookup contract, capability model,
  cache, kernel/userspace split, slice-1-vs-slice-3 scope. Spec: `sys_ns_*` numbers
  22–25 reserved + `IoResult.result` word noted.
- [x] **Part B** (`phase-2/namespace-object`, PR #41) — `Namespace` kernel object +
  binding store + longest-prefix resolution engine (host-tested; no syscalls).
- [x] **Part C** (`phase-2/namespace-syscalls`) — `IoResult.result` (16→24 B) +
  `PendingOperation` result payload; the four `sys_ns_*` syscalls (lookup →
  pre-signalled PO carrying the resolved handle; resolution failures via the PO's
  `NotFound` status, arg/permission/alloc failures synchronous; bind gated by the
  `BIND` handle right, `BIND_NAMESPACE` syscap deferred to the syscap model);
  `Process::namespace` field + boot-time root namespace for pid 1 (handle in `rsi`);
  QEMU `ns_demo` create→bind→lookup→wait→use→unbind.
- [x] **Part D** (`phase-2/namespace-inherit-cache`) — per-`Namespace` lookup cache
  (path→binding-index, flush-on-mutation); spawn-time namespace inheritance via a
  4-register bootstrap ABI (`rdi`=notif, `rsi`=namespace, `rdx`=installed[0],
  `rcx`=arg0) + a `SpawnArgs.namespace` field (`0`=inherit, else a constructed
  restricted namespace; child gets a LOOKUP-only handle); boot banner → Phase 2.
  **Namespace foundation (slice 1) complete.**
- *(slice 3)* `UserspaceServer` trait, `OpStatus`, `UserspaceServerRegistry`,
  IPC-forwarded lookup + cross-context handle install.

#### 2. Entropy

Moved ahead of the in-kernel resource servers: the `/dev/entropy` server in
the next slice depends on this subsystem (the original plan listed it in both
places — a forward self-reference). Design:
[`docs/architecture/entropy.md`]. Broken into a docs-first design pass + three
code parts, each its own PR (mirroring the namespace slice). The read interface is
async by contract (a `PendingOperation` when unseeded) but the pool seeds at boot,
before userspace, so reads are synchronous in practice.

- [x] **Part A — design doc** (`phase-2/entropy-design`): sources (RDSEED/RDRAND +
  TSC jitter), the pool + seeded gate, ChaCha20 + fast-key-erasure + reseed policy,
  boot integration, the `EntropyObject` read contract, lock discipline, kernel/
  userspace + slice-2/slice-3 scope. Spec: `sys_entropy_create = 26` /
  `sys_entropy_read = 27` reserved.
- [x] **Part B** (`phase-2/entropy-csprng-hwrng`, PR #45) — hand-rolled ChaCha20
  CSPRNG (RFC 8439 vectors) with fast key erasure + arch HW-RNG access
  (`arch::Entropy`: RDSEED preferred, RDRAND fallback; CPUID-detected). Host-tested.
- [x] **Part C** (`phase-2/entropy-pool-seeding`) — entropy pool + boot seeding +
  TSC-jitter mixing at interrupt dispatch + periodic/byte-threshold reseed + the
  256-bit seeded gate; the handle-table free-list PRNG now seeds from the CSPRNG
  (`PHASE1_SEED` removed). One `IrqSpinLock<EntropyState>` leaf. QEMU opts in
  `+rdrand,+rdseed`; boot shows `seeded=true`.
- [x] **Part D** (`phase-2/entropy-object-syscalls`) — `EntropyObject` kernel object
  + `sys_entropy_create` / `sys_entropy_read` (returns `0` on synchronous fill when
  seeded; a `PendingOperation` when not, with the seed-latch waking PO waiters from
  the timer tick) + QEMU demo. **Entropy subsystem (slice 2) complete.**

#### 3. In-kernel resource servers

**Scope (decided 2026-06-22 — see decision log).** Slice 3 builds the **in-kernel**
resource-server framework and the servers with an immediate consumer/demo. In-kernel
servers dispatch by **direct kernel function call** (no IPC); the kernel binds them
into pid 1's root namespace at boot, so the whole slice is demoable via the existing
parent process without init.

Deliberately deferred (build-when-consumed, to avoid large unexercised machinery):

- **Userspace-RS path** — IPC-forwarded lookup, cross-context handle install,
  `librsproto`, and the Ready handshake → **slice 7** (the fs-server is the first
  userspace-RS consumer).
- **`/initramfs` + CPIO + `sys_release_initramfs`** → **slice 4** (Init, its only
  consumer).
- **`/dev/framebuffer`** → deferred (needs userspace framebuffer mapping, not built).
- **The filtered/full process server** (`/proc/<pid>`, enumeration) → a later slice:
  it needs a global process registry *and* is the ambient-authority-sensitive
  surface (see the `/proc/self` note below). Slice 3 ships **only `/proc/self`**.

Broken into a prerequisites pass + docs-first + two code parts (mirrors slices 1/2):

**Part 0 — fault diagnostics prerequisite (done).** Motivated by the slice-2 entropy
demo's "hang"; landing it first makes all later slice-3/Init debugging tractable.
Measuring before building (see `decision-log` 2026-06-22, Part 0) corrected two of
the planned premises:

- [x] **Surface unhandled ring-3 faults** (`phase-2/slice3-userspace-rt-fault-diag`).
  A fault that leaves **no runnable thread** to service it (notably an init/pid-1
  crash) suspended silently — a hang. `sched::suspend_with_fault` now detects the
  *scheduler-stranded* case (the dequeue falls through to idle, so no thread remains
  to receive the notification + `sys_exception_resume` it) and emits a last-ditch
  diagnostic (`pid/tid/kind/addr`) via the emergency serial writer. Fires only for
  genuinely-stranded faults — a serviced fault (the worker demo) wakes its supervisor
  before the dequeue and stays silent. (The naïve "no notification channel" condition
  was rejected: pid 1 *has* a channel — it services its own faults — so that check
  never fires for it.)
- ~~Freestanding-userspace mem intrinsics~~ — **dropped (not needed).** Measurement
  showed `compiler_builtins` already supplies `memcpy`/`memset`/`memcmp`/`memmove`
  on-demand for `x86_64-unknown-none` (the kernel defines all four; the parent links
  `memcmp` with zero undefined symbols). The original `a != b` "hang" was a separate
  inlined-`[u8; N]`-equality codegen quirk (infinite loop, no `memcmp` call), not an
  intrinsics gap — documented as a known issue; userspace keeps the manual-loop idiom.

**Part A — design doc (done, `phase-2/slice3-rs-framework-design`).** Formalized the
in-kernel RS framework into `docs/architecture/namespace-and-resource-servers.md`
(extended in place — it's the living RS doc): the kernel-server dispatch model
(`lookup(suffix, rights) -> OpStatus::{Completed(handle) | Rejected(err)}`; `Pending`
reserved for slice 7), the `BindingTarget` enum (`DirectHandle` + `KernelServer`;
`UserspaceServer`/IPC + `SubNamespace`/`Rewrite` deferred), how lookup dispatches
**synchronously** and reuses the slice-1 pre-signalled-PO delivery (`IoResult.result`),
boot-time binding into pid 1's root namespace, the per-server content model (a lookup
returns a handle to a kernel object), and the `/proc/self` authority model below.

**Part B — the framework + `/dev/entropy` (done, `phase-2/slice3-kernel-server-framework`).**
`object/kernel_server.rs` (`KernelServerId`, `OpStatus::{Completed|Rejected}`, the
`dispatch` registry); `BindingTarget`/`ResolvedTarget` in `namespace.rs` (replacing the
bare `ObjectRef` target; `bind_kernel_server`; `unbind`/`resolve` updated, drop
discipline preserved); `sys_ns_lookup` calls a server synchronously → installs the
rights-attenuated handle → pre-signals the PO. The **whole `/dev/entropy` server** was
folded in as the demonstrator (entropy is complete; it closes the loop that motivated
landing entropy first) — bound into pid 1's root namespace at boot (`main.rs`),
inherited by children, exercised by a `parent` QEMU demo (resolve → read). Host-tested
(`kernel_server` dispatch + `namespace` bind/resolve/unbind). No ABI-hash impact.

**Part C — the remaining servers + demo.**

- [x] `/dev/entropy` — lookup returns an `EntropyObject` (reuses slice 2;
  `sys_entropy_read` on the resolved handle). **Landed in Part B** as the framework
  demonstrator.
- [x] `/proc/self/*` — **self-reference only**: `process`/`thread`/`namespace` resolve
  to the **caller's own** objects (from the calling syscall context, no pid parameter).
  **Done** (`phase-2/slice3-proc-self`): per-leaf `KernelServer` bindings with
  type-correct rights (`process`/`thread` → `SIGNAL|TERMINATE`+generic; `namespace` →
  `LOOKUP`+generic, no `BIND`); `sched::current_thread()` added; bound into pid 1's root
  ns at boot; QEMU demo stats process/thread + resolves `/dev/entropy` through the
  returned namespace handle. Registry-free; no cross-process access.
- [ ] *(deferred)* `/proc/self/status` — numeric pid/tid snapshot. Needs a
  `MemoryObject` synthesis primitive (or extended handle introspection); the scalar-via-
  `IoResult.result` shortcut was rejected. See `deferred-decisions.md`.
- [ ] *(deferred)* `/dev` directory stub — `DeviceNode` has no struct, no enumeration
  syscall, no consumer; deferred to a device manager (slice 7) / enumeration. See
  `deferred-decisions.md`.
- [ ] *(deferred)* `/dev/log` — a readable kernel-log snapshot needs a log ring buffer
  (new infra) + the same synthesis primitive.
- [x] QEMU demo: the parent looks these up and uses the results.

> **`/proc/self` authority (no ambient authority).** Reachability is by **namespace
> construction** — `/proc/self` resolves only if a supervisor bound it (a sandbox may
> omit it; it is *not* a kernel-forced universal). What it returns is strictly the
> **caller's own** resources, derived from the running context — there is **no pid
> parameter to forge**, so it grants nothing about other processes (and returned
> handles are still owner-pid-checked on use). Cross-process introspection
> (`/proc/<pid>`, enumeration) is a **separate, narrowly-bound** capability
> (init/admin namespaces) with its own registry — deferred, not built here. See
> `os-design-v5.1.md` §"Synthetic /proc/self" + the namespace-composition examples
> (standard user → filtered `/proc`; admin → full `/proc`; sandbox → none).

#### 4. Init (PID 1) — bootstrapping form

This slice lands a *bootstrapping* init: it starts (handle-set reception, TOML
parsing, reaping loop) on top of slices 1 and 3. Its full critical-path mount
loop is not milestone-complete until the storage + fs-server slices (5–8)
land; see the milestone note.

The **initramfs substrate** lives here (moved from slice 3, 2026-06-22) — its only
consumer is init reading `init.toml` + spawnable images, so it lands where it's used.
It reuses the slice-3 in-kernel RS framework: `/initramfs` is just another in-kernel
server, bound at boot.

Decided as the userspace library scope for this slice (2026-06-23): pull forward only a
real **`libkern`** (init's mandated foundation); `libos`/`librt`/`libstream` stay
Phase 3, `librsproto` slice 7. Path-based spawn + relocating the demos onto the
initramfs defer to slice 7 (driven by fs-servers). Done as ordered PR parts:

- [x] **Part 1 — real `libkern` + migrate the demos** (`phase-2/slice4-libkern`, PR #53):
  the canonical userspace ABI mirror (`syscall`/`error`/`handle`/`abi`/`debug`);
  parent/child/hello migrated off ~485 lines of triplication; host tests in
  `cargo xtask test`.
- [x] **Part 2 — initramfs substrate** (`phase-2/slice4-initramfs`): Limine module
  request (`kernel/src/limine.rs`) + `boot/limine.conf` module + xtask CPIO-newc packer;
  in-kernel CPIO-newc parser (`kernel/src/initramfs.rs`, host-tested); the `/initramfs`
  `KernelServer` (first subtree server) returning a read-only `MemoryObject` copy via the
  new `MemoryObject::try_new_filled`; bound into pid 1's root namespace at boot. Verified
  by the parent demo resolving+mapping `/initramfs/etc/init.toml`.
- [x] **Part 3 — init crate skeleton** (`phase-2/slice4-init-skeleton`):
  `userspace/init` as a bare-target `#![no_std]`+`alloc` lib+bin (libkern only); static-
  arena bump `#[global_allocator]` (host-tested); `_start` handle-set reception + alloc
  proof + clean exit; spawnable via `ImageId::Init` and reaped by the parent demo.
  Surfaced + fixed two userspace-runtime bugs init's first `alloc` use hit: a mis-placed
  `compiler_builtins` `memcpy` (now strong `libkern::mem` intrinsics) and a `/DISCARD`-ed
  `.got` (now kept in all four `user.ld`). See the decision log (2026-06-23).
- [x] **Part 4 — minimal TOML parser + init.toml manifest** (`phase-2/slice4-toml`):
  `init::toml_lite` (the `[[mount]]` / `[mount.options]` / scalar subset) +
  `init::manifest` (`MountSpec` validation + shallowest-first topo-sort), per
  [docs/spec/init-toml-schema.md]. 15 host tests; an on-target smoke test parses an
  embedded sample. The mount-processing loop stays Part 5 / slice 7.
- [x] **Part 5 — init becomes PID 1 + reaping loop + bootstrap skeleton**
  (`phase-2/slice4-init-pid1`): kernel boots init (`ImageId::Init`); init reads+parses
  the real `/initramfs/etc/init.toml`, logs the topo-sorted mount plan, spawns `parent`
  (`ImageId::Parent`) → `child`, and runs the reaping loop. Process tree is now
  init (1) → parent (2) → child (3/4). The mount loop stops before the Ready handshake
  (slice 7); `parent`'s `ns_demo` rebased onto a fresh namespace (its inherited root is
  LOOKUP-only under init). Required + depends on the GS-base `#DF` fix (PR #57).
- [ ] ~~`sys_release_initramfs`~~ — **deferred** to the general resource-server
  lifecycle work (load/unload for kernel + userspace servers); the blob stays mapped
  through bootstrapping. See `deferred-decisions.md`.

#### 5. Storage drivers — **complete**

Depends on the prerequisite band (all complete): ACPI MCFG (ECAM), IOAPIC
(device IRQs), the DPC queue (completion handling), `PendingOperation` (async
reads), DMA allocation (`mm::dma::DmaBuffer` — command lists / PRDTs), and the
uncached (`PageFlags::NO_CACHE`) mapping path for BAR access. Staged as ordered
PR parts (all merged); the Part 0 design decisions are in the decision log
(2026-06-23). End-to-end result: a userspace process resolves `/dev/blk/0` and
reads disk sectors via `sys_io_submit` against the real AHCI controller.

- [x] **Part 0 — specs & decisions** (docs only): the storage-slice ABI and
  object contracts settled before the ABI hash bakes them in.
  [`io-operation.md`](../spec/io-operation.md) (`IoOp`/`IoOpcode`),
  [`irp-layout.md`](../spec/irp-layout.md) (`Irp` + sub-types),
  [`device-node.md`](../spec/device-node.md) (the `DeviceNode` object, resource
  descriptor, `/dev/blk` naming + registry); `syscall-abi.md` /
  `abi-version-hash.md` / `deferred-decisions.md` updated. Key calls: block I/O
  via the generic `sys_io_submit` (28) on a `DeviceNode` handle (no new
  `KObjectType`); one `KernelServerId::BlockDevice` + a kernel registry for the
  dynamic disks; `InterruptObject` built this slice; in-kernel MMIO (not
  `sys_device_map_mmio`).
- [x] **Part 1 — PCI(e) enumeration + `DeviceNode`** (`phase-2/slice5-pci-enum`).
  ECAM walk over `arch::platform::pcie_ecam_regions()` via a single repointed
  uncached scan window (`mm::kvmap::remap_mmio_page`); decode identity/class,
  size BARs (32/64-bit + I/O), read the interrupt line/pin; `DeviceNode` kernel
  object (`object/device_node.rs` + the `dispatch_destroy`/type-rights arms);
  boot-time `device::init()` enumerates into a global table and logs each
  function. Host-tested against a synthetic config space (BAR sizing incl.
  64-bit). No driver claims a node yet. QEMU: discovers the ICH9 AHCI controller
  (`8086:2922` class `01.06.01`) + its ABAR (BAR5) and 5 other functions; boot
  proceeds to init→parent→child cleanly. (Per [io-operation], [irp-layout],
  [device-node].)
- [x] **Part 2 — IRP framework + `InterruptObject` + the I/O core, on a ramdisk**
  (`phase-2/slice5-irp-iocore`). `Irp` + sub-types (`io/irp.rs`, offsets pinned by
  asserts); `InterruptObject` waitable (`object/interrupt_object.rs`, a latching
  edge-counter; 3 sched dispatch arms + `signal_interrupt` from a DPC + consume at
  `sys_wait`); the block I/O core (`io/block.rs`: a `BlockBackend` fn-pointer on
  `DeviceNode` + `dispatch_block_irp` + the `IrpBox` owning wrapper + the
  completion DPC); `sys_io_submit`(28)/`sys_io_cancel`(29, `Unsupported`);
  `IoOp`/`IoOpcode` in both libkerns. Proven by a boot self-test (`io::self_test`)
  on a RAM-backed device (`io/ramdisk.rs`): read 8 KiB → DPC → PO completes
  (status 0, result = bytes) → buffer content verified; and a DPC signals an
  `InterruptObject` → latch → consume. Independent of AHCI register/DMA work.
- [x] **AHCI driver (Part 3)** (`phase-2/slice5-ahci`). Tier 1 driver
  (`drivers/ahci.rs`): `mm::kvmap::map_mmio` of the ABAR (uncached), HBA/port
  bring-up, polled `IDENTIFY DEVICE`, command list / FIS / command-table+PRDT in
  `DmaBuffer`, `READ DMA EXT` issued against the IRP's buffer fragments (the
  controller DMAs straight into the client's `MemoryObject` frames); real IRQ via
  a neutral `arch::install_pci_irq` free function (GSI from the PCI interrupt-line register →
  ISR → IRP completion DPC → PO; the ISR also signals the controller
  `InterruptObject`). `drivers::probe` matches class-`01.06.01` and publishes the
  disk as a block `DeviceNode`. Proven against the **existing AHCI boot disk** (no
  new disk needed): `drivers::self_test` reads sector 0 and verifies the `0x55AA`
  boot signature, mirroring the PIT self-test's brief interrupt window (with a
  polled fallback). QEMU: HBA up, port 0 disk (64 MiB), `read self-test OK …
  via IRQ`, 4/4 clean boots. Phase 2 scope: single controller/single SATA disk,
  one outstanding command; multi-port/NCQ/MSI and ACPI `_PRT` routing deferred.
  The dedicated `xtask build-disk` + ext4 test disk move to Part 4/7 (fs-server).
  `KError::IoError` (-40) added (both libkerns) for device/medium errors.
- [x] **Block device resource server registration (Part 4)**
  (`phase-2/slice5-block-server`). `KernelServerId::BlockDevice` + the
  `block_device_server` (parses the suffix as a decimal index → resolves the
  n-th block-class `DeviceNode` via `device::find_block_device`, the device-table
  registry). The supervisor binds `/dev/blk` (read-only) into init's root
  namespace at boot, **unconditionally** (the registry carries liveness). Disks
  resolve at **`/dev/blk/0`** (component-boundary matching — not `/dev/blk0`). The
  `parent` userspace demo resolves `/dev/blk/0`, `sys_io_submit`s a 512-byte read,
  `sys_wait`s, and verifies the `0x55AA` boot signature — the full userspace block
  path the kernel self-tests stood in for. QEMU: `parent: /dev/blk/0 read OK
  (sector 0 boot sig 0x55AA)`, 4/4 clean boots. (The dedicated `xtask build-disk`
  + ext4 disk arrive with the fs-server, slice 7.)

#### 6. Partition handling — **complete** (`phase-2/slice6-gpt`)

The first **two-layer block IRP stack**: a partition `DeviceNode` rebases a
partition-relative offset to disk-absolute and forwards to the disk (realised by
`BlockBackend` delegation — `io::block::Partition`/`partition_rebase`, not formal
`stack_index` descent; the latter stays designed-ahead for filter drivers).

- [x] **GPT driver (Tier 1)** (`drivers/gpt.rs`): parses LBA 1 (`EFI PART` +
  bounds; CRC deferred) and the entry array, reading the disk synchronously at
  boot via the new `io::block::read_blocking` (a polled read using the new
  `BlockBackend::poll`, since interrupts are masked at probe time).
- [x] **Partition DeviceNode registration**: each used entry becomes a block
  `DeviceNode` over an `io::block::Partition` window, registered in the device
  table — so it also resolves at `/dev/blk/<n>` (the ESP at `/dev/blk/1`).
- [x] **`/dev/disk/by-partuuid/*` + `/dev/disk/by-partlabel/*`**: stable
  direct-handle bindings created at boot (`gpt::bind_partition_names`); the GUID
  is formatted GPT mixed-endian, the label decoded from UTF-16. Read-only.

Proven on the existing GPT boot disk (no new disk needed): QEMU logs `gpt:
partition 0 lba 2048..131038`; the `parent` demo reads sector 0 of the disk
(`/dev/blk/0`), the partition (`/dev/blk/1`), and the partition under
`/dev/disk/by-partlabel/NITROX_ESP` — all verifying the `0x55AA` boot signature.
`partition_rebase` is unit-tested (partition LBA 0 → disk LBA 2048 + bounds).

#### 7. Filesystem in userspace — **the first userspace resource server**

The Phase-2 init milestone: a userspace `fs-server-ext4` reads a read-only ext4
root over the block device and serves it over IPC, reached **transparently
through the namespace**; init mounts it at `/` via the Ready handshake and reads
`/system/current-generation`. **Read model:** a forwarded `sys_ns_lookup` of a
file returns a read-only `MemoryObject` of its content (reuses the `/initramfs`
server pattern, so init's existing lookup→map→read code works verbatim; 64 KiB
cap — slice 8's page cache makes it lazy). The **userspace-RS kernel path** lands
here (moved from slice 3, 2026-06-22): `BindingTarget::UserspaceServer` +
IPC-forwarded lookup + cross-context handle install (the `Pending` `OpStatus`
path the slice-3 framework reserved). Staged as ordered PR parts; design +
decisions in the decision log (2026-06-25). ext4 scope is **minimal** (single
regular file via extents); `librsproto` is **codec + server-side only** (sync
`RsClient` deferred to eshell, slice 9). The async-shaped transport uses
`sys_channel_send` (Block/NoBlock) + `sys_wait`-on-recv (no async executor in
Phase 2).

- [x] **Part 1 — `librsproto` wire codec** (`phase-2/slice7-librsproto`): the
  pure `no_std`/no-`alloc`/no-deps codec — `RsMsgHeader` envelope, explicit LE
  byte serialization, the Meta bodies (Hello/Ping/Ready), and the new
  `Namespace::Resolve` op (`docs/spec/rsproto-namespace-ops.md`,
  `RESOLVE_FILE_AS_MEMOBJ` + 64 KiB cap). 11 host round-trip tests. *(Meta-op
  codec done; the Hello version-negotiation **logic** is the fs-server, Part 4.)*
- [x] **Part 2 — ext4 read-only reader** (host-testable library,
  `phase-2/slice7-ext4-reader`): `userspace/fs-server-ext4/` (lib-only; the
  `[[bin]]` is Part 4). A `BlockReader` trait (`read_at(offset, buf)`) so the
  parser is `no_std`/no-`alloc` (reads into caller buffers + bounded stack
  scratch) and 100% host-tested. Superblock (`0xEF53`, reject 64-bit / >4 KiB
  blocks), group descriptors, inode location, the **extent tree** (`0xF30A`,
  depth 0 + index levels), linear `ext4_dir_entry_2` walk, path resolve →
  `read_file(path, out) -> size`. 6 host tests against **real `mke2fs` images**
  (1 K + 4 K blocks). Skips journal/bigalloc/inline-data/htree-specific/64-bit/RW.
- [x] **Part 3 — kernel transparent-forwarding** (`phase-2/slice7-fwd`):
  `BindingTarget::UserspaceServer` + `ResolvedTarget` arm; `OpStatus::Pending`;
  the new `UserspaceServerReg` kobject (type 13) owning the kernel endpoint + the
  N=1 pending-lookup table; `IpcChannel` `us_reg` back-pointer; `sys_ns_bind`
  `IpcChannel`→`UserspaceServer` branch; `sys_ns_lookup` forwarding arm (originate
  via `IpcChannel::send_push`, leave PO pending); **inline-in-send** reply
  completion (`run_pending` runs only at the interrupt-dispatch tail, so a DPC
  would add a tick of latency — see the decision log) with cross-context install +
  PO signal; dead-server / dead-client / duplicate-reply race handling; the kernel
  hand-coded rsproto Resolve mirror (`kernel/src/rsproto.rs`). **Proven in QEMU by a
  single-process self-forwarding demo in `parent`** (bind a Userspace Server, look
  a path up through it, serve the kernel-forwarded Resolve, map the returned
  MemoryObject) — no second binary / disk needed. *Refinements vs. the original
  plan:* the stub server is the inline `parent` demo (not an embedded ELF), so
  `ImageId::FsServerExt4` + the embedded fs-server move to **Part 4** (their first
  real consumer); a forwarded lookup's returned object takes rights `requested ∩
  the rights the server granted on the transfer` (the bound IPC endpoint's rights
  are not a meaningful content cap) — see `rsproto-namespace-ops.md`.
- [x] **Part 4 — the real `fs-server-ext4` process** (`phase-2/slice7-fs-server`):
  the server `[[bin]]` wiring Part 1 (librsproto) + Part 2 (ext4 reader) + a
  `BlockReader` over `sys_io_submit`. **Alloc-free** (fixed `.bss` buffers, no
  global allocator). Bootstrap: receive the **control channel** in `rdx`; recv the
  **setup message** transferring the read-only device handle; create the forwarding
  channel pair, keep the serving end, send `Meta::Ready` on the control channel
  **transferring the kernel end** (init binds it, Part 6); then the serve loop
  (recv `Namespace::Resolve` → `serve_resolve` → fill + restrict + transfer a
  `MemoryObject`). The request→reply logic (`serve` module, generic over
  `BlockReader`) is **host-tested** against the `mke2fs` fixture (success +
  NotFound + directory + wrong-op + garbage). Adds `ImageId::FsServerExt4 = 3`
  (kernel enum + libkern `IMAGE_FS_SERVER_EXT4` mirror) + the embedded ELF + the
  xtask build step. *(No QEMU yet — the server needs a disk (Part 5) and a
  supervisor to spawn + bind it (Part 6); end-to-end boot is the Part 6 milestone.)*
- [x] **Part 5 — xtask ext4 test disk** (`phase-2/slice7-ext4-disk`): the boot disk
  grows to **128 MiB** with two GPT partitions — the FAT32 ESP (`NITROX_ESP`, 48 MiB)
  and the ext4 `nitrox-root` (filling the rest). Both partitions are built as
  separate, exactly-partition-sized images (so each filesystem is bounded to its
  partition) and **spliced** into the GPT disk at the offsets queried from
  `sgdisk -i`: the ESP via `mformat`/`mcopy`, the rootfs via `mke2fs -d` (populate-
  at-creation, no root/mount; features `^has_journal,^64bit,^metadata_csum,
  ^resize_inode`, 4 KiB blocks — the reader's supported set) staging
  `/system/current-generation`. **Confirmed:** the slice-6 GPT driver enumerates
  *every* non-empty entry (no type-GUID filter) and decodes the ASCII label, so
  `nitrox-root` rides the existing boot disk (no separate QEMU drive); QEMU boots
  clean (`gpt: 2 partition(s)`, the smaller ESP still FAT32-boots) and
  `/dev/disk/by-partlabel/<label>` binds (proven via `NITROX_ESP` in `parent`'s
  block demo). The Part-6 init loop resolves `gpt-partlabel:nitrox-root` → the
  device handle.
- [x] **Part 6 — init mount loop + the milestone** (`phase-2/slice7-mount-milestone`):
  the slice's end-to-end payoff. `manifest::device_ns_path` maps
  `gpt-partlabel:nitrox-root` → `/dev/disk/by-partlabel/nitrox-root`; per `MountSpec`
  (topo order) init resolves the device handle (READ|TRANSFER), `sys_channel_create`s
  a control channel, spawns `fs-server-ext4` (the control endpoint moved in via
  spawn → `rdx`), sends a **setup message** transferring the device handle, awaits
  **Ready** (bounded 30 s, hand-parsed — magic + op, init never speaks librsproto),
  and `sys_ns_bind`s the forwarding endpoint at the mount point. Then the milestone:
  `ns_lookup_wait("/system/current-generation", MAP_READ)` → map → log. **Proven in
  QEMU:** `fs-server: ready` → `init: mounted fs-server-ext4 at /` →
  `init: /system/current-generation = nitrox-rootfs generation 1` — the whole stack
  (ext4 on disk → fs-server `sys_io_submit` → librsproto reply → kernel cross-context
  install → init maps + logs), with the boot staying clean afterward (`parent` demos
  + reaping, no `#DF`/panic). *(Fix found here: the `fs-server-ext4` crate was missing
  the `.cargo/config.toml` + `build.rs` + `user.ld` that force static **ET_EXEC**
  linking — it built as a PIE/`ET_DYN`, which `load_elf` rejects. Copied init's
  lib+bin variant, `rustc-link-arg-bins`, so the fixed-address script reaches the bin
  but not the host lib-test link.)*

**Slice 7 is COMPLETE** — the first userspace resource server, reached transparently
through the namespace, serving a real ext4 filesystem on disk.

Read-only is the Phase-2 target; RW (and writeback) is Phase 3. Path-based spawn
from the initramfs (replacing the embedded `ImageId`) defers to slice 8.

#### 8. Page cache integration with fs-server

Makes file-backed mappings **lazy**: a `sys_memory_map` of a file reserves the range
and faults pages in on demand through a kernel **page cache**, replacing slice 7's
eager whole-file `MemoryObject` copy (and lifting its 64 KiB cap). Depends on the
demand-paging `#PF` handler + `MappingKind::FileBacked` from the prerequisite band —
the fault-in path is what makes "reads files" real.

The page cache, the lazy `FileBacked` VMA, the lazy `sys_memory_map`, and the
**async fault path** (the hard part — a file fault submits the read, **parks** the
faulting thread, and resumes it at the faulting instruction on completion, so the
`#PF` handler never blocks) are built behind a **fill-producer seam** ("fill
page-cache page for file F, offset X") so the fill mechanism is swappable.

Slice 8 uses the **range-read fill (Model B)**: on a miss the kernel asks the
fs-server for the *bytes* of a range (a new rsproto op), reusing the slice-7
fs-server's `BlockReader`/ext4 reader; the kernel copies them into a page-cache page
and maps it. This is the general fill (works for any fs-server, block-backed or not)
and a small delta over slice 7. The **extent fill (Model A)** — fs-server returns
LBA extents and the kernel reads blocks **zero-copy** into cache pages — is the
optimized path for block filesystems and is deferred to Phase 3, where writeback
forces the same extent machinery (see Phase 3 § "fs-server-ext4 read-write"). See the
decision log (2026-06-25 — page-cache fill model).

Built as ordered Parts (each independently provable), mirroring slice 7. The
detailed contracts (`rsproto-file-ops.md`, the `FileObject` handle-encoding entry,
the memory-management page-cache section) are written in their Parts, as in slice 7
— not front-loaded. See the decision log (2026-06-25 — slice 8 fill model + scope).

- [x] **Part 1 — `FileObject` kobject + the page cache** (`phase-2/slice8-file-object`,
  PR #72). The new kobject (type **14**) owns a **sparse per-page cache**
  (`reserve`/`mark_ready`/`lookup`; frames freed on drop) behind the fill-producer
  seam. Host-tested; no fault path. *(The producer fields — fs-server endpoint +
  suffix — deferred to Part 3, their first consumer.)*
- **Part 2 — lazy `FileBacked` mmap + the async fault path** (the hard part), split
  into two for a focused review of the scary async half:
  - [x] **Part 2a — the FileBacked VMA + fault wiring** (`phase-2/slice8-fault-path`):
    `sys_memory_map` on a `FileObject` → `AddressSpace::map_file` (a lazy
    `MappingKind::FileBacked` VMA holding the object, **no PTEs**); `fault_in`'s
    FileBacked arm → `FaultIn::FileBacked` (a signal — it does **not** touch the file
    cache, whose lock is rank 4 like the AS lock and must never nest); `file_backing`
    (re-fetch the object + page index outside the AS lock) + `map_file_page` (install
    the PTE for a resident cache frame, re-validating the VMA). Fully host-tested (5
    tests); no async, no producer — a file fault is still fatal until 2b.
  - [x] **Part 2b — the async fill + block-on-fault + the stub proof**
    (`phase-2/slice8-fault-fill`). `try_fault_in` (the `#PF` handler) on
    `FaultIn::FileBacked` → `AddressSpace::file_backing` → `FileObject::fault_in_page`
    (reserve; create a fill PO; `start_fill`; **block the faulting thread** on the PO
    via the scheduler's `wait_on` — sound: the ring-3 fault holds no kernel locks, and
    the block switches to another thread while the timer keeps the DPC draining) →
    `map_file_page` on wake. The `FileObject` gained a `Producer` (`Stub { base }`;
    Part 3 adds `FsServer`); the stub fill enqueues a DPC that writes the page +
    `mark_ready` + completes the PO. **Proven in QEMU** by a boot fixture (a stub
    `FileObject` bound at `/dev/test/pagecache` in pid-1's namespace) + a `parent`
    demo that maps it and reads one byte from each of 3 pages — a **real user fault**
    that parks + resumes: `page-cache demand-faulted 3 pages ok (0xA0,0xA1,0xA2)`,
    boot clean (no `#DF`/panic). No fs-server/IPC.
- [x] **Part 3 — the `File::ReadRange` wire op** (the Model-B fill contract;
  `phase-2/slice8-readrange`). A new **`File` category at `0x06`**
  (`docs/spec/rsproto-file-ops.md`): `File::ReadRange(offset, len, suffix) → bytes`
  (the bytes ride in `handles[0]` as a ≤1-page `MemoryObject`; `content_len` covers
  the short EOF tail). librsproto codec (`file.rs`) + the kernel mirror
  (`build_read_range_request` / `parse_read_range_reply` / `reply_op` router) + the
  paired `Namespace` additions (`RESOLVE_FILE_LAZY` flag, `OBJECT_KIND_FILE`). `File`
  is kept distinct from `Stream` (`0x02`, cursor I/O) and `Block` (`0x03`, Model A's
  future extent home). Host round-trip tests pin the offsets both sides. **Wire
  contract only** — the kernel send-side + the fault wiring land in Part 4 (a page
  fault blocks the faulting thread, so the *filler* must be a separate process — the
  real fs-server — which arrives in Part 4; isolating the send-side would need
  throwaway two-process scaffolding).
- [x] **Part 4a — the kernel send-side + lazy-resolve plumbing** (dormant;
  `phase-2/slice8-fill-integration`). The `FileObject` gains `Producer::FsServer
  { reg, suffix }`; `start_fill` originates a `File::ReadRange` over the slice-7
  forwarding endpoint (`sched::us_forward_originate_fill`), recording a pending-**fill**
  slot on `UserspaceServerReg` (`PendingFill`, alongside the pending-lookup slot, own
  `request_id`). The reply-completion path routes by `rsproto::reply_op`: a `Resolve`
  reply on `OBJECT_KIND_FILE` **builds a `FileObject`** (no handle; `content_len` = file
  size; producer ← reg + the lookup's inline-stored suffix) and installs it instead of
  an eager `MemoryObject`; a `ReadRange` reply copies the transferred ≤1-page
  `MemoryObject` into the cache frame, marks the page ready, completes the fill PO. The
  kernel now requests `RESOLVE_FILE_LAZY`, but the unchanged fs-server ignores it and
  still replies `MEMOBJ` — **boot stays eager** (the kernel handles both kinds). Host
  tests for the reg's fill slot + stored suffix; QEMU regression = eager milestone +
  stub demo still work.
- [x] **Part 4b — the fs-server side (activates + proves the lazy path)**
  (`phase-2/slice8-fill-integration`). The ext4 reader gained `stat_file` (size, no
  content, no `MAX_FILE` cap) + `read_file_range` (positioned per-block extent read),
  sharing a `resolve_regular_file` helper. `serve` dispatches by op: a
  `RESOLVE_FILE_LAZY` resolve replies `OBJECT_KIND_FILE` + size, no handle; a
  `File::ReadRange` reads the range → replies a `MemoryObject` of the bytes
  (**stateless**, re-resolving per range). Error replies carry the request's op so the
  kernel routes a failed fill to the pending fill (not a lookup) — else the faulter
  hangs. **Proven by the slice-7 milestone going lazy** — init's
  `/system/current-generation` lookup returns a `FileObject` and faults in via
  `ReadRange` from the real fs-server (`init: /system/current-generation = nitrox-rootfs
  generation 1`, boot clean). Retired the Part-2b stub fixture + parent demo
  (`Producer::Stub` stays for host tests). **Slice 8's Model-B core is complete.**
- [x] **Part 5 — disk + the large-file milestone** (`phase-2/slice8-large-file`).
  xtask stages `system/large.bin` (256 KiB / 64 pages) with position-sensitive content
  (`byte[i] = ((i >> 12) ^ i) as u8`); init maps it lazily and reads **every** byte
  (`read_large_file`), each first page-touch a demand fault served by a `File::ReadRange`
  to the fs-server, verifying against the shared `fill_byte`. QEMU: `init: large.bin
  verified 262144 bytes across 64 demand-faulted pages ok` — the 64 KiB cap is gone,
  **multi-page demand faulting proven end to end**. (Multi-page, not multi-extent: a
  256 KiB file is laid contiguously as a single extent; the extent tree's interior-node
  path stays host-tested. init learns the size from a shared `LARGE_FILE_BYTES`
  constant — a temporary bridge; proper discovery (a `HandleInfo.size` field via
  `sys_handle_stat`) is deferred to its first real consumer, eshell `cat` in slice 9.)
  **Phase 2 slice 8 (the kernel page cache) is complete.**

Deferred to Phase 3: the **Model A extent fill** (block-fs zero-copy fast path, added
*alongside* `ReadRange` which stays the general fallback) + writeback (with
fs-server-ext4 RW) — see Phase 3 § "fs-server-ext4 read-write".

#### 9. Emergency shell — `eshell` + the first user input

The first **interactivity**: a serial command shell + the **keyboard/serial input**
subsystem behind it. Input is read through the **universal device interface**
(`sys_io_submit` + `sys_wait`) — the console is a char-class `DeviceNode`, not a
console-specific syscall. **Deferred** (decided with the user): `reboot` (needs an
`ArchPower` interface) and `edit` (needs filesystem write + an editor); the userspace
console/tty server (cooked line discipline) layers on the raw char device later. See
the decision log (2026-06-27) and the design in `docs/conventions/arch-boundary.md`
(`console_arm_rx`) + `docs/spec/io-operation.md` (the char read path).

- [x] **Part 1 — console input subsystem (kernel)** (`phase-2/slice9-eshell`, PR #78):
  interrupt-driven COM1 RX driver (`drivers/console.rs`: ring + parked-read slot +
  ISR→DPC), `DeviceClass::Char` + `CharBackend`, the `sys_io_submit` char branch (a
  stream read completing a PO), `/dev/console` (`KernelServerId::Console`). `install_isa_irq`
  kept arch-internal; the console arms RX via the neutral `arch::serial::console_arm_rx`.
  Proven by a boot loopback self-test.
- [x] **Part 2 — the eshell crate + line editor + interactive launch**
  (`phase-2/slice9-eshell-crate`, PR #79): `userspace/eshell` (new, `no_std`+no-alloc,
  libkern only); a line editor over `/dev/console` via `io_submit`+`wait` (echo,
  backspace, CR/LF); `help` / `echo` / `lsblk`; `ImageId::Eshell = 4`; init spawns it
  as the persistent interactive console. **Proven by a scripted serial session** —
  real typed input through the Part-1 ISR path end to end.
- [x] **Part 3 — `cat` + `HandleInfo.size`** (`phase-2/slice9-cat`): added `size: u64`
  to `HandleInfo` (kernel + libkern; `stat_on` reads the per-type size; the lazy resolve
  grants `INSPECT`), and eshell `cat <path>` (lookup → stat → map → demand-fault → print,
  NUL-trimmed). Closes the slice-8 size-discovery deferral. Also **retired the concurrent
  `parent` demo**: it now runs to completion *before* eshell (the shared
  single-outstanding-command disk was corrupting the fs-server's reads → flaky `cat`),
  giving a clean console — resolving the Part-2 follow-up.
- [x] **Part 4 — `mounts` + `sys_ns_enumerate`** (`phase-2/slice9-mounts`): a
  namespace-binding enumerate syscall (`= 30`; `sys_ns_enumerate(ns, index, out)` →
  `NsEntry { path, path_len, kind, rights }`, requires `LOOKUP`, `NotFound` past the
  end), listing mount points + kernel resources (**not** fs `readdir`). eshell `mounts`
  lists them with kind tags (kernel resource / direct / mount). Proven in QEMU.
- [x] **Part 5 — kernel log ring + `/dev/log`** (`phase-2/slice9-klog`): `kernel/src/klog.rs`
  (a 16 KiB append buffer teed from the serial `write_str` path — `kprint!` + the panic
  writer; `IrqSpinLock::try_lock` keeps the tee panic-safe) + a `/dev/log` resource
  (`KernelServerId::Log`, a `MemoryObject` snapshot). Read with `cat /dev/log` (no bespoke
  `dmesg`). Bonus: `sys_kprint` now translates `\n`→`\r\n`, fixing all userspace terminal
  rendering. Proven in QEMU (the kernel boot log dumps correctly).
- [x] **Part 6 — init failure → eshell** (`phase-2/slice9-init-failure`): implement the
  documented critical-path-failure drop to eshell (`userspace/init/CLAUDE.md` §"Failure →
  eshell"). `mount_all` now returns `bool` (a failed required mount is critical-path);
  `_start` computes `booted` from `read_manifest` + `mount_all` and, when `!booted`, calls
  `emergency(notif)` (logs `init: critical-path failure -- dropping to emergency shell`,
  spawns eshell, enters the reaping loop) instead of running the boot milestones + `parent`
  demo. `supervise` was split into `supervise` (healthy: parent → `reap_loop`), `emergency`
  (failure: log + `spawn_eshell` + `reap_loop`), and a shared `reap_loop(notif, parent_h)`.
  Proven in QEMU both ways: a forced bad device label (`gpt-partlabel:does-not-exist`) drops
  straight to an `eshell>` prompt with no demo, and the operator can still inspect the broken
  system (`mounts` lists every binding *except* the failed `/`, `lsblk`, `cat /dev/log`);
  the healthy boot is unchanged (milestones → `parent` → reap → eshell). **Slice 9 complete.**

#### 10. FAT for completeness (RO is fine for now) — **deferred to Phase 3**

**No Phase 2 milestone clause consumes `fs-server-fat`,** so this slice is
**deferred to Phase 3** (decided 2026-06-26 at Phase 2 close). The ESP's FAT32
is read by UEFI firmware and Limine, *not* by Nitrox — booting never requires
Nitrox to read its own ESP. This server exists for parity/completeness, not for
boot, and ext4 already proves the userspace-filesystem path end to end. Pick it
up when an in-OS FAT consumer appears (e.g. updating the ESP from within the OS).

- [ ] `userspace/fs-server-fat/` crate (FAT32/FAT16/FAT12 read-only) — *Phase 3*
- [ ] Needed only for in-OS access to FAT volumes (e.g. updating the ESP from
  within the OS), not for booting — *Phase 3*

### Milestone

`xtask qemu` boots to a system that:
1. Boots Limine from the FAT32 ESP
2. Kernel comes up, initializes subsystems, enumerates PCI
3. Init starts from the initramfs
4. Init reads `init.toml`, spawns fs-server-ext4 for the ext4 root partition, waits for Ready, binds the endpoint at `/`
5. Init reads `/system/current-generation` and logs the contents to the kernel log
6. Init enters its reaping loop

Disk image is built by `xtask build-disk` with a real ext4 partition containing test data.

The milestone is **unchanged** by the 2026-06-11 re-sequencing — only the
slice order and the explicit prerequisite band changed. Note that init
(slice 4) is only *milestone-complete* once the storage/fs-server/page-cache
slices land (it can spawn fs-server-ext4, wait for Ready, and bind `/`).

### Notes / deviations

- **2026-06-11 — Phase 2 re-sequencing.** Added the explicit prerequisite
  band (architecture docs, ACPI table parser, IOAPIC, DPC queue, demand-paging
  `#PF` + `FileBacked`, `PendingOperation`/async-I/O, DMA allocation); moved
  Entropy ahead of the in-kernel resource servers; corrected the FAT
  "required to boot" justification; clarified that slice-4 init is the
  bootstrapping form. Rationale and the full dependency analysis are in the
  decision log entry of 2026-06-11. No milestone change.
