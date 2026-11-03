//! Processes: [`Spawn`] a program, and [`exit`].

use libkern::{KError, RawHandle, Rights, SPAWN_MAX_HANDLES, SpawnArgs, SysCaps};

use crate::error::{Error, Result, check};
use crate::handle::{CanLookup, CanMapRead, File, Handle, Handles, MapRead, Mappable, Namespace, Only, Process, Ref};
use crate::sys;

/// A child process to start: its image, its namespace, the handles it is given and its `arg0`.
///
/// **A grant moves or copies.** [`grant`](Spawn::grant) takes a handle, which the child has
/// once it starts and which comes back in [`Refused`] if it does not; [`grant_copy`]
/// (Spawn::grant_copy) borrows one, which needs `TRANSFER` and `DUPLICATE`. The first grant is the child's
/// `rdx` at entry.
pub struct Spawn<'a> {
    image: u64,
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
    pub fn new<T: Mappable + 'a, M: CanMapRead + 'a>(image: impl Into<Ref<'a, T, M>>) -> Self {
        Spawn::of(image.into().raw().0, None)
    }

    fn of(image: u64, _held_image: Option<Handle<File, MapRead>>) -> Self {
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
        Spawn { image, _held_image, args, moved: Handles::new(), _b: core::marker::PhantomData }
    }

    /// Spawn the program at `path` in `ns`.
    pub async fn path<M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Result<Spawn<'a>> {
        // TODO(surface-shape): the kernel reads a file's image within the spawn, until spawn takes
        // only memory and this reads it first.
        let image: Handle<File, MapRead> = ns.into().lookup(path, Rights::MAP_READ).await?;
        Ok(Spawn::of(image.raw().0, Some(image)))
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
        self.args.image = self.image;
        match check(sys::process_spawn(&self.args)) {
            Ok(h) => {
                self.moved.moved();
                Ok(Handle::fresh(h, Rights::SIGNAL | Rights::TERMINATE))
            }
            Err(error) => Err(Refused { error, moved: core::mem::take(&mut self.moved) }),
        }
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
    #[should_panic(expected = "exit(3)")]
    fn exit_ends_the_process() {
        exit(3);
    }
}
