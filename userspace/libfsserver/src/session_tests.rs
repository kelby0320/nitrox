//! `serve::session` against a volume that records what reaches it, so a refusal can be told apart
//! from one the filesystem would have made itself.

use super::*;
use crate::volume::Mapped;
use librsproto::error::parse_error;
use librsproto::file::{name_request, parse_read_dir_reply, read_dir_request, rename_request};
use librsproto::{RS_MAX_BODY, decode};
use std::cell::RefCell;
use std::string::String;
use std::vec::Vec;

/// What a session asked of the volume, each call as a line.
#[derive(Default)]
struct Spy {
    calls: RefCell<Vec<String>>,
    orphan: Option<u64>,
    entries: usize,
}

struct Never;

impl core::fmt::Display for Never {
    fn fmt(&self, _: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Ok(())
    }
}

impl Refusal for Never {
    fn fs_error(&self) -> FsError {
        FsError::Io
    }
}

impl Spy {
    fn log(&self, what: &str, name: &[u8]) {
        self.calls.borrow_mut().push(std::format!("{what} {}", String::from_utf8_lossy(name)));
    }
}

impl Volume for Spy {
    const NAME: &'static [u8] = b"spy";
    const KIND: &'static [u8] = b"spy";
    type Unservable = Never;
    fn read_only(&self) -> bool {
        false
    }
    fn check(&self) -> Result<(), Never> {
        Ok(())
    }
    fn state_unwritable() -> Never {
        Never
    }
    fn was_left_clean(&self) -> Result<bool, FsError> {
        Ok(true)
    }
    fn mark_mounted(&self) -> Result<(), FsError> {
        Ok(())
    }
    fn mark_clean(&self) -> Result<(), FsError> {
        Ok(())
    }
    fn map_file(&self, _: &[u8], _: &mut [BlockRun]) -> Result<Mapped, FsError> {
        Err(FsError::NotFound)
    }
    fn read_file(&self, _: &[u8], _: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::NotFound)
    }
    fn read_file_range(&self, _: &[u8], _: u64, _: usize, _: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::NotFound)
    }
    fn create_file(&self, _: &[u8], _: &[u8], _: i64) -> Result<(), FsError> {
        Err(FsError::NotFound)
    }
    fn grow_file(&self, _: &[u8], _: usize, _: i64) -> Result<u64, FsError> {
        Err(FsError::NotFound)
    }
    fn truncate_file(&self, _: &[u8], _: usize, _: i64) -> Result<Option<u64>, FsError> {
        Err(FsError::NotFound)
    }
    fn resolve_dir(&self, _: &[u8]) -> Result<u64, FsError> {
        Ok(7)
    }
    fn read_dir(&self, _: u64, _: u64, mut emit: impl FnMut(&DirEntry) -> bool) -> Result<u64, FsError> {
        let name = [b'n'; 200];
        for i in 0..self.entries {
            if !emit(&DirEntry { id: i as u32, kind: 1, mode: 0, size: 0, mtime: 0, name: &name }) {
                return Ok(i as u64 + 1);
            }
        }
        Ok(0)
    }
    fn mkdir_at(&self, _: u64, name: &[u8], _: i64) -> Result<(), FsError> {
        self.log("mkdir", name);
        Ok(())
    }
    fn unlink_at(&self, _: u64, name: &[u8], _: i64) -> Result<Option<u64>, FsError> {
        self.log("unlink", name);
        Ok(self.orphan)
    }
    fn rmdir_at(&self, _: u64, name: &[u8], _: i64, freeing: impl FnOnce(u64)) -> Result<(), FsError> {
        self.log("rmdir", name);
        freeing(99);
        Ok(())
    }
    fn touch_at(&self, _: u64, name: &[u8], _: i64) -> Result<(), FsError> {
        self.log("touch", name);
        Ok(())
    }
    fn rename_at(&self, _: u64, old: &[u8], new: &[u8], _: i64) -> Result<(), FsError> {
        self.log("rename", &[old, b" ", new].concat());
        Ok(())
    }
    fn rename_path(&self, _: &[u8], _: &[u8], _: bool, _: i64) -> Result<Option<u64>, FsError> {
        Err(FsError::NotFound)
    }
    fn touch_file(&self, _: u64, _: i64) -> Result<(), FsError> {
        Ok(())
    }
    fn release(&self, _: u64, _: i64) -> Result<(), FsError> {
        Ok(())
    }
}

/// One session request through `vol`: the reply's error code, or the reply's body; and the ids
/// handed to `release` and to `freeing`.
fn ask(vol: &Spy, op: u16, body: &[u8], reply_room: usize) -> (Result<Vec<u8>, i32>, Vec<u64>, Vec<u64>) {
    let mut req = [0u8; 4096];
    let len = encode(&mut req, op, 5, 0, body, 0).unwrap();
    let (mut reply, mut listing) = (vec![0u8; reply_room], [0u8; RS_MAX_BODY]);
    let (mut released, mut freed) = (Vec::new(), Vec::new());
    let n = session(vol, 7, &req[..len], &mut reply, &mut listing, 0, |id| released.push(id), |id| freed.push(id));
    let m = decode(&reply[..n.unwrap()]).unwrap();
    assert_eq!((m.op, m.request_id), (op, 5));
    let answer = if m.is_error() { Err(parse_error(m.body).unwrap().kerror) } else { Ok(m.body.to_vec()) };
    (answer, released, freed)
}

fn named(name: &[u8]) -> Vec<u8> {
    let mut body = [0u8; 64];
    let n = name_request(&mut body, name).unwrap();
    body[..n].to_vec()
}

fn renamed(old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut body = [0u8; 64];
    let n = rename_request(&mut body, old, new).unwrap();
    body[..n].to_vec()
}

/// **A session names no entry outside its directory**: an empty name, `.`, `..` or one holding a
/// `/` is refused before the filesystem is asked anything, whatever the filesystem would say.
#[test]
fn a_session_asks_its_volume_nothing_of_a_name_outside_its_directory() {
    let unsupported = Err(KError::Unsupported.as_i32());
    for bad in [&b""[..], b".", b"..", b"a/b", b"/"] {
        let vol = Spy::default();
        for op in [OP_FILE_MKDIR, OP_FILE_UNLINK, OP_FILE_RMDIR, OP_FILE_TOUCH] {
            assert_eq!(ask(&vol, op, &named(bad), 4096).0, unsupported, "op {op:#x} of {bad:?}");
        }
        assert_eq!(ask(&vol, OP_FILE_RENAME, &renamed(b"x", bad), 4096).0, unsupported, "to {bad:?}");
        assert_eq!(ask(&vol, OP_FILE_RENAME, &renamed(bad, b"x"), 4096).0, unsupported, "from {bad:?}");
        assert!(vol.calls.borrow().is_empty(), "{bad:?} reached the volume: {:?}", vol.calls.borrow());
    }
}

/// **Each session op reaches its own call**, an unlinked file's id goes to `release` and a removed
/// directory's to `freeing`, a listing comes back as entries, and one too long for the reply as an
/// error rather than nothing.
#[test]
fn each_session_op_reaches_its_call_and_answers() {
    let vol = Spy { orphan: Some(42), entries: 3, ..Spy::default() };
    for (op, body) in
        [(OP_FILE_MKDIR, named(b"m")), (OP_FILE_TOUCH, named(b"t")), (OP_FILE_RENAME, renamed(b"a", b"b"))]
    {
        assert_eq!(ask(&vol, op, &body, 4096).0, Ok(Vec::new()), "op {op:#x}");
    }
    assert_eq!(ask(&vol, OP_FILE_UNLINK, &named(b"u"), 4096), (Ok(Vec::new()), vec![42], vec![]));
    assert_eq!(ask(&vol, OP_FILE_RMDIR, &named(b"r"), 4096), (Ok(Vec::new()), vec![], vec![99]));
    assert_eq!(*vol.calls.borrow(), ["mkdir m", "touch t", "rename a b", "unlink u", "rmdir r"]);

    let mut body = [0u8; 16];
    let n = read_dir_request(&mut body, 0).unwrap();
    let listed = ask(&vol, OP_FILE_READ_DIR, &body[..n], 4096).0.unwrap();
    let (header, entries) = parse_read_dir_reply(&listed).unwrap();
    assert_eq!((header.entry_count, header.next_cursor, entries.count()), (3, 0, 3));
    let cramped = ask(&vol, OP_FILE_READ_DIR, &body[..n], 300).0;
    assert_eq!(cramped, Err(KError::KernelError.as_i32()), "a listing past the reply");
    assert_eq!(ask(&vol, 0x7777, &[], 4096).0, Err(KError::Unsupported.as_i32()), "an unknown op");
}
