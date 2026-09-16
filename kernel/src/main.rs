//! Nitrox kernel entry point.
//!
//! Boot path:
//!   1. UEFI firmware loads Limine from the ESP.
//!   2. Limine parses our ELF, locates our request statics (the
//!      `.limine_requests` bracket below), sets up long mode + paging +
//!      a framebuffer, and jumps to [`_start`].
//!   3. We verify the bootloader honoured base revision 6, install the kernel's
//!      GDT/TSS/IDT, put the console on the framebuffer and COM1, and bring the
//!      machine up in the order `kernel_main` spells out — memory, platform
//!      discovery, interrupts and timers, devices, the scheduler and the APs —
//!      before loading `init` from the initramfs as pid 1.
//!
//! The boot thread then retires into the idle thread; it does not return to
//! [`_start`], whose [`arch::Cpu::halt_loop`] is only reached by an early
//! failure. `docs/architecture/boot-flow.md` is the long form.

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
use nitrox_kernel::arch::memory_types::{ArchMemoryTypes, Policy};
use nitrox_kernel::arch::paging::ArchPaging;
use nitrox_kernel::arch::platform::ArchPlatform;
use nitrox_kernel::arch::smp::ArchSmp;
use nitrox_kernel::arch::timer::ArchTimer;
use nitrox_kernel::dpc;
use nitrox_kernel::fbcon;
use nitrox_kernel::framebuffer;
use nitrox_kernel::kprintln;
use nitrox_kernel::libkern::printable::{Printable, c_bytes};
use nitrox_kernel::limine::{
    BaseRevision, BootloaderInfoRequest, DateAtBootRequest, ExecutableCmdlineRequest,
    FirmwareTypeRequest, FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest,
    Framebuffer, RequestsEndMarker, RequestsStartMarker, SmpInfo, SmpRequest,
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

/// The Limine base revision this kernel is written against.
const BASE_REVISION_WANTED: u64 = 6;

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new(BASE_REVISION_WANTED);

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

// The handoff facts a boot reports and nothing else reads (Phase 5 Part D.1): which bootloader,
// on which firmware, on what date, with which command line. `static mut` — Limine writes
// `response`.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut BOOTLOADER_INFO_REQUEST: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut FIRMWARE_TYPE_REQUEST: FirmwareTypeRequest = FirmwareTypeRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut DATE_AT_BOOT_REQUEST: DateAtBootRequest = DateAtBootRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut CMDLINE_REQUEST: ExecutableCmdlineRequest = ExecutableCmdlineRequest::new();

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

    // Install the architecture's CPU control tables (on x86_64: GDT + TSS,
    // then IDT). The ordering dependency between them lives in the arch
    // layer, not here. **First**, because it prints nothing and needs nothing, and
    // everything after it can fault: with the IDT live, a bad framebuffer descriptor below
    // produces a register dump rather than a silent triple fault (PR #296 review).
    arch::Cpu::init_tables();

    // The screen, then serial — both need nothing but what Limine handed over, and
    // everything after them can report its progress and its failures to both. The screen goes
    // first so that the first line is on it: on a machine with no serial port it is the only
    // place a boot that dies from here on can say why (Phase 5 Part B).
    let console = limine_framebuffer().and_then(|fb| {
        // SAFETY: Limine's live descriptor; it maps the framebuffer into the higher half before
        // jumping here and the kernel never unmaps it.
        unsafe { fbcon::init(fb) }.ok_or("the framebuffer is not a 32-bit layout the console can draw")
    });
    arch::serial::init();
    kprintln!("Nitrox kernel — diagnostics online");
    match console {
        Ok(g) => {
            kprintln!("fbcon: on the framebuffer, {}x{} cells at scale {}", g.cols, g.rows, g.scale)
        }
        Err(why) => kprintln!("fbcon: no console on screen — {why}"),
    }
    kprintln!("CPU tables installed (GDT/TSS/IDT)");

    // What was handed over and what this processor is (Phase 5 Part D.1), before anything that
    // can fail on an unfamiliar machine: a boot that stops at the next step has already said
    // what it stopped on. Both only read — Limine's responses, and CPUID.
    let cmdline = log_handoff();
    arch::Cpu::log_identity();
    let flags = parse_cmdline(cmdline);
    // Kept whole for `/proc/cmdline`: the words this kernel does not act on are userspace's.
    nitrox_kernel::cmdline::record(cmdline);

    // Bring up the physical-memory buddy allocator and the slab on top of
    // it. This walks Limine's memory map and pokes the allocator — a likely
    // place for a bug to fault — so the IDT is live before we reach it (as it is
    // for the framebuffer walk above). Returns false if Limine didn't populate a
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
    // The first gate hold (`fbcon-gate` only): the lines so far — the first, with its em dash,
    // through the timer — stay on screen for a second so `check-fbcon` reads them every run.
    // After the timer, because the hold needs a calibrated clock.
    #[cfg(feature = "fbcon-gate")]
    fbcon::hold_for_gate();

    // Anchor the wall clock from the hardware RTC, now that the monotonic source
    // it offsets from is calibrated. Read **once**: every later `CLOCK_REALTIME`
    // is monotonic + this offset, so time-of-day advances smoothly and cannot
    // step backwards. A machine with no readable RTC simply leaves the clock
    // unset — `CLOCK_REALTIME` keeps reporting `Unsupported` rather than
    // pretending it is 1970.
    match nitrox_kernel::clock::init() {
        Some(epoch) => kprintln!("wall clock: anchored at {} (Unix epoch seconds, UTC)", epoch),
        None => kprintln!("wall clock: no readable RTC — CLOCK_REALTIME stays unsupported"),
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

    // Milestone 3 Part A: the i8042. Same constraints as the console above and for a
    // sharper reason — bring-up is polled precisely so every controller response is
    // consumed here, with interrupts masked, before a byte can reach the scancode decoder.
    // `0xAA` (self-test passed) is byte-identical to a Left Shift release, so arming first
    // would deliver a phantom Shift on every boot.
    nitrox_kernel::drivers::ps2::init();

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

    // Record the display aperture **before** userspace exists, because
    // `run_first_userspace` binds `/dev/framebuffer` into init's namespace and init may
    // look it up as soon as it is scheduled. An earlier version folded this into the boot
    // screen's drawing, which ran *after* this line — the binding resolved to "no aperture
    // recorded" every time, and the boot still passed because the demo is non-fatal.
    record_framebuffer();

    // The hardware report, on a boot whose command line asked for one (Phase 5 Part D.3): here,
    // because the drivers have bound, every CPU is up or failed to be, and the framebuffer is
    // recorded — every fact the report shows exists — and nothing of userspace does yet.
    if let Some(page_wait_secs) = flags.hwreport {
        nitrox_kernel::report::run(page_wait_secs);
    }

    run_first_userspace();

    // Retire the boot thread into the idle thread. We must NOT fall through to `_start`'s
    // `halt_loop` (it `cli`s, which would freeze preemption): `exit` switches to the idle
    // thread, which `hlt`s with interrupts enabled so the periodic tick keeps running.
    //
    // (A boot screen used to be drawn here, clearing the whole framebuffer with nothing
    // synchronising it against `display-selftest`'s first frame. The console that replaced it
    // stops drawing under its own lock when the aperture is handed out, which is that
    // synchronisation.)
    //
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
            // Our APIC id was never bound — a bring-up bug, and now a **fatal** one.
            //
            // Running with a default/guessed index would collide with another core's
            // GDT/TSS/scheduler slots (the migration hazard), so this core cannot
            // continue either way. What changed 2026-08-19 is that the machine no
            // longer continues without it: a CPU we intended to run and failed to
            // bring up means the topology does not match what the kernel believes,
            // and the kernel stops rather than running a configuration nobody asked
            // for. The decision is in `docs/decision-log.md`, 2026-08-19.
            //
            // **This core cannot be the one that stops the machine**, for two
            // reasons, so it only diagnoses and the BSP does the killing:
            //
            //   * `ap_cpu_init` has not run, so this core has no local APIC and
            //     cannot send an IPI;
            //   * with no identity, `this_cpu()` reads the `IA32_TSC_AUX` reset
            //     default and reports **0**, so anything touching per-CPU state —
            //     including `kprintln!`, whose `IrqSpinLock` raises
            //     `PREEMPT_OFF[this_cpu()]` — would corrupt the *BSP's* slots. That
            //     is the same collision this branch exists to avoid, reached from
            //     the other side.
            //
            // So: the unsynchronised emergency writer, which is port I/O and touches
            // neither. Printing at all is new — this branch used to halt silently,
            // and the only signal was the BSP's spin-timeout roughly two billion
            // iterations later, by which point the reason was gone.
            //
            // **Raw bytes, not `writeln!`.** `<SerialPort as fmt::Write>::write_str`
            // tees into the kernel log ring *first* — `klog::push` takes `KLOG`, an
            // `IrqSpinLock`, whose `lockrank::acquired` indexes `FLOOR[cpu]`/`DEPTH[cpu]`
            // by `this_cpu()`. On this core that is 0, the BSP's slots, and the BSP is
            // concurrently spinning in `bring_up_aps`. So the formatted writer touches
            // exactly the per-CPU state this branch exists to stay out of, and the rank
            // tracker is live in every image the project boots (`cmd_build` builds dev,
            // not release). `write_byte` is the port I/O on its own.
            //
            // This is the same class as PR #198's blocking finding — an unbound AP
            // reporting index 0 and clobbering the BSP's — which `leave_online` guards
            // with `identity_bound()`. Found in review of PR #214; the first version of
            // this branch used `writeln!` and claimed it touched no per-CPU state.
            //
            // No APIC id in the message: reading it needs `hw_apic_id`, which is
            // arch-internal and not on the neutral `ArchSmp` trait, and widening that
            // trait to name one core in one diagnostic is not the trade. The BSP's panic
            // says how many failed; this says why one of them did.
            let w = arch::serial::emergency_writer();
            for &b in b"\nsmp: FATAL - an AP's hardware APIC id has no bound dense index \
(bring-up bug); halting this core, the BSP will stop the machine\n"
            {
                if b == b'\n' {
                    w.write_byte(b'\r');
                }
                w.write_byte(b);
            }
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
    // 4. Build this CPU's scheduler context — and only then report it online.
    //
    // **The order matters.** `ap_init` was inside `ap_run` below, which runs *after* this
    // increment, so a failure left the BSP counting a CPU that never joined: it would exit
    // its wait loop, print the online count and boot on. In a `test-harness` build the
    // panic reaches `debug_exit` and the gate fails, which is the path that was measured —
    // but in a production build the panic handler prints and halts this one core, and the
    // machine boots to userspace on one fewer CPU than it was told to use. That is exactly
    // the "topology nobody chose, silently" this change exists to remove, so the commit
    // point moved rather than the diagnosis. Found in review of PR #214.
    if sched::ap_init().is_err() {
        // Out of memory building this CPU's scheduler context. Unlike the identity failure
        // above, this core can speak for itself: `ap_cpu_init` has run, so it has an
        // identity, per-CPU state and a local APIC. An allocation this small failing out of
        // a fresh boot heap says the sizing is wrong, not that memory is tight.
        panic!("smp: AP scheduler init failed (out of memory building its context)");
    }
    // **Do not join a machine that is stopping.** `stop_the_machine` targets the *online*
    // set, which this core is about to enter — so a stop that began while it was still in
    // bring-up sent it no NMI and would leave it running on a machine whose other cores are
    // halted wherever they were, several of them possibly holding `SCHED`. That is not
    // hypothetical: `bring_up_aps`' deadline panic is *about* APs missing from the online
    // set, so the stop it triggers skips exactly the cores it is complaining about, and a
    // merely-slow one would arrive here afterwards. Found in review of PR #215.
    if arch::stopping() {
        arch::Cpu::halt_loop();
    }
    AP_ONLINE.fetch_add(1, Ordering::Release);
    kprintln!("smp: cpu {} online (AP)", idx);

    // 5. Retire into the scheduler (enables interrupts; diverges).
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
    // Wait (bounded) until every launched AP reports online.
    //
    // **A wall-clock deadline, not a spin count.** The cap was `2_000_000_000`
    // iterations, which is not a duration: it means different things on KVM and TCG,
    // and on a loaded host it outlives the gates. Measured 2026-08-19 by forcing an AP
    // to fail identity adoption — the count had not expired when `test-qemu`'s 90 s
    // timeout fired, so the boot reported "likely a hang" and the panic below never
    // ran. That was tolerable while this was a warning and is not now that it is fatal:
    // failing fast is the whole point, and a fatal path that never fires is a hang.
    //
    // Five seconds is three orders of magnitude above a real bring-up (which is
    // microseconds of emulated instructions) and well inside every gate's timeout. The
    // calibrated timer is live here — `sched_bringup` runs before this and says so.
    const AP_ONLINE_DEADLINE_NS: u64 = 5_000_000_000;
    // **The deadline is only a deadline if the clock runs.** `Timer::read_ns` returns a
    // constant 0 before `Timer::init` (its base and multiplier are zeroed statics), so a boot
    // reordering that moved calibration after this point would turn the wait into an infinite
    // spin — back to "TIMED OUT after 90 s, likely a hang", the exact symptom this replaced,
    // with nothing to notice. The ordering is documented on `sched_bringup`; this makes the
    // dependency self-checking rather than a comment.
    debug_assert!(
        arch::Timer::monotonic_hz() != 0,
        "AP wait deadline needs a calibrated timer — Timer::init must run before bring_up_aps"
    );
    let deadline = arch::Timer::read_ns() + AP_ONLINE_DEADLINE_NS;
    while AP_ONLINE.load(Ordering::Acquire) < launched {
        core::hint::spin_loop();
        if arch::Timer::read_ns() > deadline {
            // **Fatal, not a warning.** A CPU we launched and that never reported in
            // means either it faulted before `ap_run` or it never reached our code at
            // all; both say something is wrong underneath, and neither leaves the
            // kernel's view of the machine matching the machine. Continuing would run
            // a topology nobody chose, silently.
            //
            // Measured 2026-08-19 by forcing one AP's identity adoption to fail, on
            // the code this replaces (a `kprintln!` and a `return`):
            //
            //   `--kvm`, which is what CI runs:
            //       smp: WARNING — only 2/3 APs online after wait cap
            //       xtask: integration tests PASSED (qemu exit 33)
            //   TCG:
            //       xtask: integration tests TIMED OUT after 90s (likely a hang)
            //
            // So **CI passed with a CPU missing**, and locally the same fault looked
            // like a hang with no diagnosis — the gate adjudicates on the exit code and
            // never reads the log.
            //
            // The only tolerated shortfall is `MAX_CPUS`, which is handled at launch
            // time above: those cores are never given a `goto_address`, never enter
            // our code, and are reported as the supported configuration limit they
            // are. See `docs/decision-log.md`, 2026-08-19.
            panic!(
                "smp: only {}/{} launched APs came online within {} ms — a CPU we \
                 intended to run failed to start",
                AP_ONLINE.load(Ordering::Acquire),
                launched,
                AP_ONLINE_DEADLINE_NS / 1_000_000
            );
        }
    }
    kprintln!("smp: {} CPU(s) online (1 BSP + {} AP)", launched + 1, launched);
}

/// Log what Limine handed over, in three lines: the bootloader, its firmware and the base
/// revision it loaded the kernel under; the HHDM offset, the firmware's date and the boot
/// entry's command line; and the memory map, summarised by kind.
///
/// Reads nothing but Limine's responses, so it runs before the allocators. A response the
/// bootloader did not provide is said, not skipped — on a new machine an absence is a fact.
///
/// Returns the command line's bytes (empty when there is none), for [`parse_cmdline`].
fn log_handoff() -> &'static [u8] {
    // SAFETY (every request read below): the statics are written by Limine before `_start` and
    // only read afterwards; reading through a raw-pointer copy stops the optimiser folding the
    // pre-Limine null. A non-null response is a valid response of its type, and its strings
    // live in bootloader-reclaimable memory, which this kernel never reclaims.
    let info = unsafe { (&raw const BOOTLOADER_INFO_REQUEST).read().response };
    let (name, version) = if info.is_null() {
        (&b"unidentified bootloader"[..], &b""[..])
    } else {
        // SAFETY: as above; both strings are NUL-terminated.
        unsafe { (c_bytes((*info).name, 64), c_bytes((*info).version, 64)) }
    };
    // SAFETY: as above.
    let firmware = unsafe { (&raw const FIRMWARE_TYPE_REQUEST).read().response };
    let firmware = if firmware.is_null() {
        "unreported firmware"
    } else {
        // SAFETY: as above.
        nitrox_kernel::handoff::firmware_name(unsafe { (*firmware).firmware_type })
    };
    match BASE_REVISION.loaded() {
        Some(rev) => kprintln!(
            "boot: {} {} on {}, base revision {}",
            Printable(name),
            Printable(version),
            firmware,
            rev
        ),
        None => kprintln!(
            "boot: {} {} on {}, base revision {} (loaded revision unreported)",
            Printable(name),
            Printable(version),
            firmware,
            BASE_REVISION_WANTED
        ),
    }

    // SAFETY: as above.
    let hhdm = unsafe { (&raw const HHDM_REQUEST).read().response };
    // SAFETY: as above — a non-null response Limine wrote.
    let hhdm = if hhdm.is_null() { 0 } else { unsafe { (*hhdm).offset } };
    // SAFETY: as above.
    let date = unsafe { (&raw const DATE_AT_BOOT_REQUEST).read().response };
    // SAFETY: as above.
    let date = (!date.is_null()).then(|| unsafe { (*date).timestamp });
    // SAFETY: as above.
    let cmdline = unsafe { (&raw const CMDLINE_REQUEST).read().response };
    let cmdline = if cmdline.is_null() {
        None
    } else {
        // SAFETY: as above; the command line is NUL-terminated.
        Some(unsafe { c_bytes((*cmdline).cmdline, CMDLINE_MAX) })
    };
    match (date, cmdline) {
        (Some(d), Some(c)) => {
            kprintln!("boot: HHDM {:#x}, date {}, cmdline \"{}\"", hhdm, d, CmdlineShown(c))
        }
        (None, Some(c)) => {
            kprintln!("boot: HHDM {:#x}, date unreported, cmdline \"{}\"", hhdm, CmdlineShown(c))
        }
        (Some(d), None) => kprintln!("boot: HHDM {:#x}, date {}, cmdline unreported", hhdm, d),
        (None, None) => kprintln!("boot: HHDM {:#x}, date unreported, cmdline unreported", hhdm),
    }

    // SAFETY: as above.
    let memmap = unsafe { (&raw const MEMMAP_REQUEST).read().response };
    if memmap.is_null() {
        kprintln!("memmap: no Limine memory map");
        return cmdline.unwrap_or(&[]);
    }
    // SAFETY: as above.
    let memmap = unsafe { &*memmap };
    let mut tally = nitrox_kernel::handoff::MemoryTally::default();
    for i in 0..memmap.entry_count as usize {
        // SAFETY: `entries` is an array of `entry_count` pointers to valid entries.
        let e = unsafe { &**memmap.entries.add(i) };
        tally.add(e.kind, e.length);
    }
    kprintln!("memmap: {}", tally);
    cmdline.unwrap_or(&[])
}

/// The longest command line read. Limine sets no limit; a boot-menu entry is a line of text.
const CMDLINE_MAX: usize = 1024;

/// Parse the boot entry's command line, logging what it asks for and each word it ignores. A
/// command line never stops a boot (see `nitrox_kernel::cmdline`).
fn parse_cmdline(line: &'static [u8]) -> nitrox_kernel::cmdline::Flags {
    use nitrox_kernel::cmdline::{self, Ignored};
    let flags = cmdline::parse(line, |ignored| match ignored {
        // **Not "ignored": passed on.** The kernel acts on the words it knows and serves the
        // whole line at `/proc/cmdline`, so a word it does not recognise may still be somebody's
        // — `install` is `session-mgr`'s (Phase 5 Part H.1). Saying "ignoring" of a word that
        // decides what the machine does would be the log lying about the boot.
        Ignored::Unknown(word) => kprintln!(
            "cmdline: \"{}\" is not a kernel flag; userspace reads it at /proc/cmdline",
            Printable(word)
        ),
        Ignored::BadValue(word) => kprintln!(
            "cmdline: \"{}\" has no readable number of seconds — using {}",
            Printable(word),
            cmdline::HWREPORT_DEFAULT_SECS
        ),
    });
    if let Some(secs) = flags.hwreport {
        kprintln!("cmdline: hardware report — each page waits up to {} s for a key", secs);
    }
    flags
}

/// A command line as a log line shows it: [`Printable`], except that an empty one stays empty
/// rather than printing `-`, since it is quoted and `""` already says so.
struct CmdlineShown<'a>(&'a [u8]);

impl core::fmt::Display for CmdlineShown<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.0.is_empty() { Ok(()) } else { write!(f, "{}", Printable(self.0)) }
    }
}

/// Limine's framebuffer descriptor, or why there is none.
fn limine_framebuffer() -> Result<&'static Framebuffer, &'static str> {
    // SAFETY: `FRAMEBUFFER_REQUEST.response` is written by Limine before jumping to
    // `_start`; the kernel only ever reads it.
    let response = unsafe { (&raw const FRAMEBUFFER_REQUEST).read().response };
    if response.is_null() {
        return Err("no Limine framebuffer response");
    }
    // SAFETY: a non-null response pointer guarantees a valid `FramebufferResponse`.
    let response = unsafe { &*response };
    if response.framebuffer_count == 0 || response.framebuffers.is_null() {
        return Err("Limine reported no framebuffer");
    }
    // SAFETY: the array is dense; slot 0 is present when `framebuffer_count > 0`.
    let fb_ptr = unsafe { *response.framebuffers };
    if fb_ptr.is_null() {
        return Err("Limine reported no framebuffer");
    }
    // SAFETY: Limine's descriptors stay valid until bootloader-reclaimable memory is
    // reclaimed, which this kernel never does.
    Ok(unsafe { &*fb_ptr })
}

/// Capture Limine's framebuffer as a userspace-mappable aperture.
///
/// Must run before `run_first_userspace` binds `/dev/framebuffer`. Requires the HHDM, since
/// Limine reports the framebuffer at a higher-half virtual address and a `MemoryObject` needs
/// the physical base.
/// Whether this CPU has a cache-attribute table, recorded by `paging_init` for
/// [`record_framebuffer`] to read: without one there is no write-combining to ask for.
///
/// A static rather than a second `CPUID` because the answer is the one the boot already acted on —
/// two reads could disagree only if they disagreed about the same CPU, which would be worse.
static ATTRIBUTE_TABLE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// See [`ATTRIBUTE_TABLE`].
fn policy_installed() -> bool {
    ATTRIBUTE_TABLE.load(core::sync::atomic::Ordering::Relaxed)
}

fn record_framebuffer() {
    let fb = match limine_framebuffer() {
        Ok(fb) => fb,
        Err(why) => {
            kprintln!("framebuffer: {why} — /dev/framebuffer will be unavailable");
            return;
        }
    };

    // SAFETY: `fb` is Limine's live descriptor and `hhdm_offset()` is the offset Limine
    // reported for this boot, so `address - offset` is the aperture's physical base.
    // **The aperture is a framebuffer, so it is write-combining** (Phase 5 Part G.3) — said once,
    // here, and read by everything that maps it: the `MemoryObject` `/dev/framebuffer` mints, and
    // the measurement below.
    //
    // **Unless this CPU has no attribute table to name it in**, in which case the bit that would
    // select write-combining is reserved, and a mapping that set it would fault rather than run
    // slowly (PR #306 review, optional 5).
    let caching = match policy_installed() {
        true => nitrox_kernel::mm::Caching::WriteCombining,
        false => nitrox_kernel::mm::Caching::Normal,
    };
    if unsafe { framebuffer::record_aperture(fb, nitrox_kernel::mm::heap::hhdm_offset(), caching) } {
        // **The padding, as a number** (Phase 5 Part D.1): the bytes each row carries past its
        // last pixel. The laptop's is 40, and a reader should not have to subtract to see it.
        let padding = fb.pitch as i64 - (fb.width * (fb.bpp as u64).div_ceil(8)) as i64;
        kprintln!(
            "framebuffer: {}x{} pitch {} padding {} bpp {} — /dev/framebuffer available",
            fb.width,
            fb.height,
            fb.pitch,
            padding,
            fb.bpp
        );
        report_framebuffer_cost(fb.address as u64);
    } else {
        kprintln!("framebuffer: unsupported depth ({} bpp) — /dev/framebuffer unavailable", fb.bpp);
    }
}

/// What writing to the screen costs on this machine, and why (Phase 5 Part G's measurement).
///
/// **Four facts, and the fourth is the one the first boot did not expect.** What the firmware set
/// the caching of physical memory to; what that makes the framebuffer's own address; how long a
/// full-screen fill takes through the mapping the console draws on; and how long the same fill
/// takes through a mapping made the way **userspace's** is — built by `page_flags_for` from the
/// aperture's own answer, which is what a `/dev/framebuffer` mapping is built from.
///
/// The laptop's first measurement had only the third of those, and it came back at GiB/s over
/// memory the range registers call uncacheable. That is not possible for an uncached write, so the
/// cost is not a property of the memory: it is a property of the page table entry, and the two
/// mappings of this framebuffer did not agree until Part G made them. The pair of timings is what
/// says so — 54 MiB/s against 2924 before, 2451 against 2931 after.
///
/// **A second mapping of device memory with a different type is exactly the aliasing the manuals
/// warn about.** Since Part G the two agree — the aperture's answer reaches both — so this maps
/// the same attribute the console's mapping has, holds it for one fill, and unmaps it.
fn report_framebuffer_cost(console_virt: u64) {
    // SAFETY: ring 0, during boot, with the console up.
    unsafe { arch::MemoryTypes::log_configuration() };
    let Some((phys, info, caching)) = framebuffer::aperture() else { return };
    // SAFETY: ring 0; reads configuration registers only.
    match unsafe { arch::MemoryTypes::at(phys.as_u64()) } {
        Some(kind) => kprintln!(
            "framebuffer: {:#x} is {} memory, and a plain mapping of it is write-back{}",
            phys.as_u64(),
            kind.name(),
            match kind {
                nitrox_kernel::arch::memory_types::MemoryType::WriteBack => "",
                _ => " — the stronger of the two wins, so a plain mapping pays that",
            }
        ),
        None => kprintln!(
            "framebuffer: {:#x} has no memory type of its own; the page table decides",
            phys.as_u64()
        ),
    }

    // A second mapping of the aperture, made as a `/dev/framebuffer` mapping is: the same
    // translation, from the same recorded answer. `map_mmio` would not do — it forces uncached, a
    // third thing neither mapping is.
    let bytes = info.pitch as usize * info.height as usize;
    let pages = (bytes as u64).div_ceil(nitrox_kernel::mm::PAGE_SIZE as u64);
    let plain = plain_mapping(phys, pages, caching);
    // SAFETY: `plain`, when mapped, addresses the same framebuffer for the length of the call;
    // the console is the kernel's and no userspace exists yet.
    let measured = unsafe { fbcon::time_full_fills(plain.map(|v| v.as_u64() as *mut u8)) };
    // **What each mapping asks for**, which is the fact the timings are evidence of. The
    // bootloader made the console's; `page_flags_for` makes the other's, as it makes every
    // `/dev/framebuffer` mapping's.
    let asks = |what: &str, virt: u64| {
        // SAFETY: ring 0; walks the active page tables and reads a configuration register.
        match unsafe { arch::MemoryTypes::of_mapping(virt) } {
            Some(kind) => kprintln!("framebuffer: {what} asks for {}", kind.name()),
            None => kprintln!("framebuffer: {what} could not be read"),
        }
    };
    asks("the console's mapping", console_virt);
    if let Some(v) = plain {
        asks("a mapping made as userspace's is", v.as_u64());
    }
    if let Some(v) = plain {
        // SAFETY: undoing this function's own mapping of pages nothing else refers to. The vmap
        // range is never reused, so the address is not handed out again.
        //
        // **Before the early return below**, not after it: `time_full_fills` answers `None` when
        // the console has no screen — a framebuffer `record_aperture` accepted and `Screen::new`
        // refused, say — and a write-back alias of the whole aperture left mapped for the life of
        // the boot is the one thing this measurement must not leave behind (PR #305 review,
        // optional 5).
        unsafe { unmap_plain(v, pages) };
    }
    let Some(f) = measured else { return };
    let rate = |ns: u64| -> u64 {
        if ns == 0 { 0 } else { (f.bytes as u64) * 1_000_000_000 / ns / (1024 * 1024) }
    };
    kprintln!(
        "framebuffer: a full-screen fill of {} KiB took {} us ({} MiB/s) through the console's mapping",
        f.bytes / 1024,
        f.own_ns / 1000,
        rate(f.own_ns)
    );
    match f.other_ns {
        Some(ns) => kprintln!(
            "framebuffer: the same fill took {} us ({} MiB/s) through a mapping made as userspace's is",
            ns / 1000,
            rate(ns)
        ),
        None => kprintln!("framebuffer: no second mapping to compare — the vmap is exhausted"),
    }
}

/// Map `pages` of the framebuffer at `phys` with the flags a `/dev/framebuffer` mapping gets —
/// which is what makes the second timing worth having: it is not a model of userspace's mapping,
/// it is one. `None` if the vmap or the page tables refuse, which costs the comparison and
/// nothing else.
fn plain_mapping(
    phys: nitrox_kernel::mm::PhysAddr,
    pages: u64,
    caching: nitrox_kernel::mm::Caching,
) -> Option<nitrox_kernel::mm::VirtAddr> {
    use nitrox_kernel::mm::vmm::Protection;
    use nitrox_kernel::mm::{PhysAddr, VirtAddr, addr_space, kvmap};
    // **The flags a `/dev/framebuffer` mapping gets, from the function that gives them to it.**
    // Not a second copy of that translation: the first version rebuilt the flags here, and a
    // control that deleted the real translation left this measurement — and the gate reading its
    // line — perfectly green (PR #306 review, blocking 1).
    let flags = addr_space::page_flags_for(Protection::WRITE, caching);
    let base = kvmap::vmap_alloc_pages(pages).ok()?;
    let root = arch::Paging::active_root();
    for i in 0..pages {
        let v = VirtAddr::new(base.as_u64() + i * nitrox_kernel::mm::PAGE_SIZE as u64);
        let p = PhysAddr::new(phys.as_u64() + i * nitrox_kernel::mm::PAGE_SIZE as u64);
        // SAFETY: `v` is a fresh vmap page in the shared kernel half; `p` is a frame of the
        // framebuffer aperture this kernel already maps; the flags are the ones a user mapping of
        // it gets.
        unsafe { arch::Paging::map_page(root, v, p, flags).ok()? };
        // SAFETY: `v`'s entry has just changed.
        unsafe { arch::Paging::flush_tlb_page(v) };
    }
    Some(base)
}

/// Undo [`plain_mapping`].
///
/// # Safety
/// `base .. base + pages` must be the mapping [`plain_mapping`] returned, unused from here on.
unsafe fn unmap_plain(base: nitrox_kernel::mm::VirtAddr, pages: u64) {
    use nitrox_kernel::mm::VirtAddr;
    let root = arch::Paging::active_root();
    for i in 0..pages {
        let v = VirtAddr::new(base.as_u64() + i * nitrox_kernel::mm::PAGE_SIZE as u64);
        // SAFETY: forwarded from this function's contract.
        unsafe {
            let _ = arch::Paging::unmap_page(root, v);
            arch::Paging::flush_tlb_page(v);
        }
    }
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

    // `/dev/framebuffer` — the display aperture, and its `/info` leaf. **Whoever holds
    // this binding is the compositor**: authority here is the namespace grant, not a
    // capability bit and not a registration call, exactly as for the fs-server and
    // profile-server (`docs/design/display-substrate.md` §3).
    //
    // `MAP_READ | MAP_WRITE` because a compositor writes pixels; `INSPECT` so it can
    // `stat` the object's size; `TRANSFER` so init can hand the binding onward to the
    // process that will actually drive the display. No `READ`/`WRITE` — only the `MAP_*`
    // band is valid on a `MemoryObject`, and including them would be rejected as
    // `BadRights`.
    let framebuffer_binding_rights = Rights::MAP_READ
        | Rights::MAP_WRITE
        | Rights::DUPLICATE
        | Rights::INSPECT
        | Rights::TRANSFER;
    if ns
        .bind_kernel_server(
            b"/dev/framebuffer",
            KernelServerId::Framebuffer,
            framebuffer_binding_rights,
        )
        .is_err()
    {
        // Non-fatal: a system with no usable framebuffer still boots to a serial
        // console, and every existing test path is serial. The display arm is the only
        // consumer.
        kprintln!("init: binding /dev/framebuffer failed — display unavailable");
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
    // `/proc/self/status` returns a synthesized read-only `MemoryObject` text
    // snapshot (numeric pid/tid), so its cap is the snapshot-server shape
    // (`MAP_READ` + the generic band), not a principal-object cap.
    let proc_self_status_rights =
        Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
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
        (
            &b"/proc/self/status"[..],
            KernelServerId::ProcSelfStatus,
            proc_self_status_rights,
        ),
        // `/proc/cmdline` — the line this boot was given, as text. Not under `/proc/self`: it is
        // one fact about the machine, the same for every reader. `session-mgr` looks for
        // `install` in it (Phase 5 Part H.1), and a person debugging a boot wants to see it.
        (
            &b"/proc/cmdline"[..],
            KernelServerId::ProcCmdline,
            proc_self_status_rights,
        ),
    ];
    for (path, id, rights) in proc_self_binds {
        if ns.bind_kernel_server(path, id, rights).is_err() {
            kprintln!("init: binding /proc/self/* failed");
            return;
        }
    }

    // `/proc/sched/stats` — per-CPU scheduler statistics as a read-only
    // `MemoryObject` text snapshot (the same shape as `/dev/log`): the caller
    // maps + stats it, so the binding grants `MAP_READ` + the generic management
    // band. Reachability is by namespace construction, like all of `/proc` — a
    // supervisor may omit it from a sandbox's namespace.
    let sched_stats_binding_rights =
        Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(
            b"/proc/sched/stats",
            KernelServerId::SchedStats,
            sched_stats_binding_rights,
        )
        .is_err()
    {
        kprintln!("init: binding /proc/sched/stats failed");
        return;
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
    // The binding grants `READ` + `WRITE` (the RW fs-server writes filesystem metadata via
    // `sys_io_submit` writes; the Model A data path is the kernel's) plus the generic band, and
    // **`MAP_READ` for the `<n>/info` leaf** (Phase 5 Part H.1): that leaf answers with a
    // read-only `MemoryObject`, and a lookup attenuates to the binding's rights, so without this
    // the record resolves and cannot be read. `MAP_READ` on a `DeviceNode` means nothing — a
    // device is not mappable — so this widens only what the leaf serves.
    let block_binding_rights = Rights::READ
        | Rights::WRITE
        | Rights::MAP_READ
        | Rights::DUPLICATE
        | Rights::INSPECT
        | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/blk", KernelServerId::BlockDevice, block_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/blk failed");
        return;
    }

    // `/dev/input/raw/<n>` — the i8042's keyboard and mouse nodes. Read-only (input),
    // plus the generic management band so a supervisor can `stat` one and hand it to the
    // input-server at spawn. A lookup is `NotFound` when no such device answered, which is
    // the normal case on a machine with no i8042.
    //
    // **Bound into the root namespace only.** This is the authority to read one input
    // device unfiltered — a keylogger, if it reaches the wrong process — so no session
    // namespace should ever project it (`docs/architecture/input-subsystem.md` §5).
    let raw_input_binding_rights =
        Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns
        .bind_kernel_server(b"/dev/input/raw", KernelServerId::RawInput, raw_input_binding_rights)
        .is_err()
    {
        kprintln!("init: binding /dev/input/raw failed");
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
    // Every module after the first is a disk (Phase 5 Part C — the live image's root). Recorded
    // here, while the responses are fresh; published by `drivers::probe`, once the device table and
    // the interrupt table exist.
    for index in 1..resp.module_count as usize {
        // SAFETY: `modules` points at `module_count` non-null `*mut LimineFile`s, in
        // never-reclaimed memory.
        let file = unsafe { &**resp.modules.add(index) };
        // SAFETY: Limine's `path` is a NUL-terminated string in bootloader memory the kernel never
        // reclaims.
        let path = unsafe { core::ffi::CStr::from_ptr(file.path as *const core::ffi::c_char) };
        // SAFETY: the module's `address..address + size` is HHDM-mapped, never reclaimed, and
        // nothing else uses it — this kernel reads modules through nothing but these two paths.
        if !unsafe {
            nitrox_kernel::io::ramdisk::record_module(index, file.address, file.size as usize, path.to_bytes())
        } {
            kprintln!("initramfs: module {} ignored — more modules than disks the kernel keeps", index);
        }
    }
    // SAFETY: `modules` points at an array of `module_count` `*mut LimineFile`;
    // the first is the initramfs.
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
    // Enable this CPU's FP/SIMD units before any thread is created (a thread's
    // save area is initialised at construction and restored on its first
    // switch-in). Each AP repeats this for itself in `ap_cpu_init` — CR0/CR4 and
    // the extended-state mask are per-CPU registers.
    arch::fpu_init_cpu();
    // **The cache-policy table becomes the kernel's, here** (Phase 5 Part G.1), before this CPU
    // makes any mapping that names an entry in it. Each AP does the same for itself in
    // `ap_cpu_init`: the table is per-CPU state, and CPUs that disagreed would give one page
    // different meanings depending on which core touched it.
    //
    // SAFETY: ring 0, on the boot CPU, before `kvmap::init` maps anything.
    let policy = unsafe { arch::MemoryTypes::install_policy() };
    ATTRIBUTE_TABLE.store(policy != Policy::NoTable, core::sync::atomic::Ordering::Relaxed);
    match policy {
        Policy::Unchanged => kprintln!(
            "cache policy: the kernel's table is installed; the bootloader's was the same"
        ),
        Policy::Replaced => kprintln!(
            "cache policy: the kernel's table is installed; the bootloader's DIFFERED — mappings \
             it made, including the console's, chose entries from another table"
        ),
        // Not "differed": there was nothing to differ. Said plainly because the consequence is
        // real — no attribute table means no write-combining, so the framebuffer below is
        // recorded as ordinary memory and the desktop pays the uncached price it always did.
        Policy::NoTable => kprintln!(
            "cache policy: this CPU has no attribute table — every mapping is what the range \
             registers say, and write-combining is unavailable"
        ),
    }
    kprintln!(
        "fp/simd enabled ({}-bit vectors, {} B per-thread save area)",
        arch::fpu_vector_bits(),
        arch::fpu_area_bytes()
    );
    // SAFETY: HHDM is up (init_memory ran first) and the buddy
    // allocator is live; no AS exists yet whose captured template
    // could disagree with the new PML4 entries.
    unsafe {
        mm::kvmap::init();
        arch::Paging::init_kernel_template(arch::Paging::active_root());
    }
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
    // Scan the panicking stack for kernel-text addresses — a poor man's backtrace. The
    // kernel has no unwind tables, but a `call` leaves its return address behind, so this
    // recovers the call chain (with stale-slot false positives, hence "candidates").
    //
    // **Shared with the exception dump rather than reimplemented**, which is the point: this
    // used to be its own copy of the loop with no page probe, under a `SAFETY` comment
    // claiming "reading within the current kernel stack, which is mapped". It is not — a
    // kernel stack's guard page is at the *bottom*, so scanning **up** 96 words from a
    // shallow `rsp` runs off the top into unmapped vmap. The `#PF` that follows is taken
    // *before* `debug_exit` below, so the panic that got here never delivered its verdict and
    // the run died on the 90 s timeout reporting `likely a hang` — burying the one-line
    // diagnosis under an unrelated fault dump. A syscall-path assertion is exactly the
    // shallow-stack case, so the guard added in PR #231 was landing in that hole
    // (re-review, finding 6).
    arch::dump_stack_candidates(&mut w);
    // Under the integration-test build, a kernel panic is a test failure: end the
    // QEMU run with the fail verdict so the runner reports it (instead of hanging
    // until the wall-clock timeout). `0x11` → QEMU exit 35 → the runner maps to fail.
    #[cfg(feature = "test-harness")]
    arch::debug_exit(0x11);
    // **Stop every CPU**, not just this one: a panicked kernel must not run on, and until
    // 2026-08-19 "must not run on" meant only the panicking core while the rest kept
    // scheduling on whatever it had been holding. `debug_exit` returns when no
    // `isa-debug-exit` device is attached, so this runs either way.
    //
    // Safe from any core, including one whose APIC is not up yet — `stop_the_machine` checks
    // and falls back to halting itself, which is the early-boot case `ap_entry` relies on.
    arch::Cpu::stop_the_machine()
}
