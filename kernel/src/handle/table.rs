//! The segmented kernel [`HandleTable`] — allocate, lookup, close,
//! restrict, duplicate, stat, quiesce.
//!
//! See `docs/spec/handle-encoding.md` for the normative wire format,
//! `docs/architecture/handle-system.md` for the implementation
//! overview, and the [parent module documentation](super) for the
//! two-layer concurrency model.

use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::libkern::handle::{KObjectType, RawHandle, Rights};
use crate::libkern::{AllocError, KVec, SpinLock};

use super::entry::{WriteGuard, read_snapshot};
use super::grace::GraceTracker;
use super::prng::Xorshift64;
use super::segment::{SegmentEntries, SegmentMeta, free_entries, try_alloc_initialised};
use super::type_rights::is_rights_compatible;
use super::{
    DIRECTORY_LEN, SEGMENT_LEN, current_ctx_id, release_refcount, try_acquire_refcount,
};

/// Number of deferred-close entries the per-table ring can hold
/// between drain calls. Each entry is 16 bytes (handle + epoch) so
/// the ring is `256 * (16 + Option discriminant) ≈ 6 KiB`. Sized to
/// absorb a burst of closes between `allocate`/`close` drain
/// opportunities; if it ever fills, `close` releases the rank-3 lock,
/// yields, and retries.
pub const DEFER_RING_CAPACITY: usize = 256;

/// Backoff used by `close` when the defer ring is full and `drain`
/// could not free a slot. In tests this yields to the host scheduler
/// so a reader stuck spinning on `read_snapshot` can complete and
/// quiesce; in production builds (`no_std`) it emits a `PAUSE`-style
/// hint and lets the caller spin. Production Phase 1 is single-CPU
/// and never actually reaches this path — the closing thread is the
/// only possible reader and is already quiesced by call time.
#[cfg(test)]
fn yield_for_grace() {
    std::thread::yield_now();
}

#[cfg(not(test))]
fn yield_for_grace() {
    core::hint::spin_loop();
}

/// Why a handle table operation failed.
///
/// The handle table favours explicit variants over coercing several
/// distinct failure modes to one. Syscall layers may collapse
/// `NotOwner` into `InvalidHandle` to avoid leaking owner-existence
/// information to the caller, but the table itself reports the more
/// precise reason for telemetry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// The supplied handle was [`RawHandle::NULL`].
    NullHandle,
    /// Segment id, slot id, generation, or `object`-non-null check
    /// failed. The handle does not refer to a live kernel object in
    /// this table.
    InvalidHandle,
    /// The caller's PID does not match the entry's `owner_pid`.
    NotOwner,
    /// The caller asked for rights the handle does not carry.
    NoAccess,
    /// The directory is full and no further segments can be allocated.
    OutOfHandles,
    /// A segment allocation failed because the kernel heap is
    /// exhausted.
    OutOfMemory,
    /// `allocate` was called with rights not valid for the supplied
    /// `KObjectType`, per the spec's type-rights compatibility matrix.
    BadRights,
}

impl From<AllocError> for HandleError {
    fn from(_: AllocError) -> Self {
        HandleError::OutOfMemory
    }
}

/// What a successful [`HandleTable::lookup`] returns.
///
/// `object` is type-erased and refcounted by the next slice's
/// `ObjectRef`. In Phase 1 the [`try_acquire_refcount`] stub returns
/// `true` unconditionally so the caller receives the raw pointer
/// with no refcount obligation.
#[derive(Debug)]
pub struct LookupOk {
    pub object: *mut (),
    pub object_type: KObjectType,
    pub rights: Rights,
}

// SAFETY: At the handle-table layer `object` is opaque — it is not
// dereferenced, and its thread-safety properties belong to whatever
// kernel object the pointer refers to, not to `LookupOk`. Callers
// that hand a `LookupOk` across thread boundaries are accepting that
// responsibility on behalf of the pointee.
unsafe impl Send for LookupOk {}
// SAFETY: as `Send`.
unsafe impl Sync for LookupOk {}

/// Snapshot of handle metadata returned by [`HandleTable::stat`].
#[derive(Copy, Clone, Debug)]
pub struct HandleStat {
    pub object_type: KObjectType,
    pub rights: Rights,
    pub owner_pid: u32,
    pub generation: u32,
}

/// The object pointer returned by [`HandleTable::close`] so the
/// caller can release whatever the next slice's `KObjectHeader`
/// associates with the handle. The wrapper exists to make the
/// `Result<ClosedObject, HandleError>` Send-able for callers (and
/// stress tests) that want to spawn closures over the handle table —
/// the bare `*mut ()` is `!Send`, which would otherwise infect any
/// closure containing a `close` call.
#[derive(Copy, Clone, Debug)]
pub struct ClosedObject(pub *mut ());

// SAFETY: as `LookupOk` — the pointer is opaque at the handle-table
// layer; thread-safety of the pointee is the caller's concern.
unsafe impl Send for ClosedObject {}
// SAFETY: as `Send`.
unsafe impl Sync for ClosedObject {}

/// The segmented handle table.
///
/// `directory` is a fixed-size inline array of `AtomicPtr` slots; each
/// non-null slot points at a [`SegmentEntries`] (256 KiB) block on the
/// heap. Lookups follow `directory[seg_id]` with a single `Acquire`
/// load, no lock taken. The rank-3 [`SpinLock`] in `inner` serialises
/// allocate / close / restrict / segment-grow bookkeeping.
pub struct HandleTable {
    directory: [AtomicPtr<SegmentEntries>; DIRECTORY_LEN],
    /// Cheap allocator hint — which segment to start the free-list
    /// scan at. Always brought back in range by the `allocate` loop.
    next_segment_hint: AtomicU32,
    grace: GraceTracker,
    inner: SpinLock<Inner>,
}

struct Inner {
    /// Per-segment free-list metadata. Index matches `directory`. Only
    /// the first `segments_count` entries are meaningful.
    segment_meta: [SegmentMeta; DIRECTORY_LEN],
    /// Number of segments brought online so far. Always
    /// `<= DIRECTORY_LEN`.
    segments_count: u32,
    defer_ring: DeferredQueue,
    prng: Xorshift64,
}

#[derive(Copy, Clone)]
struct DeferredClose {
    handle: RawHandle,
    epoch: u64,
}

/// Fixed-capacity ring buffer of pending deferred closes, allocated
/// once at table construction.
struct DeferredQueue {
    buf: KVec<Option<DeferredClose>>,
    head: usize,
    len: usize,
}

impl DeferredQueue {
    fn try_new(capacity: usize) -> Result<Self, AllocError> {
        let mut buf = KVec::new();
        buf.try_reserve(capacity)?;
        for _ in 0..capacity {
            buf.try_push(None)?;
        }
        Ok(Self {
            buf,
            head: 0,
            len: 0,
        })
    }

    fn capacity(&self) -> usize {
        self.buf.len()
    }

    fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, d: DeferredClose) -> Result<(), ()> {
        if self.is_full() {
            return Err(());
        }
        let cap = self.capacity();
        let pos = (self.head + self.len) % cap;
        self.buf[pos] = Some(d);
        self.len += 1;
        Ok(())
    }

    fn front(&self) -> Option<DeferredClose> {
        if self.len == 0 {
            return None;
        }
        self.buf[self.head]
    }

    fn pop_front(&mut self) -> Option<DeferredClose> {
        if self.len == 0 {
            return None;
        }
        let item = self.buf[self.head].take();
        self.head = (self.head + 1) % self.capacity();
        self.len -= 1;
        item
    }
}

impl HandleTable {
    /// Construct an empty table, seeded with `seed` for the segment
    /// freelist shuffles, and eagerly grow segment 0.
    ///
    /// Eager grow keeps the first allocation off the slow grow path,
    /// which is helpful for tests (predictable first-allocation
    /// latency) and for early-boot consumers (no surprise allocation
    /// when bringing init online).
    pub fn try_new(seed: u64) -> Result<Self, HandleError> {
        let defer_ring = DeferredQueue::try_new(DEFER_RING_CAPACITY)?;
        let table = Self {
            directory: [const { AtomicPtr::new(ptr::null_mut()) }; DIRECTORY_LEN],
            next_segment_hint: AtomicU32::new(0),
            grace: GraceTracker::new(),
            inner: SpinLock::new(Inner {
                segment_meta: [SegmentMeta::empty(); DIRECTORY_LEN],
                segments_count: 0,
                defer_ring,
                prng: Xorshift64::new(seed),
            }),
        };
        table.grow_one()?;
        Ok(table)
    }

    /// Allocate one more segment, publishing it into the directory.
    /// Releases the rank-3 lock during the heap allocation per
    /// `kernel/CLAUDE.md` § "Forbidden patterns" (no allocations while
    /// holding a spinlock). On the rare race where another caller
    /// publishes the same slot first, our spare segment is freed and
    /// the function returns Ok — the caller's outer retry loop will
    /// observe the racer's segment.
    fn grow_one(&self) -> Result<(), HandleError> {
        // (1) Under the lock: pick the target slot and snapshot a
        // shuffle seed from the table PRNG.
        let (next_seg, seed) = {
            let mut guard = self.inner.lock();
            if guard.segments_count as usize >= DIRECTORY_LEN {
                return Err(HandleError::OutOfHandles);
            }
            let next = guard.segments_count;
            let seed = guard.prng.next_u64();
            (next, seed)
        };

        // (2) Without holding the lock: allocate the segment (256 KiB
        // + a 16 KiB scratch shuffle buffer routes through rank-6
        // allocators).
        let (entries, meta) = try_alloc_initialised(seed)?;

        // (3) Reacquire the lock and publish — or, on race, discard.
        let mut guard = self.inner.lock();
        // A concurrent grower may have published the same slot
        // (directory entry non-null) or grown past us
        // (segments_count moved beyond our `next_seg`).
        if guard.segments_count > next_seg
            || !self.directory[next_seg as usize]
                .load(Ordering::Relaxed)
                .is_null()
        {
            drop(guard);
            // SAFETY: `entries` came from `try_alloc_initialised`
            // above; nothing else has a reference because the pointer
            // never entered the directory.
            unsafe {
                free_entries(entries);
            }
            return Ok(());
        }
        guard.segment_meta[next_seg as usize] = meta;
        guard.segments_count = next_seg + 1;
        // Publish with Release so a lookup that observes the pointer
        // also observes the fully-initialised entries.
        self.directory[next_seg as usize].store(entries.as_ptr(), Ordering::Release);
        Ok(())
    }

    /// Allocate a fresh handle pointing at `object`.
    ///
    /// `owner_pid` becomes the security-critical owner of the handle.
    /// `object_type` and `rights` must satisfy the type-rights
    /// compatibility matrix or this returns [`HandleError::BadRights`].
    ///
    /// `object` is taken as type-erased; in the next slice it will be
    /// a `KBox<KObjectHeader>::into_raw()` pointer with a refcount
    /// bumped before this call.
    pub fn allocate(
        &self,
        owner_pid: u32,
        object: *mut (),
        object_type: KObjectType,
        rights: Rights,
    ) -> Result<RawHandle, HandleError> {
        debug_assert!(!object.is_null(), "callers must not store null objects");
        if !is_rights_compatible(object_type, rights) {
            return Err(HandleError::BadRights);
        }

        loop {
            // Try the fast path under the rank-3 lock.
            {
                let mut guard = self.inner.lock();
                self.drain_expired(&mut guard);

                let segments_count = guard.segments_count as usize;
                // Walk segments starting from the hint (wraps via two
                // chained ranges so a hint past the end retries from 0).
                let hint = (self.next_segment_hint.load(Ordering::Relaxed) as usize)
                    .min(segments_count.saturating_sub(1));
                let scan = (hint..segments_count).chain(0..hint);
                let mut chosen: Option<(u32, u32, u32)> = None; // (seg_id, slot_id, new_gen)
                for seg_id in scan {
                    if guard.segment_meta[seg_id].free_count > 0 {
                        let slot_id = guard.segment_meta[seg_id].free_head;
                        let entries_ptr = self.directory[seg_id].load(Ordering::Relaxed);
                        debug_assert!(
                            !entries_ptr.is_null(),
                            "segment_meta marks segment {seg_id} non-empty but directory is null",
                        );
                        // SAFETY: directory entry is non-null (debug
                        // assertion); segments published once outlive the
                        // table.
                        let entry =
                            unsafe { &(*entries_ptr)[slot_id as usize] };
                        let next_head = entry.free_next.load(Ordering::Relaxed);
                        guard.segment_meta[seg_id].free_head = next_head;
                        guard.segment_meta[seg_id].free_count -= 1;

                        let new_gen = entry
                            .generation
                            .load(Ordering::Relaxed)
                            .wrapping_add(1);
                        {
                            let _wg = WriteGuard::new(entry);
                            entry.generation.store(new_gen, Ordering::Relaxed);
                            entry.owner_pid.store(owner_pid, Ordering::Relaxed);
                            entry.rights.store(rights.bits(), Ordering::Relaxed);
                            entry
                                .object_type
                                .store(object_type as u32, Ordering::Relaxed);
                            entry.next_owned.store(RawHandle::NULL.bits(), Ordering::Relaxed);
                            // Publish the object pointer LAST: a reader
                            // that sees `object != null` is also
                            // guaranteed (by the Release here pairing
                            // with the Acquire load in `lookup`) to see
                            // every metadata store above it.
                            entry.object.store(object, Ordering::Release);
                        }
                        chosen = Some((seg_id as u32, slot_id, new_gen));
                        break;
                    }
                }
                if let Some((seg_id, slot_id, new_gen)) = chosen {
                    self.next_segment_hint.store(seg_id, Ordering::Relaxed);
                    return Ok(RawHandle::encode(seg_id, slot_id, new_gen));
                }
            }
            // No free slot anywhere — grow another segment and retry.
            self.grow_one()?;
        }
    }

    /// Look up a handle and validate it per spec § "Validation
    /// algorithm".
    ///
    /// `required` is the rights subset the caller needs to perform
    /// its operation; pass [`Rights::empty()`] to merely confirm the
    /// handle is live.
    pub fn lookup(
        &self,
        h: RawHandle,
        caller_pid: u32,
        required: Rights,
    ) -> Result<LookupOk, HandleError> {
        // Step 0: enter a read-side critical section. The guard is
        // dropped on every exit path; while held, the table will not
        // recycle any slot we might still hold a reference into.
        let _read_guard = self.grace.enter_read(current_ctx_id());

        // Step 1: decode.
        if h.is_null() {
            return Err(HandleError::NullHandle);
        }
        let (seg_id, slot_id, gen_expected) = h.decode();

        // Step 2: seg_id bound.
        if seg_id as usize >= DIRECTORY_LEN {
            return Err(HandleError::InvalidHandle);
        }
        // Step 3: segment exists. Acquire pairs with `grow_one`'s
        // Release store of the directory entry.
        let entries_ptr = self.directory[seg_id as usize].load(Ordering::Acquire);
        if entries_ptr.is_null() {
            return Err(HandleError::InvalidHandle);
        }
        // Step 4: slot_id bound.
        if slot_id as usize >= SEGMENT_LEN {
            return Err(HandleError::InvalidHandle);
        }
        // SAFETY: a non-null directory entry was published once and
        // outlives the table. The borrow is tied to `&self`.
        let entry = unsafe { &(*entries_ptr)[slot_id as usize] };

        // Bounded retry loop. The seqlock plus the rank-3 lock cap
        // the number of writer windows we can race with at one per
        // contending writer; `1024` is a wide tripwire for a logic bug
        // in debug builds.
        let mut retries = 0u32;
        loop {
            // Step 5: seqlock-protected metadata snapshot.
            let snap = read_snapshot(entry);

            // Step 6: object non-null.
            let obj = entry.object.load(Ordering::Acquire);
            if obj.is_null() {
                return Err(HandleError::InvalidHandle);
            }

            // Decode object_type defensively — a writer corruption
            // would manifest as an unknown discriminant.
            let object_type = match KObjectType::from_u32(snap.object_type) {
                Some(t) => t,
                None => return Err(HandleError::InvalidHandle),
            };

            // Step 7: try to bump the object refcount. Stubbed this
            // slice; rewired to ObjectRef::try_acquire in the next.
            if !try_acquire_refcount(obj, object_type) {
                return Err(HandleError::InvalidHandle);
            }

            // Step 8: re-read seq; if changed or odd, release and retry.
            let s2 = entry.seq.load(Ordering::Acquire);
            if s2 != snap.seq || (s2 & 1) != 0 {
                release_refcount(obj, object_type);
                retries += 1;
                debug_assert!(
                    retries < 1024,
                    "handle table lookup spinning past 1024 retries — logic bug",
                );
                continue;
            }

            // Step 9: generation match.
            if snap.generation != gen_expected {
                release_refcount(obj, object_type);
                return Err(HandleError::InvalidHandle);
            }

            // Step 10: owner_pid match — security-critical.
            if snap.owner_pid != caller_pid {
                release_refcount(obj, object_type);
                return Err(HandleError::NotOwner);
            }

            // Step 11: rights subset.
            if !required.is_subset_of(snap.rights) {
                release_refcount(obj, object_type);
                return Err(HandleError::NoAccess);
            }

            // Step 12: return.
            return Ok(LookupOk {
                object: obj,
                object_type,
                rights: snap.rights,
            });
        }
    }

    /// Close a handle. Returns the object pointer the caller stored
    /// at allocate so the (next slice's) refcount can be decremented.
    pub fn close(&self, h: RawHandle, caller_pid: u32) -> Result<ClosedObject, HandleError> {
        if h.is_null() {
            return Err(HandleError::NullHandle);
        }
        let (seg_id, slot_id, gen_expected) = h.decode();
        if seg_id as usize >= DIRECTORY_LEN || slot_id as usize >= SEGMENT_LEN {
            return Err(HandleError::InvalidHandle);
        }

        let mut guard = self.inner.lock();
        let entries_ptr = self.directory[seg_id as usize].load(Ordering::Relaxed);
        if entries_ptr.is_null() {
            return Err(HandleError::InvalidHandle);
        }
        // SAFETY: as in lookup; segments are published once and outlive
        // the table.
        let entry = unsafe { &(*entries_ptr)[slot_id as usize] };

        // Validate under the lock. Plain loads suffice because no
        // other writer can race us — they would need the same lock.
        let current_gen = entry.generation.load(Ordering::Relaxed);
        if current_gen != gen_expected {
            return Err(HandleError::InvalidHandle);
        }
        let current_owner = entry.owner_pid.load(Ordering::Relaxed);
        if current_owner != caller_pid {
            return Err(HandleError::NotOwner);
        }
        let prev_obj = entry.object.load(Ordering::Acquire);
        if prev_obj.is_null() {
            return Err(HandleError::InvalidHandle);
        }

        // Null the object under the seqlock; generation is NOT bumped
        // here (per spec § "Generation counter behavior").
        {
            let _wg = WriteGuard::new(entry);
            entry.object.store(ptr::null_mut(), Ordering::Release);
        }

        // Schedule deferred reclamation. Snapshot the epoch *before*
        // draining (drain advances the global epoch) so the deferral
        // waits only for readers that were already in flight at close
        // time, not readers that start after the drain bump.
        let epoch = self.grace.current_epoch();
        let deferred = DeferredClose { handle: h, epoch };
        // Backpressure loop: try to drain and push; if the ring is
        // still full, release the rank-3 lock (so any spinning
        // reader on a different CPU/host-thread can make progress
        // through its lookup and quiesce), yield, and retry.
        //
        // Phase 1 single-CPU: the closing thread is the only
        // possible reader, and any prior lookup it ran is already
        // quiesced by the time `close` is called. Drain succeeds on
        // the first iteration and the loop exits immediately.
        //
        // SMP and hosted multi-thread tests: under heavy write
        // pressure a reader stuck in `read_snapshot`'s seqlock loop
        // may not have quiesced yet. Yielding lets it complete.
        loop {
            self.drain_expired(&mut guard);
            if guard.defer_ring.push(deferred).is_ok() {
                break;
            }
            // Drop the lock so readers can finish their lookups.
            // Reacquire after a yield/spin hint.
            drop(guard);
            yield_for_grace();
            guard = self.inner.lock();
        }

        Ok(ClosedObject(prev_obj))
    }

    /// Attenuate a handle's rights in place. New rights are
    /// `existing & new_rights`; the spec forbids amplification, so
    /// extra bits in `new_rights` that the handle does not currently
    /// hold are silently dropped.
    pub fn restrict(
        &self,
        h: RawHandle,
        caller_pid: u32,
        new_rights: Rights,
    ) -> Result<(), HandleError> {
        if h.is_null() {
            return Err(HandleError::NullHandle);
        }
        let (seg_id, slot_id, gen_expected) = h.decode();
        if seg_id as usize >= DIRECTORY_LEN || slot_id as usize >= SEGMENT_LEN {
            return Err(HandleError::InvalidHandle);
        }

        let _guard = self.inner.lock();
        let entries_ptr = self.directory[seg_id as usize].load(Ordering::Relaxed);
        if entries_ptr.is_null() {
            return Err(HandleError::InvalidHandle);
        }
        // SAFETY: as above.
        let entry = unsafe { &(*entries_ptr)[slot_id as usize] };

        if entry.generation.load(Ordering::Relaxed) != gen_expected {
            return Err(HandleError::InvalidHandle);
        }
        if entry.owner_pid.load(Ordering::Relaxed) != caller_pid {
            return Err(HandleError::NotOwner);
        }
        if entry.object.load(Ordering::Acquire).is_null() {
            return Err(HandleError::InvalidHandle);
        }

        let current = Rights::from_bits_truncate(entry.rights.load(Ordering::Relaxed));
        let new = current & new_rights;
        {
            let _wg = WriteGuard::new(entry);
            entry.rights.store(new.bits(), Ordering::Relaxed);
        }
        Ok(())
    }

    /// Duplicate a handle. Returns a new handle to the same object
    /// with rights `existing & new_rights`. Requires
    /// [`Rights::DUPLICATE`] on the source handle.
    ///
    /// **TOCTOU note.** With Phase 1's no-op refcount stubs there is
    /// a race window between `lookup` returning and `allocate`
    /// running: a concurrent `close` of the source handle can drop
    /// the object's last reference, and the duplicate would install
    /// a dangling pointer. Closed by the next slice — once `lookup`
    /// returns an `ObjectRef` that holds a refcount across the gap,
    /// the duplicate's `allocate` bumps the count again before the
    /// `ObjectRef` is dropped. See `docs/architecture/handle-system.md`
    /// § "Phase 1 limitations" and the matching checklist entry
    /// under "Kernel object infrastructure" in
    /// `docs/planning/implementation-plan.md`.
    pub fn duplicate(
        &self,
        h: RawHandle,
        caller_pid: u32,
        new_rights: Rights,
    ) -> Result<RawHandle, HandleError> {
        let info = self.lookup(h, caller_pid, Rights::DUPLICATE)?;
        let dup_rights = info.rights & new_rights;
        // The spec's subset semantics let the caller drop DUPLICATE
        // from the new handle by omitting it in `new_rights`; we do
        // not force it.
        self.allocate(caller_pid, info.object, info.object_type, dup_rights)
    }

    /// Snapshot a handle's metadata for `sys_handle_stat`. Requires
    /// [`Rights::INSPECT`] on the handle.
    ///
    /// All four fields come from `lookup`'s single seqlock-bracketed
    /// snapshot:
    ///
    /// - `object_type` and `rights` are returned directly from
    ///   `LookupOk`.
    /// - `owner_pid` is the caller's pid (lookup step 10 verified
    ///   `snap.owner_pid == caller_pid` before returning Ok).
    /// - `generation` is the handle's encoded generation (lookup
    ///   step 9 verified `snap.generation == gen_expected`).
    ///
    /// Doing a second `read_snapshot` here would race: between
    /// `lookup`'s ReadGuard drop and re-entry, a concurrent
    /// close-plus-realloc on the same slot (legal between two
    /// threads of the same owning process) could install a new
    /// generation and owner. Reporting those would mix metadata
    /// from two distinct slot lifetimes.
    pub fn stat(&self, h: RawHandle, caller_pid: u32) -> Result<HandleStat, HandleError> {
        let info = self.lookup(h, caller_pid, Rights::INSPECT)?;
        let (_, _, generation) = h.decode();
        Ok(HandleStat {
            object_type: info.object_type,
            rights: info.rights,
            owner_pid: caller_pid,
            generation,
        })
    }

    /// Mark the calling context quiescent. Called by syscall exit
    /// paths that did not themselves take a read guard but should
    /// still let grace periods advance.
    pub fn quiesce(&self, ctx_id: u32) {
        self.grace.mark_quiescent(ctx_id);
    }

    /// Number of segments currently brought online.
    pub fn segments_allocated(&self) -> usize {
        self.inner.lock().segments_count as usize
    }

    /// Approximate count of live handles. Walks every segment's
    /// metadata under the rank-3 lock; intended for tests and
    /// debugging, not for hot-path use.
    pub fn allocated_count(&self) -> usize {
        let guard = self.inner.lock();
        let mut sum = 0usize;
        for seg_id in 0..guard.segments_count as usize {
            sum += SEGMENT_LEN - guard.segment_meta[seg_id].free_count as usize;
        }
        // Subtract pending deferrals — those slots are not yet on the
        // freelist but also do not point at a live object.
        sum -= guard.defer_ring.len();
        sum
    }

    /// Pop every deferred close whose grace period has fully elapsed
    /// and return its slot to the segment's freelist. Then bump the
    /// global epoch so subsequent closes are tagged with a fresh
    /// epoch their own context cannot have observed.
    ///
    /// Called from `allocate` and `close` while the rank-3 lock is
    /// held.
    fn drain_expired(&self, inner: &mut Inner) {
        while let Some(d) = inner.defer_ring.front() {
            if !self.grace.is_grace_period_past(d.epoch) {
                break;
            }
            inner.defer_ring.pop_front();
            let (seg_id, slot_id, _) = d.handle.decode();
            let entries_ptr = self.directory[seg_id as usize].load(Ordering::Relaxed);
            if entries_ptr.is_null() {
                // Shouldn't happen — a deferred handle was for a slot
                // in a segment that has since vanished. Skip rather
                // than crash; in Phase 1 segments never vanish.
                continue;
            }
            // SAFETY: as in allocate/close.
            let entry = unsafe { &(*entries_ptr)[slot_id as usize] };
            let cur_head = inner.segment_meta[seg_id as usize].free_head;
            entry.free_next.store(cur_head, Ordering::Relaxed);
            inner.segment_meta[seg_id as usize].free_head = slot_id;
            inner.segment_meta[seg_id as usize].free_count += 1;
        }
        // Bump the global epoch unconditionally so any reader entering
        // *after* this drain is tagged at a strictly later epoch than
        // closes scheduled before us.
        self.grace.advance_epoch();
    }
}

impl Drop for HandleTable {
    fn drop(&mut self) {
        for i in 0..DIRECTORY_LEN {
            let ptr = self.directory[i].load(Ordering::Acquire);
            if !ptr.is_null() {
                // SAFETY: `&mut self` proves exclusive access; every
                // segment was published exactly once and has not been
                // freed (no shrink path this slice).
                unsafe {
                    free_entries(NonNull::new_unchecked(ptr));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::FAIL_NEXT_ACQUIRE;
    use crate::handle::entry::FREE_NEXT_TAIL;
    use crate::mm::test_support::init_global_heap;

    /// Holds the integer payload of an opaque object pointer so test
    /// closures can carry it across threads without tripping Rust's
    /// disjoint-closure-captures inferring a captured `*mut ()`
    /// (which is `!Send`). The pointer is reconstituted at the call
    /// site via [`ObjPtr::ptr`].
    #[derive(Copy, Clone)]
    struct ObjPtr(usize);

    impl ObjPtr {
        fn ptr(self) -> *mut () {
            self.0 as *mut ()
        }
    }

    fn obj(addr: usize) -> *mut () {
        addr as *mut ()
    }

    fn fresh_table() -> HandleTable {
        init_global_heap();
        HandleTable::try_new(0xCAFE_BABE_DEAD_BEEF).unwrap()
    }

    // --- Construction ------------------------------------------------

    #[test]
    fn try_new_eagerly_allocates_segment_zero() {
        let t = fresh_table();
        assert_eq!(t.segments_allocated(), 1);
        assert_eq!(t.allocated_count(), 0);
    }

    // --- Allocate ----------------------------------------------------

    #[test]
    fn allocate_returns_non_null_handle() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert!(!h.is_null());
    }

    #[test]
    fn allocate_lookup_round_trip() {
        let t = fresh_table();
        let h = t.allocate(7, obj(0xBEEF), KObjectType::IoRing, Rights::READ | Rights::WRITE).unwrap();
        let ok = t.lookup(h, 7, Rights::READ).unwrap();
        assert_eq!(ok.object, obj(0xBEEF));
        assert_eq!(ok.object_type, KObjectType::IoRing);
        assert!(ok.rights.contains(Rights::READ));
        assert!(ok.rights.contains(Rights::WRITE));
    }

    #[test]
    fn allocate_rejects_incompatible_rights_for_type() {
        let t = fresh_table();
        // `MAP_WRITE` is principal-band but not on Process's allow-list.
        let err = t.allocate(1, obj(0x1000), KObjectType::Process, Rights::MAP_WRITE)
            .unwrap_err();
        assert_eq!(err, HandleError::BadRights);
    }

    #[test]
    fn allocate_many_handles_in_a_row() {
        let t = fresh_table();
        let mut handles = [RawHandle::NULL; 32];
        for (i, h) in handles.iter_mut().enumerate() {
            *h = t.allocate(1, obj(0x1000 + i), KObjectType::IoRing, Rights::READ).unwrap();
        }
        // All distinct.
        for i in 0..handles.len() {
            for j in (i + 1)..handles.len() {
                assert_ne!(handles[i], handles[j], "duplicate handle at {i} {j}");
            }
        }
        assert_eq!(t.allocated_count(), handles.len());
    }

    // --- Lookup: owner enforcement ----------------------------------

    #[test]
    fn lookup_wrong_owner_pid_returns_not_owner() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.lookup(h, 2, Rights::empty()).unwrap_err(), HandleError::NotOwner);
    }

    #[test]
    fn lookup_correct_owner_succeeds_for_zero_pid_too() {
        let t = fresh_table();
        let h = t.allocate(0, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert!(t.lookup(h, 0, Rights::READ).is_ok());
    }

    // --- Lookup: rights enforcement ---------------------------------

    #[test]
    fn lookup_insufficient_rights_returns_no_access() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.lookup(h, 1, Rights::WRITE).unwrap_err(), HandleError::NoAccess);
    }

    #[test]
    fn lookup_superset_rights_request_returns_no_access() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(
            t.lookup(h, 1, Rights::READ | Rights::WRITE).unwrap_err(),
            HandleError::NoAccess,
        );
    }

    #[test]
    fn lookup_subset_rights_request_succeeds() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ | Rights::WRITE).unwrap();
        assert!(t.lookup(h, 1, Rights::READ).is_ok());
        assert!(t.lookup(h, 1, Rights::WRITE).is_ok());
        assert!(t.lookup(h, 1, Rights::READ | Rights::WRITE).is_ok());
        assert!(t.lookup(h, 1, Rights::empty()).is_ok());
    }

    // --- Lookup: null / out-of-range --------------------------------

    #[test]
    fn null_handle_lookup_returns_null_handle() {
        let t = fresh_table();
        assert_eq!(
            t.lookup(RawHandle::NULL, 0, Rights::empty()).unwrap_err(),
            HandleError::NullHandle,
        );
    }

    #[test]
    fn out_of_range_segment_returns_invalid_handle() {
        let t = fresh_table();
        let bogus = RawHandle::encode((DIRECTORY_LEN - 1) as u32, 0, 1);
        // Segment exists but slot 0 was never allocated.
        assert_eq!(
            t.lookup(bogus, 0, Rights::empty()).unwrap_err(),
            HandleError::InvalidHandle,
        );
    }

    // --- Close -------------------------------------------------------

    #[test]
    fn close_makes_handle_invalid() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        let prev = t.close(h, 1).unwrap();
        assert_eq!(prev.0, obj(0x1000));
        assert_eq!(
            t.lookup(h, 1, Rights::empty()).unwrap_err(),
            HandleError::InvalidHandle,
        );
    }

    #[test]
    fn close_rejects_wrong_owner() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.close(h, 2).unwrap_err(), HandleError::NotOwner);
        // Still usable by the real owner.
        assert!(t.lookup(h, 1, Rights::READ).is_ok());
    }

    #[test]
    fn double_close_returns_invalid_on_second() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        t.close(h, 1).unwrap();
        assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
    }

    #[test]
    fn close_then_allocate_reuses_slot_with_new_generation() {
        let t = fresh_table();
        let h1 = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        let (seg1, slot1, gen1) = h1.decode();
        t.close(h1, 1).unwrap();
        // Allocate once more — single-context grace period elapses
        // immediately on Phase 1.
        // Force a couple more allocate/close cycles to push the
        // freelist back to slot1 if needed (single-segment, LIFO).
        let h2 = t.allocate(1, obj(0x2000), KObjectType::IoRing, Rights::READ).unwrap();
        let (seg2, slot2, gen2) = h2.decode();
        // We don't strictly require the same slot, but for a fresh
        // table with one segment the closed slot is the most recent
        // freelist push, so LIFO returns it.
        if seg1 == seg2 && slot1 == slot2 {
            assert_ne!(gen1, gen2, "generation must bump on slot reuse");
        }
        // The old handle must always be invalid.
        assert_eq!(
            t.lookup(h1, 1, Rights::empty()).unwrap_err(),
            HandleError::InvalidHandle,
        );
        // The new handle must be valid.
        assert!(t.lookup(h2, 1, Rights::READ).is_ok());
    }

    // --- Restrict ----------------------------------------------------

    #[test]
    fn restrict_cannot_amplify_rights() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        // Ask to "add" WRITE — the intersection with current rights
        // (just READ) is empty.
        t.restrict(h, 1, Rights::WRITE).unwrap();
        assert_eq!(
            t.lookup(h, 1, Rights::READ).unwrap_err(),
            HandleError::NoAccess,
        );
        // Lookup with empty rights still succeeds — the handle remains
        // valid, just stripped.
        assert!(t.lookup(h, 1, Rights::empty()).is_ok());
    }

    #[test]
    fn restrict_drops_rights() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ | Rights::WRITE).unwrap();
        t.restrict(h, 1, Rights::READ).unwrap();
        assert!(t.lookup(h, 1, Rights::READ).is_ok());
        assert_eq!(
            t.lookup(h, 1, Rights::WRITE).unwrap_err(),
            HandleError::NoAccess,
        );
    }

    #[test]
    fn restrict_rejects_wrong_owner() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.restrict(h, 2, Rights::READ).unwrap_err(), HandleError::NotOwner);
    }

    // --- Duplicate ---------------------------------------------------

    #[test]
    fn duplicate_requires_duplicate_right() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(
            t.duplicate(h, 1, Rights::READ).unwrap_err(),
            HandleError::NoAccess,
        );
    }

    #[test]
    fn duplicate_yields_independent_handle_with_intersected_rights() {
        let t = fresh_table();
        let original = t.allocate(
            1,
            obj(0x1000),
            KObjectType::IoRing,
            Rights::READ | Rights::WRITE | Rights::DUPLICATE,
        ).unwrap();
        // Duplicate dropping WRITE.
        let copy = t.duplicate(original, 1, Rights::READ | Rights::DUPLICATE).unwrap();
        assert_ne!(copy, original);
        // Copy can READ but not WRITE.
        assert!(t.lookup(copy, 1, Rights::READ).is_ok());
        assert_eq!(
            t.lookup(copy, 1, Rights::WRITE).unwrap_err(),
            HandleError::NoAccess,
        );
        // Original retains both.
        assert!(t.lookup(original, 1, Rights::WRITE).is_ok());
        // Closing one doesn't affect the other.
        t.close(copy, 1).unwrap();
        assert!(t.lookup(original, 1, Rights::READ).is_ok());
    }

    // --- Stat --------------------------------------------------------

    #[test]
    fn stat_returns_snapshot_when_inspect_granted() {
        let t = fresh_table();
        let h = t.allocate(
            42,
            obj(0x1000),
            KObjectType::IoRing,
            Rights::READ | Rights::INSPECT,
        ).unwrap();
        let s = t.stat(h, 42).unwrap();
        assert_eq!(s.object_type, KObjectType::IoRing);
        assert!(s.rights.contains(Rights::READ));
        assert_eq!(s.owner_pid, 42);
        let (_, _, generation) = h.decode();
        assert_eq!(s.generation, generation);
    }

    #[test]
    fn stat_requires_inspect_right() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.stat(h, 1).unwrap_err(), HandleError::NoAccess);
    }

    // --- Segment growth ----------------------------------------------

    #[test]
    fn segment_grows_when_first_segment_full() {
        let t = fresh_table();
        // Fill segment 0 exactly.
        for i in 0..SEGMENT_LEN {
            t.allocate(1, obj(0x1000 + i), KObjectType::IoRing, Rights::READ).unwrap();
        }
        assert_eq!(t.segments_allocated(), 1);
        // One more allocation triggers grow.
        let h = t.allocate(1, obj(0x9999), KObjectType::IoRing, Rights::READ).unwrap();
        assert_eq!(t.segments_allocated(), 2);
        let (seg, _, _) = h.decode();
        assert_eq!(seg, 1, "second segment id");
    }

    // --- Freelist invariants ----------------------------------------

    #[test]
    fn freelist_invariant_count_matches_chain() {
        let t = fresh_table();
        // Alloc then close a sequence; quiesce by way of the natural
        // lookup-induced quiesce in between.
        let mut handles = [RawHandle::NULL; 64];
        for i in 0..64 {
            handles[i] = t.allocate(1, obj(0x1000 + i), KObjectType::IoRing, Rights::READ).unwrap();
        }
        for i in (0..64).step_by(2) {
            t.close(handles[i], 1).unwrap();
        }
        // Force a drain by attempting another allocate (which may pull
        // a closed slot back off the freelist; do one then close to
        // re-pad). The invariant we want: free_count for segment 0
        // matches the linked-list length.
        let h_temp = t.allocate(1, obj(0xAAAA), KObjectType::IoRing, Rights::READ).unwrap();
        t.close(h_temp, 1).unwrap();
        // Take the lock to inspect — read free_head and free_count.
        let guard = t.inner.lock();
        let free_head = guard.segment_meta[0].free_head;
        let free_count = guard.segment_meta[0].free_count;
        drop(guard);
        let entries_ptr = t.directory[0].load(Ordering::Acquire);
        assert!(!entries_ptr.is_null());
        let entries = unsafe { &*entries_ptr };
        let mut idx = free_head;
        let mut walked = 0u32;
        while idx != FREE_NEXT_TAIL {
            assert!((idx as usize) < SEGMENT_LEN, "freelist idx out of range");
            walked += 1;
            assert!(walked <= SEGMENT_LEN as u32 + 1, "freelist appears cyclic");
            idx = entries[idx as usize].free_next.load(Ordering::Relaxed);
        }
        assert_eq!(walked, free_count, "free_count mismatch with chain length");
    }

    // --- ObjectRef seam stub ----------------------------------------

    #[test]
    fn failed_acquire_refcount_returns_invalid_handle() {
        let t = fresh_table();
        let h = t.allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ).unwrap();
        // Force step 7 to fail on the next lookup *on this thread*.
        FAIL_NEXT_ACQUIRE.with(|f| f.set(true));
        assert_eq!(
            t.lookup(h, 1, Rights::READ).unwrap_err(),
            HandleError::InvalidHandle,
        );
        // Flag is one-shot; the subsequent lookup succeeds.
        assert!(t.lookup(h, 1, Rights::READ).is_ok());
    }

    // --- Generation wrap --------------------------------------------

    #[test]
    fn generation_wraps_at_u32_max() {
        let t = fresh_table();
        // Allocate so we know which slot to poke.
        let h1_initial = t
            .allocate(1, obj(0x1000), KObjectType::IoRing, Rights::READ)
            .unwrap();
        let (seg, slot, _) = h1_initial.decode();
        // Poke the entry's generation to `u32::MAX - 1`. The handle we
        // close with must agree, so re-encode it.
        let entries_ptr = t.directory[seg as usize].load(Ordering::Acquire);
        let entry = unsafe { &(*entries_ptr)[slot as usize] };
        entry.generation.store(u32::MAX - 1, Ordering::Relaxed);
        let h1_poked = RawHandle::encode(seg, slot, u32::MAX - 1);
        t.close(h1_poked, 1).unwrap();
        // The very next allocation in a fresh single-segment table
        // pops the LIFO freelist head — the slot we just closed — and
        // bumps its generation to `u32::MAX`.
        let h2 = t
            .allocate(1, obj(0x2000), KObjectType::IoRing, Rights::READ)
            .unwrap();
        let (s2, sl2, g2) = h2.decode();
        assert_eq!((s2, sl2), (seg, slot), "expected LIFO reuse of closed slot");
        assert_eq!(g2, u32::MAX, "generation must bump from MAX-1 to MAX");
        t.close(h2, 1).unwrap();
        // And the next one wraps to 0.
        let h3 = t
            .allocate(1, obj(0x3000), KObjectType::IoRing, Rights::READ)
            .unwrap();
        let (s3, sl3, g3) = h3.decode();
        assert_eq!((s3, sl3), (seg, slot));
        assert_eq!(g3, 0, "generation wraps from u32::MAX to 0");
    }

    // --- Multi-thread tests -----------------------------------------

    /// Each of N threads owns its own PID space and runs a small
    /// allocate/lookup/close loop. Cross-pid lookups must always fail;
    /// at the end no handles remain.
    #[test]
    fn concurrent_allocate_lookup_close_pid_isolation() {
        use std::sync::Arc;
        use std::thread;

        let t = Arc::new(fresh_table());
        const N_THREADS: usize = 8;
        const ITERS: usize = 2000;

        let handles: Vec<_> = (0..N_THREADS)
            .map(|tid| {
                let t = Arc::clone(&t);
                let my_pid = (tid as u32) + 1;
                let token = ObjPtr(0x1000 + tid);
                thread::spawn(move || {
                    for _ in 0..ITERS {
                        let h = t
                            .allocate(my_pid, token.ptr(), KObjectType::IoRing, Rights::READ)
                            .expect("allocate");
                        // Owner can look up.
                        let ok = t.lookup(h, my_pid, Rights::READ).expect("lookup");
                        assert_eq!(ok.object, token.ptr());
                        // Wrong owner cannot.
                        let other_pid = if my_pid == 1 { 2 } else { 1 };
                        assert_eq!(
                            t.lookup(h, other_pid, Rights::READ).unwrap_err(),
                            HandleError::NotOwner,
                        );
                        t.close(h, my_pid).expect("close");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("join");
        }
        // Allow grace-period drain to catch up via a final allocate.
        let h = t.allocate(99, obj(0xFEED), KObjectType::IoRing, Rights::READ).unwrap();
        t.close(h, 99).unwrap();
        // allocated_count is approximate while drains are mid-flight;
        // a final allocate-close pair drains the queue.
        assert!(t.allocated_count() <= 1, "stray handles after stress: {}", t.allocated_count());
    }

    /// Many threads hammer one handle: one writer closing-and-
    /// reallocating, several readers looking up. Any reader that sees
    /// a non-error `LookupOk` must observe internally consistent
    /// metadata (object pointer matches owner_pid via a sentinel
    /// encoding) — proves the seqlock catches torn reads.
    ///
    /// **Note on workload shape.** Production Phase 1 is single-CPU,
    /// so the closing thread *is* the only possible reader and the
    /// defer ring never accumulates. Hosted multi-threaded tests
    /// expose a separate seqlock-starvation pattern not present on
    /// target: a reader stuck spinning on `read_snapshot` (because
    /// the writer keeps toggling the seq on the same entry) never
    /// quiesces, so every close scheduled while that reader is in
    /// flight cannot reclaim. We throttle the writer with
    /// `yield_now` so the OS scheduler periodically runs readers to
    /// completion, and we cap iterations so the test runs in a
    /// bounded window. The seqlock correctness property is still
    /// exercised end-to-end.
    #[test]
    fn concurrent_torn_read_torture() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let t = Arc::new(fresh_table());
        let stop = Arc::new(AtomicBool::new(false));

        // Writer: allocates with sentinel `object = 0x1000 + pid`,
        // closes, repeats. Yields every 8 cycles so readers can
        // quiesce and the defer ring drains.
        let writer = {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut pid = 1u32;
                let mut cycles_since_yield = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let token = obj(0x1000 + (pid as usize));
                    if let Ok(h) = t.allocate(pid, token, KObjectType::IoRing, Rights::READ) {
                        let _ = t.close(h, pid);
                    }
                    pid = pid.wrapping_add(1);
                    if pid == 0 {
                        pid = 1;
                    }
                    cycles_since_yield += 1;
                    if cycles_since_yield == 8 {
                        thread::yield_now();
                        cycles_since_yield = 0;
                    }
                }
            })
        };

        // Readers: scan the (slot, pid, generation) space; whenever
        // a lookup succeeds, the sentinel invariant must hold.
        let mut readers = Vec::new();
        for _ in 0..2 {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            readers.push(thread::spawn(move || {
                let mut iterations = 0u32;
                while !stop.load(Ordering::Relaxed) && iterations < 1_000 {
                    for slot in 0..4u32 {
                        for pid in 1..8u32 {
                            for generation in 1..8u32 {
                                let h = RawHandle::encode(0, slot, generation);
                                if let Ok(ok) = t.lookup(h, pid, Rights::READ) {
                                    let expected = obj(0x1000 + (pid as usize));
                                    assert_eq!(
                                        ok.object, expected,
                                        "torn read: pid={pid} object={:?} expected={:?}",
                                        ok.object, expected,
                                    );
                                }
                            }
                        }
                    }
                    iterations += 1;
                    // Brief yield so the writer also makes progress.
                    thread::yield_now();
                }
            }));
        }

        // Bounded run window.
        std::thread::sleep(std::time::Duration::from_millis(30));
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }

    // --- Single-context defer drain ---------------------------------

    #[test]
    fn close_then_allocate_drains_immediately_on_single_context() {
        let t = fresh_table();
        // Allocate, close, allocate, close — a tight loop on a single
        // context should never grow the defer ring beyond a small
        // working set (in fact: <=1 entry after the loop, because the
        // close pushes and the next allocate drains).
        for i in 0..1024 {
            let h = t.allocate(1, obj(0x1000 + i), KObjectType::IoRing, Rights::READ).unwrap();
            t.close(h, 1).unwrap();
        }
        // Final state: zero live handles.
        assert_eq!(t.allocated_count(), 0);
        // The defer ring is internal but should not have grown.
        // Allocating again must succeed without OOM.
        let h = t.allocate(1, obj(0xFEED), KObjectType::IoRing, Rights::READ).unwrap();
        t.close(h, 1).unwrap();
    }
}
