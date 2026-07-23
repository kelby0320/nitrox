//! `File` category (`op = 0x06xx`) bodies. Slice 8 defines `ReadRange` — the
//! Model-B page-cache fill: the kernel, demand-faulting a file-backed mapping,
//! asks the resource server for a byte range of a file and the server replies with
//! the bytes in a transferred `MemoryObject`. See `docs/spec/rsproto-file-ops.md`.
//!
//! `File` is the **file-content access** category — positioned, stateless reads of
//! a file resolved lazily (`Namespace::Resolve` with `RESOLVE_FILE_LAZY`, reply
//! `OBJECT_KIND_FILE`). It is deliberately distinct from `Stream` (`0x02`,
//! cursor-based streaming) and `Block` (`0x03`, extent/block-level — Model A's
//! future home).

use crate::{get_u16, get_u32, get_u64, put_u16, put_u32, put_u64};

// --- ReadRange request ------------------------------------------------------

/// Fixed prefix of a `ReadRangeRequest` (before the suffix bytes).
pub const READ_RANGE_REQUEST_PREFIX_LEN: usize = 16;

/// A parsed `ReadRangeRequest`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadRangeRequest<'a> {
    /// File byte offset of the range (page-aligned in the page-cache caller).
    pub offset: u64,
    /// Number of bytes requested (the page-cache caller asks at most one page).
    pub len: u32,
    /// The path suffix naming the file (UTF-8, no leading `/`) — the fill is
    /// stateless, so each `ReadRange` re-identifies its file by path.
    pub suffix: &'a [u8],
}

/// Write a `ReadRangeRequest` body; returns its length.
pub fn read_range_request(out: &mut [u8], offset: u64, len: u32, suffix: &[u8]) -> Option<usize> {
    if suffix.len() > u16::MAX as usize {
        return None;
    }
    let total = READ_RANGE_REQUEST_PREFIX_LEN + suffix.len();
    if out.len() < total {
        return None;
    }
    put_u64(out, 0, offset);
    put_u32(out, 8, len);
    put_u16(out, 12, suffix.len() as u16);
    put_u16(out, 14, 0);
    out[READ_RANGE_REQUEST_PREFIX_LEN..total].copy_from_slice(suffix);
    Some(total)
}

/// Parse a `ReadRangeRequest` body.
pub fn parse_read_range_request(body: &[u8]) -> Option<ReadRangeRequest<'_>> {
    if body.len() < READ_RANGE_REQUEST_PREFIX_LEN {
        return None;
    }
    let suffix_len = get_u16(body, 12) as usize;
    let end = READ_RANGE_REQUEST_PREFIX_LEN.checked_add(suffix_len)?;
    if body.len() < end {
        return None;
    }
    Some(ReadRangeRequest {
        offset: get_u64(body, 0),
        len: get_u32(body, 8),
        suffix: &body[READ_RANGE_REQUEST_PREFIX_LEN..end],
    })
}

// --- ReadRange reply (success) ----------------------------------------------

/// `ReadRangeReply` wire length (the filled bytes ride in `IpcMsg.handles[0]` as a
/// read-only `MemoryObject` of at most one page).
pub const READ_RANGE_REPLY_LEN: usize = 8;

/// A parsed success `ReadRangeReply`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadRangeReply {
    /// Valid bytes in `handles[0]` (≤ the requested `len`); the rest of the page,
    /// if any, is zero — a short tail at end-of-file.
    pub content_len: u32,
}

/// Write a success `ReadRangeReply` body; returns its length.
pub fn read_range_reply(out: &mut [u8], content_len: u32) -> Option<usize> {
    if out.len() < READ_RANGE_REPLY_LEN {
        return None;
    }
    put_u32(out, 0, content_len);
    put_u32(out, 4, 0);
    Some(READ_RANGE_REPLY_LEN)
}

/// Parse a success `ReadRangeReply` body.
pub fn parse_read_range_reply(body: &[u8]) -> Option<ReadRangeReply> {
    if body.len() < READ_RANGE_REPLY_LEN {
        return None;
    }
    Some(ReadRangeReply { content_len: get_u32(body, 0) })
}

// --- ReadDir (open-directory-handle enumeration) ----------------------------
//
// `ReadDir` is client-initiated on an **open directory handle** — a session channel the
// server minted, scoped to one directory (the fs-server mints it when a directory path resolves). The
// request names no path: the channel *is* the directory, so entries are addressed by name
// and there is nothing to escape confinement with. Listings that exceed one message
// paginate via an opaque `cursor` (0 = from the start; the reply returns the next cursor,
// 0 = no more).

/// Neutral directory-entry kind (independent of any on-disk `file_type` encoding).
pub const DIRENT_KIND_UNKNOWN: u8 = 0;
/// A regular file.
pub const DIRENT_KIND_FILE: u8 = 1;
/// A directory.
pub const DIRENT_KIND_DIR: u8 = 2;
/// A symbolic link.
pub const DIRENT_KIND_SYMLINK: u8 = 3;

/// `ReadDirRequest` wire length (fixed — the handle identifies the directory).
pub const READ_DIR_REQUEST_LEN: usize = 8;

/// A parsed `ReadDirRequest`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadDirRequest {
    /// Opaque resume position: `0` starts from the first entry; otherwise the
    /// `next_cursor` a prior reply returned. The server defines its meaning (for the
    /// ext4 server it is a byte offset into the directory's logical data).
    pub cursor: u64,
}

/// Write a `ReadDirRequest` body; returns its length.
pub fn read_dir_request(out: &mut [u8], cursor: u64) -> Option<usize> {
    if out.len() < READ_DIR_REQUEST_LEN {
        return None;
    }
    put_u64(out, 0, cursor);
    Some(READ_DIR_REQUEST_LEN)
}

/// Parse a `ReadDirRequest` body.
pub fn parse_read_dir_request(body: &[u8]) -> Option<ReadDirRequest> {
    if body.len() < READ_DIR_REQUEST_LEN {
        return None;
    }
    Some(ReadDirRequest { cursor: get_u64(body, 0) })
}

/// Fixed header of a `ReadDirReply` body, before the packed entries.
pub const READ_DIR_REPLY_HEADER_LEN: usize = 12;
/// Fixed prefix of each packed directory entry, before its name bytes:
/// `inode: u32`, `kind: u8`, `name_len: u8`, `_pad: u16`.
pub const DIR_ENTRY_PREFIX_LEN: usize = 8;

/// The header of a success `ReadDirReply` (the entries follow, packed).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadDirReplyHeader {
    /// Resume position for the next call; `0` means no further entries.
    pub next_cursor: u64,
    /// Number of entries packed in this reply.
    pub entry_count: u16,
}

/// One decoded directory entry (borrowing its name from the reply body).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DirEntry<'a> {
    pub inode: u32,
    pub kind: u8,
    pub name: &'a [u8],
}

/// Builds a `ReadDirReply` body incrementally, packing entries until the buffer is full.
/// The server appends entries with [`push`](DirReplyWriter::push) until one is rejected
/// (buffer full), then calls [`finish`](DirReplyWriter::finish) with the next cursor.
pub struct DirReplyWriter<'a> {
    buf: &'a mut [u8],
    len: usize,
    count: u16,
}

impl<'a> DirReplyWriter<'a> {
    /// Start a writer over `buf`, reserving the header. Returns `None` if `buf` cannot
    /// hold even the header.
    pub fn new(buf: &'a mut [u8]) -> Option<Self> {
        if buf.len() < READ_DIR_REPLY_HEADER_LEN {
            return None;
        }
        Some(Self { buf, len: READ_DIR_REPLY_HEADER_LEN, count: 0 })
    }

    /// Try to append one entry. Returns `false` (appending nothing) if it would not fit —
    /// the caller stops and resumes it in the next reply via the cursor.
    pub fn push(&mut self, inode: u32, kind: u8, name: &[u8]) -> bool {
        if name.len() > u8::MAX as usize {
            return false;
        }
        let need = DIR_ENTRY_PREFIX_LEN + name.len();
        if self.len + need > self.buf.len() {
            return false;
        }
        put_u32(self.buf, self.len, inode);
        self.buf[self.len + 4] = kind;
        self.buf[self.len + 5] = name.len() as u8;
        put_u16(self.buf, self.len + 6, 0);
        self.buf[self.len + DIR_ENTRY_PREFIX_LEN..self.len + need].copy_from_slice(name);
        self.len += need;
        self.count += 1;
        true
    }

    /// `true` if no entry has been packed yet (used to detect a single oversized entry).
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Finalise: stamp the header with `next_cursor` and the entry count, returning the
    /// total body length.
    pub fn finish(self, next_cursor: u64) -> usize {
        put_u64(self.buf, 0, next_cursor);
        put_u16(self.buf, 8, self.count);
        put_u16(self.buf, 10, 0);
        self.len
    }
}

/// Parse a `ReadDirReply` body into its header and an [`iterator`](DirEntryIter) over
/// entries.
pub fn parse_read_dir_reply(body: &[u8]) -> Option<(ReadDirReplyHeader, DirEntryIter<'_>)> {
    if body.len() < READ_DIR_REPLY_HEADER_LEN {
        return None;
    }
    let header = ReadDirReplyHeader {
        next_cursor: get_u64(body, 0),
        entry_count: get_u16(body, 8),
    };
    Some((
        header,
        DirEntryIter { body: &body[READ_DIR_REPLY_HEADER_LEN..], remaining: header.entry_count },
    ))
}

/// Iterator over the packed entries of a `ReadDirReply`. Yields `None` early (ending the
/// iteration) if the body is truncated mid-entry — a malformed reply drops its tail
/// rather than panicking.
pub struct DirEntryIter<'a> {
    body: &'a [u8],
    remaining: u16,
}

impl<'a> Iterator for DirEntryIter<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<DirEntry<'a>> {
        if self.remaining == 0 || self.body.len() < DIR_ENTRY_PREFIX_LEN {
            return None;
        }
        let name_len = self.body[5] as usize;
        let end = DIR_ENTRY_PREFIX_LEN + name_len;
        if self.body.len() < end {
            self.remaining = 0;
            return None;
        }
        let entry = DirEntry {
            inode: get_u32(self.body, 0),
            kind: self.body[4],
            name: &self.body[DIR_ENTRY_PREFIX_LEN..end],
        };
        self.body = &self.body[end..];
        self.remaining -= 1;
        Some(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_dir_request_round_trips() {
        let mut buf = [0u8; 16];
        let n = read_dir_request(&mut buf, 0x4000).unwrap();
        assert_eq!(n, READ_DIR_REQUEST_LEN);
        assert_eq!(parse_read_dir_request(&buf[..n]).unwrap(), ReadDirRequest { cursor: 0x4000 });
    }

    #[test]
    fn read_dir_reply_round_trips() {
        let mut buf = [0u8; 256];
        let mut w = DirReplyWriter::new(&mut buf).unwrap();
        assert!(w.push(2, DIRENT_KIND_DIR, b"."));
        assert!(w.push(2, DIRENT_KIND_DIR, b".."));
        assert!(w.push(11, DIRENT_KIND_FILE, b"hello.txt"));
        assert!(w.push(12, DIRENT_KIND_DIR, b"subdir"));
        assert!(!w.is_empty());
        let n = w.finish(0);

        let (h, iter) = parse_read_dir_reply(&buf[..n]).unwrap();
        assert_eq!(h.next_cursor, 0);
        assert_eq!(h.entry_count, 4);
        let got: Vec<_> = iter.map(|e| (e.inode, e.kind, e.name.to_vec())).collect();
        assert_eq!(got, vec![
            (2, DIRENT_KIND_DIR, b".".to_vec()),
            (2, DIRENT_KIND_DIR, b"..".to_vec()),
            (11, DIRENT_KIND_FILE, b"hello.txt".to_vec()),
            (12, DIRENT_KIND_DIR, b"subdir".to_vec()),
        ]);
    }

    #[test]
    fn writer_stops_when_full_and_cursor_resumes() {
        // A tiny buffer holds the header + exactly one 1-char-name entry.
        let mut buf = [0u8; READ_DIR_REPLY_HEADER_LEN + DIR_ENTRY_PREFIX_LEN + 1];
        let mut w = DirReplyWriter::new(&mut buf).unwrap();
        assert!(w.push(2, DIRENT_KIND_DIR, b"a"));
        assert!(!w.push(3, DIRENT_KIND_DIR, b"b"), "second entry must not fit");
        let n = w.finish(0x99);
        let (h, mut iter) = parse_read_dir_reply(&buf[..n]).unwrap();
        assert_eq!(h.next_cursor, 0x99, "a non-zero cursor signals more entries remain");
        assert_eq!(h.entry_count, 1);
        assert_eq!(iter.next().unwrap().name, b"a");
        assert!(iter.next().is_none());
    }

    #[test]
    fn parse_tolerates_a_truncated_tail() {
        // Header claims 2 entries but only one full entry's bytes are present.
        let mut buf = [0u8; 256];
        let mut w = DirReplyWriter::new(&mut buf).unwrap();
        w.push(2, DIRENT_KIND_DIR, b"only");
        let mut n = w.finish(0);
        // Forge the count to 2, then hand the parser a body cut mid-second-entry.
        super::put_u16(&mut buf, 8, 2);
        n += 3; // a few stray bytes, far short of a second entry
        let (h, iter) = parse_read_dir_reply(&buf[..n]).unwrap();
        assert_eq!(h.entry_count, 2);
        assert_eq!(iter.count(), 1, "the truncated second entry is dropped, not panicked on");
    }

    #[test]
    fn read_range_request_round_trips() {
        let mut buf = [0u8; 128];
        let n = read_range_request(&mut buf, 0x2000, 4096, b"system/big-file").unwrap();
        assert_eq!(n, READ_RANGE_REQUEST_PREFIX_LEN + 15);
        let r = parse_read_range_request(&buf[..n]).unwrap();
        assert_eq!(r.offset, 0x2000);
        assert_eq!(r.len, 4096);
        assert_eq!(r.suffix, b"system/big-file");
    }

    #[test]
    fn read_range_reply_round_trips() {
        let mut buf = [0u8; 16];
        let n = read_range_reply(&mut buf, 4096).unwrap();
        assert_eq!(n, READ_RANGE_REPLY_LEN);
        assert_eq!(parse_read_range_reply(&buf[..n]).unwrap(), ReadRangeReply { content_len: 4096 });
        // A short end-of-file tail.
        read_range_reply(&mut buf, 100).unwrap();
        assert_eq!(parse_read_range_reply(&buf).unwrap().content_len, 100);
    }

    #[test]
    fn parse_rejects_truncated() {
        assert!(parse_read_range_request(&[0u8; 8]).is_none());
        // suffix_len claims 50 bytes but the body is short.
        let mut buf = [0u8; 32];
        super::put_u16(&mut buf, 12, 50);
        assert!(parse_read_range_request(&buf).is_none());
        assert!(parse_read_range_reply(&[0u8; 4]).is_none());
    }
}
