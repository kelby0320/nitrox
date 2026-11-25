# Nitrox Implementation Plan — Phase 7 — The portable runtime

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. This phase is
**planned, not built** — nothing below describes current behaviour.

**Status: agreed 2026-08-25; detailed 2026-11-24, on `main` at #470's merge (`7713078f`):
eight parts, A–H, a PR each.**

---

## Phase 7: The portable runtime

**Goal:** ordinary Rust programs run on Nitrox, built unmodified from their published source.

**Agreed 2026-08-25 as "Phase 5"** and renumbered when bare metal and USB moved ahead of it
(decision log, 2026-09-10). Its second half, a Nitrox program cross-built and run on Linux, was
dropped at the detail pass (the maintainer's call): porting a Rust program to Nitrox is the more
useful proof.

**Why these are one phase: thread-local storage is the shared prerequisite.** `std::thread`
needs it, and so does `ld.so`, whose dynamically linked programs' thread-locals the loader
resolves. Doing either alone builds most of the other.

### The calls (2026-11-24, the maintainer's)

1. **`std` is patched, in our own toolchain.** The pinned nightly's `library/std/build.rs` gives
   any `target_os` not on its list, `nitrox` among them, `restricted_std`, which makes `std`
   unstable: every crate using it would need `#![feature(restricted_std)]`, so no crate builds
   unmodified and the no-nightly rule breaks. Our toolchain is the pinned nightly with its
   `rust-src` patched: a `sys/pal/nitrox`, `nitrox` on `build.rs`'s list, and Nitrox in the `cfg`
   lists that choose each implementation. The nightly is bumped rarely, so the patch is redone
   rarely. Upstreaming a target, as Motor OS and Hermit did, and injecting the feature by flag were
   the other two ways.
2. **Thread-local storage has a syscall of its own**, `sys_thread_set_tls`.
3. **Locks wait on an address, through `sys_wait`.** `sys_wait` takes a word and its expected
   value beside its handles, and a new `sys_wake` wakes a word's waiters. `std` then runs its own
   futex-based `Mutex`, `Condvar`, `RwLock`, `Once` and thread parker, the code Linux, Hermit and
   Motor OS run; the patch is `cfg` lines and three functions. The other proposal, an event object
   each thread parks on, left `Mutex` and `Condvar` to be written by us: `std` has no parking-based
   ones.
4. **The program is ripgrep** (`rg`, 16.0.0), with tokei as the fallback.
5. **Panics abort.** The target is `panic=abort` already; unwinding is deferred.
6. **`std::process::Command` gives a child a copy of its parent's namespace**, args, env and stdio,
   as the shell gives its stages; a `std::os::nitrox` extension hands it a narrower one.
7. **Dynamic linking is the phase's last part, and may be deferred**: the Definition of done does
   not need it, and the maintainer does not count it critical.
8. **Ported programs speak bytes, and the shell adapts.** Our `std` marks every program it links
   with an ELF note, "stdio: bytes"; a program without it is typed, as every native one is. The
   shell reads the note at launch and puts a small adapter stage at each pipe where the two kinds
   meet. Invalid UTF-8 in a wrapped line is replaced.

### Where things stand

Measured on `main` at `7713078f`:
- **Threads**: `sys_thread_create` takes a 64-byte `ThreadArgs` (36 bytes reserved) and returns a
  `Thread` handle. SMP schedules them, each with its own XSAVE state. A user thread running on
  another CPU is stopped by an IPI when its process exits (2026-10-25, held by boot-probe's
  `exit_with_a_running_sibling_test`); no lock or wait has yet woken one across CPUs. Only
  test-harness makes threads.
  A `Thread` handle cannot be waited on: `sys_wait` takes timers, notification and IPC channels,
  pending operations and interrupt objects.
- **No TLS**: `kernel/src/object/thread.rs` has no `fs_base`. The kernel never uses `FS`; it
  reaches its per-CPU block through `KERNEL_GS_BASE` and `swapgs`, and keeps user `GS_BASE` 0.
- **No wait on an address**, and no kernel object one thread signals for another. Arming a timer
  in the past fires at the next tick (`timer_arm` pushes the deadline heap), up to 10 ms later.
- **The ELF loader** loads static, non-PIE `ET_EXEC` only, refuses `ET_DYN` and `PT_INTERP`, and
  ignores `PT_TLS`. A file maps through the page cache (`Backing::File`).
- **The toolchain**: `x86_64-unknown-nitrox.json` (`"os": "nitrox"`, no target family,
  `panic=abort`) on `nightly-2026-07-20`, rebuilding `core`, `alloc` and `compiler_builtins` only.
- **Pipelines**: a stage's stdout is the next stage's stdin, and the shell holds the last stage's
  output. Every reader requires the TSM1 header. The shell reads a program's head before it runs
  it (`exec::what_runs`). Arguments and streams come in a setup message after spawn.
- **The rest `std` stands on**: libheap is thread-safe, by a spinlock; `Monotonic` and `Realtime`
  clocks; `/dev/entropy`; the online CPU count in `/proc/sched/stats`.
- **ripgrep 16.0.0**, audited by reading: 37 crates for our target, Linux's set but `libc`. Every
  item gated on `unix` or `windows` that the rest of a library calls has a third arm, `memmap2`'s
  stub and `same-file`'s `unknown` among them; the few without one are extras (`crossbeam-utils`'
  `JoinHandleExt` impls, `grep-searcher`'s `madvise`) or tests. Some third arms change what a
  flag does: F.4 lists them.

---

## Part A — The kernel's threads, ready for `std`

Kernel changes, each a syscall-ABI change: `syscall-abi.md`, `thread-args.md` and the ABI hash
move with them.

### A.1 Thread-local storage

- **`sys_thread_set_tls(base)`** sets the calling thread's `FS_BASE`. A thread keeps it in an
  `fs_base` field, and **a switch into a thread writes it, by `wrmsr`, every time**: ring 3 can
  change `FS_BASE` without a syscall, by loading a selector into `FS` (`mov fs, ds` loads the user
  data descriptor's base, 0), so no remembered value can stand for the register. The recorded base
  is the thread's: one that loads `FS` itself has the base restored at its next switch. The
  `wrmsr`'s cost on the switch path is measured. A new thread starts at 0, a spawned process's
  first thread too.
- **A base at or past `USER_VIRT_END` is refused** (`InvalidArgument`), as `sys_thread_create`
  checks an entry and a stack: a non-canonical `wrmsr` is a #GP in ring 0, which stops every CPU.
- **Not `wrfsbase`**: enabling `CR4.FSGSBASE` would let userspace set `GS_BASE` as well, and the
  syscall entry's `swapgs` discipline is argued on user `GS_BASE` being 0.
- **Tests**: the switch path's host tests; the bound at `USER_VIRT_END` and one below it; a
  boot-probe check of two threads on two CPUs each reading its own base through `fs:`, across
  forced switches; a thread that loads `FS` itself, then a thread of another process with the same
  base switched in after it on that CPU; and a base kept across a return from an interrupt.

### A.2 Waiting on an address

- **`sys_wait(handles, count, results, deadline, word, expected)`**: with a non-null `word`, the
  wait also ends when the word is woken. If the word does not hold `expected` when the wait
  begins, it returns at once. With `count == 0` and a word, waiting for ever is allowed.
- **The word is a 4-aligned `u32`**, `std`'s futex, read by one aligned 32-bit load and compared
  in 32 bits; a misaligned one is refused (`InvalidArgument`), so no word straddles two pages.
- **`sys_wake(word, count)`** wakes up to `count` of the word's waiters, and says how many. A
  waiter it counts reports woken, even if its deadline came at the same moment, so a `Condvar`'s
  notify is not spent on a thread told it timed out.
- **Private to a process**: keyed by address space and address. Waits on shared memory, keyed by
  memory object and offset, can come later in the same shape.
- **The read of the word under the wait's lock must not fault.** It reads with faults off; on a
  page not present it drops the lock, faults the page in, and starts again. **A page that cannot
  be faulted in** (unmapped, a guard page, a file page whose fill fails) ends the wait with the
  fault's error, not another try. This is the part to get right, and its own sub-part's tests: a
  word on a page never touched, one on a file page, and one on no page at all.
- **The kernel counts address waits and wakes**, so a gate can tell contention happened.
- **Tests**: the table's host tests (Miri runs them); a lost-wakeup check (the waker stores, then
  wakes; the waiter reads under the lock) between threads on several CPUs under KVM, the window
  between the waiter's read and its sleep widened by a probe delay, so a check outside the lock
  is caught; a mismatch returning at once; a misaligned word; a timed wait ending at its
  deadline, and a wake meeting a deadline; a wake of `n` of `m`; a process exit ending its
  threads' waits.

### A.3 A thread's end, waitable

- **A `Thread` handle signals in `sys_wait` once its thread has ended** and the kernel no longer
  touches its user stack, so `std`'s `join` frees the stack only then. Waiting on it needs `WAIT`
  on the handle, which `sys_thread_create`'s handle gains.
- **Tests**: a join after the thread ended and one before; a handle waited on by two threads; a
  joiner that unmaps the stack the moment it wakes, while a probe delay holds the ended thread's
  last steps in the kernel after the signal, so a signal given too early faults.

### A.4 The CPU count

- `std::thread::available_parallelism` sizes ripgrep's walker: without it the walk runs on one
  thread. The count is in `/proc/sched/stats`, which an application's namespace may not reach.
  **This sub-part decides between that snapshot and a general query**, by what an application's
  namespace holds; a call of its own is the last choice.

## Part B — Our toolchain, and a first `std` program

### B.1 The patched toolchain

- **The patch lives in `tools/std/`**, against the pinned nightly's `library/`: the PAL's new files
  whole, and each edit to an existing file as a small diff. `cargo xtask toolchain` assembles
  `tools/build-cache/toolchain/`, the pinned nightly with its `rust-src` replaced, and links it
  with `rustup` under a name of its own.
- **Measured first**: `rustc` finds its sysroot from its own binary, so the directory must hold
  copies where links would lead back to the stock toolchain. What it costs to build, in time and
  bytes, decides how CI caches it, keyed by the patch's hash.
- **The no-nightly rule is unchanged.** The patch is the toolchain's: `std`'s own source uses
  nightly features, as it always has. `check-nightly` reads `kernel/` and `userspace/`, which
  still hold no `#![feature]`.

### B.2 The target, and what `std` stands on

- **What a `std` program needs to link is the target's**, not each crate's: a port's workspace has
  no `.cargo/config.toml` or `build.rs` of ours. Today's link scripts cannot link one, and linking
  a test object against `nxsh`'s `user.ld` shows why: no `PT_TLS` segment (a thread-local "doesn't
  have a PT_TLS segment"), the ELF and program headers not loaded (so `__ehdr_start` is 0), and
  `.note.*` discarded. The target spec gains `has-thread-local` and a link script of its own with
  a `PT_TLS` segment, the headers in the first segment, and the stdio note kept; and it carries the
  static, non-PIE link the native crates set in their own config, so a port does not come out
  position-independent, which the loader refuses.
- **`std` reaches the kernel through `libkern`**, which takes a `rustc-dep-of-std` feature, so the
  syscall numbers and layouts have one source. `std::fs` stands on `librsproto` and `libfs` the
  same way: `librsproto::path::resolve` stays the one path resolution (`nxsh/CLAUDE.md`). Each such
  crate, `libos` with them, must build as a dependency of `std`.
- **`userspace/CLAUDE.md`'s dependency rules** say "`no_std`, with `alloc` at most" and "anything
  reaching for `std` will not build". A ported program is the exception, and the rules say so in
  this sub-part, with the root `CLAUDE.md`'s toolchain paragraph.

### B.3 A first `std` program

- **The start**, in the PAL: `_start` finds `PT_TLS` through `__ehdr_start`, builds the main
  thread's TLS block and sets it (A.1), receives the setup message, and calls `main` with the
  heap ready (libheap as `System`).
- **stdout and stderr** write raw bytes on the stage's channels; `main`'s return and
  `process::exit` set the exit status.
- **The process's notification queue has one owner**, a runtime thread `_start` starts, as no
  other `std` code reads it. It routes each notification: `TerminateRequested` ends the process as
  a native stage asked to stop ends (Ctrl-C at the shell, a supervisor's stop: without it a ported
  program ignores both, and the shell waits on it for ever); a `ChildExited` goes to the `Child`
  waiting on that pid (D.6); a fault of one of the process's own threads ends the process.
- **An abort ends the process.** `std`'s fallback `abort_internal` is `core::intrinsics::abort()`,
  a trap, and on Nitrox a ring-3 fault suspends its thread for good: the process would never exit.
  The PAL's `abort_internal` is a process exit instead, and a panic writes its message, then
  aborts. A thread that faults, one run into its guard page among them, ends the process through
  the runtime thread. **Either reads to the parent as `Crashed`**, not as a normal exit, which
  `sys_process_exit` cannot say today: this sub-part decides how the kernel lets it.
- **The program carries the note**, which a test reads back from the built binary.
- **Gate**: a "hello" `std` program, run by boot-probe, prints its args and environment; one that
  panics, one that overflows a thread's stack, and one asked to stop each end, the first two read
  as `Crashed`.

## Part C — Threads and locks

- **C.1 `std::thread`**: spawn maps **one region a thread**, a guard page, its stack and its TLS
  block, and starts the thread at an entry that sets its TLS first. `join` waits on the `Thread`
  handle (A.3), then unmaps the region. **A detached thread** (its `JoinHandle` dropped) frees its
  own: its last step closes its handle and unmaps the region, then exits, in a few instructions
  that use no stack, as musl's `__unmapself` does. `sleep`, `yield_now`, names,
  `available_parallelism` (A.4).
- **C.2 `std::sync`**: `nitrox` joins the futex arms of the `Mutex`, `Condvar`, `RwLock`, `Once` and
  thread-parking lists; the PAL's `futex_wait`, `futex_wake` and `futex_wake_all` call A.2. A
  timed wait's deadline resolves on the 10 ms tick, as every `sys_wait`'s does.
- **C.3 `thread_local!`** on native TLS. For an OS not on its list, `std`'s destructor guard
  falls to `key::enable`, which needs a `LazyKey` the key list's catch-all does not define, so
  Nitrox joins the guard arm Hermit's and Xous's share (`enable() {}`), and the PAL's thread entry
  runs `destructors::run` and `rt::thread_cleanup` before the thread exits, as Hermit's does.
- **Gate**: a `std` test program on four vCPUs under KVM: threads contending a `Mutex`, a
  `Condvar` ping-pong, `RwLock` readers and a writer, a `Once`, a `thread_local!` per thread whose
  `Drop` is seen to run at the thread's end, every thread joined, and a thread detached. The
  kernel's address-wait and wake counts (A.2) must each pass a floor the test sets, so a run in
  which no thread waited, or a `sys_wake` that did nothing while waiters spun, fails.
- **libheap's lock** may sleep on A.2 under contention, if C's gate measures it spinning.

## Part D — `std`'s I/O

- **D.1 `fs`**: `File` (open, create, read, write, seek, `set_len`, metadata, sync), `read_dir`,
  `create_dir`, `remove_file`, `remove_dir`, `rename`, `metadata`, over `librsproto` and `libfs` and
  the file-backed mapping. A relative path resolves against `PWD` in the process's own namespace,
  the bounded ambient authority of 2026-07-20. `OsStr` is bytes.
- **D.2 `io`**: blocking is `sys_io_submit` and `block_on`. stdin reads raw bytes from the fed
  channel or the terminal. `is_terminal` answers from the setup message: stdout when the stage is
  last and shown, stdin when it reads the terminal.
- **D.3 `env`**: `args_os` from the setup message's `argv`, `vars` from the environment record,
  `current_dir` from `PWD` and `set_current_dir` setting it.
- **D.4 `time`**: `Instant` on `Monotonic`, `SystemTime` on `Realtime`.
- **D.5 random**: `HashMap`'s keys from `/dev/entropy`.
- **D.6 `process`**: `Command` spawns by path with a copy of the parent's namespace (call 6), its
  args, env and stdio. `wait` waits for its own child's `ChildExited`, which the runtime thread
  (B.3) routes by pid: the queue also carries other children's exits, so two threads waiting on
  two children each get their own. No `fork`. `net` answers `Unsupported`.
- **Gate**: a `std` test program for each area, run by boot-probe, each asserting what it read
  back and printing a line a check: a file written, read, sought, cut and renamed, a directory
  listed; args and env as sent; `Instant` moving forward and `SystemTime` near the clock's; a child
  spawned, its output read and its exit code waited on, two children at once from two threads. The
  gate matches each line, and an area answering `Unsupported` must fail its program.

## Part E — The shell meets ported programs

- **E.1 The note**, found by the shell from the program headers its head holds: a second read at the
  `PT_NOTE` they name. The head is `HEAD_LEN`, 256 bytes, and a `std` program's ELF and program
  headers alone pass it (64 and 56 bytes each), so the note is never within it. A program without
  the note is typed.
- **E.2 Two adapter stages**, native programs with host-tested logic: one wraps bytes as a
  text-fallback stream, a `line` record a line, invalid UTF-8 replaced; one renders a stream as
  text, a text-fallback or one-column stream as its lines and any other table as `display` shows
  it, uncoloured.
- **E.3 The shell places them**: at a typed-to-bytes or bytes-to-typed pipe, an adapter; bytes to
  bytes, a direct pipe; a bytes stage last and shown, its bytes straight to the terminal; captured,
  wrapped. A bytes stage gets a stderr of its own, shown as plain error text, as the shared one's
  lines open with a level byte.
- **E.4 The specs**: `pipeline-stdio.md` gains the two kinds, the note and the adapters;
  `typed-stream-format.md` § Text fallback, "wrapped by the runtime", becomes "by the shell".
- **Gate**: a bytes test program in the test package, which only a test root holds, so
  `check-images`' allow-list is unchanged: boot-probe runs `nxsh` over `prog | count`,
  `open f.txt | prog`, `prog | prog` and `prog` shown, and matches each result. `test-interactive`,
  which boots the release image, holds the same through ripgrep in F.3.

## Part F — ripgrep

- **F.1 Built unmodified**: ripgrep 16.0.0 from crates.io, `--locked`, built by our toolchain for
  our target in a workspace of its own, with its own `Cargo.lock`. The dependency rules apply to
  its 37 crates: pure Rust, pinned, each licence in `assets/licenses`. **This sub-part decides**
  between fetching through CI's registry cache and vendoring the sources.
- **F.2 In the image**: `/bin/rg` from the store, its licences staged.
- **F.3 The gate**: `test-interactive` runs `rg` over a known tree: a match, `-i`, `-c`,
  `rg foo | count`, `open f.txt | rg foo -`, `rg --version`, and Ctrl-C during a long search
  returning the prompt. **The walk on several threads is read back**: `rg --debug` logs
  `using N thread(s)`, and the gate requires N above one.
- **F.4 What it cannot do yet**, said:
  - **A pipe into `rg` needs `-`**: `grep-cli`'s `is_readable_stdin` answers false on an OS that
    is neither `unix` nor `windows`, so `rg foo` with no path searches `./`, not its stdin.
  - `--one-file-system` fails on every entry (`ignore`'s `device_num` has no third arm that
    answers), and `-L` on each followed directory (`same-file`'s `unknown` handle always errors).
  - A non-UTF-8 argument or path does not round-trip: `std::os::unix::ffi` is absent.
  - Memory maps fall back to reads (`memmap2`'s stub), and hyperlinks have no hostname.
- **If ripgrep needs a source change**, it stops being the proof, and tokei takes its place.

## Part G — Dynamic linking (may be deferred)

- **G.1 The kernel** accepts `ET_DYN` and `PT_INTERP`: it maps the interpreter at a random base
  and enters it.
- **G.2 A userspace `ld.so`** maps segments over file-backed memory, walks the dependency graph,
  applies the `RELATIVE`, `GLOB_DAT` and `JUMP_SLOT` relocations, resolves symbols, runs the init
  and fini arrays, and serves TLS's dynamic models (`__tls_get_addr`).
- **G.3 The toolkit as the first shared library** in the five applications, under the deferral's
  ABI answer: one compiler a generation, Rust's ABI within it. `std` stays static.
- **G.4 Measured**: the image's bytes and the pages resident, against today's 176–539 KB
  binaries. If the maintainer defers the part, its entry in `deferred-decisions.md` stays, with this
  plan as its shape.

## Part H — Miri at the phase's end

Over the job's targets the phase changes, the kernel's among them; Tree Borrows over the `unsafe`
added. The ported crates are not in the job: their `unsafe` is theirs.

---

### What Phase 7 does not do

- **`std::net`**, until [Phase 8](phase-8-networking.md).
- **Unwinding**: panics abort (call 5).
- **`fork`**, and **the POSIX C shim**, deferred until a must-have C dependency forces it.
- **A Nitrox program on Linux**, dropped at the detail pass (the Goal, above).
- **`thread_set_affinity` and `io_cancel`**: `std` asks for neither. Both stay deferred.
- **Waits on shared memory**: A.2's waits are private to a process.

### Definition of Done

- **ripgrep 16.0.0, built unmodified from its published source**, runs on Nitrox, searching a tree
  on more than one thread by its own count, in a gate.
- **`std`'s `fs`, `io`, `thread`, `sync`, `env`, `time` and `process`** each hold their test
  program's checks in a gate (C's and D's), the threads on several CPUs with contention counted.
- **Ported programs and native ones pipe into each other** both ways, and a ported program stops
  when it is asked to.
- **Dynamic linking** built, or deferred with its entry (the maintainer's call).
- **Miri at the phase's end** triaged.
