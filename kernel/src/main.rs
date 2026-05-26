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
//! After `kernel_main` returns, [`_start`] enters [`arch::halt_loop`]
//! forever. The kernel does no further work in this slice; Phase 1's
//! remaining items (paging, scheduler, syscalls, userspace) land next.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

use nitrox_kernel::arch;
use nitrox_kernel::framebuffer::{FbWriter, Rgb};
use nitrox_kernel::kprintln;
use nitrox_kernel::limine::{
    BaseRevision, FramebufferRequest, HhdmRequest, MemoryMapRequest, RequestsEndMarker,
    RequestsStartMarker,
};
use nitrox_kernel::mm;

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
    arch::halt_loop();
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

    // Install the kernel's own GDT + TSS, then the IDT. The IDT's gates
    // reference the kernel code selector that only becomes ours once our
    // GDT is loaded, and the double-fault gate needs the TSS's IST stack.
    arch::gdt::init();
    arch::idt::init();
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

    // Smoke-test the new paging arch layer against Limine's live tables.
    paging_smoke_test();

    // SAFETY: `FRAMEBUFFER_REQUEST.response` is written by Limine before
    // jumping to `_start`. We are the sole reader; no other thread exists.
    let response = unsafe { (&raw const FRAMEBUFFER_REQUEST).read().response };
    if response.is_null() {
        return;
    }
    // SAFETY: A non-null response pointer guarantees Limine populated a
    // valid `FramebufferResponse`. The framebuffer count and array
    // pointer come straight from the protocol contract.
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

/// Smoke-test the paging arch layer: enable the no-execute extension,
/// then walk Limine's live page tables with [`arch::translate`] to
/// confirm the kernel's table-walk agrees with the hardware.
///
/// Read-only — it never installs or switches a mapping. The first item
/// of the "Address Spaces and Paging" slice has no boot-path caller of
/// its own; this exercises the new code against a real address space and
/// a real HHDM offset (host tests run with an HHDM offset of 0). See
/// `docs/planning/implementation-plan.md`.
fn paging_smoke_test() {
    arch::ensure_nxe();

    // Limine's top-level page table is whatever the CPU runs on now.
    let root = arch::active_root();

    // This function's own code is certainly mapped; resolve its address.
    let probe = mm::VirtAddr::new(paging_smoke_test as fn() as usize as u64);
    // SAFETY: `root` is the live top-level page table the CPU is using,
    // reachable through the HHDM. `translate` only reads page-table
    // memory — it installs and switches nothing.
    match unsafe { arch::translate(root, probe) } {
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
    let status = b"PHASE 1: ALLOCATORS UP";
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
    arch::halt_loop()
}
