//! Nitrox kernel entry point.
//!
//! Boot path:
//!   1. UEFI firmware loads Limine from the ESP.
//!   2. Limine parses our ELF, locates our request statics (the
//!      `.limine_requests` bracket below), sets up long mode + paging +
//!      a framebuffer, and jumps to [`_start`].
//!   3. We verify the bootloader honoured base revision 6, bring up the
//!      serial console, install the kernel's GDT/TSS/IDT, bring up the
//!      buddy and slab allocators from Limine's memory map and HHDM,
//!      then render the boot screen.
//!
//! After `kernel_main` returns, [`_start`] enters [`arch::Cpu::halt_loop`]
//! forever. The kernel does no further work in this slice; Phase 1's
//! remaining items (paging, scheduler, syscalls, userspace) land next.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nitrox_kernel::arch;
use nitrox_kernel::arch::cpu::ArchCpu;
use nitrox_kernel::arch::paging::ArchPaging;
use nitrox_kernel::dpc;
use nitrox_kernel::framebuffer::{FbWriter, Rgb};
use nitrox_kernel::kprintln;
use nitrox_kernel::limine::{
    BaseRevision, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    RequestsEndMarker, RequestsStartMarker,
};
use nitrox_kernel::mm;
use nitrox_kernel::sched;

// --- Limine request statics ---------------------------------------------
//
// Each item below lives in `.limine_requests*` so the linker keeps it and
// Limine can find it by scanning the bracketed region. `#[used]` is
// belt-and-braces: `KEEP()` in the linker script already prevents GC, but
// the attribute also stops rustc from inlining the static away before the
// linker sees it.

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new(6);

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

// `static mut`, not `static`: Limine writes to the `response` field
// after the kernel is loaded but before `_start` runs. With a plain
// `static`, rustc is allowed to constant-fold reads against the
// const-initialised null and never observe Limine's write — which
// silently breaks `init_memory`. The `static mut` here mirrors
// `FRAMEBUFFER_REQUEST` above; `BASE_REVISION` gets away without it
// because its `supported()` reads via `ptr::read_volatile`.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

// The initramfs module (Limine loads `boot/initramfs`, tagged
// `MEMMAP_KERNEL_AND_MODULES`, mapped in the HHDM). `static mut` for the same
// reason as `MEMMAP_REQUEST`: Limine writes `response` after load.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut MODULE_REQUEST: ModuleRequest = ModuleRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQUESTS_START: RequestsStartMarker = RequestsStartMarker::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END: RequestsEndMarker = RequestsEndMarker::new();

// --- Entry point --------------------------------------------------------

/// ELF entry point. Limine jumps here after setting up long mode, paging,
/// a 64 KiB stack, and the framebuffer. We never return; the bootloader's
/// caller frame pushed a zero return address as a tripwire.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kernel_main();
    arch::Cpu::halt_loop();
}

fn kernel_main() {
    if !BASE_REVISION.supported() {
        // Limine refused our protocol revision. No framebuffer is safe to
        // touch in this state — just halt.
        return;
    }

    // Serial first: it touches only fixed I/O ports — no dependency on
    // the allocator or the IDT — so every step after this can report its
    // progress, and its failures, to the console.
    arch::serial::init();
    kprintln!("Nitrox kernel — diagnostics online");

    // Install the architecture's CPU control tables (on x86_64: GDT + TSS,
    // then IDT). The ordering dependency between them lives in the arch
    // layer, not here.
    arch::Cpu::init_tables();
    kprintln!("CPU tables installed (GDT/TSS/IDT)");

    // Bring up the physical-memory buddy allocator and the slab on top of
    // it. This is the first code that walks Limine's structures and pokes
    // the allocator — the first place a bug can fault — so the IDT is
    // live before we reach it. Returns false if Limine didn't populate a
    // required response, in which case there is nothing useful to do.
    if !init_memory() {
        kprintln!("init_memory failed — halting");
        return;
    }
    kprintln!("allocators up");

    // One-time paging setup, then a smoke test against Limine's live
    // tables. `paging_init` enables NX and captures the kernel-half
    // PML4 template every future `AddressSpace::new` will inherit
    // from; it must run before any AS is constructed.
    paging_init();
    paging_smoke_test();

    // Record the Limine-loaded initramfs module (if any) so the `/initramfs`
    // resource server can serve it. Needs the HHDM (the module's `address` is an
    // HHDM-virtual pointer); non-fatal if no module was configured.
    init_initramfs();

    // Discover platform hardware from the firmware tables (ACPI on x86_64): the
    // PCIe ECAM window (for PCI enumeration) and the interrupt-routing topology
    // (for the IOAPIC bring-up). Reads firmware memory through the HHDM, so it
    // runs after the allocator/HHDM are up; before the local controller so the
    // future IOAPIC step can consume the cached routing facts. Missing or
    // malformed tables are logged, not fatal, and the parser logs its summary.
    {
        use arch::platform::ArchPlatform;
        // SAFETY: ring 0, single CPU, called once during boot after the HHDM is
        // available; reads firmware-owned physical memory, no allocation.
        let _ = unsafe { arch::Platform::init() };
    }

    // Bring up this CPU's local interrupt controller (xAPIC). Interrupts
    // stay masked (IF=0) for this whole slice — nothing is delivered yet; the
    // timer source lands with the Timers slice and the spurious/timer IDT
    // stubs + IF=1 with the preemptive-scheduling slice.
    {
        use arch::irq::ArchIrq;
        // SAFETY: ring 0, single CPU, called once during boot after CPU
        // feature enablement and after the kernel-vmap allocator is up; IF is
        // 0, so software-enabling the controller delivers nothing.
        if unsafe { arch::Irq::init() }.is_err() {
            kprintln!("local APIC bring-up failed — halting");
            return;
        }
        kprintln!("local APIC up (xAPIC, id {})", arch::Irq::id());
    }

    // Calibrate the monotonic time source (TSC) and the per-CPU timer (LAPIC
    // timer) against the legacy PIT. Must follow Irq::init — the local
    // controller's MMIO has to be mapped before its timer can be programmed.
    // Interrupts stay masked (IF=0): the timer is calibrated and armable, but
    // fires nothing this slice (the periodic tick lands with preemptive
    // scheduling, one-shot deadlines with wait queues).
    {
        use arch::timer::ArchTimer;
        // SAFETY: ring 0, single CPU, called once during boot after Irq::init
        // mapped the local-controller MMIO; IF=0, so arming delivers nothing.
        unsafe { arch::Timer::init() };
        kprintln!(
            "timer up: monotonic {} MHz, per-CPU timer {} MHz (clock t0={} ns)",
            arch::Timer::monotonic_hz() / 1_000_000,
            arch::Timer::timer_hz() / 1_000_000,
            arch::Timer::read_ns(),
        );
    }

    // Reserve the DPC (deferred-procedure-call) queue before any interrupt can
    // enqueue onto it — the IOAPIC self-test below routes the PIT, whose ISR
    // queues a DPC, and device IRQs queue DPCs in general. Needs the allocator
    // (it reserves its backing list once); never allocates again.
    if dpc::init().is_err() {
        kprintln!("DPC queue init failed — halting");
        return;
    }

    // Bring up the system interrupt router (the IOAPIC) so external device
    // interrupts can be delivered, then prove the routing path end-to-end with
    // a brief self-test. This runs while the LAPIC timer is still masked and the
    // scheduler is not yet running, so the self-test's short interrupt-enabled
    // window fires only the source it routes (the legacy PIT). The router needs
    // the local controller (Irq::init) up — routed interrupts land on a LAPIC.
    {
        use arch::irq_router::ArchIrqRouter;
        // SAFETY: ring 0, single CPU, once during boot after Irq/Timer init; the
        // ACPI MADT facts are cached (Platform::init ran).
        if unsafe { arch::IrqRouter::init() }.is_err() {
            kprintln!("interrupt router bring-up failed — halting");
            return;
        }
        // SAFETY: ring 0, after the router is up and before the scheduler arms
        // its periodic timer; briefly enables interrupts for the routed source.
        unsafe { arch::IrqRouter::self_test() };
    }

    // Seed the entropy subsystem (CSPRNG). Runs after the timer is up (so the
    // monotonic clock is live for jitter mixing) and before the handle table, so
    // the table seeds its free-list shuffle from the CSPRNG rather than a fixed
    // constant. On any CPU with RDSEED/RDRAND this latches `seeded` immediately.
    nitrox_kernel::entropy::init();

    // Bring up the single global handle table. It eagerly allocates its
    // first segment, so the heap must be up (it is — `init_memory` ran); it
    // must be live before any userspace can issue a handle syscall.
    if nitrox_kernel::handle::global::init().is_err() {
        kprintln!("global handle table init failed — halting");
        return;
    }
    kprintln!("global handle table up");

    // Enumerate hardware into the device table. Runs after the allocators, the
    // HHDM, the kvmap, and `Platform::init` (the ECAM regions) — all up by now.
    // Phase 2 slice 5 Part 1: discovery + `DeviceNode`s only; no driver claims a
    // node yet.
    nitrox_kernel::device::init();

    // Phase 2 slice 5 Part 2: prove the async I/O spine (IRP → DPC →
    // PendingOperation) and InterruptObject signalling on a RAM-backed device,
    // before any real driver exists. Needs the DPC queue + scheduler waitables.
    nitrox_kernel::io::self_test();

    // Phase 2 slice 5 Part 3: match Tier 1 drivers against the enumerated
    // devices (the AHCI controller) and bring up any disks, then read sector 0
    // through the real driver to prove the IRP → controller DMA → IRQ → DPC → PO
    // path against hardware.
    nitrox_kernel::drivers::probe();
    nitrox_kernel::drivers::self_test();

    // Bring up the cooperative scheduler and run a few kernel threads to
    // prove the context switch end-to-end: each worker prints and yields
    // round-robin, then exits; the boot thread drains the queue and
    // returns here. See `docs/architecture/overview.md` § Scheduling.
    run_scheduler_demo();

    // Prove the demand-paging on-fault path in the live kernel before handing
    // off — the first userspace process then exercises it for real (its stack
    // is reserved lazily and faults in on first use).
    demand_paging_smoke_test();

    // Prove the DMA-buffer allocation path (the storage slice's prerequisite).
    dma_smoke_test();

    // Arm the syscall fast path and prove it end-to-end by dropping to
    // ring 3 and running a tiny hand-assembled blob that calls sys_kprint.
    // (Throwaway harness — replaced next slice by an ELF-loaded process.)
    run_first_userspace();

    // Best-effort boot screen, then retire the boot thread into the idle
    // thread. We must NOT fall through to `_start`'s `halt_loop` (it `cli`s,
    // which would freeze preemption): `exit` switches to the idle thread, which
    // `hlt`s with interrupts enabled so the periodic tick keeps running.
    draw_boot_screen();
    // The boot thread has no owning process; exit with a benign status (no
    // `ChildExited` is produced for a process-less thread).
    sched::exit_thread(nitrox_kernel::libkern::ExitStatus {
        kind: nitrox_kernel::libkern::ExitKind::Normal as u32,
        code: 0,
    });
}

/// Draw the boot screen to Limine's framebuffer, if one is present. Best-effort:
/// any missing piece simply skips the draw. Runs in the boot thread.
fn draw_boot_screen() {
    // SAFETY: `FRAMEBUFFER_REQUEST.response` is written by Limine before
    // jumping to `_start`. We are the sole reader.
    let response = unsafe { (&raw const FRAMEBUFFER_REQUEST).read().response };
    if response.is_null() {
        return;
    }
    // SAFETY: A non-null response pointer guarantees Limine populated a
    // valid `FramebufferResponse`.
    let response = unsafe { &*response };
    if response.framebuffer_count == 0 || response.framebuffers.is_null() {
        return;
    }
    // SAFETY: The framebuffer array is dense; the first slot is always
    // present when `framebuffer_count > 0`.
    let fb_ptr = unsafe { *response.framebuffers };
    if fb_ptr.is_null() {
        return;
    }
    // SAFETY: Limine guarantees this pointer outlives the kernel (the
    // framebuffer descriptor lives in bootloader-reclaimable memory which
    // we have not reclaimed in Phase 0).
    let fb = unsafe { &*fb_ptr };

    // SAFETY: We trust Limine's framebuffer descriptor — its `address`,
    // `pitch`, and `height` describe a writable linear region.
    let mut writer = match unsafe { FbWriter::from_limine(fb) } {
        Some(w) => w,
        None => return,
    };

    draw_nitrox_band(&mut writer);
}

/// A CPU-bound demo kernel thread that **never yields**: each round spins on a
/// bounded busy loop, then prints. Under preemption the periodic timer forces
/// the workers (and the boot thread) to interleave — cooperatively, worker 1
/// would finish all its rounds before worker 2 ever printed. The interleaved
/// output is the proof of preemption. Returns when done (the trampoline calls
/// [`sched::exit`]).
extern "C" fn busy_worker(arg: usize) {
    for round in 0..3 {
        // Enough work to span several 10 ms ticks so preemption is visible.
        let mut acc: u64 = 0;
        for i in 0..20_000_000u64 {
            acc = acc.wrapping_add(i ^ arg as u64);
        }
        core::hint::black_box(acc);
        kprintln!("worker {} round {}", arg, round);
    }
    kprintln!("worker {} exiting", arg);
}

/// Initialise the scheduler, **arm preemption** (periodic timer + IF=1), spawn
/// three CPU-bound workers that never yield, and let the timer interleave them.
/// Proves switch-in, the timer-IRQ-driven preemptive switch, round-robin
/// rotation, clean exit, and stack reclamation. The boot thread itself is
/// preempted while it busy-waits for the queue to drain.
fn run_scheduler_demo() {
    use arch::cpu::ArchCpu;
    use arch::timer::ArchTimer;

    if sched::init().is_err() {
        kprintln!("sched: init failed — skipping demo");
        return;
    }

    // Arm the periodic tick, then raise IF. Order matters: program the timer's
    // period before enabling delivery. The IDT timer stub (installed by
    // `Cpu::init_tables`) and the calibrated timer (`Timer::init`) are already
    // in place.
    // SAFETY: ring 0, single CPU, once; the IDT + calibrated timer are live.
    unsafe {
        arch::Timer::start_periodic(sched::TICK_NS);
        arch::Cpu::interrupts_enable();
    }
    kprintln!(
        "preemption armed (IF=1, {} Hz tick)",
        1_000_000_000 / sched::TICK_NS
    );

    for id in 1..=3 {
        if sched::spawn(busy_worker, id).is_err() {
            kprintln!("sched: spawn {} failed", id);
        }
    }
    // Busy-wait (no cooperative yield) until every worker has run to completion;
    // the timer preempts the boot thread too. `reap_pending` reclaims each
    // exited worker's stack.
    loop {
        sched::reap_pending();
        if sched::ready_is_empty() {
            break;
        }
        core::hint::spin_loop();
    }
    kprintln!("sched: all workers done (preemptively interleaved)");
}

// --- First userspace process --------------------------------------------
//
// Load the embedded `hello` ELF into a fresh address space, wrap it in a
// Process (pid 1), spawn its main thread, and let the scheduler run it into
// ring 3. It prints via `sys_kprint`, then `sys_process_exit` routes through
// the scheduler — the thread is reaped on the next scheduler entry, freeing
// the Process and its address space. This is the substrate-works milestone.

/// Arm the syscall fast path, then load + launch **init** as pid 1 with a handle
/// to its own notification channel (`rdi`) and a full-rights root-namespace handle
/// (`rsi`) carrying the boot kernel-server bindings. init reads its manifest from
/// the initramfs, spawns the demo chain (`parent` → `child`), and runs the reaping
/// loop. This boot thread hands off (via `sched::exit` in `kernel_main`) into init
/// and then idles; init is the supervisor now (not the kernel). The init ELF is
/// embedded in the kernel lib (`embedded_images`, [`ImageId::Init`]) along with the
/// spawn-able `parent`/`child`.
fn run_first_userspace() {
    use mm::addr_space::AddressSpace;
    use mm::elf::load_elf;
    use nitrox_kernel::handle::global;
    use nitrox_kernel::libkern::ImageId;
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::libkern::handle::{KObjectType, Rights};
    use nitrox_kernel::object::kernel_server::KernelServerId;
    use nitrox_kernel::object::{Namespace, NotificationChannel, ObjectRef, Process};

    // Arm the `syscall` entry MSRs once. The per-CPU kernel stack is set
    // per-thread (by the scheduler's `thread_enter`), not here.
    arch::init_syscall_entry();
    kprintln!("syscall fast-path armed");

    // Fresh address space (kernel half inherited → loadable), from the init ELF.
    let aspace = match AddressSpace::new() {
        Ok(a) => a,
        Err(_) => {
            kprintln!("init: address space alloc failed");
            return;
        }
    };
    let info = match load_elf(&aspace, nitrox_kernel::embedded_images::image_bytes(ImageId::Init)) {
        Ok(i) => i,
        Err(e) => {
            kprintln!("init: ELF load failed: {:?}", e);
            return;
        }
    };

    // Build the parent process (pid 1; it is the root — no `parent_notif`).
    let mut proc_box = match Process::try_new_user(1, aspace) {
        Ok(p) => p,
        Err(_) => {
            kprintln!("init: process alloc failed");
            return;
        }
    };

    // The parent's notification channel: the Process owns one reference; a
    // handle in pid 1's table owns the other (passed to the parent in `rdi` so
    // it can `sys_wait` / `sys_notif_recv` for its children's `ChildExited`).
    let chan = match NotificationChannel::try_new() {
        Ok(c) => c,
        Err(_) => {
            kprintln!("init: notification channel alloc failed");
            return;
        }
    };
    let chan_ptr = KBox::into_raw(chan).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it, clone
    // one for the Process, install the other as a handle (refcount → 2).
    let chan_ref = unsafe { ObjectRef::from_raw(chan_ptr, KObjectType::NotificationChannel) };
    proc_box.set_notification_channel(chan_ref.clone());
    let (cp, ct) = chan_ref.into_raw();
    let notif_rights = Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT;
    let notif_h = match global::get().allocate(1, cp, ct, notif_rights) {
        Ok(h) => h,
        Err(_) => {
            // SAFETY: reclaim the channel-handle reference; `proc_box` drops too.
            drop(unsafe { ObjectRef::from_raw(cp, ct) });
            kprintln!("init: notification handle alloc failed");
            return;
        }
    };

    // The parent's root namespace (the namespace it resolves names against —
    // `Process::namespace`). The Process owns one reference; a handle in pid 1's
    // table owns the other, passed to the parent in `rsi` so it can bind into and
    // look up against its root namespace. Mirrors the notification channel above.
    let ns = match Namespace::try_new() {
        Ok(n) => n,
        Err(_) => {
            kprintln!("init: namespace alloc failed");
            return;
        }
    };

    // Bind the in-kernel resource servers into pid 1's root namespace (the kernel
    // is the "supervisor" at boot; userspace servers register via the Ready
    // handshake instead — slice 7). Children inherit these via namespace
    // inheritance. `/dev/entropy` is the first Kernel Server: a lookup mints an
    // `EntropyObject` (see `object::kernel_server`). Its binding rights are the
    // band `sys_entropy_create` mints (`entropy_rights`): `READ` + the generic
    // management band, so a full-rights lookup yields exactly that.
    let entropy_binding_rights =
        Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, entropy_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/entropy failed");
        return;
    }

    // `/proc/self/*` — self-reference servers. Each binding is just a dispatch id;
    // the *answer* is the looking-up thread's OWN object, resolved per-caller from
    // syscall context (no ambient authority — see `kernel_server`). One binding is
    // therefore shared by all callers/descendants. Per-leaf bindings (not one
    // `/proc/self` prefix) because the returned types — Process / Thread / Namespace
    // — carry disjoint principal rights, so each needs its own type-correct cap.
    let proc_self_principal_rights = Rights::SIGNAL
        | Rights::TERMINATE
        | Rights::DUPLICATE
        | Rights::INSPECT
        | Rights::TRANSFER;
    // The namespace view is LOOKUP-only (a resolve view; self already holds a
    // full-rights root-namespace handle via `rsi`). No BIND — granting self-bind
    // ambiently would be a capability-escalation smell.
    let proc_self_namespace_rights =
        Rights::LOOKUP | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    let proc_self_binds = [
        (
            &b"/proc/self/process"[..],
            KernelServerId::ProcSelfProcess,
            proc_self_principal_rights,
        ),
        (
            &b"/proc/self/thread"[..],
            KernelServerId::ProcSelfThread,
            proc_self_principal_rights,
        ),
        (
            &b"/proc/self/namespace"[..],
            KernelServerId::ProcSelfNamespace,
            proc_self_namespace_rights,
        ),
    ];
    for (path, id, rights) in proc_self_binds {
        if ns.bind_kernel_server(path, id, rights).is_err() {
            kprintln!("init: binding /proc/self/* failed");
            return;
        }
    }

    // `/initramfs/<path>` — a subtree server returning a read-only `MemoryObject`
    // copy of a file from the boot CPIO blob. The caller maps it `MAP_READ`, so
    // the binding grants `MAP_READ` + the generic management band.
    let initramfs_binding_rights =
        Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/initramfs", KernelServerId::Initramfs, initramfs_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /initramfs failed");
        return;
    }

    // `/dev/blk/<n>` — a subtree server resolving the n-th discovered block device
    // to a `DeviceNode` handle (the caller `sys_io_submit`s reads on it). Bound
    // **unconditionally**: the device-table registry carries liveness, so a
    // lookup of `/dev/blk/0` is `NotFound` if no disk was discovered, harmless.
    // Read-only in Phase 2 — the binding grants `READ` + the generic band, so a
    // write IoOp is rejected at the lookup rights gate.
    let block_binding_rights =
        Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/blk", KernelServerId::BlockDevice, block_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/blk failed");
        return;
    }

    let ns_ptr = KBox::into_raw(ns).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it, clone
    // one for the Process, install the other as a handle (refcount → 2).
    let ns_ref = unsafe { ObjectRef::from_raw(ns_ptr, KObjectType::Namespace) };
    proc_box.set_namespace(ns_ref.clone());
    let (np, nt) = ns_ref.into_raw();
    // Full namespace rights — kept in sync with `syscall::table::namespace_rights`
    // (LOOKUP|BIND principals + the UNBIND modifier + the generic band).
    let ns_rights = Rights::LOOKUP
        | Rights::BIND
        | Rights::UNBIND
        | Rights::DUPLICATE
        | Rights::TRANSFER
        | Rights::INSPECT;
    let ns_h = match global::get().allocate(1, np, nt, ns_rights) {
        Ok(h) => h,
        Err(_) => {
            // SAFETY: reclaim the namespace-handle reference; `proc_box` (which
            // still owns its clone) drops on return.
            drop(unsafe { ObjectRef::from_raw(np, nt) });
            kprintln!("init: namespace handle alloc failed");
            return;
        }
    };

    let proc_ref = {
        let ptr = KBox::into_raw(proc_box).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(ptr, KObjectType::Process) }
    };

    // Spawn the parent's main thread, seeding `rdi` = its notification handle and
    // `rsi` = its root-namespace handle.
    if sched::spawn_user(
        proc_ref,
        info.entry_point.as_u64(),
        info.stack_top.as_u64(),
        [notif_h.bits(), ns_h.bits(), 0, 0],
    )
    .is_err()
    {
        kprintln!("init: spawn_user failed");
        return;
    }
    kprintln!("init: spawned init (pid 1); handing off to userspace");
}

/// Record the Limine-loaded initramfs module (the first one) for the
/// `/initramfs` resource server. Non-fatal if no module was configured.
fn init_initramfs() {
    // SAFETY: `MODULE_REQUEST` lives in `.limine_requests`; Limine wrote its
    // `response` before `_start`. Read through a raw-pointer copy.
    let resp = unsafe { (&raw const MODULE_REQUEST).read().response };
    if resp.is_null() {
        kprintln!("initramfs: no module loaded");
        return;
    }
    // SAFETY: a non-null response is a valid Limine `ModuleResponse`.
    let resp = unsafe { &*resp };
    if resp.module_count == 0 || resp.modules.is_null() {
        kprintln!("initramfs: no module loaded");
        return;
    }
    // SAFETY: `modules` points at an array of `module_count` `*mut LimineFile`;
    // we take the first (Nitrox configures exactly one module).
    let file = unsafe { &**resp.modules };
    let (addr, size) = (file.address, file.size as usize);
    if addr.is_null() || size == 0 {
        kprintln!("initramfs: module empty");
        return;
    }
    // SAFETY: `addr` is an HHDM-virtual pointer to `size` live bytes (the module,
    // in never-reclaimed `MEMMAP_KERNEL_AND_MODULES` memory).
    unsafe { nitrox_kernel::initramfs::set_blob(addr, size) };
    kprintln!("initramfs: loaded {} bytes", size);
}

/// Bring up the buddy allocator and the slab on top of it. Returns false
/// if Limine didn't populate either of the requests we depend on.
fn init_memory() -> bool {
    // SAFETY: `MEMMAP_REQUEST` and `HHDM_REQUEST` live in
    // `.limine_requests*`. Limine writes the response pointer into each
    // before jumping to `_start`. Reading through a raw-pointer copy
    // avoids the optimiser caching the pre-Limine null.
    let memmap_resp = unsafe { (&raw const MEMMAP_REQUEST).read().response };
    if memmap_resp.is_null() {
        return false;
    }
    let hhdm_resp = unsafe { (&raw const HHDM_REQUEST).read().response };
    if hhdm_resp.is_null() {
        return false;
    }
    // SAFETY: Each non-null response pointer guarantees Limine populated
    // a valid response of the corresponding type. The responses live in
    // bootloader-reclaimable memory which we have not yet reclaimed.
    let memmap = unsafe { &*memmap_resp };
    let hhdm_offset = unsafe { (*hhdm_resp).offset };
    // SAFETY: `memmap` is a live Limine response and `hhdm_offset` is
    // the bootloader's HHDM base — together they satisfy the contract
    // of `BuddyAllocator::new` (see `kernel/src/mm/buddy.rs`).
    unsafe {
        mm::heap::init_buddy(memmap, hhdm_offset);
    }
    mm::slab::slab_init();
    true
}

/// One-time paging setup that must run before any `AddressSpace::new`:
///
/// 1. Enable every CPU memory-protection feature the kernel depends
///    on. On x86_64: NX paging extension, SMEP, SMAP. The arch impl
///    panics if any required feature is missing from the running CPU.
/// 2. Pre-allocate the kernel-vmap region's intermediate page tables
///    in the live PML4, so the next step's snapshot captures them and
///    every future AS inherits the shared sub-tree.
/// 3. Capture the kernel-half PML4 entries from Limine's live tables
///    into the boot template every new address space inherits.
///
/// The ordering matters: `kvmap::init` modifies the live PML4 in ways
/// the template must see; the template snapshot freezes the kernel
/// half post-call. See the "Kernel-half PML4 sharing" section in
/// `docs/architecture/memory-management.md`.
fn paging_init() {
    arch::Cpu::init_protections();
    kprintln!("memory protections enabled");
    // SAFETY: HHDM is up (init_memory ran first) and the buddy
    // allocator is live; no AS exists yet whose captured template
    // could disagree with the new PML4 entries.
    unsafe {
        mm::kvmap::init();
        arch::Paging::init_kernel_template(arch::Paging::active_root());
    }
}

/// Smoke-test the paging arch layer: walk Limine's live page tables
/// with [`arch::translate`] to confirm the kernel's table-walk agrees
/// with the hardware.
///
/// Read-only — it never installs or switches a mapping. Exercises the
/// real address space and real HHDM offset (host tests run with an
/// HHDM offset of 0).
fn paging_smoke_test() {
    // Limine's top-level page table is whatever the CPU runs on now.
    let root = arch::Paging::active_root();

    // This function's own code is certainly mapped; resolve its address.
    let probe = mm::VirtAddr::new(paging_smoke_test as fn() as usize as u64);
    // SAFETY: `root` is the live top-level page table the CPU is using,
    // reachable through the HHDM. `translate` only reads page-table
    // memory — it installs and switches nothing.
    match unsafe { arch::Paging::translate(root, probe) } {
        Some(phys) => kprintln!(
            "paging: NX enabled; translate {:#x} -> {:#x}",
            probe.as_u64(),
            phys.as_u64()
        ),
        None => kprintln!(
            "paging: translate {:#x} -> UNMAPPED — walk disagrees with hardware",
            probe.as_u64()
        ),
    }
}

/// Demand-paging smoke test: exercise the on-fault path end-to-end in the live
/// kernel. Reserve a lazy anonymous range (`map_vma_lazy` — no frames), confirm
/// it is genuinely unbacked (the hardware walk finds no translation), fault each
/// page in via `AddressSpace::fault_in`, and confirm the pages are now mapped.
/// The throwaway address space is torn down on drop. The first userspace process
/// additionally exercises this path for real: its stack is reserved lazily and
/// faults in on use.
fn demand_paging_smoke_test() {
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::mm::addr_space::{AddressSpace, FaultIn};
    use nitrox_kernel::mm::vmm::{FaultAccess, MappingKind, Protection, VAddrRange, Vma};

    let page = mm::PAGE_SIZE as u64;
    let Ok(asp) = AddressSpace::new() else {
        kprintln!("demand-paging: smoke test skipped (AS alloc failed)");
        return;
    };
    // Arbitrary page-aligned user-half address for the reservation.
    let base = 0x4000_0000u64;
    let range = VAddrRange::new(mm::VirtAddr::new(base), mm::VirtAddr::new(base + 2 * page))
        .expect("smoke-test range is valid by construction");
    let Ok(vma) = KBox::try_new(Vma::new(
        range,
        Protection::WRITE | Protection::USER,
        MappingKind::Anonymous,
    )) else {
        kprintln!("demand-paging: smoke test skipped (VMA alloc failed)");
        return;
    };
    if asp.map_vma_lazy(vma).is_err() {
        kprintln!("demand-paging: smoke test skipped (lazy reserve failed)");
        return;
    }

    // Reserved but unbacked: the hardware walk must find no translation yet.
    // SAFETY: `translate` only reads this AS's own page-table memory via the
    // HHDM; it installs and switches nothing.
    let before = unsafe { arch::Paging::translate(asp.root(), mm::VirtAddr::new(base)) };

    // Fault each page in — a write to page 0, a read to page 1.
    let r0 = asp.fault_in(mm::VirtAddr::new(base), FaultAccess::Write);
    let r1 = asp.fault_in(mm::VirtAddr::new(base + page), FaultAccess::Read);

    // SAFETY: as above — page 0 should now resolve.
    let after = unsafe { arch::Paging::translate(asp.root(), mm::VirtAddr::new(base)) };

    if before.is_none() && after.is_some() && r0 == FaultIn::Mapped && r1 == FaultIn::Mapped {
        kprintln!(
            "demand-paging: on-fault path OK — 2 anonymous pages backed lazily ({} faulted in since boot)",
            mm::demand_fault_count()
        );
    } else {
        kprintln!(
            "demand-paging: smoke test FAILED (before={:?} after={:?} r0={:?} r1={:?})",
            before,
            after,
            r0,
            r1
        );
    }
}

/// DMA-buffer smoke test: prove `DmaBuffer` (the storage slice's allocation
/// prerequisite) gives a physically-contiguous, page-aligned, zeroed block whose
/// physical address is exactly what a device would see at its `virt()` mapping.
/// Allocate a 2-page buffer, check it's zeroed + page-aligned, write a sentinel
/// through the CPU view, and confirm the active page tables translate `virt()`
/// back to `phys()` (the same bytes the device DMAs). Dropped at the end.
fn dma_smoke_test() {
    use nitrox_kernel::mm::DmaBuffer;

    let mut buf = match DmaBuffer::alloc(2 * mm::PAGE_SIZE) {
        Ok(b) => b,
        Err(_) => {
            kprintln!("dma: smoke test skipped (alloc failed)");
            return;
        }
    };
    let phys = buf.phys();
    let zeroed = buf.as_slice().iter().all(|&b| b == 0);
    // Write a sentinel at the first and last byte through the CPU (HHDM) view.
    let len = buf.len();
    buf.as_mut_slice()[0] = 0xD3;
    buf.as_mut_slice()[len - 1] = 0xA7;

    // The active page tables must translate the buffer's virtual base back to its
    // physical base — i.e. a device programmed with `phys()` sees these bytes.
    // SAFETY: read-only walk of the live top-level page table via the HHDM.
    let mapped = unsafe {
        arch::Paging::translate(arch::Paging::active_root(), mm::VirtAddr::new(buf.virt() as u64))
    };

    if zeroed && phys.is_page_aligned() && phys.as_u64() != 0 && mapped == Some(phys) {
        kprintln!(
            "dma: {}-page buffer @ phys {:#x} (contiguous, page-aligned, zeroed)",
            len / mm::PAGE_SIZE,
            phys.as_u64()
        );
    } else {
        kprintln!(
            "dma: smoke test FAILED (zeroed={} aligned={} mapped={:?} phys={:#x})",
            zeroed,
            phys.is_page_aligned(),
            mapped,
            phys.as_u64()
        );
    }
}

/// Render the boot screen as a scuba Nitrox tank decal: a yellow band
/// bordered by dark-green bands with `NITROX` lettered in green across
/// the centre, plus a phase indicator below.
fn draw_nitrox_band(writer: &mut FbWriter) {
    writer.clear(Rgb::BG);

    let width = writer.width();
    let height = writer.height();

    // Band geometry. The yellow stripe carries the title; the two green
    // stripes sandwich it the way they do on a real tank decal.
    let yellow_h: usize = 160;
    let green_h: usize = 28;
    let total_h: usize = yellow_h + green_h * 2;
    let band_top = height.saturating_sub(total_h) / 2;

    writer.fill_rect(0, band_top, width, green_h, Rgb::NITROX_GREEN);
    writer.fill_rect(0, band_top + green_h, width, yellow_h, Rgb::NITROX_YELLOW);
    writer.fill_rect(
        0,
        band_top + green_h + yellow_h,
        width,
        green_h,
        Rgb::NITROX_GREEN,
    );

    // "NITROX" centred on the yellow band, in dark green. Pick the
    // largest integer scale that still leaves a margin inside the band.
    let text = b"NITROX";
    let scale = pick_scale(text, width, yellow_h);
    let text_w = FbWriter::text_width(text, scale);
    let text_h = FbWriter::text_height(scale);
    let text_x = (width - text_w) / 2;
    let text_y = band_top + green_h + (yellow_h - text_h) / 2;
    writer.draw_text_at(text_x, text_y, text, Rgb::NITROX_GREEN, scale);

    // Phase indicator below the band, slightly dimmer so the eye reads
    // the tank decal first.
    let status = b"PHASE 2: NAMESPACES UP";
    let status_scale = 2;
    let status_w = FbWriter::text_width(status, status_scale);
    let status_x = (width - status_w) / 2;
    let status_y = band_top + total_h + 32;
    writer.draw_text_at(status_x, status_y, status, Rgb::FG, status_scale);
}

/// Choose the largest integer scale such that the text fits within
/// `max_w` and `max_h` with reasonable margins on both axes.
fn pick_scale(text: &[u8], max_w: usize, max_h: usize) -> usize {
    let w_margin = 64;
    let h_margin = 24;
    let available_w = max_w.saturating_sub(w_margin);
    let available_h = max_h.saturating_sub(h_margin);
    let mut scale: usize = 1;
    while scale < 16 {
        let next = scale + 1;
        if FbWriter::text_width(text, next) > available_w
            || FbWriter::text_height(next) > available_h
        {
            return scale;
        }
        scale = next;
    }
    scale
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    use core::fmt::Write;

    // Use the unsynchronised emergency serial path, not `kprintln!`: a
    // panic can occur while `SERIAL`'s lock is held, and re-locking would
    // deadlock. Bypassing the lock is sound under Phase 1's single-CPU,
    // interrupts-masked model — no other context can be driving COM1.
    let mut w = arch::serial::emergency_writer();
    let _ = writeln!(w, "\n*** KERNEL PANIC ***");
    if let Some(loc) = info.location() {
        let _ = writeln!(w, "  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    let _ = writeln!(w, "  {}", info.message());
    arch::Cpu::halt_loop()
}
