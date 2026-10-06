//! **ext4 as a [`Volume`]** (Phase 6 Part E.1): what `libfsserver`'s server loop calls, over this
//! crate's library. Until the loop moved into `libfsserver` these calls were made from the
//! binary directly; the wrapper is all that is left of them here.

use crate::ext4::{self, Unservable};
use crate::{BlockReader, BlockRun, BlockWriter, FsError};
use libfsserver::{DirEntry, Mapped, Refusal, Volume};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, DIRENT_KIND_SYMLINK, DIRENT_KIND_UNKNOWN};

/// **An ext4 filesystem on `R`**: the read-write disk, or the `ReadOnly` over it a read-only
/// mount is served through.
pub struct Ext4<'a, R>(pub &'a R);

impl Refusal for Unservable {
    fn fs_error(&self) -> FsError {
        Unservable::fs_error(*self)
    }
}

/// Map an ext4 `ext4_dir_entry_2.file_type` to the neutral wire kind, falling back to the
/// inode's `i_mode` format bits when the directory entry does not carry a type (a
/// filesystem without the `filetype` feature stores `0` in every entry, and a listing that
/// reported everything as "unknown" would be useless).
fn map_kind(ext4_ft: u8, mode: u16) -> u8 {
    match ext4_ft {
        1 => DIRENT_KIND_FILE, // EXT4_FT_REG_FILE
        ext4::EXT4_FT_DIR => DIRENT_KIND_DIR,
        ext4::EXT4_FT_SYMLINK => DIRENT_KIND_SYMLINK,
        _ => match mode & 0xF000 {
            0x8000 => DIRENT_KIND_FILE,
            0x4000 => DIRENT_KIND_DIR,
            0xA000 => DIRENT_KIND_SYMLINK,
            _ => DIRENT_KIND_UNKNOWN,
        },
    }
}

/// An inode number, from a protocol id: ext4's ids are its inode numbers, which are `u32`.
fn ino(id: u64) -> Result<u32, FsError> {
    u32::try_from(id).map_err(|_| FsError::NotFound)
}

impl<R: BlockReader + BlockWriter> Volume for Ext4<'_, R> {
    const NAME: &'static [u8] = b"fs-server-ext4";
    const KIND: &'static [u8] = b"ext4";
    type Unservable = Unservable;

    fn read_only(&self) -> bool {
        self.0.read_only()
    }

    fn check(&self) -> Result<(), Unservable> {
        ext4::check_device(self.0)
    }

    fn state_unwritable() -> Unservable {
        Unservable::StateUnwritable
    }

    fn was_left_clean(&self) -> Result<bool, FsError> {
        ext4::was_left_clean(self.0)
    }

    fn mark_mounted(&self) -> Result<(), FsError> {
        ext4::mark_mounted(self.0)
    }

    fn mark_clean(&self) -> Result<(), FsError> {
        ext4::mark_clean(self.0)
    }

    fn map_file(&self, path: &[u8], runs: &mut [BlockRun]) -> Result<Mapped, FsError> {
        let m = ext4::map_file(self.0, path, runs)?;
        Ok(Mapped { size: m.size, block_size: m.block_size, runs: m.runs, id: m.ino as u64 })
    }

    fn read_file(&self, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
        ext4::read_file(self.0, path, out)
    }

    fn read_file_range(&self, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Result<usize, FsError> {
        ext4::read_file_range(self.0, path, offset, len, out)
    }

    fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError> {
        ext4::create_file(self.0, parent, name, now).map(drop)
    }

    fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<(), FsError> {
        ext4::grow_file(self.0, path, size, now).map(drop)
    }

    /// An ext4 file keeps its inode at any size, so a truncate ends no id.
    fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<Option<u64>, FsError> {
        ext4::truncate_file(self.0, path, size, now).map(|_| None)
    }

    fn resolve_dir(&self, path: &[u8]) -> Result<u64, FsError> {
        ext4::resolve_dir(self.0, path).map(u64::from)
    }

    fn read_dir(&self, dir: u64, cursor: u64, mut emit: impl FnMut(&DirEntry) -> bool) -> Result<u64, FsError> {
        ext4::read_dir_stat(self.0, ino(dir)?, cursor, |id, ft, name, st| {
            emit(&DirEntry { id, kind: map_kind(ft, st.mode), mode: st.mode, size: st.size, mtime: st.mtime, name })
        })
    }

    fn mkdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        ext4::mkdir_at(self.0, ino(dir)?, name, now)
    }

    fn unlink_at(&self, dir: u64, name: &[u8], now: i64) -> Result<Option<u64>, FsError> {
        ext4::unlink_at(self.0, ino(dir)?, name, now).map(|orphan| orphan.map(u64::from))
    }

    fn rmdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        ext4::rmdir_at(self.0, ino(dir)?, name, now)
    }

    fn touch_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        ext4::touch_at(self.0, ino(dir)?, name, now)
    }

    fn rename_at(&self, dir: u64, old: &[u8], new: &[u8], now: i64) -> Result<(), FsError> {
        ext4::rename_at(self.0, ino(dir)?, old, new, now)
    }

    fn rename_path(&self, old: &[u8], new: &[u8], replace: bool, now: i64) -> Result<Option<u64>, FsError> {
        ext4::rename_path(self.0, old, new, replace, now).map(|replaced| replaced.map(u64::from))
    }

    fn touch_file(&self, id: u64, now: i64) -> Result<(), FsError> {
        ext4::touch_file(self.0, ino(id)?, now)
    }

    fn release(&self, id: u64, now: i64) -> Result<(), FsError> {
        ext4::release_inode(self.0, ino(id)?, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libfsserver::serve::{MAX_REASON, Served, encode_refusal, serve, serve_resolve};
    use librsproto::namespace::{OBJECT_KIND_MEMOBJ, RESOLVE_FILE_LAZY};
    use librsproto::{OP_FILE_READ_RANGE, OP_NS_RESOLVE, RS_FLAG_ERROR, decode, encode};
    use libkern::KError;
    use crate::test_support::{ImageReader, fixture};
    use librsproto::file::{parse_read_range_reply, read_range_request};
    use librsproto::namespace::{RESOLVE_FILE_AS_MEMOBJ, parse_resolve_reply, resolve_request};
    use librsproto::error::parse_error;

    /// Build a `Namespace::Resolve` request for `suffix` (the kernel's wire form).
    fn make_request(request_id: u64, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len =
            resolve_request(&mut body, /*requested_rights*/ 0x8000, RESOLVE_FILE_AS_MEMOBJ, suffix)
                .unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_NS_RESOLVE, request_id, /*flags*/ 0, &body[..body_len], 0)
            .unwrap();
        (buf, n)
    }

    #[test]
    fn resolves_a_file_to_a_memobj_reply() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n"));
        let (req, req_len) = make_request(42, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve_resolve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::File { reply_len, content_len } => {
                assert_eq!(&content[..content_len], b"nitrox-gen-0001\n");
                // The reply is a success ResolveReply echoing the request id.
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_NS_RESOLVE);
                assert_eq!(m.request_id, 42);
                assert!(m.is_reply() && !m.is_error());
                assert_eq!(m.handle_count, 1);
                let rr = parse_resolve_reply(m.body).unwrap();
                assert_eq!(rr.object_kind, OBJECT_KIND_MEMOBJ);
                assert_eq!(rr.content_len as usize, content_len);
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn missing_path_yields_a_not_found_error_reply() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_request(7, b"system/nope");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve_resolve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 7);
                assert!(m.is_reply() && m.is_error());
                assert_eq!(m.handle_count, 0);
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    #[test]
    fn a_directory_is_not_a_regular_file() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_request(1, b"system"); // a directory
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("a directory must not resolve to a file"),
        }
    }

    #[test]
    fn a_non_resolve_op_is_unsupported() {
        let r = ImageReader(fixture(1024, b"x\n"));
        // A well-formed envelope with the wrong op (Ping).
        let mut buf = [0u8; 64];
        let n = encode(&mut buf, librsproto::OP_PING, 9, 0, &[], 0).unwrap();
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&Ext4(&r), &buf[..n], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 9);
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::Unsupported.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    #[test]
    fn a_garbage_request_replies_invalid_argument_id_zero() {
        let r = ImageReader(fixture(1024, b"x\n"));
        let garbage = [0u8; 8]; // too short / bad magic
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve_resolve(&Ext4(&r), &garbage, &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.request_id, 0); // unrecoverable id
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::InvalidArgument.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    /// Build a `RESOLVE_FILE_LAZY` resolve request (the slice-8 kernel's form).
    fn make_lazy_request(request_id: u64, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len =
            resolve_request(&mut body, 0x8000, RESOLVE_FILE_LAZY, suffix).unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_NS_RESOLVE, request_id, 0, &body[..body_len], 0).unwrap();
        (buf, n)
    }

    /// Build a `File::ReadRange` request (the page-cache fill's form).
    fn make_range_request(request_id: u64, offset: u64, len: u32, suffix: &[u8]) -> ([u8; 512], usize) {
        let mut body = [0u8; 256];
        let body_len = read_range_request(&mut body, offset, len, suffix).unwrap();
        let mut buf = [0u8; 512];
        let n = encode(&mut buf, OP_FILE_READ_RANGE, request_id, 0, &body[..body_len], 0).unwrap();
        (buf, n)
    }

    #[test]
    fn lazy_resolve_replies_block_map_and_device_handle() {
        use librsproto::namespace::OBJECT_KIND_FILE_BLOCKS;
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n")); // 16 bytes → 1 block
        let (req, req_len) = make_lazy_request(11, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::LazyBlocks { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_NS_RESOLVE);
                assert_eq!(m.request_id, 11);
                assert!(m.is_reply() && !m.is_error());
                assert_eq!(m.handle_count, 1); // the transferred device handle
                let body = m.body;
                let rr = parse_resolve_reply(body).unwrap();
                assert_eq!(rr.object_kind, OBJECT_KIND_FILE_BLOCKS);
                assert_eq!(rr.content_len, 16); // the file size
                let block_size = u32::from_le_bytes(body[8..12].try_into().unwrap());
                let run_count = u32::from_le_bytes(body[12..16].try_into().unwrap());
                assert_eq!(block_size, 1024);
                assert_eq!(run_count, 1);
                // The file's id is its inode — what the kernel keeps one object per, and what
                // it will touch by. Nothing here is read-only.
                let file_id = u64::from_le_bytes(body[16..24].try_into().unwrap());
                let flags = u32::from_le_bytes(body[24..28].try_into().unwrap());
                let mut runs = [crate::BlockRun::default(); 4];
                let ino = ext4::map_file(&r, b"/system/current-generation", &mut runs).unwrap().ino;
                assert!(ino > 2, "a file's inode, not the root's");
                assert_eq!(file_id, ino as u64);
                assert_eq!(flags, 0);
                // The single run, at 32, covers file block 0, non-hole, length 1.
                let file_block = u64::from_le_bytes(body[32..40].try_into().unwrap());
                let device_lba = u64::from_le_bytes(body[40..48].try_into().unwrap());
                let length = u32::from_le_bytes(body[48..52].try_into().unwrap());
                assert_eq!(file_block, 0);
                assert_eq!(length, 1);
                assert_ne!(device_lba, 0);
            }
            _ => panic!("expected a LazyBlocks reply"),
        }
    }

    /// **A read-only mount marks what it resolves** — the flag at 24 of the block-file body,
    /// which the kernel installs without `MAP_WRITE` — and a writable one does not. The mark
    /// comes from the reader the server serves through, the same one that refuses its writes.
    #[test]
    fn a_read_only_mount_marks_its_files_read_only() {
        let r = ImageReader(fixture(1024, b"nitrox-gen-0001\n"));
        let flags_of = |served: Served, reply: &[u8]| match served {
            Served::LazyBlocks { reply_len } => {
                let body = decode(&reply[..reply_len]).unwrap().body;
                u32::from_le_bytes(body[24..28].try_into().unwrap())
            }
            _ => panic!("expected a LazyBlocks reply"),
        };
        let (req, req_len) = make_lazy_request(12, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        let served = serve(&Ext4(&crate::ReadOnly(&r)), &req[..req_len], &mut content, &mut reply);
        assert_eq!(flags_of(served, &reply), librsproto::namespace::FILE_BLOCKS_READ_ONLY);
        let served = serve(&Ext4(&r), &req[..req_len], &mut content, &mut reply);
        assert_eq!(flags_of(served, &reply), 0);
    }

    #[test]
    fn read_range_serves_a_byte_window() {
        let content_bytes = b"0123456789ABCDEF\n"; // 17 bytes
        let r = ImageReader(fixture(1024, content_bytes));
        let (req, req_len) = make_range_request(22, 4, 6, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];

        match serve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::File { reply_len, content_len } => {
                assert_eq!(content_len, 6);
                assert_eq!(&content[..content_len], b"456789");
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_FILE_READ_RANGE);
                assert_eq!(m.request_id, 22);
                assert_eq!(m.handle_count, 1);
                let rr = parse_read_range_reply(m.body).unwrap();
                assert_eq!(rr.content_len, 6);
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn read_range_tail_clamps_at_eof() {
        let r = ImageReader(fixture(1024, b"ABCDEFG\n")); // 8 bytes
        // Ask a full page from offset 4 → only 4 bytes remain.
        let (req, req_len) = make_range_request(23, 4, 4096, b"system/current-generation");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::File { content_len, .. } => {
                assert_eq!(content_len, 4);
                assert_eq!(&content[..content_len], b"EFG\n");
            }
            _ => panic!("expected a File reply"),
        }
    }

    #[test]
    fn read_range_error_carries_the_read_range_op() {
        // A ReadRange for a missing file must reply with the ReadRange op so the
        // kernel routes the error to the pending fill (not a lookup) — else the
        // faulting thread would hang.
        let r = ImageReader(fixture(1024, b"x\n"));
        let (req, req_len) = make_range_request(24, 0, 4096, b"system/nope");
        let mut content = [0u8; ext4::MAX_FILE];
        let mut reply = [0u8; 4096];
        match serve(&Ext4(&r), &req[..req_len], &mut content, &mut reply) {
            Served::Error { reply_len } => {
                let m = decode(&reply[..reply_len]).unwrap();
                assert_eq!(m.op, OP_FILE_READ_RANGE);
                assert_eq!(m.request_id, 24);
                assert!(m.is_error());
                let e = parse_error(m.body).unwrap();
                assert_eq!(e.kerror, KError::NotFound.as_i32());
            }
            _ => panic!("expected an Error reply"),
        }
    }

    // ---- the refusal in place of Ready (Phase 5) ----

    use crate::ext4::Unservable;

    #[test]
    fn a_refusal_is_a_ready_with_the_error_flag_the_reason_and_no_handle() {
        let why = Unservable::NoMagic { found: 0 };
        let mut out = [0u8; 512];
        let n = encode_refusal(&mut out, &why).unwrap();
        let m = decode(&out[..n]).unwrap();
        assert_eq!(m.op, librsproto::OP_READY);
        assert_eq!(m.flags, RS_FLAG_ERROR, "an error, and not a reply to anything");
        assert_eq!(m.handle_count, 0, "a refusal hands over no endpoint");
        let e = parse_error(m.body).unwrap();
        assert_eq!(e.msg, format!("{why}").as_bytes());
        assert_eq!(e.kerror, KError::IoError as i32);
    }

    /// `MAX_REASON`'s doc says every reason fits: so no refusal the server sends is cut.
    #[test]
    fn every_reason_fits_a_refusal_uncut() {
        let all = [
            Unservable::Unreadable,
            Unservable::NoMagic { found: 0xFFFF },
            Unservable::Wide64Bit,
            Unservable::BlockTooLarge { log: u32::MAX },
            Unservable::ZeroField("s_inodes_per_group"),
            Unservable::RootUnreadable,
            Unservable::RootNotDirectory { mode: 0xFFFF },
            Unservable::StateUnwritable,
        ];
        for why in all {
            let text = format!("{why}");
            assert!(text.len() < MAX_REASON, "{} bytes: {text}", text.len());
            let mut out = [0u8; 512];
            let n = encode_refusal(&mut out, &why).unwrap();
            assert_eq!(parse_error(decode(&out[..n]).unwrap().body).unwrap().msg, text.as_bytes());
        }
    }
}
