//! Files by path: the verbs that make, grow, shrink and rename one, each a resolve answered by the
//! filesystem server that owns the path, and a file's flush.

use libkern::{IO_OPCODE_FLUSH, IoOp, Rights};

use crate::error::{Result, check};
use crate::exec::{Op, submit_again};
use crate::handle::{CanLookup, CanMapWrite, File, Handle, Namespace, Ref};
use crate::sys::{self, Resize, Resolve};
use crate::time::Deadline;

/// Make the file at `path`, at least `size` bytes, and resolve it with at most `rights`. A file
/// already there is resolved, grown if it is shorter.
pub async fn create<'a, M: CanLookup + 'a, F>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    size: u64,
    rights: Rights,
) -> Result<Handle<File, F>> {
    resize(Resize::Create, ns.into(), path, size, rights).await
}

/// Resolve the file at `path`, grown to `size` first if it is shorter.
pub async fn grow<'a, M: CanLookup + 'a, F>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    size: u64,
    rights: Rights,
) -> Result<Handle<File, F>> {
    resize(Resize::Grow, ns.into(), path, size, rights).await
}

/// Resolve the file at `path`, shrunk to `size` first if it is longer.
pub async fn truncate<'a, M: CanLookup + 'a, F>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    size: u64,
    rights: Rights,
) -> Result<Handle<File, F>> {
    resize(Resize::Truncate, ns.into(), path, size, rights).await
}

async fn resize<M: CanLookup, F>(
    verb: Resize,
    ns: Ref<'_, Namespace, M>,
    path: &str,
    size: u64,
    rights: Rights,
) -> Result<Handle<File, F>> {
    let po = check(sys::ns_lookup(ns.raw().0, path.as_bytes(), rights.bits(), Resolve::Size(verb, size)))?;
    let done = Op::from_po(po).await?;
    Ok(Handle::fresh(done.result, rights))
}

/// Rename `from` to `to` on one filesystem, with the server's `flags`; `Unsupported` across
/// two, the cue to copy and remove.
pub async fn rename<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    from: &str,
    to: &str,
    flags: u16,
) -> Result<()> {
    let rename = Resolve::Rename { to: to.as_bytes(), flags };
    let po = check(sys::ns_lookup(ns.into().raw().0, from.as_bytes(), Rights::MAP_READ.bits(), rename))?;
    // A lookup's: a server may answer with a handle, which this closes.
    let done = Op::from_po(po).await?;
    if done.result != 0 {
        sys::handle_close(done.result);
    }
    Ok(())
}

/// Put every page of `file` on the device; needs `MAP_WRITE`.
pub async fn flush<'a, F: CanMapWrite + 'a>(file: impl Into<Ref<'a, File, F>>) -> Result<()> {
    let op = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    let raw = file.into().raw();
    let po = submit_again(|| sys::io_submit(raw.0, &op), Deadline::NEVER)?;
    Op::status(po).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapRead, MapReadWrite, NsMutable, block_on};
    use libkern::KError;

    fn rw() -> Rights {
        Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT
    }

    #[test]
    fn create_grow_and_truncate_set_the_size_they_say() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let f: Handle<File, MapReadWrite> = block_on(create(&ns, "/f", 100, rw())).unwrap();
        assert_eq!(f.stat().unwrap().size, 100);
        let size = |h: Handle<File, MapReadWrite>| h.stat().unwrap().size;
        assert_eq!(size(block_on(grow(&ns, "/f", 50, rw())).unwrap()), 100, "a grow never shrinks");
        assert_eq!(size(block_on(grow(&ns, "/f", 300, rw())).unwrap()), 300);
        assert_eq!(size(block_on(truncate(&ns, "/f", 400, rw())).unwrap()), 300, "a truncate never grows");
        assert_eq!(size(block_on(truncate(&ns, "/f", 7, rw())).unwrap()), 7);
        assert_eq!(size(block_on(create(&ns, "/f", 0, rw())).unwrap()), 7, "a create is not exclusive");
        let gone = block_on(grow::<_, MapReadWrite>(&ns, "/g", 1, rw())).err().unwrap();
        assert!(gone.is_not_found());
        let big = block_on(create::<_, MapReadWrite>(&ns, "/h", 1 << 32, rw())).err().unwrap();
        assert_eq!(big.code(), KError::TooLarge, "a size the wire cannot carry, refused before it resolves");
    }

    #[test]
    fn a_files_flush_is_its_flush_asked_again_while_its_mount_is_busy() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let f: Handle<File, MapReadWrite> = block_on(create(&ns, "/f", 1, rw())).unwrap();
        sys::busy_flushes(1);
        assert!(block_on(flush(&f)).is_ok());
        let flushes = sys::submitted().into_iter().filter(|op| op.opcode == IO_OPCODE_FLUSH && op.buffer == 0);
        assert_eq!(flushes.count(), 2, "refused once, and taken");
        sys::fail_next_sync(KError::IoError as i32);
        assert_eq!(block_on(flush(&f)).err().map(|e| e.code()), Some(KError::IoError));
    }

    #[test]
    fn a_rename_moves_the_name() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let _f: Handle<File, MapReadWrite> = block_on(create(&ns, "/a", 1, rw())).unwrap();
        block_on(rename(&ns, "/a", "/b", 0)).unwrap();
        assert!(block_on(ns.lookup::<File, MapRead>("/a", Rights::MAP_READ)).err().unwrap().is_not_found());
        assert!(block_on(ns.lookup::<File, MapRead>("/b", Rights::MAP_READ)).is_ok());
        assert!(block_on(rename(&ns, "/a", "/c", 0)).err().unwrap().is_not_found());
    }

    /// The flags reach the server: without the replace bit an existing name is refused, with it
    /// the name is taken.
    #[test]
    fn a_rename_onto_a_name_replaces_it_only_when_asked() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let _a: Handle<File, MapReadWrite> = block_on(create(&ns, "/a", 1, rw())).unwrap();
        let _b: Handle<File, MapReadWrite> = block_on(create(&ns, "/b", 1, rw())).unwrap();
        assert_eq!(block_on(rename(&ns, "/a", "/b", 0)).map_err(|e| e.code()), Err(KError::AlreadyExists));
        assert!(block_on(ns.lookup::<File, MapRead>("/a", Rights::MAP_READ)).is_ok(), "refused, the source stays");
        block_on(rename(&ns, "/a", "/b", 1)).unwrap();
        assert!(block_on(ns.lookup::<File, MapRead>("/a", Rights::MAP_READ)).err().unwrap().is_not_found());
    }

    #[test]
    fn a_rename_answered_with_a_handle_leaves_none_open() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let _f: Handle<File, MapReadWrite> = block_on(create(&ns, "/a", 1, rw())).unwrap();
        let open = sys::open_handles();
        sys::rename_with_handle();
        block_on(rename(&ns, "/a", "/b", 0)).unwrap();
        assert_eq!(sys::open_handles(), open);
    }

    #[test]
    fn a_flush_needs_map_write_and_says_when_it_failed() {
        sys::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let f: Handle<File, MapReadWrite> = block_on(create(&ns, "/f", 1, rw())).unwrap();
        block_on(flush(&f)).unwrap();
        sys::fail_next_sync(KError::IoError as i32);
        assert_eq!(block_on(flush(&f)).err().unwrap().code(), KError::IoError);
        let ro: Handle<File, MapReadWrite> = block_on(ns.lookup("/f", Rights::MAP_READ)).unwrap();
        assert_eq!(block_on(flush(&ro)).err().unwrap().code(), KError::NoAccess);
    }
}
