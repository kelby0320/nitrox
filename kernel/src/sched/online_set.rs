    pub(in crate::sched) use backing::*;

    #[cfg(not(test))]
    mod backing {
        use core::sync::atomic::{AtomicU64, Ordering};

        static ONLINE_MASK: AtomicU64 = AtomicU64::new(0);

        pub(in crate::sched) fn load() -> u64 {
            ONLINE_MASK.load(Ordering::Acquire)
        }
        pub(in crate::sched) fn add(cpu: usize) {
            ONLINE_MASK.fetch_or(1u64 << cpu, Ordering::Release);
        }
        pub(in crate::sched) fn remove(cpu: usize) {
            ONLINE_MASK.fetch_and(!(1u64 << cpu), Ordering::AcqRel);
        }
    }

    #[cfg(test)]
    mod backing {
        std::thread_local! {
            static ONLINE_MASK: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
        }

        pub(in crate::sched) fn load() -> u64 {
            ONLINE_MASK.with(|m| m.get())
        }
        pub(in crate::sched) fn add(cpu: usize) {
            ONLINE_MASK.with(|m| m.set(m.get() | (1u64 << cpu)));
        }
        pub(in crate::sched) fn remove(cpu: usize) {
            ONLINE_MASK.with(|m| m.set(m.get() & !(1u64 << cpu)));
        }
        /// Establish a starting set. Test-only: the machine reaches its set by booting.
        pub(in crate::sched) fn force(mask: u64) {
            ONLINE_MASK.with(|m| m.set(mask));
        }
    }
