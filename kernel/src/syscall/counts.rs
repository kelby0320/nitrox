//! **Each process's syscalls, counted**, for `cargo xtask syscall-counts`: a kernel built with the
//! `syscall-counts` feature, which no shipped image has.
//!
//! A count per process and number, from the syscall entry. At each spawn and each exit every
//! process's counts since the last are written to the console and zeroed, so a program's run is
//! bounded by its own spawn and exit:
//!
//! ```text
//! SCDUMP spawn pid=7
//! SC pid=12 name=fs-server-ext4 total=82 0:9 10:27 ...
//! SCEND
//! ```
//!
//! The kernel keeps no process's name, so a process is named by its first console line, up to its
//! first `:`. A pid past [`PIDS`] shares a row with one below it.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use crate::mm::user_access::{UserPtr, copy_slice_from_user};
use crate::syscall::table::{SYS_CLOCK_SET, SYS_DEBUG_KPRINT, SYS_PROCESS_EXIT, SYS_PROCESS_SPAWN};

const PIDS: usize = 512;
/// The numbered syscalls, then `debug_kprint`, then any other.
const SLOTS: usize = SYS_CLOCK_SET as usize + 3;
const NAME_WORDS: usize = 3;

static COUNTS: [[AtomicU32; SLOTS]; PIDS] = [const { [const { AtomicU32::new(0) }; SLOTS] }; PIDS];
static NAMES: [[AtomicU64; NAME_WORDS]; PIDS] = [const { [const { AtomicU64::new(0) }; NAME_WORDS] }; PIDS];

/// Count syscall `nr`, called from the entry before it is served: name a process at its first
/// console line, and dump everything at a spawn or an exit.
pub fn before(nr: u64, a0: u64, a1: u64) {
    let pid = crate::sched::current_owner_pid();
    let slot = match nr {
        n if n <= SYS_CLOCK_SET => n as usize,
        SYS_DEBUG_KPRINT => SLOTS - 2,
        _ => SLOTS - 1,
    };
    COUNTS[pid as usize % PIDS][slot].fetch_add(1, Relaxed);
    match nr {
        SYS_DEBUG_KPRINT => name(pid, a0, a1 as usize),
        SYS_PROCESS_SPAWN => dump("spawn", pid),
        SYS_PROCESS_EXIT => dump("exit", pid),
        _ => {}
    }
}

/// Name `pid` by the text before the first `:` of the line at `ptr`, if it has no name yet.
fn name(pid: u32, ptr: u64, len: usize) {
    let words = &NAMES[pid as usize % PIDS];
    let mut buf = [0u8; NAME_WORDS * 8];
    let n = len.min(buf.len());
    let Ok(src) = UserPtr::<u8>::new(ptr) else { return };
    if words[0].load(Relaxed) != 0 || copy_slice_from_user(&mut buf[..n], src).is_err() {
        return;
    }
    let end = buf[..n].iter().position(|&b| b == b':').unwrap_or(0);
    buf[end..].fill(0);
    let word = |i: usize| u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
    if end > 0 && words[0].compare_exchange(0, word(0), Relaxed, Relaxed).is_ok() {
        for (i, w) in words.iter().enumerate().skip(1) {
            w.store(word(i), Relaxed);
        }
    }
}

fn dump(why: &str, who: u32) {
    crate::kprintln!("SCDUMP {why} pid={who}");
    for (pid, row) in COUNTS.iter().enumerate() {
        let total: u64 = row.iter().map(|c| u64::from(c.load(Relaxed))).sum();
        if total == 0 {
            continue;
        }
        let mut name = [0u8; NAME_WORDS * 8];
        for (i, w) in NAMES[pid].iter().enumerate() {
            name[i * 8..i * 8 + 8].copy_from_slice(&w.load(Relaxed).to_le_bytes());
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        let name = core::str::from_utf8(&name[..len]).unwrap_or("?");
        crate::kprint!("SC pid={pid} name={} total={total}", if name.is_empty() { "-" } else { name });
        for (nr, c) in row.iter().enumerate() {
            let n = c.swap(0, Relaxed);
            if n != 0 {
                crate::kprint!(" {nr}:{n}");
            }
        }
        crate::kprintln!("");
    }
    crate::kprintln!("SCEND");
}
