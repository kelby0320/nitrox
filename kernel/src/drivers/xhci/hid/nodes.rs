//! The USB input nodes' slots: each one's state, epoch, reader and lights write waiting.

use super::*;

pub(super) const FREE: u8 = 0;
pub(super) const BOUND: u8 = 1;
/// A slot whose device has departed: its node reads what its ring holds and then refuses.
pub(super) const RETIRED: u8 = 2;

/// **The USB input nodes, by slot**: each slot's state, epoch, reader side, lights write waiting,
/// and which slots hold keyboards. A node's `CharBackend` context names its slot and the epoch it
/// was made in ([`context`]).
///
/// One static in a boot, [`NODES`]; **a value so a host test can drive** slot reuse, which no
/// boot reaches.
pub(super) struct Nodes {
    /// Each slot's state: [`FREE`], [`BOUND`] or [`RETIRED`]. Changed by the hub thread alone.
    pub(super) states: [AtomicU8; MAX_NODES],
    /// **Each slot's epoch**, bumped when a retired slot is given to another device, so a handle to
    /// a node retired from a slot since reused is refused `PeerClosed`.
    pub(super) epochs: [AtomicU32; MAX_NODES],
    pub(super) readers: [IrqSpinLock<Reader>; MAX_NODES],
    /// **A lights write waiting for the hub thread**, by slot: its operation and the lights.
    pub(super) lights: IrqSpinLock<[Option<(ObjectRef, u8)>; MAX_NODES]>,
    /// Which slots hold keyboards, one bit each: what takes lights, and what the hardware report
    /// drains.
    pub(super) keyboards: AtomicU32,
}

const _: () = assert!(MAX_NODES <= 32, "`Nodes::keyboards` has a bit per slot");

pub(super) static NODES: Nodes = Nodes::new();

pub(super) fn context(slot: usize, epoch: u32) -> *mut () {
    ((epoch as usize) << 8 | slot) as *mut ()
}

pub(super) fn slot_of(ctx: *mut ()) -> (usize, u32) {
    let c = ctx as usize;
    (c & 0xFF, (c >> 8) as u32)
}

/// **The slot to give a new node**: one no node has had, else one whose device has departed — the
/// first of either — or `None` when every slot is bound.
pub(super) fn choose(states: &[u8; MAX_NODES]) -> Option<usize> {
    states.iter().position(|&s| s == FREE).or_else(|| states.iter().position(|&s| s == RETIRED))
}

impl Nodes {
    pub(super) const fn new() -> Nodes {
        Nodes {
            states: [const { AtomicU8::new(FREE) }; MAX_NODES],
            epochs: [const { AtomicU32::new(0) }; MAX_NODES],
            readers: [const { IrqSpinLock::new(LockRank::Leaf, Reader::new()) }; MAX_NODES],
            lights: IrqSpinLock::new(LockRank::Leaf, [const { None }; MAX_NODES]),
            keyboards: AtomicU32::new(0),
        }
    }

    pub(super) fn current(&self, ctx: *mut ()) -> bool {
        let (index, epoch) = slot_of(ctx);
        self.epochs.get(index).is_some_and(|e| e.load(Ordering::Acquire) == epoch)
    }

    /// **Take a slot for a new node**: a free one, else a retired one, reset — its epoch first,
    /// so a stale handle's next read finds it changed under the node's lock, then its reader and
    /// any lights write still waiting on it. The slot and its epoch, or `None` when every slot is
    /// bound. From the hub thread.
    pub(super) fn take(&self, keyboard: bool) -> Option<(usize, u32)> {
        let states: [u8; MAX_NODES] = core::array::from_fn(|i| self.states[i].load(Ordering::Acquire));
        let index = choose(&states)?;
        if states[index] == RETIRED {
            self.epochs[index].fetch_add(1, Ordering::AcqRel);
            self.reclaim();
            let old = core::mem::take(&mut *self.readers[index].lock());
            // Its references dropped here, with the lock let go.
            drop(old);
            let waiting = self.lights.lock()[index].take();
            if let Some((po, _)) = waiting {
                // SAFETY: `po` is the lights write's `PendingOperation` reference, taken from its slot.
                unsafe { crate::sched::complete_pending_op(po.as_ptr(), KError::PeerClosed as i32, 0) };
                drop(po);
            }
        }
        // Said before the node can be reached: the hardware report drains what this marks.
        let bit = 1u32 << index;
        if keyboard {
            self.keyboards.fetch_or(bit, Ordering::Relaxed);
        } else {
            self.keyboards.fetch_and(!bit, Ordering::Relaxed);
        }
        self.states[index].store(BOUND, Ordering::Release);
        Some((index, self.epochs[index].load(Ordering::Acquire)))
    }

    /// **Retire slot `index`'s node**, its device gone: push `releases`, answer a waiting read with
    /// them, or refuse it if there are none, and complete a waiting lights write `PeerClosed`.
    /// Thread context, no lock held: completing takes the scheduler's lock, and a drop may free.
    pub(super) fn retire(&self, index: usize, releases: &[InputEvent], now: u64) {
        let mut scratch = [0u8; DRAIN_MAX];
        let (ready, refused) = {
            let mut r = self.readers[index].lock();
            if !releases.is_empty() {
                r.ring.push_group(releases);
            }
            r.retire(&mut scratch, now)
        };
        if let Some((read, n)) = ready {
            input::deliver(&read, &scratch[..n]);
            drop(read);
        }
        if let Some(read) = refused {
            input::refuse(&read, KError::PeerClosed);
            drop(read);
        }
        let waiting = self.lights.lock()[index].take();
        if let Some((po, _)) = waiting {
            // SAFETY: `po` is the lights write's `PendingOperation` reference, taken from its slot.
            unsafe { crate::sched::complete_pending_op(po.as_ptr(), KError::PeerClosed as i32, 0) };
            drop(po);
        }
        self.states[index].store(RETIRED, Ordering::Release);
    }

    /// **Events decoded from slot `index`'s device**, made in `epoch`: pushed to its ring, and a
    /// read waiting there taken if they answer it. From the DPC.
    ///
    /// **Dropped once the node has retired**, under the lock retiring takes: the DPC decodes under
    /// the bound table's lock and pushes after letting it go, so a report decoded before a
    /// departure could land after the releases, leaving a key down on a keyboard that has gone. And
    /// dropped when the slot has been given to another device since, whose ring this is not.
    pub(super) fn push(
        &self,
        index: usize,
        epoch: u32,
        events: &[InputEvent],
        scratch: &mut [u8],
        now: u64,
    ) -> Option<(ParkedRead, usize)> {
        let mut r = self.readers[index].lock();
        if !self.current(context(index, epoch)) || !r.offer(events) {
            return None;
        }
        r.take_ready(scratch, now)
    }

    pub(super) fn read(
        &self,
        buffer: &ObjectRef,
        po: &ObjectRef,
        buf_offset: u64,
        max_len: u64,
        ctx: *mut (),
        now: u64,
    ) -> Result<(), KError> {
        let (index, _) = slot_of(ctx);
        if index >= MAX_NODES {
            return Err(KError::InvalidArgument);
        }
        let max_len = input::read_len(max_len)?;
        let mut tmp = [0u8; DRAIN_MAX];
        let found = input::read_or_park(
            || self.reclaim(),
            || {
                let mut r = self.readers[index].lock();
                // **Under the node's lock**, against which a reused slot's reset is ordered: a node from an
                // earlier epoch reads none of the next device's ring.
                if !self.current(ctx) {
                    return ReadNow::Gone;
                }
                let found = r.read_now(&mut tmp[..max_len], now);
                if found == ReadNow::Empty {
                    r.park(ParkedRead::new(po, buffer, buf_offset, max_len));
                }
                found
            },
        );
        match found {
            Submitted::Drained(n) => input::deliver_now(buffer, po, buf_offset, &tmp[..n]),
            Submitted::Parked => {}
            Submitted::Busy => return Err(KError::WouldBlock),
            Submitted::Gone => return Err(KError::PeerClosed),
        }
        Ok(())
    }

    pub(super) fn write(
        &self,
        x: Option<&Xhci>,
        buffer: &ObjectRef,
        po: &ObjectRef,
        buf_offset: u64,
        len: u64,
        ctx: *mut (),
    ) -> Result<(), KError> {
        let (index, _) = slot_of(ctx);
        if index >= MAX_NODES {
            return Err(KError::Unsupported);
        }
        // A node from an earlier epoch is gone, whatever its slot holds now.
        if !self.current(ctx) {
            return Err(KError::PeerClosed);
        }
        if self.keyboards.load(Ordering::Relaxed) & (1 << index) == 0 {
            return Err(KError::Unsupported);
        }
        let lights = input::lights_from(buffer, buf_offset, len)?;
        let Some(x) = x else {
            return Err(KError::PeerClosed);
        };
        match x.hid.lock().entries.iter().flatten().find(|b| b.node == index).map(|b| b.lights_dead) {
            None => return Err(KError::PeerClosed),
            Some(true) => return Err(KError::IoError),
            Some(false) => {}
        }
        let superseded = self.lights.lock()[index].replace((po.clone(), lights));
        if let Some((old, _)) = superseded {
            // SAFETY: `old` is the superseded write's `PendingOperation` reference.
            unsafe { crate::sched::complete_pending_op(old.as_ptr(), 0, 1) };
            drop(old);
        }
        x.hid_lights.store(true, Ordering::Release);
        // SAFETY: `hub_wake` is the controller's `InterruptObject`, held for its life.
        unsafe { crate::sched::signal_interrupt(x.hub_wake.as_ptr()) };
        Ok(())
    }

    /// Drop every read the DPC has finished with. **Thread context only**.
    pub(super) fn reclaim(&self) {
        for (i, node) in self.readers.iter().enumerate() {
            if self.states[i].load(Ordering::Acquire) == FREE {
                continue;
            }
            let owed = node.lock().take_owed();
            drop(owed);
        }
    }

    pub(super) fn drain_keyboards(&self, now: u64) {
        let keyboards = self.keyboards.load(Ordering::Relaxed);
        let mut scratch = [0u8; DRAIN_MAX];
        for (i, node) in self.readers.iter().enumerate() {
            if keyboards & (1 << i) != 0 {
                while node.lock().ring.drain_into(&mut scratch, now) > 0 {}
            }
        }
    }
}
