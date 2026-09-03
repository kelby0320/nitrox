//! `compose-bench` — what composing a drag costs, and where the cost is.
//!
//! Milestone 13 Part A opens with a measurement, and this is it. The plan's claim is that
//! composing into RAM and copying the finished damage rectangle to the aperture in one pass is
//! *also faster* than composing straight into the aperture — "the per-pixel work moving off MMIO
//! into cached RAM" — and records that as plausible and unproven.
//!
//! ## Why the claim needed checking before the code
//!
//! **This system does not map the aperture uncached.** `protection_to_page_flags`
//! (`kernel/src/mm/addr_space.rs`) never sets `PageFlags::NO_CACHE`, and nothing carries a cache
//! attribute from a `MemoryObject` to a user PTE — `NO_CACHE` is used only by `kvmap` for kernel
//! MMIO. So from the CPU's side the framebuffer is already write-back cached RAM, and the stated
//! mechanism cannot be what makes a shadow buffer faster. On that reasoning alone the shadow
//! buffer should be **slower**, by about one full-screen copy.
//!
//! There is a second mechanism with the opposite sign, which nothing has written down: under KVM,
//! QEMU dirty-tracks the framebuffer's memory slot so it knows what to redraw. Composing in place
//! touches aperture pages repeatedly and scattered; composing then copying touches each page once,
//! contiguously.
//!
//! Two plausible mechanisms, opposite signs, neither established — so this does not try to
//! confirm a win. **It establishes the price.** The flicker fix stands on its own: it is an
//! ordering property (no background-only intermediate state is ever on screen), not a speed one,
//! and no number here can show it. What the numbers decide is whether the price is acceptable.
//!
//! ## The two experiments
//!
//! **E1 — is the aperture behaving like RAM?** Write the same bytes to the aperture and to an
//! anonymous mapping of the same size, and compare. This settles the first mechanism by
//! *measurement* rather than by reading page-table code: if the two agree, the aperture is
//! ordinary cached memory and "off MMIO into cached RAM" describes nothing.
//!
//! **E2 — in place versus shadow, on a drag.** A window moves one step per frame and the damage
//! is the union of where it was and where it is, which is the workload the flicker was reported
//! from. Both arms run the *real* `libdraw::compose::compose`; the shadow arm adds a
//! `MemFramebuffer` and a copy, which is as close to the eventual implementation as a measurement
//! can be without committing to it.
//!
//! ## What makes the numbers trustworthy
//!
//! - **Arms interleaved**, not batched: a boot has host-load drift in it, and all-A-then-all-B
//!   attributes drift to whichever arm ran second.
//! - **Every frame reported**, not a mean. Flicker is a tail phenomenon and the host does the
//!   statistics, so a later run can be compared against this one at any percentile.
//! - **Run under both accelerators.** TCG models neither caches nor dirty-tracking, so it measures
//!   instruction count and should show the shadow arm strictly slower by roughly one copy — that
//!   is the *control*. KVM is where the real answer is, and **if the two agree, this is not
//!   measuring what it claims to.**
//! - **Nothing else is drawing.** It is declared to run after every display client in the image,
//!   and the bench image carries no `boot-probe` — this program owns the screen and fires the
//!   verdict itself.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;

use libdraw::acquire::{self, AcquireError};
use libdraw::compose::{SurfaceRef, compose, compose_exposed};
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Point, Rect};
use libkern::debug::Line;
use libkern::syscall::{
    SYS_CLOCK_READ, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_TEST_EXIT, syscall1, syscall4,
};
use libkern::{exit, kprint};

/// `alloc` backing: the shadow buffer and the surfaces.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// How many frames each arm draws.
///
/// **Enough that the distribution has a tail worth reading**, and few enough that the boot stays
/// inside the harness timeout: 200 frames per arm at a few milliseconds each is under a second of
/// work, and the numbers are printed rather than accumulated so a truncated run is still readable
/// up to where it stopped.
const FRAMES: usize = 200;

/// The moving window's size — an ordinary application window, not the screen.
const WIN_W: u32 = 560;
/// See [`WIN_W`].
const WIN_H: u32 = 420;
/// How far it moves per frame. One pixel is the worst case for damage area versus progress, and
/// it is what a drag actually produces.
const STEP: i32 = 1;

/// Read the monotonic clock in nanoseconds, or 0 if it cannot be read.
///
/// **Monotonic, not realtime**: realtime is derived from the RTC and is `Unsupported` on a machine
/// whose RTC could not be read, which would turn every interval into a zero rather than a failure.
fn now_ns() -> u64 {
    let mut out: u64 = 0;
    // SAFETY: `&mut out` is a valid writable `u64` out-param, which is what the ABI asks for.
    let r = unsafe {
        syscall4(
            SYS_CLOCK_READ,
            libkern::abi::CLOCK_MONOTONIC,
            (&mut out as *mut u64) as u64,
            0,
            0,
        )
    };
    if r < 0 { 0 } else { out }
}

/// An anonymous mapping of `len` bytes, for E1's control arm.
fn anonymous(len: usize) -> Option<*mut u8> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: maps the object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            h as u64,
            0,
            len as u64,
            libkern::handle::RIGHT_MAP_READ | libkern::handle::RIGHT_MAP_WRITE,
        )
    };
    if base < 0 { None } else { Some(base as u64 as *mut u8) }
}

/// **E1** — write `len` bytes to `dst` `rounds` times, reporting nanoseconds per round.
///
/// A plain store loop rather than anything clever: what is being compared is the *memory*, and a
/// vectorised copy would compare the compiler's output on two identical loops instead.
///
/// # Safety
/// `dst` must be writable for `len` bytes.
unsafe fn write_throughput(dst: *mut u8, len: usize, rounds: usize, label: &[u8]) {
    for r in 0..rounds {
        let t0 = now_ns();
        // SAFETY: the caller guarantees `dst` is writable for `len` bytes; the value varies per
        // round so nothing can be hoisted out of the loop as a constant store.
        unsafe {
            let byte = (r & 0xFF) as u8;
            for i in 0..len {
                dst.add(i).write_volatile(byte);
            }
        }
        let dt = now_ns().saturating_sub(t0);
        Line::new()
            .s(b"compose-bench: e1 ")
            .s(label)
            .s(b" ")
            .u(dt)
            .end();
    }
}

/// Where the window sits on frame `i`, wrapped so a long run stays on screen.
fn window_at(i: usize, screen: Rect) -> Rect {
    let span = screen.size.w.saturating_sub(WIN_W).max(1) as i32;
    // A triangle wave: right until the edge, then back. A wrap would put one frame's damage at
    // opposite ends of the screen, which is a different workload from a drag.
    let raw = (i as i32 * STEP) % (2 * span);
    let x = if raw <= span { raw } else { 2 * span - raw };
    Rect::new(x, 80, WIN_W, WIN_H)
}

/// The damage a one-step move produces: where the window was, and where it is.
///
/// **Two rectangles, not their bounding box.** That is what the compositor produces and what makes
/// the flicker visible — the union of two overlapping rects is nearly the area of one, and a
/// bounding box would make this benchmark measure a wider repaint than anything real.
fn damage(prev: Rect, next: Rect) -> [Rect; 2] {
    [prev, next]
}

/// **E2** — one frame composed straight into `fb`.
fn frame_in_place<F: Framebuffer + ?Sized>(
    fb: &mut F,
    surfaces: &[SurfaceRef<'_>],
    dmg: &[Rect],
) -> u64 {
    let t0 = now_ns();
    compose(fb, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    now_ns().saturating_sub(t0)
}

/// **E3** — one frame composed with the background fill skipped where a surface covers it.
///
/// The third arm, added after E2 answered: `compose` writes most of a drag's pixels twice, once
/// as background and once as the window, and [`compose_exposed`] writes them once. It is the same
/// insight the shadow buffer rests on, from the other side — **the fill is the flash** — so this
/// measures whether removing the fill pays for the copy that removing the flicker costs.
fn frame_exposed<F: Framebuffer + ?Sized>(
    fb: &mut F,
    surfaces: &[SurfaceRef<'_>],
    dmg: &[Rect],
) -> u64 {
    let t0 = now_ns();
    compose_exposed(fb, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    now_ns().saturating_sub(t0)
}

/// **E4** — both together: composed exposed into the shadow, then copied.
///
/// The combination is the one that matters, because it is what Part A would actually ship: the
/// shadow buffer for the ordering and the skipped fill for the work. Measuring the two separately
/// says nothing about whether they compose.
fn frame_exposed_shadow<F: Framebuffer + ?Sized>(
    fb: &mut F,
    shadow: &mut MemFramebuffer,
    surfaces: &[SurfaceRef<'_>],
    dmg: &[Rect],
) -> u64 {
    let t0 = now_ns();
    compose_exposed(shadow, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    copy_damage(fb, shadow, dmg);
    now_ns().saturating_sub(t0)
}

/// Copy each damage rectangle from `shadow` to `fb`, row by row.
fn copy_damage<F: Framebuffer + ?Sized>(fb: &mut F, shadow: &MemFramebuffer, dmg: &[Rect]) {
    let screen = fb.geometry().bounds();
    let (sg, dg) = (shadow.geometry(), fb.geometry());
    for area in dmg {
        let Some(area) = area.intersect(&screen) else { continue };
        for y in area.origin.y..area.bottom() as i32 {
            let so = y as usize * sg.pitch + area.origin.x as usize * 4;
            let n = area.size.w as usize * 4;
            let row = &shadow.bytes()[so..so + n];
            let dof = y as usize * dg.pitch + area.origin.x as usize * 4;
            fb.bytes_mut()[dof..dof + n].copy_from_slice(row);
        }
    }
}

/// **E2** — one frame composed into `shadow`, then copied to `fb` a damage rectangle at a time.
///
/// **The copy is per damage rectangle, not the whole screen**, because that is what the real
/// change would do: a full-screen copy per frame would measure a strategy nobody is proposing and
/// would flatter the in-place arm by comparison.
fn frame_shadow<F: Framebuffer + ?Sized>(
    fb: &mut F,
    shadow: &mut MemFramebuffer,
    surfaces: &[SurfaceRef<'_>],
    dmg: &[Rect],
) -> u64 {
    let t0 = now_ns();
    compose(shadow, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    copy_damage(fb, shadow, dmg);
    now_ns().saturating_sub(t0)
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _a: u64, _b: u64) -> ! {
    kprint(b"compose-bench: up\n");

    // SAFETY: `root_ns` is this process's live root namespace for its whole run.
    let (mut fb, info) = match unsafe { acquire::acquire(root_ns) } {
        Ok(pair) => pair,
        Err(e) => {
            kprint(match e {
                AcquireError::InfoUnmappable => b"compose-bench: info unmappable\n" as &[u8],
                AcquireError::NoBinding => b"compose-bench: no /dev/framebuffer\n",
                _ => b"compose-bench: acquire failed\n",
            });
            verdict(false)
        }
    };
    let geometry = fb.geometry();
    Line::new()
        .s(b"compose-bench: screen ")
        .u(geometry.width as u64)
        .s(b"x")
        .u(geometry.height as u64)
        .s(b" pitch ")
        .u(geometry.pitch as u64)
        .end();
    let _ = info;

    // ---- E1: is the aperture behaving like RAM? ----
    //
    // The bytes written are a *row*, not the screen: a full-screen pass per round would make each
    // sample tens of milliseconds and hide the per-write cost in loop overhead, and one row is
    // still four kilobytes of contiguous stores.
    let row = geometry.pitch;
    let rounds = 64;
    kprint(b"compose-bench: e1 begins (aperture vs anonymous, one row per round)\n");
    match anonymous(row) {
        Some(ram) => {
            // Interleaved, for the reason the header gives.
            for _ in 0..rounds {
                // SAFETY: the aperture is mapped writable for at least one row, and `ram` for
                // exactly `row` bytes.
                unsafe {
                    write_throughput(fb.bytes_mut().as_mut_ptr(), row, 1, b"aperture");
                    write_throughput(ram, row, 1, b"anonymous");
                }
            }
        }
        None => kprint(b"compose-bench: e1 SKIPPED (no anonymous mapping)\n"),
    }

    // ---- E2: in place versus shadow, on a drag ----
    let screen = geometry.bounds();
    let win_geom = match Geometry::with_pitch(
        WIN_W,
        WIN_H,
        WIN_W as usize * 4,
        PixelFormat::XRGB8888,
    ) {
        Some(g) => g,
        None => verdict(false),
    };
    // A window's worth of pixels, filled once. What is being measured is the compositing, not the
    // client's drawing, so the surface is static across frames — which is also what a drag is.
    let mut win = vec![0u8; win_geom.pitch * WIN_H as usize];
    for (i, px) in win.chunks_exact_mut(4).enumerate() {
        let c = Rgb::new((i % 251) as u8, 0xED, 0xEC);
        px.copy_from_slice(&PixelFormat::XRGB8888.encode(c).to_le_bytes());
    }
    let mut shadow = MemFramebuffer::new(geometry);

    kprint(b"compose-bench: e2 begins (in-place vs shadow, one-pixel drag)\n");
    let mut prev = window_at(0, screen);
    for i in 1..=FRAMES {
        let next = window_at(i, screen);
        let dmg = damage(prev, next);
        let surfaces = [SurfaceRef::new(win_geom, Point::new(next.origin.x, next.origin.y), &win)];

        // **Interleaved within the frame**, so every arm sees the same window position and the
        // same damage area. Comparing arm A at frame 7 against arm B at frame 8 would compare
        // two different overlaps.
        //
        // **And rotated**, which the two-arm version did not need to be. A fixed order hands the
        // first arm a cold framebuffer every frame and the last one rows three arms have already
        // walked; with the baseline nailed to the first slot, that bias lands directly on the
        // ratio being reported. Starting at `i % 4` gives every arm each slot equally often.
        let (mut a, mut b, mut c, mut d) = (0u64, 0u64, 0u64, 0u64);
        for slot in 0..4 {
            match (i as usize + slot) % 4 {
                0 => a = frame_in_place(&mut fb, &surfaces, &dmg),
                1 => b = frame_shadow(&mut fb, &mut shadow, &surfaces, &dmg),
                2 => c = frame_exposed(&mut fb, &surfaces, &dmg),
                _ => d = frame_exposed_shadow(&mut fb, &mut shadow, &surfaces, &dmg),
            }
        }
        Line::new()
            .s(b"compose-bench: e2 ")
            .u(dmg_area(&dmg, screen) as u64)
            .s(b" inplace ")
            .u(a)
            .s(b" shadow ")
            .u(b)
            .s(b" exposed ")
            .u(c)
            .s(b" both ")
            .u(d)
            .end();
        prev = next;
    }

    kprint(b"compose-bench: done\n");
    verdict(true)
}

/// The pixels a frame's damage actually covers, clipped to the screen.
///
/// Reported per frame so the host can normalise: a drag's two rectangles overlap by different
/// amounts as the window moves, and nanoseconds per frame without the area is a number that
/// drifts for a reason unrelated to either arm.
fn dmg_area(dmg: &[Rect], screen: Rect) -> usize {
    dmg.iter()
        .filter_map(|r| r.intersect(&screen))
        .map(|r| r.size.w as usize * r.size.h as usize)
        .sum()
}

/// Fire the boot verdict and terminate QEMU.
fn verdict(pass: bool) -> ! {
    let code = if pass {
        libkern::syscall::TEST_EXIT_SUCCESS
    } else {
        libkern::syscall::TEST_EXIT_FAILURE
    };
    // SAFETY: `SYS_TEST_EXIT` takes the verdict in a0; under the kernel's test-harness build it
    // writes the `isa-debug-exit` device and does not return.
    unsafe { syscall1(SYS_TEST_EXIT, code as u64) };
    exit(if pass { 0 } else { 1 })
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"compose-bench: PANIC\n");
    verdict(false)
}
