//! Processes: [`Spawn`] a program, and [`exit`].

use libkern::{HandleInfo, IO_OPCODE_READ, IoOp, KError, RawHandle, Rights, SPAWN_MAX_HANDLES, SpawnArgs, SysCaps};

use crate::error::{Error, Result, check};
use crate::exec::Op;
use crate::handle::{
    CanLookup, CanMapRead, File, Handle, Handles, MapRead, MapReadWrite, Mappable, Memory, Namespace, Only, Process,
    Ref, both,
};
use crate::{Deadline, WaitSet, sys};

/// **How long a spawn waits for its image's read**, as long as a flush: twice a device's 30 s.
const IMAGE_READ_NS: u64 = crate::FLUSH_TIMEOUT_NS;

/// A child process to start: its image, its namespace, the handles it is given and its `arg0`.
///
/// **A grant moves or copies.** [`grant`](Spawn::grant) takes a handle, which the child has
/// once it starts and which comes back in [`Refused`] if it does not; [`grant_copy`]
/// (Spawn::grant_copy) borrows one, which needs `TRANSFER` and `DUPLICATE`. The first grant is the child's
/// `rdx` at entry.
pub struct Spawn<'a> {
    image: u64,
    /// Whether `image` is a file, which needs `INSPECT` for its size.
    file: bool,
    /// The image [`path`](Spawn::path) looked up, closed once the spawn is done with it.
    _held_image: Option<Handle<File, MapRead>>,
    args: SpawnArgs,
    moved: Handles<SPAWN_MAX_HANDLES>,
    _b: core::marker::PhantomData<&'a ()>,
}

/// A spawn the kernel refused, and the handles it would have moved, still this process's.
pub struct Refused {
    /// Why.
    pub error: Error,
    /// The moved grants, in the order granted.
    pub moved: Handles<SPAWN_MAX_HANDLES>,
}

impl From<Refused> for Error {
    fn from(r: Refused) -> Error {
        r.error
    }
}

impl<'a> Spawn<'a> {
    /// Spawn the ELF image `image` holds, which it needs `MAP_READ` on.
    /// A file is read into memory when it starts, and needs `INSPECT` too, for its size.
    pub fn new<T: Mappable + 'a, M: CanMapRead + 'a>(image: impl Into<Ref<'a, T, M>>) -> Self {
        Spawn::of(image.into().raw().0, T::FILE, None)
    }

    fn of(image: u64, file: bool, _held_image: Option<Handle<File, MapRead>>) -> Self {
        let args = SpawnArgs {
            image,
            handle_count: 0,
            move_mask: 0,
            arg0: 0,
            handles: [0; SPAWN_MAX_HANDLES],
            rights: [0; SPAWN_MAX_HANDLES],
            namespace: 0,
            syscaps: 0,
        };
        Spawn { image, file, _held_image, args, moved: Handles::new(), _b: core::marker::PhantomData }
    }

    /// Spawn the program at `path` in `ns`.
    pub async fn path<M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<Spawn<'a>> {
        let image: Handle<File, MapRead> = ns.into().lookup(path, Rights::MAP_READ | Rights::INSPECT).await?;
        Ok(Spawn::of(image.raw().0, true, Some(image)))
    }

    /// The child's root: a namespace it may only look up in. Without one, this process's own root,
    /// shared and not copied: a later bind in it reaches the child too.
    pub fn namespace<M: CanLookup + 'a>(&mut self, ns: impl Into<Ref<'a, Namespace, M>>) -> &mut Self {
        self.args.namespace = ns.into().raw().0;
        self
    }

    /// The child's `arg0`, its `rcx` at entry.
    pub fn arg0(&mut self, arg0: u64) -> &mut Self {
        self.args.arg0 = arg0;
        self
    }

    /// The system capabilities to give it, which the kernel holds to this process's own.
    pub fn syscaps(&mut self, caps: SysCaps) -> &mut Self {
        self.args.syscaps = caps.bits();
        self
    }

    /// Give the child `h`, with at most `rights`: it leaves this process when the child starts.
    /// Handed back once [`SPAWN_MAX_HANDLES`] are granted.
    pub fn grant<T, M>(&mut self, h: Handle<T, M>, rights: Rights) -> core::result::Result<&mut Self, Handle<T, M>> {
        let i = self.args.handle_count as usize;
        if i == SPAWN_MAX_HANDLES {
            return Err(h);
        }
        let raw = h.raw().0;
        self.moved.push(h)?;
        self.place(i, raw, rights);
        self.args.move_mask |= 1 << i;
        Ok(self)
    }

    /// Give the child a copy of `h`, with at most `rights`, which needs `TRANSFER` and `DUPLICATE`;
    /// this process keeps its own. `TooLarge` once [`SPAWN_MAX_HANDLES`] are granted.
    pub fn grant_copy<T: 'a, M: 'a>(&mut self, h: impl Into<Ref<'a, T, M>>, rights: Rights) -> Result<&mut Self> {
        let i = self.args.handle_count as usize;
        if i == SPAWN_MAX_HANDLES {
            return Err(Error::from(KError::TooLarge));
        }
        self.place(i, h.into().raw().0, rights);
        Ok(self)
    }

    fn place(&mut self, i: usize, raw: u64, rights: Rights) {
        self.args.handles[i] = raw;
        self.args.rights[i] = rights.bits();
        self.args.handle_count += 1;
    }

    /// Start it. The kernel starts it whole or not at all, so a refusal hands back every moved
    /// grant.
    pub fn spawn(mut self) -> core::result::Result<Handle<Process, Only>, Refused> {
        let read = match read_image(self.image, self.file) {
            Ok(read) => read,
            Err(error) => return Err(Refused { error, moved: core::mem::take(&mut self.moved) }),
        };
        self.args.image = read.as_ref().map_or(self.image, |m| m.raw().0);
        match check(sys::process_spawn(&self.args)) {
            Ok(h) => {
                self.moved.moved();
                Ok(Handle::fresh(h, Rights::SIGNAL | Rights::TERMINATE))
            }
            Err(error) => Err(Refused { error, moved: core::mem::take(&mut self.moved) }),
        }
    }
}

both!([] Process, Only {
    /// Ask the process to exit: a `TerminateRequested` on its queue, which it may ignore, as no
    /// process is stopped from outside. Needs `SIGNAL`; `Unsupported` if it has no queue.
    pub fn terminate(&self) -> Result<()> {
        check(sys::process_terminate(self.raw().0)).map(|_| ())
    }
});

/// **A file image read into memory**, as the kernel spawns from nothing else, so no spawn waits on
/// a device; `None` for an image that is memory already. Its size is the file's stat, and a read
/// shorter than that is refused; one that runs past [`IMAGE_READ_NS`] is abandoned, `TimedOut`.
fn read_image(image: u64, file: bool) -> Result<Option<Handle<Memory, MapReadWrite>>> {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    match check(sys::handle_stat(image, &mut info)) {
        Err(e) if file => return Err(e),
        Ok(_) if info.object_type == libkern::handle::KOBJ_FILE_OBJECT => {}
        _ => return Ok(None),
    }
    if info.size > libkern::SPAWN_IMAGE_MAX {
        return Err(Error::from(KError::TooLarge));
    }
    let memory = Handle::<Memory, MapReadWrite>::create(info.size as usize)?;
    let deadline = Deadline::after(IMAGE_READ_NS).unwrap_or(Deadline::at(IMAGE_READ_NS));
    let op =
        IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: memory.raw().0, buf_offset: 0, offset: 0, length: info.size };
    let mut op = Op::submit_by(RawHandle(image), &op, deadline)?;
    let signalled = {
        let mut set = WaitSet::<1>::new();
        set.add_op(&op)?;
        set.wait(deadline)?
    };
    match op.completion_in(&signalled) {
        None => {
            op.abandon(memory);
            Err(Error::from(KError::TimedOut))
        }
        Some(Ok(done)) if done.result == info.size => Ok(Some(memory)),
        Some(Ok(_)) => Err(Error::from(KError::InvalidArgument)),
        Some(Err(e)) => Err(e),
    }
}

/// End this process with `status`, closing every handle it holds.
pub fn exit(status: i32) -> ! {
    sys::process_exit(status)
}

/// Spawn a child process from `args` (a libkern [`SpawnArgs`]), returning an **owning**
/// handle. Its moves are held only by the caller's comments; [`Spawn`] holds them.
pub fn spawn(args: &SpawnArgs) -> Result<Handle<Process, Only>> {
    let h = check(sys::process_spawn(args))?;
    // SAFETY: `sys_process_spawn` returned a fresh child-process handle we own.
    Ok(unsafe { Handle::from_raw(RawHandle(h), Rights::SIGNAL | Rights::TERMINATE) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, MapReadWrite, Memory, NsMutable, block_on};
    use libkern::IO_OPCODE_READ;

    fn program() -> (Handle<Namespace, NsMutable>, Handle<Memory, MapReadWrite>) {
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let image = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        ns.bind("/bin/x", &image).unwrap();
        (ns, image)
    }

    #[test]
    fn a_spawn_by_path_moves_its_grants_and_copies_its_copies() {
        sys::reset();
        let (ns, _image) = program();
        let (ours, theirs) = Handle::<Channel, Only>::pair(4).unwrap();
        let moved = theirs.raw().0;
        let mut s = block_on(Spawn::path(&ns, "/bin/x")).unwrap();
        s.namespace(&ns).arg0(7);
        s.grant(theirs, Rights::SEND | Rights::RECV).ok().unwrap();
        s.grant_copy(&ours, Rights::SEND).unwrap();
        let child = s.spawn().ok().unwrap();
        let args = sys::spawned().pop().unwrap();
        assert_eq!((args.handle_count, args.move_mask, args.arg0, args.namespace), (2, 1, 7, ns.raw().0));
        assert_eq!(args.handles[..2], [moved, ours.raw().0], "the moved one first: the child's rdx");
        assert!(!sys::is_open(moved) && sys::is_open(ours.raw().0), "moved once, copied once");
        assert_eq!(sys::open_handles(), 6, "two devices, ns, image, ours, child: the image lookup closed");
        drop(child);
        assert_eq!(sys::stray_closes(), 0, "nothing closed twice, the moved grant least of all");
    }

    /// **A child holds its grants**: a moved end's peer still sends to it, and it reads with the
    /// rights it was granted, no more.
    #[test]
    fn a_spawned_child_holds_what_it_was_granted() {
        sys::reset();
        let (ns, _image) = program();
        let (ours, theirs) = Handle::<Channel, Only>::pair(4).unwrap();
        let mut s = block_on(Spawn::path(&ns, "/bin/x")).unwrap();
        s.grant(theirs, Rights::RECV).ok().unwrap();
        let _child = s.spawn().ok().unwrap();
        let mut msg = crate::Msg::new();
        msg.set(b"hi").unwrap();
        ours.send(&msg, &mut crate::Carried::new()).unwrap();
        // SAFETY: a handle the mock has just made for the test.
        let theirs = unsafe { Handle::<Channel, Only>::from_raw(RawHandle(sys::granted(0, 0)), Rights::empty()) };
        let mut got = crate::Msg::new();
        theirs.recv(&mut got).unwrap();
        assert_eq!(got.payload(), b"hi");
        let refused = theirs.send(&msg, &mut crate::Carried::new()).err().unwrap().code();
        assert_eq!(refused, KError::NoAccess, "granted RECV alone");
        let (ours, theirs) = Handle::<Channel, Only>::pair(4).unwrap();
        let mut s = block_on(Spawn::path(&ns, "/bin/x")).unwrap();
        s.grant(theirs, Rights::RECV).ok().unwrap();
        sys::exit_next_child();
        let _gone = s.spawn().ok().unwrap();
        let closed = ours.send(&msg, &mut crate::Carried::new()).err().unwrap().code();
        assert_eq!(closed, KError::PeerClosed, "a child that exited holds nothing");
    }

    #[test]
    fn a_refused_spawn_hands_back_what_it_would_have_moved() {
        sys::reset();
        let (ns, _image) = program();
        let (ours, theirs) = Handle::<Channel, Only>::pair(4).unwrap();
        let no_duplicate = ours.duplicate(Rights::SEND | Rights::TRANSFER).unwrap();
        let mut s = block_on(Spawn::path(&ns, "/bin/x")).unwrap();
        s.grant(theirs, Rights::SEND).ok().unwrap();
        s.grant_copy(&no_duplicate, Rights::SEND).unwrap();
        let mut refused = s.spawn().err().unwrap();
        assert_eq!(refused.error.code(), KError::NoAccess, "a copy without DUPLICATE");
        let back: Handle<Channel, Only> = refused.moved.take(0).unwrap();
        assert!(sys::is_open(back.raw().0));
        assert!(sys::spawned().is_empty());
    }

    #[test]
    fn a_fifth_grant_is_handed_back_and_a_missing_program_is_not_found() {
        sys::reset();
        let (ns, image) = program();
        let mut s = Spawn::new(&image);
        for _ in 0..SPAWN_MAX_HANDLES {
            s.grant(Handle::<Memory, MapReadWrite>::create(4096).unwrap(), Rights::empty()).ok().unwrap();
        }
        assert!(s.grant(Handle::<Memory, MapReadWrite>::create(4096).unwrap(), Rights::empty()).is_err());
        assert_eq!(s.grant_copy(&image, Rights::empty()).err().unwrap().code(), KError::TooLarge);
        assert!(block_on(Spawn::path(&ns, "/bin/y")).err().unwrap().is_not_found());
    }

    /// **A grant after four copies is handed back**: the copies fill the slots the moves' list
    /// still has room for.
    #[test]
    fn a_grant_after_four_copies_is_handed_back() {
        sys::reset();
        let (_ns, image) = program();
        let copies: std::vec::Vec<_> =
            (0..SPAWN_MAX_HANDLES).map(|_| Handle::<Memory, MapReadWrite>::create(4096).unwrap()).collect();
        let mut s = Spawn::new(&image);
        for c in &copies {
            s.grant_copy(c, Rights::empty()).unwrap();
        }
        let back = s.grant(Handle::<Memory, MapReadWrite>::create(4096).unwrap(), Rights::empty()).err().unwrap();
        assert!(sys::is_open(back.raw().0));
        assert!(s.spawn().is_ok());
        assert_eq!(sys::spawned()[0].handle_count as usize, SPAWN_MAX_HANDLES);
    }

    #[test]
    fn a_terminate_asks_and_needs_signal() {
        sys::reset();
        let (_ns, image) = program();
        let child = Spawn::new(&image).spawn().ok().unwrap();
        child.terminate().unwrap();
        assert_eq!(sys::terminated(), 1);
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        // SAFETY: the mock answers for a handle of the wrong type; nothing uses it as memory.
        let wrong = unsafe { Ref::<Process, Only>::from_raw(ns.raw(), Rights::empty()) };
        assert_eq!(wrong.terminate().err().unwrap().code(), KError::NoAccess, "rights before type");
        assert_eq!(sys::terminated(), 1);
    }

    #[test]
    #[should_panic(expected = "exit(3)")]
    fn exit_ends_the_process() {
        exit(3);
    }

    fn file_program(rights: Rights) -> (Handle<Namespace, NsMutable>, Handle<File, MapReadWrite>) {
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let file = block_on(crate::fs::create(&ns, "/bin/f", 5000, rights | Rights::MAP_WRITE)).unwrap();
        (ns, file)
    }

    #[test]
    fn a_file_image_is_read_into_memory_and_the_memory_spawned() {
        sys::reset();
        let (ns, _file) = file_program(Rights::MAP_READ | Rights::INSPECT);
        let s = block_on(Spawn::path(&ns, "/bin/f")).unwrap();
        assert!(s.spawn().is_ok(), "the kernel, as the model, takes no file");
        let reads: std::vec::Vec<_> = sys::submitted().into_iter().filter(|op| op.opcode == IO_OPCODE_READ).collect();
        assert_eq!(reads.iter().map(|op| op.length).collect::<std::vec::Vec<_>>(), [5000], "its stat's size");
        assert!(!sys::is_open(reads[0].buffer), "the memory let go once the child has its copy");
    }

    #[test]
    fn an_image_read_that_does_not_finish_is_abandoned_and_a_short_one_refused() {
        sys::reset();
        let (ns, _file) = file_program(Rights::MAP_READ | Rights::INSPECT);
        sys::next_read_short(1);
        let short = block_on(Spawn::path(&ns, "/bin/f")).unwrap().spawn();
        assert_eq!(short.err().map(|r| r.error.code()), Some(KError::InvalidArgument), "shorter than its stat");
        sys::reads_never_finish();
        let stuck = block_on(Spawn::path(&ns, "/bin/f")).unwrap().spawn();
        assert_eq!(stuck.err().map(|r| r.error.code()), Some(KError::TimedOut));
        assert_eq!(sys::closed_in_flight(), 1, "abandoned, its buffer with it");
        assert!(sys::spawned().is_empty());
    }

    #[test]
    fn a_file_image_the_spawner_cannot_stat_is_refused() {
        sys::reset();
        let (_ns, file) = file_program(Rights::MAP_READ);
        let refused = Spawn::new(&file).spawn();
        assert_eq!(refused.err().map(|r| r.error.code()), Some(KError::NoAccess), "its size is its stat's");
    }

    #[test]
    fn a_file_image_past_what_spawn_takes_is_refused_before_it_is_read() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let rights = Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT;
        let _big: Handle<File, MapReadWrite> =
            block_on(crate::fs::create(&ns, "/bin/big", libkern::SPAWN_IMAGE_MAX + 1, rights)).unwrap();
        let refused = block_on(Spawn::path(&ns, "/bin/big")).unwrap().spawn();
        assert_eq!(refused.err().map(|r| r.error.code()), Some(KError::TooLarge));
        assert!(!sys::submitted().iter().any(|op| op.opcode == IO_OPCODE_READ), "nothing read");
        let _edge: Handle<File, MapReadWrite> =
            block_on(crate::fs::create(&ns, "/bin/edge", libkern::SPAWN_IMAGE_MAX, rights)).unwrap();
        let taken = block_on(Spawn::path(&ns, "/bin/edge")).unwrap().spawn();
        assert_ne!(taken.err().map(|r| r.error.code()), Some(KError::TooLarge), "the largest spawn takes");
        assert!(sys::submitted().iter().any(|op| op.opcode == IO_OPCODE_READ), "read");
    }
}
