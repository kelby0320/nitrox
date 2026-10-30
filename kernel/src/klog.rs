//! The kernel log ring: a bounded in-memory capture of kernel `kprint!` output.
//!
//! Every kernel diagnostic written through `kprint!` / `kprintln!` (the serial `write_str` path)
//! is **teed** into a fixed-size buffer here, besides the serial console. A reader gets it as a
//! `MemoryObject` snapshot bound at `/dev/log` (`KernelServerId::Log`): `cat /dev/log` is
//! `dmesg`. It captures **kernel** messages, not userspace `sys_kprint` output.
//!
//! ## Buffer model: keep both ends
//!
//! - a **frozen prefix** captures from boot until [`PREFIX_CAP`] and is then never overwritten:
//!   the early-boot, first-failure context;
//! - a **keep-recent ring** takes everything after it, so a long-running system retains its
//!   *latest* output.
//!
//! Neither property is traded for the other; what is lost is the *middle*, and the reader is told
//! how much (see [`copy_into_object`]). `PREFIX_CAP` is sized from a measurement: a full
//! integration boot captured 3801 bytes and dropped nothing.
//!
//! ## Tests
//!
//! The layout lives in one place ([`Klog::runs`]) that both the production copy and the host
//! tests walk, so the tests cannot pass against a layout production does not use. The host tests
//! cannot reach the page-wise copy into physical frames (it needs HHDM-mapped memory); a boot
//! exercises it for the prefix-only case, since the boot chain reads `/dev/log`.
//!
//! ## Locking
//!
//! All state is behind an [`IrqSpinLock`]. [`push`] uses **`try_lock`** and skips the line if
//! contended, so teeing from the panic/exception path (which also flows through `write_str`) can
//! never deadlock against a fault that strikes while the ring lock is held. The reader paths
//! ([`len`] with [`copy_into_object`] for `/dev/log`, [`copy_into`] for the hardware report) are
//! thread context and block on the lock normally.

use crate::libkern::IrqSpinLock;
use crate::libkern::lockrank::LockRank;
use crate::object::MemoryObject;

/// Total capacity of the kernel log buffer (bytes). 16 KiB = 4 pages.
pub const KLOG_CAP: usize = 16 * 1024;

/// Bytes reserved for the never-overwritten boot prefix: better than 2x the measured boot log. If
/// the log outgrows it the *overflow* lands in the ring (nothing is lost at the time) and the
/// prefix stops being a complete boot log, which the `klog:` line in a `test-harness` run shows.
pub const PREFIX_CAP: usize = 8 * 1024;

/// Bytes of keep-recent ring — everything after the prefix.
pub const RING_CAP: usize = KLOG_CAP - PREFIX_CAP;

struct Klog {
    /// Boot prefix: `prefix[..prefix_len]`, frozen once `prefix_len == PREFIX_CAP`. Raw bytes:
    /// newlines are bare `\n` (the reader's `sys_kprint` translates them for the terminal).
    prefix: [u8; PREFIX_CAP],
    prefix_len: usize,
    /// Keep-recent ring: `ring_len` bytes are live; once full, `head` is the index of both the next
    /// write and the oldest live byte.
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
        if self.prefix_len < PREFIX_CAP {
            let n = rest.len().min(PREFIX_CAP - self.prefix_len);
            self.prefix[self.prefix_len..self.prefix_len + n].copy_from_slice(&rest[..n]);
            self.prefix_len += n;
            rest = &rest[n..];
        }
        if rest.is_empty() {
            return;
        }
        // A write longer than the whole ring can only leave its tail: skip to the last `RING_CAP`
        // bytes rather than copying through the ring repeatedly.
        if rest.len() >= RING_CAP {
            let skip = rest.len() - RING_CAP;
            self.elided += self.ring_len.min(RING_CAP) + skip;
            self.ring.copy_from_slice(&rest[skip..]);
            self.head = 0;
            self.ring_len = RING_CAP;
            return;
        }
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

    /// The snapshot's byte runs, in order: boot prefix, elision notice, then the ring oldest-first
    /// (in two runs when wrapped). Empty runs are included so the shape is fixed.
    ///
    /// **The single definition of the snapshot's layout**, consumed by [`copy_into_object`] and the
    /// host tests alike: a test that re-derived the layout it checks would pass while production
    /// got it wrong.
    fn runs<'a>(&'a self, notice: &'a [u8]) -> [&'a [u8]; 4] {
        let start = self.ring_start();
        let (a, b) = if self.ring_len == 0 {
            (&self.ring[..0], &self.ring[..0])
        } else if start + self.ring_len <= RING_CAP {
            (&self.ring[start..start + self.ring_len], &self.ring[..0])
        } else {
            let tail = RING_CAP - start;
            (&self.ring[start..], &self.ring[..self.ring_len - tail])
        };
        [&self.prefix[..self.prefix_len], notice, a, b]
    }

    /// Index of the oldest live ring byte.
    fn ring_start(&self) -> usize {
        if self.ring_len < RING_CAP { 0 } else { self.head }
    }

    /// Total bytes a linearised snapshot occupies: prefix + elision notice + ring.
    fn snapshot_len(&self) -> usize {
        self.prefix_len + self.notice_len() + self.ring_len
    }

    /// Hand the snapshot (the same [`runs`](Self::runs)) to `put` as `(offset, bytes)`, up to `cap`.
    fn copy_with(&self, cap: usize, mut put: impl FnMut(usize, &[u8])) -> usize {
        let mut notice = [0u8; NOTICE_MAX];
        let notice_len = if self.elided == 0 { 0 } else { format_notice(self.elided, &mut notice) };
        let mut written = 0;
        for run in self.runs(&notice[..notice_len]) {
            let n = run.len().min(cap - written);
            put(written, &run[..n]);
            written += n;
        }
        written
    }

    fn copy_into(&self, out: &mut [u8]) -> usize {
        self.copy_with(out.len(), |at, run| out[at..at + run.len()].copy_from_slice(run))
    }

    fn copy_into_object(&self, obj: &MemoryObject) -> usize {
        self.copy_with(obj.size(), |at, run| obj.copy_in(at, run))
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
/// Hand-formatted rather than `write!`-ed: this runs under the ring lock, in `no_std`, with no
/// allocation permitted.
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
    Klog { prefix: [0; PREFIX_CAP], prefix_len: 0, ring: [0; RING_CAP], head: 0, ring_len: 0, elided: 0 },
);

/// Append `bytes` to the kernel log (called from the serial `write_str` tee).
///
/// Once the buffer fills the prefix freezes and the ring keeps the most recent output. **Skips
/// silently if the lock is contended** (a fault mid-`push`, re-entered via the emergency writer):
/// logging is best-effort and must never deadlock the panic path.
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

/// `(prefix_len, ring_len, elided)`: the measurement behind [`PREFIX_CAP`], reported by a
/// `test-harness` run so an outgrown prefix is visible.
pub fn stats() -> (usize, usize, usize) {
    let g = KLOG.lock();
    (g.prefix_len, g.ring_len, g.elided)
}

/// Copy the linearised log into `out`, as [`copy_into_object`] lays it out, stopping when `out` is
/// full. Returns the bytes copied. The hardware report's read; size `out` with [`len`]. Runs under
/// the ring lock, which masks interrupts for the copy: at most [`KLOG_CAP`] bytes and a notice.
pub fn copy_into(out: &mut [u8]) -> usize {
    KLOG.lock().copy_into(out)
}

/// Copy the linearised log into `obj`, the `/dev/log` snapshot, up to its size; returns the count.
///
/// The layout is boot prefix, then a `[klog: N bytes elided]` notice if any middle was overwritten,
/// then the ring oldest-first: a gap the reader cannot see is a log that lies about being complete.
///
/// Runs under the ring lock (no allocation). A concurrent `push` may advance the ring between
/// [`len`] and this call; the copy is bounded by both, so the result is a possibly-truncated
/// snapshot, never an overrun.
pub fn copy_into_object(obj: &MemoryObject) -> usize {
    KLOG.lock().copy_into_object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> Klog {
        Klog { prefix: [0; PREFIX_CAP], prefix_len: 0, ring: [0; RING_CAP], head: 0, ring_len: 0, elided: 0 }
    }

    /// Concatenate the snapshot by walking the **same** `runs` production walks, so these tests
    /// check the production layout.
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
        k.append(&[b'a'; RING_CAP]);
        assert_eq!(k.elided, 0);
        k.append(b"Z");
        assert_eq!(k.ring_len, RING_CAP);
        assert_eq!(k.elided, 1);
        let out = linearise(&k);
        assert_eq!(*out.last().unwrap(), b'Z');
        let text = String::from_utf8_lossy(&out).into_owned();
        assert!(text.contains("[klog: 1 bytes elided]"), "got: {}", &text[PREFIX_CAP..]);
        assert_eq!(out.iter().filter(|&&b| b == b'a').count(), RING_CAP - 1);
    }

    #[test]
    fn a_write_longer_than_the_ring_keeps_only_its_tail() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
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
        assert_eq!(k.snapshot_len(), linearise(&k).len());
    }

    #[test]
    fn the_buffer_copy_is_the_snapshot_and_stops_when_the_buffer_is_full() {
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        k.append(&[b'a'; RING_CAP - 2]);
        k.append(b"BCDE"); // wrapped, and two bytes elided
        let whole = linearise(&k);
        let mut out = vec![0u8; k.snapshot_len() + 10];
        assert_eq!(k.copy_into(&mut out), whole.len());
        assert_eq!(&out[..whole.len()], &whole[..]);
        let mut short = vec![0u8; PREFIX_CAP + 5];
        assert_eq!(k.copy_into(&mut short), PREFIX_CAP + 5);
        assert_eq!(&short[..], &whole[..PREFIX_CAP + 5]);
    }

    /// **The `/dev/log` fill writes its object and nothing else**, cut at the object's size.
    #[test]
    fn the_log_is_copied_into_its_object_cut_at_its_size() {
        use crate::mm::PAGE_SIZE;
        crate::mm::test_support::init_global_heap();
        let mut k = fresh();
        k.append(&[b'P'; PREFIX_CAP]);
        k.append(b"after");
        let whole = linearise(&k);
        assert!(whole.len() > PAGE_SIZE);

        let page = MemoryObject::try_new(PAGE_SIZE).unwrap();
        assert_eq!(k.copy_into_object(&page), PAGE_SIZE);
        assert_eq!(page.copy_to_kvec().unwrap()[..], whole[..PAGE_SIZE]);

        let room = MemoryObject::try_new(whole.len() + 1).unwrap();
        assert_eq!(k.copy_into_object(&room), whole.len());
        let back = room.copy_to_kvec().unwrap();
        assert_eq!(back[..whole.len()], whole[..]);
        assert!(back[whole.len()..].iter().all(|&b| b == 0));
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
