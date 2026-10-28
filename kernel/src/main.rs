//! Nitrox kernel entry point. Limine (loaded by UEFI) jumps to [`_start`] in long mode with paging
//! and a framebuffer; we check base revision 6, install the GDT/TSS/IDT, put the console on the
//! framebuffer and COM1, and bring the machine up in the order `kernel_main` spells out before
//! loading `init` as pid 1. The boot thread then retires into idle and never returns to [`_start`],
//! whose `halt_loop` is only for early failure. `docs/architecture/boot-flow.md` is the long form.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nitrox_kernel::arch;
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
    BaseRevision, BootloaderInfoRequest, DateAtBootRequest, ExecutableCmdlineRequest, FirmwareTypeRequest, Framebuffer,
    FramebufferRequest, HhdmRequest, MemoryMapRequest, ModuleRequest, RequestsEndMarker, RequestsStartMarker, SmpInfo,
    SmpRequest,
};
use nitrox_kernel::mm;
use nitrox_kernel::sched;

use core::sync::atomic::{AtomicU32, Ordering};

/// Boot-time self-tests and demos, compiled only under the `selftest` feature.
#[cfg(feature = "selftest")]
mod boot_selftest;

// Each request lives in `.limine_requests*`, kept by the linker script's `KEEP()`, so Limine finds
// it by scanning the bracketed region.
// `#[used]` also stops rustc inlining the static away before the linker sees it.

/// The Limine base revision this kernel is written against.
const BASE_REVISION_WANTED: u64 = 6;

#[used]
#[unsafe(link_section = ".limine_requests")]
static BASE_REVISION: BaseRevision = BaseRevision::new(BASE_REVISION_WANTED);

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut FRAMEBUFFER_REQUEST: FramebufferRequest = FramebufferRequest::new();

// `static mut`: Limine writes `response` after load, and a plain `static` could be constant-folded
// to its null initialiser, silently breaking `init_memory`. `BASE_REVISION` reads volatile.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut MEMMAP_REQUEST: MemoryMapRequest = MemoryMapRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests")]
static mut HHDM_REQUEST: HhdmRequest = HhdmRequest::new();

// The initramfs module (`boot/initramfs`, `MEMMAP_KERNEL_AND_MODULES`, mapped in the HHDM).
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut MODULE_REQUEST: ModuleRequest = ModuleRequest::new();

// SMP: Limine starts and parks the APs; the kernel launches each by writing its `goto_address`.
#[used]
#[unsafe(link_section = ".limine_requests")]
static mut SMP_REQUEST: SmpRequest = SmpRequest::new();

// Handoff facts a boot reports and nothing else reads: bootloader, firmware, date, command line.
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

/// ELF entry point. Limine jumps here in long mode on a 64 KiB stack. We never return: the
/// bootloader's caller frame pushed a zero return address as a tripwire.
#[unsafe(no_mangle)]
pub extern "C" fn _start() -> ! {
    kernel_main();
    arch::Cpu::halt_loop();
}

fn kernel_main() {
    if !BASE_REVISION.supported() {
        // Limine refused our protocol revision, so no framebuffer is safe to touch: just halt.
        return;
    }

    // Install the CPU control tables (x86_64: GDT + TSS, then IDT; their order lives in the arch
    // layer). First, because it prints nothing and needs nothing, and with the IDT live a bad
    // framebuffer descriptor below gives a register dump rather than a silent triple fault.
    arch::Cpu::init_tables();

    // The screen, then serial, both needing only what Limine handed over. The screen first, so a
    // machine with no serial port can still say why a boot died.
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

    // Say what was handed over and what this processor is before anything that can fail on an
    // unfamiliar machine, so a boot that stops at the next step has said what it stopped on. Both
    // only read (Limine's responses, CPUID). The build first: it is the first thing to know about a
    // machine that misbehaves, and the first line of the hardware report's first page.
    kprintln!("nitrox: built from {}", BUILD_COMMIT);
    let cmdline = log_handoff();
    arch::Cpu::log_identity();
    let flags = parse_cmdline(cmdline);
    // Kept whole for `/proc/cmdline`: the words this kernel does not act on are userspace's.
    nitrox_kernel::cmdline::record(cmdline);

    // Buddy allocator and slab. It walks Limine's memory map, a likely place to fault, so the IDT
    // is live first; false means a required Limine response is missing.
    if !init_memory() {
        kprintln!("init_memory failed — halting");
        return;
    }
    kprintln!("allocators up");

    // Paging setup (NX, and the kernel-half PML4 template every `AddressSpace::new` inherits)
    // before any address space exists, then a smoke test.
    paging_init();
    #[cfg(feature = "selftest")]
    boot_selftest::paging();

    // Record the initramfs module for `/initramfs`. Needs the HHDM; non-fatal if absent.
    init_initramfs();

    // Platform discovery from firmware tables (ACPI): the ECAM window and the interrupt routing the
    // IOAPIC step uses. Reads firmware memory through the HHDM; missing tables are logged, not
    // fatal.
    // SAFETY: ring 0, single CPU, called once during boot after the HHDM is
    // available; reads firmware-owned physical memory, no allocation.
    let _ = unsafe { arch::Platform::init() };

    // This CPU's local interrupt controller; IF stays 0, so nothing is delivered yet.
    // SAFETY: ring 0, single CPU, called once during boot after CPU
    // feature enablement and after the kernel-vmap allocator is up; IF is
    // 0, so software-enabling the controller delivers nothing.
    if unsafe { arch::Irq::init() }.is_err() {
        kprintln!("local APIC bring-up failed — halting");
        return;
    }
    kprintln!("local APIC up (x2APIC, id {})", arch::Irq::id());

    // Calibrate the TSC and the per-CPU LAPIC timer against the legacy PIT, after `Irq::init`
    // brought up the local controller. IF stays 0.
    // SAFETY: ring 0, single CPU, called once during boot after Irq::init
    // mapped the local-controller MMIO; IF=0, so arming delivers nothing.
    unsafe { arch::Timer::init() };
    kprintln!(
        "timer up: monotonic {} MHz, per-CPU timer {} MHz (clock t0={} ns)",
        arch::Timer::monotonic_hz() / 1_000_000,
        arch::Timer::timer_hz() / 1_000_000,
        arch::Timer::read_ns(),
    );
    // First gate hold (`fbcon-gate`): the lines so far stay on screen for a second so `check-fbcon`
    // reads them every run; after the timer, which the hold needs.
    #[cfg(feature = "fbcon-gate")]
    fbcon::hold_for_gate();

    // Anchor the wall clock from the RTC, read once now that the monotonic source it offsets from
    // is calibrated; every later `CLOCK_REALTIME` is monotonic + this offset. With no readable RTC
    // the clock stays unset (`Unsupported`).
    match nitrox_kernel::clock::init() {
        Some(epoch) => kprintln!("wall clock: anchored at {} (Unix epoch seconds, UTC)", epoch),
        None => kprintln!("wall clock: no readable RTC — CLOCK_REALTIME stays unsupported"),
    }

    // Reserve the DPC queue before any interrupt can enqueue: the IOAPIC self-test routes the PIT,
    // whose ISR queues a DPC. It reserves its list once (needs the allocator) and never allocates
    // again.
    if dpc::init().is_err() {
        kprintln!("DPC queue init failed — halting");
        return;
    }

    // The IOAPIC router, then an end-to-end self-test. After `Irq::init` (routed interrupts land on
    // a LAPIC); runs with the LAPIC timer masked and the scheduler off, so its interrupt-enabled
    // window fires only the PIT it routes.
    // SAFETY: ring 0, single CPU, once during boot after Irq/Timer init; the
    // ACPI MADT facts are cached (Platform::init ran).
    if unsafe { arch::IrqRouter::init() }.is_err() {
        kprintln!("interrupt router bring-up failed — halting");
        return;
    }
    #[cfg(feature = "selftest")]
    boot_selftest::irq_routing();

    // Seed the entropy CSPRNG after the timer (jitter mixing needs the clock) and before the handle
    // table, which shuffles its free list from it.
    nitrox_kernel::entropy::init();

    // The global handle table allocates eagerly (heap up) and must be live before any handle
    // syscall.
    if nitrox_kernel::handle::global::init().is_err() {
        kprintln!("global handle table init failed — halting");
        return;
    }
    kprintln!("global handle table up");

    // Enumerate hardware into the device table, after the allocators, HHDM, kvmap and
    // `Platform::init` (the ECAM regions). Discovery only: no driver claims a node here.
    nitrox_kernel::device::init();

    // Prove the async I/O spine (IRP -> DPC -> PendingOperation) on a RAM-backed device before any
    // real driver; needs the DPC queue and scheduler waitables.
    #[cfg(feature = "selftest")]
    boot_selftest::irp_spine();

    // Match Tier 1 drivers to the enumerated devices and bring up disks; the self-test reads sector
    // 0 through the real driver.
    nitrox_kernel::drivers::probe(&flags);
    #[cfg(feature = "selftest")]
    boot_selftest::storage();

    // Serial console input (COM1 RX), with interrupts masked: its RX self-test polls before RX IRQs
    // are armed. Publishes the console char `DeviceNode` for `/dev/console`.
    nitrox_kernel::drivers::console::init();

    // The i8042. Polled bring-up consumes every controller response here, with interrupts masked,
    // before a byte reaches the scancode decoder: `0xAA` (self-test passed) is byte-identical to a
    // Left Shift release, so arming first delivers a phantom Shift each boot.
    nitrox_kernel::drivers::ps2::init();

    // Establish the BSP's dense index 0 before the scheduler uses `current_cpu()` and before any AP
    // is online, while the boot thread is pinned to the BSP. Later it could migrate onto an AP, and
    // this `wrmsr(IA32_TSC_AUX, 0)` would overwrite that CPU's index, aliasing it onto the BSP's
    // per-CPU scheduler slots. Each AP sets its own in `arch::adopt_dense_index`.
    arch::Smp::init_this_cpu(0);
    kprintln!(
        "smp: cpu {} online (RDTSCP/TSC_AUX), {} of max {}",
        arch::Smp::current_cpu(),
        arch::Smp::cpu_count(),
        arch::MAX_CPUS,
    );

    // Initialise the scheduler and arm preemption (periodic timer, IF=1) before AP bring-up (APs
    // pull from the runqueue) and any userspace thread. See `docs/architecture/overview.md` §
    // Scheduling.
    sched_bringup();

    #[cfg(feature = "selftest")]
    boot_selftest::pre_smp();

    // Bring up the APs (Limine started and parked them); after this any CPU can pick up a runnable
    // thread, init included.
    bring_up_aps();

    // Drivers' threads, USB's hub and AHCI's watch, after every CPU is up: AP bring-up is the
    // scheduler's most delicate moment, and the overlap is worth less than staying out of it.
    nitrox_kernel::drivers::start();

    #[cfg(feature = "selftest")]
    boot_selftest::post_smp();

    // Record the display aperture before userspace exists: `run_first_userspace` binds
    // `/dev/framebuffer` into init's namespace, and a later recording resolves to "no aperture
    // recorded" while the boot still passes.
    record_framebuffer();

    // Wait, bounded, for the hub thread's first round, so the report below lists what is attached
    // and a device present at boot is registered before `device-mgr` replays it.
    nitrox_kernel::drivers::settle();

    // The hardware report, when the command line asked for one: the drivers have bound, every CPU
    // is up or failed to be, the framebuffer is recorded, and nothing of userspace exists yet.
    if let Some(page_wait_secs) = flags.hwreport {
        nitrox_kernel::report::run(page_wait_secs);
    }

    run_first_userspace();

    // Retire the boot thread into idle. Falling through to `_start`'s `halt_loop` would `cli` and
    // freeze preemption; `exit` switches to the idle thread, which `hlt`s with interrupts enabled
    // so the tick keeps running. The boot thread has no process, so no `ChildExited` is produced.
    sched::exit_thread(nitrox_kernel::libkern::ExitStatus {
        kind: nitrox_kernel::libkern::ExitKind::Normal as u32,
        code: 0,
    });
}

/// Bring up the preemptive scheduler: initialise it, then arm preemption (set the periodic tick,
/// then raise IF). The IDT timer stub and the calibrated timer are already live. Must run before
/// `bring_up_aps` (the APs pull from the runqueue) and before any userspace thread.
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
    kprintln!("preemption armed (IF=1, {} Hz tick)", 1_000_000_000 / sched::TICK_NS);
}

/// APs that have finished bring-up, bumped near the end of [`ap_entry`]; the BSP waits on it.
static AP_ONLINE: AtomicU32 = AtomicU32::new(0);

/// Entry point for an application processor, jumped to by Limine (`*const SmpInfo` in `RDI`) once
/// [`bring_up_aps`] writes its `goto_address`. Adopts its dense index by matching its hardware APIC
/// id ([`arch::adopt_dense_index`]), brings up its per-CPU state and timer, then retires into the
/// scheduler. Never returns.
extern "C" fn ap_entry(_info: *const SmpInfo) -> ! {
    // Identity first: the per-CPU GDT/TSS, syscall block and scheduler slots all index off
    // `current_cpu()`. Match our hardware APIC id against the map the BSP populated before
    // launching us; unique by construction, so no core shares another's per-CPU slots.
    let idx = match arch::adopt_dense_index() {
        Some(i) => i,
        None => {
            // Our APIC id was never bound: a bring-up bug, and fatal (`docs/decision-log.md`,
            // 2026-08-19). The BSP does the stopping, because this core cannot: `ap_cpu_init` has
            // not run, so it has no local APIC and cannot send an IPI, and with no identity
            // `this_cpu()` reads the `IA32_TSC_AUX` reset default, **0**, so anything touching
            // per-CPU state would corrupt the BSP's slots.
            //
            // That rules out `kprintln!` and `writeln!` too: `write_str` tees into the log ring
            // first, and `klog::push` takes `KLOG`, whose `lockrank::acquired` indexes
            // `FLOOR[cpu]`/`DEPTH[cpu]` by `this_cpu()`, again the BSP's slots while it spins in
            // `bring_up_aps`. So only the unsynchronised emergency writer's raw `write_byte`, which
            // is port I/O alone. No APIC id in the message: `hw_apic_id` is arch-internal, and
            // widening `ArchSmp` for one diagnostic is not the trade.
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
    arch::ap_cpu_init();
    // SAFETY: ring 0; this CPU's x2APIC + IDT are now live.
    unsafe {
        arch::Timer::start_periodic(sched::TICK_NS);
    }
    // Build this CPU's scheduler context, and only then report it online: a failure after the
    // increment would leave the BSP counting a CPU that never joined, and in a production build the
    // panic only halts this core, so the machine would run on one fewer CPU than it was told to
    // use.
    if sched::ap_init().is_err() {
        // Out of memory building the scheduler context. Unlike the identity failure above, this
        // core has an identity and a local APIC, so it can speak for itself; an allocation this
        // small failing says the sizing is wrong.
        panic!("smp: AP scheduler init failed (out of memory building its context)");
    }
    // Do not join a machine that is stopping. `stop_the_machine` targets the online set, which this
    // core is about to enter: a stop begun during its bring-up sent it no NMI, so it would run on
    // beside halted cores, some holding `SCHED`. The deadline panic in `bring_up_aps` is about APs
    // missing from the online set, so its stop skips exactly them.
    if arch::stopping() {
        arch::Cpu::halt_loop();
    }
    AP_ONLINE.fetch_add(1, Ordering::Release);
    kprintln!("smp: cpu {} online (AP)", idx);

    sched::ap_run();
}

/// Start every application processor Limine reports, giving each a **dense** logical index (BSP =
/// 0; APs 1, 2, ..., capped at `MAX_CPUS`), then wait until all are online. No-op if Limine reports
/// no SMP support. The BSP must already have the scheduler, IDT, APIC and timer up.
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

    // Bind the BSP's dense index (0) to its APIC id so the map is complete and no AP is assigned 0.
    // Each core adopts its own index by matching its hardware APIC id.
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
        // Bind the AP's index before launching it, so it finds its own entry when it adopts its
        // index from hardware.
        arch::bind_cpu_identity(idx, info.lapic_id);
        // The release store to `goto_address` launches the AP and pairs with its acquire of the
        // map.
        info.goto_address.store(ap_entry as *const () as u64, Ordering::Release);
    }

    let launched = next_idx - 1;
    if launched == 0 {
        return;
    }
    // Wait, bounded, until every launched AP reports online. A wall-clock deadline, not a spin
    // count: iterations mean different things on KVM and TCG, and a count had not expired when
    // `test-qemu`'s 90 s timeout fired, so the panic below never ran. Five seconds is three orders
    // of magnitude above a real bring-up and inside every gate's timeout.
    const AP_ONLINE_DEADLINE_NS: u64 = 5_000_000_000;
    // The deadline needs a running clock: `Timer::read_ns` returns 0 before `Timer::init`, so
    // calibrating later would turn this into an infinite spin.
    debug_assert!(
        arch::Timer::monotonic_hz() != 0,
        "AP wait deadline needs a calibrated timer — Timer::init must run before bring_up_aps"
    );
    let deadline = arch::Timer::read_ns() + AP_ONLINE_DEADLINE_NS;
    while AP_ONLINE.load(Ordering::Acquire) < launched {
        core::hint::spin_loop();
        if arch::Timer::read_ns() > deadline {
            // Fatal, not a warning. A launched CPU that never reported in either faulted before
            // `ap_run` or never reached our code, and continuing would run a topology nobody chose.
            // As a warning, `--kvm` CI passed with a CPU missing and TCG looked like a hang, since
            // the gate reads the exit code and not the log. The only tolerated shortfall is
            // `MAX_CPUS`, handled at launch: those cores never get a `goto_address`. See
            // `docs/decision-log.md`, 2026-08-19.
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

/// The commit this kernel was built from, as `xtask` set it (`-dirty` if changed); else `unknown`.
const BUILD_COMMIT: &str = match option_env!("NITROX_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

/// Log what Limine handed over, in three lines: the bootloader, firmware and base revision; the
/// HHDM offset, the firmware's date and the command line; and the memory map by kind.
///
/// Reads only Limine's responses, so it runs before the allocators; an absent response is said, not
/// skipped. Returns the command line's bytes (empty if none), for [`parse_cmdline`].
fn log_handoff() -> &'static [u8] {
    // SAFETY: for every request read below, the statics are written by Limine before `_start` and
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
        Some(rev) => {
            kprintln!("boot: {} {} on {}, base revision {}", Printable(name), Printable(version), firmware, rev)
        }
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
    let hhdm = if hhdm.is_null() {
        0
    } else {
        // SAFETY: as above — a non-null response Limine wrote.
        unsafe { (*hhdm).offset }
    };
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
        // Not "ignored": passed on. The kernel serves the whole line at `/proc/cmdline`, so a word
        // it does not recognise may still be userspace's, and "ignoring" it would be the log lying.
        Ignored::Unknown(word) => {
            kprintln!("cmdline: \"{}\" is not a kernel flag; it is served whole at /proc/cmdline", Printable(word))
        }
        Ignored::BadValue(word) => kprintln!(
            "cmdline: \"{}\" has no readable number of seconds — using {}",
            Printable(word),
            cmdline::HWREPORT_DEFAULT_SECS
        ),
        Ignored::BadSwitch(word) => {
            kprintln!("cmdline: \"{}\" takes on or off — ignoring it", Printable(word))
        }
    });
    if let Some(secs) = flags.hwreport {
        kprintln!("cmdline: hardware report — each page waits up to {} s for a key", secs);
    }
    if flags.usb_off {
        kprintln!("cmdline: usb=off — no driver takes the USB host controller");
    }
    flags
}

/// A command line as a log line shows it: [`Printable`], but an empty one stays empty, not `-`.
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

/// Whether this CPU has a cache-attribute table, recorded by `paging_init` for
/// [`record_framebuffer`]: without one there is no write-combining to ask for.
static ATTRIBUTE_TABLE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// See [`ATTRIBUTE_TABLE`].
fn policy_installed() -> bool {
    ATTRIBUTE_TABLE.load(core::sync::atomic::Ordering::Relaxed)
}

/// Capture Limine's framebuffer as a userspace-mappable aperture, before `run_first_userspace`
/// binds `/dev/framebuffer`. Needs the HHDM: Limine reports a virtual address and a `MemoryObject`
/// needs the physical base.
fn record_framebuffer() {
    let fb = match limine_framebuffer() {
        Ok(fb) => fb,
        Err(why) => {
            kprintln!("framebuffer: {why} — /dev/framebuffer will be unavailable");
            return;
        }
    };

    // The aperture is write-combining, said once here for everything that maps it (the
    // `/dev/framebuffer` `MemoryObject`, the measurement below), unless this CPU has no attribute
    // table: the bit that would select write-combining is then reserved, and a mapping setting it
    // would fault rather than run slowly.
    let caching = match policy_installed() {
        true => nitrox_kernel::mm::Caching::WriteCombining,
        false => nitrox_kernel::mm::Caching::Normal,
    };
    // SAFETY: `fb` is Limine's live descriptor and `hhdm_offset()` is the offset Limine
    // reported for this boot, so `address - offset` is the aperture's physical base.
    if unsafe { framebuffer::record_aperture(fb, nitrox_kernel::mm::heap::hhdm_offset(), caching) } {
        // The padding as a number: the bytes each row carries past its last pixel.
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

/// What writing to the screen costs on this machine, and why: the caching the firmware set for
/// physical memory, what that makes the framebuffer's address, and a full-screen fill's time
/// through the console's mapping and through a mapping made as **userspace's** is
/// (`page_flags_for`).
///
/// The pair of timings is the evidence: GiB/s over memory the range registers call uncacheable is
/// impossible for an uncached write, so the cost is the page table entry. A second mapping of
/// device memory with a different type is the aliasing the manuals warn about, so this maps the
/// same attribute the console's has, for one fill, then unmaps it.
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
        None => kprintln!("framebuffer: {:#x} has no memory type of its own; the page table decides", phys.as_u64()),
    }

    // A second mapping of the aperture as `/dev/framebuffer` maps it; `map_mmio` would force
    // uncached.
    let bytes = info.pitch as usize * info.height as usize;
    let pages = (bytes as u64).div_ceil(nitrox_kernel::mm::PAGE_SIZE as u64);
    let plain = plain_mapping(phys, pages, caching);
    // SAFETY: `plain`, when mapped, addresses the same framebuffer for the length of the call;
    // the console is the kernel's and no userspace exists yet.
    let measured = unsafe { fbcon::time_full_fills(plain.map(|v| v.as_u64() as *mut u8)) };
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
    // Unmapped before the early return below: `time_full_fills` answers `None` when the console has
    // no screen, and a write-back alias of the whole aperture must not outlive the measurement.
    if let Some(v) = plain {
        // SAFETY: undoing this function's own mapping of pages nothing else refers to. The vmap
        // range is never reused, so the address is not handed out again.
        unsafe { unmap_plain(v, pages) };
    }
    let Some(f) = measured else { return };
    let rate = |ns: u64| -> u64 { ((f.bytes as u64) * 1_000_000_000).checked_div(ns).unwrap_or(0) / (1024 * 1024) };
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

/// Map `pages` of the framebuffer at `phys` with the flags a `/dev/framebuffer` mapping gets, so
/// the second timing measures userspace's mapping. `None` if the vmap or page tables refuse.
fn plain_mapping(
    phys: nitrox_kernel::mm::PhysAddr,
    pages: u64,
    caching: nitrox_kernel::mm::Caching,
) -> Option<nitrox_kernel::mm::VirtAddr> {
    use nitrox_kernel::mm::vmm::Protection;
    use nitrox_kernel::mm::{addr_space, kvmap};
    // The flags come from the function that gives them to `/dev/framebuffer`, not a second copy.
    let flags = addr_space::page_flags_for(Protection::WRITE, caching);
    // SAFETY: `phys .. + pages` is the framebuffer aperture this kernel already maps; the flags
    // are the ones a user mapping of it gets.
    unsafe { kvmap::map_device(phys, pages, flags) }.ok()
}

/// Undo [`plain_mapping`].
///
/// # Safety
/// `base .. base + pages` must be the mapping [`plain_mapping`] returned, unused from here on.
unsafe fn unmap_plain(base: nitrox_kernel::mm::VirtAddr, pages: u64) {
    // SAFETY: forwarded from this function's contract.
    unsafe { nitrox_kernel::mm::kvmap::unmap_device(base, pages) };
}

/// Arm the syscall fast path, then load and launch **init** from the initramfs (`/sbin/init`) as
/// pid 1, with `rdi` = its notification-channel handle, `rsi` = a full-rights root-namespace handle
/// carrying the boot kernel-server bindings, `rdx` = the system-control handle.
fn run_first_userspace() {
    use mm::addr_space::AddressSpace;
    use mm::elf::load_elf;
    use nitrox_kernel::handle::global;
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::libkern::handle::{KObjectType, Rights};
    use nitrox_kernel::object::kernel_server::KernelServerId;
    use nitrox_kernel::object::{Namespace, NotificationChannel, ObjectRef, Process};

    arch::init_syscall_entry();
    kprintln!("syscall fast-path armed");

    let aspace = match AddressSpace::new() {
        Ok(a) => a,
        Err(_) => {
            kprintln!("init: address space alloc failed");
            return;
        }
    };
    let init_bytes =
        match nitrox_kernel::initramfs::blob().and_then(|blob| nitrox_kernel::initramfs::lookup(blob, b"sbin/init")) {
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

    // Pid 1 is the root. The capability bootstrap: init holds the full syscap set, and all
    // authority traces to this grant (docs/architecture/syscaps.md).
    let mut proc_box = match Process::try_new_user(1, aspace, nitrox_kernel::libkern::SysCaps::all()) {
        Ok(p) => p,
        Err(_) => {
            kprintln!("init: process alloc failed");
            return;
        }
    };

    // The notification channel: the Process owns one reference, and a handle in pid 1's table the
    // other.
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

    // The root namespace, referenced like the channel above (its handle goes in `rsi`).
    let ns = match Namespace::try_new() {
        Ok(n) => n,
        Err(_) => {
            kprintln!("init: namespace alloc failed");
            return;
        }
    };

    // Bind the in-kernel resource servers into pid 1's root namespace (the kernel is the supervisor
    // at boot); children inherit them. `/dev/entropy` mints an `EntropyObject` on lookup; its
    // rights are the band `sys_entropy_create` mints (`entropy_rights`).
    let entropy_binding_rights = Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/entropy", KernelServerId::Entropy, entropy_binding_rights).is_err() {
        kprintln!("init: binding /dev/entropy failed");
        return;
    }

    let console_binding_rights = Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/console", KernelServerId::Console, console_binding_rights).is_err() {
        kprintln!("init: binding /dev/console failed");
        return;
    }

    let log_binding_rights = Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/log", KernelServerId::Log, log_binding_rights).is_err() {
        kprintln!("init: binding /dev/log failed");
        return;
    }

    // `/dev/framebuffer`, the display aperture. Whoever holds this binding is the compositor:
    // authority is the namespace grant, not a capability bit or a registration call
    // (`docs/design/display-substrate.md` §3). `MAP_*` only: `READ`/`WRITE` are not valid on a
    // `MemoryObject` and would be rejected as `BadRights`.
    let framebuffer_binding_rights =
        Rights::MAP_READ | Rights::MAP_WRITE | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/framebuffer", KernelServerId::Framebuffer, framebuffer_binding_rights).is_err() {
        // Non-fatal: a system with no usable framebuffer still boots to a serial console.
        kprintln!("init: binding /dev/framebuffer failed — display unavailable");
    }

    // `/proc/self/*`: each binding is a dispatch id; the answer is the looking-up thread's own
    // object, resolved per caller (no ambient authority). Per leaf, not one prefix, because Process
    // / Thread / Namespace carry disjoint principal rights.
    let proc_self_principal_rights =
        Rights::SIGNAL | Rights::TERMINATE | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    // LOOKUP-only, no BIND: granting self-bind ambiently would be a capability escalation.
    let proc_self_namespace_rights = Rights::LOOKUP | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    let proc_self_status_rights = Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    let proc_self_binds = [
        (&b"/proc/self/process"[..], KernelServerId::ProcSelfProcess, proc_self_principal_rights),
        (&b"/proc/self/thread"[..], KernelServerId::ProcSelfThread, proc_self_principal_rights),
        (&b"/proc/self/namespace"[..], KernelServerId::ProcSelfNamespace, proc_self_namespace_rights),
        (&b"/proc/self/status"[..], KernelServerId::ProcSelfStatus, proc_self_status_rights),
        // `/proc/cmdline`: one fact about the machine, the same for every reader, so not
        // per-process.
        (&b"/proc/cmdline"[..], KernelServerId::ProcCmdline, proc_self_status_rights),
    ];
    for (path, id, rights) in proc_self_binds {
        if ns.bind_kernel_server(path, id, rights).is_err() {
            kprintln!("init: binding /proc/self/* failed");
            return;
        }
    }

    // `/proc/sched/stats`: a read-only snapshot. Reachability is by namespace construction, so a
    // supervisor may omit it from a sandbox.
    let sched_stats_binding_rights = Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/proc/sched/stats", KernelServerId::SchedStats, sched_stats_binding_rights).is_err() {
        kprintln!("init: binding /proc/sched/stats failed");
        return;
    }

    let initramfs_binding_rights = Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/initramfs", KernelServerId::Initramfs, initramfs_binding_rights).is_err() {
        kprintln!("init: binding /initramfs failed");
        return;
    }

    // `/dev/blk/<n>`: a subtree server resolving the n-th block device. Bound unconditionally: the
    // registry carries liveness, so `/dev/blk/0` is `NotFound` with no disk. `READ` + `WRITE` (the
    // RW fs-server writes metadata via `sys_io_submit`), and `MAP_READ` for the `<n>/info` leaf: it
    // answers with a read-only `MemoryObject`, and a lookup attenuates to the binding's rights.
    // `MAP_READ` means nothing on a `DeviceNode`, which is not mappable, so it widens only the
    // leaf.
    let block_binding_rights =
        Rights::READ | Rights::WRITE | Rights::MAP_READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/blk", KernelServerId::BlockDevice, block_binding_rights).is_err() {
        kprintln!("init: binding /dev/blk failed");
        return;
    }

    // `/dev/input/raw/<n>`: the i8042's keyboard and mouse nodes, `NotFound` when none answered.
    // Root namespace only: it is the authority to read an input device unfiltered, a keylogger if
    // it reaches the wrong process, so no session namespace should project it
    // (`docs/architecture/input-subsystem.md` §5).
    let raw_input_binding_rights = Rights::READ | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    if ns.bind_kernel_server(b"/dev/input/raw", KernelServerId::RawInput, raw_input_binding_rights).is_err() {
        kprintln!("init: binding /dev/input/raw failed");
        return;
    }

    // `/dev/registry`: the device table, a snapshot of every node's record and each node by id.
    // Root namespace only, like `/dev/input/raw`: it spans every raw device, so `/dev/blk`'s
    // rights, whose `MAP_READ` lets the snapshot be read.
    if ns.bind_kernel_server(b"/dev/registry", KernelServerId::Registry, block_binding_rights).is_err() {
        kprintln!("init: binding /dev/registry failed");
        return;
    }

    nitrox_kernel::drivers::gpt::bind_partition_names(&ns);

    let ns_ptr = KBox::into_raw(ns).as_ptr() as *mut ();
    // SAFETY: `into_raw` yielded the single creation reference; adopt it, clone
    // one for the Process, install the other as a handle (refcount → 2).
    let ns_ref = unsafe { ObjectRef::from_raw(ns_ptr, KObjectType::Namespace) };
    proc_box.set_namespace(ns_ref.clone());
    let (np, nt) = ns_ref.into_raw();
    // Full namespace rights, in sync with `syscall::table::namespace_rights`.
    let ns_rights =
        Rights::LOOKUP | Rights::BIND | Rights::UNBIND | Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT;
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

    // The system-control object, the capability to stop the machine, handed to init in `rdx`. Its
    // handle has `WRITE` and `INSPECT` but not `DUPLICATE` or `TRANSFER`, so init cannot give it
    // away.
    let system_control_h = match nitrox_kernel::object::SystemControl::try_new() {
        Ok(sc) => {
            let sp = KBox::into_raw(sc).as_ptr() as *mut ();
            let rights = nitrox_kernel::object::system_control::INIT_RIGHTS;
            match global::get().allocate(1, sp, KObjectType::SystemControl, rights) {
                Ok(h) => h.bits(),
                Err(_) => {
                    // SAFETY: `allocate` did not adopt the creation reference; reclaim and drop it.
                    drop(unsafe { ObjectRef::from_raw(sp, KObjectType::SystemControl) });
                    kprintln!("init: system-control handle alloc failed");
                    0
                }
            }
        }
        Err(_) => {
            kprintln!("init: system-control object alloc failed");
            0
        }
    };

    let proc_ref = {
        let ptr = KBox::into_raw(proc_box).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(ptr, KObjectType::Process) }
    };

    if sched::spawn_user(
        proc_ref,
        info.entry_point.as_u64(),
        info.stack_top.as_u64(),
        [notif_h.bits(), ns_h.bits(), system_control_h, 0],
    )
    .is_err()
    {
        kprintln!("init: spawn_user failed");
        return;
    }
    kprintln!("init: spawned init (pid 1); handing off to userspace");
}

/// Record the first Limine module as the `/initramfs` blob; non-fatal if none was configured.
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
    // Every module after the first is a disk (the live image's root), recorded while the responses
    // are fresh and published by `drivers::probe` once the device and interrupt tables exist.
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
    // The disk the machine started from: the first module came off it, and its GPT GUID is what the
    // partition pass compares every disk's with.
    nitrox_kernel::drivers::partitions::set_boot_disk(file.gpt_disk_uuid);
    match nitrox_kernel::drivers::partitions::boot_disk() {
        Some(guid) => kprintln!(
            "boot: the modules came from partition {} of the GPT disk {}",
            file.partition_index,
            nitrox_kernel::drivers::partitions::Guid(&guid)
        ),
        None => kprintln!("boot: the modules came from a volume Limine names no GPT disk for"),
    }
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
    // SAFETY: as above.
    let hhdm_resp = unsafe { (&raw const HHDM_REQUEST).read().response };
    if hhdm_resp.is_null() {
        return false;
    }
    // SAFETY: Each non-null response pointer guarantees Limine populated
    // a valid response of the corresponding type. The responses live in
    // bootloader-reclaimable memory which we have not yet reclaimed.
    let memmap = unsafe { &*memmap_resp };
    // SAFETY: as above.
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
/// 1. Enable every CPU memory-protection feature the kernel depends on (x86_64: NX, SMEP, SMAP).
/// 2. Pre-allocate the kernel-vmap region's intermediate page tables in the live PML4.
/// 3. Capture the kernel-half PML4 entries into the template every new address space inherits.
///
/// The order matters: `kvmap::init` modifies the live PML4 in ways the template must see, and the
/// snapshot freezes the kernel half. See "Kernel-half PML4 sharing" in
/// `docs/architecture/memory-management.md`.
fn paging_init() {
    arch::Cpu::init_protections();
    kprintln!("memory protections enabled");
    // FP/SIMD before any thread is created; per-CPU registers, so each AP repeats it in
    // `ap_cpu_init`.
    arch::fpu_init_cpu();
    // The cache-policy table becomes the kernel's before any mapping names an entry in it. Per-CPU,
    // so each AP repeats it in `ap_cpu_init`: CPUs that disagreed would give a page different
    // meanings by core.
    //
    // SAFETY: ring 0, on the boot CPU, before `kvmap::init` maps anything.
    let policy = unsafe { arch::MemoryTypes::install_policy() };
    ATTRIBUTE_TABLE.store(policy != Policy::NoTable, core::sync::atomic::Ordering::Relaxed);
    match policy {
        Policy::Unchanged => kprintln!("cache policy: the kernel's table is installed; the bootloader's was the same"),
        Policy::Replaced => kprintln!(
            "cache policy: the kernel's table is installed; the bootloader's DIFFERED — mappings \
             it made, including the console's, chose entries from another table"
        ),
        // Nothing to differ from. With no attribute table there is no write-combining, so the
        // framebuffer is recorded as ordinary memory.
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
        mm::kvmap::init(arch::Paging::active_root());
        arch::Paging::init_kernel_template(arch::Paging::active_root());
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    use core::fmt::Write;

    // Use the unsynchronised emergency serial path, not `kprintln!`: a panic can occur while
    // `SERIAL`'s lock is held, and re-locking would deadlock.
    let mut w = arch::serial::emergency_writer();
    let _ = writeln!(w, "\n*** KERNEL PANIC ***");
    if let Some(loc) = info.location() {
        let _ = writeln!(w, "  at {}:{}:{}", loc.file(), loc.line(), loc.column());
    }
    let _ = writeln!(w, "  {}", info.message());
    // Scan the panicking stack for kernel-text addresses, a poor man's backtrace (stale slots give
    // false positives, hence "candidates"). Shared with the exception dump, which probes pages: a
    // kernel stack's guard page is at the *bottom*, so scanning up from a shallow `rsp` (a
    // syscall-path assertion) runs off the top into unmapped vmap, and that `#PF`, taken before
    // `debug_exit` below, buries the diagnosis and ends in the 90 s timeout.
    arch::dump_stack_candidates(&mut w);
    // Under the integration-test build a panic is a test failure: end the QEMU run with the fail
    // verdict (`0x11` -> exit 35) rather than hang until the wall-clock timeout.
    #[cfg(feature = "test-harness")]
    arch::debug_exit(0x11);
    // Stop every CPU, not just this one: a panicked kernel must not run on while the rest keep
    // scheduling. `debug_exit` returns when no `isa-debug-exit` device is attached, so this runs
    // either way. Safe from any core, even one whose APIC is not up: `stop_the_machine` falls back
    // to halting itself, the early-boot case `ap_entry` uses.
    arch::Cpu::stop_the_machine()
}
