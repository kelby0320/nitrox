//! The segmented kernel [`HandleTable`] — allocate, lookup, close, restrict,
//! duplicate, stat, quiesce.
//!
//! See `docs/spec/handle-encoding.md` (wire format),
//! `docs/architecture/handle-system.md` (overview) and the
//! [parent module](super) for the two-layer concurrency model.

use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::libkern::handle::{KObjectType, RawHandle, Rights};
use crate::libkern::{AllocError, KVec, SpinLock};
use crate::object::ObjectRef;

use super::entry::{WriteGuard, read_snapshot};
use super::grace::GraceTracker;
use super::prng::Xorshift64;
use super::segment::{SegmentEntries, SegmentMeta, free_entries, try_alloc_initialised};
use super::type_rights::is_rights_compatible;
use super::{DIRECTORY_LEN, SEGMENT_LEN, current_ctx_id, release_refcount, try_acquire_refcount};
use crate::libkern::lockrank::LockRank;

/// Deferred-close entries the per-table ring holds between drains, sized to
/// absorb a burst of closes. If it fills, `close` releases the rank-3 lock,
/// yields and retries.
pub const DEFER_RING_CAPACITY: usize = 256;

#[cfg(test)]
std::thread_local! {
    /// One-shot per-thread flag forcing the next [`HandleTable::allocate`] on
    /// this thread to fail with [`HandleError::OutOfMemory`], to exercise the
    /// duplicate-error reclaim path. Per-thread as for
    /// [`crate::handle::FAIL_NEXT_ACQUIRE`].
    pub(crate) static FAIL_NEXT_ALLOCATE: core::cell::Cell<bool> =
        const { core::cell::Cell::new(false) };
}

/// Backoff for `close` when the defer ring is full and `drain` freed nothing.
/// Tests yield to the host scheduler so a reader stuck in `read_snapshot` can
/// finish; production emits a `PAUSE`-style hint and the caller spins.
#[cfg(test)]
fn yield_for_grace() {
    std::thread::yield_now();
}

#[cfg(not(test))]
fn yield_for_grace() {
    core::hint::spin_loop();
}

/// Why a handle table operation failed. Syscall layers may collapse `NotOwner`
/// into `InvalidHandle` to avoid leaking owner existence; the table reports the
/// precise reason for telemetry.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum HandleError {
    /// The supplied handle was [`RawHandle::NULL`].
    NullHandle,
    /// Segment id, slot id, generation, or `object`-non-null check failed.
    InvalidHandle,
    /// The caller's PID does not match the entry's `owner_pid`.
    NotOwner,
    /// The caller asked for rights the handle does not carry.
    NoAccess,
    /// The directory is full and no further segments can be allocated.
    OutOfHandles,
    /// A segment allocation failed: the kernel heap is exhausted.
    OutOfMemory,
    /// `allocate` got rights invalid for the `KObjectType` (type-rights matrix).
    BadRights,
}

impl From<AllocError> for HandleError {
    fn from(_: AllocError) -> Self {
        HandleError::OutOfMemory
    }
}

/// What a successful [`HandleTable::lookup`] returns. `object` holds one
/// refcount while the `LookupOk` lives (released on drop), which keeps the
/// object alive across a syscall and closes the `duplicate` TOCTOU.
#[derive(Debug)]
pub struct LookupOk {
    /// The handle's object, pinned by one reference.
    pub object: ObjectRef,
    /// The handle's full rights, not just those `lookup` required.
    pub rights: Rights,
}

/// Snapshot of handle metadata returned by [`HandleTable::stat`].
#[derive(Copy, Clone, Debug)]
pub struct HandleStat {
    /// The object's type.
    pub object_type: KObjectType,
    /// The handle's rights.
    pub rights: Rights,
    /// The owning process: the caller, whom `lookup` verified.
    pub owner_pid: u32,
    /// The generation encoded in the handle.
    pub generation: u32,
}

/// The object pointer and type returned by [`HandleTable::close`].
///
/// `close` nulls the slot but does **not** decrement the refcount: it transfers
/// the entry's one reference to this token, which the caller must account for
/// (normally `ObjectRef::from_raw(co.0, co.1)`, then drop). Leaving the
/// decrement to the caller keeps a racing `lookup` safe (a concurrent
/// `try_acquire` sees either a positive count or zero) and keeps destruction,
/// which reaches the rank-6 allocator, out from under the rank-3 lock. The
/// wrapper makes the `Result` `Send`; a bare `*mut ()` is not.
#[derive(Copy, Clone, Debug)]
pub struct ClosedObject(pub *mut (), pub KObjectType);

// SAFETY: as `LookupOk` — the pointer is opaque at the handle-table
// layer; thread-safety of the pointee is the caller's concern.
unsafe impl Send for ClosedObject {}
// SAFETY: as `Send`.
unsafe impl Sync for ClosedObject {}

/// Where a [`HandleTable::close_owned_batch`] sweep resumes: it stops mid-scan
/// so references are dropped outside the rank-3 lock. Start at
/// [`SweepCursor::START`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SweepCursor {
    seg: usize,
    slot: usize,
}

impl SweepCursor {
    /// A sweep that has examined nothing yet.
    pub const START: SweepCursor = SweepCursor { seg: 0, slot: 0 };
}

/// `directory` is an inline array of `AtomicPtr`; each non-null slot is a
/// 256 KiB [`SegmentEntries`] block. Lookups follow `directory[seg_id]` with
/// one `Acquire` load and no lock. The rank-3 [`SpinLock`] in `inner`
/// serialises allocate / close / restrict / segment growth.
pub struct HandleTable {
    directory: [AtomicPtr<SegmentEntries>; DIRECTORY_LEN],
    /// Allocator hint: the segment at which the free-list scan starts.
    next_segment_hint: AtomicU32,
    grace: GraceTracker,
    inner: SpinLock<Inner>,
}

struct Inner {
    /// Per-segment free-list metadata, indexed like `directory`; only the
    /// first `segments_count` are meaningful.
    segment_meta: [SegmentMeta; DIRECTORY_LEN],
    /// Segments brought online; `<= DIRECTORY_LEN`.
    segments_count: u32,
    defer_ring: DeferredQueue,
    prng: Xorshift64,
}

#[derive(Copy, Clone)]
struct DeferredClose {
    handle: RawHandle,
    epoch: u64,
}

/// Fixed-capacity ring of pending deferred closes, allocated at construction.
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
        Ok(Self { buf, head: 0, len: 0 })
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
    /// An empty table seeded with `seed` for the freelist shuffles, with
    /// segment 0 grown eagerly so the first allocation avoids the grow path.
    pub fn try_new(seed: u64) -> Result<Self, HandleError> {
        let defer_ring = DeferredQueue::try_new(DEFER_RING_CAPACITY)?;
        let table = Self {
            directory: [const { AtomicPtr::new(ptr::null_mut()) }; DIRECTORY_LEN],
            next_segment_hint: AtomicU32::new(0),
            grace: GraceTracker::new(),
            inner: SpinLock::new(
                LockRank::HandleTable,
                Inner {
                    segment_meta: [SegmentMeta::empty(); DIRECTORY_LEN],
                    segments_count: 0,
                    defer_ring,
                    prng: Xorshift64::new(seed),
                },
            ),
        };
        table.grow_one()?;
        Ok(table)
    }

    /// Allocate one more segment and publish it. Drops the rank-3 lock during
    /// the heap allocation (`kernel/CLAUDE.md` § "Forbidden patterns"); if
    /// another caller publishes the slot first, the spare is freed and this
    /// returns `Ok`.
    fn grow_one(&self) -> Result<(), HandleError> {
        let (next_seg, seed) = {
            let mut guard = self.inner.lock();
            if guard.segments_count as usize >= DIRECTORY_LEN {
                return Err(HandleError::OutOfHandles);
            }
            let next = guard.segments_count;
            let seed = guard.prng.next_u64();
            (next, seed)
        };

        let (entries, meta) = try_alloc_initialised(seed)?;

        let mut guard = self.inner.lock();
        // A concurrent grower may have published this slot or grown past
        // `next_seg`.
        if guard.segments_count > next_seg || !self.directory[next_seg as usize].load(Ordering::Relaxed).is_null() {
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
        // Release: a lookup that sees the pointer sees the initialised entries.
        self.directory[next_seg as usize].store(entries.as_ptr(), Ordering::Release);
        Ok(())
    }

    /// Allocate a fresh handle for `object`, owned by `owner_pid`.
    /// `object_type` and `rights` must satisfy the type-rights matrix or this
    /// returns [`HandleError::BadRights`]. `object` is type-erased and **adopts
    /// one reference** the caller already holds (a `KBox::into_raw()` pointer, or
    /// one from [`ObjectRef::into_raw`], as `duplicate` does). `allocate` never
    /// bumps the refcount; on failure the caller still owns the reference.
    pub fn allocate(
        &self,
        owner_pid: u32,
        object: *mut (),
        object_type: KObjectType,
        rights: Rights,
    ) -> Result<RawHandle, HandleError> {
        debug_assert!(!object.is_null(), "callers must not store null objects");
        #[cfg(test)]
        {
            if FAIL_NEXT_ALLOCATE.with(|f| f.replace(false)) {
                return Err(HandleError::OutOfMemory);
            }
        }
        if !is_rights_compatible(object_type, rights) {
            return Err(HandleError::BadRights);
        }

        loop {
            {
                let mut guard = self.inner.lock();
                self.drain_expired(&mut guard);

                let segments_count = guard.segments_count as usize;
                let hint =
                    (self.next_segment_hint.load(Ordering::Relaxed) as usize).min(segments_count.saturating_sub(1));
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
                        let entry = unsafe { &(*entries_ptr)[slot_id as usize] };
                        let next_head = entry.free_next.load(Ordering::Relaxed);
                        guard.segment_meta[seg_id].free_head = next_head;
                        guard.segment_meta[seg_id].free_count -= 1;

                        // Bump within the 31-bit field: the mask wraps
                        // `GENERATION_MAX` to 0 and keeps the reserved bit 63
                        // clear. The generation-ABA this admits (a stale handle
                        // re-validating after 2^31 reuses of one slot, within the
                        // same owner) is accepted; see docs/spec/handle-encoding.md
                        // § "Generation counter behavior".
                        let new_gen =
                            entry.generation.load(Ordering::Relaxed).wrapping_add(1) & RawHandle::GENERATION_MAX;
                        {
                            let _wg = WriteGuard::new(entry);
                            entry.generation.store(new_gen, Ordering::Relaxed);
                            entry.owner_pid.store(owner_pid, Ordering::Relaxed);
                            entry.rights.store(rights.bits(), Ordering::Relaxed);
                            entry.object_type.store(object_type as u32, Ordering::Relaxed);
                            entry.next_owned.store(RawHandle::NULL.bits(), Ordering::Relaxed);
                            // Publish the object pointer LAST: the Release pairs
                            // with the Acquire in `lookup`, so a reader seeing
                            // `object != null` sees every metadata store above.
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
            self.grow_one()?;
        }
    }

    /// Look up a handle and validate it per spec § "Validation algorithm".
    /// `required` is the rights subset the caller needs; pass
    /// [`Rights::empty()`] to merely confirm the handle is live.
    pub fn lookup(&self, h: RawHandle, caller_pid: u32, required: Rights) -> Result<LookupOk, HandleError> {
        // Step 0: enter a read-side critical section, dropped on every exit
        // path; while held, no slot we might reference is recycled.
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

        // Bounded retry: writer windows are capped at one per contending writer;
        // 1024 is a debug tripwire.
        let mut retries = 0u32;
        loop {
            // Step 5: seqlock-protected metadata snapshot.
            let snap = read_snapshot(entry);

            // Step 6: object non-null.
            let obj = entry.object.load(Ordering::Acquire);
            if obj.is_null() {
                return Err(HandleError::InvalidHandle);
            }

            let object_type = match KObjectType::from_u32(snap.object_type) {
                Some(t) => t,
                None => return Err(HandleError::InvalidHandle),
            };

            // Step 7: try to bump the refcount (fails if it is already zero,
            // i.e. the object is being torn down). The reference is adopted at
            // step 12 or released on the retry/error paths below.
            // SAFETY: `obj` was read from a live entry under the grace guard, so it addresses a live object.
            if !unsafe { try_acquire_refcount(obj, object_type) } {
                return Err(HandleError::InvalidHandle);
            }

            // Step 8: re-read seq; if changed or odd, release and retry.
            let s2 = entry.seq.load(Ordering::Acquire);
            if s2 != snap.seq || (s2 & 1) != 0 {
                // SAFETY: releases the reference `try_acquire_refcount` took above.
                unsafe { release_refcount(obj, object_type) };
                retries += 1;
                debug_assert!(retries < 1024, "handle table lookup spinning past 1024 retries — logic bug",);
                continue;
            }

            // Step 9: generation match.
            if snap.generation != gen_expected {
                // SAFETY: releases the reference `try_acquire_refcount` took above.
                unsafe { release_refcount(obj, object_type) };
                return Err(HandleError::InvalidHandle);
            }

            // Step 10: owner_pid match — security-critical.
            if snap.owner_pid != caller_pid {
                // SAFETY: releases the reference `try_acquire_refcount` took above.
                unsafe { release_refcount(obj, object_type) };
                return Err(HandleError::NotOwner);
            }

            // Step 11: rights subset.
            if !required.is_subset_of(snap.rights) {
                // SAFETY: releases the reference `try_acquire_refcount` took above.
                unsafe { release_refcount(obj, object_type) };
                return Err(HandleError::NoAccess);
            }

            // Step 12: adopt the reference step 7 took into an `ObjectRef` (no
            // second increment); dropping the `LookupOk` releases it.
            return Ok(LookupOk {
                // SAFETY: step 7 acquired exactly one reference on `obj`
                // (type `object_type`); `from_raw` adopts that reference
                // without double-counting, and `obj` is non-null (step 6).
                object: unsafe { ObjectRef::from_raw(obj, object_type) },
                rights: snap.rights,
            });
        }
    }

    /// Close a handle, returning a [`ClosedObject`] with the object pointer and
    /// type; the caller releases the handle's reference (see [`ClosedObject`]).
    /// Does not decrement the refcount itself.
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

        // Validate under the lock: plain loads suffice since every other writer
        // needs the same lock. **This order is not `lookup`'s**, which reads the
        // object pointer at step 6, before generation and owner. Observable: a
        // non-owner closing a closed handle gets `NotOwner` here and
        // `InvalidHandle` from `lookup`.
        // TODO(handle-validation-order): see `docs/rationale/deferred-decisions.md`.
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
        // Capture the type before nulling so the caller can rebuild an `ObjectRef`.
        let object_type = match KObjectType::from_u32(entry.object_type.load(Ordering::Relaxed)) {
            Some(t) => t,
            None => return Err(HandleError::InvalidHandle),
        };

        // Null the object under the seqlock; the generation is NOT bumped (spec
        // § "Generation counter behavior").
        {
            let _wg = WriteGuard::new(entry);
            entry.object.store(ptr::null_mut(), Ordering::Release);
        }

        // Snapshot the epoch *before* draining (drain advances it), so the
        // deferral waits only for readers in flight at close time.
        let epoch = self.grace.current_epoch();
        let deferred = DeferredClose { handle: h, epoch };
        // Backpressure: drain and push; if the ring is still full, drop the
        // rank-3 lock so a spinning reader (stuck in `read_snapshot` under write
        // pressure) can finish and quiesce, yield, and retry.
        loop {
            self.drain_expired(&mut guard);
            if guard.defer_ring.push(deferred).is_ok() {
                break;
            }
            drop(guard);
            yield_for_grace();
            guard = self.inner.lock();
        }

        Ok(ClosedObject(prev_obj, object_type))
    }

    /// Attenuate a handle's rights in place to `existing & new_rights`; the spec
    /// forbids amplification, so extra bits are silently dropped.
    pub fn restrict(&self, h: RawHandle, caller_pid: u32, new_rights: Rights) -> Result<(), HandleError> {
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

        // Same ladder as `close`, and the same divergence from `lookup`'s.
        // TODO(handle-validation-order): see `docs/rationale/deferred-decisions.md`.
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

    /// Duplicate a handle: a new handle to the same object with rights
    /// `existing & new_rights`; requires [`Rights::DUPLICATE`]. The
    /// `lookup`→`allocate` gap is race-free: `lookup`'s [`ObjectRef`] holds one
    /// reference, so a concurrent `close` of the source can drop only the source
    /// handle's reference, never the object's last. That reference moves into the
    /// new handle ([`ObjectRef::into_raw`] + `allocate`, which adopts it without
    /// bumping) and is reclaimed if `allocate` fails. See
    /// `docs/architecture/handle-system.md`.
    pub fn duplicate(&self, h: RawHandle, caller_pid: u32, new_rights: Rights) -> Result<RawHandle, HandleError> {
        let info = self.lookup(h, caller_pid, Rights::DUPLICATE)?;
        let dup_rights = info.rights & new_rights;
        let (object, object_type) = info.object.into_raw();
        // DUPLICATE may be dropped by omitting it from `new_rights`; it is not forced.
        match self.allocate(caller_pid, object, object_type, dup_rights) {
            Ok(new_handle) => Ok(new_handle),
            Err(e) => {
                // SAFETY: `into_raw` above transferred exactly one
                // outstanding reference to us; we account for it once.
                drop(unsafe { ObjectRef::from_raw(object, object_type) });
                Err(e)
            }
        }
    }

    /// Snapshot a handle's metadata for `sys_handle_stat`; requires
    /// [`Rights::INSPECT`]. All four fields come from `lookup`'s single
    /// seqlock-bracketed snapshot (`owner_pid` and `generation` were verified at
    /// steps 10 and 9). A second `read_snapshot` would race: a concurrent
    /// close-plus-realloc of the slot (legal between two threads of one owner)
    /// could install a new generation and owner, mixing two slot lifetimes.
    pub fn stat(&self, h: RawHandle, caller_pid: u32) -> Result<HandleStat, HandleError> {
        let info = self.lookup(h, caller_pid, Rights::INSPECT)?;
        let (_, _, generation) = h.decode();
        Ok(HandleStat {
            object_type: info.object.object_type(),
            rights: info.rights,
            owner_pid: caller_pid,
            generation,
        })
    }

    /// Mark the calling context quiescent, for paths that took no read guard.
    pub fn quiesce(&self, ctx_id: u32) {
        self.grace.mark_quiescent(ctx_id);
    }

    /// Number of segments currently brought online.
    pub fn segments_allocated(&self) -> usize {
        self.inner.lock().segments_count as usize
    }

    /// Approximate count of live handles; walks every segment under the rank-3 lock.
    pub fn allocated_count(&self) -> usize {
        let guard = self.inner.lock();
        let mut sum = 0usize;
        for seg_id in 0..guard.segments_count as usize {
            sum += SEGMENT_LEN - guard.segment_meta[seg_id].free_count as usize;
        }
        sum -= guard.defer_ring.len();
        sum
    }

    /// Close up to `out.len()` live handles owned by `pid`, resuming at
    /// `cursor`, writing each closed slot's object into `out`; returns
    /// `(count, more_remain)`. As with [`close`](HandleTable::close) the caller
    /// **must** release each returned reference, and only **after this
    /// returns**, never under the rank-3 lock: a destructor can take rank-4
    /// object locks and, for an IPC endpoint, rank-1 `SCHED`, inverting the
    /// ranking.
    ///
    /// `more_remain` is `true` when the batch filled, the deferred-close ring is
    /// full (the caller draining its batch is what lets it drain), or the scan
    /// has not reached the end; call again with the same `cursor` until `false`.
    /// This is the process-exit sweep: otherwise a dead process's entries pin
    /// their objects, notably its end of every pipe, forever.
    pub fn close_owned_batch(
        &self,
        pid: u32,
        cursor: &mut SweepCursor,
        out: &mut [Option<ClosedObject>],
    ) -> (usize, bool) {
        let mut n = 0usize;
        if out.is_empty() {
            return (0, true);
        }
        let mut guard = self.inner.lock();
        self.drain_expired(&mut guard);
        let segments = guard.segments_count as usize;

        while cursor.seg < segments {
            let entries_ptr = self.directory[cursor.seg].load(Ordering::Relaxed);
            if entries_ptr.is_null() {
                cursor.seg += 1;
                cursor.slot = 0;
                continue;
            }
            while cursor.slot < SEGMENT_LEN {
                // SAFETY: as in `close` — segments are published once and outlive
                // the table; the rank-3 lock excludes every other writer.
                let entry = unsafe { &(*entries_ptr)[cursor.slot] };
                let matches =
                    entry.owner_pid.load(Ordering::Relaxed) == pid && !entry.object.load(Ordering::Acquire).is_null();
                if !matches {
                    cursor.slot += 1;
                    continue;
                }
                if n == out.len() {
                    return (n, true); // batch full — resume at this same slot
                }
                let obj = entry.object.load(Ordering::Acquire);
                let object_type = match KObjectType::from_u32(entry.object_type.load(Ordering::Relaxed)) {
                    Some(t) => t,
                    None => {
                        // A malformed entry must not stall the sweep.
                        cursor.slot += 1;
                        continue;
                    }
                };
                let handle =
                    RawHandle::encode(cursor.seg as u32, cursor.slot as u32, entry.generation.load(Ordering::Relaxed));
                let epoch = self.grace.current_epoch();
                if guard.defer_ring.push(DeferredClose { handle, epoch }).is_err() {
                    // Ring full and nothing expired: stop *without* closing this
                    // slot; the caller releasing its batch frees the ring.
                    return (n, true);
                }
                {
                    let _wg = WriteGuard::new(entry);
                    entry.object.store(ptr::null_mut(), Ordering::Release);
                }
                out[n] = Some(ClosedObject(obj, object_type));
                n += 1;
                cursor.slot += 1;
            }
            cursor.seg += 1;
            cursor.slot = 0;
        }
        (n, false)
    }

    /// Pop every deferred close whose grace period has elapsed, returning its
    /// slot to the freelist, then bump the global epoch so later closes are
    /// tagged with an epoch their context cannot have observed. Called with the
    /// rank-3 lock held.
    fn drain_expired(&self, inner: &mut Inner) {
        while let Some(d) = inner.defer_ring.front() {
            if !self.grace.is_grace_period_past(d.epoch) {
                break;
            }
            inner.defer_ring.pop_front();
            let (seg_id, slot_id, _) = d.handle.decode();
            let entries_ptr = self.directory[seg_id as usize].load(Ordering::Relaxed);
            if entries_ptr.is_null() {
                // Segments are never freed before drop, so this should not
                // happen; skip rather than crash.
                continue;
            }
            // SAFETY: as in allocate/close.
            let entry = unsafe { &(*entries_ptr)[slot_id as usize] };
            let cur_head = inner.segment_meta[seg_id as usize].free_head;
            entry.free_next.store(cur_head, Ordering::Relaxed);
            inner.segment_meta[seg_id as usize].free_head = slot_id;
            inner.segment_meta[seg_id as usize].free_count += 1;
        }
        // Bump unconditionally so a reader entering after this drain is tagged
        // at a strictly later epoch than closes scheduled before it.
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
                // freed (there is no shrink path).
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
    use crate::libkern::KBox;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;
    use crate::object::{Process, Thread};

    fn fresh_table() -> HandleTable {
        init_global_heap();
        HandleTable::try_new(0xCAFE_BABE_DEAD_BEEF).unwrap()
    }

    fn mk_process(pid: u32) -> *mut () {
        KBox::into_raw(Process::try_new(pid).unwrap()).as_ptr() as *mut ()
    }

    fn mk_thread(tid: u32, owner_pid: u32) -> *mut () {
        KBox::into_raw(Thread::try_new(tid, owner_pid).unwrap()).as_ptr() as *mut ()
    }

    fn fake_obj(addr: usize) -> *mut () {
        addr as *mut ()
    }

    fn close_release(t: &HandleTable, h: RawHandle, pid: u32) -> Result<(), HandleError> {
        let co = t.close(h, pid)?;
        // SAFETY: `co` carries exactly the handle's one reference; we
        // account for it once.
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        Ok(())
    }

    fn process_magic_ok(r: &ObjectRef) -> bool {
        debug_assert_eq!(r.object_type(), KObjectType::Process);
        // SAFETY: `r` pins a live `Process`.
        unsafe { &*(r.as_ptr() as *const Process) }.magic_ok()
    }

    fn sig() -> Rights {
        Rights::SIGNAL
    }
    fn sigterm() -> Rights {
        Rights::SIGNAL | Rights::TERMINATE
    }

    #[test]
    fn try_new_eagerly_allocates_segment_zero() {
        let t = fresh_table();
        assert_eq!(t.segments_allocated(), 1);
        assert_eq!(t.allocated_count(), 0);
    }

    #[test]
    fn allocate_returns_non_null_handle() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert!(!h.is_null());
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn allocate_lookup_round_trip() {
        let t = fresh_table();
        let p = mk_process(7);
        let h = t.allocate(7, p, KObjectType::Process, sigterm()).unwrap();
        let ok = t.lookup(h, 7, sig()).unwrap();
        assert_eq!(ok.object.as_ptr(), p);
        assert_eq!(ok.object.object_type(), KObjectType::Process);
        assert!(ok.rights.contains(Rights::SIGNAL));
        assert!(ok.rights.contains(Rights::TERMINATE));
        drop(ok);
        close_release(&t, h, 7).unwrap();
    }

    fn sweep_owned(t: &HandleTable, pid: u32) -> usize {
        let mut cursor = SweepCursor::START;
        let mut total = 0;
        loop {
            let mut batch: [Option<ClosedObject>; 4] = [None; 4];
            let (n, more) = t.close_owned_batch(pid, &mut cursor, &mut batch);
            for co in batch[..n].iter().flatten() {
                // SAFETY: each carries exactly the closed handle's one reference.
                drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
            }
            total += n;
            if !more {
                return total;
            }
        }
    }

    #[test]
    fn sweep_closes_every_handle_of_one_process_and_no_others() {
        let t = fresh_table();
        let mut doomed = Vec::new();
        let mut survivor = Vec::new();
        for i in 0..5 {
            doomed.push(t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap());
            survivor.push(t.allocate(2, mk_process(2), KObjectType::Process, sig()).unwrap());
            let _ = i;
        }

        assert_eq!(sweep_owned(&t, 1), 5);

        for h in &doomed {
            assert_eq!(t.lookup(*h, 1, sig()).unwrap_err(), HandleError::InvalidHandle);
        }
        for h in &survivor {
            assert!(t.lookup(*h, 2, sig()).is_ok());
        }
        for h in survivor {
            close_release(&t, h, 2).unwrap();
        }
    }

    #[test]
    fn sweep_releases_the_objects() {
        let t = fresh_table();
        let before = test_probe::process_destroys();
        for _ in 0..3 {
            t.allocate(9, mk_process(9), KObjectType::Process, sig()).unwrap();
        }
        assert_eq!(test_probe::process_destroys(), before, "nothing destroyed yet");
        assert_eq!(sweep_owned(&t, 9), 3);
        assert_eq!(
            test_probe::process_destroys(),
            before + 3,
            "each swept handle held the last reference to its object"
        );
    }

    #[test]
    fn sweep_resumes_across_batches() {
        let t = fresh_table();
        for _ in 0..17 {
            t.allocate(3, mk_process(3), KObjectType::Process, sig()).unwrap();
        }
        assert_eq!(sweep_owned(&t, 3), 17);
        assert_eq!(sweep_owned(&t, 3), 0, "a second sweep finds nothing left");
    }

    #[test]
    fn sweep_of_a_process_with_no_handles_is_a_no_op() {
        let t = fresh_table();
        let h = t.allocate(4, mk_process(4), KObjectType::Process, sig()).unwrap();
        assert_eq!(sweep_owned(&t, 99), 0);
        assert!(t.lookup(h, 4, sig()).is_ok());
        close_release(&t, h, 4).unwrap();
    }

    #[test]
    fn swept_slots_are_reusable() {
        let t = fresh_table();
        let before = t.allocated_count();
        for _ in 0..8 {
            t.allocate(5, mk_process(5), KObjectType::Process, sig()).unwrap();
        }
        assert_eq!(t.allocated_count(), before + 8);
        assert_eq!(sweep_owned(&t, 5), 8);
        let h = t.allocate(6, mk_process(6), KObjectType::Process, sig()).unwrap();
        close_release(&t, h, 6).unwrap();
        assert_eq!(t.allocated_count(), before, "swept slots were not returned to the freelist");
    }

    #[test]
    fn allocate_rejects_incompatible_rights_for_type() {
        let t = fresh_table();
        let err = t.allocate(1, fake_obj(0x1000), KObjectType::Process, Rights::MAP_WRITE).unwrap_err();
        assert_eq!(err, HandleError::BadRights);
    }

    #[test]
    fn allocate_many_handles_in_a_row() {
        let t = fresh_table();
        let mut handles = [RawHandle::NULL; 32];
        for (i, h) in handles.iter_mut().enumerate() {
            *h = t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap();
        }
        for i in 0..handles.len() {
            for j in (i + 1)..handles.len() {
                assert_ne!(handles[i], handles[j], "duplicate handle at {i} {j}");
            }
        }
        assert_eq!(t.allocated_count(), handles.len());
        for h in handles {
            close_release(&t, h, 1).unwrap();
        }
    }

    #[test]
    fn lookup_wrong_owner_pid_returns_not_owner() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.lookup(h, 2, Rights::empty()).unwrap_err(), HandleError::NotOwner);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn lookup_correct_owner_succeeds_for_zero_pid_too() {
        let t = fresh_table();
        let h = t.allocate(0, mk_process(0), KObjectType::Process, sig()).unwrap();
        assert!(t.lookup(h, 0, sig()).is_ok());
        close_release(&t, h, 0).unwrap();
    }

    #[test]
    fn lookup_insufficient_rights_returns_no_access() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.lookup(h, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn lookup_superset_rights_request_returns_no_access() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.lookup(h, 1, sigterm()).unwrap_err(), HandleError::NoAccess,);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn lookup_subset_rights_request_succeeds() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
        assert!(t.lookup(h, 1, Rights::SIGNAL).is_ok());
        assert!(t.lookup(h, 1, Rights::TERMINATE).is_ok());
        assert!(t.lookup(h, 1, sigterm()).is_ok());
        assert!(t.lookup(h, 1, Rights::empty()).is_ok());
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn null_handle_lookup_returns_null_handle() {
        let t = fresh_table();
        assert_eq!(t.lookup(RawHandle::NULL, 0, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
    }

    #[test]
    fn out_of_range_segment_returns_invalid_handle() {
        let t = fresh_table();
        let bogus = RawHandle::encode((DIRECTORY_LEN - 1) as u32, 0, 1);
        assert_eq!(t.lookup(bogus, 0, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    }

    #[test]
    fn close_makes_handle_invalid() {
        let t = fresh_table();
        let p = mk_process(1);
        let h = t.allocate(1, p, KObjectType::Process, sig()).unwrap();
        let prev = t.close(h, 1).unwrap();
        assert_eq!(prev.0, p);
        assert_eq!(prev.1, KObjectType::Process);
        // SAFETY: the table's reference, which `close` handed back.
        drop(unsafe { ObjectRef::from_raw(prev.0, prev.1) });
        assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    }

    #[test]
    fn close_rejects_wrong_owner() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.close(h, 2).unwrap_err(), HandleError::NotOwner);
        assert!(t.lookup(h, 1, sig()).is_ok());
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn double_close_returns_invalid_on_second() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        close_release(&t, h, 1).unwrap();
        assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
    }

    #[test]
    fn close_then_allocate_reuses_slot_with_new_generation() {
        let t = fresh_table();
        let h1 = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        let (seg1, slot1, gen1) = h1.decode();
        close_release(&t, h1, 1).unwrap();
        let h2 = t.allocate(1, mk_process(2), KObjectType::Process, sig()).unwrap();
        let (seg2, slot2, gen2) = h2.decode();
        assert_eq!((seg1, slot1), (seg2, slot2), "the freed slot must be the one reused");
        assert_ne!(gen1, gen2, "generation must bump on slot reuse");
        assert_eq!(t.lookup(h1, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
        assert!(t.lookup(h2, 1, sig()).is_ok());
        close_release(&t, h2, 1).unwrap();
    }

    #[test]
    fn generation_wraps_at_max_without_retiring_the_slot() {
        let t = fresh_table();
        let h1 = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        let (seg, slot, _) = h1.decode();

        {
            let _guard = t.inner.lock();
            let entries_ptr = t.directory[seg as usize].load(Ordering::Acquire);
            assert!(!entries_ptr.is_null());
            // SAFETY: segment published once, outlives the table; we hold the
            // rank-3 lock so we are the only writer.
            let entry = unsafe { &(*entries_ptr)[slot as usize] };
            let _wg = WriteGuard::new(entry);
            entry.generation.store(RawHandle::GENERATION_MAX, Ordering::Relaxed);
        }
        let h_max = RawHandle::encode(seg, slot, RawHandle::GENERATION_MAX);
        // A max-generation handle still has bit 63 clear — never a negative isize.
        assert!((h_max.bits() as i64) >= 0, "max-generation handle aliases an error code",);
        assert!(t.lookup(h_max, 1, sig()).is_ok());

        // Reallocate: the slot is recycled, not retired, and wraps
        // `GENERATION_MAX` to 0.
        close_release(&t, h_max, 1).unwrap();
        let h_next = t.allocate(1, mk_process(2), KObjectType::Process, sig()).unwrap();
        let (nseg, nslot, ngen) = h_next.decode();
        assert_eq!((nseg, nslot), (seg, slot), "slot must be recycled, not retired");
        assert_eq!(ngen, 0, "generation wraps from GENERATION_MAX to 0");
        assert!((h_next.bits() as i64) >= 0, "wrapped handle stays non-negative");
        assert_eq!(t.lookup(h_max, 1, sig()).unwrap_err(), HandleError::InvalidHandle,);
        close_release(&t, h_next, 1).unwrap();
    }

    #[test]
    fn restrict_cannot_amplify_rights() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        t.restrict(h, 1, Rights::TERMINATE).unwrap();
        assert_eq!(t.lookup(h, 1, Rights::SIGNAL).unwrap_err(), HandleError::NoAccess,);
        assert!(t.lookup(h, 1, Rights::empty()).is_ok());
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn restrict_drops_rights() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
        t.restrict(h, 1, Rights::SIGNAL).unwrap();
        assert!(t.lookup(h, 1, Rights::SIGNAL).is_ok());
        assert_eq!(t.lookup(h, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess,);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn restrict_rejects_wrong_owner() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.restrict(h, 2, Rights::SIGNAL).unwrap_err(), HandleError::NotOwner);
        close_release(&t, h, 1).unwrap();
    }

    // The entry points validate in different orders, which decides how an input
    // must be built to isolate one guard:
    //
    //   close/restrict:  null → seg|slot range → segment present → generation → owner
    //                    → object non-null
    //   lookup:          null → seg range → segment present → slot range → seqlock snapshot
    //                    → object non-null → refcount → seq re-check → generation → owner
    //                    → rights
    //
    // `lookup` rejects a null object at step 6, before generation and owner, so a
    // closed slot isolates `restrict`'s object-null guard but not `lookup`'s
    // generation guard. For a closed handle and a non-owner caller `lookup`
    // answers `InvalidHandle` and `close`/`restrict` answer `NotOwner`: a
    // divergence from the spec's ordered steps, filed as
    // `TODO(handle-validation-order)` and marked at the two ladders. Each test
    // below isolates its guard; otherwise it passes with that guard deleted.

    /// A slot freed and handed to a new object of the same owner, returning
    /// `(stale, live)`: the only shape where the generation check stands alone.
    fn reused_slot(t: &HandleTable) -> (RawHandle, RawHandle) {
        let stale = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
        let (seg1, slot1, gen1) = stale.decode();
        close_release(t, stale, 1).unwrap();
        let live = t.allocate(1, mk_process(2), KObjectType::Process, sigterm()).unwrap();
        let (seg2, slot2, gen2) = live.decode();
        assert_eq!((seg1, slot1), (seg2, slot2), "the freed slot must be the one reused");
        assert_ne!(gen1, gen2, "reuse must bump the generation");
        (stale, live)
    }

    /// `close`'s generation check alone. After a close the slot is empty, so a
    /// second close in `double_close_returns_invalid_on_second` is rejected by the
    /// null-object check instead. Here the slot is live, and a stale handle that
    /// closed it would destroy an object its holder had no capability for.
    #[test]
    fn close_rejects_a_stale_handle_whose_slot_was_reused() {
        let t = fresh_table();
        let (stale, live) = reused_slot(&t);
        assert_eq!(t.close(stale, 1).unwrap_err(), HandleError::InvalidHandle);
        assert!(t.lookup(live, 1, sigterm()).is_ok(), "the live object was closed by a stale handle");
        close_release(&t, live, 1).unwrap();
    }

    /// The same for `restrict`.
    #[test]
    fn restrict_rejects_a_stale_handle_whose_slot_was_reused() {
        let t = fresh_table();
        let (stale, live) = reused_slot(&t);
        assert_eq!(t.restrict(stale, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle);
        let r = t.lookup(live, 1, sigterm()).expect("a stale restrict stripped the live handle");
        drop(r);
        close_release(&t, live, 1).unwrap();
    }

    /// `restrict`'s null-object check alone: `close` leaves the generation (spec
    /// § "Generation counter behavior") and `owner_pid`, so on a closed slot only
    /// this check can fire.
    #[test]
    fn restrict_rejects_a_handle_whose_object_was_closed() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sigterm()).unwrap();
        close_release(&t, h, 1).unwrap();
        assert_eq!(t.restrict(h, 1, sig()).unwrap_err(), HandleError::InvalidHandle);
    }

    #[test]
    fn the_null_handle_is_rejected_by_lookup_close_and_restrict() {
        let t = fresh_table();
        assert_eq!(t.lookup(RawHandle::NULL, 1, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
        assert_eq!(t.close(RawHandle::NULL, 1).unwrap_err(), HandleError::NullHandle);
        assert_eq!(t.restrict(RawHandle::NULL, 1, Rights::empty()).unwrap_err(), HandleError::NullHandle,);
    }

    /// Segment and slot indices past the end, on all three entry points.
    /// `close`/`restrict` fold them into one `||`, so each half needs an input
    /// that trips only it.
    #[test]
    fn out_of_range_ids_are_rejected_by_lookup_close_and_restrict() {
        let t = fresh_table();
        let bad_seg = RawHandle::encode(DIRECTORY_LEN as u32, 0, 1);
        let bad_slot = RawHandle::encode(0, SEGMENT_LEN as u32, 1);
        for h in [bad_seg, bad_slot] {
            assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
            assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
            assert_eq!(t.restrict(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
        }
    }

    /// A segment in range but never brought online: `try_new` grows only
    /// segment 0, so segment 1 is a null directory slot.
    #[test]
    fn an_unpopulated_segment_is_rejected_by_lookup_close_and_restrict() {
        let t = fresh_table();
        assert_eq!(t.segments_allocated(), 1, "the fixture must leave segment 1 absent");
        let h = RawHandle::encode(1, 0, 1);
        assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
        assert_eq!(t.close(h, 1).unwrap_err(), HandleError::InvalidHandle);
        assert_eq!(t.restrict(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
    }

    #[test]
    fn duplicate_requires_duplicate_right() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.duplicate(h, 1, sig()).unwrap_err(), HandleError::NoAccess,);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn duplicate_yields_independent_handle_with_intersected_rights() {
        let t = fresh_table();
        let original = t
            .allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::TERMINATE | Rights::DUPLICATE)
            .unwrap();
        let copy = t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap();
        assert_ne!(copy, original);
        assert!(t.lookup(copy, 1, Rights::SIGNAL).is_ok());
        assert_eq!(t.lookup(copy, 1, Rights::TERMINATE).unwrap_err(), HandleError::NoAccess,);
        assert!(t.lookup(original, 1, Rights::TERMINATE).is_ok());
        close_release(&t, copy, 1).unwrap();
        assert!(t.lookup(original, 1, Rights::SIGNAL).is_ok());
        close_release(&t, original, 1).unwrap();
    }

    #[test]
    fn duplicate_refcount_accounting_destroys_once_at_last_close() {
        let t = fresh_table();
        test_probe::reset();
        let original = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // object refcount = 1 (one handle)
        let copy = t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // refcount = 2 (two handles)
        close_release(&t, copy, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 0, "destroyed while a handle remains");
        close_release(&t, original, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn duplicate_allocate_error_reclaims_ref() {
        let t = fresh_table();
        test_probe::reset();
        let original = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap(); // refcount = 1
        FAIL_NEXT_ALLOCATE.with(|f| f.set(true));
        assert_eq!(t.duplicate(original, 1, Rights::SIGNAL | Rights::DUPLICATE).unwrap_err(), HandleError::OutOfMemory,);
        assert_eq!(test_probe::process_destroys(), 0);
        assert!(t.lookup(original, 1, Rights::SIGNAL).is_ok());
        close_release(&t, original, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn lookup_holds_ref_until_lookupok_dropped() {
        let t = fresh_table();
        test_probe::reset();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        let ok = t.lookup(h, 1, sig()).unwrap(); // refcount = 2
        close_release(&t, h, 1).unwrap(); // refcount = 1 (the ObjectRef)
        assert_eq!(test_probe::process_destroys(), 0, "destroyed while ObjectRef held");
        assert!(process_magic_ok(&ok.object), "object freed under a held ref");
        drop(ok); // refcount = 0 -> destroy
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn close_does_not_destroy_until_caller_drops_token() {
        let t = fresh_table();
        test_probe::reset();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        let co = t.close(h, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 0);
        // SAFETY: `co` still holds the table's reference, so `close` freed nothing.
        assert!(unsafe { &*(co.0 as *const Process) }.magic_ok());
        // SAFETY: the table's reference, which `close` handed back.
        drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn stat_returns_snapshot_when_inspect_granted() {
        let t = fresh_table();
        let h = t.allocate(42, mk_process(42), KObjectType::Process, Rights::SIGNAL | Rights::INSPECT).unwrap();
        let s = t.stat(h, 42).unwrap();
        assert_eq!(s.object_type, KObjectType::Process);
        assert!(s.rights.contains(Rights::SIGNAL));
        assert_eq!(s.owner_pid, 42);
        let (_, _, generation) = h.decode();
        assert_eq!(s.generation, generation);
        close_release(&t, h, 42).unwrap();
    }

    #[test]
    fn stat_requires_inspect_right() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        assert_eq!(t.stat(h, 1).unwrap_err(), HandleError::NoAccess);
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn segment_grows_when_first_segment_full() {
        let t = fresh_table();
        let mut handles = KVec::<RawHandle>::new();
        handles.try_reserve(SEGMENT_LEN + 1).unwrap();
        for i in 0..SEGMENT_LEN {
            handles.try_push(t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap()).unwrap();
        }
        assert_eq!(t.segments_allocated(), 1);
        let h = t.allocate(1, mk_process(0xFFFF), KObjectType::Process, sig()).unwrap();
        handles.try_push(h).unwrap();
        assert_eq!(t.segments_allocated(), 2);
        let (seg, _, _) = h.decode();
        assert_eq!(seg, 1, "second segment id");
        for i in 0..handles.len() {
            close_release(&t, handles[i], 1).unwrap();
        }
    }

    #[test]
    fn freelist_invariant_count_matches_chain() {
        let t = fresh_table();
        let mut handles = [RawHandle::NULL; 64];
        for (i, h) in handles.iter_mut().enumerate() {
            *h = t.allocate(1, mk_process(i as u32), KObjectType::Process, sig()).unwrap();
        }
        for i in (0..64).step_by(2) {
            close_release(&t, handles[i], 1).unwrap();
        }
        let h_temp = t.allocate(1, mk_process(0xAAAA), KObjectType::Process, sig()).unwrap();
        close_release(&t, h_temp, 1).unwrap();
        let guard = t.inner.lock();
        let free_head = guard.segment_meta[0].free_head;
        let free_count = guard.segment_meta[0].free_count;
        drop(guard);
        let entries_ptr = t.directory[0].load(Ordering::Acquire);
        assert!(!entries_ptr.is_null());
        // SAFETY: segment 0's entries, which the table keeps for its life.
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
        for i in (1..64).step_by(2) {
            close_release(&t, handles[i], 1).unwrap();
        }
    }

    #[test]
    fn failed_acquire_refcount_returns_invalid_handle() {
        let t = fresh_table();
        let h = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        FAIL_NEXT_ACQUIRE.with(|f| f.set(true));
        assert_eq!(t.lookup(h, 1, sig()).unwrap_err(), HandleError::InvalidHandle,);
        assert!(t.lookup(h, 1, sig()).is_ok());
        close_release(&t, h, 1).unwrap();
    }

    #[test]
    fn destructor_dispatches_on_object_type() {
        let t = fresh_table();
        test_probe::reset();
        let hp = t.allocate(1, mk_process(1), KObjectType::Process, sig()).unwrap();
        let ht = t.allocate(1, mk_thread(1, 1), KObjectType::Thread, sig()).unwrap();
        close_release(&t, hp, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 1);
        assert_eq!(test_probe::thread_destroys(), 0, "wrong destructor for Process");
        close_release(&t, ht, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 1);
        assert_eq!(test_probe::thread_destroys(), 1, "Thread destructor did not run");
    }

    /// Each thread owns its own PID space and loops allocate/lookup/close on
    /// real objects. Cross-pid lookups always fail; at the end no handles remain
    /// and every object was destroyed exactly once.
    #[test]
    fn concurrent_allocate_lookup_close_pid_isolation() {
        use std::sync::Arc;
        use std::thread;

        let t = Arc::new(fresh_table());
        const N_THREADS: usize = 8;
        const ITERS: usize = 2000;

        let workers: Vec<_> = (0..N_THREADS)
            .map(|tid| {
                let t = Arc::clone(&t);
                let my_pid = (tid as u32) + 1;
                thread::spawn(move || {
                    test_probe::reset();
                    for i in 0..ITERS {
                        let obj = mk_process(my_pid * 1_000_000 + i as u32);
                        let h = t.allocate(my_pid, obj, KObjectType::Process, sig()).expect("allocate");
                        let ok = t.lookup(h, my_pid, sig()).expect("lookup");
                        assert!(process_magic_ok(&ok.object));
                        drop(ok);
                        let other_pid = if my_pid == 1 { 2 } else { 1 };
                        assert_eq!(t.lookup(h, other_pid, sig()).unwrap_err(), HandleError::NotOwner,);
                        close_release(&t, h, my_pid).expect("close");
                    }
                    test_probe::process_destroys()
                })
            })
            .collect();
        let mut total_destroys = 0usize;
        for w in workers {
            total_destroys += w.join().expect("join");
        }
        assert_eq!(total_destroys, N_THREADS * ITERS, "every object destroyed once");
        let h = t.allocate(99, mk_process(99), KObjectType::Process, sig()).unwrap();
        close_release(&t, h, 99).unwrap();
        assert!(t.allocated_count() <= 1, "stray handles after stress: {}", t.allocated_count());
    }

    /// One writer closes and reallocates a real `Process` in one slot while
    /// readers look it up. A `LookupOk` pins the object, so its sentinel and owner
    /// must be consistent: the seqlock catches torn reads, the refcount keeps the
    /// object alive.
    #[test]
    fn concurrent_torn_read_torture() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let t = Arc::new(fresh_table());
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                let mut pid = 1u32;
                let mut cycles_since_yield = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    let obj = mk_process(pid);
                    if let Ok(h) = t.allocate(pid, obj, KObjectType::Process, sig()) {
                        let _ = close_release(&t, h, pid);
                    } else {
                        // SAFETY: adopts the object's creation reference.
                        drop(unsafe { ObjectRef::from_raw(obj, KObjectType::Process) });
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
                                if let Ok(ok) = t.lookup(h, pid, sig()) {
                                    // SAFETY: `ok.object` pins the object, and this test stores only `Process`es.
                                    let p = unsafe { &*(ok.object.as_ptr() as *const Process) };
                                    assert!(p.magic_ok(), "torn/UAF read: bad magic");
                                    assert_eq!(p.pid(), pid, "object pid != owner pid");
                                }
                            }
                        }
                    }
                    iterations += 1;
                    thread::yield_now();
                }
            }));
        }

        std::thread::sleep(std::time::Duration::from_millis(30));
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        for r in readers {
            r.join().unwrap();
        }
    }

    /// **Headline TOCTOU test.** Workers hammer `duplicate(H)` on one long-lived
    /// handle, closing each duplicate, while others `lookup(H)` and verify the
    /// pinned object. `H` pins it throughout, so only the final close destroys it;
    /// a use-after-free corrupts the sentinel, a refcount bug destroys early or
    /// leaks.
    #[test]
    fn concurrent_duplicate_vs_close_toctou_torture() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        let t = Arc::new(fresh_table());
        test_probe::reset();
        let h0 = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL | Rights::DUPLICATE).unwrap();
        let stop = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::new();
        for _ in 0..3 {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            workers.push(thread::spawn(move || {
                let src = h0;
                test_probe::reset();
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(dup) = t.duplicate(src, 1, Rights::SIGNAL | Rights::DUPLICATE) {
                        if let Ok(ok) = t.lookup(dup, 1, Rights::SIGNAL) {
                            assert!(process_magic_ok(&ok.object), "UAF via duplicate");
                            drop(ok);
                        }
                        let _ = close_release(&t, dup, 1);
                    }
                }
                test_probe::process_destroys()
            }));
        }
        for _ in 0..2 {
            let t = Arc::clone(&t);
            let stop = Arc::clone(&stop);
            workers.push(thread::spawn(move || {
                let src = h0;
                test_probe::reset();
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(ok) = t.lookup(src, 1, Rights::SIGNAL) {
                        assert!(process_magic_ok(&ok.object), "UAF via lookup");
                        drop(ok);
                    }
                }
                test_probe::process_destroys()
            }));
        }

        std::thread::sleep(std::time::Duration::from_millis(40));
        stop.store(true, Ordering::Relaxed);
        let mut worker_destroys = 0usize;
        for w in workers {
            worker_destroys += w.join().unwrap();
        }
        assert_eq!(worker_destroys, 0, "object destroyed while H was held");
        let ok = t.lookup(h0, 1, Rights::SIGNAL).unwrap();
        assert!(process_magic_ok(&ok.object));
        drop(ok);
        test_probe::reset();
        close_release(&t, h0, 1).unwrap();
        assert_eq!(test_probe::process_destroys(), 1, "final destroy did not happen exactly once");
    }

    #[test]
    fn close_then_allocate_drains_immediately_on_single_context() {
        let t = fresh_table();
        for i in 0..1024 {
            let h = t.allocate(1, mk_process(i), KObjectType::Process, sig()).unwrap();
            close_release(&t, h, 1).unwrap();
        }
        assert_eq!(t.allocated_count(), 0);
        let h = t.allocate(1, mk_process(0xFEED), KObjectType::Process, sig()).unwrap();
        close_release(&t, h, 1).unwrap();
    }
}
