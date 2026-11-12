//! [`InFlight`]: a block I/O request between its submit and its completion, the one handle a driver
//! holds to it.

use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use crate::io::irp::{Irp, PhysFrag};

/// **A block I/O request in flight**, minted by [`io::block`](super)'s dispatch alone, its fragments
/// checked there to hold its length. [`complete`](Self::complete)d once, or [`keep`](Self::keep)t by a
/// driver whose device may still move it; **one dropped unfinished fails a debug build**, as its
/// waiter would wait for good.
pub struct InFlight {
    irp: NonNull<Irp>,
    /// A host test's, which it may drop.
    #[cfg(test)]
    lax: bool,
}

// SAFETY: the IRP is this token's alone while in flight, whichever CPU holds it, and its box, which
// keeps what it points at, is reclaimed only after its completion.
unsafe impl Send for InFlight {}

impl InFlight {
    /// The IRP at `irp`, in flight; `None`, minting nothing, if its fragments do not sum to its
    /// length.
    ///
    /// # Safety
    /// `irp` is live and submitted nowhere yet, and is this token's alone until it is completed or
    /// kept; its fragments live as long.
    pub(super) unsafe fn mint(irp: NonNull<Irp>) -> Option<InFlight> {
        let token = InFlight {
            irp,
            #[cfg(test)]
            lax: false,
        };
        let sum = token.frags().iter().try_fold(0u64, |sum, f| sum.checked_add(f.len));
        if sum == Some(token.length()) {
            return Some(token);
        }
        token.keep();
        None
    }

    /// A host test's IRP, in flight.
    ///
    /// # Safety
    /// As [`mint`](Self::mint), but for the fragments' sum.
    #[cfg(test)]
    pub(crate) unsafe fn for_test(irp: *mut Irp) -> InFlight {
        InFlight { irp: NonNull::new(irp).expect("an IRP"), lax: false }
    }

    /// [`for_test`](Self::for_test), but one the test may drop.
    ///
    /// # Safety
    /// As [`for_test`](Self::for_test).
    #[cfg(test)]
    pub(crate) unsafe fn for_test_lax(irp: *mut Irp) -> InFlight {
        InFlight { irp: NonNull::new(irp).expect("an IRP"), lax: true }
    }

    fn irp(&self) -> &Irp {
        // SAFETY: the IRP is this token's while it lives, per `mint`'s contract.
        unsafe { self.irp.as_ref() }
    }

    /// Its [`IrpOp`](crate::io::irp::IrpOp).
    pub fn op(&self) -> u32 {
        self.irp().op
    }

    /// The byte offset on its device it starts at.
    pub fn offset(&self) -> u64 {
        self.irp().offset
    }

    /// The bytes it moves.
    pub fn length(&self) -> u64 {
        self.irp().length
    }

    /// Its buffer's physical fragments, which hold its length; none for a request moving no data.
    pub fn frags(&self) -> &[PhysFrag] {
        let b = self.irp().buffer;
        if b.count == 0 {
            return &[];
        }
        // SAFETY: a `[PhysFrag; count]` its box holds while it is in flight, per `mint`'s contract.
        unsafe { core::slice::from_raw_parts(b.frags as *const PhysFrag, b.count as usize) }
    }

    /// Move it to `offset` on its device: a partition's, rebased to its disk.
    pub fn rebase(&mut self, offset: u64) {
        // SAFETY: the IRP is this token's while it lives.
        unsafe { self.irp.as_mut().offset = offset };
    }

    /// **Complete it** with `status` and `transferred` bytes, its waiter answered once its DPC runs.
    pub fn complete(self, status: i32, transferred: u64) {
        let irp = ManuallyDrop::new(self).irp;
        // SAFETY: in flight, this token's alone, and completed this once, as the token is spent.
        unsafe { crate::io::irp::complete(irp.as_ptr(), status, transferred) };
    }

    /// **Keep it for good**: its device may still move its data, so its buffer is never handed back.
    pub fn keep(self) {
        let _ = ManuallyDrop::new(self);
    }

    /// Whether it is the IRP at `irp`.
    #[cfg(test)]
    pub(crate) fn is(&self, irp: *const Irp) -> bool {
        core::ptr::eq(self.irp.as_ptr(), irp)
    }

    /// The IRP, its token spent: a host test's device takes it raw.
    #[cfg(test)]
    pub(crate) fn into_raw(self) -> *mut Irp {
        ManuallyDrop::new(self).irp.as_ptr()
    }
}

#[cfg(test)]
impl PartialEq for InFlight {
    fn eq(&self, other: &Self) -> bool {
        self.irp == other.irp
    }
}

impl core::fmt::Debug for InFlight {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "InFlight({:p})", self.irp)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.lax || std::thread::panicking() {
            return;
        }
        if cfg!(debug_assertions) {
            panic!("an I/O request dropped unfinished: neither completed nor kept");
        }
    }
}

#[cfg(test)]
mod tests;
