//! Per-CPU state: a CPU's dense index, the pin under which it is read, and the arrays it reaches.

use core::cell::UnsafeCell;
use core::marker::PhantomData;

use crate::arch::MAX_CPUS;

/// A dense CPU index, below [`MAX_CPUS`] by construction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CpuIndex(u32);

impl CpuIndex {
    /// The boot processor's.
    pub const BSP: Self = Self(0);

    pub(in crate::arch) const fn new(i: usize) -> Option<Self> {
        if i < MAX_CPUS { Some(Self(i as u32)) } else { None }
    }

    /// The running CPU's, under `pin`; unchecked in release, as each write of the index is bounded.
    pub fn current(pin: &CpuPin<'_>) -> Self {
        pin.debug_check();
        let cpu = super::raw_current_cpu();
        debug_assert!((cpu as usize) < MAX_CPUS, "cpu index {cpu} past MAX_CPUS");
        Self(cpu)
    }

    /// As an array index.
    pub const fn get(self) -> usize {
        self.0 as usize
    }
}

/// A borrow under which the running thread stays on its CPU.
pub struct CpuPin<'a>(PhantomData<(&'a (), *const ())>);

impl CpuPin<'_> {
    /// # Safety
    /// This CPU's interrupts stay masked, or its preemption disabled, while the pin lives.
    pub unsafe fn assume() -> Self {
        Self(PhantomData)
    }

    fn debug_check(&self) {
        debug_assert!(
            crate::libkern::irqs::masked() || crate::sched::preemption_disabled(),
            "a CPU pin with interrupts on and preemption enabled"
        );
    }
}

/// One `T` per CPU, by raw slot alone; shared values are [`cpu_local!`]'s, so no write meets a borrow.
pub struct PerCpu<T>(UnsafeCell<[T; MAX_CPUS]>);

// SAFETY: nothing is reached but through `slot`'s raw pointers, each dereference its caller's to
// argue; a `T` moves to whichever CPU's thread writes its slot.
unsafe impl<T: Send> Sync for PerCpu<T> {}

impl<T> PerCpu<T> {
    /// `slots[i]` for CPU `i`.
    pub const fn new(slots: [T; MAX_CPUS]) -> Self {
        Self(UnsafeCell::new(slots))
    }

    /// CPU `cpu`'s slot, bounds-checked.
    pub fn slot(&self, cpu: CpuIndex) -> *mut T {
        let all = self.0.get();
        // SAFETY: `all` addresses this live array; the place is projected, not read or borrowed.
        unsafe { &raw mut (*all)[cpu.get()] }
    }
}

/// One shared `T` per CPU, from [`cpu_local!`]; a host test's thread has a set of its own.
pub struct CpuLocalKey<T: 'static> {
    #[cfg(not(test))]
    slots: [T; MAX_CPUS],
    #[cfg(test)]
    twin: &'static std::thread::LocalKey<[T; MAX_CPUS]>,
}

impl<T: Sync> CpuLocalKey<T> {
    #[cfg(not(test))]
    pub const fn new(slots: [T; MAX_CPUS]) -> Self {
        Self { slots }
    }

    #[cfg(test)]
    pub const fn new(twin: &'static std::thread::LocalKey<[T; MAX_CPUS]>) -> Self {
        Self { twin }
    }

    /// `f` on the running CPU's, which `pin` keeps the thread on.
    pub fn with<R>(&'static self, pin: &CpuPin<'_>, f: impl FnOnce(&T) -> R) -> R {
        self.remote(CpuIndex::current(pin).get(), f)
    }

    /// `f` on CPU `cpu`'s, bounds-checked.
    pub fn remote<R>(&'static self, cpu: usize, f: impl FnOnce(&T) -> R) -> R {
        #[cfg(not(test))]
        return f(&self.slots[cpu]);
        #[cfg(test)]
        self.twin.with(|slots| f(&slots[cpu]))
    }
}

/// Declare a [`CpuLocalKey`]: `static NAME: T = const_init;`.
macro_rules! cpu_local {
    ($(#[$attr:meta])* $vis:vis static $name:ident: $ty:ty = $init:expr;) => {
        $(#[$attr])*
        #[cfg(not(test))]
        $vis static $name: $crate::arch::percpu::CpuLocalKey<$ty> =
            $crate::arch::percpu::CpuLocalKey::new([const { $init }; $crate::arch::MAX_CPUS]);
        $(#[$attr])*
        #[cfg(test)]
        $vis static $name: $crate::arch::percpu::CpuLocalKey<$ty> = {
            std::thread_local! {
                static TWIN: [$ty; $crate::arch::MAX_CPUS] = const { [const { $init }; $crate::arch::MAX_CPUS] };
            }
            $crate::arch::percpu::CpuLocalKey::new(&TWIN)
        };
    };
}
pub(crate) use cpu_local;

#[cfg(test)]
mod tests;
