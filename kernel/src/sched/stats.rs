use core::fmt::Write;

use crate::arch::MAX_CPUS;
use crate::libkern::{AllocError, KString};

/// One CPU's monotonic scheduler event counters. Owned by
/// [`SchedState`](super::SchedState) under the rank-1 `SCHED` lock;
/// snapshot-copied by [`stats_snapshot`](super::stats_snapshot).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counters {
    /// Context switches performed by this CPU — every [`switch_into`]
    /// (super), across all four parking dispositions (ready / blocked /
    /// suspended / reap). Nonzero on ≥2 CPUs is the Phase 3 milestone's
    /// "two CPUs visibly active" witness.
    pub switches: u64,
    /// Threads this CPU stole from a peer's ready queue (`steal_one`).
    pub steals: u64,
    /// Runnable threads placed **onto** this CPU's ready queue by spawn
    /// placement or wake re-homing (`place_thread`) — placements *received*,
    /// counted against the target CPU, possibly performed by another.
    pub placed: u64,
    /// Reschedule IPIs handled by this CPU (`on_reschedule_ipi`).
    pub resched_ipis: u64,
    /// Periodic scheduler ticks taken by this CPU (`on_timer_tick`).
    pub ticks: u64,
}

impl Counters {
    /// All-zero counters (`const` — usable in the `SCHED` static initializer).
    pub const ZERO: Counters = Counters { switches: 0, steals: 0, placed: 0, resched_ipis: 0, ticks: 0 };
}

/// One CPU's row in a [`Snapshot`]: its [`Counters`] plus instantaneous
/// state read at capture time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CpuSnapshot {
    /// Whether this CPU's scheduler state is initialized (`cpu_online`).
    pub online: bool,
    /// Whether this CPU was running its idle thread at capture time.
    pub idle: bool,
    /// This CPU's ready-queue length at capture time.
    pub ready: u32,
    /// The monotonic event counters.
    pub counters: Counters,
}

impl CpuSnapshot {
    /// A never-onlined CPU's row (`const` — the snapshot array initializer).
    pub const OFFLINE: CpuSnapshot = CpuSnapshot { online: false, idle: false, ready: 0, counters: Counters::ZERO };
}

/// A consistent point-in-time copy of every CPU's scheduler statistics,
/// captured under one `SCHED` hold by [`stats_snapshot`](super::stats_snapshot).
#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    /// Per-CPU rows, indexed by dense CPU index.
    pub cpus: [CpuSnapshot; MAX_CPUS],
    /// Handle tables the reaper has closed since boot. Machine-wide, not per-CPU —
    /// there is one reaper. Carried *in the snapshot* rather than read inside
    /// [`format`], which is documented pure and host-tested as such.
    pub reaper_closed: u64,
    /// Handles closed by processes in their own exit syscall (exit-context teardown).
    pub exit_closed: u64,
}

impl Snapshot {
    /// The number of online CPUs in this snapshot.
    pub fn cpus_online(&self) -> usize {
        self.cpus.iter().filter(|c| c.online).count()
    }
}

/// Render `snap` as the `/proc/sched/stats` text: a `cpus_online=N` header
/// line, then one `name=value` row per **online** CPU (offline CPUs are
/// omitted — their counters are zero by construction). Booleans render as
/// `0`/`1`. Pure (host-tested); the only failure is heap exhaustion.
///
/// ```text
/// cpus_online=2
/// reaper_closed=17
/// exit_closed=214
/// cpu=0 online=1 switches=1342 steals=3 placed=57 ipis=12 ticks=4096 ready=1 idle=0
/// cpu=1 online=1 switches=987 steals=11 placed=40 ipis=9 ticks=4080 ready=0 idle=1
/// ```
pub fn format(snap: &Snapshot) -> Result<KString, AllocError> {
    let mut out = KString::new();
    // `fmt::Error` from a `KString` sink only ever means allocation failure
    // (see `KString`'s `Write` impl).
    (|| -> Result<(), core::fmt::Error> {
        writeln!(out, "cpus_online={}", snap.cpus_online())?;
        writeln!(out, "reaper_closed={}", snap.reaper_closed)?;
        writeln!(out, "exit_closed={}", snap.exit_closed)?;
        for (c, cpu) in snap.cpus.iter().enumerate() {
            if !cpu.online {
                continue;
            }
            writeln!(
                out,
                "cpu={} online=1 switches={} steals={} placed={} ipis={} ticks={} ready={} idle={}",
                c,
                cpu.counters.switches,
                cpu.counters.steals,
                cpu.counters.placed,
                cpu.counters.resched_ipis,
                cpu.counters.ticks,
                cpu.ready,
                cpu.idle as u8,
            )?;
        }
        Ok(())
    })()
    .map_err(|_| AllocError)?;
    Ok(out)
}
