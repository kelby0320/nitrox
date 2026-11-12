//! This CPU's interrupts masked, as a guard that pins its thread to the CPU.

use core::marker::PhantomData;

use crate::arch::percpu::CpuPin;

/// This CPU's interrupts masked, by [`mask`](Self::mask) or as [`observed`](Self::observed).
#[must_use = "a mask is undone as it drops"]
pub struct IrqsOff {
    restore: Option<bool>,
    _here: PhantomData<*const ()>,
}

impl IrqsOff {
    /// Mask this CPU's interrupts until the guard drops, which restores what was.
    pub fn mask() -> Self {
        Self::counted(Some(backend::disable()))
    }

    /// Interrupts masked already, as a `syscall` or an interrupt leaves them, checked in every build.
    #[track_caller]
    pub fn observed() -> Self {
        assert!(!backend::enabled(), "interrupts observed masked, and they were on");
        Self::counted(None)
    }

    fn counted(restore: Option<bool>) -> Self {
        let off = Self { restore, _here: PhantomData };
        live::up(&off.pin());
        off
    }

    /// The pin the mask lends.
    pub fn pin(&self) -> CpuPin<'_> {
        // SAFETY: interrupts are masked while `self` lives, as nothing unmasks inside one, and a
        // switch, which would move the thread with them masked, refuses a live one.
        unsafe { CpuPin::assume() }
    }
}

impl Drop for IrqsOff {
    fn drop(&mut self) {
        live::down(&self.pin());
        if let Some(prev) = self.restore {
            backend::restore(prev);
        }
    }
}

/// Whether this CPU's interrupts are masked: a host test's, the modelled flag.
pub fn masked() -> bool {
    !backend::enabled()
}

/// Debug builds: no [`IrqsOff`] lives on this CPU, at a switch: masked interrupts do not keep a
/// thread on its CPU across one, and no other check there sees an `IrqsOff`.
pub fn assert_no_live_masks(pin: &CpuPin<'_>) {
    debug_assert_eq!(live::count(pin), 0, "a switch with an `IrqsOff` live");
}

#[cfg(debug_assertions)]
mod live {
    use core::sync::atomic::{AtomicU32, Ordering};

    use crate::arch::percpu::{CpuPin, cpu_local};

    cpu_local! {
        static LIVE: AtomicU32 = AtomicU32::new(0);
    }

    pub(super) fn up(pin: &CpuPin<'_>) {
        LIVE.with(pin, |n| n.fetch_add(1, Ordering::Relaxed));
    }

    pub(super) fn down(pin: &CpuPin<'_>) {
        LIVE.with(pin, |n| n.fetch_sub(1, Ordering::Relaxed));
    }

    pub(super) fn count(pin: &CpuPin<'_>) -> u32 {
        LIVE.with(pin, |n| n.load(Ordering::Relaxed))
    }
}

#[cfg(not(debug_assertions))]
mod live {
    use crate::arch::percpu::CpuPin;

    #[inline(always)]
    pub(super) fn up(_: &CpuPin<'_>) {}

    #[inline(always)]
    pub(super) fn down(_: &CpuPin<'_>) {}

    #[inline(always)]
    pub(super) fn count(_: &CpuPin<'_>) -> u32 {
        0
    }
}

/// Interrupt-flag backend for [`IrqSpinLock`](crate::libkern::IrqSpinLock).
///
/// On target, this drives the real CPU interrupt flag through the arch layer. Under
/// `cfg(test)` (host ring 3) the real `cli`/`sti` would `#GP`, so a mock models `IF` in a
/// **per-thread** `Cell<bool>`, enough to exercise the lock's save/restore *logic*.
#[cfg(not(test))]
pub(in crate::libkern) mod backend {
    use crate::arch::Cpu;
    use crate::arch::cpu::ArchCpu;

    #[inline]
    pub(in crate::libkern) fn disable() -> bool {
        Cpu::interrupts_disable()
    }

    #[inline]
    pub(in crate::libkern) fn restore(prev: bool) {
        // SAFETY: ring-0; restoring an interrupt state captured by `disable`.
        unsafe { Cpu::interrupts_restore(prev) }
    }

    #[inline]
    pub(super) fn enabled() -> bool {
        Cpu::interrupts_enabled()
    }
}

#[cfg(test)]
pub(in crate::libkern) mod backend {
    std::thread_local! {
        /// Models `RFLAGS.IF` for host tests, **one flag per thread**. Starts enabled (the
        /// boot default once preemption is armed).
        ///
        /// Per-thread because `IF` *is* per-CPU: a process-global flag is a machine where any
        /// core's `cli` masks interrupts on all of them, and a host test thread stands in for a
        /// core. `cargo test` runs tests concurrently, so a global flag let one test's write fail
        /// `irq_lock_masks_while_held_and_restores_on_drop`.
        static MOCK_IF: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
    }

    pub(in crate::libkern) fn disable() -> bool {
        MOCK_IF.with(|f| f.replace(false))
    }

    pub(in crate::libkern) fn restore(prev: bool) {
        MOCK_IF.with(|f| f.set(prev));
    }

    pub(in crate::libkern) fn enabled() -> bool {
        MOCK_IF.with(|f| f.get())
    }

    /// Establish a prior interrupt state before acquiring, for tests that need one.
    pub(in crate::libkern) fn force(enabled: bool) {
        MOCK_IF.with(|f| f.set(enabled));
    }
}

/// Set this test thread's modelled interrupt flag.
#[cfg(test)]
pub fn force(enabled: bool) {
    backend::force(enabled);
}

#[cfg(test)]
mod tests;
