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
use nitrox_kernel::framebuffer::{FbWriter, Rgb};
use nitrox_kernel::kprintln;
use nitrox_kernel::limine::{
    BaseRevision, FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker,
    RequestsStartMarker,
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

    // Bring up the single global handle table. It eagerly allocates its
    // first segment, so the heap must be up (it is — `init_memory` ran); it
    // must be live before any userspace can issue a handle syscall.
    if nitrox_kernel::handle::global::init().is_err() {
        kprintln!("global handle table init failed — halting");
        return;
    }
    kprintln!("global handle table up");

    // Bring up the cooperative scheduler and run a few kernel threads to
    // prove the context switch end-to-end: each worker prints and yields
    // round-robin, then exits; the boot thread drains the queue and
    // returns here. See `docs/architecture/overview.md` § Scheduling.
    run_scheduler_demo();

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

/// The first userspace program (the spawn-demo **parent**), embedded at kernel
/// build time. Built by `cargo xtask` (which builds `userspace/parent` before
/// the kernel) as a static, non-PIE `ET_EXEC`. Spawn-able children are embedded
/// in the kernel lib (`embedded_images`).
static PARENT_ELF: &[u8] =
    include_bytes!("../../userspace/target/x86_64-unknown-none/release/parent");

/// Arm the syscall fast path, then load + launch the **parent** process (pid 1)
/// with a handle to its own notification channel in the bootstrap register
/// `rdi`. The parent then drives the demo entirely from userspace: it creates a
/// channel, spawns two children over it, and reaps their `ChildExited`s. This
/// boot thread hands off (via `sched::exit` in `kernel_main`) into the parent
/// and then idles; the parent is the supervisor now (not the kernel).
fn run_first_userspace() {
    use mm::addr_space::AddressSpace;
    use mm::elf::load_elf;
    use nitrox_kernel::handle::global;
    use nitrox_kernel::libkern::KBox;
    use nitrox_kernel::libkern::handle::{KObjectType, Rights};
    use nitrox_kernel::object::{NotificationChannel, ObjectRef, Process};

    // Arm the `syscall` entry MSRs once. The per-CPU kernel stack is set
    // per-thread (by the scheduler's `thread_enter`), not here.
    arch::init_syscall_entry();
    kprintln!("syscall fast-path armed");

    // Fresh address space (kernel half inherited → loadable), from the parent ELF.
    let aspace = match AddressSpace::new() {
        Ok(a) => a,
        Err(_) => {
            kprintln!("init: address space alloc failed");
            return;
        }
    };
    let info = match load_elf(&aspace, PARENT_ELF) {
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

    let proc_ref = {
        let ptr = KBox::into_raw(proc_box).as_ptr() as *mut ();
        // SAFETY: `into_raw` yielded the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(ptr, KObjectType::Process) }
    };

    // Spawn the parent's main thread, seeding `rdi` = its notification handle.
    if sched::spawn_user(
        proc_ref,
        info.entry_point.as_u64(),
        info.stack_top.as_u64(),
        [notif_h.bits(), 0, 0],
    )
    .is_err()
    {
        kprintln!("init: spawn_user failed");
        return;
    }
    kprintln!("init: spawned parent (pid 1); handing off to userspace");
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
    let status = b"PHASE 1: USERSPACE UP";
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
