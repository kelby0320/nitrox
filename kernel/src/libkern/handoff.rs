//! [`Handoff`]: what a DPC uses but must not drop, handed between it and thread context. A
//! last-reference drop in a DPC reaches `SlabCache::free`, whose plain `SpinLock` deadlocks on its
//! own CPU (decision log, 2026-08-06), and another CPU dropping what a DPC still uses is a free
//! under its feet: both have bitten this kernel.

enum Slot<T> {
    Empty,
    Parked(T),
    Lent,
    Owed(T),
}

/// **Up to `N` values a DPC must not drop**, each parked, lent, owed, or none.
pub struct Handoff<T, const N: usize = 1> {
    slots: [Slot<T>; N],
}

/// **A value lent to a DPC**, given back by [`Handoff::give_back`]. Dropped instead, it panics a
/// debug build, and a release build leaks it rather than drop it where it was lent.
pub struct Loan<T> {
    value: Option<T>,
    slot: usize,
}

impl<T, const N: usize> Handoff<T, N> {
    /// None parked.
    pub const fn new() -> Self {
        Handoff { slots: [const { Slot::Empty }; N] }
    }

    /// **Park `value`** in a free slot, or hand it back if none is: one lent or owed is never
    /// written over, which would drop it where it lies.
    pub fn park(&mut self, value: T) -> Result<(), T> {
        match self.slots.iter_mut().find(|s| matches!(s, Slot::Empty)) {
            Some(slot) => {
                *slot = Slot::Parked(value);
                Ok(())
            }
            None => Err(value),
        }
    }

    /// **Park the value `make` makes**, made only if a slot is free. Whether it was.
    pub fn park_with(&mut self, make: impl FnOnce() -> T) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|s| matches!(s, Slot::Empty)) else { return false };
        *slot = Slot::Parked(make());
        true
    }

    /// Whether a value is parked.
    pub fn is_parked(&self) -> bool {
        self.slots.iter().any(|s| matches!(s, Slot::Parked(_)))
    }

    /// Whether every slot is empty: nothing parked, lent or owed.
    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|s| matches!(s, Slot::Empty))
    }

    /// **Lend the first parked value** to the DPC, under the keeper's lock.
    pub fn lend(&mut self) -> Option<Loan<T>> {
        let slot = self.slots.iter().position(|s| matches!(s, Slot::Parked(_)))?;
        Some(self.lend_at(slot))
    }

    /// Lend every parked value, by slot.
    pub fn lend_all(&mut self) -> [Option<Loan<T>>; N] {
        core::array::from_fn(|i| matches!(self.slots[i], Slot::Parked(_)).then(|| self.lend_at(i)))
    }

    fn lend_at(&mut self, slot: usize) -> Loan<T> {
        let Slot::Parked(value) = core::mem::replace(&mut self.slots[slot], Slot::Lent) else {
            unreachable!("only a parked slot is lent")
        };
        Loan { value: Some(value), slot }
    }

    /// **Give `loan` back**, owed to thread context. A loan from elsewhere is leaked, as dropping it
    /// here is what this type prevents.
    pub fn give_back(&mut self, mut loan: Loan<T>) {
        let value = loan.value.take();
        match self.slots.get_mut(loan.slot) {
            Some(slot @ Slot::Lent) => *slot = value.map_or(Slot::Empty, Slot::Owed),
            _ => {
                core::mem::forget(value);
                debug_assert!(false, "a loan given back where it was not lent");
            }
        }
    }

    /// **Take a parked value back**, in thread context: it was never lent.
    pub fn take_parked(&mut self) -> Option<T> {
        let slot = self.slots.iter().position(|s| matches!(s, Slot::Parked(_)))?;
        match core::mem::replace(&mut self.slots[slot], Slot::Empty) {
            Slot::Parked(value) => Some(value),
            _ => unreachable!("the slot found parked"),
        }
    }

    /// **Everything owed**, for thread context to drop outside the lock.
    pub fn take_all(&mut self) -> [Option<T>; N] {
        core::array::from_fn(|i| match core::mem::replace(&mut self.slots[i], Slot::Empty) {
            Slot::Owed(value) => Some(value),
            other => {
                self.slots[i] = other;
                None
            }
        })
    }
}

impl<T, const N: usize> Default for Handoff<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> core::ops::Deref for Loan<T> {
    type Target = T;
    fn deref(&self) -> &T {
        // unwrap: a loan holds its value until `give_back`, which consumes it.
        self.value.as_ref().unwrap()
    }
}

impl<T> Drop for Loan<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else { return };
        core::mem::forget(value);
        #[cfg(test)]
        if std::thread::panicking() {
            return;
        }
        if cfg!(debug_assertions) {
            panic!("a loan dropped where it was lent, not given back");
        }
    }
}

#[cfg(test)]
mod tests;
