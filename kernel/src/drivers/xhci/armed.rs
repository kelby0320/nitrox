//! **A wait the hub thread arms and the DPC ends.** The hub thread holds a `PendingOperation`, arms a
//! record with a [`Completer`] for it, and waits through [`Armed`]; the DPC takes the record and
//! completes the operation through the `Completer`, once it has let go of the record's lock.

use core::mem::ManuallyDrop;

use crate::arch::timer::ArchTimer;
use crate::object::PendingOperation;
use crate::object::header::Ref;

/// Where an armed [`Completer`] waits for the DPC.
pub(super) trait Record {
    /// The `Completer` taken back, for a wait that ran out; `None` once the DPC has taken it, and
    /// its completion is on its way.
    fn take_back(&self) -> Option<Completer>;
}

/// **The DPC's half of an armed wait**: spent once, by [`complete`](Self::complete). One dropped
/// unspent fails a debug build, as its waiter would wait for good.
pub(super) struct Completer {
    po: *mut (),
    /// A host test's, over no operation, which it may drop.
    #[cfg(test)]
    lax: bool,
}

// SAFETY: the operation's count is atomic and its completion takes `SCHED`; the `Armed` that minted
// the completer pins the operation, whichever CPU holds it.
unsafe impl Send for Completer {}

impl Completer {
    /// **Complete the operation** with `code` and `result`, the scheduler's lock taken.
    pub(super) fn complete(self, code: u8, result: u64) {
        let po = ManuallyDrop::new(self).po;
        // SAFETY: the `Armed` that minted it holds the operation until it sees this completion or
        // takes the completer back, which this one, spent here, can no longer be.
        unsafe { crate::sched::complete_pending_op_at(po, code as i32, result) };
    }

    fn withdraw(self) {
        let _ = ManuallyDrop::new(self);
    }

    /// A host test's completer, over no operation: never completed, only compared and dropped.
    #[cfg(test)]
    pub(super) fn for_test(at: usize) -> Completer {
        Completer { po: at as *mut (), lax: true }
    }
}

impl Drop for Completer {
    fn drop(&mut self) {
        #[cfg(test)]
        if self.lax || std::thread::panicking() {
            return;
        }
        if cfg!(debug_assertions) {
            panic!("an armed wait's completer dropped unspent");
        }
    }
}

impl core::fmt::Debug for Completer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Completer({:p})", self.po)
    }
}

#[cfg(test)]
impl PartialEq for Completer {
    fn eq(&self, other: &Self) -> bool {
        self.po == other.po
    }
}

/// **The hub thread's half**: its operation, armed in `record`, waited for by [`Armed::wait`].
/// Dropped unwaited, it takes its record back or waits out the completion on its way.
pub(super) struct Armed<'po, R: Record> {
    po: &'po Ref<PendingOperation>,
    record: R,
    settled: bool,
}

/// **Arm `po` in `record`**: `install` puts the [`Completer`] where the DPC finds it, or hands it back
/// with why it refused, as it does while another is armed.
pub(super) fn arm<'po, R: Record, E>(
    po: &'po Ref<PendingOperation>,
    record: R,
    install: impl FnOnce(Completer) -> Result<(), (Completer, E)>,
) -> Result<Armed<'po, R>, E> {
    let completer = Completer {
        po: po.as_object().as_ptr(),
        #[cfg(test)]
        lax: false,
    };
    match install(completer) {
        Ok(()) => Ok(Armed { po, record, settled: false }),
        Err((completer, why)) => {
            completer.withdraw();
            Err(why)
        }
    }
}

impl<R: Record> Armed<'_, R> {
    /// **Wait up to `bound_ns`**: the operation's code and result, or `None` if it did not complete
    /// and its record was taken back.
    pub(super) fn wait(mut self, bound_ns: u64) -> Option<(u8, u64)> {
        let now = crate::arch::Timer::read_ns();
        if !self.signalled(now + bound_ns, now) && !self.settle() {
            return None;
        }
        self.settled = true;
        let (code, result) = crate::sched::pending_op_completion(self.po);
        Some((code as u8, result))
    }

    fn signalled(&self, deadline: u64, now: u64) -> bool {
        let woke = crate::sched::wait_on(&[self.po.as_object()], deadline, now);
        matches!(woke, crate::sched::WaitResult::Signaled(_))
    }

    /// **After a wait that ran out**: the record taken back, `false`, so nothing completes an
    /// operation about to be dropped; or, the DPC having taken it, its completion waited out.
    fn settle(&mut self) -> bool {
        self.settled = true;
        if let Some(completer) = self.record.take_back() {
            completer.withdraw();
            return false;
        }
        while !self.signalled(u64::MAX, 0) {
            super::hub::sleep(1_000_000);
        }
        true
    }
}

impl<R: Record> Drop for Armed<'_, R> {
    fn drop(&mut self) {
        if !self.settled {
            self.settle();
        }
    }
}

#[cfg(test)]
mod tests;
