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
use super::grace::{GraceTracker, yield_for_grace};
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

#[cfg(test)]
std::thread_local! {
    /// A test's stand-in for another CPU, run once between a lookup's reading the entry's object
    /// and its taking a reference.
    pub(crate) static BEFORE_ACQUIRE: core::cell::Cell<Option<std::boxed::Box<dyn FnOnce()>>> =
        const { core::cell::Cell::new(None) };
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
/// (normally `ObjectRef::from_raw(co.0, co.1)`, then drop), once `close` has waited out every
/// lookup that read it. Leaving the decrement to the caller keeps destruction, which reaches
/// the rank-6 allocator, out from under the rank-3 lock. The
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
                            entry.in_transit.store(0, Ordering::Relaxed);
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
            // Steps 5 to 7 are a read section, which a close waits out before releasing. Releases
            // come after it: a last one runs a destructor, which may close.
            let (snap, obj, object_type) = {
                let _read_guard = self.grace.enter_read(current_ctx_id());

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

                #[cfg(test)]
                if let Some(other_cpu) = BEFORE_ACQUIRE.take() {
                    other_cpu();
                }

                // Step 7: try to bump the refcount (fails if it is already zero,
                // i.e. the object is being torn down). The reference is adopted at
                // step 12 or released on the retry/error paths below.
                // SAFETY: `obj` was read from a live entry in a read section, which a close waits
                // out, so it addresses a live object.
                if !unsafe { try_acquire_refcount(obj, object_type) } {
                    return Err(HandleError::InvalidHandle);
                }
                (snap, obj, object_type)
            };

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

    /// Look up `h` as [`lookup`](Self::lookup) does, and claim it to be moved: until the [`Claim`]
    /// commits or drops, another claim of it fails with `InvalidHandle`. Two moves of one handle
    /// at once would otherwise both pass their lookups, a copy without `DUPLICATE`.
    pub fn claim(&self, h: RawHandle, caller_pid: u32, required: Rights) -> Result<(LookupOk, Claim<'_>), HandleError> {
        let ok = self.lookup(h, caller_pid, required)?;
        let claimed = {
            let _guard = self.inner.lock();
            // Closed, reused or claimed since the lookup: the handle is leaving, or has left.
            let entry = self.entry_of(h).filter(|e| {
                e.object.load(Ordering::Relaxed) == ok.object.as_ptr() && e.in_transit.load(Ordering::Relaxed) == 0
            });
            entry.inspect(|e| e.in_transit.store(1, Ordering::Relaxed)).is_some()
        };
        if !claimed {
            return Err(HandleError::InvalidHandle);
        }
        Ok((ok, Claim { table: self, handle: h, pid: caller_pid }))
    }

    /// `h`'s entry if its generation is still `h`'s. Called with the rank-3 lock held.
    fn entry_of(&self, h: RawHandle) -> Option<&super::entry::HandleEntry> {
        let (seg_id, slot_id, generation) = h.decode();
        let entries = self.directory.get(seg_id as usize)?.load(Ordering::Relaxed);
        if entries.is_null() || slot_id as usize >= SEGMENT_LEN {
            return None;
        }
        // SAFETY: as in `close`; segments are published once and outlive the table.
        let entry = unsafe { &(*entries)[slot_id as usize] };
        (entry.generation.load(Ordering::Relaxed) == generation).then_some(entry)
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
        drop(guard);
        self.grace.synchronize();

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
        let (n, more) = self.unpublish_owned_batch(pid, cursor, out);
        if n > 0 {
            self.grace.synchronize();
        }
        (n, more)
    }

    fn unpublish_owned_batch(
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

/// A handle being moved out of its owner's table, from [`HandleTable::claim`].
pub struct Claim<'a> {
    table: &'a HandleTable,
    handle: RawHandle,
    pid: u32,
}

impl Claim<'_> {
    /// The move has committed: close the handle and release its reference. One its owner closed
    /// meanwhile has nothing left to close.
    pub fn commit(self) {
        let this = core::mem::ManuallyDrop::new(self);
        if let Ok(co) = this.table.close(this.handle, this.pid) {
            // SAFETY: `close` transferred the handle's one reference into `co`.
            drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        }
    }
}

/// The move failed: the handle stays with its owner.
impl Drop for Claim<'_> {
    fn drop(&mut self) {
        let _guard = self.table.inner.lock();
        if let Some(e) = self.table.entry_of(self.handle) {
            e.in_transit.store(0, Ordering::Relaxed);
        }
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
mod tests;
