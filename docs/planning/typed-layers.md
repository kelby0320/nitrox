# The kernel's typed layers

**Status: added 2026-10-27, when the safety survey was triaged (the maintainer's call); detailed
2026-11-05.** Part J of [`pre-phase-7.md`](pre-phase-7.md), between Part E's two halves
([`syscall-surface.md`](syscall-surface.md) § *The order*).

## Why

Part I fixed the survey's 221 defects a site at a time. Six of its eight layers end a class
instead, each held by a type where a convention holds it now
([`safety-survey.md`](safety-survey.md) § *The triage*):

| | Layer |
|---|---|
| P1 | The typed kernel object: tag and pointer tied, liveness and the scheduler lock as types |
| P2 | Frames: an owned frame, and a bounded view of its bytes |
| P3 | Page tables: typed roots, atomic slots, one walk |
| P4 | Per-CPU state: an index that cannot outlive the code that pins it |
| P5 | Device memory and I/O: in-flight IRPs, DMA and MMIO accessors, device teardown |
| P6 | Unsafe that needs none: functions whose only contract is "ring 0" |

They change no syscall, so they are a part of their own. They run after every crate is on libos,
and before E.6 to E.11, whose new kernel code, threads holding files, buffers and
`PendingOperation`s and copying between frames, wants P1 and P2 under it.

## Part J in detail

*Detailed 2026-11-05, measured on `main` at PR #422's merge (`d6cf7ca7`).* Where the survey's text
and this differ, this is the plan. **Fifteen sub-parts, a PR each.**

### What the pass found

- **Part I already took part of eleven items.** kplat-o1's atomic slots and single walk are its
  `TableRef` (kplat-d7). kmem-o6's two defects closed with a kernel stack that keeps no root (kmem-d4)
  and a loader that takes the address space `&mut`. sched-o6's inline waiter set is `WaitQueue`.
  drv-o13's free-before-disable is `Programmed` and `Quiesced` (drv-o4), and its unfenced ring writes
  are one `RingMem`. drv-o3's overrun of the hub's data page closed (drv-d8). drv-o1's single
  completion is `io::irp::complete`, its provenance the box's. drv-o12's function-pointer compare
  became a window the node carries (drv-d16). kplat-o12's refused installs take no vector.
  kplat-o6's log copy is `copy_in` (kplat-d15). sched-o8's clones, dropped under SCHED, went when
  the scheduler stopped borrowing a whole `Thread` (kobj-d11), leaving raw reads. And **every bug
  kplat-o3 was to close is fixed** (kplat-d2, d8, d11, d18, d19): what is left of P4 is debug
  assertions to make types.
- **Six things the survey drew do not hold:**
  1. **Masked interrupts do not pin a thread across a switch.** The scheduler switches with IF=0,
     so a token taken from IF=0 is good only until the next call that can switch (J.11).
  2. **A host-test twin cannot live inside a generic per-CPU type**: `thread_local!` needs a static
     of its own, so it is a macro (J.11).
  3. **No `&'static AhciDisk` exists when its node is made**, before the controller is published, so
     a block backend's context cannot be an enum holding one (J.14).
  4. **A safe `Copy` handle to DMA memory is what Part I removed** (`RingMem::kept` is `unsafe`), so
     drv-o3's `DmaRegion` does not come back (J.12).
  5. **A raw copy does not end kobj-d13's race**: `copy_nonoverlapping` forms no reference but is
     still a non-atomic read racing a writer, which no test could tell from the slice (J.8).
  6. **A vector cannot simply be cleared once a device was programmed with it**: an enabled MSI
     may still signal, and so may a line routed to it (J.13).
- **Two latent defects, kplat-d23 and sched-d16**, appended to the survey's record and fixed in
  J.11. `init_syscall_entry` runs after preemption is armed and the APs are up, unpinned, and reads
  its CPU's block before writing that CPU's MSRs; two later re-arms are all that cover a migration
  between. `arm_kernel_stack_for` says its caller holds SCHED, and `switch_into` calls it after
  releasing it.
- **Five files sit at their size entries**, so a sub-part that touches one moves code out first
  or shrinks it: `syscall/table.rs` (2,195), `object/file_object.rs` (2,459), `main.rs` (1,079),
  `pci/mod.rs` (1,045) and `arch/x86_64/acpi.rs` (1,036). Eleven more are past 900 lines:
  `mm/vmm.rs` (988), `drivers/xhci/hub.rs` (985), `sched/tests.rs` (980), `arch/x86_64/idt.rs`
  (949), `handle/table/tests.rs` (937), `object/namespace.rs` (933), `syscall/table/tests.rs` (930),
  `drivers/xhci/mod.rs` (921), `device.rs` (916), `arch/x86_64/paging.rs` (913) and
  `object/thread.rs` (909).

### Where it stands

Outside tests, `cargo xtask unsafe-inventory kernel/src` counts **1,113 `unsafe` blocks, 348 `unsafe
fn`s, 52 `unsafe impl`s and 344 raw dereferences.** By subsystem:

| | `unsafe` blocks | `unsafe fn` | `unsafe impl` | raw derefs |
|---|---:|---:|---:|---:|
| `arch` | 207 | 142 | 0 | 13 |
| `sched` | 197 | 45 | 0 | 7 |
| `object` | 178 | 106 | 16 | 87 |
| `drivers` | 134 | 7 | 5 | 30 |
| `mm` | 92 | 23 | 3 | 122 |
| `syscall` | 90 | 0 | 0 | 30 |
| `io` | 64 | 6 | 8 | 42 |
| `libkern` | 40 | 2 | 13 | 7 |
| `handle` | 30 | 3 | 3 | 1 |
| the rest: `main`, `limine`, `boot_selftest`, `power` and eight more | 81 | 14 | 4 | 5 |

Each row is one of the scan's groups, a module's file and its directory together, so the rows sum to
the totals. Each sub-part says what it removes from that count, and from the patterns it names.

### The order

**P6, then P1, P2, P3, P4 and P5**, all before E.6:

- **P6 first**: the smallest, and both P3 and P4 edit what it edits, the paging trait and the
  interrupt mask. Keeping `interrupts_enable`, `restore`, `sti` and `idle_halt` `unsafe` is what
  gives P4's pins meaning.
- **P1 next**, as the layer the others are written on: P2's object methods, P5's typed completions
  and its dispatch take P1's typed references. Within P1, the scheduler's own repetition goes before
  the types that would otherwise touch five copies of it.
- **P2 before P3 and P5**: an owned frame is what a safe map and a `DmaBuffer` hold.
- **P4 needs none of the others**, and goes after the page tables, where it crowds none of them.

**Part J converts every site but what E.9 removes**, `thread_set_affinity` and `io_cancel`, whose
bodies it leaves as they are. Code E.7 moves into a resolve's operations moves typed.

### The yardstick: `check-unsafe`

**A ratchet on the kernel's `unsafe`, as `check-syscalls` is on userspace's raw calls** (decided in
the pass, for the maintainer's eye): `cargo xtask check-unsafe`, in CI from J.1, counts each kernel
subsystem's `unsafe` of every kind, blocks and `unsafe fn`s first, with `unsafe-inventory`'s scan
against `tools/xtask/unsafe-counts.toml`, whose entries J.1 writes from the scan's groups at its
base, not from the table above, and fails a rise, and a stale entry as `check-size` does. Each
sub-part lowers its entries; new kernel code in E.6 to E.11 that needs `unsafe` raises its entry in
the commit that says why. kernel/CLAUDE.md asks for that justification and nothing holds it.

### How each is held

Every sub-part: `/pr-review`; the host tests; and the KVM gates its files reach, `test-qemu` at
least, on the debug kernel every gate boots, so a new debug assertion runs on all of them. **The
whole suite runs under Miri once, after J.15, and Part J is not done until what it reports is
triaged** (the maintainer's call, 2026-11-06: Miri after each phase of work, not each part). A
sub-part runs under Miri only the tests its own controls need, by name, where the control is a
report only Miri can make, as J.6's, J.7's and J.8's are. A
defect's control fails without its fix, and an opportunity's is the tests that already hold the
code, run before and after. Each ticks its items in `safety-survey.md` with the commit that
finished them, and updates the `architecture/` docs it outdates.

### J.1 The `unsafe` that needs none (kplat-o8), and the ratchet

**About eighteen `unsafe fn`s whose whole contract is "ring 0"**, their bodies unable to break
memory safety: `flush_tlb_page` and `flush_tlb_all` (a thread's root never changes, and no PCID is
used), `invlpg`, `wbinvd`, `cli`, `interrupts_disable`, `MemoryTypes::{at, of_mapping,
log_configuration}`, and `tlb::invalidate_local`; 22 call sites. `ArchCpu::halt` has no caller and
goes. **`sti`, `interrupts_enable`, `interrupts_restore`, `idle_halt` and `install_policy` stay
`unsafe`**: each can break the IF=0 window around a switch or a lock's mask. `of_mapping` keeps one
inner block, for the active root, until J.10.

CI denies `unnecessary_safety_comment` and `unnecessary_safety_doc`, so every `# Safety` section and
call-site `SAFETY` on these goes in the same change; `memory-management.md` says the flushes are all
`unsafe`. Control: the compiler both ways, and `kernel_asm.rs`'s floor of five asm blocks, which
the seven left once `halt` goes still meet. With `check-unsafe` and its table. Author.

**Built 2026-11-06.** `check-unsafe` is in CI, its table starting at 1,113 `unsafe` blocks and 348
`unsafe fn`s in 28 modules, and holding `unsafe impl`s, `#[unsafe(..)]` attributes, `extern` blocks
and `static mut`s as well. Eighteen `unsafe fn`s went, the ten named, `tlb::invalidate_local`
under the flushes, and `halt`, leaving 1,094 blocks and 330 `unsafe fn`s. The kernel's `.text`,
debug and release, is byte-identical before and after.

### J.2 The typed kernel object (kobj-o1, with kmem-o1, drv-o2 and kplat-o5)

A tag and a pointer are tied only by hand. Measured: **75 downcasts** written `&*(x.as_ptr() as
*const T)` in 21 files, **21 adoptions** tagged by hand and **22 calls** of the two `adopt` helpers,
**23 `lookup_typed`** calls each followed by a cast, and **17 release-build assertions** of a tag
before a cast, Part I's answer to kmem-d6.

- **`unsafe trait KObject: Send + Sync { const TYPE: KObjectType; }`**, implemented by a
  `kobject!` macro beside each type, which asserts `offset_of!(T, header) == 0`. The header is built
  from `T::TYPE`, so no constructor can mistag one.
- **`ObjectRef::new(KBox<T>)`, safe**, taking the tag from `T`; **`downcast_ref::<T>()`**, an
  `Option<&T>` comparing the header's tag; **`Ref<T>`**, an owning typed reference that derefs to
  `T`, from `new` or `downcast`, and `erase` back; **`lookup_as::<T>`**, for `lookup_typed` and its
  cast. `ObjectRef` reads its tag from the header, where today it keeps the one its creator passed.
- **`Vma`'s backing is an enum**: `Anonymous`, `Object(Ref<MemoryObject>)` or `File(Ref<FileObject>)`,
  and three of the assertions go with it (kmem-o1).
- **`drivers::adopt`, `io::adopt` and `adopt_node` go.**
- **`Thread` has no `&Thread` until J.7.** Its fields are written raw under SCHED, outside any cell,
  so a shared reference to one is kobj-d11's race. `Ref<T>` derefs only for a type whose `&T` is
  sound, marked so; the other thirteen are, keeping what changes in cells or atomics.
- **`Process` is not `Send` today**, the VMA tree's links being raw, so the supertrait makes that an
  argued impl, where `ObjectRef`'s blanket claim covers it now.

Control: a downcast to each other kind is `None`, and fails if it reads a recorded tag rather than
the header's, shown by an `ObjectRef` made with the wrong one through the `unsafe` `from_raw` that
handle entries keep. The author writes the types; the implementer moves the sites a directory at a
time, each diff read.

**Built 2026-11-06.** None of what the pass measured is left: no hand-written downcast of an
`ObjectRef` (76 at J.2's base), no hand-tagged adoption or `adopt` helper call, no `lookup_typed`,
and no tag assertion before a cast. Tests are included; `Thread`'s raw accessors are left for J.6
and J.7. The kernel's `unsafe` blocks fall from 1,094 to 954 and its `unsafe fn`s from 330 to 327.
Its `unsafe impl`s rise from 52 to 67, each raised by hand:
- the thirteen `Shared` claims, each argued beside its type;
- `kobject!`'s impl;
- `VmaTree: Send`, which is where `Process`'s missing `Send` came from; the tree owns every node
  its links reach.

Two things differ from the plan:
- **`from_raw` takes no tag.** An `ObjectRef` keeps only the header's pointer, so the control
  enters a Process into a handle under the Thread tag. It looks up as a Process, and on `main` as
  a Thread.
- **`Ref::try_acquire` is added**, for the caches that keep a `T`'s address weakly: a
  registration's files, the console's node, an endpoint's registration.

### J.3 A handle holds its reference (kobj-o2 with kplat-o7; kobj-o9; kplat-d16)

Measured: **21 `allocate` calls**, each an `into_raw` with a `from_raw` on its error arm; `close`
and the exit sweep hand out a `Copy` `ClosedObject` adopted by hand; **28 hand-written rollbacks** of
handles installed partway through a syscall (`table.rs` 14, `spawn.rs` 11, `submit.rs` 2, `send.rs`
1).

- **`install(pid, ObjectRef, rights)`**, giving the reference back with the error; `allocate`
  private.
- **`close` answers a `Closed(ObjectRef)`**, `#[must_use]` and not `Copy`, dropped by its caller
  outside the rank-3 lock as now; the sweep fills `[Option<Closed>]`.
- **`Installed<N>`**: the handles a syscall has installed so far, closed on drop unless committed.
  Spawn, channel creation, receive, the PO paths and `run_first_userspace`, whose twelve early
  returns leak three handles today (kplat-d16).

Controls: a failed install hands back its reference, the object's destroy count 0 until it drops;
and each syscall's installs fail in turn, with the table's handle count after each as it was
before. `FAIL_NEXT_ALLOCATE` is a one-shot, which fails only a syscall's first install, and every
rollback `Installed<N>` replaces is past the first, so it becomes a countdown failing from the
*n*th on, as `TABLES_LEFT` is. kplat-d16 is boot-only, so its early returns are held by the
guard's own test. Author.

**Built 2026-11-06.** No caller of the handle table holds a raw pointer: `install` answers the
reference back with a refusal, `allocate` is the table's own and `unsafe`, and `close` and the exit
sweep answer a `Closed`. Every syscall that installs more than one handle, or installs one before a
step that can fail, holds them in an `Installed<N>`. That covers spawn's two tables, channel
creation, receive, the PO paths, a thread's start, and pid 1's three at boot. None of the 28
hand-written rollbacks is left. The control drives each such syscall on the host with its installs
refused in turn, and counts this thread's handles in and out, since the global table is shared by
every test. Spawn is driven up to the parent's handle and not past it: a started child needs a kernel
stack the host has not. `unsafe` blocks fall from 954 to 931; `allocate` is one more `unsafe fn`, and
`ClosedObject`'s two `unsafe impl`s are gone.

### J.4 The scheduler's own plumbing (sched-o6's rest, sched-o9, sched-o10, sched-o11)

Four repetitions, each one function after; before J.5 to J.7, which would otherwise touch five
copies.

- **One `wake_all`** for the five wake loops.
- **A guard that defers drops**: a site declares a `Deferred<N>`, sized to its burst as `Orphans<N>`
  is today (48 at an endpoint's close), and locks SCHED through it. `defer(ObjectRef)` puts a
  reference in the bag, which drops after the guard, by declaration order, once SCHED is released;
  in interrupt context, where nothing may be freed, its contents move to `deferred_drops` instead,
  whose reserve covers the largest bag a site there declares. A full bag is a sizing bug and ends
  the machine, as `push_reserved` does. **`switch_away` takes only a plain guard**, so no bag
  crosses a switch, and the terminal exit path keeps `deferred_drops`. The eleven hand-carried
  releases go, and no acquisition, the tick's among them, carries a bag it does not use.
  **`ObjectRef::drop` asserts no SCHED in every debug build**, where it asserts only in host tests
  now, and the kernel images are debug builds.
- **One `switch_away(guard, prev, Park)`** for the five switch tails, `Park` naming where the
  outgoing thread goes; `switch_into` takes the slot from `prev` itself, where a caller passes it.
- **A deadline entry names its target by kind**, an enum, where it carries bare `usize`s.

Controls: the drop assertion fails a host test that drops under SCHED; a bag deferring one past
its size ends its test; the parking is split out for host tests, one per `Park`, as
`clear_online_bit` was. Author.

**Built 2026-11-06.** Each repetition is one function:
- **`wake_all`** takes a `Waiters<N>`, which only a `WaitQueue` makes, and holds the five wake
  loops' one `unsafe` block. A wait's signalled slots come back by value, where a caller-sized
  buffer was checked by a `debug_assert!` alone.
- **A site that lets go of references under SCHED declares a `Deferred<N>`** and locks through
  it. The guard borrows the bag, so the compiler keeps the bag declared first and dropped last.
  - None of the eleven hand-carried releases is left.
  - The one place a refused value is forgotten is `reserve_exhausted`. `push_reserved`, a wake's
    `place_woken`, the bag and the held-send queue all end through it.
  - `ObjectRef::drop` asserts no SCHED on every drop in every debug build. A boot with one clone
    dropped under SCHED fails `test-qemu`.
- **`switch_away(guard, Outgoing, Park)`** replaces the five tails. Only taking this CPU's
  `current` makes an `Outgoing`, `park` is host-tested once per `Park`, and one `unsafe` switch
  call is left.
- **A deadline's target is an enum** carrying each kind's addresses.

`unsafe` blocks fall from 931 to 915. Three things differ from the plan:
- **A bag for interrupt context is declared so by its site**, `Deferred::in_interrupt`, no larger,
  at compile time, than `deferred_drops`' reserve, whose room for every such bag at once rests on
  the one site's lifetime total: nothing tells interrupt context at run time. The
  tick's entropy wake, the one such site, takes its own hold, so the hold it switches with carries
  no bag.
- **A target's addresses are `usize`s**, as `idle_addr` is, which keeps the scheduler's state
  `Send` without an `unsafe impl`.
- **A held send's deadline is removed by its whole target, the endpoint too.** Nothing tested
  that removal before; a test now drives a delivery and a close, and naming the other endpoint
  fails it.

### J.5 Typed scheduler calls (kobj-o7, with sched-o2 and drv-o8; sched-o8)

Measured: **31 `unsafe fn` entry points** in `sched/` take a raw object, a `*mut ()` or, for the two
waits, a list of addresses, called 93 times from outside it, **76 of them passing the `.as_ptr()`
of a reference the caller holds**.

- **The entry points take `&Ref<T>`**, or a `&T` borrowed from one, and the borrow is the pin;
  `wait_on` takes the references, and the hub's sleep, written `wait_on(&[])`, calls `sleep_until`.
  **`park_briefly`'s `wait_on_io(&[])` stays an I/O wait**: `sleep_until` heeds an exit, and a
  thread ending while it faults on a page in flight must still wait for the fill, where it would
  return at once on every pass and spin. The three drop hooks stay raw, and so do the twelve
  callers holding only a pointer an IRP or a driver pins, until J.14 and J.15 give those a type.
- **A borrow for a read under SCHED** (sched-o8's rest). Part I took its clones; what is left is a
  raw child pointer, read and argued alive by its owner, at three channel-pointer reads and four
  `endpoint_ptr` reads. `Process::notification_channel`, `Process::parent_notif` and
  `UserspaceServerReg::endpoint` borrow what the owner holds. **`Thread::process`, for the six
  `process_of` reads, waits for J.7**, as it borrows a `&Thread`.

Depends on J.2. Control: an opportunity, so the tests that reach these reads, under Miri before and
after. Host tests reach `deliver_child_exited` and `us_forward_originate`; J.5 names the test that
reaches each of the seven reads, and writes one where none does. Author; the call sites to the
implementer.

**Built 2026-11-06.** No scheduler entry point takes a raw object but the three drop hooks and
`complete_pending_op_at`. Each takes its object borrowed, the borrow its pin, and is safe:
- a `&T` for the thirteen `Shared` kinds, a `&Ref<Thread>` for a thread;
- `&[&ObjectRef]` for the waits, dispatched by header as before;
- the hub's sleep is `sleep_until`, and `park_briefly` stays an I/O wait.

Three of the six callers holding only a pointer an IRP or a driver pins form the `&T` in one
`unsafe` each. xHCI's three complete by address, through `complete_pending_op_at`: the hub thread
that pins each PO lets go once it sees the completion, before a borrow held for the call would end.
Every untyped holder that only ever held one kind is a `Ref<T>`: every PO a driver, a file, the
power path and the syscalls keep, a pending lookup's, a fill's, a held send's and an entropy seed
waiter's among them, a started user thread, the `InterruptObject`s AHCI and xHCI wake on, and a char
device's read and write backends' `po`. `unsafe` blocks fall from 915 to 829 and `unsafe fn`s from
328 to 299.

The reads under SCHED borrow: `Process::notification_channel` and `parent_notif`, now
`Ref<NotificationChannel>`s, and `UserspaceServerReg::endpoint`, a field set once outside the
SCHED-guarded cell. The tests reaching the seven reads:
- `deliver_child_exited`'s: `the_last_thread_out_of_a_process_wakes_the_reaper_with_the_status_its_exit_recorded`;
- `suspend_with_fault`'s: the harness's fault demo, in every `test-qemu`;
- `request_terminate`'s: `a_terminate_request_reaches_the_channel_its_process_holds`, new;
- the four endpoint reads: `a_forward_its_server_cannot_take_lets_go_of_its_slot` and
  `a_registration_dropped_fails_its_fills`.

The host ones pass under Miri on `main` and after, among `sched::` and the three objects' tests, 112
and 115. One thing differs from the plan: **an entry point takes a `&T`, not a `&Ref<T>`**, as no
PO was held typed and a caller with only a pinned pointer can form a `&T` but not a `Ref`. The
objects' accessors reach their state through an `UnsafeCell`, so a pointer from a `&T` is sound.

### J.6 What SCHED guards, as a type (kobj-o3 with sched-o1; kobj-o10 with sched-o7; sched-d11)

- **A `SchedToken`**, zero-sized, a field of `SchedState`, so only the guard reaches one, and
  **`SchedCell<T>`**, whose `get` takes `&mut SchedToken`: one interior borrowed at a time, and never
  without SCHED. The six objects SCHED mutates (`Timer`, `PendingOperation`, `NotificationChannel`,
  `InterruptObject`, `IpcChannel`, `UserspaceServerReg`) keep that state in one cell each: their 60
  `unsafe fn`s taking the object as a `*mut ()`, an `inner` each among them, become the cell and
  safe methods.
- **A back-pointer set and read with a witness** (kobj-o10): `IpcChannel.peer` and `.us_reg` with the
  token, the file cache's with its guard, so a stale one cannot be followed. `is_live`, which reads
  memory that may be freed, goes.
- **sched-d11**: `dispatch_destroy` runs the type's closing hook on the raw pointer before it
  rebuilds the box, so no `&mut self` is live while another CPU writes the object under SCHED.

Control: sched-d11's, the survey's host test, two threads under Miri, which reports the retag today.
Depends on J.2 and J.4. Author.

**Built 2026-11-10.** The six objects `SCHED` mutates keep that state in a `SchedCell`, borrowed
only with the `SchedToken` the scheduler's state keeps, and their raw accessors, an `inner` each
among them, are safe methods; their twelve `Send`/`Sync` claims go, a `WaitQueue`'s `Send` argued
once instead. `wait_on`'s dispatchers borrow each object by kind.
- **Back-pointers are `Weak<T>`** (`object/header.rs`): an endpoint's `peer` and `us_reg`, and the
  file cache's entries, each made under the contract that its target's closing clears it under
  the lock it is kept behind. `sched::follow` reads one out of a cell and lends the token back with
  the target, so endpoint, peer and registration chain without `unsafe`. `is_live` goes.
- **sched-d11**: `dispatch_destroy` runs a type's `KObject::closing`, which `kobject!` sets, on the
  raw pointer before it rebuilds the box: an endpoint's, a timer's and a registration's, and a
  `FileObject`'s cache removal, which had the same shape. Its two-thread test fails under both Miri
  models on `main` and passes here. A type a box of which could be linked is made as a `Ref`, as a
  box's drop would skip its closing; a hook takes a `Closing` only `dispatch_destroy` makes; and
  each type's `Drop` asserts its closing ran.

What stays raw: the deadline heap's targets, which the tick forms into a `&Timer`, a `&IpcChannel`
and a `&PendingOperation` under the closing's argument, and `complete_pending_op_at`. `unsafe`
blocks fall from 829 to 685, `unsafe fn`s from 299 to 233 (`object`'s 106 to 50, `sched`'s 17 to 7)
and `unsafe impl`s from 65 to 57, `SchedCell`'s `Sync` the one new. Under Miri, `sched::` and the
six objects', `process`'s and `header`'s tests pass on `main` (148) and here (149), measured
before a registration's outstanding requests came out as their references alone, a change in safe
code: taken whole, the tables overflowed the debug kernel's stack in a closing.

One thing differs from the survey: **`follow` lends the token back with the target**, where it had
a `SchedWeak::get` borrowing the token: a followed target's own cell is borrowed with the token,
which a target borrowing it would forbid.

### J.7 The thread, split (kobj-o3's `Thread` half; sched-o8's last)

`Thread`'s **41 accessors**, called 112 times in `sched/`. Its identity stays plain; what SCHED
guards moves into a `SchedCell<ThreadSched>`; `on_cpu` and the wait phase stay atomics; and `arch`
and the FPU area, which `context_switch` writes without SCHED, get a cell and a narrow raw API of
their own. Then `&Thread` is sound and `Ref<Thread>` derefs, and `Thread::process` borrows the
process for the six `process_of` reads, sched-o8's last. Control: kobj-d11's Miri test, and
`sched`'s tests under Miri. Author.

**Built 2026-11-10.** A `Thread`'s fields are split by who writes them:
- its identity is fixed when it is made;
- what `SCHED` guards is a `SchedCell<ThreadSched>`;
- its wait phase, `on_cpu` guard and ended mark are atomics;
- its parked context, the saved stack pointer and the FP/SIMD image, is reached only by address.

`Thread` is `Shared`, so `Ref<Thread>` derefs, and its 43 raw accessors are safe methods, those for
`SCHED`'s state taking the token. The scheduler's lists hold `Ref<Thread>`, and
`Thread::process` borrows the process for the six `process_of` reads.
- **The parked context is read only with its guard clear.** `Thread::parked_sp` is `None` while
  `on_cpu` is raised, and only the switch's asm clears the guard, so a read under `SCHED` never
  meets the switch's write.
- **A switch is prepared under the hold** (`Switch::prepare`): what it needs of both threads is
  read there, the incoming thread's stack top among it, sched-d16's read. Its arming stays after the
  release, for J.11.
- **The ended mark is an atomic**, as the reaper reads it with no lock held.

What stays raw: a waiter's address. `wake_all` and the tick's thread deadline follow it, as a
scheduler list pins a waiting thread until it unregisters.

Counts:
- `unsafe` blocks fall from 685 to 571 and `unsafe fn`s from 233 to 188 (`object`'s 50 to 9,
  `sched`'s 7 to 3);
- `unsafe impl`s rise from 57 to 59: `Thread`'s `Shared` claim, and its parked context's `Sync`.

**The control fails on `main` under both Miri models and passes here.** One thread reads a thread
through a `&Thread`, another sets its affinity under `SCHED`, and a third writes its parked context
as the switch does. With the control, `sched::` and the objects' tests pass under Miri: 162 on
`main` and 163 here.

### J.8 A frame's bytes, through a view (kobj-o11 with kmem-o5; kobj-d13 with kmem-d18; drv-o14; kplat-o6's view)

Measured: **30 `hhdm_offset()` sites** outside tests. Nine reach an object's frames, four are driver
reads of a buffer, eight zero a fresh frame, four reach borrowed memory (ACPI, the framebuffer, a RAM
disk's fragments), and the rest are the page tables', the slab's and the definition's. **Two still
form a `&[u8]` over frames a user mapping may write** (`copy_to_kvec`, `read_to_kvec`): kobj-d13,
reached by a spawn of an image its parent has mapped writable.

- **Miri sees frames first**: under Miri the buddy hands out a host allocation per block, as
  kmalloc's route does, so a frame used after its free, freed twice or overrun is reported. The
  buddy's own tests keep its coalescing, on their local allocator.
- **`mm::frame`**: `read`, `write`, `fill` and `copy` within one frame, each checking `at + len <=
  PAGE_SIZE`, reached through `MemoryObject` and `FileObject` methods on `&self`, with
  `MemoryObject::copy_out` the twin of `copy_in`. **A frame another party may write is copied in
  relaxed atomic words**, as `TableRef` reads a table: plain moves on x86_64, and never a reference.
- **The four driver reads and both `copy_into_memobj`s** (drv-o14) use `copy_out` and `copy_in` on
  the `ObjectRef` each caller holds.
- **A fill's copy finds its destination under the file's lock**, by index and PO as `settle` does,
  where it copies into a bare address held across the wait. **The loader writes through
  `AddressSpace::copy_in`**, under the `&mut` it holds.
- **Borrowed memory keeps one `unsafe fn hhdm`**: ACPI's tables, the page tables, a RAM disk's
  fragments.

**A view borrows its owner, which holds because a Ready cache page is freed only with its object**:
`resize` retires one, and `settle` frees only a page whose fill failed. E.8's `IO_NOCACHE` is the
first to free one early, and its count of operations holding a file's frames is what keeps the
borrow sound; that is E.8's.

Controls: a writer thread storing into an object's frame while `copy_to_kvec` reads it, under Miri,
a race reported against the slice and none against the copy, the writer storing at the copy's
width; neighbour tests at `PAGE_SIZE - 1`, `PAGE_SIZE` and one past, and across a frame.
`file_object.rs` and `acpi.rs` are at their size entries, so their tests move out first. Author.

**Built 2026-11-10.**
- **Miri sees frames** (`392cc4c7`): under Miri each buddy block is a host allocation of its own. A
  frame read after its free, a probe, is reported, and passes with the route switched off.
- **`mm::frame`**: `FrameView`, with `read`, `write`, `fill` and `copy_from`, each stopping past
  the frame's end, every access an aligned 8-byte atomic, a write of part of a word merged by
  compare-and-swap; `alloc_zeroed` for the eight sites that
  zeroed a fresh frame by hand; and `hhdm` for borrowed memory. Owners lend views:
  `MemoryObject::page`, under `copy_in` and its twin `copy_out`; a `FileObject`'s ready page; and
  `AddressSpace::copy_in`, which the loader writes through.
- **A fill's reply copies under the file's lock**, into the page its fill still is, found by
  index and PO; `PendingFill` loses its frame. `mark_ready` passes over a page a fault's fill is
  filling, so no ready page has a fill and none is freed before its object.
- **`hhdm` is a safe function**, not the `unsafe fn` drawn here: it only computes an address,
  and each borrowed reach is its caller's `unsafe`.
- `acpi.rs` shrank a line, so its tests stay; `file_object.rs`'s moved, in two files.

Counts: `unsafe` blocks fall from 571 to 559; `unsafe fn`s rise from 188 to 189, `FrameView::new`.

**The controls fail on `main` under both Miri models and pass here, over 16 seeds**: a writer storing
into a frame in atomic words while `copy_to_kvec` or `read_to_kvec` copies it. A raw
`copy_nonoverlapping` in the view's place races too. Under Miri, the tests of `mm`, the memory
and file objects, the registration, `syscall::table`, `sched::ipc`, `drivers::input`, `io`, and
the paging and ACPI modules pass with frames on the host's allocator: 396, two ignored as before.

**After review** (`811c7fa4`):
- **The RAM disk reaches an I/O request's frames through views.** Its copies are the CPU's, which
  the views' owners lend; a raw copy beside `copy_in` raced under both Miri models.
- **Every access is an aligned word.** Two sizes of atomic over one word race: a byte copied in at
  an odd offset against `copy_to_kvec`'s word loads, under both models.
- Tests pin `ready_page`'s and `mark_ready`'s guards, `read_to_kvec`'s refusal of a page truncated
  after its fault, and `AddressSpace::copy_in`'s refusals. The RSDP is reached at `hhdm`.

### J.9 An owned frame (kmem-o2, kmem-d11; kplat-o6's owned half)

`heap::buddy_free`, `BuddyAllocator::free` and the pager's `free` are safe functions that hand a
frame out again, and a frame is a `Copy` address: **10 allocations and 18 frees by hand**, eight
zeroing blocks, the owned-or-borrowed flag kept by hand, and three hand-written rollbacks.

- **`Frame`**, `!Copy`, freed by its drop with its own order, and **`FrameBlock`** for order *n*;
  `alloc_zeroed` the one zeroing `unsafe`. The frees become `unsafe` and crate-private; the slab,
  which rebuilds an address from a pointer, keeps that one.
- **`MemoryObject` is `Owned(KVec<Frame>)` or `Borrowed { base, npages }`**, a borrowed range being
  contiguous, so the framebuffer's list of 8,000 addresses goes. A cache page's frame and the retired
  list are `Frame`s, so a failed fill's `settle` is a drop. `VmapFrames` holds `[Frame; N]`, dropped
  after its shootdown, the order it needs; `DmaBuffer` holds a `FrameBlock`; a page table that loses
  `ensure_table`'s race is dropped.
- **Pages the cache lends stay addresses**: `Fault::Hit`, a fill's frame list and `build_frags`
  borrow, and `Frame` is `#[repr(transparent)]`, so a `&[Frame]` serves them.
- **A frame's drop takes the buddy's lock**, rank 6, so the rule against freeing under SCHED or in
  an interrupt now binds a drop where it bound a call. Every free today is in thread context, or the
  `#PF` path's documented exception; `lock-ordering.md` says so.
- **Not closed here: a frame freed while a device still writes it.** That is P5's.

Controls: each owner's frees counted (an order-2 block frees four); each rollback failing at every
index, through a countdown in `Frame`'s allocation, which every owner's frames pass, beside
`TABLES_LEFT`: `FRAMES_LEFT` reaches only `kvmap`'s frames and `TABLES_LEFT` only an index that
needs a new table, so a three-page VMA inside one table fails at its second and third pages as
well; and the survey's probe, a live frame freed and read, reported by J.8's Miri route. Author.

**Built 2026-11-11** (`9b98803f`).
- **`Frame` and `FrameBlock`** (`mm::frame`), each freed as it drops, at its order, and neither
  `Copy`. The buddy's frees, `heap::buddy_free`, `BuddyAllocator::free` and `BuddyPager::free`,
  are `unsafe`, the first `mm`'s alone; the slab's large free keeps its call, and is `unsafe` too.
- **Every owner holds its frames typed**: a `MemoryObject` is `Owned(KVec<Frame>)` or
  `Borrowed { base, npages }`; a cache page and a retired frame are `Frame`s; `VmapFrames` holds
  `[Frame; N]`, freed as its fields drop after its shootdown; a `DmaBuffer` a `FrameBlock`; an
  `AddressSpace` its root. The three rollbacks by hand went, with `MemoryObject`'s, `FileObject`'s
  and `DmaBuffer`'s `Drop`.
- **A mapped frame is its PTE's**: `into_pte` hands it over, and an unmap takes an anonymous
  page's back by the `unsafe` `from_pte`, at the three sites drawn here for J.10: with the frees
  `unsafe`, they could not wait. J.10 moves them into `UserTables`.
- **Addresses the cache lends stay `PhysAddr`s**, and `Frame::addrs` lends an object's list to
  `build_frags`. `lock-ordering.md` binds a frame's drop by the rule against freeing under `SCHED`
  or in an interrupt.

Counts: `unsafe` blocks rise from 558 to 567, and `unsafe fn`s from 189 to 195, the frees' danger
made visible: the four frees, the slab's large free and `from_pte` are `unsafe fn`s; the calls
through them, the two drops that free, `from_pte`'s three sites, the cast that lends addresses and
`Frame::view` are blocks, less the two views an object and a cache page had formed by hand.

**The controls**: each owner's frees counted, an order-2 block's four. A countdown in the frame
allocation, `fail_frames_after`, which every owner's frames pass, fails each rollback at every
take: a three-page map in a fresh space at each of its six, an object and a vmap range at each
frame. Eleven mutations, each freeing nothing, at the wrong order or before a shootdown, fail the
test that names them. Safe code freeing, copying or taking back a frame, or keeping a view past it,
does not compile. The survey's probe, a frame freed while its owner still reads it, now needs an
`unsafe` `from_pte`; it passes natively and Miri reports it. Under Miri, both models, the tests of
`mm`, the memory object, the cache's frees, the paging race, the block layer and the RAM disk pass.

**After review** (`ac2ed261`):
- **An address space's root and a `VmapFrames`' frames are handed over by `into_pte`**, as a mapped
  frame is, and taken back by `from_pte` as their owner drops: held as `Frame`s, a safe `view`
  reached a live page table and a kernel stack. Viewing either no longer compiles; blocks: 569.
- **`BuddyPager` and `HeapBuddy` are `mm`'s alone**: a free by hand from `object/` compiled.
- **`fault_in`'s refusal is driven at each of its four takes, and a read ahead's frames are counted
  taken and freed** (`frames_taken_by_this_thread`): a leak at either passed every test.
- A retired frame, and a reference the file cache lists, are pushed only into the room reserved
  for them, never dropped by a failed push: a reserve drifting from its loop fails tests now.
- `lock-ordering.md` and `memory-management.md` lose the `Drop`s J.9 removed, and the former lists
  every frame the `#PF` path drops.

### J.10 Typed page tables (kmem-o6, kplat-o1's rest, kmem-o10, kmem-o7's first step)

**Most of kplat-o1 is Part I's**: slots read and written atomically through a `TableRef`, one walk,
an index bounds-checked. What is left is the root: **eight of the paging trait's `unsafe fn`s take a
bare `root: PhysAddr`**, called at 23 sites, and the root is kept raw in six places, the boot root
twice. `AddressSpace` repeats one `unsafe` map or unmap, with one argument, at eight sites.

- **Two table types over one arch root.** The kernel half is mapped from many CPUs without a lock,
  so its tables take `&self`; a user half is its address space's alone, `&mut self`. **`KernelTables`**
  is made once at boot, before `kvmap::init`, and replaces both copies of the boot root;
  **`UserTables`** is owned by `AddressSpace`'s locked state, dropped after its VMAs are torn down,
  the order `AddressSpace::drop` keeps by hand today.
- **`translate` and `attribute_index` are safe on either**, and `AddressSpace::translate` takes
  about 45 test sites out of `unsafe`.
- **`UserTables::map_owned(va, Frame)`**, safe, giving the frame back with the error, for `map_vma`
  and `fault_in`, whose failure arms each free the frame by hand today; the type frees it by
  construction. Mapping an object's frame, kept alive by the VMA's reference, keeps a narrow
  `unsafe` for that half.
- **An unmap gives back an anonymous mapping's frames as `Frame`s, by the VMA's kind**, through an
  `unsafe from_pte` at its three sites: the VMA is the one record of what owns a page, and a second
  in the PTE could disagree with it.
- **Loading a root stays a narrow `unsafe`**, as the thread's cached root outlives any borrow. **A
  walk of the active root stays `unsafe`**, at its five diagnostic sites: making it safe would
  promise that intermediate tables are never freed.
- **`subtree_max_end` goes** (kmem-o7's first step): nothing reads it, and `memory-management.md`
  still says it is kept for future queries.

`x86_64/paging.rs`'s tests move out first. Depends on J.9. Control: an opportunity, so none of its
own; the 20 paging tests, the Miri walk among them, and the 39 address-space tests, the unmap's
shootdown ordering among them. Author.

**Built 2026-11-11** (`bb331b92`; the paging tests moved first, `94ec28f8`, and `subtree_max_end`
went, `abdf3772`, with two blocks and an `unsafe fn`).
- **`mm::page_tables`: `KernelTables` and `UserTables`.** The kernel's is the boot root, adopted
  once; it replaces kvmap's and the scheduler's copies. Its `map` and `unmap` stay `unsafe`,
  through `&self`. An address space holds a `UserTables` in its locked state, freed as it drops,
  after the VMAs.
- **`adopt` links the vmap window's top-level tables and captures the kernel half in one call**,
  from `kvmap::window`, so the template always holds the window: `kvmap::init` went, and with it
  the order `main.rs` kept by hand. The plan had kvmap link them after the tables were made.
- **`map_owned(va, Frame)` is safe**, handing the frame back with the error; `map`, for an
  object's or a file's frame, stays `unsafe`; `unmap` is safe and gives back the address, an
  anonymous page's frame taken back by `from_pte`, by the VMA's kind, at the three sites.
- **For the maintainer's eye: a user half refuses a kernel-half address, a global mapping, and an
  owned frame mapped other than write-back** (`MapError` and `UnmapError::NotUser`). Without
  them the safe calls would let any address space's tables unmap a kernel stack from every root,
  map a frame that outlives the space's time loaded, or alias RAM the direct map holds
  write-back with another type, which the processor leaves undefined.
- **`translate` and `attribute_index` are safe on either**: `AddressSpace::translate` takes 24
  test walks and 2 self-test walks out of `unsafe` (the plan's 45 counted calls behind helpers).
  The five walks of the active root stay `unsafe`, as does loading a root, whose contract now
  says it stays live while loaded.

Counts: `unsafe` blocks fall from 569 to 566 and `unsafe fn`s rise from 195 to 197. `page_tables`'
11 blocks and four `unsafe fn`s (`adopt`, the kernel tables' `map` and `unmap`, the user tables'
`map` of a frame its owner keeps) hold the root that `addr_space`'s nine replaced blocks and
`kvmap::init` took bare; `mm`'s entries are raised by hand in the code commit.

**The controls**: an opportunity's, the tests that held the code, passing before and after: the
20 paging tests, the Miri walk among them, and the 41 address-space tests, the unmap's shootdown
ordering among them. The refusals, which are new, have their own: eight mutations, each failing
the test that names it. They unmap or map a kernel page through a user half, allow a global or an
uncached owned mapping, refuse a borrowed frame write-combining, never free the root, free an owned
frame while mapped, or lose it on a refusal. Forging tables from an address, or mapping through
either without `unsafe`, does not compile (E0451, E0133). Under Miri, both models, the tests of
paging, the tables, the address space, kvmap, kstack and the VMA tree: 110 pass.

**After review** (`45019557`):
- **`map_page` links no kernel-half top-level table** (`MapError::Unshared`): `KernelTables::map`
  past the vmap's window had linked one into the boot root alone, after the template's capture.
- **A user half refuses from `USER_MAP_END`, not `USER_VIRT_END`**: the top user page, which a
  `sysretq` faults on in ring 0, had mapped through the safe call.
- **A VMA keeps no caching**: anonymous and file pages map write-back, an object's mapping its
  object's, so a write-combining anonymous VMA, which had panicked naming `Misaligned`, cannot be
  written. The arms that cannot be reached name the error they got.
- `KernelTables` walks too, as the doc said; `adopt` refuses a second call; the refusals of `map`
  and `map_owned`'s hand-back on an arch error are tested. Blocks: 568.

### J.11 Per-CPU state (kplat-o3 with sched-o3; kplat-d23, sched-d16)

No dense CPU index type, no per-CPU container and no pin exist: the index is a `u32`, a `usize` or a
`u8` by where it is read; **five `static mut` arrays** are reached through unchecked `.add(idx)`;
and the nets are debug assertions of IF=0. Of 36 `this_cpu()` reads, **23 are under the SCHED
guard**, four under a local mask, eight rely on IF=0 by context with no guard, and one is unpinned
by design.

- **`CpuIndex`**, made only as `BSP`, from `adopt_dense_index`, or as `current(pin)`. The boot and an
  AP's bring-up pass theirs down to `gdt::init` and `init_syscall_entry`, before either can be
  scheduled. **`PerCpu<T>`** in `arch`, for the five arrays: a raw slot by `CpuIndex`, and a safe `get`
  for `T: Sync` alone, as dropping an outer lock early can unmask under a live inner pin. **No
  release check in `current()`**: the bound is held where `TSC_AUX` is written, and the array is
  bounds-checked.
- **A pin**: borrowed from an `IrqSpinLockGuard`, or from `IrqsOff` and `PreemptOff` guards, which
  `preempt_disable` and the interrupt mask return. **A site with no guard checks IF itself**,
  `IrqsOff::observed()`, in every build: a `pushfq` on each return to ring 3, small beside a
  syscall, and no `unsafe` and no argument to keep. Grace's read side holds a `PreemptOff`.
  **A debug count of live pins is asserted zero at `switch_into`**, as masked interrupts do not pin
  a thread across a switch.
- **`cpu_local!`** for the per-CPU state host tests run, `PREEMPT_OFF`, `RESCHED_PENDING`, `ENDING`
  and lockrank's three arrays, its twin a thread-local; `remote` for `end_siblings`' store takes the
  SCHED state. The 23 reads already under SCHED become `SchedState::me()`, to the implementer.
- **kplat-d23**: the BSP's `init_syscall_entry` moves beside `init_tables`, with `CpuIndex::BSP`.
  **sched-d16**: `arm_kernel_stack_for` moves above the release, borrowing the guard as its pin.

Controls: `init_syscall_entry` asserting its pin fires on every debug boot with the old placement,
where IF=1 and no race is needed; `current(pin)` checked both ways on the host through
`irq_backend::force` and `PreemptRaised`; `cpu_local!`'s twin, two threads each holding a lock and
seeing a depth of one, which a process-wide backing fails. `scheduler.md` and `smp.md` say how the
index is read. Author.

### J.12 DMA memory, bounded and owned (drv-o3 with kmem-o9; drv-o13)

**15 `.virt()` sites** in the drivers behind **21 `unsafe` blocks**, three of them addresses stored
as `u64` in tables a DPC reads; `as_slice` and `as_mut_slice`, references over memory a device
writes; and AHCI's PRDT count held by `submit` alone.

- **Bounded volatile accessors on `DmaBuffer`**, over J.8's frame view: `read::<T: Pod>(off)`,
  `write`, `copy_in`, `copy_out` and `fill`, each checking `off + size_of::<T>() <= len()` and
  alignment, as `read_volatile` needs. `as_slice`, `as_mut_slice` and `Programmed::as_mut_slice` go;
  `virt()` is crate-private, with an address accessor for the boot self-test, another crate.
- **The storage `Dev` and HID `Bound` own their buffers** (drv-o13), so each use makes its `RingMem`
  from the buffer, and `RingMem::kept`, the `unsafe` that made a `'static` one, goes with the three
  stored addresses. A departure hands them back: `Released::Freed` carries the `Quiesced` that frees
  them, and a kept slot leaks them, as now.

Controls: a `write::<u32>` at `len() - 3` refused where `len() - 4` passes, and an unaligned read
refused; `build_command` with a fragment past the PRDT's room stops, where it writes past the page
today; a departure whose slot was kept frees none of a device's buffers and one that disabled
frees all, counted. `hub.rs` is 985 lines, so a move comes first. Author.

### J.13 Registers in a window, and vectors given back (drv-o7, kplat-o12)

**138 register accesses** through safe functions that dereference any base and offset, and
**offsets a device supplies used unchecked**: xHCI's operational, runtime and doorbell bases, never
held to BAR0's size; PORTSC from MaxPorts; an Enable Slot's slot never held to `max_slots`; AHCI's
port bases, its mapping's length not kept.

- **`kvmap::map_mmio` returns an `Mmio { base, len }`**, whose `read32` and `write32` check alignment
  and `off + 4 <= len`, and whose `sub(off, len)` checks xHCI's three windows and AHCI's ports once,
  at bring-up, in a pure `windows(..)` a host test reaches. PCI's `MmioCfg` is a 4 KiB `Mmio`; its
  `Cfg` callers are unchanged.
- **The vector pool answers `Err(PoolFull)`**, where it asserts, and a `DeviceVector` guard gives a
  vector back unless committed. **Only a vector nothing was programmed with or routed to comes
  back**: a declined xHCI whose MSI is enabled may still signal, and an AHCI that declines at its
  publish has had its GSI routed, unmasked, to its vector by `install_intx`. Each keeps its vector,
  and the guard masks a routed entry first.

Controls: `sub` at the BAR's end allowed and a byte past refused; a fake whose RTSOFF points past
BAR0 declined, where Miri reports the write past the fake today; a full pool answering `Err`, the
pool a value for it, as the global counter is shared by parallel tests; a guard dropped after a
routed install leaves the entry masked and the vector taken, on the IOAPIC's register fake.
`pci/mod.rs` is at its size entry, so `MmioCfg` may not grow it. Author.

### J.14 An I/O request in flight (drv-o12, drv-o1)

**38 `*mut Irp`s**, 24 dereferenced raw, through **12 safe functions** taking one; three handlings
of an empty fragment list; and `BlockBackend`'s public `ctx: *mut ()`, re-cast at every submit and
poll, or an integer carried through it.

- **`BlockBackend::new::<C>(ctx)`**, its fields private: the constructor types the context and pairs
  it with `C`'s `submit` and `poll`, so no code can pair one backend's context with another's
  functions. A closed enum would make `io` name its drivers and shut out a Tier 2 one.
- **`InFlight`**, minted by `io::block`'s dispatch alone: `frags()`, built once and checked to sum to
  the length; `rebase` for a partition; `complete(self, ..)`; and `keep(self)`, an explicit leak for a
  command a stuck device may still move, which AHCI's give-up and USB storage's kept slot do
  silently today. **One dropped unfinished fails a debug build**, a lost IRP being drv-d3's class.
  USB storage's `Owner`, `Command`, `IrpCmd`, `Action` and `BeginRescan` stop being `Copy`.

Controls: a dropped token panics; a fragment sum short of the length is refused at the mint, while
`a_transfer_moves_its_length_whatever_its_fragments_hold` keeps driving the RAM disk's own check
with a raw slice, so the new guard does not hide the old; a port that will not stop keeps its
command by `keep()`. Depends on J.2, which rewrites the same dispatch lines. Author.

### J.15 The hub's waits, and the DPC's hand-offs (drv-o15, drv-o6)

- **`Armed` and `Completer`** (drv-o15): the hub thread arms a record with a PO it holds and waits
  through it, taking the record back on timeout or waiting out the completion already on its way;
  the DPC completes through a `Completer` it must consume. **Arming refuses while a record is
  armed**, where a second hub command overwrites the first today. The raw PO stays inside the pair.
- **One `Handoff<T>`** (drv-o6) for what a DPC must not drop: `park`, which refuses where `owe`
  overwrites today; a `Loan` that cannot be dropped; `give_back`; and `take_all`, in thread
  context, registered once with `reap_pending`. **The console moves onto `input::Reader`**, deleting
  its copy. io's `RECLAIM` list stays as it is: it must be unbounded, and a fixed park would drop in
  place when full.

Controls: a second hub command refused; a second `park` refused with the first kept, counted by a
`Drop`; a dropped `Loan` panics. After J.14. Author.

### What Part J does not do

- **Replace the VMA tree**: kmem-o7's second step is later, a balanced tree the maintainer's
  preference when it is revisited.
- **`CharBackend`'s `ctx: *mut ()`**, the same pattern as the block backend's in a part no item
  names; nor lockrank asserting that locks are released in order.
- **A safe walk of the active root**, and a PTE bit saying a frame is owned (J.10).
- **The survey's ten items marked later**, and the four bash findings Part E takes.
