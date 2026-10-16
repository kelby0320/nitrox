use super::*;

/// The set of online CPUs as a bitmask (dense index → bit), read lock-free.
/// Used by [`crate::tlb`] to pick TLB-shootdown IPI targets without taking `SCHED`.
pub fn online_mask() -> u64 {
    online_set::load()
}

/// Drop this CPU from the online set, because it is about to stop executing forever.
///
/// **Every permanent park must call this, and the reason is a whole-machine deadlock.**
/// `online_mask` is the target set for TLB-shootdown IPIs, and [`crate::tlb::shootdown`]
/// waits for an acknowledgement from *every* CPU in it. A CPU that halts while still
/// counted online can never send that acknowledgement, so the next shootdown — which is
/// any `sys_memory_unmap`, i.e. any large `free` in any process — spins forever, taking
/// the initiating thread with it and, through the shootdown lock, every later one.
///
/// That is not hypothetical: it is exactly how a parked CPU froze `nxterm` mid-repaint
/// under `check-terminal` (2026-08-13). The CPU had halted with interrupts disabled in
/// `debug_exit`'s device-absent fallback ~6,000 ticks earlier, and nothing noticed until
/// a glyph outline was freed.
///
/// **Does nothing unless this CPU's identity is its own.** A CPU that has not yet
/// established one reports index `0` (x86's `IA32_TSC_AUX` reset default), so an
/// unchecked version of this clears *the BSP's* bit when an AP with an unbound APIC id
/// parks — leaving the running BSP out of the shootdown set, where the slow path's
/// self-IPI is the only thing that invalidates its own TLB. The result is a freed frame
/// still reachable through a stale translation: silent memory corruption, strictly worse
/// than the deadlock this function exists to prevent (PR #198 review, blocking 1).
/// [`ArchSmp::identity_bound`](crate::arch::smp::ArchSmp::identity_bound) is that check;
/// the range check below is a separate, weaker guard and does not cover this — an unbound
/// CPU's `0` is in range.
///
/// **The check errs toward doing nothing, including once where it arguably should act.**
/// The BSP's bit is set by [`init`] (`main.rs:353`) before `bind_cpu_identity(0, …)`
/// (`main.rs:432`), so a BSP panic in that window finds its own identity unbound and leaves
/// its bit set. That is harmless rather than lucky: APs are not launched until after the
/// binding, so in that window no other CPU exists to initiate a shootdown and wait on it.
/// The asymmetry is deliberate — failing to clear a bit costs a hang on a machine that has
/// already panicked, while clearing the wrong one costs silent memory corruption on a
/// healthy one.
pub fn leave_online() {
    use crate::arch::smp::ArchSmp;
    clear_online_bit(crate::arch::Smp::identity_bound(), crate::arch::Smp::current_cpu() as usize);
}

/// The decision half of [`leave_online`], split out so the rule that cost a review round is
/// pinned by a host test rather than only by a comment.
pub(super) fn clear_online_bit(identity_bound: bool, me: usize) {
    if !identity_bound || me >= MAX_CPUS {
        return;
    }
    online_set::remove(me);
}

/// The number of CPUs whose scheduler state is online (BSP + initialized APs).
pub fn online_cpus() -> usize {
    SCHED.lock().cpu_online.iter().filter(|&&o| o).count()
}
