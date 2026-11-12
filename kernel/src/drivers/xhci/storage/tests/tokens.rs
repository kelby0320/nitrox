//! How a test hands the driver its requests, and spends what the driver hands back.

use super::*;

/// `irp`, in flight: dropped by the driver, it fails the test.
pub(super) fn fly(irp: *mut Irp) -> InFlight {
    // SAFETY: the test's IRP, which outlives the disks it is submitted to.
    unsafe { InFlight::for_test(irp) }
}

/// A token for `irp` the test may drop: what it compares a result with.
pub(super) fn was(irp: *mut Irp) -> InFlight {
    // SAFETY: as `fly`'s.
    unsafe { InFlight::for_test_lax(irp) }
}

/// **A result the test acts on**: each token in it spent, a twin the test may drop in its place.
pub(super) trait Lax {
    fn lax(self) -> Self;
}

impl Lax for InFlight {
    fn lax(self) -> Self {
        was(self.into_raw())
    }
}

impl Lax for Programmed {
    fn lax(self) -> Self {
        self
    }
}

impl<T: Lax> Lax for Option<T> {
    fn lax(self) -> Self {
        self.map(T::lax)
    }
}

impl<T: Lax, const N: usize> Lax for [T; N] {
    fn lax(self) -> Self {
        self.map(T::lax)
    }
}

impl<A: Lax, B: Lax, C: Lax> Lax for (A, B, C) {
    fn lax(self) -> Self {
        (self.0.lax(), self.1.lax(), self.2.lax())
    }
}

impl Lax for Action {
    fn lax(self) -> Self {
        match self {
            Action::Complete { irp, status, transferred } => Action::Complete { irp: irp.lax(), status, transferred },
            Action::Ejected { irp, said } => Action::Ejected { irp: irp.lax(), said },
            Action::Stopped { irp } => Action::Stopped { irp: irp.lax() },
            a => a,
        }
    }
}

impl Lax for BeginRescan {
    fn lax(self) -> Self {
        match self {
            BeginRescan::Ready { irp, lun, unit } => BeginRescan::Ready { irp: irp.lax(), lun, unit },
            b => b,
        }
    }
}

/// **A test's disks keep what they hold when it ends**, as a kept slot does: so only a token the
/// driver let go of unfinished fails it.
impl Drop for Disks {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        for s in &self.devs {
            let Some(mut dev) = s.lock().take() else { continue };
            if let Some(Command { owner: Owner::Irp { cmd, .. }, .. }) = dev.inflight.take() {
                cmd.irp.keep();
            }
            while let Some(c) = dev.queue.pop() {
                c.irp.keep();
            }
            if let Some(irp) = dev.stranded.take() {
                irp.keep();
            }
            if let Some((irp, _)) = dev.rescan.take() {
                irp.keep();
            }
        }
    }
}
