//! The device vectors: a [`VectorPool`] of handlers the device-IRQ stubs dispatch through, and a
//! [`DeviceVector`] taken from it, given back only while nothing may raise it.

use core::sync::atomic::{AtomicUsize, Ordering};

use super::idt::{DEVICE_IRQ_BASE, DEVICE_IRQ_COUNT};
use super::ioapic::X86IoApic;
use crate::arch::irq_install::InstallError;
use crate::arch::irq_router::ArchIrqRouter;

/// **A handler a device vector**, a function pointer in a `usize`, `0` while free: taken at
/// bring-up, read in IRQ context, lock-free. A value, so a host test has a pool of its own.
pub(crate) struct VectorPool {
    handlers: [AtomicUsize; DEVICE_IRQ_COUNT],
}

/// The pool the stubs dispatch through.
pub(crate) static DEVICES: VectorPool = VectorPool::new();

impl VectorPool {
    pub(crate) const fn new() -> Self {
        VectorPool { handlers: [const { AtomicUsize::new(0) }; DEVICE_IRQ_COUNT] }
    }

    /// **A free vector for `handler`**, held as `held` says; [`InstallError::PoolFull`] with none.
    pub(crate) fn take(&'static self, handler: extern "C" fn(), held: Held) -> Result<DeviceVector, InstallError> {
        let free = |h: &AtomicUsize| h.compare_exchange(0, handler as usize, Ordering::AcqRel, Ordering::Relaxed);
        let slot = self.handlers.iter().position(|h| free(h).is_ok()).ok_or(InstallError::PoolFull)?;
        Ok(DeviceVector { vector: DEVICE_IRQ_BASE + slot as u8, pool: self, held })
    }

    /// The handler of the vector `slot` places past the first, `0` for none.
    pub(super) fn handler(&self, slot: usize) -> usize {
        self.handlers.get(slot).map_or(0, |h| h.load(Ordering::Acquire))
    }

    fn give_back(&self, vector: u8) {
        self.handlers[(vector - DEVICE_IRQ_BASE) as usize].store(0, Ordering::Release);
    }
}

/// What may raise a taken vector, which decides whether dropping it gives it back.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Held {
    /// Nothing, until its taker raises it: given back.
    Idle,
    /// A device handed an MSI message for it: kept, unless [`DeviceVector::give_back`] says none was.
    Msi,
    /// The line with this GSI, routed to it: masked, then kept, as an interrupt may be on its way.
    Line(u32),
}

/// **A device vector, its handler registered**, given back when dropped only while nothing may
/// raise it ([`Held`]); [`commit`](Self::commit) keeps it for good.
#[must_use]
pub struct DeviceVector {
    vector: u8,
    pool: &'static VectorPool,
    held: Held,
}

impl DeviceVector {
    /// The vector's number.
    pub fn vector(&self) -> u8 {
        self.vector
    }

    /// Keep it for the kernel's life: the device it serves is claimed.
    pub fn commit(self) -> u8 {
        core::mem::ManuallyDrop::new(self).vector
    }

    /// Its line, `gsi`, routed to it.
    pub(super) fn routed(mut self, gsi: u32) -> Self {
        self.held = Held::Line(gsi);
        self
    }

    /// Give back an MSI vector whose message no device was handed: its capability refused it.
    pub fn give_back(mut self) {
        self.held = Held::Idle;
    }
}

impl Drop for DeviceVector {
    fn drop(&mut self) {
        match self.held {
            Held::Idle => self.pool.give_back(self.vector),
            Held::Msi => {}
            // SAFETY: ring 0, where vectors are taken; a masked line raises nothing more.
            Held::Line(gsi) => unsafe { X86IoApic::mask(gsi) },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "C" fn handler() {}

    /// **A full pool answers `PoolFull`**, where it asserted, and a vector given back is taken again.
    #[test]
    fn a_full_pool_answers_pool_full_and_a_given_back_vector_is_taken_again() {
        static POOL: VectorPool = VectorPool::new();
        let taken: std::vec::Vec<u8> =
            (0..DEVICE_IRQ_COUNT).map(|_| POOL.take(handler, Held::Msi).unwrap().commit()).collect();
        assert_eq!(taken, (DEVICE_IRQ_BASE..DEVICE_IRQ_BASE + DEVICE_IRQ_COUNT as u8).collect::<std::vec::Vec<_>>());
        assert_eq!(POOL.take(handler, Held::Idle).map(|v| v.commit()), Err(InstallError::PoolFull));
        POOL.give_back(DEVICE_IRQ_BASE + 3);
        assert_eq!(POOL.take(handler, Held::Idle).map(|v| v.commit()), Ok(DEVICE_IRQ_BASE + 3));
    }

    /// **Only a vector nothing may raise comes back**: an idle one dropped, an MSI one given back;
    /// one a device may have a message for stays taken, as does one committed.
    #[test]
    fn only_a_vector_nothing_may_raise_comes_back() {
        static POOL: VectorPool = VectorPool::new();
        let taken = |v: u8| POOL.handler((v - DEVICE_IRQ_BASE) as usize) != 0;
        let idle = POOL.take(handler, Held::Idle).unwrap();
        let v = idle.vector();
        drop(idle);
        assert!(!taken(v), "idle, given back");
        let msi = POOL.take(handler, Held::Msi).unwrap();
        let v = msi.vector();
        drop(msi);
        assert!(taken(v), "a device may write its message");
        let refused = POOL.take(handler, Held::Msi).unwrap();
        let w = refused.vector();
        refused.give_back();
        assert!(!taken(w), "its message refused, given back");
        let kept = POOL.take(handler, Held::Idle).unwrap().commit();
        assert!(taken(kept), "committed");
    }
}
