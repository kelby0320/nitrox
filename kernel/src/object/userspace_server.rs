//! The [`UserspaceServerReg`] kernel object — the kernel's registration record
//! for a **Userspace Server** (the other kind of resource server is a Kernel Server, in
//! [`crate::object::kernel_server`]).
//!
//! A [`BindingTarget::UserspaceServer`](crate::object::namespace::BindingTarget) binding holds
//! an `ObjectRef` to one. It owns the kernel's private end of an IPC channel (an
//! [`IpcChannel`] whose *peer* the server process services) and the **pending tables**
//! correlating in-flight forwarded requests, by `request_id`, with the replies that complete
//! them. See `docs/architecture/namespace-and-resource-servers.md` § "Userspace Servers" and
//! `docs/spec/rsproto-namespace-ops.md`.
//!
//! `sys_ns_lookup` forwards a resolve via [`crate::sched::us_forward_originate`], leaving the
//! lookup's `PendingOperation` **uncompleted**; the server's reply is drained inline in its
//! send syscall (the peer is this endpoint, found by the `IpcChannel` back-pointer), which
//! takes the matching entry and completes the PO.
//!
//! ## Mutation discipline
//!
//! As for [`IpcChannel`] / [`Timer`](crate::object::Timer): the pending tables live in a
//! [`SchedCell`], borrowed only with the token the rank-1 `SCHED` lock keeps; the owned `endpoint`
//! is set once at construction, and the file cache has a lock of its own. The endpoint and the pending `po`s
//! are dropped only when the registration is destroyed (`dispatch_destroy`, **outside** `SCHED`) or
//! by the caller of a `take_pending_*`, never under the lock.
//!
//! One endpoint is shared by *many* bindings (`docs/architecture/namespace-and-resource-servers.md`
//! § Subtree scoping), so up to [`US_PENDING_MAX`] lookups and, independently, fills can be in
//! flight; a full table fails a new request `WouldBlock`.

use crate::libkern::handle::Rights;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::object::header::{Header, Ref, Shared, Weak, kobject};
use crate::object::{FileObject, Forgotten, IpcChannel, PendingOperation};
use crate::sched::{SchedCell, SchedToken};
use crate::syscall::error::KError;

/// Largest lookup suffix a [`PendingLookup`] stores inline (no allocation under `SCHED`). A
/// longer one keeps its true `suffix_len` but a truncated buffer: harmless for an eager
/// `MEMOBJ` reply, but a `FILE` reply for it fails `TooLarge`.
pub const LOOKUP_SUFFIX_MAX: usize = 256;

/// **What a forwarded resolve asks its server to do**, kept with the pending lookup so a
/// `SUBNAMESPACE` reply can send the same request on into another namespace. The syscall that
/// started it is gone by then, and with it its user pointers; these are values.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ForwardOp {
    /// A plain lookup.
    Plain,
    /// A size change first (`RESOLVE_OP_CREATE` / `_GROW` / `_TRUNCATE`).
    Size(u32, crate::rsproto::SizeChange),
    /// A rename to the lookup's `dest`, with its `RENAME_*` flags.
    Rename {
        /// The rename's flags.
        flags: u16,
    },
}

/// A forwarded resolve's request, beyond its suffix: the operation, a rename's mount-relative
/// destination, and how many `SUBNAMESPACE` replies it has already followed.
#[derive(Copy, Clone)]
pub struct Forward<'a> {
    pub op: ForwardOp,
    pub dest: &'a [u8],
    pub depth: u8,
}

impl Forward<'_> {
    /// A plain lookup's: no destination, never continued.
    pub const PLAIN: Forward<'static> = Forward { op: ForwardOp::Plain, dest: b"", depth: 0 };
}

/// One outstanding forwarded lookup: what completing its `PendingOperation` needs when the
/// reply arrives. Moved out by `take_pending_matching` / `take_outstanding`; the caller drops
/// its `po` **outside** `SCHED`.
pub struct PendingLookup {
    /// The `request_id` stamped on the Resolve request; the reply must echo it.
    pub(crate) request_id: u64,
    /// The lookup's `PendingOperation` (a clone of the client's), pinned so the reply can
    /// complete it even if the client closed its handle.
    pub(crate) po: Ref<PendingOperation>,
    /// The `Rights` the lookup requested; the installed handle gets `requested ∩ granted`.
    pub(crate) requested: Rights,
    /// The path suffix, inline (`suffix[..suffix_len.min(MAX)]`); `suffix_len` is the *true*
    /// length. A `FILE` reply names its file with it; an eager `MEMOBJ` reply ignores it.
    pub(crate) suffix: [u8; LOOKUP_SUFFIX_MAX],
    pub(crate) suffix_len: u16,
    /// What the request asked for, so a `SUBNAMESPACE` reply can ask it again elsewhere.
    pub(crate) op: ForwardOp,
    /// A rename's destination as sent, inline like `suffix`, with its true length.
    pub(crate) dest: [u8; LOOKUP_SUFFIX_MAX],
    pub(crate) dest_len: u16,
    /// How many `SUBNAMESPACE` replies this lookup has already followed.
    pub(crate) depth: u8,
}

impl PendingLookup {
    /// A rename's stored destination, or `None` if it overran [`LOOKUP_SUFFIX_MAX`].
    pub(crate) fn dest(&self) -> Option<&[u8]> {
        let n = self.dest_len as usize;
        if n > LOOKUP_SUFFIX_MAX { None } else { Some(&self.dest[..n]) }
    }

    /// The stored suffix bytes, or `None` if the true length overran [`LOOKUP_SUFFIX_MAX`] (the
    /// path is then unrecoverable).
    pub(crate) fn suffix(&self) -> Option<&[u8]> {
        let n = self.suffix_len as usize;
        if n > LOOKUP_SUFFIX_MAX { None } else { Some(&self.suffix[..n]) }
    }
}

/// One outstanding forwarded **page-cache fill** (`File::ReadRange`): the faulting thread is
/// parked on `po`, and the reply copies the bytes into the page, marks it ready and completes
/// `po`. The caller drops `po` / `file_obj` **outside** `SCHED`.
pub struct PendingFill {
    /// The `request_id` stamped on the `ReadRange` request; the reply echoes it.
    pub(crate) request_id: u64,
    /// The fill's `PendingOperation` (the faulting thread blocks on it).
    pub(crate) po: Ref<PendingOperation>,
    /// The `FileObject` being filled (pins it; the reply marks its page ready).
    pub(crate) file_obj: Ref<FileObject>,
    /// The page index within the file.
    pub(crate) index: usize,
}

/// Concurrent outstanding forwarded requests a registration tracks, for lookups and
/// (independently) fills. One endpoint is shared by *many* bindings, so several can be in
/// flight; a full table fails a new request `WouldBlock` (the caller retries).
pub const US_PENDING_MAX: usize = 8;

struct Inner {
    /// Outstanding forwarded lookups, by `request_id`; a free slot is `None`.
    pending: [Option<PendingLookup>; US_PENDING_MAX],
    /// Outstanding forwarded fills; separate from `pending`, routed by reply op.
    pending_fill: [Option<PendingFill>; US_PENDING_MAX],
    /// Monotonic `request_id` stamp (shared across lookups and fills).
    next_id: u64,
}

/// One file in a registration's cache: the id its server gave it, and its `FileObject`.
struct CachedFile {
    id: u64,
    /// The object, **weakly**: a lookup must upgrade it. The object's closing takes its entry out,
    /// under the files lock, by address, so an entry never outlives the memory it names.
    obj: Weak<FileObject>,
    /// **Its server forgot it with I/O in flight** (`File::Forget`): the entry stays so a second
    /// `Forget` finds the object and waits on the first one's answer. No resolve or sync sees
    /// it, and it goes with the object.
    forgotten: bool,
}

/// The kernel's registration record for one Userspace Server (`#[repr(C)]`, header first).
#[repr(C)]
pub struct UserspaceServerReg {
    header: Header<Self>,
    magic: u64,
    /// The kernel's IPC endpoint; its peer is the endpoint the server services. Set once, so read
    /// without `SCHED`.
    endpoint: Ref<IpcChannel>,
    inner: SchedCell<Inner>,
    /// **The file cache: one `FileObject` per file this server's replies named**, keyed by the
    /// id a `FILE_BLOCKS` reply carries, so every resolve shares one object and a sync or unmount
    /// can enumerate them. An index only: users hold the objects, or they hold themselves while
    /// dirty.
    ///
    /// Its own lock, not `SCHED`, since the objects it hands out are cloned and dropped: ranked
    /// [`Registry`](LockRank::Registry), above the page caches it indexes (it allocates while
    /// held), and never taken inside a `FileObject`'s lock. **No reference is ever dropped
    /// under it**, since the last one would re-enter it from `FileObject::leave_cache`.
    files: SpinLock<KVec<CachedFile>>,
    /// Ranked as `files` is, as it allocates while held; nothing is dropped under it.
    ops: SpinLock<Budget>,
}

/// **How many file operations one mount runs at once**: a stuck device holds up its mount alone.
pub const FILE_OPS_MAX: u32 = 16;

/// **How many of a mount's file operations one process may have queued or running.**
pub const FILE_OPS_PER_PROCESS: u32 = 4;

/// **What a file operation does.**
pub enum FileWork {
    /// Write back every dirty file of the mount.
    FlushMount,
    /// Write back one file's pages.
    Flush(Ref<FileObject>),
}

/// A file operation for process `pid`, answered through `po`.
pub struct QueuedOp {
    /// What it does.
    pub work: FileWork,
    /// The operation it completes.
    pub po: Ref<PendingOperation>,
    /// The process that asked.
    pub pid: u32,
}

struct Budget {
    running: u32,
    waiting: KVec<QueuedOp>,
    by_process: KVec<(u32, u32)>,
}

kobject!(UserspaceServerReg, closing = crate::sched::us_reg_detach);
// SAFETY: a registration changes once shared only in `inner`, a `SchedCell`, and behind the locks
// of `files` and `ops`.
unsafe impl Shared for UserspaceServerReg {}

impl UserspaceServerReg {
    /// Sentinel written into [`UserspaceServerReg::magic`] at construction.
    pub const MAGIC: u64 = 0x55_73_53_72_76_52_67_21; // "UsSrvRg!"

    /// Allocate a registration owning `endpoint` (the kernel's IPC endpoint), refcount one, with
    /// empty pending tables. The caller installs the back-pointer from `endpoint` to this
    /// object (under `SCHED`) and binds it as a `UserspaceServer` target.
    pub fn try_new(endpoint: Ref<IpcChannel>) -> Result<Ref<Self>, AllocError> {
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            endpoint,
            inner: SchedCell::new(Inner {
                pending: core::array::from_fn(|_| None),
                pending_fill: core::array::from_fn(|_| None),
                next_id: 1,
            }),
            files: SpinLock::new(LockRank::Registry, KVec::new()),
            ops: SpinLock::new(
                LockRank::Registry,
                Budget { running: 0, waiting: KVec::new(), by_process: KVec::new() },
            ),
        })
        .map(Ref::new)
    }

    /// **Admit `op` to this mount's budget**: `Some` to run now, `None` queued, or `WouldBlock`.
    pub fn admit_op(&self, op: QueuedOp) -> Result<Option<QueuedOp>, (KError, QueuedOp)> {
        let mut g = self.ops.lock();
        let at = g.by_process.iter().position(|&(pid, _)| pid == op.pid);
        if at.is_some_and(|i| g.by_process[i].1 >= FILE_OPS_PER_PROCESS) {
            return Err((KError::WouldBlock, op));
        }
        let run = g.running < FILE_OPS_MAX && g.waiting.is_empty();
        let room = run || g.waiting.try_reserve(1).is_ok();
        let counted = match at {
            Some(i) => {
                g.by_process[i].1 += 1;
                true
            }
            None => room && g.by_process.try_push((op.pid, 1)).is_ok(),
        };
        if !room || !counted {
            if counted {
                Self::uncount(&mut g, op.pid);
            }
            return Err((KError::OutOfMemory, op));
        }
        if run {
            g.running += 1;
            return Ok(Some(op));
        }
        g.waiting.push_within_capacity(op).map(|()| None).map_err(|f| (KError::OutOfMemory, f))
    }

    /// **An operation of `pid`'s is done**: its place passed to the oldest waiting, or given back.
    pub fn op_done(&self, pid: u32) -> Option<QueuedOp> {
        let mut g = self.ops.lock();
        Self::uncount(&mut g, pid);
        if g.waiting.is_empty() {
            g.running = g.running.saturating_sub(1);
            return None;
        }
        Some(g.waiting.remove(0))
    }

    fn uncount(g: &mut Budget, pid: u32) {
        if let Some(i) = g.by_process.iter().position(|&(p, _)| p == pid) {
            g.by_process[i].1 -= 1;
            if g.by_process[i].1 == 0 {
                g.by_process.remove(i);
            }
        }
    }

    // --- The file cache (its own lock; never under `SCHED`) --------------

    /// **The one object for `obj`'s file, or `obj` becomes it.** A live cached object is returned
    /// as a new reference, `obj` left for the caller to drop outside this lock; otherwise `obj` is
    /// entered under its id (replacing a mid-teardown entry) and `Ok(None)` returned. An object
    /// this registration does not cache, by its producer, is not entered and also answers
    /// `Ok(None)`. `Err` only if the index cannot grow. A forgotten entry is passed by: its file
    /// is freed or about to be.
    pub fn cache_get_or_insert(&self, obj: &Ref<FileObject>) -> Result<Option<Ref<FileObject>>, AllocError> {
        let Some((_, id)) = obj.cache_key().filter(|&(owner, _)| core::ptr::eq(owner, self)) else {
            return Ok(None);
        };
        let mut g = self.files.lock();
        // SAFETY: kept behind the files lock, which `obj`'s closing takes to remove its entry, by
        // address, before the object is freed: its producer names this registration.
        let weak = || unsafe { Weak::new(obj.as_ptr()) };
        if let Some(e) = g.iter_mut().find(|e| e.id == id && !e.forgotten) {
            if let Some(existing) = e.obj.upgrade() {
                return Ok(Some(existing));
            }
            // Mid-teardown (refcount zero): its closing will look for its own address, which
            // this overwrites, so it removes nothing.
            e.obj = weak();
            return Ok(None);
        }
        g.try_push(CachedFile { id, obj: weak(), forgotten: false })?;
        Ok(None)
    }

    /// **`File::Forget` of file `id`**, with `answer` the PO its server waits on if it has to.
    ///
    /// A live entry is the file the server means. Failing that, the entry an earlier `Forget`
    /// left because I/O of the file was in flight: this is a second `Forget` while the first
    /// waits, and [`FileObject::forget`] hands it the first one's PO. The entry is marked
    /// forgotten, so no later resolve of the id (which after a free may name a new file) and
    /// no sync finds the object; it goes now if nothing is in flight, else with the object.
    /// Taking the entry out on every `Forget` would answer a second one at once, with the
    /// first one's IRP still in flight. [`Forgotten::Now`] if the id is not cached.
    pub fn forget_file(&self, id: u64, answer: &Ref<PendingOperation>) -> Forgotten {
        let obj = {
            let mut g = self.files.lock();
            let Some(i) =
                g.iter().position(|e| e.id == id && !e.forgotten).or_else(|| g.iter().rposition(|e| e.id == id))
            else {
                return Forgotten::Now;
            };
            match g[i].obj.upgrade() {
                Some(obj) => {
                    g[i].forgotten = true;
                    obj
                }
                // Mid-teardown, so nothing of it is in flight: an IRP pins its object.
                None => {
                    g.remove(i);
                    return Forgotten::Now;
                }
            }
        };
        // Outside the files lock: the object's own lock ranks below it.
        let outcome = obj.forget(answer);
        if matches!(outcome, Forgotten::Now) {
            self.cache_forget_object(obj.as_ptr());
        }
        // Dropped here, outside the lock, since it may be the last reference.
        drop(obj);
        outcome
    }

    /// Take the entry naming `obj` out of the cache, if one does — a `FileObject`'s closing.
    pub fn cache_forget_object(&self, obj: *const FileObject) {
        let mut g = self.files.lock();
        if let Some(i) = g.iter().position(|e| e.obj.names(obj)) {
            g.remove(i);
        }
    }

    /// Every live cached object, each a new reference the caller drops — outside this lock,
    /// which it is by the time this returns. Forgotten ones are left out: nothing of theirs is
    /// written again. `Err` if the list cannot be built.
    pub fn cache_objects(&self) -> Result<KVec<Ref<FileObject>>, AllocError> {
        let mut out = KVec::new();
        let g = self.files.lock();
        out.try_reserve(g.len())?;
        for e in g.iter().filter(|e| !e.forgotten) {
            if let Some(r) = e.obj.upgrade() {
                // `try_reserve` above guarantees this push does not allocate.
                let Ok(()) = out.push_within_capacity(r) else { unreachable!("reserved above") };
            }
        }
        Ok(out)
    }

    /// How many files the cache holds. Test/observability only.
    #[cfg(test)]
    pub(crate) fn cached_files(&self) -> usize {
        self.files.lock().len()
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// The kernel's endpoint, borrowed: a forwarded request is sent on it and lands in the
    /// *peer*'s inbox.
    pub fn endpoint(&self) -> &IpcChannel {
        &self.endpoint
    }

    /// A plain lookup's [`begin_forward`](Self::begin_forward), the tests' shorthand.
    #[cfg(test)]
    pub(crate) fn begin(
        &self,
        tok: &mut SchedToken,
        po: &Ref<PendingOperation>,
        requested: Rights,
        suffix: &[u8],
    ) -> Option<u64> {
        self.begin_forward(tok, po, requested, suffix, Forward::PLAIN)
    }

    /// Reserve a free pending-lookup slot, returning its new `request_id`; `None` if all
    /// [`US_PENDING_MAX`] are in flight (the caller fails the lookup `WouldBlock`).
    ///
    /// Stores a clone of `po` (an atomic bump, sound under `SCHED`) so the reply can complete
    /// it, and `fwd`, what the request asked for, so a `SUBNAMESPACE` reply can send it on.
    pub(crate) fn begin_forward(
        &self,
        tok: &mut SchedToken,
        po: &Ref<PendingOperation>,
        requested: Rights,
        suffix: &[u8],
        fwd: Forward<'_>,
    ) -> Option<u64> {
        let inner = self.inner.get(tok);
        let slot = inner.pending.iter().position(Option::is_none)?;
        let request_id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        // A memcpy, no allocation (sound under `SCHED`); the *true* length is recorded so an
        // overrun is detectable later.
        let mut sbuf = [0u8; LOOKUP_SUFFIX_MAX];
        let n = suffix.len().min(LOOKUP_SUFFIX_MAX);
        sbuf[..n].copy_from_slice(&suffix[..n]);
        let mut dbuf = [0u8; LOOKUP_SUFFIX_MAX];
        let d = fwd.dest.len().min(LOOKUP_SUFFIX_MAX);
        dbuf[..d].copy_from_slice(&fwd.dest[..d]);
        inner.pending[slot] = Some(PendingLookup {
            request_id,
            po: po.clone(),
            requested,
            suffix: sbuf,
            suffix_len: suffix.len() as u16,
            op: fwd.op,
            dest: dbuf,
            dest_len: fwd.dest.len() as u16,
            depth: fwd.depth,
        });
        Some(request_id)
    }

    /// Reserve a free pending-**fill** slot for a forwarded `File::ReadRange`, returning its
    /// `request_id`; `None` if all [`US_PENDING_MAX`] are in flight. Stores clones of `po` and
    /// `file_obj` so the reply can land the page even if the faulting thread's references changed.
    pub(crate) fn begin_fill(
        &self,
        tok: &mut SchedToken,
        po: &Ref<PendingOperation>,
        file_obj: &Ref<FileObject>,
        index: usize,
    ) -> Option<u64> {
        let inner = self.inner.get(tok);
        let slot = inner.pending_fill.iter().position(Option::is_none)?;
        let request_id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        inner.pending_fill[slot] = Some(PendingFill { request_id, po: po.clone(), file_obj: file_obj.clone(), index });
        Some(request_id)
    }

    /// Take the pending fill **iff** a slot's `request_id` matches the `ReadRange` reply's
    /// echoed id; `None` otherwise. The caller drops `po` / `file_obj` **outside** `SCHED`.
    pub(crate) fn take_pending_fill_matching(&self, tok: &mut SchedToken, request_id: u64) -> Option<PendingFill> {
        let inner = self.inner.get(tok);
        let slot = inner.pending_fill.iter().position(|s| matches!(s, Some(p) if p.request_id == request_id))?;
        inner.pending_fill[slot].take()
    }

    /// Take the pending lookup **iff** a slot's `request_id` matches the reply's echoed id;
    /// `None` for a duplicate or stale reply. The caller drops `po` **outside** `SCHED`.
    pub(crate) fn take_pending_matching(&self, tok: &mut SchedToken, request_id: u64) -> Option<PendingLookup> {
        let inner = self.inner.get(tok);
        let slot = inner.pending.iter().position(|s| matches!(s, Some(p) if p.request_id == request_id))?;
        inner.pending[slot].take()
    }

    /// Take everything outstanding, for **failing** it when the server or the registration goes:
    /// the caller completes each and drops its references **outside** `SCHED`. Only the references
    /// come out; the rest of each entry is plain data, dropped here.
    pub(crate) fn take_outstanding(&self, tok: &mut SchedToken) -> Outstanding {
        let inner = self.inner.get(tok);
        Outstanding {
            lookups: core::array::from_fn(|i| inner.pending[i].take().map(|pl| pl.po)),
            fills: core::array::from_fn(|i| inner.pending_fill[i].take().map(|pf| (pf.po, pf.file_obj))),
        }
    }
}

impl Drop for UserspaceServerReg {
    /// Its closing, run first, failed and took out everything outstanding.
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        let none = inner.pending.iter().all(Option::is_none) && inner.pending_fill.iter().all(Option::is_none);
        debug_assert!(none, "UserspaceServerReg dropped with requests outstanding");
    }
}

/// What a registration had outstanding, taken whole: every forwarded lookup's PO, and every
/// fill's PO and file.
pub(crate) struct Outstanding {
    pub(crate) lookups: [Option<Ref<PendingOperation>>; US_PENDING_MAX],
    pub(crate) fills: [Option<(Ref<PendingOperation>, Ref<FileObject>)>; US_PENDING_MAX],
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;
    use crate::object::{IpcChannel, PendingOperation};

    /// A live `IpcChannel` endpoint adopted into a `Ref`; its peer is dropped at once.
    fn endpoint() -> Ref<IpcChannel> {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        drop(b);
        a
    }

    fn make_po() -> Ref<PendingOperation> {
        Ref::new(PendingOperation::try_new().unwrap())
    }

    fn reg() -> Ref<UserspaceServerReg> {
        let r = UserspaceServerReg::try_new(endpoint()).unwrap();
        assert!(r.magic_ok());
        r
    }

    /// An endpoint can outlive its registration, and its `us_reg` back-pointer is uncounted, so the
    /// registration's closing must clear it: without that, four consumers dereferenced freed
    /// memory (`#GP`/`#PF` in `signal_pending_op`, about one boot in four under load).
    #[test]
    fn dropping_a_registration_clears_its_endpoints_back_pointer() {
        init_global_heap();
        let tok = SchedToken::for_test();
        // Held apart from the registration, as a server handle or a parked waiter holds it.
        let ep = endpoint();
        let reg = UserspaceServerReg::try_new(ep.clone()).unwrap();
        assert!(crate::sched::us_server_attach(&reg).is_none());
        assert_eq!(ep.us_reg_addr(&tok), reg.as_ptr().cast_const());
        drop(reg); // the last registration reference; the endpoint survives
        assert!(
            ep.us_reg_addr(&tok).is_null(),
            "a dying registration must clear its endpoint's back-pointer, or every \
             consumer of it is reading freed memory"
        );
    }

    /// A reply holds the registration it was routed to: the last binding, dropped on another CPU
    /// between the lookup and the reply's completion, cannot free it under the reply.
    #[test]
    fn a_reply_holds_the_registration_it_found() {
        init_global_heap();
        let (k, s_ref) = IpcChannel::try_new_pair(4).unwrap();
        let binding = UserspaceServerReg::try_new(k).unwrap();
        assert!(crate::sched::us_server_attach(&binding).is_none());

        let reply = crate::sched::us_forward_reg_for_send(&s_ref).expect("routed to the registration");
        let destroyed = test_probe::userspace_server_reg_destroys();
        drop(binding);
        assert_eq!(test_probe::userspace_server_reg_destroys(), destroyed, "freed under the reply");
        assert!(crate::sched::us_forward_take_pending(&reply, 1).is_none());

        drop(reply);
        assert_eq!(test_probe::userspace_server_reg_destroys(), destroyed + 1);
        assert!(crate::sched::us_forward_reg_for_send(&s_ref).is_none());
        drop(s_ref);
    }

    /// Only a pointer that still names *this* registration may be cleared:
    /// `us_server_attach` can retarget an endpoint, and a dying predecessor must not
    /// stomp its live successor's reply routing.
    #[test]
    fn a_dying_registration_leaves_a_successors_back_pointer_alone() {
        init_global_heap();
        let mut tok = SchedToken::for_test();
        let ep = endpoint();
        let old = UserspaceServerReg::try_new(ep.clone()).unwrap();
        assert!(crate::sched::us_server_attach(&old).is_none());
        let successor = UserspaceServerReg::try_new(ep.clone()).unwrap();
        // SAFETY: kept in the endpoint's cell, and the successor's closing clears it there.
        ep.set_us_reg(&mut tok, unsafe { Weak::new(successor.as_ptr()) });
        drop(old);
        assert_eq!(
            ep.us_reg_addr(&tok),
            successor.as_ptr().cast_const(),
            "the predecessor cleared a pointer that no longer named it"
        );
        drop(successor);
        assert!(ep.us_reg_addr(&tok).is_null(), "and the successor's own closing clears it");
    }

    #[test]
    fn begin_assigns_monotonic_ids_and_caps_at_capacity() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        for expect in 1..=US_PENDING_MAX as u64 {
            let id = r.begin(tok, &po, Rights::MAP_READ, b"sys/gen");
            assert_eq!(id, Some(expect));
        }
        assert_eq!(r.begin(tok, &po, Rights::MAP_READ, b"sys/gen"), None);
        let taken = r.take_pending_matching(tok, 1);
        assert_eq!(taken.as_ref().map(|p| p.request_id), Some(1));
        drop(taken);
        let id = r.begin(tok, &po, Rights::MAP_READ, b"sys/gen");
        assert_eq!(id, Some(US_PENDING_MAX as u64 + 1));
        drop(r.take_outstanding(tok));
        drop(po);
        drop(r);
    }

    /// **A pending lookup keeps what it asked for**: the operation, a rename's destination and
    /// the depth come back with it. A destination longer than the inline buffer reads as absent
    /// rather than as a truncated path, which would continue somewhere else.
    #[test]
    fn a_pending_lookup_keeps_its_operation_for_a_continuation() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        let fwd = Forward { op: ForwardOp::Rename { flags: 1 }, dest: b"fs/l1/b", depth: 2 };
        let id = r.begin_forward(tok, &po, Rights::MAP_READ, b"fs/l1/a", fwd).unwrap();
        let pl = r.take_pending_matching(tok, id).unwrap();
        assert_eq!((pl.op, pl.dest(), pl.depth), (ForwardOp::Rename { flags: 1 }, Some(&b"fs/l1/b"[..]), 2));
        assert_eq!(pl.suffix(), Some(&b"fs/l1/a"[..]));
        drop(pl);
        let long = [b'a'; LOOKUP_SUFFIX_MAX + 1];
        let fwd = Forward { op: ForwardOp::Rename { flags: 0 }, dest: &long, depth: 0 };
        let id = r.begin_forward(tok, &po, Rights::MAP_READ, b"a", fwd).unwrap();
        let pl = r.take_pending_matching(tok, id).unwrap();
        assert_eq!(pl.dest(), None, "an overlong destination is absent, not truncated");
        drop(pl);
        drop(po);
        drop(r);
    }

    #[test]
    fn concurrent_lookups_correlate_independently() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        let a = r.begin(tok, &po, Rights::MAP_READ, b"one").unwrap();
        let b = r.begin(tok, &po, Rights::MAP_READ, b"two").unwrap();
        assert_ne!(a, b);
        let tb = r.take_pending_matching(tok, b).unwrap();
        assert_eq!(tb.suffix(), Some(&b"two"[..]));
        let ta = r.take_pending_matching(tok, a).unwrap();
        assert_eq!(ta.suffix(), Some(&b"one"[..]));
        drop(tb);
        drop(ta);
        drop(po);
        drop(r);
    }

    #[test]
    fn take_matching_correlates_by_request_id() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        let id = r.begin(tok, &po, Rights::MAP_READ, b"sys/gen").unwrap();
        assert!(r.take_pending_matching(tok, id ^ 0xFF).is_none());
        let taken = r.take_pending_matching(tok, id).unwrap();
        assert_eq!(taken.request_id, id);
        assert_eq!(taken.requested, Rights::MAP_READ);
        assert!(r.take_pending_matching(tok, id).is_none());
        drop(taken);
        drop(po);
        drop(r);
    }

    /// A `FileObject` adopted into a `Ref` (the form a `PendingFill` holds).
    fn file_obj() -> Ref<FileObject> {
        use crate::object::Producer;
        let f = FileObject::try_new(4096, Producer::Stub { base: 0 }).unwrap();
        Ref::new(f)
    }

    #[test]
    fn begin_records_the_lookup_suffix() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        let id = r.begin(tok, &po, Rights::MAP_READ, b"a/b/c").unwrap();
        let taken = r.take_pending_matching(tok, id).unwrap();
        assert_eq!(taken.suffix(), Some(&b"a/b/c"[..]));
        drop(taken);
        drop(po);
        drop(r);
    }

    #[test]
    fn fill_slot_is_independent_and_correlates_by_id() {
        init_global_heap();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        let fo = file_obj();
        let lid = r.begin(tok, &po, Rights::MAP_READ, b"x").unwrap();
        let fid = r.begin_fill(tok, &po, &fo, 2).unwrap();
        assert_ne!(lid, fid); // distinct request ids
        assert!(r.take_pending_fill_matching(tok, fid ^ 0xFF).is_none());
        let pf = r.take_pending_fill_matching(tok, fid).unwrap();
        assert_eq!(pf.request_id, fid);
        assert_eq!(pf.index, 2);
        let pl = r.take_pending_matching(tok, lid).unwrap();
        assert_eq!(pl.request_id, lid);
        drop(pf);
        drop(pl);
        drop(fo);
        drop(po);
        drop(r);
    }

    #[test]
    fn dropping_registration_routes_through_dispatch_destroy() {
        init_global_heap();
        let r = reg();
        // Reset AFTER construction: `reg()`/`endpoint()` drop the peer endpoint
        // (one `ipc_channel` destroy) while wiring up the pair.
        test_probe::reset();
        assert_eq!(test_probe::userspace_server_reg_destroys(), 0);
        assert_eq!(test_probe::ipc_channel_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::userspace_server_reg_destroys(), 1, "reg destructor ran");
        assert_eq!(test_probe::ipc_channel_destroys(), 1, "owned endpoint cascaded");
    }

    #[test]
    fn pending_po_is_released_when_registration_drops() {
        init_global_heap();
        test_probe::reset();
        let r = reg();
        let tok = &mut SchedToken::for_test();
        let po = make_po();
        r.begin(tok, &po, Rights::MAP_READ, b"sys/gen");
        assert_eq!(test_probe::pending_op_destroys(), 0);
        drop(r); // releases the pending entry's PO clone
        assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held by `po`");
        drop(po); // last ref → PO destroyed
        assert_eq!(test_probe::pending_op_destroys(), 1);
    }

    /// A file `reg`'s server names `id`, as a `FILE_BLOCKS` reply makes it.
    fn cached_file(reg: &Ref<UserspaceServerReg>, id: u64) -> Ref<FileObject> {
        use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
        use crate::object::{DeviceNode, Producer};
        let dev = DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap();
        let producer =
            Producer::FsServerBlocks { device: Ref::new(dev), block_size: 4096, reg: reg.clone(), file_id: id };
        Ref::new(FileObject::try_new(4096, producer).unwrap())
    }

    /// `f`'s count taken to zero with its closing yet to run, as another CPU dropping the last
    /// reference leaves it; the test leaks the object, so nothing is freed.
    fn mid_teardown(f: Ref<FileObject>) {
        let (obj, _) = f.erase().into_raw();
        // SAFETY: the object `f` held, whole, and never freed here.
        let header = unsafe { &*obj.cast::<crate::object::header::KObjectHeader>() };
        assert!(header.release(), "the test's was the last reference");
    }

    /// **An entry whose object is mid-teardown is replaced** by the next object for its file, which
    /// the dying one's closing, looking for its own address, leaves.
    #[test]
    fn a_mid_teardown_entry_is_replaced_by_the_next_object_for_its_file() {
        init_global_heap();
        let r = reg();
        let dying = cached_file(&r, 7);
        assert!(r.cache_get_or_insert(&dying).unwrap().is_none());
        mid_teardown(dying);
        let next = cached_file(&r, 7);
        assert!(r.cache_get_or_insert(&next).unwrap().is_none(), "entered in its place");
        let found = r.cache_get_or_insert(&cached_file(&r, 7)).unwrap();
        assert_eq!(found.as_ref().map(Ref::as_ptr), Some(next.as_ptr()), "the entry names it");
    }

    /// **`File::Forget` of a file whose object is mid-teardown answers at once** and takes its
    /// entry out: nothing of it is in flight, since an IRP pins its object.
    #[test]
    fn forgetting_a_mid_teardown_file_takes_its_entry_out() {
        init_global_heap();
        let r = reg();
        let dying = cached_file(&r, 7);
        assert!(r.cache_get_or_insert(&dying).unwrap().is_none());
        mid_teardown(dying);
        assert!(matches!(r.forget_file(7, &make_po()), Forgotten::Now));
        assert_eq!(r.cached_files(), 0);
    }

    /// **Only an object this registration produces is cached here**: an object's closing takes its
    /// entry out of the cache its producer names, so another's would outlive it.
    #[test]
    fn an_object_another_registration_produces_is_not_cached_here() {
        init_global_heap();
        let (mine, theirs) = (reg(), reg());
        let stub = Ref::new(FileObject::try_new(4096, crate::object::Producer::Stub { base: 0 }).unwrap());
        for f in [stub, cached_file(&theirs, 9)] {
            assert!(mine.cache_get_or_insert(&f).unwrap().is_none());
        }
        assert_eq!(mine.cached_files(), 0);
    }
}
