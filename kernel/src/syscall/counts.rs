//! **Each process's syscalls, counted**, for `cargo xtask syscall-counts`: a kernel built with the
//! `syscall-counts` feature, which no shipped image has.
//!
//! A count per process and number, from the syscall entry. At each spawn and each exit every
//! process's counts since the last are written to the console and zeroed, so a program's run is
//! bounded by its own spawn and exit. Each row is one write, and carries its dump's `seq`, so a row
//! is whole and its dump known whatever another CPU writes meanwhile:
//!
//! ```text
//! SCDUMP spawn pid=7 seq=3
//! SC seq=3 pid=12 name=fs-server total=82 0:9 10:27 kprint:2 ...
//! SCEND seq=3
//! ```
//!
//! The kernel keeps no process's name, so a process is named by its first console line, up to its
//! first `:`. A pid past [`PIDS`] shares a row with one below it, the row showing the last.

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::mm::user_access::{UserPtr, copy_slice_from_user};
use crate::syscall::table::{SYS_CLOCK_SET, SYS_DEBUG_KPRINT, SYS_PROCESS_EXIT, SYS_PROCESS_SPAWN};

const PIDS: usize = 512;
/// The numbered syscalls, then `debug_kprint`, then any other, by name.
const SLOTS: usize = SYS_CLOCK_SET as usize + 3;
const NAME_WORDS: usize = 3;

static COUNTS: [[AtomicU32; SLOTS]; PIDS] = [const { [const { AtomicU32::new(0) }; SLOTS] }; PIDS];
static NAMES: [[AtomicU64; NAME_WORDS]; PIDS] = [const { [const { AtomicU64::new(0) }; NAME_WORDS] }; PIDS];
static OWNERS: [AtomicU32; PIDS] = [const { AtomicU32::new(0) }; PIDS];
static SEQ: AtomicU64 = AtomicU64::new(0);

/// The slot syscall `nr` counts in.
fn slot(nr: u64) -> usize {
    match nr {
        n if n <= SYS_CLOCK_SET => n as usize,
        SYS_DEBUG_KPRINT => SLOTS - 2,
        _ => SLOTS - 1,
    }
}

/// Count syscall `nr`, called from the entry before it is served: name a process at its first
/// console line, and dump everything at a spawn or an exit.
pub fn before(nr: u64, a0: u64, a1: u64) {
    let pid = crate::sched::current_owner_pid();
    COUNTS[pid as usize % PIDS][slot(nr)].fetch_add(1, Relaxed);
    OWNERS[pid as usize % PIDS].store(pid, Relaxed);
    match nr {
        SYS_DEBUG_KPRINT => name(pid, a0, a1 as usize),
        SYS_PROCESS_SPAWN => dump("spawn", pid),
        SYS_PROCESS_EXIT => dump("exit", pid),
        _ => {}
    }
}

/// The text before a line's first `:`, or nothing if none falls in `line`.
fn name_of(line: &[u8]) -> &[u8] {
    line.iter().position(|&b| b == b':').map_or(&[], |end| &line[..end])
}

/// Name `pid` by its line at `ptr`, if it has no name yet.
fn name(pid: u32, ptr: u64, len: usize) {
    let words = &NAMES[pid as usize % PIDS];
    let mut buf = [0u8; NAME_WORDS * 8];
    let n = len.min(buf.len());
    let Ok(src) = UserPtr::<u8>::new(ptr) else { return };
    if words[0].load(Relaxed) != 0 || copy_slice_from_user(&mut buf[..n], src).is_err() {
        return;
    }
    let end = name_of(&buf[..n]).len();
    buf[end..].fill(0);
    let word = |i: usize| u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    if end > 0 && words[0].compare_exchange(0, word(0), Relaxed, Relaxed).is_ok() {
        for (i, w) in words.iter().enumerate().skip(1) {
            w.store(word(i), Relaxed);
        }
    }
}

/// A row's text, built whole before one write.
struct Row {
    buf: [u8; 1024],
    len: usize,
}

impl Write for Row {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(core::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// One process's row of dump `seq`: its pid, its name or `-`, its total, and each non-zero count
/// by its number, the last two slots as `kprint` and `other`.
fn row(seq: u64, pid: u32, name: &str, counts: &[u32; SLOTS]) -> Row {
    let mut r = Row { buf: [0; 1024], len: 0 };
    let total: u64 = counts.iter().map(|&n| u64::from(n)).sum();
    let name = if name.is_empty() { "-" } else { name };
    let _ = write!(r, "SC seq={seq} pid={pid} name={name} total={total}");
    for (nr, &n) in counts.iter().enumerate().filter(|(_, n)| **n != 0) {
        let _ = if nr == SLOTS - 2 {
            write!(r, " kprint:{n}")
        } else if nr == SLOTS - 1 {
            write!(r, " other:{n}")
        } else {
            write!(r, " {nr}:{n}")
        };
    }
    r
}

fn dump(why: &str, who: u32) {
    let seq = SEQ.fetch_add(1, Relaxed);
    crate::kprintln!("SCDUMP {why} pid={who} seq={seq}");
    for (i, slots) in COUNTS.iter().enumerate() {
        let counts: [u32; SLOTS] = core::array::from_fn(|nr| slots[nr].swap(0, Relaxed));
        if counts.iter().all(|&n| n == 0) {
            continue;
        }
        let mut name = [0u8; NAME_WORDS * 8];
        for (w, word) in NAMES[i].iter().enumerate() {
            name[w * 8..w * 8 + 8].copy_from_slice(&word.load(Relaxed).to_le_bytes());
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let r = row(seq, OWNERS[i].load(Relaxed), core::str::from_utf8(&name[..len]).unwrap_or("?"), &counts);
        crate::kprintln!("{}", core::str::from_utf8(&r.buf[..r.len]).unwrap_or("SC ?"));
    }
    crate::kprintln!("SCEND seq={seq}");
}

/// A row as [`row`] writes it, which `cargo xtask syscall-counts`'s parser is tested against.
#[cfg(test)]
pub(crate) const EXAMPLE_ROW: &str = "SC seq=3 pid=12 name=fs-server total=6 0:1 10:2 kprint:2 other:1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_number_has_its_slot_and_the_debug_and_other_calls_theirs() {
        assert_eq!((slot(0), slot(SYS_CLOCK_SET)), (0, SYS_CLOCK_SET as usize));
        assert_eq!(slot(SYS_DEBUG_KPRINT), SLOTS - 2);
        assert_eq!((slot(SYS_CLOCK_SET + 1), slot(0xFFFF_0002)), (SLOTS - 1, SLOTS - 1));
    }

    #[test]
    fn a_row_is_written_whole_and_names_its_last_two_slots() {
        let mut counts = [0; SLOTS];
        (counts[0], counts[10], counts[SLOTS - 2], counts[SLOTS - 1]) = (1, 2, 2, 1);
        let r = row(3, 12, "fs-server", &counts);
        assert_eq!(core::str::from_utf8(&r.buf[..r.len]), Ok(EXAMPLE_ROW));
        let r = row(0, 1, "", &[u32::MAX; SLOTS]);
        assert!(r.len < r.buf.len() && r.buf[..r.len].starts_with(b"SC seq=0 pid=1 name=- "), "every slot full fits");
    }

    #[test]
    fn a_process_is_named_by_its_line_up_to_its_first_colon() {
        assert_eq!(name_of(b"fs-server: up"), b"fs-server");
        assert_eq!(name_of(b"no colon here"), b"");
        assert_eq!(name_of(b": empty"), b"");
    }
}
