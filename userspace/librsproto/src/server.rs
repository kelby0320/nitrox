//! A resource server's side of a reply: the messages a server answers a forwarded request with,
//! built where they are sent and handed to the kernel without waiting.
//!
//! **A forwarded resolve waits for its answer with no deadline**, so a reply that cannot be sent
//! is answered with an error, and the handle it carried is closed. Each function says whether its
//! reply went; `false` means the requester was told no, or could not be told.

use crate::error::error_body;
use crate::file::DirReplyWriter;
use crate::namespace::{OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, resolve_reply, subnamespace_reply};
use crate::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, RS_MAX_BODY, encode_with};
use libkern::{KError, Rights};
use libos::{Carried, Channel, Error, Handle, MapRead, MapReadWrite, Memory, Msg, Namespace, Only, Ref};

const PAGE: usize = 4096;

/// Send one message on `ch` without blocking, moving `handles` on success. A refused send keeps
/// them in `handles`.
pub fn send<'e>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    op: u16,
    request_id: u64,
    flags: u32,
    body: &[u8],
    handles: &mut Carried,
) -> libos::Result<()> {
    send_with(ch.into(), op, request_id, flags, handles, |room| {
        room.get_mut(..body.len())?.copy_from_slice(body);
        Some(body.len())
    })
}

/// [`send`] with the body built in the message by `fill`, which answers its length.
fn send_with(
    ch: Ref<'_, Channel, Only>,
    op: u16,
    request_id: u64,
    flags: u32,
    handles: &mut Carried,
    fill: impl FnOnce(&mut [u8]) -> Option<usize>,
) -> libos::Result<()> {
    let mut msg = Msg::new();
    let n = encode_with(msg.payload_mut(), op, request_id, flags, handles.len() as u16, fill)
        .ok_or(Error::from(KError::InvalidArgument))?;
    msg.set_len(n)?;
    ch.send(&msg, handles)
}

/// Send `Meta::Ready` on `control` as `name`, moving `endpoint`, the end its supervisor binds. A
/// send that fails closes `endpoint`. NoBlock: a control channel's ring starts empty.
pub fn ready<'c>(control: impl Into<Ref<'c, Channel, Only>>, name: &[u8], endpoint: Handle<Channel, Only>) -> bool {
    let mut carried = Carried::new();
    carried.push(endpoint).is_ok()
        && send_with(control.into(), crate::OP_READY, 0, 0, &mut carried, |room| crate::meta::ready(room, name)).is_ok()
}

/// Answer request `op` with an error: `err`, and `why` if it fits, cut to fit if it does not.
pub fn reply_error<'e>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    op: u16,
    request_id: u64,
    err: KError,
    why: &[u8],
) -> bool {
    let flags = RS_FLAG_REPLY | RS_FLAG_ERROR;
    send_with(ch.into(), op, request_id, flags, &mut Carried::new(), |room| error_body(room, err.as_i32(), 0, why))
        .is_ok()
}

/// Answer a resolve with `body` and `handle`. A send that fails closes `handle` and answers
/// `KernelError`, so the resolve is not left waiting.
pub fn reply_handle<'e, T, M>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    request_id: u64,
    body: &[u8],
    handle: Handle<T, M>,
) -> bool {
    let ch = ch.into();
    let mut carried = Carried::new();
    if carried.push(handle).is_ok() && send(ch, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, body, &mut carried).is_ok() {
        return true;
    }
    drop(carried);
    reply_error(ch, OP_NS_RESOLVE, request_id, KError::KernelError, b"the reply could not be sent");
    false
}

/// Answer a resolve with a channel: the client's end of a subscription or a session.
pub fn reply_channel<'e>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    request_id: u64,
    client_end: Handle<Channel, Only>,
) -> bool {
    reply_handle(ch, request_id, &resolved(OBJECT_KIND_CHANNEL, 0), client_end)
}

/// Answer a resolve with a read-only memory object holding `bytes`. A create or a map that fails
/// answers `OutOfMemory`.
pub fn reply_object<'e>(ch: impl Into<Ref<'e, Channel, Only>>, request_id: u64, bytes: &[u8]) -> bool {
    let ch = ch.into();
    let Ok(len) = u32::try_from(bytes.len()) else {
        return reply_error(ch, OP_NS_RESOLVE, request_id, KError::InvalidArgument, b"");
    };
    match content_object(bytes) {
        Ok(object) => reply_handle(ch, request_id, &resolved(OBJECT_KIND_MEMOBJ, len), object),
        Err(err) => reply_error(ch, OP_NS_RESOLVE, request_id, err, b""),
    }
}

/// `bytes` in a fresh memory object that can only be mapped for reading, stated and moved: a
/// reader sizes a file by its stat. It is filled through a mapping that is gone before this
/// returns: one left would pin every object served.
fn content_object(bytes: &[u8]) -> Result<Handle<Memory, MapRead>, KError> {
    let size = bytes.len().max(1).div_ceil(PAGE) * PAGE;
    let mem = Handle::<Memory, MapReadWrite>::create(size).map_err(|_| KError::OutOfMemory)?;
    mem.map(size).and_then(|mut view| view.write_from(0, bytes)).map_err(|_| KError::OutOfMemory)?;
    let rights = Rights::MAP_READ | Rights::INSPECT | Rights::TRANSFER;
    mem.duplicate(rights).and_then(Handle::into_read_only).map_err(|_| KError::KernelError)
}

fn resolved(kind: u16, content_len: u32) -> [u8; RESOLVE_REPLY_LEN] {
    let mut body = [0; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, kind, content_len);
    body
}

/// Answer a resolve with `ns` as a continuation: the first `consumed` bytes of the suffix stand for
/// `base` in it. `InvalidArgument` if `consumed` is past `u16` or the body will not fit.
pub fn reply_subnamespace<'e, M>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    request_id: u64,
    ns: Ref<'_, Namespace, M>,
    consumed: usize,
    base: &[u8],
) -> bool {
    let ch = ch.into();
    let mut body = [0u8; RS_MAX_BODY];
    let built = u16::try_from(consumed).ok().and_then(|c| subnamespace_reply(&mut body, c, base));
    let Some(n) = built else {
        return reply_error(ch, OP_NS_RESOLVE, request_id, KError::InvalidArgument, b"");
    };
    match ns.duplicate(Rights::LOOKUP | Rights::TRANSFER) {
        Ok(dup) => reply_handle(ch, request_id, &body[..n], dup),
        Err(_) => reply_error(ch, OP_NS_RESOLVE, request_id, KError::KernelError, b""),
    }
}

/// An entry of a directory listing, as [`serve_read_dir`] writes it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ListedEntry<'a> {
    /// The name, at most [`MAX_NAME`](crate::file::MAX_NAME) bytes; a longer one is left out.
    pub name: &'a [u8],
    /// One of the `DIRENT_KIND_*` constants.
    pub kind: u8,
    /// The entry's size in bytes.
    pub size: u64,
}

/// Answer a `File::ReadDir` with the entries from `cursor` that fit one message, and the cursor
/// to resume at (`0` once the listing is done).
///
/// `entries(i)` is the `i`th entry, `None` past the end, so a listing built on demand builds only
/// the entries a page reaches. A cursor past the end is an empty page. Entry `i` is numbered
/// `i + 1`, with no mode or time.
pub fn serve_read_dir<'e, 'n>(
    ch: impl Into<Ref<'e, Channel, Only>>,
    request_id: u64,
    cursor: usize,
    mut entries: impl FnMut(usize) -> Option<ListedEntry<'n>>,
) -> bool {
    let sent = send_with(ch.into(), OP_FILE_READ_DIR, request_id, RS_FLAG_REPLY, &mut Carried::new(), |room| {
        let mut page = DirReplyWriter::new(room)?;
        let (mut at, mut next) = (cursor, 0);
        while let Some(e) = entries(at) {
            let inode = u32::try_from(at + 1).unwrap_or(u32::MAX);
            if !page.push(inode, e.kind, 0, e.size, 0, e.name) && !page.is_empty() {
                next = at as u64;
                break;
            }
            at += 1;
        }
        Some(page.finish(next))
    });
    sent.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::parse_error;
    use crate::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, parse_read_dir_reply};
    use crate::namespace::{OBJECT_KIND_SUBNAMESPACE, SUBNAMESPACE_PREFIX_LEN, parse_resolve_reply};
    use crate::{RS_MAX_PAYLOAD, Reader, decode};
    use libos::{MapExec, NsMutable, mock};
    use std::vec::Vec;

    type Pair = (Handle<Channel, Only>, Handle<Channel, Only>);

    /// The serve end, and the end a test reads each reply at.
    fn pair() -> Pair {
        mock::reset();
        Handle::pair(8).unwrap()
    }

    /// A reply as its peer receives it.
    struct Reply {
        op: u16,
        request_id: u64,
        flags: u32,
        handle_count: u16,
        body: Vec<u8>,
        handles: Carried,
        payload_len: usize,
    }

    fn read(peer: &Handle<Channel, Only>) -> Reply {
        let mut msg = Msg::new();
        let handles = peer.recv(&mut msg).unwrap();
        let m = decode(msg.payload()).unwrap();
        Reply {
            op: m.op,
            request_id: m.request_id,
            flags: m.flags,
            handle_count: m.handle_count,
            body: m.body.to_vec(),
            handles,
            payload_len: msg.payload().len(),
        }
    }

    fn nothing_queued(peer: &Handle<Channel, Only>) -> bool {
        peer.recv(&mut Msg::new()).err().map(|e| e.code()) == Some(KError::WouldBlock)
    }

    fn code<T>(r: libos::Result<T>) -> Option<KError> {
        r.err().map(|e| e.code())
    }

    const ERR: u32 = RS_FLAG_REPLY | RS_FLAG_ERROR;

    #[test]
    fn send_delivers_its_fields_and_moves_its_handles() {
        let (ch, peer) = pair();
        let (kept, far) = Handle::<Channel, Only>::pair(2).unwrap();
        let mut carried = Carried::new();
        carried.push(kept).ok().unwrap();
        send(&ch, 0x0123, 77, RS_FLAG_REPLY, b"body", &mut carried).unwrap();
        assert!(carried.is_empty(), "moved");
        let mut got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (0x0123, 77, RS_FLAG_REPLY, 1));
        assert_eq!(got.body, b"body");
        let moved: Handle<Channel, Only> = got.handles.take(0).unwrap();
        let mut msg = Msg::new();
        msg.set(b"hi").unwrap();
        moved.send(&msg, &mut Carried::new()).unwrap();
        assert_eq!(far.recv(&mut Msg::new()).map(drop).ok(), Some(()), "the handle that arrived is the one sent");
    }

    #[test]
    fn a_ready_names_its_server_and_moves_its_endpoint() {
        let (control, supervisor) = pair();
        let (endpoint, far) = Handle::<Channel, Only>::pair(2).unwrap();
        assert!(ready(&control, b"clipboard-server", endpoint));
        let mut got = read(&supervisor);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (crate::OP_READY, 0, 0, 1));
        assert_eq!(crate::meta::parse_ready(&got.body), Some(&b"clipboard-server"[..]));
        let moved: Handle<Channel, Only> = got.handles.take(0).unwrap();
        moved.send(&Msg::new(), &mut Carried::new()).unwrap();
        assert!(far.recv(&mut Msg::new()).is_ok(), "the endpoint that arrived is the one sent");
    }

    #[test]
    fn a_ready_that_cannot_go_closes_its_endpoint() {
        let (control, supervisor) = pair();
        let (endpoint, _far) = Handle::<Channel, Only>::pair(2).unwrap();
        let held = endpoint.raw().0;
        mock::fail_next_send(KError::WouldBlock.as_i32());
        assert!(!ready(&control, b"clipboard-server", endpoint));
        assert!(!mock::is_open(held) && nothing_queued(&supervisor));
        let (endpoint, _far) = Handle::<Channel, Only>::pair(2).unwrap();
        let held = endpoint.raw().0;
        let long = [b'n'; RS_MAX_PAYLOAD];
        assert!(!ready(&control, &long, endpoint), "a name no message holds");
        assert!(!mock::is_open(held) && nothing_queued(&supervisor));
        assert_eq!(mock::stray_closes(), 0);
    }

    #[test]
    fn send_refuses_a_body_too_long_for_a_message() {
        let (ch, peer) = pair();
        let body = [0u8; crate::RS_MAX_BODY + 1];
        let refused = send(&ch, 1, 1, 0, &body, &mut Carried::new());
        assert_eq!(refused.err().map(|e| e.code()), Some(KError::InvalidArgument));
        assert!(nothing_queued(&peer));
    }

    #[test]
    fn reply_error_arrives_with_its_code_and_reason() {
        let (ch, peer) = pair();
        assert!(reply_error(&ch, OP_NS_RESOLVE, 9, KError::NotFound, b"no such table"));
        let got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (OP_NS_RESOLVE, 9, ERR, 0));
        let e = parse_error(&got.body).unwrap();
        assert_eq!((e.kerror, e.server_code, e.msg), (KError::NotFound.as_i32(), 0, &b"no such table"[..]));
    }

    /// A reason of any length: the code always goes, the reason whole or cut to fit.
    #[test]
    fn reply_error_sizes_its_body_to_the_reason() {
        let (ch, peer) = pair();
        let reasons: [Vec<u8>; 3] = [Vec::new(), std::vec![b'r'; 108], std::vec![b'x'; RS_MAX_PAYLOAD + 500]];
        for reason in &reasons {
            assert!(reply_error(&ch, OP_NS_RESOLVE, 1, KError::TooLarge, reason));
            let got = read(&peer);
            assert_eq!(got.flags, ERR);
            let e = parse_error(&got.body).unwrap();
            assert_eq!(e.kerror, KError::TooLarge.as_i32(), "the code goes whatever the reason");
            let whole = reason.len().min(crate::RS_MAX_BODY - crate::error::ERROR_BODY_LEN);
            assert_eq!(e.msg, &reason[..whole], "whole, or cut to what one message holds");
            assert!(got.payload_len <= RS_MAX_PAYLOAD);
        }
    }

    #[test]
    fn reply_channel_carries_the_client_end() {
        let (ch, peer) = pair();
        let (client, far) = Handle::<Channel, Only>::pair(2).unwrap();
        assert!(reply_channel(&ch, 5, client));
        let mut got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (OP_NS_RESOLVE, 5, RS_FLAG_REPLY, 1));
        let r = parse_resolve_reply(&got.body).unwrap();
        assert_eq!((r.object_kind, r.content_len), (OBJECT_KIND_CHANNEL, 0));
        let end: Handle<Channel, Only> = got.handles.take(0).unwrap();
        end.send(&Msg::new(), &mut Carried::new()).unwrap();
        assert!(far.recv(&mut Msg::new()).is_ok());
    }

    /// A handle without `TRANSFER` is refused at the send: the resolve is answered with an error,
    /// and the handle closed once.
    #[test]
    fn a_reply_the_kernel_refuses_is_answered_with_an_error_and_closes_its_handle() {
        let (ch, peer) = pair();
        let open = mock::open_handles();
        let stuck = Handle::<Memory, MapReadWrite>::create(4096).unwrap().without_transfer().unwrap();
        let held = stuck.raw().0;
        assert!(!reply_handle(&ch, 3, &resolved(OBJECT_KIND_MEMOBJ, 1), stuck));
        let got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (OP_NS_RESOLVE, 3, ERR, 0));
        assert_eq!(parse_error(&got.body).unwrap().kerror, KError::KernelError.as_i32());
        assert!(!mock::is_open(held), "closed");
        assert_eq!(mock::open_handles(), open, "nothing leaked");
        assert_eq!(mock::stray_closes(), 0, "and nothing closed twice");
        assert!(nothing_queued(&peer), "one answer");
    }

    #[test]
    fn a_reply_to_a_peer_that_has_gone_is_false_and_leaves_nothing_open() {
        mock::reset();
        let none = mock::open_handles();
        let (ch, peer) = pair();
        drop(peer);
        let (client, _far) = Handle::<Channel, Only>::pair(2).unwrap();
        let held = client.raw().0;
        assert!(!reply_error(&ch, OP_NS_RESOLVE, 1, KError::NotFound, b"x"));
        assert!(!reply_channel(&ch, 1, client));
        assert!(!reply_object(&ch, 1, b"bytes"));
        assert!(!mock::is_open(held));
        drop((ch, _far));
        assert_eq!(mock::open_handles(), none);
        assert_eq!((mock::mappings(), mock::stray_closes()), (0, 0));
    }

    #[test]
    fn reply_object_hands_over_the_bytes_read_only() {
        let (ch, peer) = pair();
        let bytes: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        assert!(reply_object(&ch, 8, &bytes));
        assert_eq!(mock::mappings(), 0, "no mapping left to pin the object");
        let mut got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (OP_NS_RESOLVE, 8, RS_FLAG_REPLY, 1));
        let r = parse_resolve_reply(&got.body).unwrap();
        assert_eq!((r.object_kind, r.content_len), (OBJECT_KIND_MEMOBJ, 5000));

        let mem: Handle<Memory, MapRead> = got.handles.take(0).unwrap();
        let mut back = std::vec![0u8; bytes.len()];
        mem.map(bytes.len()).unwrap().read_into(0, &mut back).unwrap();
        assert_eq!(back, bytes);
        assert_eq!(code(mem.duplicate(Rights::MAP_READ)), Some(KError::NoAccess), "no duplicate");
        assert_eq!(mem.stat().map(|s| s.size).ok(), Some(8192), "a stat, for its size");
        let (onward, far) = Handle::<Channel, Only>::pair(2).unwrap();
        let mut carried = Carried::new();
        carried.push(mem).ok().unwrap();
        onward.send(&Msg::new(), &mut carried).unwrap();
        drop(far);
        assert_eq!(mock::stray_closes(), 0);
    }

    /// The handle that arrives is typed for more than it holds, so the kernel's refusal shows.
    #[test]
    fn a_replied_object_cannot_be_mapped_for_writing_or_execution() {
        let (ch, peer) = pair();
        assert!(reply_object(&ch, 1, b"table") && reply_object(&ch, 2, b"table"));
        let rw: Handle<Memory, MapReadWrite> = read(&peer).handles.take(0).unwrap();
        assert_eq!(code(rw.map(5)), Some(KError::NoAccess));
        let exec: Handle<Memory, MapExec> = read(&peer).handles.take(0).unwrap();
        assert_eq!(code(exec.map(5)), Some(KError::NoAccess));
    }

    #[test]
    fn reply_object_of_nothing_is_still_an_object() {
        let (ch, peer) = pair();
        assert!(reply_object(&ch, 1, b""));
        let mut got = read(&peer);
        assert_eq!(parse_resolve_reply(&got.body).unwrap().content_len, 0);
        assert!(got.handles.take::<Memory, MapRead>(0).is_some());
    }

    #[test]
    fn reply_object_that_cannot_be_created_answers_out_of_memory() {
        let (ch, peer) = pair();
        let open = mock::open_handles();
        assert!(reply_object(&ch, 2, &std::vec![0u8; (16 << 20) + 1]));
        let got = read(&peer);
        assert_eq!((got.flags, got.handle_count), (ERR, 0));
        assert_eq!(parse_error(&got.body).unwrap().kerror, KError::OutOfMemory.as_i32());
        assert_eq!((mock::open_handles(), mock::mappings()), (open, 0));
    }

    fn subnamespace_body(body: &[u8]) -> (u16, Vec<u8>) {
        let mut r = Reader::new(body);
        assert_eq!(r.u16(), Some(OBJECT_KIND_SUBNAMESPACE));
        r.skip(6).unwrap();
        let consumed = r.u16().unwrap();
        (consumed, r.lenpfx_u16().unwrap().to_vec())
    }

    #[test]
    fn reply_subnamespace_sends_a_lookup_duplicate_and_the_continuation() {
        let (ch, peer) = pair();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        assert!(reply_subnamespace(&ch, 4, ns.as_ref(), 12, b"/home/kelby"));
        let mut got = read(&peer);
        assert_eq!((got.op, got.request_id, got.flags, got.handle_count), (OP_NS_RESOLVE, 4, RS_FLAG_REPLY, 1));
        assert_eq!(subnamespace_body(&got.body), (12, b"/home/kelby".to_vec()));
        let dup: Handle<Namespace, NsMutable> = got.handles.take(0).unwrap();
        assert!(dup.copy().is_ok(), "LOOKUP");
        assert_eq!(code(dup.duplicate(Rights::LOOKUP)), Some(KError::NoAccess), "not DUPLICATE");
        assert_eq!(code(dup.bind("/x", &ns)), Some(KError::NoAccess), "not BIND");
        assert_eq!(code(dup.unbind("/x")), Some(KError::NoAccess), "not UNBIND");
        assert!(ns.copy().is_ok(), "the server's own handle is untouched");
    }

    #[test]
    fn reply_subnamespace_refuses_what_it_cannot_say_and_keeps_the_namespace() {
        let (ch, peer) = pair();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let open = mock::open_handles();
        let too_long = std::vec![b'/'; crate::RS_MAX_BODY - SUBNAMESPACE_PREFIX_LEN + 1];
        let cases: [(usize, &[u8]); 3] = [(65_536, b"/"), (usize::MAX, b"/"), (1, &too_long)];
        for (consumed, base) in cases {
            assert!(reply_subnamespace(&ch, 6, ns.as_ref(), consumed, base));
            let got = read(&peer);
            assert_eq!((got.flags, got.handle_count), (ERR, 0));
            assert_eq!(parse_error(&got.body).unwrap().kerror, KError::InvalidArgument.as_i32());
        }
        assert!(reply_subnamespace(&ch, 6, ns.as_ref(), 65_535, b"/"), "the largest consumed is allowed");
        assert_eq!(read(&peer).flags, RS_FLAG_REPLY);
        assert_eq!(mock::open_handles(), open, "no duplicate made for a refusal");
    }

    /// A namespace the server cannot pass on is answered, not left for the lookup to wait on.
    #[test]
    fn reply_subnamespace_of_a_namespace_it_cannot_duplicate_answers_an_error() {
        let (ch, peer) = pair();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        // SAFETY: a handle the mock has just made for the test, which nothing else holds.
        let held: Handle<Namespace, NsMutable> = unsafe {
            Handle::from_raw(libkern::RawHandle(mock::with_rights(ns.raw().0, Rights::LOOKUP)), Rights::empty())
        };
        let open = mock::open_handles();
        assert!(reply_subnamespace(&ch, 7, held.as_ref(), 1, b"/"));
        let got = read(&peer);
        assert_eq!((got.request_id, got.flags, got.handle_count), (7, ERR, 0));
        assert_eq!(parse_error(&got.body).unwrap().kerror, KError::KernelError.as_i32());
        assert_eq!(mock::open_handles(), open);
    }

    /// `n` entries named to fill several pages.
    fn names(n: usize) -> Vec<(Vec<u8>, u8, u64)> {
        (0..n)
            .map(|i| {
                let kind = if i % 7 == 0 { DIRENT_KIND_DIR } else { DIRENT_KIND_FILE };
                (std::format!("entry-{i:04}-{}", "n".repeat(60 + i % 40)).into_bytes(), kind, i as u64 * 1000)
            })
            .collect()
    }

    fn page(
        ch: &Handle<Channel, Only>,
        peer: &Handle<Channel, Only>,
        list: &[(Vec<u8>, u8, u64)],
        cursor: usize,
    ) -> Reply {
        let entry = |i: usize| list.get(i).map(|(name, kind, size)| ListedEntry { name, kind: *kind, size: *size });
        assert!(serve_read_dir(ch, 11 + cursor as u64, cursor, entry));
        read(peer)
    }

    #[test]
    fn serve_read_dir_pages_a_listing_larger_than_one_body() {
        let (ch, peer) = pair();
        let list = names(200);
        let (mut seen, mut cursor, mut pages) = (Vec::new(), 0usize, 0);
        loop {
            let got = page(&ch, &peer, &list, cursor);
            assert_eq!((got.op, got.request_id, got.flags), (OP_FILE_READ_DIR, 11 + cursor as u64, RS_FLAG_REPLY));
            assert!(got.body.len() <= crate::RS_MAX_BODY);
            let (head, entries) = parse_read_dir_reply(&got.body).unwrap();
            let before = seen.len();
            for e in entries {
                seen.push((e.name.to_vec(), e.kind, e.size, e.inode));
                assert_eq!((e.mode, e.mtime), (0, 0));
            }
            assert_eq!(seen.len() - before, head.entry_count as usize);
            assert!(seen.len() > before, "a page lists something while entries remain");
            pages += 1;
            if head.next_cursor == 0 {
                break;
            }
            assert_eq!(head.next_cursor as usize, seen.len(), "resumes at the first entry not sent");
            cursor = head.next_cursor as usize;
        }
        assert!(pages > 1, "the listing needed more than one page");
        let want: Vec<_> = list.iter().enumerate().map(|(i, (n, k, s))| (n.clone(), *k, *s, i as u32 + 1)).collect();
        assert_eq!(seen, want, "every entry once, in order");
    }

    #[test]
    fn serve_read_dir_answers_an_empty_listing_and_a_cursor_past_the_end_with_an_empty_page() {
        let (ch, peer) = pair();
        for (list, cursor) in [(names(0), 0), (names(3), 3), (names(3), 99)] {
            let got = page(&ch, &peer, &list, cursor);
            let (head, mut entries) = parse_read_dir_reply(&got.body).unwrap();
            assert_eq!((head.next_cursor, head.entry_count), (0, 0));
            assert!(entries.next().is_none());
        }
    }

    #[test]
    fn serve_read_dir_leaves_out_a_name_no_page_can_hold() {
        let (ch, peer) = pair();
        let mut list = names(3);
        list[0].0 = std::vec![b'a'; 256];
        let got = page(&ch, &peer, &list, 0);
        let (head, entries) = parse_read_dir_reply(&got.body).unwrap();
        assert_eq!(head.next_cursor, 0);
        assert_eq!(entries.map(|e| e.inode).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn serve_read_dir_asks_only_for_the_entries_a_page_reaches() {
        let (ch, peer) = pair();
        let list = names(200);
        let asked = core::cell::Cell::new(0usize);
        let entry = |i: usize| {
            asked.set(asked.get().max(i + 1));
            list.get(i).map(|(name, kind, size)| ListedEntry { name, kind: *kind, size: *size })
        };
        assert!(serve_read_dir(&ch, 1, 0, entry));
        let (head, _) = parse_read_dir_reply(&read(&peer).body).unwrap();
        assert_eq!(asked.get(), head.next_cursor as usize + 1, "one past the page, to see that it was full");
    }
}
