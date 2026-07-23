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
  fs-server's block I/O to 4 KiB blocks (8× fewer wakes). **Deferred within dir-ops:** a
  `libos` `open_dir`/`read_dir` client wrapper (parent drives raw syscalls today),
  cross-directory + overwrite `rename`, a new-parent-block grow on a full directory, the
  `MAX_SESSIONS = 7` session cap, and a `File` directory-ops spec doc.
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
- [ ] **stdio / pipe convention** — a spawn contract + library for wiring `stdin`/`stdout`/`stderr`
  channels across pipeline stages, plus a `libstream` stdin-*reader* pattern and `libos`
  pipe-wiring helpers. Resolve the **bootstrap-capacity collision** first: a stage needs 5 handles
  (notif + namespace + stdin + stdout + stderr) but spawn delivers only 4 (`SPAWN_MAX_HANDLES`,
  4 bootstrap registers) — either raise the limit (ABI/spawn-hash change) or adopt the
  stack-resident bootstrap block the kernel already anticipates (`object/thread.rs`). **ABI
  decision; record it in the decision log.**

Each prereq slice self-validates (host tests for the codecs; a throwaway producer/consumer pair in
QEMU for the stdio convention). The first *integrated* proof — real coreutils streaming over a real
pipe — is the subproject's Milestone 1, once these three are in.

### Typed shell + coreutils (subproject)

Once the prereqs above are in, the language, interpreter, generic operators, coreutils breadth, and
a minimal (non-rich) REPL are their own subproject:

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

- [ ] Display server over the persisted **boot framebuffer** Limine hands us (GOP-style, no modesetting — GPUs are too opaque to modeset blind; firmware-fixed resolution, one linear framebuffer, no acceleration)
- [ ] Input routing: keyboard + mouse (PS/2 under QEMU; USB HID later — see below)
- [ ] Font rasterization (a `no_std`-friendly Rust crate, e.g. `fontdue`/`ab_glyph`) + a text/ANSI render path

### Compositor + shared GUI toolkit

- [ ] Compositor (userspace server): windows/surfaces, stacking, focus, damage/redraw
- [ ] Shared GUI toolkit (the "common GUI library"): window creation, an event loop, drawing primitives, basic widgets. **Conventional surface model first** (apps draw into a surface; the compositor composites — Wayland-shaped)
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
- [ ] **Dynamic linking** — off the std critical path (Rust static-links); an ecosystem/image-size concern
- [ ] **POSIX C shim** — deferred until a must-have C dependency forces it (target the pure-Rust ecosystem first)
- [ ] **Additional filesystems:** fs-server-fat read-write (ESP updates from within the OS; also the orphaned Phase-2 "FAT read-only" deferral folds in here), btrfs/xfs if a use case emerges
- [ ] **Phase 2 ACPI:** vendor ACPICA (`kernel/vendor/acpica/`), OSL (`kernel/src/kacpi/osl/`), `bindgen` integration, power-management daemon — triggered by laptop / graceful-shutdown needs
- [ ] **GPU / compositor acceleration** — modesetting GPU driver is out of scope (opacity); the boot framebuffer is the display substrate
- [ ] **aarch64:** fill `kernel/src/arch/aarch64/` stubs once x86_64 is mature; equivalent userspace work

### Notes

This phase is open-ended. The implementation plan stops being useful as a fine-grained
tracking tool around here; ongoing work is better tracked as GitHub issues / project boards.
The north star and the decision log (2026-07-20) are the durable guides.
