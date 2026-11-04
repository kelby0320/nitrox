//! Namespaces: resolving a path, binding and unbinding, a namespace's bindings, and a mount's
//! flush.

use libkern::{KError, NsEntry, Rights};

use crate::error::{Error, Result, check};
use crate::exec::Op;
use crate::handle::{CanBind, CanLookup, Channel, Handle, Namespace, NsMutable, Only, Ref, both};
use crate::sys;
use crate::wait::Signalled;

impl Handle<Namespace, NsMutable> {
    /// A new, empty namespace, with every right a namespace has.
    pub fn create() -> Result<Self> {
        let h = check(sys::ns_create())?;
        Ok(Handle::fresh(h, Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::UNBIND))
    }
}

both!([M: CanLookup] Namespace, M {
    /// Resolve `path` to a `T` of mode `M2` with at most `rights`. The type and mode are the
    /// caller's expectation, which the kernel checks at the first use.
    pub async fn lookup<T, M2>(&self, path: &str, rights: Rights) -> Result<Handle<T, M2>> {
        let po = check(sys::ns_lookup(self.raw().0, path.as_bytes(), rights.bits()))?;
        let done = Op::from_po(po).await?;
        Ok(Handle::fresh(done.result, rights))
    }

    /// Resolve `path` as [`lookup`](Self::lookup) does, without waiting here: for a caller that
    /// answers other handles while it resolves, and adds the [`Lookup`]'s operation to its wait.
    pub fn start_lookup<T, M2>(&self, path: &str, rights: Rights) -> Result<Lookup<T, M2>> {
        let po = check(sys::ns_lookup(self.raw().0, path.as_bytes(), rights.bits()))?;
        Ok(Lookup { op: Op::from_po(po), rights, _t: core::marker::PhantomData })
    }

    /// A new namespace holding a copy of every binding here, with every right a namespace has.
    pub fn copy(&self) -> Result<Handle<Namespace, NsMutable>> {
        let h = check(sys::ns_derive(self.raw().0))?;
        Ok(Handle::fresh(h, Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::UNBIND))
    }

    /// The bindings, in the order they were made: their paths, kinds and rights, not the files
    /// a mounted server holds.
    pub fn bindings(&self) -> Bindings<'_> {
        Bindings { ns: self.raw().0, next: 0, done: false, _b: core::marker::PhantomData }
    }

    /// Write back every dirty file of the server `path` resolves to: how many it wrote.
    pub async fn flush(&self, path: &str) -> Result<u64> {
        // TODO(surface-shape): the kernel answers it within the call, until a flush is a resolve.
        check(sys::ns_sync(self.raw().0, path.as_bytes()))
    }

    /// How many of the files the server `path` resolves to has handed out something still holds.
    pub fn held(&self, path: &str) -> Result<u64> {
        check(sys::ns_held(self.raw().0, path.as_bytes()))
    }
});

both!([M: CanBind] Namespace, M {
    /// Bind `resource` at `path`, so every lookup there resolves to its object with at most its
    /// rights; it needs `TRANSFER`, and this process `BIND_NAMESPACE`. A channel endpoint binds as
    /// a server instead, as [`bind_server`](Self::bind_server) does with no base.
    pub fn bind<'h, T: 'h, M2: 'h>(&self, path: &str, resource: impl Into<Ref<'h, T, M2>>) -> Result<()> {
        check(sys::ns_bind(self.raw().0, path.as_bytes(), resource.into().raw().0, b"")).map(|_| ())
    }

    /// Bind the server at `endpoint` at `path`, reaching only its tree under `base` when that is
    /// not empty: a lookup under `path` is forwarded to it, and the endpoint needs no right.
    pub fn bind_server(&self, path: &str, endpoint: Ref<'_, Channel, Only>, base: &str) -> Result<()> {
        check(sys::ns_bind(self.raw().0, path.as_bytes(), endpoint.raw().0, base.as_bytes())).map(|_| ())
    }

    /// Remove the binding at `path`; needs `UNBIND`.
    pub fn unbind(&self, path: &str) -> Result<()> {
        check(sys::ns_unbind(self.raw().0, path.as_bytes())).map(|_| ())
    }
});

/// A lookup in flight: its operation to add to a [`WaitSet`](crate::WaitSet), and the handle it
/// resolves to once a wait on that set finds it. Dropped unfinished, it closes what it resolved.
pub struct Lookup<T, M> {
    op: Op,
    rights: Rights,
    _t: core::marker::PhantomData<(T, M)>,
}

impl<T, M> Lookup<T, M> {
    /// The operation, for [`WaitSet::add_op`](crate::WaitSet::add_op).
    pub fn op(&self) -> &Op {
        &self.op
    }

    /// The handle, or why the lookup failed, if `signalled` found it; `None` while in flight.
    pub fn completion_in<const N: usize>(&mut self, signalled: &Signalled<N>) -> Option<Result<Handle<T, M>>> {
        let done = self.op.completion_in(signalled)?;
        Some(done.map(|r| Handle::fresh(r.result, self.rights)))
    }
}

/// A namespace's bindings, one call each, borrowed from its handle.
pub struct Bindings<'a> {
    ns: u64,
    next: u64,
    done: bool,
    _b: core::marker::PhantomData<&'a ()>,
}

impl Iterator for Bindings<'_> {
    type Item = Result<NsEntry>;

    fn next(&mut self) -> Option<Result<NsEntry>> {
        if self.done {
            return None;
        }
        let mut entry = NsEntry::zeroed();
        match check(sys::ns_enumerate(self.ns, self.next, &mut entry)) {
            Ok(_) => {
                self.next += 1;
                Some(Ok(entry))
            }
            Err(e) => {
                self.done = true;
                (e.code() != KError::NotFound).then_some(Err(e))
            }
        }
    }
}

impl Error {
    /// Whether a path resolved to nothing.
    pub fn is_not_found(&self) -> bool {
        self.code() == KError::NotFound
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapRead, MapReadWrite, Memory, NsReadOnly, block_on};

    fn ns() -> Handle<Namespace, NsMutable> {
        Handle::<Namespace, NsMutable>::create().unwrap()
    }

    #[test]
    fn a_bound_object_resolves_with_at_most_its_rights() {
        sys::reset();
        let ns = ns();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/m", &mem).unwrap();
        let got: Handle<Memory, MapRead> = block_on(ns.lookup("/m", Rights::MAP_READ | Rights::SIGNAL)).unwrap();
        assert_eq!(got.stat().err().unwrap().code(), KError::NoAccess, "INSPECT was not asked");
        let got: Handle<Memory, MapRead> = block_on(ns.lookup("/m", Rights::MAP_READ | Rights::INSPECT)).unwrap();
        assert_eq!(got.stat().unwrap().rights, (Rights::MAP_READ | Rights::INSPECT).bits());
        let missing = block_on(ns.lookup::<Memory, MapRead>("/n", Rights::MAP_READ)).err().unwrap();
        assert!(missing.is_not_found());
        assert_eq!(
            block_on(ns.lookup::<Memory, MapRead>("m", Rights::MAP_READ)).err().unwrap().code(),
            KError::InvalidArgument
        );
    }

    #[test]
    fn a_lookup_started_is_taken_from_a_wait_beside_other_handles() {
        sys::reset();
        // SAFETY: the mock's namespace, open for the test.
        let ns =
            unsafe { Ref::<Namespace, NsReadOnly>::from_raw(libkern::RawHandle(sys::served("/srv")), Rights::empty()) };
        let (a, _a2) = Handle::<Channel, Only>::pair(4).unwrap();
        let mut lookup = ns.start_lookup::<Memory, MapRead>("/srv/x", Rights::MAP_READ).unwrap();
        fn wait<T, M>(a: &Handle<Channel, Only>, l: &Lookup<T, M>, deadline: crate::Deadline) -> Signalled<2> {
            let mut set = crate::WaitSet::<2>::new();
            set.add(a).unwrap();
            set.add_op(l.op()).unwrap();
            set.wait(deadline).unwrap()
        }
        let s = wait(&a, &lookup, crate::Deadline::POLL);
        assert!(lookup.completion_in(&s).is_none(), "in flight: the server has not answered");
        let s = wait(&a, &lookup, crate::Deadline::at(1));
        let got = lookup.completion_in(&s).unwrap().unwrap();
        assert!(got.map(4096).is_ok(), "the server's answer, the caller's");
        assert_eq!(got.extra_rights(), Rights::MAP_READ, "the rights asked, as `lookup`'s handle carries them");
        let open = sys::open_handles();
        drop(lookup);
        assert_eq!(sys::open_handles(), open - 1, "the PO closes, and the handle taken stays");
        assert!(sys::is_open(got.raw().0));
    }

    #[test]
    fn a_lookup_a_wait_read_and_nobody_took_closes_its_handle() {
        sys::reset();
        // SAFETY: the mock's namespace, open for the test.
        let ns =
            unsafe { Ref::<Namespace, NsReadOnly>::from_raw(libkern::RawHandle(sys::served("/srv")), Rights::empty()) };
        let none = sys::open_handles();
        let lookup = ns.start_lookup::<Memory, MapRead>("/srv/x", Rights::MAP_READ).unwrap();
        let read = {
            let mut set = crate::WaitSet::<1>::new();
            set.add_op(lookup.op()).unwrap();
            set.wait(crate::Deadline::at(1)).unwrap().len()
        };
        assert_eq!(read, 1, "read, so installed");
        drop(lookup);
        assert_eq!(sys::open_handles(), none, "neither the PO nor what it resolved left open");
        assert_eq!(sys::stray_closes(), 0);
    }

    #[test]
    fn a_bind_needs_transfer_and_a_path_bound_once() {
        sys::reset();
        let ns = ns();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap().without_transfer().unwrap();
        assert_eq!(ns.bind("/m", &mem).err().unwrap().code(), KError::NoAccess);
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/m", &mem).unwrap();
        assert_eq!(ns.bind("/m", &mem).err().unwrap().code(), KError::AlreadyExists);
        sys::fail_next_bind(KError::NoAccess as i32);
        assert_eq!(ns.bind("/k", &mem).err().unwrap().code(), KError::NoAccess, "no BIND_NAMESPACE");
    }

    #[test]
    fn a_server_binds_with_no_right_and_a_base_and_answers_under_its_path() {
        sys::reset();
        let ns = ns();
        let (ep, _theirs) = Handle::<Channel, Only>::pair(4).unwrap();
        let ep = ep.without_transfer().unwrap();
        ns.bind_server("/srv", ep.as_ref(), "/home/alice").unwrap();
        ns.bind("/srv2", &ep).unwrap();
        assert_eq!(sys::bound_base(ns.raw().0, "/srv").as_deref(), Some(&b"/home/alice"[..]));
        assert_eq!(sys::bound_base(ns.raw().0, "/srv2").as_deref(), Some(&b""[..]));
        let got: Handle<Memory, MapRead> = block_on(ns.lookup("/srv/notes", Rights::MAP_READ)).unwrap();
        assert!(got.map(4096).is_ok(), "the server's answer");
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        assert_eq!(
            sys::ns_bind(ns.raw().0, b"/m", mem.raw().0, b"/x"),
            KError::InvalidArgument as i64,
            "a base on a direct bind"
        );
        assert_eq!(ns.bind_server("/srv3", ep.as_ref(), "/").err().unwrap().code(), KError::InvalidArgument);
    }

    #[test]
    fn a_server_can_answer_with_an_endpoint_whose_peer_reads_what_is_sent() {
        sys::reset();
        let (root, peer) = sys::answering("/log/x");
        // SAFETY: the mock's handles, open for the test.
        let (root, peer) = unsafe {
            (
                Ref::<Namespace, NsReadOnly>::from_raw(libkern::RawHandle(root), Rights::empty()),
                Ref::<Channel, Only>::from_raw(libkern::RawHandle(peer), Rights::empty()),
            )
        };
        let ep: Handle<Channel, Only> = block_on(root.lookup("/log/x", Rights::SEND)).unwrap();
        let mut m = crate::Msg::new();
        m.set(b"hi").unwrap();
        ep.send(&m, &mut crate::Carried::new()).unwrap();
        let mut got = crate::Msg::new();
        peer.recv(&mut got).unwrap();
        assert_eq!(got.payload(), b"hi");
    }

    #[test]
    fn bindings_list_in_order_then_end_and_unbind_removes() {
        sys::reset();
        let ns = ns();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/a", &mem).unwrap();
        ns.bind("/b", &mem).unwrap();
        let paths = |ns: &Handle<Namespace, NsMutable>| -> std::vec::Vec<std::vec::Vec<u8>> {
            ns.bindings().map(|e| e.map(|e| e.path[..e.path_len as usize].to_vec()).unwrap()).collect()
        };
        assert_eq!(paths(&ns), [b"/a".to_vec(), b"/b".to_vec()]);
        ns.unbind("/a").unwrap();
        assert_eq!(paths(&ns), [b"/b".to_vec()]);
        assert!(ns.unbind("/a").err().unwrap().is_not_found());
    }

    #[test]
    fn a_copy_is_a_snapshot() {
        sys::reset();
        let ns = ns();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/a", &mem).unwrap();
        let copy = ns.copy().unwrap();
        ns.bind("/b", &mem).unwrap();
        assert_eq!(copy.bindings().count(), 1, "a later bind reaches only the source");
    }

    #[test]
    fn a_borrowed_root_resolves_and_cannot_be_bound_into() {
        sys::reset();
        let ns = ns();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/m", &mem).unwrap();
        let root_raw = sys::with_rights(ns.raw().0, Rights::LOOKUP);
        // SAFETY: the mock's handle stays open for the test.
        let root = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(libkern::RawHandle(root_raw), Rights::empty()) };
        assert!(block_on(root.lookup::<Memory, MapRead>("/m", Rights::MAP_READ)).is_ok());
    }

    #[test]
    fn a_mount_flushes_and_counts_what_is_held() {
        sys::reset();
        let ns = ns();
        sys::mount("/", 3, 1);
        assert_eq!(block_on(ns.flush("/")).unwrap(), 3);
        assert_eq!(ns.held("/").unwrap(), 1);
        sys::fail_next_sync(KError::IoError as i32);
        assert_eq!(block_on(ns.flush("/")).err().unwrap().code(), KError::IoError);
        assert!(ns.held("/nothing").err().unwrap().is_not_found());
    }
}
