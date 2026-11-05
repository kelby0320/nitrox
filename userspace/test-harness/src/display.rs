//! `display-selftest` — the display arm's guest-side gate: two checks that fail for different
//! reasons.
//!
//! 1. **The self-hash.** Composite `libdraw`'s reference scene into its own 64×32 buffer and
//!    compare its hash to [`libdraw::scene::REFERENCE_HASH`], which the host test asserts against
//!    its own composite: compositing must behave the same compiled for `x86_64-unknown-nitrox`.
//! 2. **The framebuffer is reachable.** Acquire it and report its geometry. It does not draw:
//!    `ui-testclient` presents the scene, and a second writer would race the compositor.
//!
//! On a mismatch the scene is dumped as a hex PPM to serial; two hashes cannot tell a stride
//! skew from swapped channels. Exit `0` all checked out, `1` a real failure, `2` no
//! `/dev/framebuffer` binding: under `test-harness` the emulator always reports a framebuffer, so
//! `2` means the binding is broken, and folding it into `0` once left `test-qemu` green with the
//! display arm gone.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::acquire::{self, AcquireError};
use libdraw::hash::hash_visible;
use libdraw::scene::{self, REFERENCE_HASH};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Handle, Namespace, NsReadOnly};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

fn check_reference_hash() -> bool {
    let fb = scene::render_reference();
    let got = hash_visible(&fb);
    if got == REFERENCE_HASH {
        Line::new().s(b"display-selftest: reference hash ").x(got).s(b" OK").end();
        return true;
    }

    // Two lines, two calls: each complete on its own, never torn.
    Line::new().s(b"display-selftest: reference hash MISMATCH").end();
    Line::new().s(b"  guest ").x(got).s(b"  host ").x(REFERENCE_HASH).end();
    // The picture, not just the number.
    let ppm = libdraw::ppm::to_ppm(&fb);
    Line::new()
        .s(b"display-selftest: scene follows as hex-encoded P6 PPM (")
        .u(ppm.len() as u64)
        .s(b" bytes, ")
        .u(scene::SCREEN_WIDTH as u64)
        .s(b"x")
        .u(scene::SCREEN_HEIGHT as u64)
        .s(b")")
        .end();
    // Emitted over serial so the picture leaves the guest. Extract with:
    //   sed -n '/PPM-BEGIN/,/PPM-END/p' log | grep -v PPM- | tr -d '\n' | xxd -r -p > s.ppm
    log::write(b"---PPM-BEGIN---\n");
    let mut line = [0u8; 65];
    let mut n = 0usize;
    for &b in ppm.iter() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        line[n] = HEX[(b >> 4) as usize];
        line[n + 1] = HEX[(b & 0xF) as usize];
        n += 2;
        if n == 64 {
            line[64] = b'\n';
            log::write(&line[..65]);
            n = 0;
        }
    }
    if n > 0 {
        line[n] = b'\n';
        log::write(&line[..n + 1]);
    }
    log::write(b"---PPM-END---\n");
    false
}

/// Check 2: the real framebuffer can still be acquired.
///
/// `Err` only for a *broken binding*; a machine with no display reports `NoBinding`, which the
/// caller judges.
fn check_framebuffer_binding(root: Option<&Handle<Namespace, NsReadOnly>>) -> Result<(), AcquireError> {
    let root = root.ok_or(AcquireError::NoBinding)?;
    // SAFETY: nothing else in this process writes the aperture, and this check never draws.
    let (_fb, info) = unsafe { acquire::map_framebuffer(root) }?;

    Line::new()
        .s(b"display-selftest: framebuffer ")
        .u(info.width as u64)
        .s(b"x")
        .u(info.height as u64)
        .s(b" pitch=")
        .u(info.pitch)
        .end();

    log::write(b"display-selftest: framebuffer reachable\n");
    Ok(())
}

/// Entry point: run the hash check, then the framebuffer check, and exit with the verdict.
#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    log::write(b"display-selftest: up\n");

    let hash_ok = check_reference_hash();

    let mut headless = false;
    let present_ok = match check_framebuffer_binding(start.root.as_ref()) {
        Ok(()) => true,
        Err(AcquireError::NoBinding) => {
            // Exit 2 rather than success: only the caller knows whether a display was expected.
            log::write(b"display-selftest: no /dev/framebuffer binding (headless)\n");
            headless = true;
            true
        }
        Err(e) => {
            log::write(b"display-selftest: acquire FAILED: ");
            log::write(match e {
                AcquireError::InfoUnmappable => b"info unmappable" as &[u8],
                AcquireError::InfoTruncated => b"info truncated",
                AcquireError::UnsupportedDepth(_) => b"unsupported depth",
                AcquireError::ImpossibleGeometry => b"impossible geometry",
                AcquireError::ApertureUnmappable => b"aperture unmappable",
                AcquireError::NoBinding => b"no binding",
            });
            log::write(b"\n");
            false
        }
    };

    if hash_ok && present_ok {
        if headless {
            log::write(b"display-selftest: HEADLESS (no display to present to)\n");
            libos::exit(2);
        }
        log::write(b"display-selftest: PASSED\n");
        libos::exit(0);
    }
    log::write(b"display-selftest: FAILED\n");
    libos::exit(1);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"display-selftest: panic\n");
    libos::exit(2);
}
