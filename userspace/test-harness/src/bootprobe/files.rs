//! The filesystem tests: the page cache, writes, growth, creation and unlinked files.

use super::*;
use libkern::IoOp;
use libos::{MapReadWrite, Memory, Op, ReadOnly, ReadWrite, Resource, fs};
use librsproto::session::Dir;

// **These gate the verdict.** Each returns `bool` and the verdict is their conjunction; a bare
// `return` after a `FAIL` print would let a broken filesystem pass.
//
// **`subtree_bind_test`'s `/subtreetest` binding is made by `init` under `selftest`.** A
// test-only namespace bind is the one thing "data, not code" cannot express (a declared service
// gets `namespace: 0`, an inherited root), and the demo harness needs a binding that is *also*
// an openable directory to prove `move` refuses to recurse through a mount. The sharing it
// smoke-tests is also a kernel host test,
// `namespace::tests::one_registration_bound_twice_is_shared_not_duplicated`.

/// Size of the large-file fixture (`/system/large.bin`). MUST match the xtask generator
/// (`tools/xtask/src/rootfs.rs`). 32 KiB = 8 pages — past the old 64 KiB eager read cap, so
/// reading it proves the page cache lifts the cap. Kept small because each page demand-faults
/// through the stateless fs-server fill at ~325 ms/page under QEMU.
const LARGE_FILE_BYTES: usize = 32 * 1024;

/// The expected byte at file offset `i` of `/system/large.bin` — position-sensitive
/// (the page index `i >> 12` in the high part) so a mis-faulted page is detected.
/// MUST match the xtask generator.
fn fill_byte(i: usize) -> u8 {
    (((i >> 12) ^ i) & 0xFF) as u8
}

/// What the **device** holds, as opposed to what the page cache does — the root partition
/// opened raw, read through the ext4 library `fs-server-ext4` is built on.
///
/// **Why the filesystem checks need it.** Every resolve of a file shares one page-cache object,
/// so re-resolving reads the cache, not the disk: a check that proved a write "persisted" that
/// way would pass with no write-back at all.
pub(super) struct RootDevice {
    /// A page of scratch every read passes through, and its mapping.
    page: Mapping<MapReadWrite>,
    scratch: Handle<Memory, MapReadWrite>,
    device: Handle<Resource, ReadOnly>,
    /// The device's size in bytes, where its last read ends.
    capacity: u64,
}

impl RootDevice {
    /// The root partition, by the label this boot mounted it by — the disk image's or the live
    /// image's.
    pub(super) fn open(ns: Root<'_>) -> Option<RootDevice> {
        let labels = ["/dev/disk/by-partlabel/nitrox-root", "/dev/disk/by-partlabel/nitrox-live"];
        labels.iter().find_map(|p| RootDevice::on(ns, p))
    }

    /// Any device, by its path in `ns`: a disk the storage service mounted, read as the device
    /// holds it.
    pub(super) fn on(ns: Root<'_>, path: &str) -> Option<RootDevice> {
        let device = lookup::<Resource, ReadOnly, _>(ns, path, Rights::READ | Rights::INSPECT).ok()?;
        let capacity = device.stat().ok().map(|i| i.size).filter(|&c| c > 0)?;
        let scratch = Handle::<Memory, MapReadWrite>::create(PAGE as usize).ok()?;
        let page = scratch.map(PAGE as usize).ok()?;
        Some(RootDevice { page, scratch, device, capacity })
    }

    /// `len` bytes of the file at `path` from `offset`, as the device holds them. `None` if the
    /// file is shorter or does not read.
    pub(super) fn read_file(&self, path: &[u8], offset: u64, len: usize) -> Option<alloc::vec::Vec<u8>> {
        let mut out = alloc::vec![0u8; len];
        match fs_server_ext4::ext4::read_file_range(self, path, offset, len, &mut out) {
            Ok(n) if n == len => Some(out),
            _ => None,
        }
    }
}

impl fs_server_ext4::BlockReader for RootDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), fs_server_ext4::FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            // A page from a page boundary, or to the device's end, is whole blocks of any size up
            // to a page, which the kernel requires of a request; the device's own size goes unread.
            let start = at / PAGE * PAGE;
            let span = PAGE.min(self.capacity.saturating_sub(start));
            let intra = (at - start) as usize;
            if intra >= span as usize {
                return Err(fs_server_ext4::FsError::Io);
            }
            let take = (buf.len() - done).min(span as usize - intra);
            if block_on(self.device.read(&self.scratch, 0, start, span)) != Ok(span as usize) {
                return Err(fs_server_ext4::FsError::Io);
            }
            self.page.read_into(intra, &mut buf[done..done + take]).map_err(|_| fs_server_ext4::FsError::Io)?;
            done += take;
        }
        Ok(())
    }
}

/// The byte a pattern puts at `i` of page `p` — different per page, so a page served in the
/// other's place is caught.
fn c1_byte(p: u64, i: u64) -> u8 {
    (0xC1 + p as u8) ^ (i as u8)
}

/// **One object per file, and a dirty one kept until a sync.**
///
/// 1. **A write through one mapping is read through another without a sync**: two resolves
///    share the file's object, and the device does not have the write yet, so the second read
///    is a reading of the cache.
/// 2. **Unmapped and closed without a sync, the write is kept, and `sys_ns_sync` writes it**,
///    checked on the device on both sides of the sync.
/// 3. **A truncate and a grow of a file held open read zero over the regrown range** (a whole
///    page and a partial tail) through a new mapping and on the device. The mapping reads the
///    kernel's half (pages retired, the tail zeroed); the device the server's (what the grow
///    allocated, and the tail it kept, zeroed).
pub(super) fn file_cache_test(root: Root<'_>) -> bool {
    let path = "/system/c1-cache";
    let size = 2 * PAGE;
    let Some(dev) = RootDevice::open(root) else {
        log::write(b"boot-probe: c1 root device FAIL\n");
        return false;
    };
    let on_device = |off: u64, n: usize| dev.read_file(path.as_bytes(), off, n);
    let pattern = |p: u64, n: u64| (0..n).map(|i| c1_byte(p, i)).collect::<alloc::vec::Vec<u8>>();
    let zeroes = |n: usize| alloc::vec![0u8; n];

    let Ok(w) = block_on(fs::create::<_, MapReadWrite>(root, path, size, RW)) else {
        log::write(b"boot-probe: c1 create FAIL\n");
        return false;
    };
    let r = lookup::<File, MapRead, _>(root, path, Rights::MAP_READ);
    let (Ok(mut wa), Some((r, ra))) =
        (w.map(size as usize), r.ok().and_then(|r| r.map(size as usize).ok().map(|ra| (r, ra))))
    else {
        log::write(b"boot-probe: c1 map FAIL\n");
        return false;
    };
    for p in 0..2 {
        let _ = wa.write_from((p * PAGE) as usize, &pattern(p, 64));
    }
    let mut shared = true;
    for p in 0..2 {
        let mut seen = [0u8; 64];
        let _ = ra.read_into((p * PAGE) as usize, &mut seen);
        shared &= seen[..] == pattern(p, 64)[..];
    }
    let unwritten = on_device(0, 64) == Some(zeroes(64)) && on_device(PAGE, 64) == Some(zeroes(64));
    if shared && unwritten {
        log::write(b"boot-probe: c1 a write through one mapping reads through another, unsynced ok\n");
    } else {
        log::write(b"boot-probe: c1 shared-object MISMATCH\n");
    }

    // 2. Let go of everything without a sync; the write is kept, and `sys_ns_sync` writes it.
    drop(wa);
    drop(ra);
    drop(w);
    drop(r);
    let kept = on_device(0, 64) == Some(zeroes(64));
    let written = block_on(root.flush("/system"));
    let synced = written.is_ok_and(|n| n >= 1)
        && on_device(0, 64) == Some(pattern(0, 64))
        && on_device(PAGE, 64) == Some(pattern(1, 64));
    if kept && synced {
        log::write(b"boot-probe: c1 an unsynced write closed reaches the device on sys_ns_sync ok\n");
    } else {
        log::write(b"boot-probe: c1 ns-sync MISMATCH\n");
    }

    let h = lookup::<File, MapReadWrite, _>(root, path, RW);
    let Some((h, ha)) = h.ok().and_then(|h| h.map(size as usize).ok().map(|ha| (h, ha))) else {
        log::write(b"boot-probe: c1 reopen FAIL\n");
        return false;
    };
    // Faults both pages in.
    let resident = {
        let (mut a, mut b) = ([0u8; 1], [0u8; 1]);
        let _ = ha.read_into(1, &mut a);
        let _ = ha.read_into((PAGE + 1) as usize, &mut b);
        a[0] == c1_byte(0, 1) && b[0] == c1_byte(1, 1)
    };
    let truncated = block_on(fs::truncate::<_, MapReadWrite>(root, path, 10, RW)).is_ok();
    let g = block_on(fs::grow::<_, MapRead>(root, path, size, RW)).ok();
    let ga = g.as_ref().and_then(|g| g.map(size as usize).ok());
    let mut regrown = resident && truncated && ga.is_some();
    if let Some(ga) = &ga {
        let (mut first, mut second) = ([0u8; 64], [0u8; 64]);
        let _ = ga.read_into(0, &mut first);
        let _ = ga.read_into(PAGE as usize, &mut second);
        for i in 0..64 {
            regrown &= first[i] == if i < 10 { c1_byte(0, i as u64) } else { 0 } && second[i] == 0;
        }
    }
    let mut kept_head = pattern(0, 10);
    kept_head.extend_from_slice(&zeroes(54));
    let regrown_on_device = on_device(0, 64) == Some(kept_head) && on_device(PAGE, 64) == Some(zeroes(64));
    // Clean up: unmapped first, so the sync leaves the file clean and it goes with its handles.
    drop(ha);
    drop(ga);
    let _ = block_on(fs::flush(&h));
    drop(h);
    drop(g);
    if regrown && regrown_on_device {
        log::write(b"boot-probe: c1 a truncate then a grow reads zero, a page and a tail ok\n");
    } else {
        log::write(b"boot-probe: c1 truncate-grow MISMATCH\n");
    }
    shared && unwritten && kept && synced && regrown && regrown_on_device
}

/// **An unlinked file's pages are not written back.** A file
/// written through a mapping and let go without a sync stays dirty in the kernel's cache.
/// When it is unlinked, the server's `File::Forget` takes it out of that cache before the
/// server frees its block. So a `sys_ns_sync` after the unlink finds nothing of it to write,
/// and the block the file had does not receive its bytes — which, after a free, could be
/// another file's.
///
/// The block is found before the unlink, through the ext4 library over the raw partition,
/// and read raw after the sync.
pub(super) fn unlinked_file_test(root: Root<'_>) -> bool {
    let path = "/system/c1-unlinked";
    let Some(dev) = RootDevice::open(root) else {
        log::write(b"boot-probe: c1b root device FAIL\n");
        return false;
    };
    let Ok(w) = block_on(fs::create::<_, MapReadWrite>(root, path, PAGE, RW)) else {
        log::write(b"boot-probe: c1b create FAIL\n");
        return false;
    };
    let Ok(mut wa) = w.map(PAGE as usize) else {
        log::write(b"boot-probe: c1b map FAIL\n");
        return false;
    };
    let pattern: alloc::vec::Vec<u8> = (0..64u64).map(|i| 0xF0 ^ i as u8).collect();
    let _ = wa.write_from(0, &pattern);
    // Let go without a sync: dirty, and kept by the kernel.
    drop(wa);
    drop(w);

    let mut runs = [fs_server_ext4::BlockRun::default(); 4];
    let block_at = fs_server_ext4::ext4::map_file(&dev, path.as_bytes(), &mut runs)
        .ok()
        .filter(|m| m.runs == 1 && runs[0].device_lba != 0)
        .map(|m| runs[0].device_lba * m.block_size as u64);
    let raw = |at: u64| {
        let mut b = [0u8; 64];
        fs_server_ext4::BlockReader::read_at(&dev, at, &mut b).ok().map(|()| b)
    };
    let Some(block_at) = block_at else {
        log::write(b"boot-probe: c1b block FAIL\n");
        return false;
    };
    let unwritten = raw(block_at).is_some_and(|b| b[..] != pattern[..]);

    let mut msg = Msg::new();
    let unlinked = match Dir::resolve(root, "/system", &mut msg) {
        Ok(mut dir) => {
            let r = dir.unlink(b"c1-unlinked").is_ok();
            dir.close();
            r
        }
        Err(_) => false,
    };
    let synced = block_on(root.flush("/system")).is_ok();
    let not_written = raw(block_at).is_some_and(|b| b[..] != pattern[..]);
    if unwritten && unlinked && synced && not_written {
        log::write(b"boot-probe: c1b an unlinked file's pages are not written back ok\n");
        true
    } else {
        log::write(b"boot-probe: c1b unlinked-file MISMATCH\n");
        false
    }
}

/// **A directory session names its own entries, never `.` or `..`**: `..` renamed gave the parent a
/// name inside the directory, a way out of a subtree a session is confined to, and a touch of it
/// stamped the parent. Each is refused, and the directory, keeping its `..`, is removed after.
pub(super) fn dot_entries_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: dot entries FAIL: ").s(why).end();
        false
    };
    let mut sys_msg = Msg::new();
    let Ok(mut sys) = Dir::resolve(root, "/system", &mut sys_msg) else {
        return fail(b"/system does not open");
    };
    if sys.mkdir(b"probe-dots").is_err() {
        sys.close();
        return fail(b"the directory is not made");
    }
    let mut msg = Msg::new();
    let (renamed, touched, up) = match Dir::resolve(root, "/system/probe-dots", &mut msg) {
        Ok(mut d) => {
            let renamed = [&b".."[..], b"."].iter().any(|old| d.rename(old, b"up").is_ok());
            let touched = d.touch(b"..").is_ok() || d.touch(b".").is_ok();
            let mut up = false;
            let _ = d.read_dir(|e| {
                up |= e.name == b"up";
                true
            });
            d.close();
            (renamed, touched, up)
        }
        Err(_) => (true, true, true),
    };
    let removed = sys.rmdir(b"probe-dots").is_ok();
    sys.close();
    match (renamed, touched, up, removed) {
        (false, false, false, true) => {
            log::write(b"boot-probe: a session's rename and touch of . and .. refused ok\n");
            true
        }
        (true, ..) => fail(b"a rename of . or .. was taken"),
        (_, true, ..) => fail(b"a touch of . or .. was taken"),
        (.., true, _) => fail(b"the directory lists an entry named up"),
        _ => fail(b"the directory does not remove"),
    }
}

/// **A session ends with its directory**: it holds a bare id, and the next directory made can take
/// the removed one's inode, which the session then reached into. Directories are made until one
/// takes it, so the test reaches the reuse it is about; the old session is then refused, and the
/// new directory holds nothing it made.
pub(super) fn removed_dir_session_test(root: Root<'_>) -> bool {
    fn inode_of(dir: &mut Dir<'_>, name: &[u8]) -> u32 {
        let mut inode = 0;
        let _ = dir.read_dir(|e| {
            if e.name == name {
                inode = e.inode;
            }
            true
        });
        inode
    }
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: removed-directory session FAIL: ").s(why).end();
        false
    };
    let mut sys_msg = Msg::new();
    let Ok(mut sys) = Dir::resolve(root, "/system", &mut sys_msg) else {
        return fail(b"/system does not open");
    };
    let gone = match sys.mkdir(b"probe-gone") {
        Ok(()) => inode_of(&mut sys, b"probe-gone"),
        Err(_) => 0,
    };
    let mut stale_msg = Msg::new();
    let stale = Dir::resolve(root, "/system/probe-gone", &mut stale_msg);
    let removed = sys.rmdir(b"probe-gone").is_ok();
    let mut name = *b"probe-new-0";
    let mut made = 0u8;
    let mut reused = None;
    while removed && reused.is_none() && made < 10 && sys.mkdir(&name).is_ok() {
        made += 1;
        if inode_of(&mut sys, &name) == gone {
            reused = Some(name);
        }
        name[10] += 1;
    }
    let planted = match stale {
        Ok(mut d) => {
            let planted = d.mkdir(b"planted").is_ok();
            d.close();
            planted
        }
        Err(_) => false,
    };
    let mut held = false;
    if let Some(path) = reused.map(|n| {
        let mut p = *b"/system/probe-new-0";
        p[8..].copy_from_slice(&n);
        p
    }) {
        let mut msg = Msg::new();
        if let Ok(mut d) = Dir::resolve(root, core::str::from_utf8(&path).unwrap_or_default(), &mut msg) {
            held = inode_of(&mut d, b"planted") != 0;
            let _ = d.rmdir(b"planted");
            d.close();
        }
    }
    for k in 0..made {
        name[10] = b'0' + k;
        let _ = sys.rmdir(&name);
    }
    sys.close();
    match (gone != 0 && removed, reused.is_some(), planted || held) {
        (true, true, false) => {
            log::write(b"boot-probe: a session on a removed directory is ended before its id is reused ok\n");
            true
        }
        (false, ..) => fail(b"the directory was not made and removed"),
        (_, false, _) => fail(b"no new directory took the removed one's inode: the test saw no reuse"),
        _ => fail(b"the removed directory's session made an entry in the directory given its id"),
    }
}

/// **A flush of the root disk completes.** `IoOpcode::Flush` on the root partition goes down to
/// its disk (AHCI's `FLUSH CACHE EXT` in `test-qemu`) and its `PendingOperation` completes with
/// status `0`. Two refusals alongside, which a flush path that ignored its descriptor would
/// fail: one that names a range is `InvalidArgument`, and one on a read-only handle `NoAccess`.
pub(super) fn flush_test(root: Root<'_>) -> bool {
    use libkern::{IO_OPCODE_FLUSH, KError};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: flush FAIL: ").s(why).end();
        false
    };
    // The root partition, by the index `/dev/blk` serves it at: read-write, where the
    // `by-partlabel` names are read-only.
    let Some(records) = registry_records(root) else {
        return fail(b"the registry does not read");
    };
    let labels: [&[u8]; 2] = [b"nitrox-root", b"nitrox-live"];
    let Some(root_record) = records.iter().find(|r| labels.contains(&r.name())) else {
        return fail(b"no root partition in the registry");
    };
    let Some(index) = root_record.block_index() else {
        return fail(b"the root partition has no /dev/blk index");
    };
    let path = alloc::format!("/dev/blk/{index}");
    let Ok(dev) = lookup::<Resource, ReadWrite, _>(root, &path, Rights::READ | Rights::WRITE) else {
        return fail(b"the root partition does not open read-write");
    };
    let flush = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    let flushed = Op::submit(dev.raw(), &flush).is_ok_and(|op| block_on(op).is_ok());
    let ranged = IoOp { length: 512, ..flush };
    let range_refused = matches!(Op::submit(dev.raw(), &ranged), Err(e) if e.code() == KError::InvalidArgument);
    drop(dev);
    let read_only = ["/dev/disk/by-partlabel/nitrox-root", "/dev/disk/by-partlabel/nitrox-live"]
        .iter()
        .find_map(|p| lookup::<Resource, ReadOnly, _>(root, p, Rights::READ).ok());
    let ro_refused =
        read_only.is_some_and(|h| matches!(Op::submit(h.raw(), &flush), Err(e) if e.code() == KError::NoAccess));
    if !flushed {
        return fail(b"the flush did not complete");
    }
    if !range_refused {
        return fail(b"a flush naming a range was not refused");
    }
    if !ro_refused {
        return fail(b"a flush on a read-only handle was not refused");
    }
    log::write(b"boot-probe: flush of the root disk completed, and a ranged or read-only one refused ok\n");
    true
}

/// **A resolve continues in a namespace a server hands back.**
///
/// This probe is the server. It binds a channel of its own at `/cont` in a namespace it made, and
/// answers each lookup that reaches it with `OBJECT_KIND_SUBNAMESPACE`, pointing into a copy of the
/// root namespace, where the root filesystem is. Six things, each with its own operation:
/// 1. **a lookup** continues, and the file comes back from the root filesystem;
/// 2. **a create** carries its size change on, and the file exists at that size;
/// 3. **a rename** continues both of its paths, and the file moves;
/// 4. a rename whose destination **leaves the prefix** is `Unsupported`, and nothing moves;
/// 5. a server that keeps answering into itself is stopped at **depth four**, `TooLarge`, after
///    exactly five replies;
/// 6. a continuation onto **`/proc/self`** is `Unsupported`: it runs in the replying server's
///    syscall, where `/proc/self` would hand the caller the server's own `Process`.
pub(super) fn continuation_test(root: Root<'_>) -> bool {
    use core::future::Future;
    use core::task::{Context, Poll, Waker};
    use libkern::KError;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: c4 FAIL: ").s(why).end();
        false
    };
    // The probe's own server, at /cont in a namespace of its own.
    let a = Handle::<Namespace, NsMutable>::create();
    let channel = Handle::<Channel, Only>::pair(8);
    let (Ok(a), Ok((ours, theirs))) = (a, channel) else {
        return fail(b"no namespace or channel");
    };
    if a.bind_server("/cont", theirs.as_ref(), "").is_err() {
        return fail(b"binding /cont (the probe needs BIND_NAMESPACE)");
    }
    drop(theirs);
    // Where it answers into: a copy of the root namespace.
    let Ok(b) = root.copy() else {
        return fail(b"no copy of the root namespace");
    };

    // Serve requests on `ours` until the poll of an operation completes, answering each with
    // (`consumed`, `base`, `ns`); how many requests were answered.
    let serve_until = |op: &mut dyn FnMut(&mut Context<'_>) -> Poll<()>,
                       consumed: u16,
                       base: &[u8],
                       ns: &Handle<Namespace, NsMutable>|
     -> Option<u32> {
        let deadline = clock_ns() + 10_000_000_000;
        let mut answered = 0u32;
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if op(&mut cx).is_ready() {
                return Some(answered);
            }
            let req = receive(&ours, clock_ns() + 200_000_000);
            if let Some(req) = req {
                if req.op != librsproto::OP_NS_RESOLVE || !reply_subnamespace(&ours, req.request_id, consumed, base, ns)
                {
                    return None;
                }
                answered += 1;
            }
            if clock_ns() > deadline {
                return None;
            }
        }
    };
    let direct = read_path(root, "/system/current-generation");
    let mut lookup_fut =
        core::pin::pin!(a.lookup::<File, MapRead>("/cont/fs/l1/system/current-generation", Rights::MAP_READ));
    let mut looked = None;
    let answered = serve_until(&mut |cx| lookup_fut.as_mut().poll(cx).map(|r| looked = Some(r)), 5, b"/", &b);
    let looked_up = match (answered, looked) {
        (Some(1), Some(Ok(h))) => {
            let bytes = read_all(&h);
            bytes.is_some() && bytes == direct
        }
        _ => false,
    };
    if !looked_up {
        return fail(b"a lookup did not continue to the root filesystem's file");
    }

    let c = "/cont/fs/l1/system/c4-created";
    let mut create_fut = core::pin::pin!(fs::create::<_, MapReadWrite>(&a, c, 100, RW));
    let mut created = None;
    let answered = serve_until(&mut |cx| create_fut.as_mut().poll(cx).map(|r| created = Some(r)), 5, b"/", &b);
    let created = matches!((answered, created), (Some(1), Some(Ok(_))))
        && lookup::<File, MapRead, _>(root, "/system/c4-created", Rights::MAP_READ | Rights::INSPECT)
            .ok()
            .and_then(|h| h.stat().ok())
            .map(|i| i.size)
            == Some(100);
    if !created {
        return fail(b"a create did not carry its size on");
    }

    let d = "/cont/fs/l1/system/c4-renamed";
    let mut rename_fut = core::pin::pin!(fs::rename(&a, c, d, 0));
    let mut renamed = None;
    let answered = serve_until(&mut |cx| rename_fut.as_mut().poll(cx).map(|r| renamed = Some(r)), 5, b"/", &b);
    let renamed = matches!((answered, renamed), (Some(1), Some(Ok(()))))
        && lookup::<File, MapRead, _>(root, "/system/c4-created", Rights::MAP_READ).is_err()
        && lookup::<File, MapRead, _>(root, "/system/c4-renamed", Rights::MAP_READ).is_ok();
    if !renamed {
        return fail(b"a rename did not continue both paths");
    }

    // 4. A rename whose destination leaves the prefix answered for: another mount.
    let x = "/cont/fs/l2/system/c4-elsewhere";
    let mut left_fut = core::pin::pin!(fs::rename(&a, d, x, 0));
    let mut left = None;
    let answered = serve_until(&mut |cx| left_fut.as_mut().poll(cx).map(|r| left = Some(r)), 5, b"/", &b);
    let left = matches!((answered, left), (Some(1), Some(Err(e))) if e.code() == KError::Unsupported)
        && lookup::<File, MapRead, _>(root, "/system/c4-renamed", Rights::MAP_READ).is_ok();
    if !left {
        return fail(b"a rename leaving the prefix was not refused, or moved the file");
    }

    let mut loop_fut = core::pin::pin!(a.lookup::<File, MapRead>("/cont/loop/x", Rights::MAP_READ));
    let mut looped = None;
    let answered = serve_until(&mut |cx| loop_fut.as_mut().poll(cx).map(|r| looped = Some(r)), 4, b"/cont/loop", &a);
    let bounded = matches!((answered, looped), (Some(5), Some(Err(e))) if e.code() == KError::TooLarge);
    if !bounded {
        return fail(b"a resolve answering into itself was not stopped at depth four");
    }

    let mut self_fut = core::pin::pin!(a.lookup::<Resource, ReadOnly>("/cont/proc/self/process", Rights::INSPECT));
    let mut selfed = None;
    let answered = serve_until(&mut |cx| self_fut.as_mut().poll(cx).map(|r| selfed = Some(r)), 4, b"/proc", &b);
    let refused = matches!((answered, selfed), (Some(1), Some(Err(e))) if e.code() == KError::Unsupported);
    if !refused {
        return fail(b"a continuation onto /proc/self was not refused");
    }

    log::write(b"boot-probe: c4 a lookup, a create and a rename continue in a replied namespace; a rename leaving it, a loop past depth four and /proc/self refused ok\n");
    true
}

/// **Overwrite** an existing file in place through a `MAP_WRITE` mapping, `sys_file_sync`, then
/// read the block **off the device** ([`RootDevice`]) and verify the change persisted: the write
/// data path (dirty pages, write IRPs, device) with no fs-server metadata write.
pub(super) fn overwrite_test(root: Root<'_>) -> bool {
    let path = "/system/rwtest";
    let marker = [0xDEu8, 0xAD, 0xBE, 0xEF];

    let Ok(fh) = lookup::<File, MapReadWrite, _>(root, path, RW) else {
        log::write(b"boot-probe: rwtest lookup FAIL\n");
        return false;
    };
    let Ok(mut mapped) = fh.map(PAGE as usize) else {
        log::write(b"boot-probe: rwtest map FAIL\n");
        return false;
    };
    // Byte 8 is within the mapped page; read the original (== 8) to compare later.
    let mut orig8 = [0u8; 1];
    let _ = mapped.read_into(8, &mut orig8);
    // The write dirties the page.
    let _ = mapped.write_from(0, &marker);
    // 2. Flush the mapping's pages to disk (Model A write IRPs to the existing LBAs).
    // Unmapped before the sync, so the write-back can leave the file clean.
    drop(mapped);
    let synced = block_on(fs::flush(&fh));
    drop(fh);
    if synced.is_err() {
        log::write(b"boot-probe: rwtest sync FAIL\n");
    }

    // 3. Read the device and verify the overwrite persisted and the untouched byte did not
    //    change.
    let on_device = RootDevice::open(root).and_then(|d| d.read_file(path.as_bytes(), 0, 9));
    let ok = on_device.as_deref().is_some_and(|b| b[..4] == marker);
    let reread8 = on_device.as_deref().map(|b| b[8]);
    if ok && reread8 == Some(orig8[0]) {
        log::write(b"boot-probe: rwtest overwrite persisted + verified ok\n");
        true
    } else {
        log::write(b"boot-probe: rwtest overwrite MISMATCH\n");
        false
    }
}

/// **Grow** a file past EOF via `sys_file_grow`
/// (the fs-server allocates a block + extends its extent tree + updates the inode), write
/// into the newly-allocated region, `sys_file_sync`, then read the device and confirm the
/// appended data persisted — proving the write path's metadata mutation end to end.
pub(super) fn grow_test(root: Root<'_>) -> bool {
    let path = "/system/rwtest";
    let marker = [0xC0u8, 0xFF, 0xEEu8, 0x11];
    let new_size: u64 = 8000; // 4096 (1 block) → 8000 (2 blocks)

    let Ok(fh) = block_on(fs::grow::<_, MapReadWrite>(root, path, new_size, RW)) else {
        log::write(b"boot-probe: grow FAIL\n");
        return false;
    };

    // 2. Map the grown file; write a marker in the **new** region (the appended 2nd block).
    let Ok(mut mapped) = fh.map(new_size as usize) else {
        log::write(b"boot-probe: grow map FAIL\n");
        return false;
    };
    // Offset `PAGE` is in the 2nd mapped page (the appended block).
    let _ = mapped.write_from(PAGE as usize, &marker);
    // Unmapped before the sync, so the write-back can leave the file clean.
    drop(mapped);
    let synced = block_on(fs::flush(&fh));
    drop(fh);
    if synced.is_err() {
        log::write(b"boot-probe: grow sync FAIL\n");
    }

    // 3. Read the device — through the file's new extent, so the metadata is checked too —
    //    and verify the appended data.
    let ok = RootDevice::open(root)
        .and_then(|d| d.read_file(path.as_bytes(), PAGE, marker.len()))
        .is_some_and(|b| b == marker);
    if ok {
        log::write(b"boot-probe: grow appended a block + persisted + verified ok\n");
        true
    } else {
        log::write(b"boot-probe: grow MISMATCH\n");
        false
    }
}

/// **A copy into its own subtree is refused before it makes anything**: it made the destination,
/// listed the source, which now held that, and copied it again, until it was 32 levels deep.
pub(super) fn copy_into_itself_test(root: Root<'_>) -> bool {
    let made = libfs::make_dir(root, "/system/probe-tree").is_ok();
    let copied =
        libfs::copy_recursive(root, "/system/probe-tree", "/system/probe-tree/inner", false, &mut |_, _, _| {});
    let junk = libfs::is_directory(root, "/system/probe-tree/inner");
    let removed = libfs::remove_recursive(root, "/system/probe-tree", &mut |_, _| {}).is_ok();
    if made && copied == Err(libfs::TreeError::IntoItself) && !junk && removed {
        log::write(b"boot-probe: a copy into its own subtree refused, nothing made ok\n");
        return true;
    }
    let why = alloc::format!("made {made}, copied {copied:?}, left inner {junk}, removed {removed}");
    Line::new().s(b"boot-probe: copy into itself FAIL: ").s(why.as_bytes()).end();
    false
}

/// **A create the filesystem refuses fails with its refusal**: a name past ext4's 255 bytes,
/// `Unsupported`. The server answered only a read-only mount's refusal and served the rest, so the
/// file never made was looked up and answered `NotFound`.
pub(super) fn refused_create_test(root: Root<'_>) -> bool {
    let path = alloc::format!("/system/{}", "a".repeat(300));
    let made = block_on(fs::create::<_, MapReadWrite>(root, &path, 4096, RW));
    let status = status(&made);
    if status == libkern::KError::Unsupported.as_i32() {
        log::write(b"boot-probe: a create the filesystem refuses fails with its refusal ok\n");
        true
    } else {
        Line::new().s(b"boot-probe: refused create FAIL: status ").i(status as i64).end();
        false
    }
}

/// **Create** a brand-new file via
/// `sys_file_create` (the fs-server allocates an inode + inserts a directory entry in the
/// parent, then grows it to the target size), write into it, `sys_file_sync`, then read the
/// path off the device and confirm both that it resolves there and that its data persisted —
/// proving inode allocation + directory-entry insertion end to end.
pub(super) fn create_test(root: Root<'_>) -> bool {
    let path = "/system/created";
    let marker = [0xABu8, 0xCD, 0xEFu8, 0x42];
    let new_size: u64 = 4096; // fresh file → 1 block.

    // 1. Create-resolve: the fs-server creates the file, grows it, then replies its map.
    let Ok(fh) = block_on(fs::create::<_, MapReadWrite>(root, path, new_size, RW)) else {
        log::write(b"boot-probe: create FAIL\n");
        return false;
    };

    // 2. Map the new file; write a marker at the start.
    let Ok(mut mapped) = fh.map(new_size as usize) else {
        log::write(b"boot-probe: create map FAIL\n");
        return false;
    };
    let _ = mapped.write_from(0, &marker);
    // Unmapped before the sync, so the write-back can leave the file clean.
    drop(mapped);
    let synced = block_on(fs::flush(&fh));
    drop(fh);
    if synced.is_err() {
        log::write(b"boot-probe: create sync FAIL\n");
    }

    // 3. Read the device through the path: the directory entry is on disk (a path that did
    //    not exist before now resolves there) and so is the data.
    let ok =
        RootDevice::open(root).and_then(|d| d.read_file(path.as_bytes(), 0, marker.len())).is_some_and(|b| b == marker);
    if ok {
        log::write(b"boot-probe: create new file + persisted + verified ok\n");
        true
    } else {
        log::write(b"boot-probe: create MISMATCH\n");
        false
    }
}

/// Map the **large** file `/system/large.bin` lazily and read **every** byte: each first touch
/// of a page is a demand fault serviced by a `File::ReadRange` to the fs-server. The
/// position-sensitive content catches a mis-filled or mis-ordered page. Proves **multi-page
/// demand faulting** past the old 64 KiB cap.
pub(super) fn read_large_file(root: Root<'_>) -> bool {
    let Ok(fh) = lookup::<File, MapRead, _>(root, "/system/large.bin", Rights::MAP_READ) else {
        log::write(b"boot-probe: /system/large.bin lookup FAIL\n");
        return false;
    };
    // Map the whole file lazily (a FileBacked VMA — no frames until faulted).
    let Ok(mapped) = fh.map(LARGE_FILE_BYTES) else {
        log::write(b"boot-probe: large.bin map FAIL\n");
        return false;
    };
    let mut mismatches = 0u64;
    let mut page = [0u8; PAGE as usize];
    for start in (0..LARGE_FILE_BYTES).step_by(PAGE as usize) {
        // The first touch of each page faults; the rest of it is a plain read.
        let _ = mapped.read_into(start, &mut page);
        mismatches += page.iter().enumerate().filter(|&(i, &got)| got != fill_byte(start + i)).count() as u64;
    }
    let mut ok = false;
    if mismatches == 0 {
        Line::new()
            .s(b"boot-probe: large.bin verified ")
            .u(LARGE_FILE_BYTES as u64)
            .s(b" bytes across ")
            .u(LARGE_FILE_BYTES as u64 / PAGE)
            .s(b" demand-faulted pages ok")
            .end();
        ok = true;
    } else {
        Line::new().s(b"boot-probe: large.bin MISMATCH count=").u(mismatches).end();
    }
    ok
}
