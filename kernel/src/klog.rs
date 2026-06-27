//! The kernel log ring — a bounded in-memory capture of kernel `kprint!` output.
//!
//! Every kernel diagnostic written through the `kprint!` / `kprintln!` macros (the
//! serial `write_str` path) is **teed** into a fixed-size buffer here, in addition
//! to the serial console. A supervisor/shell reads it back as a `MemoryObject`
//! snapshot bound at `/dev/log` (the `KernelServerId::Log` kernel server) — i.e.
//! `cat /dev/log` is the system's `dmesg`. It captures **kernel** messages (the
//! boot log: `ioapic`/`ahci`/`console`/`mm`/panic), not userspace `sys_kprint`
//! output (that is userspace stdout, not the kernel log).
//!
//! ## Buffer model
//!
//! A linear append buffer (not a wrap-around ring): it captures from boot until
//! full, then **drops** later output — keeping the early boot/failure context,
//! which is what an emergency inspection wants. [`KLOG_CAP`] (16 KiB) comfortably
//! holds a full boot log. (A keep-recent ring is a later refinement.)
//!
//! ## Locking
//!
//! All state is behind an [`IrqSpinLock`]. [`push`] uses **`try_lock`** (skipping
//! the line if contended) so teeing from the panic/exception path — which also
//! flows through `write_str` — can never deadlock against a fault that strikes
//! while the ring lock is held. The reader path ([`len`] / [`copy_into_frames`]) is
//! syscall context and blocks on the lock normally.

use crate::libkern::IrqSpinLock;
use crate::mm::{PAGE_SIZE, PhysAddr, heap};

/// Capacity of the kernel log buffer (bytes). 16 KiB = 4 pages — ample for a boot
/// log; output past it is dropped (the early log is retained).
pub const KLOG_CAP: usize = 16 * 1024;

struct Klog {
    /// The captured bytes (`buf[..len]`), raw — newlines are bare `\n` (the
    /// reader's `sys_kprint` translates `\n` → `\r\n` for the terminal).
    buf: [u8; KLOG_CAP],
    len: usize,
}

static KLOG: IrqSpinLock<Klog> = IrqSpinLock::new(Klog { buf: [0; KLOG_CAP], len: 0 });

/// Append `bytes` to the kernel log (called from the serial `write_str` tee). Drops
/// the bytes if the buffer is full, and **skips silently if the lock is contended**
/// (a fault mid-`push`, re-entered via the emergency writer) — logging is
/// best-effort and must never deadlock the panic path.
pub fn push(bytes: &[u8]) {
    let Some(mut g) = KLOG.try_lock() else {
        return;
    };
    let start = g.len;
    let n = bytes.len().min(KLOG_CAP - start);
    g.buf[start..start + n].copy_from_slice(&bytes[..n]);
    g.len = start + n;
}

/// The number of bytes currently captured (for sizing the `/dev/log` snapshot).
pub fn len() -> usize {
    KLOG.lock().len
}

/// Copy the captured bytes into `frames` (one page each, via the HHDM) — the
/// `/dev/log` snapshot fill. Copies `min(len, frames·PAGE)` bytes; returns the byte
/// count. The caller sizes `frames` to [`len`] (a page may grow between sizing and
/// this call; only the originally-counted, immutable prefix is copied). Bounded by
/// [`KLOG_CAP`]; runs under the ring lock (no allocation).
pub fn copy_into_frames(frames: &[PhysAddr]) -> usize {
    let g = KLOG.lock();
    let cap = frames.len() * PAGE_SIZE;
    let len = g.len.min(cap);
    let hhdm = heap::hhdm_offset();
    let mut i = 0;
    while i < len {
        let page = i / PAGE_SIZE;
        let intra = i % PAGE_SIZE;
        let n = (PAGE_SIZE - intra).min(len - i);
        let dst = (frames[page].as_u64() + hhdm + intra as u64) as *mut u8;
        // SAFETY: `dst..dst+n` is within an owned, HHDM-mapped frame (`page <
        // frames.len()`, `intra + n <= PAGE`); the source is the log buffer.
        unsafe { core::ptr::copy_nonoverlapping(g.buf.as_ptr().add(i), dst, n) };
        i += n;
    }
    len
}
