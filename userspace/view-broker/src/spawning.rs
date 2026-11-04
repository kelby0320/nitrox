//! What a program in a view is started with, apart from the message that follows it.

use alloc::string::String;
use libkern::{Rights, SysCaps};
use libos::{CanLookup, Channel, Handle, Namespace, NsMutable, Only, Process, Spawn, block_on};

/// A program started in a view, and this process's end of each channel it was handed.
pub struct Started {
    /// The process, to ask to stop.
    pub process: Handle<Process, Only>,
    /// Where the program's setup message goes.
    pub setup: Handle<Channel, Only>,
    /// The program's life channel: its peer closes when the program exits.
    pub life: Handle<Channel, Only>,
}

/// Why a program was not started.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    /// `/bin/<name>` does not resolve.
    NoSuchProgram,
    /// A channel could not be made.
    OutOfChannels,
    /// The kernel refused the spawn.
    WouldNotSpawn,
}

impl StartError {
    /// What a refused request is told.
    pub fn reason(self) -> &'static str {
        match self {
            StartError::NoSuchProgram => "there is no such program",
            StartError::OutOfChannels => "out of channels",
            StartError::WouldNotSpawn => "the program would not spawn",
        }
    }
}

/// Start `/bin/<program>` as `root` has it, in `view_ns`, moving it both channel ends it is
/// handed: the setup channel with every right the end has, the life channel with none.
///
/// **The image is looked up in `root`, not in the view**: a caller can prune its copy, and a name
/// must not fall through to some shorter binding there. `view_ns` stays this process's.
pub fn spawn_program<'a, M: CanLookup + 'a>(
    root: &'a Handle<Namespace, M>,
    program: &str,
    view_ns: &'a Handle<Namespace, NsMutable>,
    arg0: u64,
    syscaps: SysCaps,
) -> Result<Started, StartError> {
    let mut path = String::from("/bin/");
    path.push_str(program);
    let mut spawn = block_on(Spawn::path(root, &path)).map_err(|_| StartError::NoSuchProgram)?;
    let (Ok((setup_ours, setup_child)), Ok((life_ours, life_child))) = (Handle::pair(1), Handle::pair(1)) else {
        return Err(StartError::OutOfChannels);
    };
    // `syscaps` is no ambient authority but what a grant says.
    spawn.namespace(view_ns).arg0(arg0).syscaps(syscaps);
    // **The life channel is a handle the program never learns about**, so it stays open until the
    // program exits — which is how the broker learns *which* program it was
    // (`TODO(child-exit-attribution)`, as `service-mgr` does it). **No right on it**: one it could
    // duplicate or move would outlive it.
    spawn
        .grant(setup_child, Rights::from_bits(u64::MAX))
        .and_then(|s| s.grant(life_child, Rights::empty()))
        .map_err(|_| StartError::WouldNotSpawn)?;
    let process = spawn.spawn().map_err(|_| StartError::WouldNotSpawn)?;
    Ok(Started { process, setup: setup_ours, life: life_ours })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::{KError, RawHandle};
    use libos::{Carried, MapReadWrite, Memory, Msg, mock};

    fn root() -> Handle<Namespace, NsMutable> {
        mock::reset();
        let root = Handle::<Namespace, NsMutable>::create().unwrap();
        let image = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        root.bind("/bin/ls", &image).unwrap();
        root
    }

    /// The program's end of a channel it was granted, as it holds it.
    fn granted(n: usize) -> Handle<Channel, Only> {
        // SAFETY: a handle the mock made for the spawn, held by the child alone.
        unsafe { Handle::from_raw(RawHandle(mock::granted(0, n)), Rights::empty()) }
    }

    #[test]
    fn a_program_is_moved_both_ends_in_the_view_and_the_syscaps_it_is_given() {
        let root = root();
        let view_ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let started = spawn_program(&root, "ls", &view_ns, 5, SysCaps::SYSTEM_CLOCK).unwrap();
        let args = mock::spawned().pop().unwrap();
        assert_eq!((args.handle_count, args.move_mask, args.arg0), (2, 0b11, 5));
        assert_eq!((args.namespace, args.syscaps), (view_ns.raw().0, SysCaps::SYSTEM_CLOCK.bits()));
        assert_eq!(args.rights[..2], [u64::MAX, 0]);
        assert!(mock::is_open(view_ns.raw().0), "the view is the broker's still");
        assert!(mock::is_open(started.setup.raw().0) && mock::is_open(started.life.raw().0));
        drop(started);
        assert_eq!(mock::stray_closes(), 0, "a moved end is not closed again");
    }

    #[test]
    fn a_program_holds_no_right_on_its_life_channel() {
        let root = root();
        let view_ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let started = spawn_program(&root, "ls", &view_ns, 0, SysCaps::empty()).unwrap();
        let (setup, life) = (granted(0), granted(1));
        started.setup.send(&Msg::new(), &mut Carried::new()).unwrap();
        started.life.send(&Msg::new(), &mut Carried::new()).unwrap();
        assert!(setup.recv(&mut Msg::new()).is_ok(), "the setup end reads what is sent");
        let refused = life.recv(&mut Msg::new()).err().unwrap().code();
        assert_eq!(refused, KError::NoAccess, "the life end can do nothing");
    }

    #[test]
    fn a_program_that_is_not_there_starts_nothing_and_leaks_nothing() {
        let root = root();
        let view_ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let open = mock::open_handles();
        let err = spawn_program(&root, "missing", &view_ns, 0, SysCaps::empty()).err().unwrap();
        assert_eq!(err, StartError::NoSuchProgram);
        assert_eq!((mock::spawned().len(), mock::open_handles()), (0, open));
    }
}
