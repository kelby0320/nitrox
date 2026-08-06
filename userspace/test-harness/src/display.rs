//! `display-selftest` — the display arm's guest-side gate (plan M1 Part C).
//!
//! Two independent checks, deliberately separated because they fail for different
//! reasons and one of them needs no display at all:
//!
//! 1. **The self-hash.** Composite `libdraw`'s reference scene and compare its hash to
//!    [`libdraw::scene::REFERENCE_HASH`], the constant the host test asserts against its
//!    own composite of the same scene. The scene renders into its *own* 64×32 buffer, so
//!    this needs no framebuffer — what it proves is that compositing behaves identically
//!    compiled for `x86_64-unknown-nitrox` as it does on the host: integer width,
//!    endianness, optimisation. "The same hash asserted in two places is what makes this
//!    worth running" (`docs/design/display-substrate.md` §8b).
//! 2. **The framebuffer is reachable.** Acquire it and report its geometry, proving the
//!    binding and the geometry hand-off still work.
//!
//! It no longer *presents* the scene: since M2 Part D that is `ui-testclient`'s job, and
//! the picture `check-display` compares now arrives through the whole Surface protocol
//! rather than being written straight to the aperture. Writing it here as well would race
//! the compositor for the same pixels.
//!
//! Splitting them means a framebuffer problem cannot break the hash check and a
//! compositing bug cannot hide behind a display problem.
//!
//! **On failure the scene is dumped as a PPM to the serial log** (base64-free, just the
//! byte count and hash), because a mismatch otherwise reports two 64-bit numbers and
//! nothing about *what* differs — stride skew, swapped channels and wrong stacking order
//! all present identically.
//!
//! **Exit codes distinguish the two ways this can end badly**, because the program cannot
//! decide which of them matters — its environment can:
//!
//! - `0` — everything checked out.
//! - `1` — a real failure: the hash disagreed, or the aperture resolved and then
//!   misbehaved.
//! - `2` — **no `/dev/framebuffer` binding at all.** On a machine with no display that is
//!   expected; under `test-harness` the emulator always reports a framebuffer, so it means
//!   the binding is broken. Folding this into `0` is exactly the regression this program
//!   shipped with at first: the whole display arm could be gone and `test-qemu` stayed
//!   green.
//!
//! This binary is disposable. The part worth keeping — `libdraw::acquire` — is in the
//! library, so when the real compositor arrives at Milestone 2 this can simply go.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::acquire::{self, AcquireError};
use libdraw::hash::hash_visible;
use libdraw::scene::{self, REFERENCE_HASH};
use libkern::{exit, kprint};

/// `alloc` backing — `libdraw`'s scene rendering allocates its surface buffers.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Print `n` in hex, zero-padded to 16 digits, without `format!`.
fn kprint_hex64(n: u64) {
    let mut buf = [b'0'; 16];
    for i in 0..16 {
        let nib = ((n >> (60 - i * 4)) & 0xF) as u8;
        buf[i] = if nib < 10 { b'0' + nib } else { b'a' + nib - 10 };
    }
    kprint(&buf);
}

/// Print `n` in decimal, without `format!`.
fn kprint_u64(n: u64) {
    if n == 0 {
        kprint(b"0");
        return;
    }
    let mut d = [0u8; 20];
    let (mut i, mut n) = (0usize, n);
    while n > 0 {
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = d[i - 1 - j];
    }
    kprint(&out[..i]);
}

/// Check 1: the reference scene's hash matches the constant the host asserts.
fn check_reference_hash() -> bool {
    let fb = scene::render_reference();
    let got = hash_visible(&fb);
    if got == REFERENCE_HASH {
        kprint(b"display-selftest: reference hash 0x");
        kprint_hex64(got);
        kprint(b" OK\n");
        return true;
    }

    kprint(b"display-selftest: reference hash MISMATCH\n  guest 0x");
    kprint_hex64(got);
    kprint(b"\n  host  0x");
    kprint_hex64(REFERENCE_HASH);
    kprint(b"\n");
    // The picture, not just the number. A viewer can open this; the two hashes cannot
    // distinguish a stride skew from a swapped channel.
    let ppm = libdraw::ppm::to_ppm(&fb);
    kprint(b"display-selftest: scene follows as hex-encoded P6 PPM (");
    kprint_u64(ppm.len() as u64);
    kprint(b" bytes, ");
    kprint_u64(scene::SCREEN_WIDTH as u64);
    kprint(b"x");
    kprint_u64(scene::SCREEN_HEIGHT as u64);
    kprint(b")\n");
    // Emitted over serial so the picture actually leaves the guest. Without this the
    // encoder would exist and a failure would still report only two hex numbers, which
    // is the situation the dump is meant to end. Extract with:
    //   sed -n '/PPM-BEGIN/,/PPM-END/p' log | grep -v PPM- | tr -d '\n' | xxd -r -p > s.ppm
    kprint(b"---PPM-BEGIN---\n");
    let mut line = [0u8; 65];
    let mut n = 0usize;
    for &b in ppm.iter() {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        line[n] = HEX[(b >> 4) as usize];
        line[n + 1] = HEX[(b & 0xF) as usize];
        n += 2;
        if n == 64 {
            line[64] = b'\n';
            kprint(&line[..65]);
            n = 0;
        }
    }
    if n > 0 {
        line[n] = b'\n';
        kprint(&line[..n + 1]);
    }
    kprint(b"---PPM-END---\n");
    false
}

/// Check 2: acquire the real framebuffer and present the scene into a known corner.
///
/// Returns `false` only for failures that indicate a *broken binding*. A machine with no
/// display at all reports `NoBinding` and is not a failure here — the caller decides,
/// because only it knows whether a display was expected.
fn present_scene(root_ns: u64) -> Result<(), AcquireError> {
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (mut fb, info) = unsafe { acquire::acquire(root_ns) }?;

    kprint(b"display-selftest: framebuffer ");
    kprint_u64(info.width as u64);
    kprint(b"x");
    kprint_u64(info.height as u64);
    kprint(b" pitch=");
    kprint_u64(info.pitch);
    kprint(b"\n");

    // Deliberately does **not** draw. `ui-testclient` puts the scene on screen through the
    // compositor, and a second writer to the same aperture would race it.
    let _ = &mut fb;
    kprint(b"display-selftest: framebuffer reachable\n");
    Ok(())
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"display-selftest: up\n");

    let hash_ok = check_reference_hash();

    let mut headless = false;
    let present_ok = match present_scene(root_ns) {
        Ok(()) => true,
        Err(AcquireError::NoBinding) => {
            // No display bound into this namespace. Reported distinctly (exit 2) rather
            // than as success: only the caller knows whether a display was expected here.
            kprint(b"display-selftest: no /dev/framebuffer binding (headless)\n");
            headless = true;
            true
        }
        Err(e) => {
            kprint(b"display-selftest: acquire FAILED: ");
            kprint(match e {
                AcquireError::InfoUnmappable => b"info unmappable" as &[u8],
                AcquireError::InfoTruncated => b"info truncated",
                AcquireError::UnsupportedDepth(_) => b"unsupported depth",
                AcquireError::ImpossibleGeometry => b"impossible geometry",
                AcquireError::ApertureUnmappable => b"aperture unmappable",
                AcquireError::NoBinding => b"no binding",
            });
            kprint(b"\n");
            false
        }
    };

    if hash_ok && present_ok {
        if headless {
            kprint(b"display-selftest: HEADLESS (no display to present to)\n");
            exit(2);
        }
        kprint(b"display-selftest: PASSED\n");
        exit(0);
    }
    kprint(b"display-selftest: FAILED\n");
    exit(1);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"display-selftest: panic\n");
    exit(2);
}
