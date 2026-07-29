# Nitrox Implementation Plan — Phase 4 — A usable windowed desktop

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. Phases 0–3 are
complete; Phase 4 is active.

---

## Phase 4+: A usable windowed desktop (and beyond)

**Goal:** move from toy demos to an OS that looks and behaves like a production system
from a user's perspective. The phase distinction breaks down here — this is ongoing
development rather than discrete phases.

**North star (scoped now): a usable windowed desktop.** A compositor on the boot
framebuffer, one shared GUI toolkit, and three flagship apps — a **GUI terminal**, a **GUI
file browser**, and a **GUI text editor** (MVP = compositor + toolkit + GUI terminal). It is
the common denominator of the whole GUI vision: the browser and every later app are "another
window on this compositor + toolkit." **Subsequent north stars** (a web browser; networking;
a package-management + sysadmin layer) reuse this foundation. See the decision log
(2026-07-20 "Phase 3 Definition of Done, the `std` stance, and the Phase 4 north star") for
the full rationale, including the `std` stance and the browser strategy.

### Substrate hardening — the gate into Phase 4 (2026-07-21 concurrency review)

The adversarial kernel-substrate review (decision log, 2026-07-21 "Substrate concurrency
review") found two live cross-CPU deadlocks, a panic path, and two Phase-4 time bombs —
mostly single-CPU-era justifications that had become load-bearing SMP claims. Fixing them
**gates the Phase 4 build-out** (threads/FP/TLS stress the substrate harder than anything
to date). Slice `phase-4/substrate-hardening`:

- [x] **Part A — F2 + F11 + F4** (small, self-contained; landed with 3 host tests,
  `test-qemu` green, 20/20 KVM boot-loop). F2: the drained entropy
  seed-waiter refs park in a new pre-reserved `SchedState::deferred_drops` list (moves
  only under the lock) and are dropped by `reap_pending` in **thread context** — an
  `ObjectRef` drop can reach the plain-spinlock allocator, which must never run under
  `SCHED` **or in IRQ context** (cross-CPU and same-CPU deadlock); correct
  `lock-ordering.md`'s blessing. F11: `reap_pending` drains by popping into a fixed local
  under the lock instead of `mem::take` (which zeroed the reserved capacity, making every
  later exit push *allocate under `SCHED`* via `KVec::try_push` growth). F4: `steal_one`
  picks the busiest victim *among those with a stealable thread*, matching
  `steal_available` (fixes the idle-steal `expect` panic + the missed-steal liveness wart).
- [x] **Part B — F1: IF-robust TLB shootdown** (landed; `test-qemu` green, 30/30 KVM
  boot-loop). `tlb::shootdown` saves IF, runs the whole window — `LOCK` acquisition,
  IPIs, ack spin — with interrupts enabled, restores after: IF=0 initiators
  (syscall/exception-context `KernelStack::Drop` via `reap_pending`) cannot mutually
  deadlock and always service incoming shootdown IPIs while waiting. Because the
  initiator is now preemptible (and can migrate) mid-window, the request targets
  **every online CPU including the initiator's** (a self-IPI replaces the local
  invalidate) — position-independent, so the ack count stays exact wherever the
  initiator resumes. Caller contract tightened: preemptible kernel context, no
  spinlocks held, never IRQ/DPC. `smp.md` §8 updated.
- [x] **Part C — F3: broadcast shootdown on user-page unmap** (landed; `test-qemu`
  green — the sched-stats demo exercises the path ~50×/boot — and 30/30 KVM boot-loop).
  New `unmap_covering_deferred`: under the AS lock the VMA is removed and PTEs cleared,
  but every frame release is **deferred** — anonymous frames collect into a
  caller-reserved `KVec` (`Err(pages)` = reserve-outside-lock-and-retry) and the `Vma`
  keeps its object ref; `sys_memory_unmap` then runs the (IF-robust) broadcast
  shootdown **outside the AS lock** (a `#PF` handler spins on that lock IF-masked and
  could not ack) and only then frees frames / drops the VMA. The old in-lock-freeing
  `unmap_covering` is `#[cfg(test)]` (host tests have no remote TLBs). `active_cpus`
  targeting stays the later optimization.
- [x] **Part D — F5: honor `on_cpu` everywhere** (landed; `test-qemu` green, 30/30 KVM
  boot-loop). `switch_into` spins on `!is_on_cpu(next)` before reading `saved_sp` — the
  Linux `smp_cond_load_acquire` analog — covering affinity-diverted wake/resume
  placements picked up by `dequeue_front` (which, unlike `stealable_to`, has no guard
  filter); `reap_matching`/`reap_blocked_matching` wait out the guard before queueing a
  sibling's stack for freeing (the mid-switch-out UAF window). Bounded + deadlock-free
  under `SCHED`: the owning CPU clears the guard from post-release straight-line code,
  no lock needed.
- [x] **Part E — F6 + F7** (landed; host test for the fallback, `test-qemu` green,
  30/30 KVM boot-loop). F6: `pick_wake_cpu` requires queue room at the home CPU,
  falling back to the least-loaded permitted queue (which has room unless *every*
  permitted queue is full — the only case wakes still treat as fatal, e.g. a pinned
  thread whose sole queue is at reserve); `READY_RESERVE` raised 16 → 32 for Phase 4
  headroom. F7: `quantum` is per-CPU (`[u32; MAX_CPUS]`) — the shared counter was
  benign only while `QUANTUM_TICKS == 1`.
- [x] **Part F — stress selftest + F12 + docs** (landed). The **exit-storm** selftest
  (`parent` spawns 6 waves × 3 immediately-exiting children; teardown races spawn, the
  login chain, and itself across CPUs) immediately exposed **F12** — a latent
  descheduled-spinlock-holder deadlock hanging ~30 % of KVM boots (pre-hardening `main`
  hangs identically; TCG never reproduces). Diagnosed via a QEMU-monitor capture harness
  (per-CPU RIP/RFLAGS dumps, symbolized); two poses captured: the **idle thread**
  descheduled holding the shootdown `LOCK` (starved forever by its own spinners), and
  **IF-masked allocator spinners** that can neither tick nor ack. Fix:
  `sched::preempt_disable/enable` (per-CPU depth; tick/IPI latch skipped switches into
  `RESCHED_PENDING`, replayed at enable) + **every plain `SpinLock` critical section is
  a no-preemption region** (holders always run to release; `IrqSpinLock` deliberately
  unwrapped) + explicit wraps for the shootdown window and `reap_pending`'s drop phase.
  Verified: ~30 % → 4 % (wraps alone) → **0/60 + 0/60** KVM boot-loops; host suite +
  `test-qemu` green. Docs: F8 deferral entry (+ the general deferred-reclamation entry
  marked done-in-essence via `deferred_drops`), F9 corrections (affinity-validation
  claim, #PF-allocation rule, serial-at-SMP pointer), F10 TSC-sync note, smp.md
  invariant **I6**, lock-ordering § no-preemption regions.

**Stepping-stone path** (each a real, satisfying milestone; roughly ordered):

1. Phase 3 close (libstream + `/proc`) — the gate out of Phase 3. ✅ (2026-07-21)
2. FP/AVX2 + XSAVE (below). ✅ (2026-07-21)
3. CLI substrate prereqs (dir ops + `Value` collections + stdio/pipe) → then the typed shell +
   coreutils subproject → **CLI-complete**.
4. framebuffer display server + input routing.
5. compositor + minimal shared toolkit.
6. **GUI terminal** (hosts the shell) — the "looks like an OS" moment.
7. GUI file browser + GUI text editor → a usable desktop.

The **full std cluster** runs as a parallel, consumer-driven track (below) — not a
desktop-MVP gate.

### Floating-point + SIMD (early enablement)

The kernel saves **zero** FPU state today (soft-float everywhere). Real Rust programs and the
ecosystem use hardware float/SIMD. This is the one std-adjacent prerequisite that lands
*early*, ahead of any graphics: it also unblocks a pile of `no_std + alloc` ecosystem crates
(font rasterizers, image codecs) the toolkit will want.

Sequenced kernel-first (on the stable target, no toolchain change), then the userspace
target, per the decision log (2026-07-21 floating-point): Part A = the kernel FPU
mechanism; Part B = the `asm!` cross-contamination selftest + cost measurement; Part C =
`x86_64-unknown-nitrox.json` + `-Z build-std` for the userspace workspace; Part D = a
hard-float dummy program.

- [x] **Part A — kernel FPU mechanism.** `arch::fpu_init_cpu` enables the FP/SIMD units
  per-CPU (`CR0` EM/TS/MP/NE, `CR4` OSFXSR/OSXMMEXCPT/OSXSAVE, `XCR0` = x87+SSE+AVX,
  CPUID-driven area sizing) — BSP in `main.rs`, each AP in `ap_cpu_init`. Every schedulable
  `Thread` carries a boxed 64-byte-aligned `ArchFpuState`; `sched::switch_into` swaps it
  **eagerly** (`XSAVE`/`XRSTOR`, or `FXSAVE`/`FXRSTOR` when CPUID lacks XSAVE) inside the
  existing `on_cpu`-guarded window — save before the guard clears, restore after the
  incoming guard spin. `kmalloc` now routes over-aligned requests to the buddy path
  (the slab caps alignment at 8). Policy = **eager, not lazy** (CVE-2018-3665; no `CR0.TS`
  trap, no per-CPU FPU-owner tracking); AVX-512 not enabled (area-size cost for an SSE2
  baseline). *Verified:* 3 host tests + full suite (546) green; `test-qemu` now runs
  `-cpu max` (256-bit XSAVE path — splicing `+xsave` onto `qemu64` hangs TCG) PASS; KVM
  `-cpu host` PASS + 20/20 boot-loop (real hardware XSAVE/AVX under SMP migration).
- [x] **Part B — isolation selftest + measured cost.** `boot_selftest::fp_isolation_demo`
  runs 12 kernel workers (3× the CPUs, so they contend and migrate) × 6 preemption-spanning
  rounds: each stamps all 16 vector registers with a *self-identifying* pattern (mixing
  worker seed, register index, and byte offset, so a whole-register cross-wire is caught as
  surely as a byte flip), then re-reads and compares byte-for-byte. Corruption `panic!`s →
  FAIL verdict. The load/store go through `arch::fpu_selftest_{load,store}_regs`, whose asm
  declares **no vector operands** — impossible on a `-sse` target, and unnecessary, because
  that same soft-float property means rustc never allocates a vector register: between
  stamp and check the only agents that can touch them are the context switch and another
  thread, so a mismatch has exactly one explanation. **Negative-controlled both ways**:
  disabling the restore or the save in `switch_into` makes it fail loudly (52 corruption
  reports), so the test is known-sensitive rather than merely passing.
  `fp_swap_cost` prices the swap against a real switch (two threads pinned to one CPU,
  timing `yield_now`). *Measured (KVM, `-cpu host`):* **162 cycles of a ≈4109-cycle context
  switch — 3 %**, which is what settles eager-vs-lazy: a 3 % saving is not worth a
  speculative-disclosure channel. TCG PASS, KVM PASS, 20/20 KVM boot-loop.
- [x] **Part C — the custom userspace target.** `userspace/x86_64-unknown-nitrox.json`:
  freestanding ELF like `x86_64-unknown-none` but **hard-float** (`+sse,+sse2`, no
  `rustc-abi: softfloat`) and `target_os = "nitrox"`. SSE2 baseline, not AVX2 — a
  base-AVX2 target `#UD`s on pre-Haswell and on `qemu64`; wider vectors are per-function
  `#[target_feature]` + runtime CPUID, as ecosystem crates already do. All 13 bin crates
  retargeted; `userspace/rust-toolchain.toml` pins an exact nightly (+`rust-src`) and
  xtask passes `-Z build-std=core,alloc,compiler_builtins` for bare builds only, so host
  test builds keep the precompiled host sysroot. `compiler-builtins-mem` stays **off** —
  `libkern` exports its own `mem*`, whose signatures moved to `c_void` to satisfy
  rustc's runtime-symbol lint. The nightly is contained by `cargo xtask check-nightly`
  (fails on any `#![feature(`, wired into CI, negative-controlled), and the rule in
  `CLAUDE.md` is narrowed rather than dropped: *no nightly language/library features; a
  nightly toolchain solely for build-std*. Kernel and tools stay on stable.
  **Found a latent kernel ABI bug**: `enter_user` entered ring 3 with `RSP` 16-byte
  aligned, but an `extern "C"` body may assume `RSP ≡ 8 (mod 16)` (a `call` pushed a
  return address). Soft-float never spilled anything needing >8-byte alignment, so this
  was invisible for three phases; the first hard-float build made `init` `#GP` on a
  `movaps` spill. Fixed in `enter_user` (`and rsi,-16; sub rsi,8`), the ring-3 analogue
  of `thread_trampoline`'s existing `and rsp,-16`. *Verified:* every binary is `ET_EXEC`,
  no interpreter, **zero** soft-float libcalls and real `xmm` instructions; host suite,
  `check-arch`, `check-nightly` green; `test-qemu` PASS; KVM 10/10.
- [x] **Part D — first hard-float userspace code demonstrated.** Real Rust `f64`
  arithmetic running in ring 3 on the new target, checked **bit-exactly** rather than
  epsilon-fuzzily: every value is a small exact integer in an `f64`, so Σ v[k]² computed
  in `f64` must equal the same sum computed in `u64` — a self-consistent-but-wrong FPU
  (bad multiply, stuck rounding mode, uninitialised `MXCSR`) fails where a float-only
  check would not. Plus a `x → 2x+1 → (x-1)/2` round trip across a syscall (exactly
  invertible at these magnitudes), and an `#[target_feature(enable = "avx2")]` SIMD path
  cross-checked against the scalar one — gated on `XGETBV` read **from ring 3**, which is
  userspace independently confirming the `XCR0` write the kernel made in `fpu_init_cpu`.
  Two placements, deliberately: `session-mgr::fp_gate` is the **guarantee**, checked
  synchronously at the single `SYS_TEST_EXIT(PASS)` call alongside `sched_gate`;
  `parent` + `child` role 3 spawn three concurrent workers with different seeds as
  cross-process **breadth**. *The split was forced by evidence*: the check lived only in
  `parent` first and a KVM boot-loop showed it completing in **2 of 15** runs — the login
  chain owns the verdict and races the demo chain, so on a fast boot the run was
  adjudicated PASS while the workers still ran, and the check silently never executed.
  After gating: **15/15**. *Negative-controlled three ways* — corrupting the expected sum
  in `child` (exit 20 → FAIL) and in the gate (→ FAIL), and disabling the kernel's
  `fpu_restore` with Part B's kernel demo silenced, which the **ring-3** check caught on
  its own. TCG PASS, KVM 15/15.

### CLI substrate prereqs (build first — general substrate, not shell-specific)

The typed shell + coreutils is a large subproject (`docs/planning/shell-coreutils-plan.md`), but
its design leans on three pieces of substrate that don't exist yet and that are **general Phase 4
infrastructure**, not shell-specific — directory ops in particular unblock much more than the
shell. Build these first, here, so the subproject can assume them. Each is independently testable.
The full gap analysis is in the subproject plan (§1); this is the checklist.

- [x] **Directory operations** — `readdir`/`mkdir`/`rmdir`/`unlink`/`rename` (branch
  `phase-4/dir-ops`, 2026-07-23). Transport = **direct client↔fs-server RPC**: a directory
  handle is a session `IpcChannel` scoped to one inode (resolved via the normal
  `OBJECT_KIND_CHANNEL` path — **no kernel change**), and ops address entries **by name, not
  path**, so confinement is structural. `librsproto` `File::ReadDir`/`Mkdir`/`Unlink`/`Rmdir`/
  `Rename`; a multiplexed `fs-server-ext4` serve loop; four ext4 mutation ops, all
  **e2fsck-clean**. Proven end to end in QEMU (read + mutate, including the 1-vCPU path).
  Along the way: root-caused + fixed the fs-server "I/O hang" (same-CPU IRQ-wake latency —
  a scheduling point at the device-IRQ tail; decision log 2026-07-23) and batched the
  fs-server's block I/O to 4 KiB blocks (8× fewer wakes). **Deferred within dir-ops:** a client
  wrapper (landed 2026-07-24 as `librsproto::session::Dir` — *not* in `libos`, which sits below
  the protocol and is `alloc`-free), cross-directory + overwrite `rename`, a new-parent-block
  grow on a full directory, the `MAX_SESSIONS = 7` session cap, and a `File` directory-ops spec
  doc (written 2026-07-24).
- [x] **`Value` collection types** — extended the in-memory `libstream` `Value` (was scalar +
  `Str`/`Bytes`/`Handle`) with `List(Arc<[Value]>)` / `Record(Arc<Record>)` / `Table(Arc<Table>)`
  (Arc-backed, persistent), and implemented the wire codecs for the reserved `List` (0x07) /
  `Record` (0x08) `TypeTag`s. `List` is self-describing (per-element tag → heterogeneous +
  nested lists round-trip); `Record` = sub-schema + a row of values; `Table` is a whole *stream*
  (`Table::encode`/`decode`), not a cell — `type_tag()` is now `Option<TypeTag>` (`None` for a
  table) and `write_value` refuses a nested table (`WireError::NestedTable`). Factored the
  `NULLABLE`-aware row codec into shared `wire::write_row_values`/`read_row_values` (data rows,
  records, and table rows now frame identically). Dropped the `REC_WIDGET` (0x03) stub — TSM1 is
  data-only. Host-tested (23 wire tests); the live logging typed-stream path still passes
  `test-qemu` (branch `phase-4/value-collections`, 2026-07-23).
- [x] **stdio / pipe convention** — spawn contract + library for wiring `stdin`/`stdout`/`stderr`
  channels across pipeline stages (branch `phase-4/stdio-pipe`, 2026-07-24). **No kernel/ABI-hash
  change** — the register bootstrap stays, richer needs arrive in an opt-in userspace **setup
  message**, and `arg0` is the single system-wide **bootstrap descriptor** (Tier 0 = `0`, Tier 1 =
  `SETUP_PENDING`; decision log 2026-07-23 + 2026-07-24). Built: **A** `libstream::channel` transport
  (eager-chunked `ChannelSink`/`ChannelReceiver` over a `MsgPort`, `PeerClosed`); **B**
  `libstream::setup` protocol (`arg0` descriptor, `Streams` bitmap↔handles, TSM1 `Record` payload);
  **C.1** concrete `IpcPort` over `sys_channel_send`/`recv` + a two-thread transport proof (2000 rows,
  real backpressure); **C.2** the setup-message spawn — `pipe()` / `send_setup()` sender +
  `bootstrap().setup()` receiver, proven by spawning a Tier-1 stage (`child` conforming path) that
  reads `stdin` + `argv` from a real setup message. Host-tested codecs + full `test-qemu` PASS.
  **Deferred:** a `libos` `spawn_stage` convenience wrapper, `stdout`/`stderr` demo wiring, and the
  incremental (still-arriving) stream reader — land with their first shell consumer.
- [x] **Retire `parent`/`child` into a conforming test harness** (branch `phase-4/test-harness`,
  2026-07-24). Merged the two legacy demo programs into one crate `userspace/test-harness` (bins
  `test-harness` + `test-stage`); dropped the `arg0`-role abuse — `test-stage` takes its role/params
  from `argv` in the setup message (Tier-1) or exits immediately (Tier-0 = exit-storm). **Serial
  adjudication:** init runs the harness to completion **first** (a non-zero exit fails the run, a hang
  fails via timeout), *then* hands off to the login chain — no more racing session-mgr's verdict.
  **Build-flag gated:** built + initramfs-embedded only in selftest/test-harness `xtask` modes,
  **absent from release images**. Coverage preserved (all ~15 checks moved over; the redundant
  cap-prop dropped — subsumed by the C3 setup-message spawn). Along the way, running every demo to
  completion (the old raced verdict never did) exposed a latent **8-byte stack smash** in the demo
  itself: `stat_is_type` passed a 16-byte buffer for the now-24-byte `HandleInfo` (`size: u64` field),
  zeroing `_start`'s spilled `root_ns` → `InvalidHandle` on later lookups (root-caused by Fable;
  `handoff/`). Full `test-qemu` PASS.

Each prereq slice self-validates (host tests for the codecs; a conforming producer/consumer demo in
QEMU for the stdio convention). The first *integrated* proof — real coreutils streaming over a real
pipe — is the subproject's Milestone 1, once these three are in.

### Substrate gaps surfaced by the coreutils subproject

Milestone 1 (`list` + `copy`) put real programs on both ends of a real pipe for the first
time, which surfaced three substrate gaps the demos could not paper over. Each got its own
branch off `main` — kernel/filesystem work with different risk profiles, not shell work
(decision log, 2026-07-24). **All three are now closed**, and both assertions Milestone 1
had to weaken are restored and serving as their regression tests:

- [x] **Exit-time handle reclamation** (branch `phase-4/handle-reclaim`). Nothing swept the
  global handle table at process exit, so a dead process's entries pinned their objects —
  and its end of a pipe never closed, so a peer never observed `PeerClosed`. That is the
  mechanism the pipeline model needs for a stage that dies early (design §1). The exiting
  thread marks itself under `SCHED`; `reap_pending` runs a **batched** sweep (unlink under
  rank 3, release, then drop) since a destructor can take rank 1/4/6. 5 host tests,
  `test-qemu` PASS, 120/120 KVM boot loops, negative-controlled.
- [x] **Wall clock** (branch `phase-4/wall-clock`, 2026-07-24). The kernel anchors
  `CLOCK_REALTIME` from the CMOS RTC once at boot and derives it as `monotonic + offset`
  (so it cannot step backwards); the fs-server reads it and stamps inodes on create, grow,
  mkdir, unlink, rmdir and rename, including the containing directory. **Reading is
  ambient, setting is authority** — no setter exists yet, and a time *server* was rejected
  for the read path (an IPC round trip per metadata op, and it inverts fs-server bootstrap).
  Found and fixed a bug in merged M1 code: the mtime epoch-extension bits were read from
  `i_ctime_extra`. **Still open:** `mtime` on an in-place overwrite, which Model A hides
  from the server. See the decision log (2026-07-24).
- [x] **File truncate** (branch `phase-4/fs-truncate`, 2026-07-24). `sys_file_truncate` →
  `RESOLVE_TRUNCATE` → `ext4::truncate_file` (extent walk: free whole extents past the new
  end, shorten a straddling one, compact the tree, return blocks to the allocator).
  **Kernel-forwarded, not a directory-session op** — the kernel owns the page cache and
  mints the `FileObject` from the reply's size, so a shrink it never saw would leave a stale
  size and stale pages. `copy --force` now overwrites a longer destination, shrinking first
  and verifying the shrink took. e2fsck-clean; negative-controlled.

### Pre-CLI substrate hardening — the deferral audit (2026-07-24)

Three consecutive slices (handle reclamation, the wall clock, file truncate) were spent
paying down gaps that a coreutil tripped over. An audit of every deferral — the canonical
list, the decision log, the planning docs, and the code's `TODO(...)` tags — found two
things worth acting on before the shell subproject continues.

**The gaps that bit us were never recorded as deferrals.** None of the three was in
`deferred-decisions.md`. Handle reclamation lived as a sentence in `handle-system.md`
("the `Process` slice wires it up"), the wall clock as a `TODO(realtime)` in the syscall
table, truncate as a bullet in `fs-server-ext4/CLAUDE.md`. The canonical-list discipline
works; the leak is **implicit deferrals** — a stub, a TODO, or prose promising a later
slice — which never reach the list and so are never reviewed.

**The list cannot be scanned for what is still owed.** Roughly 17 of 96 entries describe
finished work, interleaved with open ones and marked four different ways. Two entries were
found stale only by reading the code: **x2APIC** (built and committed x2APIC-*only* since
2026-06-26; the entry still describes an unbuilt dual-mode plan) and **forwarded-lookup
N=1** (`US_PENDING_MAX = 8`). Verified-stale alongside them: writeback IRPs (built), the
range-TLB/shootdown entry (shootdown built, range flush not), and the AHCI
single-command entry (its stated workaround was lifted and `PendingRing` superseded it).

The pass below lands **before coreutils Milestone 2** — a solid kernel/system underneath
the userspace work, rather than discovering each gap from a coreutil.

#### Slice A — trustworthy docs, and CI that catches things ✅ (2026-07-24, PR #120)

- [x] `deferred-decisions.md` split into **Open** and a **Resolved** table (18 rows); the
  superseded ELF-copy entry deleted; the verified-stale entries corrected (x2APIC —
  actually built and x2APIC-only; forwarded-lookup N=1; writeback IRPs; the AHCI
  single-command entry; range-TLB); the four process-memory-model entries bundled into one.
  The rule is written into the document: a triggered entry *moves* to Resolved rather than
  being annotated in place.
- [x] `cargo xtask check-deferrals` — every `TODO(tag)` must be named literally in
  `deferred-decisions.md`. **Found four unrecorded deferrals on its first run**, including
  that `sys_memory_unmap` ignores its `size` argument and unmaps the whole VMA.
- [x] **CI runs `xtask image` + `xtask test-qemu`** — in a second job, under **`--kvm`**:
  the kernel is x2APIC-only and GitHub's runner ships QEMU 8.2, whose TCG cannot emulate
  x2APIC (it only does from 9.0). `xtask` now preflights that floor and fails with an
  actionable message instead of a guest panic.

#### Slice B — the demand-fault path ⏸ **measured, then deferred** (2026-07-29)

Re-scoped once before writing code (B1 was Model-B work with no shipping consumer), then
**measured before building anything else — and the measurement says don't**. Counters were
added to the fill and spawn paths and printed at the end of every adjudicated run.

| | TCG | KVM |
|---|---|---|
| Page-cache fills | 43 · 204 µs avg · 2.8 ms max · **0.5 % of a 1.75 s boot** | 43 · 137 µs avg · **0.5 % of a 1.0 s boot** |
| Concurrent-faulter spins | **0** | **0** |
| Spawns | 40 · 774 µs avg · **1.7 % of boot** | 40 · 122 µs avg · **0.4 % of boot** |
| Image materialisation | 5.5 ms · **17 % of spawn** | 3.8 ms · **78 % of spawn** |
| Image bytes | 1.94 MB over 40 spawns (~48 KB each) | same |

What the numbers mean:

- **Read-ahead (B2) would optimise ~0.5 % of boot.** The old entry's ~325 ms-per-page
  figure is dead — a fill now costs ~0.1–0.2 ms, and there are only 43 in a whole boot.
  Nearly every program is spawned from the **initramfs**, which resolves to a `MemoryObject`
  copy and never touches the page cache at all; the 43 fills are the ext4-backed reads.
- **B3 is not exercised: zero spins.** The concurrent-faulter path is real but unreached —
  and the thing that would reach it is **B4a itself** (many processes sharing one image
  `FileObject` fault the same text pages), so B3 belongs *with* B4a, never before it.
- **B4a would remove a real cost, but a small one.** Materialisation is 78 % of spawn under
  KVM — impressive-looking, but spawn is 0.4 % of boot, so the whole prize is ~4 ms. A
  four-stage shell pipeline costs ~0.5 ms of spawn today. **The shell does not need this.**

**Deferred with measured triggers**, and the counters stay in so the triggers are
*observable* rather than guessed:

- Large binaries. At ~48 KB average the copy is ~100 µs; a 2–5 MB GUI toolkit app is 40–100×
  that per launch, *plus* a private copy per instance. That is the same inflection as
  dynamic linking and CoW — one process-memory-model pass, at the toolkit milestone.
- Fill count climbing out of the tens — which B4a itself causes, by routing spawns through
  the page cache.
- `std::thread` (for B3, independently).

**Scheduled revisits — do not wait for a profile to volunteer.** Today's workload barely
touches the filesystem: 43 fills, and nearly every program loaded from the initramfs rather
than through the page cache. Two upcoming milestones change that materially, and each is a
checkpoint where these numbers should be re-read:

1. **After the typed shell + coreutils subproject.** A shell spawns a process per pipeline
   stage, runs scripts that open and rewrite files, and puts `list`/`copy`/`save` on real
   trees — far more filesystem traffic than a boot self-test, and from *userspace* rather
   than from init's fixed sequence.
2. **After the desktop UI MVP** (compositor + toolkit + GUI terminal). Large binaries, many
   concurrent instances, fonts and images loaded from files — the case B4a, CoW and dynamic
   linking all share, and the first workload likely to move these counters by an order of
   magnitude.

The review is deliberately cheap: the counters are permanent, so it is "boot `test-qemu`
and read the `page-cache fills:` / `spawns:` lines". If fills have climbed out of the tens,
or image materialisation has grown past a few milliseconds, Slice B stops being deferred.

Slice C follows directly; it has actual Milestone 2 blockers.

#### Slice C — fs/ext4 completeness for Milestone 2 *(branch `phase-4/fs-completeness`)*

- [x] **C1 — grow a full directory** ✅ (2026-07-29). A directory whose blocks are all full
  now gains another: `dir_insert` appends a block, formats it as one free record spanning
  it (what a block looks like after everything in it is deleted, so nothing downstream
  changes), and updates the parent's size/blocks/mtime. The extent-append logic is now
  shared with `grow_file` rather than duplicated. **Measured ceiling**: on 4 KiB blocks,
  creating *files* in one directory is unbounded in practice (2000+ tested — the parent's
  growth blocks stay contiguous, so one extent covers them), while *subdirectories* stop at
  **~814**, since each `mkdir` allocates the child's block between the parent's and so
  fragments it into a fresh extent each time. That residual limit is the inline extent
  header (4 leaves); tree splitting stays deferred, now with a number attached. Also
  surfaced that bulk creation is O(N²) block reads (`deferred-decisions.md`).
- [x] **C2 — cross-directory and overwrite `rename`** ✅ (2026-07-29). `move` is unblocked.
  Three parts: the ext4 `rename_path` (repoint the destination → remove the source →
  release a replaced inode's link, in that order, so a crash leaves a duplicate name rather
  than an orphan); `sys_file_rename` (syscall 35) plus the `ResolveOp` enum that replaced
  the widening tuple of `Option`s `Namespace::Resolve` had grown one side effect at a time;
  and the fs-server dispatch, which runs *before* the directory-session path because that
  path infers "directory open" from the suffix naming a directory, and renaming a directory
  names one too. The kernel reduces both paths to one mount-relative suffix and refuses a
  cross-binding rename with `Unsupported` (the `move` → copy + unlink cue; POSIX `EXDEV`).
  Two things the in-guest check caught: the refusal only guarded the *forwarding* arm, so a
  source that was not on a filesystem resolved normally and returned a handle — reporting a
  rename that never happened; and an attempted `MAP_WRITE` check on both bindings failed
  against a live mount, because a userspace-server binding's rights are the *endpoint
  handle's*, not the files' — which is a system-wide gap, now filed as
  `TODO(mount-write-authority)`, not something rename should have solved unilaterally.
- [x] **C3 — the `MAX_SESSIONS = 7` cap** ✅ (2026-07-29). It was never an fs-server number:
  a server waiting on one endpoint per client is capped by the kernel's `MAX_WAIT_HANDLES`,
  and `logging-service` carried the identical, separately-written `7`. Raised **8 → 32** (31
  concurrent clients), with both servers now *deriving* their cap from the constant so it
  cannot drift again, and a compile-time kernel-stack budget check — the wait path holds
  several arrays of that width on a 16 KiB stack, so the limit cannot simply keep growing;
  the build now fails before the stack does. The in-guest check found a second, subtler
  problem: the server drained its serving endpoint *before* the session endpoints in a wait
  batch, so a batch containing both a closed session and a new open answered the open while
  the freed slots still read as occupied — a spurious `WouldBlock` in exactly the pattern a
  shell pipeline produces (stage N exits as stage N+1 starts). Sessions are now reclaimed
  first. The structural fix — a readiness mechanism, one wait slot for any number of clients
  — is filed as `TODO(server-fanout)` with the desktop compositor as its trigger.
- [x] **C4 — `mtime` on an in-place overwrite** ✅ (2026-07-29). A same-length rewrite goes
  from the page cache to the device with no resolve and no IPC, so the server could not see
  it: a file edited repeatedly in place reported the timestamp of its last *size* change,
  usually its creation. Fixed with `File::Touch` (`0x0606`) — after a successful
  `sys_file_sync` of a Model A file the kernel sends the server the file's suffix and the
  server stamps `mtime` **from its own clock**, so no timestamp rides the wire and a writer
  cannot choose when its write appears to have happened. No reply and nothing registered
  pending: the data is already durable, so a dropped notification costs a stale timestamp
  rather than a failed sync, and ordering still holds because the message enters the same
  endpoint ring as forwarded resolves. To name the file at all, Model A's producer now
  carries its `(registration, suffix)` — fields the data path never reads, which is the
  point. Stamps on *sync* rather than on the write itself, since there is no per-page dirty
  bit (`TODO(page-dirty-tracking)`).

#### Slice D — cheap, now-triggered hygiene

- [ ] **D1 — klog keep-recent ring.** Still a linear append buffer that stops capturing
  once 16 KiB of boot log fills — i.e. exactly when a long-running system gets interesting.
- [ ] **D2 — `cargo xtask abi-sync-check`.** Its trigger was "a second non-demo consumer";
  there are now five or six.
- [ ] **D3 — a listable `/dev`.** `list /dev` is a day-one shell command. `sys_ns_enumerate`
  exists with no consumer; needs a design call on how `list` chooses namespace enumeration
  versus an fs-server directory session.
- [x] **D4 — debug-build lock-ordering enforcement** ✅ (2026-07-29). Every lock declares a
  rank at construction (`SpinLock::new(LockRank::Buddy, …)`); a per-CPU held-rank stack
  checks each acquire in debug builds and compiles away in release. Making the rank
  **mandatory** rather than optional is what did the work: it surfaced six live locks that
  were missing from the rank table entirely, and the armed boot then disagreed with the
  documented order four times — `dpc::init` and `entropy::init` both allocating under a
  leaf-ranked lock, `DEVICES`/`PARTITIONS` being mis-ranked as leaves when they push a `KVec`
  while held (a legal descent to the allocators, just not a leaf), and `tlb::LOCK` being
  unrankable because it is held with interrupts enabled (the F1
  fix), so interrupt work legitimately nests beneath it. The last is exempt with its reason
  written down; the general fix is `TODO(lockdep-irq-context)`. Two bugs in the tracker
  itself also came out of running it — the per-CPU model is invalid under host `cfg(test)`
  (many threads, one reported CPU) and the re-entrant report path spun instead of returning.
- [x] **D5 — kernel-stack watermark** ✅ (2026-07-29, landed early alongside C3 because C3's
  sizing argument depended on it). `test-harness` builds paint each stack and sample the
  high-water mark at context-switch-out — O(1) unless a record moves, and it covers blocked
  threads that a run-queue walk would miss. **Measured: 6264 B of 16384 (38%)**, identical
  across boots, versus the paper estimate that had been standing in for it. Two things
  filed off the back of it: `TODO(irq-stack)` (only `#DF` has a dedicated stack — every
  other interrupt nests onto the current thread stack, where Linux at the same 16 KiB uses
  a separate per-CPU IRQ stack) and `TODO(stack-attribution)` (the mark names *who* went
  deep, not *where*).

**Staying deferred** (verified against the code, triggers intact): MSI/MSI-X, IOMMU and
userspace drivers, NVMe, Tier-2 modules, filter drivers, IRP cancellation, networking,
GPU/3D and the compositor protocol, other filesystems, LUKS/LVM, POSIX shim, TypedRecord
enums/generics/lifetimes, iovec, vDSO clock, priority inheritance, EDF, NUMA, per-CPU slab,
empty-slab reclaim, DMA zones, intermediate page-table reclaim, the SMP panic path, and
systemwide tracing.

### Typed shell + coreutils (subproject)

The prereqs above are in, so this subproject is **🚧 active** (from 2026-07-24, at Milestone 1 —
`list` + `copy`, branch `phase-4/coreutils-m1`). The language, interpreter, generic operators,
coreutils breadth, and a minimal (non-rich) REPL are its scope:

- **See [`docs/planning/shell-coreutils-plan.md`](shell-coreutils-plan.md)** for the full breakdown
  (milestones, the `~=` regex / `save`-`open` format / env-var gaps, and the deferred rich REPL).
- Design docs: [`docs/history/nitrox-shell-design-v1.1.md`](../history/nitrox-shell-design-v1.1.md)
  (language/grammar) and [`docs/history/nitrox-ui-composition-model-v1.md`](../history/nitrox-ui-composition-model-v1.md)
  (windows/widgets as resource servers).
- The **rich interactive REPL** (reverse-search, Shift-Enter key events, schema-aware completion —
  design §11) is split out and **deferred**, gated on the console/tty server + compositor terminal
  (below). The subproject delivers the language + non-interactive scripts + a minimal line-reader
  on the raw console.

### Display + input

- [ ] **Per-interrupt-context lock-order tracking — `TODO(lockdep-irq-context)`.** Scheduled
  here, first, and deliberately *ahead* of the handlers below (decision 2026-07-29). The D4
  tracker orders locks on a per-CPU held-rank stack, which cannot express "the order restarts
  in interrupt context" — so `tlb::LOCK`, held with interrupts enabled by the F1 fix, is
  exempt (its own no-lock-held contract *is* asserted; what is unchecked is anything taken
  beneath it in interrupt context). That gap is narrow while there are three interrupt
  handlers the boot exercises end to end. This slice adds real ones — keyboard, mouse, and a
  display path — which is when interrupt-context locking stops being enumerable by hand.
  Needs every interrupt entry **and** exit hooked; missing one corrupts the tracker silently
  rather than failing loudly, so it wants its own negative controls.
- [ ] Display server over the persisted **boot framebuffer** Limine hands us (GOP-style, no modesetting — GPUs are too opaque to modeset blind; firmware-fixed resolution, one linear framebuffer, no acceleration)
- [ ] Input routing: keyboard + mouse (PS/2 under QEMU; USB HID later — see below)
- [ ] Font rasterization (a `no_std`-friendly Rust crate, e.g. `fontdue`/`ab_glyph`) + a text/ANSI render path

### Compositor + shared GUI toolkit

- [ ] Compositor (userspace server): windows/surfaces, stacking, focus, damage/redraw
- [ ] Shared GUI toolkit (the "common GUI library"): window creation, an event loop, drawing primitives, basic widgets. **Conventional surface model first** (apps draw into a surface; the compositor composites — Wayland-shaped)
- [ ] **Dynamic linking** — scheduled here rather than "opportunistic", with the
  process-memory-model bundle (CoW, lazy `MemoryObject`, rlimits, guard pages). Everything
  is static today and that is correct at 13–73 KB per binary, but **static linking defeats
  page sharing exactly where it starts to pay**: shared file-backed text (B4a) shares pages
  across instances of *one* program, and two apps that each embed the toolkit hold identical
  code in *different files*, so they share nothing. Needs TLS first, then `ET_DYN`/`PT_INTERP`
  in the loader, a userspace `ld.so`, and an answer to Rust's lack of a stable ABI — where
  the content-addressed store's generations make whole-system build coherence a better fit
  than a C seam. **Decide the toolkit's ABI seam when the toolkit is designed**; build the
  loader at the second or third app. See `deferred-decisions.md`.
- [ ] `WidgetRecord` model layered on top **later, as the typed opt-in** (programs emit structured UI over a typed stream; the display server renders — the text-floor/typed-stream duality on the screen). The first desktop is **not** gated on this research bet.

### Desktop apps (the north-star MVP)

- [ ] **GUI terminal** (hosts the shell) — the MVP flagship
- [ ] **GUI file browser**
- [ ] **GUI text editor**

### The full std cluster (parallel, consumer-driven)

Not a desktop-MVP gate — the desktop can be built on `no_std + alloc` + crates + FP. Full std
lands with **portable application programs** and the **browser**. `std` is the portable API
for application code; libos/libstream stay the capability-native API for system code. It sits
on the native ABI (no kernel change): `std::fs` resolves paths through the process's root
namespace (bounded ambient, capability-safe); `std::io` blocking maps to `sys_io_submit` +
`block_on`. See the decision log (2026-07-20 std stance; supersedes 2026-07-13).

- [ ] Thread-local storage (`FS_BASE` / `sys_thread_set_tls`)
- [ ] Real `std::thread` — multi-threaded user processes; this triggers the slice-3b **cross-CPU deschedule IPI** (its first consumer) + per-thread FPU/TLS
- [ ] `std` subset over the native ABI: `std::{fs,io,sync,thread}` (`net` after networking)
- [ ] Target spec: `x86_64-unknown-nitrox.json`
- [ ] First non-trivial external Rust crate ported unmodified; a Nitrox program cross-built + run on Linux (portability proof)

### Subsequent north stars

**Web browser** (a capstone / integration test — exercises networking, TLS, threads, FP/SIMD,
graphics, fonts, memory, std at once). Favor a **hybrid**: reuse pure-Rust Servo crates
(`html5ever`, `cssparser`, `selectors`) + a pure-Rust JS engine (`Boa`, restricted subset)
over porting full Servo (SpiderMonkey/C/GPU weight, which would force the POSIX C shim early).
Portable to Nitrox/Linux/Windows.

- [ ] Restricted HTML/CSS/JS engine on pure-Rust crates
- [ ] `rustls`-based HTTPS (needs networking below)

**Networking** (gates `std::net`, NTP, the browser's fetch path):

- [ ] Network driver (e1000 or virtio-net as starting point)
- [ ] Userspace netstack server (smoltcp port or from-scratch)
- [ ] Socket-as-namespace-resource architecture
- [ ] DHCP, DNS
- [ ] TLS-the-protocol via `rustls` + a Rust crypto provider

**Package management + system administration** (the content-store daemon + generations + GC,
pulled up from the Phase 3 backlog; the "sysadmin layer" of a production-feel OS):

- [ ] Package manager daemon (list/add/remove store paths)
- [ ] Generation manifests + atomic switch/rollback
- [ ] Store GC (mark reachable, sweep unreachable)

### Opportunistic / trigger-driven

Landed when a concrete consumer or need appears, not on a fixed schedule:

- [ ] **USB subsystem** (xHCI + USB core + HID) — real-hardware input/storage; QEMU gives PS/2, so it trails the QEMU-first loop
- [ ] **POSIX C shim** — deferred until a must-have C dependency forces it (target the pure-Rust ecosystem first)
- [ ] **Additional filesystems:** fs-server-fat read-write (ESP updates from within the OS; also the orphaned Phase-2 "FAT read-only" deferral folds in here), btrfs/xfs if a use case emerges
- [ ] **Phase 2 ACPI:** vendor ACPICA (`kernel/vendor/acpica/`), OSL (`kernel/src/kacpi/osl/`), `bindgen` integration, power-management daemon — triggered by laptop / graceful-shutdown needs
- [ ] **GPU / compositor acceleration** — modesetting GPU driver is out of scope (opacity); the boot framebuffer is the display substrate
- [ ] **aarch64:** fill `kernel/src/arch/aarch64/` stubs once x86_64 is mature; equivalent userspace work

### Notes

This phase is open-ended. The implementation plan stops being useful as a fine-grained
tracking tool around here; ongoing work is better tracked as GitHub issues / project boards.
The north star and the decision log (2026-07-20) are the durable guides.
