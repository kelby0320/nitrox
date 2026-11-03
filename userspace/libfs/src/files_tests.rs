//! The file helpers over libos's model: files with real bytes behind a mapping, in a namespace.

use super::*;
use crate::files::rename_error;
use libkern::Rights;
use libos::{File, Handle, MapReadWrite, Namespace, NsMutable, block_on, fs, mock};

fn fresh() -> Handle<Namespace, NsMutable> {
    mock::reset();
    Handle::<Namespace, NsMutable>::create().unwrap()
}

fn put(ns: &Handle<Namespace, NsMutable>, path: &str, bytes: &[u8]) {
    write(ns, path, bytes).unwrap();
}

fn get(ns: &Handle<Namespace, NsMutable>, path: &str) -> Vec<u8> {
    read(ns, path).unwrap()
}

/// A file of `size` bytes nothing has written, for sizes too big to write.
fn sized(ns: &Handle<Namespace, NsMutable>, path: &str, size: u64) {
    let rights = Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT;
    let _: Handle<File, MapReadWrite> = block_on(fs::create(ns, path, size, rights)).unwrap();
}

/// What `f` returns, and the resolves it made: the model answers each at once, in one wait.
fn resolves<R>(f: impl FnOnce() -> R) -> (R, u32) {
    let before = mock::counts().1;
    let r = f();
    (r, mock::counts().1 - before)
}

/// What `f` returns, and whether it left a handle open or a file mapped.
fn leaks<R>(f: impl FnOnce() -> R) -> (R, bool) {
    let (open, maps) = (mock::open_handles(), mock::mappings());
    let r = f();
    (r, mock::open_handles() != open || mock::mappings() != maps)
}

#[test]
fn a_written_file_reads_back_whole_and_a_shorter_write_leaves_no_tail() {
    let ns = fresh();
    put(&ns, "/f", b"hello world");
    assert_eq!(get(&ns, "/f"), b"hello world");
    put(&ns, "/f", b"hi");
    assert_eq!(file_len(&ns, "/f"), Some(2));
    assert_eq!(get(&ns, "/f"), b"hi");
    put(&ns, "/f", b"a longer file than before");
    assert_eq!(get(&ns, "/f"), b"a longer file than before");
    put(&ns, "/f", b"");
    assert_eq!(file_len(&ns, "/f"), Some(0));
    assert_eq!(get(&ns, "/f"), b"");
}

#[test]
fn a_write_spans_more_than_a_chunk() {
    let ns = fresh();
    let bytes: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
    put(&ns, "/big", &bytes);
    assert_eq!(get(&ns, "/big"), bytes);
    copy(&ns, "/big", "/copy", false).unwrap();
    assert_eq!(get(&ns, "/copy"), bytes, "a copy crosses the chunk size too");
}

#[test]
fn a_read_says_why_it_gave_nothing() {
    let ns = fresh();
    assert_eq!(read(&ns, "/none"), Err(FileError::NotFound));
    sized(&ns, "/huge", MAX_COPY + 1);
    assert_eq!(read(&ns, "/huge"), Err(FileError::TooLarge));
}

#[test]
fn a_write_past_the_limit_touches_nothing() {
    let ns = fresh();
    put(&ns, "/f", b"kept");
    assert_eq!(write(&ns, "/f", &alloc::vec![0u8; MAX_COPY as usize + 1]), Err(FileError::TooLarge));
    assert_eq!(get(&ns, "/f"), b"kept");
}

#[test]
fn a_size_is_a_files_and_none_for_a_path_that_is_not() {
    let ns = fresh();
    put(&ns, "/f", b"12345");
    assert_eq!(file_len(&ns, "/f"), Some(5));
    assert_eq!(file_len(&ns, "/none"), None);
    let mem = Handle::<libos::Memory, MapReadWrite>::create(4096).unwrap();
    ns.bind("/mem", &mem).unwrap();
    assert_eq!(file_len(&ns, "/mem"), Some(4096), "a memory object stats as a file does");
    let (root, _peer) = mock::answering("/dir");
    assert_eq!(file_size(root, b"/dir"), None, "a session endpoint is not a file");
}

#[test]
fn a_copy_gives_the_destination_exactly_the_source() {
    let ns = fresh();
    put(&ns, "/src", b"source bytes");
    assert_eq!(copy(&ns, "/src", "/missing", false), Ok(12));
    assert_eq!(get(&ns, "/missing"), b"source bytes");

    put(&ns, "/shorter", b"abc");
    assert_eq!(copy(&ns, "/src", "/shorter", true), Ok(12));
    assert_eq!(get(&ns, "/shorter"), b"source bytes");

    put(&ns, "/longer", b"a destination longer than its source");
    assert_eq!(copy(&ns, "/src", "/longer", true), Ok(12));
    assert_eq!(file_len(&ns, "/longer"), Some(12), "the old tail does not survive");
    assert_eq!(get(&ns, "/longer"), b"source bytes");

    put(&ns, "/same", b"xxxxxxxxxxxx");
    assert_eq!(copy(&ns, "/src", "/same", true), Ok(12));
    assert_eq!(get(&ns, "/same"), b"source bytes");
    assert_eq!(get(&ns, "/src"), b"source bytes", "the source is as it was");
}

#[test]
fn an_empty_source_makes_or_empties_the_destination() {
    let ns = fresh();
    put(&ns, "/empty", b"");
    assert_eq!(copy(&ns, "/empty", "/new", false), Ok(0));
    assert_eq!(file_len(&ns, "/new"), Some(0), "the destination exists though nothing maps");
    put(&ns, "/full", b"contents");
    assert_eq!(copy(&ns, "/empty", "/full", true), Ok(0));
    assert_eq!(file_len(&ns, "/full"), Some(0));
}

#[test]
fn a_copy_that_may_not_overwrite_stops_at_exists() {
    let ns = fresh();
    put(&ns, "/src", b"new");
    put(&ns, "/dst", b"old contents");
    assert_eq!(copy(&ns, "/src", "/dst", false), Err(FileError::Exists));
    assert_eq!(get(&ns, "/dst"), b"old contents");
    let (r, n) = resolves(|| copy(&ns, "/src", "/dst", false));
    assert_eq!((r, n), (Err(FileError::Exists), 2), "the source, and one look at the destination");
}

#[test]
fn a_copy_names_what_is_wrong_with_its_source() {
    let ns = fresh();
    assert_eq!(copy(&ns, "/none", "/dst", true), Err(FileError::NotFound));
    assert_eq!(file_len(&ns, "/dst"), None);
    sized(&ns, "/huge", MAX_COPY + 1);
    assert_eq!(copy(&ns, "/huge", "/dst", true), Err(FileError::TooLarge));
    assert_eq!(file_len(&ns, "/dst"), None, "nothing is made for a source too big");
}

#[test]
fn a_copy_resolves_its_destination_at_most_twice() {
    let ns = fresh();
    put(&ns, "/src", b"source");
    put(&ns, "/shorter", b"abc");
    put(&ns, "/longer", b"a destination longer than its source");
    put(&ns, "/same", b"xxxxxx");
    // The source is one more resolve than the destination's.
    for (dst, total) in [("/missing", 3), ("/shorter", 3), ("/same", 3), ("/longer", 3)] {
        let (r, n) = resolves(|| copy(&ns, "/src", dst, true));
        assert_eq!(r, Ok(6));
        assert_eq!(n, total, "{dst}");
    }
    let (_, n) = resolves(|| write(&ns, "/shorter", b"abcdefgh"));
    assert_eq!(n, 2, "a write looks once and creates");
    let (_, n) = resolves(|| write(&ns, "/shorter", b"ab"));
    assert_eq!(n, 2, "a write looks once and shrinks, and does not look again");
}

#[test]
fn nothing_is_left_open_or_mapped() {
    let ns = fresh();
    put(&ns, "/src", b"source");
    put(&ns, "/longer", b"a destination longer than its source");
    put(&ns, "/empty", b"");
    let raw = ns.raw().0;
    let ops: [(&str, &dyn Fn() -> bool); 11] = [
        ("copy to missing", &|| copy(&ns, "/src", "/m", false).is_ok()),
        ("copy to longer", &|| copy(&ns, "/src", "/longer", true).is_ok()),
        ("copy refused", &|| copy(&ns, "/src", "/m", false).is_err()),
        ("copy empty", &|| copy(&ns, "/empty", "/m", true).is_ok()),
        ("copy no source", &|| copy(&ns, "/none", "/m", true).is_err()),
        ("read", &|| read(&ns, "/src").is_ok()),
        ("read empty", &|| read(&ns, "/empty").is_ok()),
        ("write", &|| write(&ns, "/w", b"bytes").is_ok()),
        ("save", &|| save(&ns, "/w", b"saved").is_ok()),
        ("size", &|| file_len(&ns, "/src").is_some()),
        ("raw copy", &|| copy_file(raw, b"/src", b"/raw", true).is_ok()),
    ];
    for (what, op) in ops {
        let (ok, leaked) = leaks(op);
        assert!(ok, "{what} should have done what it says");
        assert!(!leaked, "{what} left a handle open or a file mapped");
    }
}

#[test]
fn a_flush_that_fails_fails_the_copy_and_the_write() {
    let ns = fresh();
    put(&ns, "/src", b"source");
    let io = FileError::Io(libkern::error::KError::IoError.as_i32());
    mock::fail_next_sync(libkern::error::KError::IoError as i32);
    assert_eq!(copy(&ns, "/src", "/dst", true), Err(io));
    mock::fail_next_sync(libkern::error::KError::IoError as i32);
    assert_eq!(write(&ns, "/dst", b"bytes"), Err(io));
}

#[test]
fn a_save_replaces_the_file_and_a_failed_one_leaves_it() {
    let ns = fresh();
    put(&ns, "/doc", b"old");
    save(&ns, "/doc", b"new, and longer").unwrap();
    assert_eq!(get(&ns, "/doc"), b"new, and longer");
    assert_eq!(file_len(&ns, "/doc.save-tmp"), None, "the temporary is renamed away");

    mock::fail_next_sync(libkern::error::KError::IoError as i32);
    let failed = save(&ns, "/doc", b"never lands");
    assert!(matches!(failed, Err(SaveError::Write(FileError::Io(_)))), "{failed:?}");
    assert_eq!(get(&ns, "/doc"), b"new, and longer");
    assert_eq!(save(&ns, "/doc", &alloc::vec![0u8; MAX_COPY as usize + 1]), Err(SaveError::Write(FileError::TooLarge)));
    assert_eq!(get(&ns, "/doc"), b"new, and longer");
}

#[test]
fn a_rename_moves_the_file_and_names_a_missing_source() {
    let ns = fresh();
    put(&ns, "/a", b"contents");
    rename_path(&ns, "/a", "/b", false).unwrap();
    assert_eq!(file_len(&ns, "/a"), None);
    assert_eq!(get(&ns, "/b"), b"contents");
    put(&ns, "/c", b"other");
    rename_path(&ns, "/c", "/b", true).unwrap();
    assert_eq!(get(&ns, "/b"), b"other");
    assert_eq!(rename_path(&ns, "/a", "/z", false), Err(FileError::NotFound));
    assert_eq!(rename(ns.raw().0, b"/b", b"/d", false), Ok(()));
    assert_eq!(file_len(&ns, "/d"), Some(5));
}

#[test]
fn a_rename_across_filesystems_is_the_cue_to_copy() {
    use libkern::error::KError;
    assert_eq!(rename_error(libos::Error::from(KError::Unsupported)), FileError::CrossDevice);
    assert_eq!(rename_error(libos::Error::from(KError::NotFound)), FileError::NotFound);
    assert_eq!(rename_error(libos::Error::from(KError::NoAccess)), FileError::Io(KError::NoAccess.as_i32()));
}

#[test]
fn creating_a_file_is_idempotent_and_keeps_what_it_holds() {
    let ns = fresh();
    ensure_file(&ns, "/f").unwrap();
    assert_eq!(file_len(&ns, "/f"), Some(0));
    put(&ns, "/f", b"kept");
    ensure_file(&ns, "/f").unwrap();
    assert_eq!(get(&ns, "/f"), b"kept");
}

#[test]
fn the_raw_forms_do_what_the_typed_ones_do() {
    let ns = fresh();
    let raw = ns.raw().0;
    write_file(raw, b"/f", b"raw bytes").unwrap();
    assert_eq!(read_file(raw, b"/f").unwrap(), b"raw bytes");
    assert_eq!(file_size(raw, b"/f"), Some(9));
    assert_eq!(copy_file(raw, b"/f", b"/g", false), Ok(9));
    assert_eq!(copy_file(raw, b"/f", b"/g", false), Err(FileError::Exists));
    assert_eq!(read_file(raw, b"/none"), Err(FileError::NotFound));
    create_file(raw, b"/e").unwrap();
    save_file(raw, b"/g", b"saved").unwrap();
    assert_eq!(get(&ns, "/g"), b"saved");
    assert_eq!(file_size(raw, b"/e"), Some(0));
    assert_eq!(unpadded(&get(&ns, "/g")), b"saved");
}

#[test]
fn a_path_that_is_not_text_resolves_to_nothing() {
    let ns = fresh();
    let raw = ns.raw().0;
    let bad: &[u8] = b"/\xff";
    assert_eq!(file_size(raw, bad), None);
    assert_eq!(read_file(raw, bad), Err(FileError::NotFound));
    assert_eq!(copy_file(raw, bad, b"/d", true), Err(FileError::NotFound));
    put(&ns, "/s", b"x");
    assert!(matches!(copy_file(raw, b"/s", bad, true), Err(FileError::Io(_))));
    assert!(matches!(write_file(raw, bad, b"x"), Err(FileError::Io(_))));
    assert!(matches!(rename(raw, b"/s", bad, false), Err(FileError::Io(_))));
    assert_eq!(lookup_wait(raw, bad, Rights::MAP_READ.bits()).1, 0);
}

#[test]
fn a_raw_lookup_gives_a_handle_the_caller_closes() {
    let ns = fresh();
    put(&ns, "/f", b"x");
    let raw = ns.raw().0;
    let (status, h) = lookup_wait(raw, b"/f", (Rights::MAP_READ | Rights::INSPECT).bits());
    assert_eq!(status, 0);
    assert!(mock::is_open(h), "the handle is the caller's now");
    // SAFETY: the mock's handle, which nothing else owns.
    drop(unsafe { Handle::<File, libos::MapRead>::from_raw(libkern::RawHandle(h), Rights::empty()) });
    assert!(!mock::is_open(h));
    let open = mock::open_handles();
    let (status, h) = lookup_wait(raw, b"/none", Rights::MAP_READ.bits());
    assert_eq!((status, h), (libkern::error::KError::NotFound.as_i32(), 0));
    assert_eq!(mock::open_handles(), open, "a refusal leaves nothing open");
}
