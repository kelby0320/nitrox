//! [`Mmio`]: a window of device registers, each access an aligned dword checked to lie inside it.

use core::marker::PhantomData;

use crate::mm::VirtAddr;

/// **A window of device registers**: `len` bytes of uncached MMIO, mapped for `'a`. Each access is
/// an aligned dword inside it, and a window inside it, [`sub`](Self::sub), is checked as it is
/// made: an offset a device supplies is held to its window once, at bring-up.
#[derive(Copy, Clone, Debug)]
pub struct Mmio<'a> {
    base: u64,
    len: u64,
    mapped: PhantomData<&'a ()>,
}

impl<'a> Mmio<'a> {
    /// The registers at `base`.
    ///
    /// # Safety
    /// `[base, base + len)` is device registers mapped uncached, which nothing unmaps for `'a`,
    /// and any aligned dword inside it is the device's to answer.
    pub unsafe fn new(base: VirtAddr, len: u64) -> Self {
        Mmio { base: base.as_u64(), len, mapped: PhantomData }
    }

    /// A host test's registers: memory standing in for a device's.
    #[cfg(test)]
    pub fn over(regs: &'a mut [u32]) -> Self {
        Mmio { base: regs.as_mut_ptr() as u64, len: core::mem::size_of_val(regs) as u64, mapped: PhantomData }
    }

    /// Its length in bytes.
    #[allow(clippy::len_without_is_empty, reason = "a window of registers")]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// The register at `off`. Panics outside the window, or unaligned.
    pub fn read32(&self, off: u64) -> u32 {
        // SAFETY: `at` holds an aligned dword inside the window, which `new`'s contract maps.
        unsafe { core::ptr::read_volatile(self.at(off)) }
    }

    /// Write the register at `off`. Panics as [`read32`](Self::read32) does.
    pub fn write32(&self, off: u64, val: u32) {
        // SAFETY: as `read32`.
        unsafe { core::ptr::write_volatile(self.at(off), val) }
    }

    /// The `len` bytes from `off`, a window of their own; `None` past this one's end, or unaligned.
    pub fn sub(&self, off: u64, len: u64) -> Option<Mmio<'a>> {
        let inside = off.checked_add(len).is_some_and(|end| end <= self.len);
        (inside && off.is_multiple_of(4)).then(|| Mmio { base: self.base + off, len, mapped: PhantomData })
    }

    fn at(&self, off: u64) -> *mut u32 {
        let inside = off.checked_add(4).is_some_and(|end| end <= self.len);
        assert!(inside && off.is_multiple_of(4), "a register at {off:#x} of a {:#x}-byte window", self.len);
        (self.base + off) as *mut u32
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    /// **A window reaches its last dword and stops one past it**, and one inside it is held to its
    /// own end: a window at the end allowed, a byte past refused.
    #[test]
    fn a_window_reaches_its_last_register_and_stops_one_past() {
        let mut regs = [0u32; 16];
        let bar = Mmio::over(&mut regs);
        bar.write32(60, 7);
        assert_eq!(bar.read32(60), 7);
        assert!(catch_unwind(AssertUnwindSafe(|| bar.read32(64))).is_err(), "one past");
        assert!(catch_unwind(AssertUnwindSafe(|| bar.write32(62, 1))).is_err(), "unaligned");
        let tail = bar.sub(48, 16).expect("the window at the end");
        assert_eq!(tail.read32(12), 7, "its last register the window's last");
        assert!(catch_unwind(AssertUnwindSafe(|| tail.read32(16))).is_err(), "its own end");
        assert!(bar.sub(48, 17).is_none(), "a byte past");
        assert!(bar.sub(2, 4).is_none(), "unaligned");
        assert!(bar.sub(u64::MAX - 3, 8).is_none(), "an end that overflows");
        assert_eq!(bar.sub(64, 0).map(|w| w.len()), Some(0), "empty, at the end");
    }
}
