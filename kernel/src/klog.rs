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
//! ## Buffer model: keep both ends
//!
//! Two regions, not one (Slice D1):
//!
//! - a **frozen prefix** that captures from boot until [`PREFIX_CAP`] and is then
//!   never overwritten — the early boot / first-failure context an emergency
//!   inspection wants;
//! - a **keep-recent ring** taking everything after it, so a long-running system
//!   retains its *latest* output instead of dropping it.
//!
//! The original buffer was a single linear append that stopped capturing when full.
//! That deliberately preserved early boot, and the cost only appears on a system
//! that runs long enough to fill it — which is exactly when a log becomes useful.
//! Keeping both ends means neither property is traded for the other; what is lost
//! is the *middle*, and the reader is told how much (see [`copy_into_frames`]).
//!
//! `PREFIX_CAP` is sized from a measurement rather than a guess: a full
//! integration boot (kernel → SMP → init → ext4 → userspace → login) captures
//! **3801 bytes** and drops nothing, so 8 KiB leaves better than 2× headroom for
//! the boot log to grow before any of it is at risk.
//!
//! ## Test coverage, and what is not covered
//!
//! The layout decision lives in one place ([`Klog::runs`]) that both the production copy
//! and the host tests walk, so the tests cannot pass against a layout production does not
//! use. What the host tests cannot reach is the page-wise copy into physical frames, which
//! needs HHDM-mapped memory. A boot exercises that for the **prefix-only** case every run
//! (`/dev/log` is read by the boot chain); the wrapped-ring case is covered by the host
//! tests up to the frame boundary and no further. A synthetic in-guest fill would close
//! that, at the cost of dirtying the real log with test bytes — judged not worth it while
//! the arithmetic is shared with the single-run path that runs every boot.
//!
//! ## Locking
//!
//! All state is behind an [`IrqSpinLock`]. [`push`] uses **`try_lock`** (skipping
//! the line if contended) so teeing from the panic/exception path — which also
//! flows through `write_str` — can never deadlock against a fault that strikes
//! while the ring lock is held. The reader path ([`len`] / [`copy_into_frames`]) is
//! syscall context and blocks on the lock normally.

use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::mm::{PAGE_SIZE, PhysAddr, heap};

/// Total capacity of the kernel log buffer (bytes). 16 KiB = 4 pages.
pub const KLOG_CAP: usize = 16 * 1024;

/// Bytes reserved for the never-overwritten boot prefix.
///
/// Sized from measurement: a full integration boot captures 3801 bytes, so this is
/// better than 2× headroom. If the boot log ever outgrows it the *overflow* lands in
/// the ring (nothing is lost at the time), and the prefix simply stops being a
/// complete boot log — which the `klog:` line in a `test-harness` run makes visible
/// rather than silent.
pub const PREFIX_CAP: usize = 8 * 1024;

/// Bytes of keep-recent ring — everything after the prefix.
pub const RING_CAP: usize = KLOG_CAP - PREFIX_CAP;

struct Klog {
    /// Boot prefix: `prefix[..prefix_len]`, frozen once `prefix_len == PREFIX_CAP`.
    /// Raw bytes — newlines are bare `\n` (the reader's `sys_kprint` translates
    /// `\n` → `\r\n` for the terminal).
    prefix: [u8; PREFIX_CAP],
    prefix_len: usize,
    /// Keep-recent ring. `ring_len` bytes are live; once full, `head` is the index of
    /// both the next write and the oldest live byte.
    ring: [u8; RING_CAP],
    head: usize,
    ring_len: usize,
    /// Bytes overwritten in the ring — the size of the elided middle.
    elided: usize,
}

impl Klog {
    /// Append `bytes`: fill the prefix while it has room, then the ring.
    fn append(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        // Prefix first, while it is still growing.
        if self.prefix_len < PREFIX_CAP {
            let n = rest.len().min(PREFIX_CAP - self.prefix_len);
            self.prefix[self.prefix_len..self.prefix_len + n].copy_from_slice(&rest[..n]);
            self.prefix_len += n;
            rest = &rest[n..];
        }
        if rest.is_empty() {
            return;
        }
        // A write longer than the whole ring can only leave its tail; skip straight to
        // the last RING_CAP bytes rather than copying through the ring repeatedly.
        if rest.len() >= RING_CAP {
            let skip = rest.len() - RING_CAP;
            self.elided += self.ring_len.min(RING_CAP) + skip;
            self.ring.copy_from_slice(&rest[skip..]);
            self.head = 0;
            self.ring_len = RING_CAP;
            return;
        }
        // Otherwise up to two copies: to the end of the ring, then wrapped.
        let first = rest.len().min(RING_CAP - self.head);
        self.ring[self.head..self.head + first].copy_from_slice(&rest[..first]);
        let second = rest.len() - first;
        if second > 0 {
            self.ring[..second].copy_from_slice(&rest[first..]);
        }
        self.head = (self.head + rest.len()) % RING_CAP;
        let overwritten = (self.ring_len + rest.len()).saturating_sub(RING_CAP);
        self.elided += overwritten;
        self.ring_len = (self.ring_len + rest.len()).min(RING_CAP);
    }

    /// The snapshot's byte runs, in order: boot prefix, elision notice, then the ring
    /// oldest-first (split in two when it has wrapped). Empty runs are included so the
    /// shape is fixed; callers skip them naturally.
    ///
    /// **The single definition of the snapshot's layout.** Both the production copy
    /// ([`copy_into_frames`]) and the host tests consume this, rather than each deciding
    /// the order for itself — a test that re-derives the layout it is checking would pass
    /// while production got it wrong.
    fn runs<'a>(&'a self, notice: &'a [u8]) -> [&'a [u8]; 4] {
        let start = self.ring_start();
        let (a, b) = if self.ring_len == 0 {
            (&self.ring[..0], &self.ring[..0])
        } else if start + self.ring_len <= RING_CAP {
            (&self.ring[start..start + self.ring_len], &self.ring[..0])
        } else {
            // Wrapped: oldest run to the end of the array, then the head run.
            let tail = RING_CAP - start;
            (&self.ring[start..], &self.ring[..self.ring_len - tail])
        };
        [&self.prefix[..self.prefix_len], notice, a, b]
    }

    /// Index of the oldest live ring byte.
    fn ring_start(&self) -> usize {
        if self.ring_len < RING_CAP {
            0
        } else {
            self.head
        }
    }

    /// Total bytes a linearised snapshot occupies: prefix + elision notice + ring.
    fn snapshot_len(&self) -> usize {
        self.prefix_len + self.notice_len() + self.ring_len
    }

    /// Length of the elision notice (`0` when nothing has been overwritten).
    fn notice_len(&self) -> usize {
        if self.elided == 0 {
            0
        } else {
            let mut buf = [0u8; NOTICE_MAX];
            format_notice(self.elided, &mut buf)
        }
    }
}

/// Upper bound on the elision notice, including a 20-digit `usize`.
const NOTICE_MAX: usize = 48;

/// Write `"\n[klog: N bytes elided]\n"` into `out`, returning its length.
///
/// Hand-formatted rather than `write!`-ed because this runs under the ring lock, in a
/// `no_std` kernel, with no allocation permitted.
fn format_notice(elided: usize, out: &mut [u8; NOTICE_MAX]) -> usize {
    fn put(s: &[u8], out: &mut [u8; NOTICE_MAX], n: &mut usize) {
        for &b in s {
            if *n < NOTICE_MAX {
                out[*n] = b;
                *n += 1;
            }
        }
    }
    let mut n = 0;
    put(b"\n[klog: ", out, &mut n);
    let mut digits = [0u8; 20];
    let mut i = 0;
    let mut v = elided;
    if v == 0 {
        digits[0] = b'0';
        i = 1;
    }
    while v > 0 {
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        if n < NOTICE_MAX {
            out[n] = digits[i];
            n += 1;
        }
    }
    put(b" bytes elided]\n", out, &mut n);
    n
}

static KLOG: IrqSpinLock<Klog> = IrqSpinLock::new(
    LockRank::Klog,
    Klog {
        prefix: [0; PREFIX_CAP],
        prefix_len: 0,
        ring: [0; RING_CAP],
        head: 0,
        ring_len: 0,
        elided: 0,
    },
);

/// Append `bytes` to the kernel log (called from the serial `write_str` tee).
///
/// Nothing is dropped once the buffer fills: the prefix freezes and the ring keeps the
/// most recent output. **Skips silently if the lock is contended** (a fault mid-`push`,
/// re-entered via the emergency writer) — logging is best-effort and must never
/// deadlock the panic path.
pub fn push(bytes: &[u8]) {
    let Some(mut g) = KLOG.try_lock() else {
        return;
    };
    g.append(bytes);
}

/// Bytes a `/dev/log` snapshot will occupy (prefix + elision notice + ring).
pub fn len() -> usize {
    KLOG.lock().snapshot_len()
}

/// `(prefix_len, ring_len, elided)` — the measurement behind [`PREFIX_CAP`], and what a
/// `test-harness` run reports so an outgrown prefix is visible rather than silent.
pub fn stats() -> (usize, usize, usize) {
    let g = KLOG.lock();
    (g.prefix_len, g.ring_len, g.elided)
}

/// Copy the linearised log into `frames` (one page each, via the HHDM) — the
/// `/dev/log` snapshot fill. Copies `min(snapshot_len, frames·PAGE)` bytes and returns
/// the byte count.
///
/// The layout is boot prefix, then — if any middle was overwritten — a
/// `[klog: N bytes elided]` notice, then the ring oldest-first. The notice is the
/// point: a gap the reader cannot see is a log that lies about being complete.
///
/// Runs under the ring lock (no allocation). A concurrent `push` may advance the ring
/// between [`len`] and this call; the copy is bounded by both, so the result is a
/// possibly-truncated snapshot, never an overrun.
pub fn copy_into_frames(frames: &[PhysAddr]) -> usize {
    let g = KLOG.lock();
    let cap = frames.len() * PAGE_SIZE;
    let hhdm = heap::hhdm_offset();

    let mut notice = [0u8; NOTICE_MAX];
    let notice_len = if g.elided == 0 { 0 } else { format_notice(g.elided, &mut notice) };

    let mut written = 0usize;
    for src in g.runs(&notice[..notice_len]) {
        let mut off = 0usize;
        while off < src.len() && written < cap {
            let page = written / PAGE_SIZE;
            let intra = written % PAGE_SIZE;
            let chunk = (PAGE_SIZE - intra).min(src.len() - off).min(cap - written);
            let dst = (frames[page].as_u64() + hhdm + intra as u64) as *mut u8;
            // SAFETY: `dst..dst+chunk` is within an owned, HHDM-mapped frame (`page <
            // frames.len()` because `written < cap`, and `intra + chunk <= PAGE`); the
            // source is `chunk` bytes inside this run's slice.
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(off), dst, chunk) };
            off += chunk;
            written += chunk;
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Klog {
        Klog {
            prefix: [0; PREFIX_CAP],
            prefix_len: 0,
            ring: [0; RING_CAP],
            head: 0,
            ring_len: 0,
            elided: 0,
        }
    }

    /// Concatenate the snapshot exactly as `copy_into_frames` would, by walking the
    /// **same** `runs` it walks — so these tests check the production layout rather than a
    /// second copy of the layout logic.
    fn linearise(k: &Klog) -> Vec<u8> {
        let mut notice = [0u8; NOTICE_MAX];
        let notice_len = if k.elided == 0 { 0 } else { format_notice(k.elided, &mut notice) };
        let mut out = Vec::new();
        for run in k.runs(&notice[..notice_len]) {
            out.extend_from_slice(run);
        }
        out
    }

    #[test]
    fn a_short_log_lives_entirely_in_the_prefix() {
        let mut k = fresh();
        k.append(b"boot line one\n");
        k.append(b"boot line two\n");
        assert_eq!(k.ring_len, 0, "the ring is untouched until the prefix fills");
        assert_eq!(k.elided, 0);
        assert_eq!(linearise(&k), b"boot line one\nboot line two\n");
    }

    #[test]
    fn the_prefix_freezes_and_the_overflow_goes_to_the_ring() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        assert_eq!(k.prefix_len, PREFIX_CAP);
        k.append(b"after");
        // The prefix is untouched by later writes — that is the whole point of it.
        assert!(k.prefix.iter().all(|&b| b == b'P'));
        assert_eq!(k.ring_len, 5);
        assert_eq!(k.elided, 0, "nothing overwritten yet");
        let out = linearise(&k);
        assert_eq!(&out[..PREFIX_CAP], &[b'P'; PREFIX_CAP]);
        assert_eq!(&out[PREFIX_CAP..], b"after");
    }

    #[test]
    fn the_ring_keeps_the_most_recent_bytes_and_reports_the_gap() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        // Fill the ring, then push one more byte so exactly one is overwritten.
        k.append(&[b'a'; RING_CAP]);
        assert_eq!(k.elided, 0);
        k.append(b"Z");
        assert_eq!(k.ring_len, RING_CAP);
        assert_eq!(k.elided, 1);
        let out = linearise(&k);
        // Newest byte last, oldest 'a' dropped, and the notice names the gap.
        assert_eq!(*out.last().unwrap(), b'Z');
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("[klog: 1 bytes elided]"), "got: {}", &text[PREFIX_CAP..]);
        assert_eq!(out.iter().filter(|&&b| b == b'a').count(), RING_CAP - 1);
    }

    #[test]
    fn a_write_longer_than_the_ring_keeps_only_its_tail() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        // One write of 1.5 rings: only the last RING_CAP bytes can survive.
        let mut big = vec![b'x'; RING_CAP / 2];
        big.extend(vec![b'y'; RING_CAP]);
        k.append(&big);
        assert_eq!(k.ring_len, RING_CAP);
        assert_eq!(k.elided, RING_CAP / 2, "the skipped head counts as elided");
        let out = linearise(&k);
        assert!(out.ends_with(&[b'y'; 8]));
        assert_eq!(out.iter().filter(|&&b| b == b'x').count(), 0);
    }

    #[test]
    fn snapshot_len_matches_what_linearising_produces() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        k.append(&[b'a'; RING_CAP + 100]);
        // The sizing call and the copy must agree, or `/dev/log` truncates or overruns.
        assert_eq!(k.snapshot_len(), linearise(&k).len());
    }

    #[test]
    fn wrapped_ring_linearises_oldest_first() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        k.append(&[b'a'; RING_CAP - 2]);
        k.append(b"BCDE"); // wraps: 2 bytes at the end, 2 at the front
        let out = linearise(&k);
        let tail = &out[out.len() - 4..];
        assert_eq!(tail, b"BCDE", "wrapped content must come back in write order");
    }
}
