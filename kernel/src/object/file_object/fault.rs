//! The fault path: a page faulted in, waiting on its fill.

use super::*;

impl FileObject {
    /// **Page `index`'s frame if it is resident**, for a fault that may not wait for a fill.
    pub fn resident_page(&self, index: usize) -> Option<PhysAddr> {
        match self.inner.lock().pages.iter().find(|p| p.index == index) {
            Some(p) if p.state == PageState::Ready => Some(p.frame.phys()),
            _ => None,
        }
    }

    /// **Fault page `index` in**, blocking until it is resident: a hit returns at once; a miss
    /// reserves a frame, starts the producer fill, and **parks the calling thread** on the fill's
    /// `PendingOperation`. `None` on a frame/PO allocation failure or a failed fill. Called from
    /// the page-fault handler **after** the address-space lock is released, so it holds no
    /// AS/cache lock. `file_obj` is the caller's reference to *this* object, which the deferred
    /// fill keeps alive.
    ///
    /// **A second faulter of a page being filled waits on that fill's PO**, not a `yield_now`
    /// spin: the handler runs with interrupts off and `yield_now` returns at once when nothing
    /// else is ready, so the CPU never acknowledged a TLB shootdown and the machine stopped.
    /// Whoever wakes first settles the page. A failed fill leaves its pages out of the cache for
    /// the next fault to retry; **one that read ahead, or that a faulter waited on, is first
    /// retried for the faulted page alone**, as the error may have been on a page it never asked
    /// for. A faulter that cannot register on a PO (`MAX_WAITERS`) parks a millisecond and looks
    /// again.
    pub fn fault_in_page(file_obj: &Ref<FileObject>, index: usize) -> Option<PhysAddr> {
        Self::fault_in_page_with(file_obj, index, &mut wait_for_fill, &mut park_briefly)
    }

    /// [`fault_in_page`](Self::fault_in_page) with its waits given, so host tests play the device.
    pub(super) fn fault_in_page_with(
        file_obj: &Ref<FileObject>,
        index: usize,
        wait: &mut dyn FnMut(&Ref<PendingOperation>) -> bool,
        park: &mut dyn FnMut(),
    ) -> Option<PhysAddr> {
        let fo: &FileObject = file_obj;
        // Whether this fault's fill may read ahead, and whether a failed fill may be tried again:
        // neither after a failure, so a fault fails at the second.
        let mut read_ahead = true;
        loop {
            match fo.reserve_fault(index) {
                Fault::Hit(frame) => {
                    fill_stats::HITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    return Some(frame);
                }
                Fault::Fill(frame, po) => {
                    let started = crate::arch::Timer::read_ns();
                    let started_kind = fo.start_fill(file_obj, index, frame, &po, read_ahead);
                    if started_kind == FillStart::Failed {
                        fo.settle(index, &po, false);
                        crate::sched::complete_pending_op(&po, crate::syscall::error::KError::OutOfMemory as i32, 0);
                        return None;
                    }
                    let ok = wait(&po);
                    let pages = match started_kind {
                        FillStart::DeviceIo(pages) => {
                            fo.finish_io();
                            pages
                        }
                        _ => 1,
                    };
                    for k in 0..pages {
                        fo.settle(index + k, &po, ok);
                    }
                    if !ok && pages > 1 {
                        read_ahead = false;
                        continue;
                    }
                    if !ok {
                        return None;
                    }
                    // A hole does no device I/O; counting it as a fill would understate their cost.
                    if started_kind == FillStart::Hole {
                        fill_stats::HOLE_FILLS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    } else {
                        fill_stats::record_fill(crate::arch::Timer::read_ns().saturating_sub(started), pages as u64);
                    }
                }
                Fault::Wait(po) => {
                    fill_stats::SPINS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let ok = wait(&po);
                    fo.settle(index, &po, ok);
                    if !ok && read_ahead {
                        read_ahead = false;
                        continue;
                    }
                    if !ok {
                        return None;
                    }
                }
                Fault::Busy => park(),
                Fault::Oom => return None,
            }
        }
    }
}
