//! `compose-bench` — what composing a drag costs, and where the cost is.
//!
//! The claim: composing into RAM and copying the finished damage rectangle to the aperture is
//! *also faster* than composing straight into it. Yet the aperture is mapped cached here, so a
//! shadow buffer should cost about one copy; under KVM, QEMU's dirty-tracking of the framebuffer
//! may favour touching each page once. Opposite signs, so this establishes the price, not a win.
//!
//! **E1** writes the same bytes to the aperture and to anonymous memory; **E2** composes a
//! one-pixel drag in place and into a shadow then copied; **E3** skips the background fill where a
//! surface covers it; **E4** does both. Arms are interleaved and every frame is reported. Run
//! under TCG, the control that must show the shadow arm slower, and KVM; **if the two agree, this
//! is not measuring what it claims to.** The bench image has no `boot-probe`: this owns the
//! screen and fires the verdict itself.

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
use libkern::syscall::{SYS_CLOCK_READ, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_TEST_EXIT, syscall1, syscall4};
use libkern::{exit, kprint};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const FRAMES: usize = 200;

const WIN_W: u32 = 560;
const WIN_H: u32 = 420;
const STEP: i32 = 1;

/// Read the monotonic clock in nanoseconds, or 0 if it cannot be read.
///
/// Monotonic, not realtime: realtime is `Unsupported` when the RTC could not be read.
fn now_ns() -> u64 {
    let mut out: u64 = 0;
    // SAFETY: `&mut out` is a valid writable `u64` out-param, which is what the ABI asks for.
    let r = unsafe { syscall4(SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&mut out as *mut u64) as u64, 0, 0) };
    if r < 0 { 0 } else { out }
}

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

/// **E1** — write `len` bytes to `dst` `rounds` times, in ns per round; a plain store loop, since
/// a vectorised copy would compare the compiler.
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
        Line::new().s(b"compose-bench: e1 ").s(label).s(b" ").u(dt).end();
    }
}

/// Where the window sits on frame `i`, wrapped so a long run stays on screen.
fn window_at(i: usize, screen: Rect) -> Rect {
    let span = screen.size.w.saturating_sub(WIN_W).max(1) as i32;
    // A triangle wave, not a wrap: a wrap would put one frame's damage at opposite ends.
    let raw = (i as i32 * STEP) % (2 * span);
    let x = if raw <= span { raw } else { 2 * span - raw };
    Rect::new(x, 80, WIN_W, WIN_H)
}

/// The damage of a one-step move: two rectangles, not their bounding box, as the compositor makes.
fn damage(prev: Rect, next: Rect) -> [Rect; 2] {
    [prev, next]
}

/// **E2** — one frame composed straight into `fb`.
fn frame_in_place<F: Framebuffer + ?Sized>(fb: &mut F, surfaces: &[SurfaceRef<'_>], dmg: &[Rect]) -> u64 {
    let t0 = now_ns();
    compose(fb, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    now_ns().saturating_sub(t0)
}

/// **E3** — one frame composed with the background fill skipped where a surface covers it, so a
/// pixel is written once, not twice: the fill is the flash.
fn frame_exposed<F: Framebuffer + ?Sized>(fb: &mut F, surfaces: &[SurfaceRef<'_>], dmg: &[Rect]) -> u64 {
    let t0 = now_ns();
    compose_exposed(fb, Rgb::new(0x2A, 0x55, 0x70), surfaces, dmg);
    now_ns().saturating_sub(t0)
}

/// **E4** — both together, measured together because that is what would ship.
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

/// **E2** — one frame composed into `shadow`, then copied per damage rectangle, not whole screen.
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

/// Entry point: run E1, then the E2 to E4 arms frame by frame, then fire the boot verdict.
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

    // ---- E1: is the aperture behaving like RAM? One row per round, not the screen. ----
    let row = geometry.pitch;
    let rounds = 64;
    kprint(b"compose-bench: e1 begins (aperture vs anonymous, one row per round)\n");
    match anonymous(row) {
        Some(ram) => {
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
    let win_geom = match Geometry::with_pitch(WIN_W, WIN_H, WIN_W as usize * 4, PixelFormat::XRGB8888) {
        Some(g) => g,
        None => verdict(false),
    };
    // A static window: what is measured is the compositing, not the client's drawing.
    let mut win = vec![0u8; win_geom.pitch * WIN_H as usize];
    for (i, px) in win.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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

        // **Interleaved within the frame** so every arm sees the same window position and damage,
        // **and rotated** (`i % 4`) so no arm always takes the cold first slot and skews the ratio.
        let (mut a, mut b, mut c, mut d) = (0u64, 0u64, 0u64, 0u64);
        for slot in 0..4 {
            match (i + slot) % 4 {
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

/// The pixels a frame's damage covers, clipped to the screen; the host normalises by it.
fn dmg_area(dmg: &[Rect], screen: Rect) -> usize {
    dmg.iter().filter_map(|r| r.intersect(&screen)).map(|r| r.size.w as usize * r.size.h as usize).sum()
}

fn verdict(pass: bool) -> ! {
    let code = if pass { libkern::syscall::TEST_EXIT_SUCCESS } else { libkern::syscall::TEST_EXIT_FAILURE };
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
