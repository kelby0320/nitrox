# Entropy

Nitrox keeps a single in-kernel **CSPRNG**: a cryptographically-strong random
generator seeded from hardware (`RDSEED`/`RDRAND`) and software (interrupt-timing
jitter) sources, drawn through a ChaCha20 stream. Userspace reaches it through an
`EntropyObject` handle; the kernel draws from the same generator for address-space
layout randomization (ASLR) and the handle-table slot shuffle.

This document is the design for the entropy subsystem. Exact ABIs (the
`sys_entropy_*` signatures) live in [`docs/spec/syscall-abi.md`](../spec/syscall-abi.md);
this is the *why* and the *shape*. The original sketch is
[`docs/history/os-design-v5.1.md`](../history/os-design-v5.1.md) § Entropy.

> **Implementation phasing.** This doc designs the whole subsystem, but it lands
> across **Phase 2 slice 2** (this slice) plus one consumer in slice 3:
> - **Slice 2 (this slice):** the ChaCha20 CSPRNG, the hardware-RNG arch interface,
>   the entropy pool + boot seeding + interrupt-jitter mixing + periodic reseed, the
>   `EntropyObject` kernel object, and the `sys_entropy_create` / `sys_entropy_read`
>   syscalls. The kernel's own PRNGs (handle-table shuffle; later ASLR) re-seed from
>   it.
> - **Slice 3 (resource servers):** the `/dev/entropy` resource server that binds an
>   `EntropyObject` into each process's namespace. Until then a process obtains a
>   handle directly via `sys_entropy_create`.
>
> Each section below marks which wave a feature belongs to.

## Goals and non-goals

The CSPRNG exists to make these unpredictable to an attacker who has not been told
the value:

- **ASLR** — ELF base, stack, and mmap-arena placement (28 bits) at spawn.
- **Handle slot shuffle** — the handle table's per-segment free-list order
  (`handle/prng.rs`), so a fresh handle's low bits aren't guessable.
- **Future cryptography** — nonces, key material, TLS, the content-addressed store's
  hashing salt, etc., as those land.

**It is not** the primary defense against handle forgery. A handle is unforgeable
because of the owner-PID check plus the 32-bit per-slot generation counter
([`docs/spec/handle-encoding.md`](../spec/handle-encoding.md) § Validation); the
slot shuffle is only defence-in-depth on top of that. Entropy quality affects the
*distribution* of slot indices, never the *correctness* of any rights or owner
check.

**Quality target.** The pool is declared **seeded** once it has absorbed an
estimated **≥ 256 bits** of entropy. Before that, reads block (see "The read
contract"); after that, the CSPRNG produces output indefinitely, reseeding
periodically.

## Sources

### Hardware (slice 2, x86_64)

A neutral arch interface (`crate::arch::Entropy`, mirroring `Paging`/`Timer`)
exposes a single fallible draw; the x86_64 implementation uses:

- **`RDSEED` preferred** — samples the on-die conditioned entropy source directly
  (the right primitive for *seeding* a software CSPRNG).
- **`RDRAND` fallback** — a CSPRNG reseeded from that source; used when `RDSEED` is
  absent or repeatedly returns "no value ready".

Both are **CPUID-detected** (`CPUID.07H:EBX.RDSEED[18]`, `CPUID.01H:ECX.RDRAND[30]`)
via the existing `regs::cpuid`. Both set CF=0 when no random value is ready; the
draw **retries a bounded number of times** then gives up for this round (the
caller treats a give-up as "no hardware sample this round", not a hang). When
**neither** instruction exists (older CPUs, some hypervisors), the pool seeds from
jitter alone — slower, and the only realistic case where a userspace read could
observe the unseeded path.

### Software (slice 2)

- **Interrupt-timing jitter.** At each interrupt dispatch (the timer tick via
  `sched::on_timer_tick`, and device IRQs via handlers registered through
  `idt::register_device_handler`), the low bits of `regs::rdtsc()` are sampled and
  pushed into the pool. The unpredictability is in the *fine-grained timing* of when
  interrupts arrive relative to the TSC — accumulated over hundreds of samples.

### Not entropy sources

HHDM/physical addresses, boot parameters, the boot-time TSC value alone, or
**anything deterministic at boot**. These may be *mixed in* (they don't hurt) but
contribute **zero** to the entropy estimate.

### Trust model: mix, never trust

No single source is trusted on its own — least of all an opaque hardware RNG. Every
sample (HW and jitter alike) is **absorbed into the pool**, never used as CSPRNG
output directly. A backdoored or broken `RDSEED` cannot weaken output below what the
jitter contributes, and vice-versa.

## The pool and the seeded gate (slice 2)

Raw samples are **absorbed** into a fixed-size pool by a mixing step (a keyed
permutation / hash absorb — not concatenation), so that low-entropy samples
accumulate rather than overwrite. The subsystem keeps a conservative **entropy
estimate** (fixed bits credited per HW sample; a small fraction of a bit per jitter
sample); when it crosses **256 bits** the pool latches **seeded** (a one-shot flag,
never cleared). Latching seeded **keys the CSPRNG** from the pool.

The pool state is bounded and statically sized — no allocation on the sampling
path (it runs in interrupt context).

## The CSPRNG (slice 2)

A hand-rolled **ChaCha20** (RFC 8439) stream generator — **no external crates**
(per `kernel/CLAUDE.md`), living in `libkern` and unit-tested against the RFC 8439
test vectors.

- **Output.** Each draw produces keystream blocks from the current key + a
  monotonic block counter.
- **Fast key erasure (forward secrecy).** After serving a request, the generator
  overwrites its own key with fresh keystream and resets the counter. A later
  compromise of the key cannot reconstruct earlier output.
- **Reseed.** The CSPRNG folds fresh pool entropy back into its key **periodically**
  (a wall-clock interval) and **after a byte threshold** of output, so long-running
  systems keep pulling in new interrupt jitter / HW samples.

### Why ChaCha20

- **No FPU/AES-NI dependency.** The kernel is soft-float and must not assume AES-NI;
  ChaCha20 is integer add/xor/rotate only — it runs anywhere and needs no XSAVE
  state in the context switch.
- **Auditable.** ~100 lines, a well-specified RFC, easy to test against published
  vectors — appropriate for a from-scratch kernel that forbids external crates.
- AES-CTR (rejected) would either depend on AES-NI or need a large constant-time
  software S-box; HW-RNG-as-CSPRNG (rejected) violates "mix, never trust".

## Boot integration (slice 2)

Entropy init runs in `kernel_main` (`kernel/src/main.rs`) **after** the APIC/timer
are up (so `rdtsc` and interrupts are live) and **before** the handle table is
initialized (so the table seeds its shuffle PRNG from the CSPRNG instead of the
fixed `PHASE1_SEED`). At init it:

1. draws an initial burst from `RDSEED`/`RDRAND` (when present) — typically enough
   to cross the 256-bit gate **immediately, in microseconds**;
2. mixes in early TSC jitter;
3. latches **seeded** and keys the CSPRNG.

**Seeded-before-userspace guarantee.** On any CPU with `RDSEED` or `RDRAND` the pool
is seeded long before `run_first_userspace`, so a userspace `sys_entropy_read`
returns immediately. The blocking path exists only for the jitter-only case (no HW
RNG), where seeding waits for enough interrupt samples.

**Re-seeding the kernel PRNGs.** The handle-table free-list PRNG (`handle/prng.rs`,
seeded via `PHASE1_SEED` in `handle/global.rs`) and — when it lands — ASLR draw
their seeds from the CSPRNG, closing the `TODO(entropy)` notes.

## The `EntropyObject` and the read contract (slice 2)

`EntropyObject` is a kernel object (`KObjectType::EntropyObject`, already reserved;
principal mask `READ` in `handle/type_rights.rs`). It is a **capability token**: the
random source is a kernel singleton (the one CSPRNG), and every `EntropyObject`
handle is a view onto it — analogous to many handles referring to one object.

```
fn sys_entropy_create() -> isize            // syscall 26
```
Returns a handle to the entropy source with `READ` + the generic management band
(`DUPLICATE | TRANSFER | INSPECT`). (The verb matches `sys_ns_create` /
`sys_timer_create`; it mints a fresh handle/token, not a new random stream.)

```
fn sys_entropy_read(handle: RawHandle, buf: UserPtr<u8>, len: usize) -> isize  // syscall 27
```
Requires `READ` on `handle`. Fills `buf[0..len]` with CSPRNG output and returns the
byte count. Bounded per call (a cap like the `sys_kprint` bounce buffer).

- **Seeded (the common case):** the syscall fills `buf` and returns `len`
  synchronously.
- **Unseeded (rare — only the no-HW-RNG, pre-jitter window):** returns a
  `PendingOperation` handle that completes once the pool seeds; the caller
  `sys_wait`s on it, then re-reads. This keeps the async-first contract
  (`docs/rationale/why-async-syscalls.md`) without ever blocking inside a syscall.
  In practice the pool is seeded before userspace runs, so this path is the safety
  net, not the norm.

Slice-3 `/dev/entropy` changes nothing here: the resource server simply binds an
`EntropyObject` handle into each process's namespace, and clients use the same
`sys_entropy_read`.

## Lock discipline (slice 2)

Sampling runs in **interrupt context** (timer tick + device IRQs), so the pool's
ingest path must not take a rank-ordered sleepable lock or allocate. It uses an
`IrqSpinLock` (or a lock-free push), exactly like the DPC queue — a **leaf** held
alone, briefly, with interrupts masked. The CSPRNG *draw* (from `sys_entropy_read`
and the kernel PRNG re-seed) is a syscall/boot-context lock at the kernel-object
rank. The `seeded` latch and the "wake the waiters" step on seeding follow the
`PendingOperation` completion path (under `SCHED`), with no `ObjectRef` drop under
the lock. The exact rank is fixed in [`kernel/docs/lock-ordering.md`](../../kernel/docs/lock-ordering.md)
when the code lands (Part C).

## Kernel vs userspace split

| Concern | Where |
|---|---|
| ChaCha20 CSPRNG, pool, entropy estimate, seeded gate | **kernel** (slice 2) |
| HW-RNG access (`RDSEED`/`RDRAND`), CPUID detection | **kernel arch layer** (slice 2) |
| Interrupt-jitter sampling | **kernel** (slice 2) |
| `sys_entropy_create` / `sys_entropy_read`, `EntropyObject` | **kernel** (slice 2) |
| Re-seeding the handle-table / ASLR PRNGs | **kernel** (slice 2) |
| `/dev/entropy` resource server + namespace binding | **kernel/userspace** (slice 3) |
| Userspace CSPRNG wrappers (`getrandom`-style in `librt`) | **userspace** (later) |

## Slice-2 vs slice-3 scope

| Item | Slice |
|---|---|
| CSPRNG + pool + sources + seeding + reseed | **2** |
| `EntropyObject` + `sys_entropy_create`/`sys_entropy_read` | **2** |
| Direct `sys_entropy_create` to obtain a handle | **2** |
| `/dev/entropy` binding via the resource server | **3** |

## Deferred

- **Fork / VM-snapshot reseed.** A restored VM snapshot (or a hypothetical
  fork) would replay identical CSPRNG state. The mitigation (a reseed triggered by a
  detected snapshot/migration, e.g. via a virtio-rng or a generation counter) is
  **deferred** — noted in `docs/rationale/deferred-decisions.md` when Part C lands.
- **aarch64 sources** (`RNDR`, SMCCC TRNG) — designed in `os-design-v5.1.md`, built
  with the aarch64 port.
- **Entropy accounting / blocking semantics beyond the one-shot gate** (a Linux
  `/dev/random`-style depleting estimate) — intentionally not done; once seeded, the
  CSPRNG is treated as an unlimited source (the modern consensus).
