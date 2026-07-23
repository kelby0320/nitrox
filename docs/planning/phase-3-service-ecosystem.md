# Nitrox Implementation Plan — Phase 3 — Service ecosystem

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. Phases 0–3 are
complete; Phase 4 is active.

---

## Phase 3: Service ecosystem

**Goal:** the userspace ecosystem. Service manager, profile servers, runtime libraries, the standard services. Scheduler matures. SMP works.

### Tasks

Unlike Phase 2's flat list, the **kernel-first** work (scheduler + SMP) is sequenced
into slices 0–3 below. The userspace workstreams that follow (service manager,
runtime libraries, content store, the standard services, auth/session, fs-server RW)
remain an **unsequenced backlog** — most have no design doc yet (see the
2026-06-26 Phase 3 scope analysis in the decision log) and are sliced + given their
missing architecture docs **just-in-time** as we reach them, the same way Phase 2's
slices were defined.

#### Kernel-first slices (sequenced)

The design is committed in `os-design-v5.1.md` §Scheduling (three classes —
RealTime fixed-priority FIFO, TimeShared CFS-like vruntime, Idle) and staged in the
decision log (2026-05-29, "Step 3 = SMP"). Today the kernel is single-CPU
**preemptive** with a **single global `SCHED` lock + flat round-robin runqueue**
(`sched.rs:299,258,474`) and a **stub SMP layer** (`cpu_count→1`,
`send_ipi→unimplemented!`, Limine SMP request unwired, shared GDT/TSS, local-only
TLB flush). The spinlocks are already SMP-correct (atomic CAS). Rollout is
**incremental**: APs first run against the existing global runqueue (slice 1);
per-CPU runqueues are a later refactor (slice 3), so AP bring-up and load-balancing
are bisectable in isolation.

- [x] **Slice 0 — Per-CPU foundation + scheduler/SMP design doc**
  (`phase-3/slice0-percpu-foundation`): wrote `docs/architecture/scheduler.md` (the
  three-class + vruntime + x2APIC + incremental-SMP design) and built the per-CPU
  substrate, still single-CPU. **Per-CPU access is arch-abstracted** — neutral
  `arch::Smp::current_cpu()` (a dense index), implemented x86-side via `RDTSCP` /
  `IA32_TSC_AUX` (`init_this_cpu` sets it; dev loop opts in `+rdtscp`); `MAX_CPUS=8`.
  The arch `CpuLocal` GS block became `CPUS[MAX_CPUS]`; the scheduler's `current`/
  `idle`/`idle_addr` became per-CPU arrays behind `cur_slot`/`idle_slot` accessors
  (single global `ready` + `SCHED` lock retained); `handle::current_ctx_id()` now
  keys on `current_cpu()`. Page-table-root / `active_cpus` tracking was **refined to
  slice 1** (no slice-0 consumer). *Verified:* build clean / check-arch / 8 host test
  suites green; boots identically to today (full `parent` demo → `eshell`), no faults.
  See the decision log (2026-06-26, Phase 3 slice 0).
- [x] **Slice 1 — SMP bring-up (APs on the shared runqueue)** (`phase-3/slice1-smp-bringup`):
  Limine's SMP request + AP startup (atomic `goto_address`, no INIT/SIPI); **x2APIC**
  (committed; MSR accessors, EXTD enable, single-`WRMSR` ICR `send_ipi`); per-CPU
  **GDT/TSS** + the shared IDT loaded per-CPU; per-CPU APIC timer; AP entry
  (`init_this_cpu` → `ap_cpu_init` → arm timer → `sched::ap_run`) creating a per-CPU
  boot+idle and pulling from the **shared** runqueue. Fixed an SMP reap
  use-after-free (per-CPU `reap` lists) + re-assert `KERNEL_GS_BASE` on user-thread
  switch-in. *Verified:* `-smp 4` boots the full userspace (init → ext4 mount → parent
  demo → eshell) **reliably** (6/6), 4 CPUs online, APs executing kernel code; `-smp 1`
  unchanged; check-arch + 8 host suites green. **Two pieces deferred to slice 3's SMP
  hardening** (below): **TLB shootdown + `active_cpus`** (not yet triggered by the
  read-only workload, but required for concurrent cross-CPU unmaps), and **user-thread
  migration safety** — a kernel-stack UAF in `syscall_entry` when a user thread is
  forced to bounce between CPUs under pathological churn (the shared-runqueue model's
  cross-CPU hazard; per-CPU runqueues remove the churn). Also unfixed:
  `has_live_siblings`/`exit_process` only scan parked lists, not other CPUs' `current[]`.
  See the decision log (2026-06-26, Phase 3 slice 1).
- [x] **Slice 2 — Scheduler classes** (`phase-3/slice2-scheduler-classes`): `SchedClass`
  (RealTime / TimeShared / Idle) + `rt_priority` / `nice` / `vruntime` on `Thread`;
  class-aware `dequeue_front` (RealTime by `rt_priority` FIFO → TimeShared by smallest
  `vruntime`; O(n) scan over the shared `ready`); CFS-like vruntime accrual per tick
  (Linux nice-weight table) with a `min_vruntime` floor + wake latency-boost; a
  kernel-thread `spawn_with_class`. *Verified:* a `sched_class_demo` shows the RealTime
  worker finishing **before any** TimeShared round, and the nice-0 worker completing all
  rounds while nice-10 is still on round 1 (vruntime fairness); `-smp 1` + `-smp 4`
  (2/2) clean to eshell, 0 faults; +4 host tests (521 total), check-arch green.
  **Deferred to the SysCaps slice** (no capability system exists yet): the `REAL_TIME`
  syscap gate and the user-facing **`ThreadArgs` class/nice/affinity ABI** — user
  threads default to TimeShared; kernel threads set class directly for now. See the
  decision log (2026-06-29, Phase 3 slice 2).
- [x] **Slice 3 — Per-CPU runqueues + work stealing + affinity** (`phase-3/slice3-percpu-runqueues`):
  `ready`/`min_vruntime` split per-CPU (mirroring `current`/`idle`/`reap`), one `SCHED`
  lock; a `place_thread` policy (kernel threads → least-loaded CPU; waking thread → its
  `last_cpu` home); **work stealing** (idle CPU steals from the busiest peer; routed via
  `pick_next` + an idle-tick trigger); **affinity** (`cpu_mask` on `Thread` honoured by
  placement/steal; `sys_thread_set_affinity` functional, `SIGNAL`-gated — no SysCaps);
  `has_live_siblings` now scans other CPUs' `current[]`. Fixed a placement bug — online
  CPUs are tracked by a `cpu_online[]` **mask** (APs run `ap_init` in arbitrary order, so
  they aren't a dense `0..n` prefix). *Verified:* distribution demo → all APs (`0b1111`);
  affinity demo → each worker on its pinned CPU; `-smp 4` **12/12** clean full boots;
  `-smp 1` unchanged; +6 host tests (524); check-arch green. **Migration decision (option
  B):** the slice-1 `syscall_entry` user-thread-migration UAF reappeared once placement/
  stealing moved user threads, so slice 3 **prevents user-thread migration entirely** —
  user threads stay on their creating CPU (re-home/wake home, never stolen); kernel
  threads distribute. Cost: userspace runs on the BSP for now. **Deferred to slice 3b:**
  TLB shootdown + `active_cpus`; the cross-CPU deschedule IPI (`exit_process` can't yet
  terminate a sibling *running* on another CPU); root-causing the `syscall_entry` hazard
  so user threads can use the APs. See the decision log (2026-06-29, Phase 3 slice 3).
- [ ] **Slice 3b — SMP correctness completion.** TLB shootdown via IPI + per-`AddressSpace`
  `active_cpus` (set/cleared at the CR3 load in `switch_into`; broadcast from the unmap
  sites); a cross-CPU **deschedule IPI** so `exit_process`/kill can stop a sibling running
  on another CPU; root-cause the `syscall_entry` per-CPU-stack hazard so **user threads may
  migrate** (use the APs) safely. *Verify:* concurrent cross-CPU unmap stays coherent; a
  multi-threaded user process runs across CPUs and an aggressive churn stress test is clean.
  - [x] **Hazard root-caused + two fixes landed** (2026-07-01): the migration hazard is a
    bring-up timing race — a user thread running on a not-yet-fully-initialised CPU. Fixed
    (a) **syscall MSRs re-armed at ring-3 descent** (`arm_user_entry_cpu_base`, the SCE
    `#UD`), and (b) **dense CPU indices derived from the hardware APIC id**
    (`bind_cpu_identity` / `adopt_dense_index`) so they're unique by construction (no
    GDT/TSS/scheduler-slot collision). See the decision log (2026-07-01).
  - [x] **TLB shootdown** (broadcast IPI + synchronous ack) — cross-CPU invalidation for the
    shared kernel vmap, wired at the kstack free site (`KernelStack::Drop`). `crate::tlb`
    (neutral coordinator) + `arch::send_shootdown_ipi` (transport, vector 0x40).
  - [x] **Migration hazard fully fixed — KVM 0/150** (2026-07-01): two further root causes.
    (a) **Switch-out race** — a stolen thread could resume from a not-yet-committed
    `saved_sp`; fixed with a Linux-style `Thread::on_cpu` guard (set under `SCHED` in
    `switch_into`, cleared by `context_switch` after committing `saved_sp`; `stealable_to`
    skips guarded threads). (b) **Dense-index collision** — `init_this_cpu(0)` ran *after*
    `bring_up_aps()` on the migratable boot thread, zeroing a migrated AP's TSC_AUX → dense-0
    aliasing; moved before AP bring-up. Also fixed a pre-existing host-test SIGSEGV
    (`flush_tlb_*` privileged ops now `#[cfg(test)]` no-ops). See the decision log.
  - [x] **User-thread migration enabled** (2026-07-01): dropped the `is_user` exclusion in
    `stealable_to` and the creating-CPU pin in `place_thread` — user threads now distribute
    across the APs (least-loaded placement) and are work-stealable, relying on the per-switch
    re-arm of CR3 + TSS.RSP0/syscall-stack/`KERNEL_GS_BASE` (`switch_into` →
    `arm_kernel_stack_for`). *Verified:* `-smp 4` KVM boot-loop **0/150**; scripted `eshell`
    interaction (`help`/`lsblk`/`mounts`/`cat …`) clean across a 50-boot stress batch with
    userspace running on the APs. `Thread::is_user` removed (last consumer gone).
  - [ ] Cross-CPU **deschedule IPI** + per-`AddressSpace` `active_cpus`. Not yet triggered:
    every userspace process today is single-threaded, so `exit_process` never has a sibling
    running on another CPU, and TLB shootdown broadcasts to all online CPUs (correct, if
    unoptimised). Lands with the first multi-threaded user process.

#### Userspace-runtime slices (sequenced)

With the kernel-first band done, the next sequenced work is the **userspace
runtime foundation** the services are built on. Sequencing rationale (decided
2026-07-13 — see the decision log):

- **Defer a real `std` port** (Phase 4+, unchanged). std is POSIX-shaped —
  ambient-authority `fs`/`net`, synchronous blocking `io`, errno, signals,
  `thread_local!` — none of which map onto Nitrox's capability + async-first +
  no-signals model without either lying (reintroducing the Unix patterns the OS
  rejects) or stubbing out half the surface. Its payoff (building *unmodified*
  crates.io crates) isn't needed yet, and it sits on the syscall ABI, which is
  explicitly pre-stabilization. Revisit once the ABI is stabilizing (v0.1+) and a
  concrete external crate justifies it.
- **Invest instead in Nitrox-native runtime libraries** (`libheap`/`libos`/
  `libstream`, on `libkern`) that give services standard-Rust ergonomics (`alloc`
  collections, `async` over `sys_wait`, typed streams) without the POSIX baggage.
  Design their APIs std-shaped where it's free (e.g. an `io::Error`-shaped error
  type) so a later std port is mostly re-exports over these libs, not a rewrite.
  **No `librt` crate** — the Go-style fiber scheduler and a standalone sync-wrapper
  crate were both cut (2026-07-13 decision log): in-process concurrency is `async`
  tasks on the libos executor, and a fiber runtime would be a second, non-standard
  concurrency model that fights a future `std` (`thread_local!`, `std::sync`) with no
  capability upside; a sync-wrapper crate is just `std::io` and would be deprecated by
  the port. The residual blocking convenience (`block_on`) folds into libos.
- **Order by ABI coupling.** The syscall surface these libs wrap is mostly *solid*
  today (handles, memory, `wait`, IPC, notifications, ns, `io_submit`, entropy);
  the parts that will still move are **SysCaps** (unimplemented) and the
  **`SpawnArgs`/`ThreadArgs`** growth (class/nice/affinity + syscap inheritance).
  So wrap the solid core first, and hold the thread-spawn/authority-facing wrappers
  until SysCaps + those ABIs finalize — the same ABI-stability discipline that
  deferred std, applied one level down.
- **Dogfood via init/eshell.** Each library's *first consumer* is init (and eshell)
  — converting them de-hacks the existing critical-path code, validates the lib
  against real code before any service depends on it, and honours the "no code
  without a consumer" rule. Constraint: init is critical-path (no `panic!`/
  `unwrap`); every conversion rides behind the existing gate — still boots to a
  live `eshell>` and passes the scripted `help`/`lsblk`/`mounts`/`cat` stress.

Net order: **allocator → libos core + libstream → SysCaps → the SysCaps-coupled
libos authority surface → services.**

- [x] **Slice 4 — Userspace allocator (freeing heap)** (2026-07-13). Replaced init's
  bump-arena `#[global_allocator]` (which never frees) with a real freeing userspace
  heap. Independent of the syscall-ABI churn ahead (it only consumes
  `sys_memory_create`/`sys_memory_map`/`sys_memory_unmap`), so it led.
  - [x] `userspace/libheap/` crate — a `GlobalAlloc` over `MemoryObject` backing
    (grows on demand by mapping 64 KiB arenas, vs init's fixed arena): segregated
    size-class freelists (16–2048 B, no coalescing) + a dedicated-mapping large path
    (unmap + close on free). `#![no_std]` + core; the [`HeapEngine`] is generic over
    an `ArenaSource` so the logic is host-tested (9 tests) with a `std`-backed source.
  - [x] **Design doc** `docs/architecture/libheap.md` (done 2026-07-13) — backing
    model (multi-arena over `MemoryObject`s), size-class-over-slabs structure, the
    engine/registration split (std-port seam), reclamation policy, init cutover.
  - [x] Freelist guarded by a userspace spinlock (single-threaded per process today,
    but a real lock so future std OS-threads are correct without a redesign). No
    FPU/TLS dependency.
  - [x] **First (and only) consumer:** init drops its bump `#[global_allocator]` and
    uses `libheap`; its `heap.rs` is retired. (eshell needs **no** allocator — it's
    `no_std` without `alloc`, fixed buffers — so there was nothing to migrate there.)
  - *Verified:* 9 libheap host tests (alloc/free/realloc-via-default/alignment/reuse/
    multi-arena grow/large path); full host suite + check-arch green; bare build clean;
    QEMU boots init's allocation-heavy bootstrap on libheap → ext4 mount → parent demo
    chain → `eshell`, with scripted `help`/`lsblk`/`mounts`/`cat` all correct; `-smp 4`
    clean (4 CPUs, no faults). See the decision log (2026-07-13).

- [x] **Slice 5 — `libos` core (the SysCaps-independent runtime)** (2026-07-13). The
  typed + async face of the *solid* syscall surface. **Scoped down:** `libstream` and
  the *multi-task* executor were cut (consumer-less; see the decision log), so slice 5
  is libos core only. Built in parts A–D (one commit each).
  - [x] **Design doc** `docs/architecture/libos.md` — the `Handle<T, M>` typestate
    model (from `os-design-v5.1.md`), the `Op` future over `sys_wait`, `block_on`, the
    `io::Error`-shaped error, the host-test syscall seam, the thin-entry seam. (Part A.)
  - [x] `userspace/libos/` — **`#![no_std]`, no `alloc`**: `Handle<T, M>` typestate
    wrappers (sealed `CanRead`/`CanWrite`/… gate ops; RAII close; `borrow` for
    non-owning views; attenuation-consumes-self) over Memory / Namespace / Notify /
    Resource; the `Op` future (wraps a PO; polls via `sys_wait`); async methods
    (`read`/`write`, `ns.lookup`, …); **`block_on`** (single-task driver, no heap —
    collapses the `po_wait` idiom); `io::Error`-shaped error. (Parts B + C.) 15 host
    tests against a mock syscall seam. *Deferred within libos (no consumer): Channel/
    IPC + Entropy wrappers, namespace bind, file mapping.*
  - [x] **First consumer:** init dogfoods libos — `read_current_generation` now
    `ns.lookup(...).block_on()` + `map()` (borrowing the bootstrap `root_ns`),
    replacing `ns_lookup_wait` + the manual closes. **eshell was deliberately kept
    `libkern`-only** — it's the recovery surface (statically-linked-`busybox`/`sash`
    ethos), so it doesn't take a libos dependency. (Part D — see the decision log; init
    & eshell `CLAUDE.md` reconciled.)
  - **Scope boundary:** no `thread_create`/`process_spawn` wrappers, no syscap-gated
    calls (slices 6–7); **no multi-task `spawn`/run-loop executor** (deferred — needs
    `alloc`, no concurrent consumer). `libstream` deferred (below).
  - *Verified:* 15 libos host tests; init/libos bare-build clean; full host suite +
    check-arch green; QEMU — init's current-generation reads via libos, boots to
    `eshell`, scripted `help`/`lsblk`/`mounts`/`cat` correct; `-smp 1` + `-smp 4` clean.

- [ ] **`libstream` (deferred out of slice 5).** Typed structured I/O
  (`TableWriter`/`TableReader`, `record_read`, `#[derive(TypedRecord)]`) per
  [docs/spec/typed-stream-format.md](../spec/typed-stream-format.md). **No consumer
  until the shell/pipeline era or the service-mgr milestone** (*"a test program
  produces typed TableWriter output to its log channel"*), and it drags in a derive
  proc-macro (first userspace external-crate decision, or a hand-rolled one). Lands
  just-in-time with its first consumer — and wants a **dedicated design pass on the
  `TSM1` wire protocol + streaming model** before implementation. See the decision log
  (2026-07-13).

- [x] **Slice 6 — SysCaps (process-level capabilities)** (2026-07-14). The kernel's
    defining feature — ambient per-process authority — is now real; the handle-`Rights`
    stand-ins are backed by actual syscaps. Built in parts A–C (one commit each).
  - [x] **Design doc** `docs/architecture/syscaps.md` (Part A, 2026-07-14) — the
    6-cap model (from v5.1), storage on `Process`, grant/attenuate-on-spawn
    (`child = parent & args.syscaps`), the `require_syscap` check point, and the ABI
    growth. **Two corrections vs the stub:** affinity stays a **handle right** (not a
    syscap); and — the consumer discipline — **all 6 caps are defined but only 2 gates
    are wired now** (`BIND_NAMESPACE`, `REAL_TIME`), the other four (`LOAD_MODULE`/
    `PHYSICAL_MEMORY`/`SYSTEM_CLOCK`/`AUDIT_CONTROL`) gated by the slice that builds
    their operation.
  - [x] **Part B (plumbing)** — `SysCaps(u64)` type (kernel + userspace mirror,
    host-tested); the immutable `Process.syscaps` field; `SpawnArgs` grown 96→104;
    `sys_process_spawn` inheritance (`child = parent & args.syscaps`); the init boot
    grant (`SysCaps::all()`). Behavior-neutral — no gate enforced.
  - [x] **Part C (enforcement)** — `require_syscap`; the **`BIND_NAMESPACE`** gate on
    `sys_ns_bind` (additional to the `BIND` right → namespace construction is
    supervisor-only); the **`REAL_TIME`** gate + the finalized **`ThreadArgs`**
    class/nice/affinity ABI (into its `_reserved`, size unchanged; RealTime gated, the
    rest ungated); init grants `parent` `BIND_NAMESPACE`. **ABI:** syscall-ABI change
    (self-pinned by asserts + specs), *not* the module hash — source comments corrected.
  - *Verified:* SysCaps + layout host tests; full suite (528 kernel) + check-arch green;
    bare build clean. QEMU — **gate allows** (init mounts fs-server, parent `ns_bind
    /store ok` via its grant) and **gate bites** (parent without the cap: `ns_create ok`
    but `ns_bind FAIL`, even on its own namespace); boots to `eshell`; `-smp 1` + `-smp
    4` clean.

- [x] **Slice 7 — the SysCaps-coupled libos surface** (2026-07-14). The libos pieces
    held back from slice 5, now that SysCaps + `ThreadArgs`/`SpawnArgs` are settled.
    Built in parts A–C (one commit each). (No `librt` crate — cut; see the 2026-07-13
    decision log.)
  - [x] **Design** (Part A, 2026-07-14) — extended `docs/architecture/libos.md` with the
    slice-7 surface. **Thin typed wrappers over the ABI structs, not a fluent builder**
    (consumer-minimal; a builder is a later ergonomic layer, and thin wrappers map
    cleanly onto a future `std::process`/`std::thread` pal): `process::spawn(&SpawnArgs)
    → Handle<Process,Only>`, `thread::create(&ThreadArgs) → Handle<Thread,Only>` (owning
    → RAII close = reaping), and the `BIND_NAMESPACE`-gated `Handle<Namespace,NsMutable>
    ::bind`. Out (consumer-less): runtime `set_affinity`, `terminate`, the Process mode
    lattice.
  - [x] **Part B (wrappers)** — the `Process`/`Thread` markers (`Only` mode);
    `libos::spawn`/`thread_create` (owning handles → RAII reaping); `Handle<Namespace,
    M: CanBind>::bind` (the `BIND_NAMESPACE`-gated call; denial → `PermissionDenied`);
    the `Sys` seam grows `process_spawn`/`thread_create`/`ns_bind` (real + mock).
    `#![no_std]`, no `alloc`. +3 host tests (17 total).
  - [x] **Part C (dogfood)** — `parent` (alloc-free, a demo) adopts all three: its
    worker via `thread_create` (Handle drops → closes, retiring the explicit close), its
    ns-demo bind via `Namespace::bind`, its child spawns via `spawn` (owning handles reap
    on drop, replacing a handle leak). *Verified:* the full parent demo chain runs
    through the wrappers under QEMU (`created worker thread`/`worker terminated`,
    `ns_bind /store ok`, `both children reaped`); boots to `eshell`; `-smp 1`/`-smp 4`
    clean, no faults. init's spawns left raw (surgical scope; its handshake is tangled).
  - **Kernel dependency:** true kernel-thread-backed parallelism (real `std::thread`
    semantics, multicore *within* a process) needs the deferred **TLS (`FS_BASE` /
    `sys_thread_set_tls`) + FPU `XSAVE`** kernel work (Phase 1 deferrals, still
    consumer-gated). Not needed here: `async` tasks on the single-threaded executor
    cover in-process concurrency, so schedule that kernel slice only when a service
    genuinely needs OS-thread parallelism or hard-float.
  - *Verify:* host tests for the spawn/authority wrappers; init spawns a child with
    attenuated syscaps.

The **service backlog below** was originally left **unsequenced** ("slice just-in-time").
After service-mgr (slice A, done 2026-07-15) a real **dependency spine** emerged — most
of the backlog assumes programs are loaded from **paths**, not the kernel-embedded
`ImageId` shim — so the backlog now carries a **recommended ordering** (below), while
still being *sliced* just-in-time within it. The spine, toward the Phase 3 milestone
(services supervised · typed log output · login + per-user namespace + home writes):

1. **Path-based spawn / userspace ELF loader** (next) — retire `ImageId`; load init and
   every program from the **initramfs** (later `/bin`, `/store`). The enabler for
   everything path-based; makes service.toml's `executable` real.
2. **Profile server + content store** — `/bin`, `/lib`, `/store` projection; programs on
   disk. A **read-only** store pre-built into the ext4 image decouples this from
   fs-server RW.
3. **Logging service** — the milestone's "typed log output"; service-mgr's `log` seam
   becomes real.
4. **fs-server-ext4 RW + Model-A page-cache** — the write path ("write files to home").
5. **Auth + session-mgr** — login, per-user namespaces, user-shell spawn. (Must not
   precede SysCaps, slice 6 — done.)
6. **Independent daemons** (device-mgr, audit, namespace-mgr, OOM, mount, crash-reporter)
   — slotted just-in-time as consumers appear.

Global constraint unchanged: service-mgr and everything after depend on the userspace-
runtime slices 4–7.

#### Service manager

- [x] `userspace/service-mgr/` crate (slice A)
- [x] TOML parser for service declarations per [docs/spec/service-toml-schema.md]
      — slice A parses the single-service subset (`[service.<name>]` + `executable` +
      the `[restart]` table); arrays/`[handles]`/multiple services are later slices
- [ ] Dependency graph + topological startup (deferred — a later slice)
- [x] Process supervision with restart policies (never, on-failure, always) (slice A)
- [x] Exponential backoff for restarts (slice A — none/linear/exponential + max_attempts)
- [ ] Resource Server Startup Protocol for spawned RS-style services (deferred — slice B)
- [x] Lifecycle control via per-service control channels (slice A — `CTRL_OP_SHUTDOWN`;
      the protocol grows with health-check/reload)

#### Path-based spawn / userspace ELF loader

**Done (2026-07-16).** Retire the kernel-embedded `ImageId` shim entirely and load every
program from the **initramfs** — the real-OS model (the bootloader hands the kernel an
initramfs; the kernel loads init from it and every subsequent program from a path). Both
halves already exist: the in-kernel initramfs reader (`kernel/src/initramfs.rs`, already
serving files) and the ELF loader (`kernel/src/mm/elf.rs::load_elf`). What's missing is
the **image-source abstraction** — today `SpawnArgs.image` is an enum selecting embedded
bytes; it becomes a handle to the program's bytes.

- [x] **Boot:** the kernel loads `/sbin/init` from the initramfs (initramfs reader →
      `load_elf`) instead of from embedded bytes. Removes the `INIT_ELF` embed.
- [x] **Spawn ABI:** `SpawnArgs.image` becomes a **`MemoryObject` handle** carrying the
      ELF (not an `ImageId`). The **spawner** resolves the program path in userspace
      (`ns_lookup(path, MAP_READ) → MemoryObject` — exactly how init/service-mgr already
      read `init.toml`/`heartbeat.toml`) and passes it to spawn; the kernel maps the ELF
      from the object's pages (via HHDM) and runs `load_elf`. **No filesystem code enters
      the kernel** — the spawner does path resolution.
- [x] **Retire `ImageId` + `kernel/src/embedded_images.rs`** entirely; the libkern
      `IMAGE_*` mirrors go away.
- [x] **xtask:** pack all program ELFs into the initramfs (`/sbin/init`, `/sbin/service-mgr`,
      `/sbin/heartbeat`, `/sbin/fs-server-ext4`, `/sbin/eshell`, + the selftest demos)
      instead of `include_bytes!`-ing them into the kernel.
- [x] **Path resolution for `executable`:** service.toml's `/sbin/heartbeat` resolves
      against the initramfs (a `/sbin` binding, or a documented `/initramfs` prefix) —
      retiring service-mgr's slice-A `image_for_executable` stopgap. `/bin`, `/store`
      resolution arrives with the profile server + store.
- **ABI note:** `SpawnArgs` layout changes (`image` enum → handle) — invalidates the
  spawn-args contract; update `docs/spec/process-spawn-args.md` + `syscall-abi.md` (fix
  its stale "there is no filesystem yet" — untrue since slice 8) and the ABI hash.
- **Verify:** boot with no embedded images — kernel loads init from initramfs; init and
  service-mgr spawn their children from `/sbin/*`; the full slice-A lifecycle still runs;
  `--selftest` + `test-qemu` green. Bounds: the ELF still must be a static `ET_EXEC`
  (the loader's existing constraint); real stacks/argv/guard-pages stay deferred.

#### Runtime libraries (full versions)

> **Sequenced above.** `libheap`/`libos`/`libstream` are now the sequenced **slices
> 4, 5, and 7** (see "Userspace-runtime slices"). The split: `libheap` (slice 4) +
> libos core & libstream (slice 5, SysCaps-independent) land before SysCaps; the
> thread-spawn/authority-facing libos surface (slice 7) lands after. **`librt` was
> cut** — no fiber scheduler (async tasks on the libos executor cover in-process
> concurrency) and no standalone sync-wrapper crate (that's `std::io`, deprecated by
> the eventual std port); see the 2026-07-13 decision log. The checklists live in
> those slices; this section is retained only as the map entry `overview.md` links to.

#### Profile server

- [x] Generic profile server binary (`userspace/profile-server/`; forwarding RS,
  resolve-by-probe, re-exports the store `FileObject`) — store/profile Part D
- [x] Profile manifest format (`[[package]]` table array; host-tested parser) — Part A/D
- [x] System profile manifest in initramfs (transitional) — Part C; store-resident
  manifest (post-bootstrap) still to come with the package manager
- [x] Init binds profile server at `/bin` (RS startup minus device handoff) — Part D.
  `/lib` projection deferred (only `/bin` is exercised this slice)

#### Content-addressed store

- [x] Store layout convention on the ext4 root: `/store/<hash>-<name>-<version>/` — Part C
- [x] Read-only namespace bindings (rights enforce immutability; the root fs is bound
  RO — holds trivially while the fs-server is read-only) — Part A/C
- [ ] Package manager daemon (basic: list, add, remove store paths; manage generations)
- [ ] Generation manifests (only `/system/current-generation` exists today)
- [ ] GC (mark reachable store paths, sweep unreachable)

#### Logging service

- [x] Log channel handle creation (capability-gated) — a client resolves a log path and
  the service mints a per-principal channel; identity is capability-derived (the channel),
  not a SysCap. `docs/architecture/logging.md`, `userspace/logging-service/`
- [x] `LogRecord` structure per architecture doc (`librsproto::log`; trusted/claimed split)
- [x] Logging service collects + stamps records (trusted `principal`/`tier`/`timestamp`/
  `sequence`); indexing + persistence deferred (persistence needs fs-server RW)
- [x] Sinks: serial + in-memory ring (behind one `Sink` trait). Persistent DB (fs-server
  RW) + network (netstack) deferred behind the same trait; ring read-back deferred

#### Audit subsystem

- [ ] Kernel audit ring buffer
- [ ] Audit service drains and persists
- [ ] Chained records (hash of previous) for tamper detection
- [ ] `SysCaps::AUDIT_CONTROL` for management

#### Other services

- [ ] Device manager (kernel module loader for Tier 2, `/dev` population)
- [ ] Namespace manager (system namespace coordination)
- [ ] Time sync service (NTP — depends on networking; defer the NTP part)
- [ ] OOM daemon (handles `Notification::MemoryPressure`)
- [ ] Mount daemon (post-boot dynamic mount/unmount)
- [ ] Crash reporter (exception notification handler, dumps, `rustfilt` symbolication)

#### Authentication and session management

Design + staging: `docs/architecture/session-and-auth.md` (written 2026-07-20). Four
forks taken at full fidelity: hand-rolled password KDF; **true subtree-scoped** home
isolation (a kernel primitive); service-mgr **spawns** session-mgr with re-delegated
`BIND_NAMESPACE`; a **separate** auth-service. Staged Parts A–E:

- [x] **Part A — `libcrypto` + design doc** (`phase-3/auth-session`): SHA-256 + HMAC +
  PBKDF2-HMAC-SHA256 + `password`/`ct_eq`, `no_std`/`core`-only/no-deps, verified vs
  NIST/RFC 4231/RFC 7914 vectors (17 host tests). Pure `core` so `xtask` links it to
  seed image hashes. Wired into `xtask test`.
- [x] **Part B — subtree-scoped namespace binding** (kernel): a `SubtreeBase` (base
  path) on a `UserspaceServer` binding (`base + suffix` forwarded, leading `/`
  stripped), `..`/`.`-rejecting; `sys_ns_bind` gained `base_ptr`/`base_len` (a4/a5,
  backward-compatible). Host-tested (`from_path`, `resolve` carries base,
  `join_subtree`); boot unaffected. **Multi-binding to one server** (a finding here,
  then resolved with the user): exposing one server through several bindings **shares
  its registration** (bind-mount semantics — one connection, many names) rather than a
  per-binding channel; the pending slot grew N = 1 → a small table (`US_PENDING_MAX`)
  for concurrent in-flight requests. Validated end-to-end under `test-qemu` (init binds
  the fs endpoint a second time as a subtree; a lookup through it resolves correctly).
- [x] **Part C — auth-service + user DB**: the credential-oracle RS speaking the new
  `Auth` rsproto category (`Authenticate` → `AUTHENTICATED{principal,home}`/`DENIED`,
  PBKDF2 verify, dummy-verify on missing user) — wire contract in
  `docs/spec/rsproto-auth-ops.md`. New `librsproto::auth` codec + `auth-service`
  crate (host-tested lib: DB parse + verify + serve; bare-target bin: read
  `/system/users`, Ready-hand a client channel, serve). `passwd`-style `/system/users`
  + `/home/alice` seeded into the ext4 by xtask (one-way verifier only — no secrets
  in-tree). Host-tested; image assembles; boot green. **Spawning/wiring is Part D.**
- [x] **Part D — service-mgr → session-mgr + endpoint plumbing**: init hands the
  retained fs-server endpoint to service-mgr; service-mgr spawns auth-service (RS Ready
  handshake → its client channel) + session-mgr (re-delegated `BIND_NAMESPACE` +
  control channel) and hands session-mgr the fs endpoint + auth channel. session-mgr
  (new bin crate) authenticates the demo user over the auth channel and constructs a
  session namespace binding `/home` as an fs-server subtree (proving `BIND_NAMESPACE` +
  subtree + shared-reg bind-mount). session-mgr fires the `test-harness` verdict.
  Sequenced after the demo chain (a concurrent direct-block + forwarded-lookup hang is
  tracked in `deferred-decisions.md`). Auth is reached over a **direct channel** (not
  bound at `/svc/auth`) since session-mgr is the sole consumer. AHCI concurrent-command
  bug fixed along the way (single-slot queue + DPC-drain).
- [x] **Part E — login + namespace construction + user shell** (the milestone):
  session-mgr authenticates (test-harness auto-login / interactive `nitrox login:` on
  the console), builds the session namespace (`/home` subtree RW + `/dev/console`),
  spawns the new **`usersh`** throwaway shell into it with **empty syscaps**, and reaps
  it. `usersh` `sys_file_create`s `/home/greeting`, writes + syncs + re-reads to verify
  — the fs-RW write path from a sandbox through the subtree binding. eshell demoted to
  emergency-only. Verdict: `test-harness` auto-login → shell home-write; wrong-password
  denied. **The auth + session-mgr slice is complete** — login → per-user namespace →
  user shell → home write runs end to end.

Scope notes (decided 2026-07-17, for when this slice runs):
- **Proper password hashing, if scope allows.** Prefer storing a **password hash** (a hand-
  rolled KDF over a hand-rolled hash — no external crates) rather than the raw password. Beyond
  what's needed to prove the login path, so fold in only if the added scope stays modest; a
  plaintext/trivially-hashed file is the fallback. Note: **audit's chained records need a
  cryptographic hash too** (`2026-07-16` audit design) — a shared hand-written hash primitive
  (SHA-256 / BLAKE2, userspace `no_std`) would serve both; consider building it once.
- **Minimal throwaway user shell.** The real user shell is Phase 4; this slice needs only a
  *very* minimal shell to prove login → per-user namespace → shell → write a file to home.
  Treat it as disposable (reuse/trim `eshell` or a tiny bespoke one) — do not invest in it.

#### fs-server-ext4 read-write + the extent page-cache data path (Model A)

The v5.1 "pure" data path (`os-design-v5.1.md` § File-Backed Memory): the fs-server
becomes a metadata / extent / block-allocation oracle that **never touches file
data**, and the kernel owns the data path end to end. Writeback forces this — to
flush a dirty page the kernel must know its LBAs, which *is* the extent map — so
reads and writes share one extent-based path here, replacing slice 8's Model-B
range-read fill with the **zero-copy extent fill** behind the page cache's
fill-producer seam. (Model B stays the general fallback for non-block / network /
transforming fs-servers, which have no LBA mapping.) See the decision log
(2026-06-25 — page-cache fill model).

- [x] **Model A block read fill** (Part B): the fs-server returns the file's `BlockRun`
  map (delivered inline in the `OBJECT_KIND_FILE_BLOCKS` lazy resolve reply, which also
  transfers the device); the kernel reads each page's block **zero-copy** into the cache
  frame via an async block IRP (`dispatch_block_irp_into_frame`). `MapRange`/`AllocRange`
  are specced (`rsproto-block-ops.md`) as the standalone re-map ops; the initial map rides
  in the resolve reply. Named `BlockRun`, not "extents" — the contract is fs-neutral.
- [x] Write path: block allocation + extent-tree extension + inode update (Part D,
  `ext4::grow_file`, `e2fsck`-verified). Journal interaction deferred (journalless fixtures).
  Overwrite (Part C) is data-only, no metadata write.
- [x] Writeback from kernel page cache (Part C, `FileObject::writeback` + `sys_file_sync`):
  the kernel writes dirty pages to their LBAs via write IRPs; the fs-server allocates blocks
  (on growth) but never touches file data.
- [ ] Filesystem consistency on power loss (journal replay on mount) — deferred to its own
  slice (the fixtures are `^has_journal`; crash consistency is best-effort ordering today).
- [x] File creation (inode allocation + directory-entry insertion) — Part E,
  `ext4::create_file`, `e2fsck`-verified; triggered by create-on-resolve
  (`RESOLVE_CREATE | RESOLVE_GROW` + `sys_file_create` = 33). Group 0 only; new-dir-block
  growth on a full parent directory deferred.
- [ ] Cross-group inode/block allocation, extent-tree splitting / index nodes,
  truncate / delete / rename — deferred (beyond Part E).

### Milestone

`xtask qemu` boots to a "system idle" state with:
- Multiple services running, all supervised by service manager
- A test program started by service manager produces typed (TableWriter-based) output to its log channel
- Two CPUs are visibly active (e.g., scheduler stats accessible via `/proc`)
- A user can log in, get a per-user namespace, and write files to their home directory

### Definition of Done (2026-07-20)

Phase 3 = **"the service-ecosystem machinery is complete and demonstrated,"** not an
exhaustive service catalogue. That machinery is done; a representative service set runs
supervised. DoD is the four milestone clauses above — **two are met** (supervised services;
login → per-user namespace → home write) and **two remain, and are the only gating work:**

- [x] **libstream + a service-mgr-driven typed-log demo** (clause 2 — the last open runtime lib).
  **Done (2026-07-20).** `libstream` (TSM1 wire codec, `TableWriter`/`TableReader`,
  `TypedRecord`); `heartbeat` emits typed beat rows `{seq, uptime_ns, healthy}` to its log
  channel; the logging service detects the `TSM1` magic and renders the decoded table (text
  `LogRecord`s still route to `parse_append`). See the decision log (2026-07-20 "libstream").
- [x] **`/proc` scheduler-stats surface** (clause 3), pulling forward the *synthesized read-only
  `MemoryObject` snapshot* primitive (also unblocks numeric `/proc/self/status`).
  **Done (2026-07-21)** — slice `phase-3/proc-sched-stats`; **Phase 3 is complete** (see the
  decision log, 2026-07-21). The primitive is the **capture → format →
  synthesize** discipline (copy `Copy` data under one lock hold; format via `KString` with no
  lock held; wrap in a read-only `MemoryObject` — `try_new_filled` is the existing synthesis
  step, as `/dev/log`/initramfs already exercise):
  - [x] **Part A — counters + capture + format.** Per-CPU `u64` counters in `SchedState`
    (`switches` / `steals` / `placed` / `resched_ipis` / `ticks`), incremented at their event
    sites — all already hold the rank-1 `SCHED` lock, so no atomics; `sched::stats_snapshot()`
    captures them plus instantaneous state (`ready` length, idle-current, online) under one
    hold; the pure `sched::stats::format` renders `cpus_online=N` + one `name=value` row per
    online CPU. Host tests for the formatter; full suite + `test-qemu` green.
  - [x] **Part B — the surface.** `KernelServerId::SchedStats` leaf server at
    `/proc/sched/stats` (the `/dev/log` rights pattern: `MAP_READ` + generic band), bound by
    pid 1 at boot; `scheduler.md` gains § "The stats surface" (counters table + the
    capture → format → synthesize discipline). Host tests (the all-offline snapshot renders
    exactly the header into a fresh `MemoryObject`; leaf suffix rejection); `test-qemu` green.
  - [x] **Part C — `/proc/self/status`.** `KernelServerId::ProcSelfStatus`: `pid=`/`tid=`
    text from the calling syscall context (`sched::current_pid_tid()`, one `SCHED` hold; a
    refcount-free `Thread::has_process` gates kernel/boot callers to *not found*), bound with
    the snapshot-server rights shape. The shared `complete_with_memobj` tail replaces the
    4× duplicated MemoryObject adoption. Closes the deferred numeric-`/proc/self/status`
    entry (`deferred-decisions.md`). Suffix rejection host-tested; success arm is
    QEMU-covered (Part D); full suite + `test-qemu` green.
  - [x] **Part D — demo + verdict gate + close-out.** The demo `parent` maps + parses both
    surfaces (pid/tid sanity; snapshot echoed grep-visibly — 4 CPUs with nonzero
    switches/steals/IPIs under `-smp 4`) and exits nonzero on failure (init's fast-fail
    path). Negative-testing exposed a **verdict race** — a failing demo loses to
    session-mgr's login-proven PASS — so the authoritative **`sched_gate` runs in
    session-mgr synchronously before the single `SYS_TEST_EXIT(PASS)`** (≥2 CPUs with
    `switches>0`); a failure cannot race the verdict by construction. Negative-tested both
    ways (injected failure → FAIL, exit 35; reverted → PASS). Decision-log entry 2026-07-21.

Everything else in the backlog below is **consumer-driven and defers to Phase 4**, landing
with its first consumer (the project's standing deferral discipline). Triage:

- **Defer — blocked or no near consumer:** time-sync (blocked on networking), device manager
  (blocked on the Tier-2 module loader; no loadable-driver need under QEMU), namespace manager
  (premature — supervisors already construct namespaces), mount daemon (no dynamic-mount
  consumer), OOM daemon (no memory-pressure scenario until heavy apps), audit subsystem
  (security; no functional consumer yet — revisit in a hardening pass).
- **Optional early Phase 4:** crash reporter (developer-experience value as userspace grows).
- **When their scale demands it:** service-mgr dependency-graph + RS startup ordering;
  content-store package manager / generations / GC (the package-management + sysadmin layer —
  a Phase 4 north-star component).

**Directory operations** (readdir/mkdir/rmdir) are **not** Phase 3 — no Phase 3 consumer needs
them; they open Phase 4's CLI-complete work, driven by coreutils/the shell. See the decision
log (2026-07-20 "Phase 3 Definition of Done, the `std` stance, and the Phase 4 north star").

### Notes / deviations

- **2026-06-26 — Phase 3 scope analysis + kernel-first sequencing.** Stock-take at
  the start of Phase 3. Phase 3's workstreams sort into three readiness tiers:
  *ready to build* (libstream — `typed-stream-format.md`; service-mgr schema —
  `service-toml-schema.md`), *partially sketched* (scheduler, SMP, libos, profile
  server, content store, fs-server RW/Model-A, auth+session), and *just a checkbox*
  (librt fiber scheduler, logging, audit, OOM/mount/crash-reporter/namespace-mgr/
  device-mgr daemons). ~8 architecture docs referenced by `overview.md` don't exist
  yet; they're written per-workstream as we reach each. The **kernel-first** work
  (scheduler + SMP) was sequenced into slices 0–3 (above), adding the missing
  **slice 0** (per-CPU foundation + the scheduler/SMP design doc) and choosing an
  **incremental SMP rollout** (APs on the shared runqueue first; per-CPU runqueues in
  slice 3). Full analysis in the decision log entry of 2026-06-26.
