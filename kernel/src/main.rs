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
// The arch abstraction's traits are brought into scope module-wide: their methods
// (`arch::Irq::init()`, `arch::Timer::start_periodic()`, …) are called across the boot
// functions here (`kernel_main`, `sched_bringup`, `ap_entry`). Method resolution is by
// receiver type, so importing them together is unambiguous.
use nitrox_kernel::arch::cpu::ArchCpu;
use nitrox_kernel::arch::irq::ArchIrq;
use nitrox_kernel::arch::irq_router::ArchIrqRouter;
use nitrox_kernel::arch::paging::ArchPaging;
use nitrox_kernel::arch::platform::ArchPlatform;
use nitrox_kernel::arch::smp::ArchSmp;
use nitrox_kernel::arch::timer::ArchTimer;
use nitrox_kernel::dpc;
use nitrox_kernel::framebuffer::{FbWriter, Rgb};
use nitrox_kernel::kprintln;
use nitrox_kernel::limine::{
    BaseRevision, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    RequestsEndMarker, RequestsStartMarker, SmpInfo, SmpRequest,
};
use nitrox_kernel::mm;
use nitrox_kernel::sched;

use core::sync::atomic::{AtomicU32, Ordering};

/// Boot-time self-tests and demos — compiled only under the `selftest` feature. A
/// normal `cargo xtask qemu` boots straight to userspace; `--selftest` runs them.
#[cfg(feature = "selftest")]
mod boot_selftest;

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

// SMP: Limine starts the APs and parks them; the kernel launches each by writing
// its `goto_address` (see `bring_up_aps`). `static mut` — Limine writes `response`.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut SMP_REQUEST: SmpRequest = SmpRequest::new();

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
    #[cfg(feature = "selftest")]
    boot_selftest::paging();

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
    // SAFETY: ring 0, single CPU, called once during boot after the HHDM is
    // available; reads firmware-owned physical memory, no allocation.
    let _ = unsafe { arch::Platform::init() };

    // Bring up this CPU's local interrupt controller (xAPIC). Interrupts
    // stay masked (IF=0) for this whole slice — nothing is delivered yet; the
    // timer source lands with the Timers slice and the spurious/timer IDT
    // stubs + IF=1 with the preemptive-scheduling slice.
    // SAFETY: ring 0, single CPU, called once during boot after CPU
    // feature enablement and after the kernel-vmap allocator is up; IF is
    // 0, so software-enabling the controller delivers nothing.
    if unsafe { arch::Irq::init() }.is_err() {
        kprintln!("local APIC bring-up failed — halting");
        return;
    }
    kprintln!("local APIC up (x2APIC, id {})", arch::Irq::id());

    // Calibrate the monotonic time source (TSC) and the per-CPU timer (LAPIC
    // timer) against the legacy PIT. Must follow Irq::init — the local
    // controller's MMIO has to be mapped before its timer can be programmed.
    // Interrupts stay masked (IF=0): the timer is calibrated and armable, but
    // fires nothing this slice (the periodic tick lands with preemptive
    // scheduling, one-shot deadlines with wait queues).
    // SAFETY: ring 0, single CPU, called once during boot after Irq::init
    // mapped the local-controller MMIO; IF=0, so arming delivers nothing.
    unsafe { arch::Timer::init() };
    kprintln!(
        "timer up: monotonic {} MHz, per-CPU timer {} MHz (clock t0={} ns)",
        arch::Timer::monotonic_hz() / 1_000_000,
        arch::Timer::timer_hz() / 1_000_000,
        arch::Timer::read_ns(),
    );

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
    // SAFETY: ring 0, single CPU, once during boot after Irq/Timer init; the
    // ACPI MADT facts are cached (Platform::init ran).
    if unsafe { arch::IrqRouter::init() }.is_err() {
        kprintln!("interrupt router bring-up failed — halting");
        return;
    }
    #[cfg(feature = "selftest")]
    boot_selftest::irq_routing();

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
    #[cfg(feature = "selftest")]
    boot_selftest::irp_spine();

    // Phase 2 slice 5 Part 3: match Tier 1 drivers against the enumerated
    // devices (the AHCI controller) and bring up any disks, then read sector 0
    // through the real driver to prove the IRP → controller DMA → IRQ → DPC → PO
    // path against hardware.
    nitrox_kernel::drivers::probe();
    #[cfg(feature = "selftest")]
    boot_selftest::storage();

    // Phase 2 slice 9 Part 1: bring up serial console **input** (COM1 RX). Runs
    // with interrupts masked (its RX self-test polls before RX IRQs are armed);
    // publishes the console char `DeviceNode` for `/dev/console`.
    nitrox_kernel::drivers::console::init();

    // Establish the BSP's per-CPU identity (dense logical index 0) **before**
    // the scheduler starts using `current_cpu()` and — critically — before any
    // AP is online (`bring_up_aps`), so this runs while the boot thread is still
    // pinned to the BSP. Doing it later (e.g. from `run_first_userspace`, after
    // AP bring-up) is unsound: by then the boot thread can migrate onto an AP,
    // and this `wrmsr(IA32_TSC_AUX, 0)` would overwrite *that* CPU's dense index
    // with 0, aliasing it onto the BSP's per-CPU scheduler slots (`current[0]` /
    // `idle[0]`) — a slot-sharing collision. Each AP sets its own index from
    // hardware in `arch::adopt_dense_index` at bring-up.
    arch::Smp::init_this_cpu(0);
    kprintln!(
        "smp: cpu {} online (RDTSCP/TSC_AUX), {} of max {}",
        arch::Smp::current_cpu(),
        arch::Smp::cpu_count(),
        arch::MAX_CPUS,
    );

    // Bring up the preemptive scheduler: initialise it and arm preemption (periodic
    // timer + IF=1). Must precede AP bring-up (APs pull from the runqueue) and any
    // userspace thread. See `docs/architecture/overview.md` § Scheduling.
    sched_bringup();

    // Boot self-tests (pre-SMP): scheduler round-robin + classes, demand paging, DMA.
    #[cfg(feature = "selftest")]
    boot_selftest::pre_smp();

    // Bring up the application processors (Limine started + parked them). After
    // this, runnable threads can be scheduled on any CPU; the BSP spawns init
    // below, and any online CPU may pick it up. Each AP logs `cpu N online (AP)`
    // from its own entry, proving it executes kernel code on the AP.
    bring_up_aps();

    // Boot self-tests (post-SMP): work distribution across the APs + CPU affinity.
    #[cfg(feature = "selftest")]
    boot_selftest::post_smp();

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

/// Bring up the preemptive scheduler: initialise it, then arm preemption (program the
/// periodic tick and raise IF). Order matters — the period is set before delivery is
/// enabled; the IDT timer stub (`Cpu::init_tables`) and the calibrated timer
/// (`Timer::init`) are already live. Must run before `bring_up_aps` (the APs pull from
/// the runqueue) and before any userspace thread. (The former `run_scheduler_demo`
/// folded a busy-worker demo into this; that demo now lives in `boot_selftest`.)
fn sched_bringup() {
    if sched::init().is_err() {
        kprintln!("sched: init failed — halting");
        return;
    }
    // SAFETY: ring 0, single CPU, once; the IDT + calibrated timer are live.
    unsafe {
        arch::Timer::start_periodic(sched::TICK_NS);
        arch::Cpu::interrupts_enable();
    }
    kprintln!(
        "preemption armed (IF=1, {} Hz tick)",
        1_000_000_000 / sched::TICK_NS
    );
}

/// Count of application processors that have finished bring-up (each AP bumps
/// this near the end of [`ap_entry`]); the BSP waits on it in [`bring_up_aps`].
static AP_ONLINE: AtomicU32 = AtomicU32::new(0);

/// Entry point for an application processor. Limine jumps each parked AP here
/// (with a `*const SmpInfo` in `RDI`) once [`bring_up_aps`] writes its
/// `goto_address`. Runs entirely on the AP: it adopts its dense CPU index by
/// matching its hardware APIC id against the map the BSP built
/// ([`arch::adopt_dense_index`]), brings up its per-CPU arch state + timer, then
/// retires into the scheduler. Never returns.
extern "C" fn ap_entry(_info: *const SmpInfo) -> ! {
    // 1. Identity first — the per-CPU GDT/TSS, syscall block, and scheduler slots
    //    all index off `current_cpu()`. Adopt our dense index by matching our own
    //    hardware APIC id against the map the BSP populated before launching us
    //    (not a handed-off value that could be stale/colliding). Unique by
    //    construction: only the BSP's APIC id maps to 0, and each AP to its own
    //    non-zero index — so no core can share another's per-CPU slots.
    let idx = match arch::adopt_dense_index() {
        Some(i) => i,
        None => {
            // Our APIC id was never bound — a bring-up bug. Running with a
            // default/guessed index would collide with another core's GDT/TSS/
            // scheduler slots (the migration hazard). Park this core safely
            // instead; the rest of the system continues without it.
            arch::Cpu::halt_loop();
        }
    };
    // 2. Per-CPU arch bring-up: GDT/TSS, the shared IDT, NX/SMEP/SMAP, x2APIC, the
    //    syscall MSRs (`KERNEL_GS_BASE` → this CPU's block).
    arch::ap_cpu_init();
    // 3. Arm this CPU's periodic tick (the BSP-calibrated frequency; IF still 0).
    // SAFETY: ring 0; this CPU's x2APIC + IDT are now live.
    unsafe {
        arch::Timer::start_periodic(sched::TICK_NS);
    }
    AP_ONLINE.fetch_add(1, Ordering::Release);
    kprintln!("smp: cpu {} online (AP)", idx);

    // 4. Retire into the scheduler (enables interrupts; diverges).
    sched::ap_run();
}

/// Start every application processor Limine reports, assigning each a **dense**
/// logical index (BSP = 0; APs 1, 2, …, capped at `MAX_CPUS`) passed via
/// `extra_argument`, then waiting until all are online. No-op if Limine reports no
/// SMP support. The BSP must already have the scheduler, IDT, APIC, and timer up.
fn bring_up_aps() {
    // SAFETY: `static mut` Limine request; Limine wrote `response` before `_start`,
    // and we read it single-threaded here (no AP is running yet).
    let resp = unsafe { SMP_REQUEST.response };
    if resp.is_null() {
        kprintln!("smp: no Limine SMP response — staying single-CPU");
        return;
    }
    // SAFETY: non-null Limine response, valid for 'static.
    let resp = unsafe { &*resp };
    let total = resp.cpu_count;
    let bsp = resp.bsp_lapic_id;
    let cap = arch::MAX_CPUS as u32;

    // Bind the BSP's own dense index (0) to its APIC id so the map is complete and
    // no AP can be assigned index 0. Each core later adopts *its own* index by
    // matching its hardware APIC id (`arch::adopt_dense_index`), so indices are
    // unique by construction — a core can never share another's per-CPU slots.
    arch::bind_cpu_identity(0, bsp);

    let mut next_idx: u32 = 1; // 0 is the BSP.
    for i in 0..total {
        // SAFETY: `cpus` points at `cpu_count` valid `*mut SmpInfo`.
        let info_ptr: *mut SmpInfo = unsafe { *resp.cpus.add(i as usize) };
        // SAFETY: a valid Limine `SmpInfo` for the lifetime of the kernel.
        let info = unsafe { &*info_ptr };
        if info.lapic_id == bsp {
            continue; // the boot processor is already running.
        }
        if next_idx >= cap {
            kprintln!("smp: more CPUs than MAX_CPUS={} — leaving extras parked", cap);
            break;
        }
        let idx = next_idx;
        next_idx += 1;
        // Bind this AP's dense index to its APIC id *before* launching it, so the
        // AP finds its (own) entry when it adopts its index. The AP no longer
        // trusts a handed-off `extra_argument`; it derives its index from hardware.
        arch::bind_cpu_identity(idx, info.lapic_id);
        // Launch the parked AP: the release store to `goto_address` makes it jump
        // to `ap_entry` (and pairs with the AP's acquire of the identity map).
        info.goto_address
            .store(ap_entry as *const () as u64, Ordering::Release);
    }

    let launched = next_idx - 1;
    if launched == 0 {
        return;
    }
    // Spin (bounded) until every launched AP reports online.
    let mut spins: u64 = 0;
    while AP_ONLINE.load(Ordering::Acquire) < launched {
        core::hint::spin_loop();
        spins += 1;
        if spins > 2_000_000_000 {
            kprintln!(
                "smp: WARNING — only {}/{} APs online after wait cap",
                AP_ONLINE.load(Ordering::Acquire),
                launched
            );
            return;
        }
    }
    kprintln!("smp: {} CPU(s) online (1 BSP + {} AP)", launched + 1, launched);
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
/// loaded from the **initramfs** (`/sbin/init`) — the real-OS model (the bootloader
/// hands the kernel an initramfs; the kernel loads init from it), retiring the former
/// kernel-embedded copy. Every later program is spawned from a path (see the
/// path-based-spawn slice).
fn run_first_userspace() {
    use mm::addr_space::AddressSpace;
    use mm::elf::load_elf;
    use nitrox_kernel::handle::global;
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::libkern::handle::{KObjectType, Rights};
    use nitrox_kernel::object::kernel_server::KernelServerId;
    use nitrox_kernel::object::{
        Namespace, NotificationChannel, ObjectRef, Process,
    };

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
    // Load `/sbin/init` from the initramfs CPIO (a contiguous `&[u8]` into the
    // HHDM-mapped Limine module — `init_initramfs` ran in `kmain` before this).
    let init_bytes = match nitrox_kernel::initramfs::blob()
        .and_then(|blob| nitrox_kernel::initramfs::lookup(blob, b"sbin/init"))
    {
        Some(bytes) => bytes,
        None => {
            kprintln!("init: /sbin/init not found in initramfs — halting");
            return;
        }
    };
    let info = match load_elf(&aspace, init_bytes) {
        Ok(i) => i,
        Err(e) => {
            kprintln!("init: ELF load failed: {:?}", e);
            return;
        }
    };

    // Build the parent process (pid 1 = init; it is the root — no `parent_notif`).
    // The **capability bootstrap**: init holds the full syscap set, and all authority
    // in the system traces to this initial kernel grant (docs/architecture/syscaps.md).
    let mut proc_box = match Process::try_new_user(1, aspace, nitrox_kernel::libkern::SysCaps::all())
    {
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

    // `/dev/console` — the serial console (a char `DeviceNode`); the caller reads
    // keyboard input with `sys_io_submit(Read)`. Input-only in Phase 2, so the
    // binding grants `READ` + the generic management band (DUPLICATE/INSPECT so a
    // client can `stat` it; TRANSFER so init can hand it to eshell at spawn).
    let console_binding_rights =
        Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/console", KernelServerId::Console, console_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/console failed");
        return;
    }

    // `/dev/log` — the kernel log ring, served as a read-only `MemoryObject`
    // snapshot (`cat /dev/log` = dmesg). The caller maps + stats it, so the binding
    // grants `MAP_READ` + the generic management band (INSPECT for `stat`).
    let log_binding_rights =
        Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/log", KernelServerId::Log, log_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/log failed");
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

    // `/dev/disk/by-partuuid/<uuid>` + `/dev/disk/by-partlabel/<label>` — stable
    // direct-handle bindings for each GPT partition the drivers discovered (the
    // content-derived names `init.toml` mount specs reference). Read-only.
    nitrox_kernel::drivers::gpt::bind_partition_names(&ns);

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
    let status = b"PHASE 3: SERVICE ECOSYSTEM";
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
    // Under the integration-test build, a kernel panic is a test failure: end the
    // QEMU run with the fail verdict so the runner reports it (instead of hanging
    // until the wall-clock timeout). `0x11` → QEMU exit 35 → the runner maps to fail.
    #[cfg(feature = "test-harness")]
    arch::debug_exit(0x11);
    #[cfg(not(feature = "test-harness"))]
    arch::Cpu::halt_loop()
}
