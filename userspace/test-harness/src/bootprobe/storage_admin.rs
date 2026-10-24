//! The storage administration tests, with and without the grant.

use super::*;

/// **Unmounting and mounting, through an admin session**, opened as the
/// view broker will open one: the endpoint `/svc/storage/admin-endpoint` mints, bound at
/// `/dev/storage/admin` in a namespace the probe builds, and resolved there.
/// 1. `InUse` names the scratch disk, `init`'s root and the disk holding it, and not the ESP.
/// 2. **An unmount is refused while a file is held**, and the mount is left as it was.
/// 3. **A file written through a mapping and never synced survives an unmount**: the unmount's
///    write-back puts it on the device, which also says it was left clean. The label is gone
///    once unmounted, and a `Mount` by name brings the filesystem back, with the file, writable.
/// 4. Each refusal is its own: a hidden label, `init`'s root, a device already mounted, a FAT
///    device, an unknown label. A session endpoint reaches no admin endpoint.
/// 5. **A watch and a media session, opened through a session endpoint**: the watch is pinged by
///    the unmount and the remount, not by the refused unmount; the media session's `Eject` of the
///    scratch disk (a RAM disk, not removable) is `NoAccess`, of an unmounted name `NotFound`,
///    both leaving the mount as it was.
/// 6. **`Reread`'s refusals**: the mounted scratch disk and the disk holding `init`'s root,
///    `WouldBlock`; an unmounted partition of the disk the machine started from, `NoAccess`; an
///    unknown name, `NotFound`. None pings the watch.
/// 7. **And one taken**, where a USB disk holding an unmounted partition is attached (`test-qemu`'s
///    stick): rescanned, its partition departed and published again, and **the watch pinged
///    though no mount changed**, which is what `disk --partition` waits on. **Its old partition's
///    node, held across it**, refuses a `Rescan` `Unsupported` before and `PeerClosed` after,
///    its window retired.
/// 8. **Every other disk answers a `Rescan` `Unsupported`**: the SATA disk's node and the scratch
///    RAM disk's, sent one directly; and the scratch disk, unmounted, refused a `Reread` for it.
pub(super) fn storage_admin_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_FILE_CREATE, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::storage::{
        OP_STORAGE_EJECT, OP_STORAGE_IN_USE, OP_STORAGE_MOUNT, OP_STORAGE_REREAD, OP_STORAGE_UNMOUNT, build_mount,
        is_changed, parse_in_use, parse_mounted,
    };
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage admin: ").s(what).s(b" FAIL").end();
        false
    };
    let read = |ns: u64, path: &[u8]| {
        let (st, h) = ns_lookup(ns, path, RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        bytes
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;

    let table = || read(root_ns, b"/svc/storage/info/all.tsm").and_then(|b| Table::decode(&b).ok());
    let Some(t) = table() else {
        return fail(b"all.tsm would not read");
    };
    let col = |t: &Table, name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let text = |t: &Table, row: usize, c: Option<usize>| match c.map(|c| &t.rows[row][c]) {
        Some(Value::Str(s)) => Some(alloc::string::String::from(s.as_str())),
        _ => None,
    };
    let row_where =
        |t: &Table, c: &str, v: &str| (0..t.rows.len()).find(|&i| text(t, i, col(t, c)).as_deref() == Some(v));
    let (Some(scratch), Some(root), Some(esp)) =
        (row_where(&t, "label", "nitrox-scratch"), row_where(&t, "mounted", "/"), row_where(&t, "filesystem", "fat"))
    else {
        return fail(b"the table has no scratch disk, root or ESP to work with");
    };
    let names: alloc::vec::Vec<alloc::string::String> =
        [scratch, root, esp].iter().map(|&r| text(&t, r, col(&t, "name")).unwrap_or_default()).collect();
    let Some(registry) = registry_records(root_ns) else {
        return fail(b"the registry does not read");
    };
    let id_of =
        |name: &str| registry.iter().find(|r| r.block_index().is_some_and(|n| alloc::format!("blk-{n}") == name));
    let (Some(scratch_rec), Some(root_rec), Some(esp_rec)) = (id_of(&names[0]), id_of(&names[1]), id_of(&names[2]))
    else {
        return fail(b"a table name matches no registry record");
    };

    let (st, endpoint) = ns_lookup(root_ns, b"/svc/storage/admin-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no admin endpoint at /svc/storage/admin-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the admin endpoint into");
    }
    let ns = ns as u64;
    let at = b"/dev/storage/admin";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
    close(endpoint);
    let (st, admin) = if bound == 0 { ns_lookup(ns, at, chan) } else { (-1, 0) };
    close(ns);
    if st != 0 || admin == 0 {
        return fail(b"/dev/storage/admin opened no admin session");
    }
    let mut next = 0u64;
    let mut ask = |op: u16, body: &[u8]| -> Option<Received> {
        next += 1;
        if !rs_send(admin, op, next, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 30_000_000_000;
        loop {
            let m = receive(admin, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == next {
                return Some(m);
            }
        }
    };
    let refused = |m: &Option<Received>, err: KError| {
        m.as_ref().is_some_and(|m| {
            m.error && librsproto::error::parse_error(&m.body).is_some_and(|e| e.kerror == err.as_i32())
        })
    };
    let answered = |m: &Option<Received>| m.as_ref().is_some_and(|m| !m.error);
    let finish = |ok: bool| {
        close(admin);
        ok
    };

    // 0. **A watch and a media session, as a session opens them**: a resolve of
    //    `info/watch` and of `info/media` on a session endpoint, each answered with a channel.
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/storage/session-endpoint", chan);
    let open = |suffix: &[u8]| -> Option<u64> {
        let mut req = [0u8; 64];
        let n = librsproto::namespace::resolve_request(&mut req, chan, 0, suffix)?;
        if !rs_send(endpoint, librsproto::OP_NS_RESOLVE, 1, &req[..n], &[]) {
            return None;
        }
        let m = receive(endpoint, clock_ns() + 5_000_000_000)?;
        if m.error || m.handles.len() != 1 {
            m.handles.iter().for_each(|&h| close(h));
            return None;
        }
        Some(m.handles[0])
    };
    let (watch, media) =
        if st == 0 && endpoint != 0 { (open(b"info/watch"), open(b"info/media")) } else { (None, None) };
    close(endpoint);
    let (Some(watch), Some(media)) = (watch, media) else {
        watch.into_iter().chain(media).for_each(close);
        return finish(fail(b"a session endpoint opened no watch, or no media session"));
    };
    let finish = |ok: bool| {
        close(watch);
        close(media);
        finish(ok)
    };
    // **Whether the watch was pinged**, waiting until `deadline` for the first — `0` only looks —
    // and taking every ping queued behind it.
    let pinged = |deadline: u64| {
        let mut any = false;
        while let Some(m) = receive(watch, if any { 0 } else { deadline }) {
            m.handles.iter().for_each(|&h| close(h));
            any |= is_changed(m.op, &m.body);
        }
        any
    };
    pinged(0);

    let mut ids = alloc::vec::Vec::new();
    let in_use = ask(OP_STORAGE_IN_USE, &[]);
    if !in_use.as_ref().is_some_and(|m| !m.error && parse_in_use(&m.body, |id| ids.push(id))) {
        return finish(fail(b"InUse was not answered"));
    }
    let disk_of_root = root_rec.parent;
    if !(ids.contains(&scratch_rec.id) && ids.contains(&root_rec.id) && ids.contains(&disk_of_root))
        || ids.contains(&esp_rec.id)
    {
        return finish(fail(b"InUse is not the scratch disk, the root and its disk, without the ESP"));
    }

    let (st, held) = ns_lookup(root_ns, b"/svc/storage/fs/nitrox-scratch/README", RIGHT_MAP_READ);
    if st != 0 || held == 0 {
        return finish(fail(b"the README would not open to hold"));
    }
    let busy = ask(OP_STORAGE_UNMOUNT, b"nitrox-scratch");
    close(held);
    if !refused(&busy, KError::WouldBlock) {
        return finish(fail(b"an unmount with a file held was not refused WouldBlock"));
    }
    // **No ping for a refusal**: the service pings at the end of the turn that changed the mounts,
    // so one owed for that turn is queued before the reply to any later request — and `InUse`'s
    // reply being here means a look at the watch now would find it.
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return finish(fail(b"an unmount refused pinged the watch, or InUse was not answered"));
    }
    if read(root_ns, b"/svc/storage/fs/nitrox-scratch/README").as_deref() != Some(SCRATCH_README) {
        return finish(fail(b"a refused unmount did not leave the mount as it was"));
    }

    const LEN: u64 = 2 * PAGE + 17;
    let Some(fh) = file_resize(SYS_FILE_CREATE, root_ns, b"/svc/storage/fs/nitrox-scratch/unsynced", LEN) else {
        return finish(fail(b"a file would not create to write unsynced"));
    };
    let Some(addr) = map_file(fh, LEN, RIGHT_MAP_READ | RIGHT_MAP_WRITE) else {
        close(fh);
        return finish(fail(b"that file would not map writable"));
    };
    let pattern = |i: u64| (i * 13 + 5) as u8;
    for i in 0..LEN {
        // SAFETY: `LEN` bytes are mapped writable at `addr`.
        unsafe { ((addr + i) as *mut u8).write_volatile(pattern(i)) };
    }
    // SAFETY: unmapping this call's own mapping; the handle goes too, with no sync.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr, 0) };
    close(fh);

    // 4. Unmount, then the label is gone — and the watch is told.
    if !answered(&ask(OP_STORAGE_UNMOUNT, b"nitrox-scratch")) {
        return finish(fail(b"the unmount was not answered"));
    }
    if !pinged(clock_ns() + 5_000_000_000) {
        return finish(fail(b"the unmount did not ping the watch"));
    }
    let (st, gone) = ns_lookup(root_ns, b"/svc/storage/fs/nitrox-scratch/README", RIGHT_MAP_READ);
    close(gone);
    if st != KError::NotFound.as_i32() {
        return finish(fail(b"the label still resolved once unmounted"));
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mounted"))))
        != Some(None)
    {
        return finish(fail(b"the table still shows the scratch disk mounted"));
    }

    let Some(raw) = RootDevice::on(root_ns, alloc::format!("/dev/blk/{}", scratch_rec.served).as_bytes()) else {
        return finish(fail(b"the scratch disk's device would not open raw"));
    };
    let want: alloc::vec::Vec<u8> = (0..LEN).map(pattern).collect();
    let on_disk = raw.read_file(b"/unsynced", 0, LEN as usize);
    let clean = fs_server_ext4::ext4::was_left_clean(&raw);
    drop(raw);
    if on_disk.as_deref() != Some(&want[..]) {
        return finish(fail(b"what was written and never synced is not on the device after the unmount"));
    }
    if clean != Ok(true) {
        return finish(fail(b"the unmounted filesystem was not left clean"));
    }

    // 5b. **A `Reread` of the scratch disk, unmounted**: a RAM disk, which the kernel cannot rescan,
    //     so `Unsupported` — the one disk this boot can ask that of.
    let reread = ask(OP_STORAGE_REREAD, names[0].as_bytes());
    if !refused(&reread, KError::Unsupported) {
        let said = reread.as_ref().and_then(|m| librsproto::error::parse_error(&m.body));
        Line::new()
            .s(b"boot-probe: storage admin: the scratch disk's Reread answered ")
            .i(said.as_ref().map_or(0, |e| e.kerror as i64))
            .s(b": ")
            .untrusted(said.as_ref().map_or(&b""[..], |e| e.msg))
            .end();
        return finish(fail(b"a Reread of the unmounted scratch disk, a RAM disk, was not refused Unsupported"));
    }

    // 6. Mounted again by name — once refused a hidden label, the one moment a mountable device
    //    exists to refuse it on.
    let mut body = [0u8; 64];
    let n = build_mount(&mut body, names[0].as_bytes(), b".hidden").unwrap_or(0);
    if !refused(&ask(OP_STORAGE_MOUNT, &body[..n]), KError::InvalidArgument) {
        return finish(fail(b"a hidden label was not refused InvalidArgument"));
    }
    let n = build_mount(&mut body, names[0].as_bytes(), b"").unwrap_or(0);
    let again = ask(OP_STORAGE_MOUNT, &body[..n]);
    let mounted = |m: &Received| parse_mounted(&m.body) == Some((&b"nitrox-scratch"[..], None));
    if !again.as_ref().is_some_and(|m| !m.error && mounted(m)) {
        return finish(fail(b"a Mount by name did not bring nitrox-scratch back"));
    }
    if !pinged(clock_ns() + 5_000_000_000) {
        return finish(fail(b"the remount did not ping the watch"));
    }
    if read(root_ns, b"/svc/storage/fs/nitrox-scratch/unsynced").as_deref() != Some(&want[..]) {
        return finish(fail(b"the remounted filesystem does not hold the unsynced file"));
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mode"))))
        != Some(Some(alloc::string::String::from("rw")))
    {
        return finish(fail(b"the remount is not writable"));
    }

    let mount = |ask: &mut dyn FnMut(u16, &[u8]) -> Option<Received>, device: &str, label: &[u8]| {
        let mut body = [0u8; 96];
        let n = build_mount(&mut body, device.as_bytes(), label).unwrap_or(0);
        ask(OP_STORAGE_MOUNT, &body[..n])
    };
    let cases: [(&[u8], bool); 4] = [
        (b"init's root", refused(&mount(&mut ask, &names[1], b""), KError::AlreadyExists)),
        (b"the mounted scratch disk", refused(&mount(&mut ask, &names[0], b""), KError::AlreadyExists)),
        (b"the ESP", refused(&mount(&mut ask, &names[2], b""), KError::Unsupported)),
        (b"an unknown label", refused(&ask(OP_STORAGE_UNMOUNT, b"no-such-label"), KError::NotFound)),
    ];
    for (what, ok) in cases {
        if !ok {
            Line::new().s(b"boot-probe: storage admin: refusing ").s(what).s(b" was not the right error").end();
            return finish(fail(b"a refusal"));
        }
    }

    let (st, session) = ns_lookup(root_ns, b"/svc/storage/session-endpoint", chan);
    if st != 0 || session == 0 {
        return finish(fail(b"no session endpoint"));
    }
    let mut req = [0u8; 64];
    let n = librsproto::namespace::resolve_request(&mut req, chan, 0, b"admin-endpoint").unwrap_or(0);
    let asked = rs_send(session, librsproto::OP_NS_RESOLVE, 1, &req[..n], &[]);
    let reply = if asked { receive(session, clock_ns() + 5_000_000_000) } else { None };
    close(session);
    if let Some(m) = &reply {
        m.handles.iter().for_each(|&h| close(h));
    }
    if !refused(&reply, KError::NotFound) {
        return finish(fail(b"a session endpoint answered admin-endpoint"));
    }

    // 9. **A session's eject** of the scratch disk, a RAM disk: refused `NoAccess`, since an
    //    internal disk is the `storage` grant's; a name nothing is mounted under, `NotFound`.
    let eject = |id: u64, name: &[u8]| {
        if !rs_send(media, OP_STORAGE_EJECT, id, name, &[]) {
            return None;
        }
        let m = receive(media, clock_ns() + 30_000_000_000)?;
        m.handles.iter().for_each(|&h| close(h));
        Some(m)
    };
    if !refused(&eject(1, b"nitrox-scratch"), KError::NoAccess) {
        return finish(fail(b"a session's eject of the scratch disk was not refused NoAccess"));
    }
    if !refused(&eject(2, b"no-such-label"), KError::NotFound) {
        return finish(fail(b"a session's eject of an unknown name was not refused NotFound"));
    }
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return finish(fail(b"an eject refused pinged the watch"));
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mode"))))
        != Some(Some(alloc::string::String::from("rw")))
    {
        return finish(fail(b"a refused eject unmounted the scratch disk"));
    }

    // 10. **`Reread`'s refusals**. The disk the machine started from is found by its flag, and
    //     its partition by its parent — never "the first FAT", which on some machines is an
    //     internal disk's, that a `Reread` would take.
    let blk = |r: &libkern::device::DeviceRecord| alloc::format!("blk-{}", r.served);
    let root_disk = registry.iter().find(|r| r.id == root_rec.parent);
    let mounted_ids: alloc::vec::Vec<u32> = ids.clone();
    let boot_part = registry.iter().find(|r| {
        r.kind() == libkern::device::DeviceKind::Partition
            && !r.is_departed()
            && registry.iter().any(|d| d.id == r.parent && d.is_boot_medium())
            && !mounted_ids.contains(&r.id)
    });
    let (Some(root_disk), Some(boot_part)) = (root_disk, boot_part) else {
        return finish(fail(
            b"no disk holding init's root, or no unmounted partition of the boot disk, to ask Reread of",
        ));
    };
    let rereads: [(&[u8], alloc::string::String, KError); 4] = [
        (b"the mounted scratch disk", names[0].clone(), KError::WouldBlock),
        (b"the disk holding init's root", blk(root_disk), KError::WouldBlock),
        (b"a partition of the boot disk", blk(boot_part), KError::NoAccess),
        (b"a name nothing has", alloc::string::String::from("blk-999"), KError::NotFound),
    ];
    for (what, name, err) in rereads {
        if !refused(&ask(OP_STORAGE_REREAD, name.as_bytes()), err) {
            Line::new().s(b"boot-probe: storage admin: a Reread of ").s(what).s(b" was not the right error").end();
            return finish(fail(b"a Reread refusal"));
        }
    }
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return finish(fail(b"a refused Reread pinged the watch"));
    }

    // 11. **A disk read again**: its old partition record departs and a new one arrives, with no
    //     mount on either — the change a watch is pinged for.
    let stick = registry.iter().find(|r| {
        r.kind() == libkern::device::DeviceKind::Disk
            && r.driver() == b"usb-storage"
            && !r.is_boot_medium()
            && !r.is_departed()
            && registry
                .iter()
                .any(|p| p.parent == r.id && p.kind() == libkern::device::DeviceKind::Partition && !p.is_departed())
    });
    // **And every other disk refuses a `Rescan` sent to it directly**: the SATA disk's AHCI and the
    // scratch disk's RAM disk, whatever is mounted on them — a rescan is refused before it looks.
    let rescan_of = |r: &libkern::device::DeviceRecord| {
        let path = alloc::format!("/dev/blk/{}", r.served);
        let (st, node) = ns_lookup(root_ns, path.as_bytes(), libkern::RIGHT_READ | libkern::RIGHT_WRITE);
        let status = if st == 0 { block_op(node, libkern::abi::IO_OPCODE_RESCAN, 0, 0) } else { st as i64 };
        close(node);
        status
    };
    let sata = registry
        .iter()
        .find(|r| r.kind() == libkern::device::DeviceKind::Disk && r.driver() == b"ahci" && !r.is_departed());
    for (what, rec) in [(b"the SATA disk".as_slice(), sata), (b"the scratch RAM disk", Some(scratch_rec))] {
        let Some(rec) = rec else {
            return finish(fail(b"no SATA disk to send a Rescan to"));
        };
        if rescan_of(rec) != KError::Unsupported.as_i32() as i64 {
            Line::new().s(b"boot-probe: storage admin: a Rescan of ").s(what).s(b" was not refused Unsupported").end();
            return finish(fail(b"a Rescan of a disk that is not a USB one"));
        }
    }
    match stick {
        Some(stick) => {
            // **Its old partition's node, held across the rescan**: the window's own
            // refusal of a `Rescan`, and its retirement, through the node a holder keeps.
            let Some(old) = registry.iter().find(|p| {
                p.parent == stick.id && p.kind() == libkern::device::DeviceKind::Partition && !p.is_departed()
            }) else {
                return finish(fail(b"the stick's partition has no record"));
            };
            let path = alloc::format!("/dev/blk/{}", old.served);
            let (st, held) = ns_lookup(root_ns, path.as_bytes(), libkern::RIGHT_READ | libkern::RIGHT_WRITE);
            if st != 0 || held == 0 {
                return finish(fail(b"the stick's partition's node would not open"));
            }
            let before = block_op(held, IO_OPCODE_READ, 0, 512);
            let refused_rescan = block_op(held, libkern::abi::IO_OPCODE_RESCAN, 0, 0);
            if before != 0 || refused_rescan != KError::Unsupported.as_i32() as i64 {
                close(held);
                Line::new().s(b"boot-probe: storage admin: through the partition: a read ").i(before).s(b", a Rescan ").i(refused_rescan).end();
                return finish(fail(b"a partition's node did not read, or did not refuse a Rescan Unsupported"));
            }
            let reply = ask(OP_STORAGE_REREAD, blk(stick).as_bytes());
            if !reply.as_ref().is_some_and(|m| !m.error && m.body.is_empty()) {
                return finish(fail(b"a Reread of a USB disk holding nothing mounted was not answered, with nothing mounted"));
            }
            if !pinged(clock_ns() + 10_000_000_000) {
                return finish(fail(b"a Reread that replaced a disk's partition did not ping the watch"));
            }
            let Some(now) = registry_records(root_ns) else {
                return finish(fail(b"the registry does not read after the Reread"));
            };
            let old: alloc::vec::Vec<u32> =
                registry.iter().filter(|p| p.parent == stick.id && !p.is_departed()).map(|p| p.id).collect();
            let replaced = old.iter().all(|&id| now.iter().any(|p| p.id == id && p.is_departed()))
                && now.iter().any(|p| p.parent == stick.id && !p.is_departed() && !old.contains(&p.id));
            if !replaced {
                close(held);
                return finish(fail(b"the Reread did not replace the disk's partition records"));
            }
            let after = block_op(held, IO_OPCODE_READ, 0, 512);
            close(held);
            if after != KError::PeerClosed.as_i32() as i64 {
                Line::new().s(b"boot-probe: storage admin: a read through the old partition after the rescan gave ").i(after).end();
                return finish(fail(b"the old partition's window was not retired by the rescan"));
            }
            Line::new()
                .s(b"boot-probe: storage admin: a Reread of ")
                .s(blk(stick).as_bytes())
                .s(b" rescanned it: its partition replaced, the old one's node refused after, and the watch pinged with no mount changed ok")
                .end();
        }
        None => kprint(b"boot-probe: storage admin: no USB disk holding a partition and nothing mounted here; test-qemu attaches one\n"),
    }
    kprint(b"boot-probe: storage admin: InUse the mounts and their disks, an unmount refused while a file was held, a file written and never synced on the device after the unmount, left clean, mounted again by name, each refusal its own, no admin reached from a session, a watch pinged by the unmount and the remount and not by a refusal, a session's eject of a RAM disk refused NoAccess, each Reread refusal its own ok\n");
    finish(true)
}

/// **One block operation on `dev`, waited for**: its status — `0`, the error its
/// completion carried, or the submit's own refusal. A page is made for the data of a read or a
/// write of `length` bytes at `offset`; a `Rescan` or a `Flush` names none.
fn block_op(dev: u64, opcode: u32, offset: u64, length: u64) -> i64 {
    let page = if length > 0 {
        // SAFETY: register-only syscall.
        unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) }
    } else {
        0
    };
    if page < 0 {
        return page;
    }
    let op = IoOp { opcode, flags: 0, buffer: page as u64, buf_offset: 0, offset, length };
    // SAFETY: a valid `IoOp`; `dev` and `page` are handles this process holds, or `page` is none.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, dev, (&op as *const IoOp) as u64) };
    let status = if po > 0 { po_wait(po as u64).0 as i64 } else { po };
    if page > 0 {
        close(page as u64);
    }
    status
}

/// **`disk`, through the `storage` grant**: the success path of `with admin disk`.
/// `disk --unmount nitrox-scratch` and then `disk --mount /dev/blk/<n>` each run in the admin
/// view with a stdout pipe; the table each writes and its exit code are checked, and then the
/// service's own table.
///
/// **Then a busy service, which is not a missing grant.** With every admin session taken,
/// `disk --unmount` in the same view is refused, and what it writes on `stderr` names the
/// service's `WouldBlock` rather than telling a person who holds the grant to go and get it.
pub(super) fn storage_grant_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::views::*;
    use libstream::wire::Value;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage grant: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let mut exited = alloc::vec::Vec::new();
    let Some(scratch) = registry_records(root_ns).and_then(|r| {
        r.iter().find(|r| r.kind() == libkern::device::DeviceKind::RamDisk).and_then(|r| r.block_index())
    }) else {
        return fail(b"no scratch disk in the registry");
    };
    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let session = match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, &[], &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        close(sup);
        return fail(b"OpenSession");
    };
    let client_path = alloc::format!("/svc/views/s/{session}");

    // One program in the admin view: its exit code, the first row of the table it wrote, and
    // what it wrote on `stderr`.
    let mut run = |args: &[&[u8]]| {
        in_admin_view(root_ns, &client_path, b"disk", args, &mut exited)
            .map(|(code, rows, said)| (code, rows.into_iter().next().unwrap_or_default(), said))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);
    let s = |v: &str| Value::Str(alloc::string::String::from(v));
    let mounted_rw = || {
        let (st, h) = ns_lookup(root_ns, b"/svc/storage/info/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        let t = bytes.and_then(|b| libstream::wire::Table::decode(&b).ok())?;
        let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
        let (l, m) = (col("label")?, col("mode")?);
        t.rows.iter().find(|r| r[l] == s("nitrox-scratch")).map(|r| r[m] == s("rw"))
    };

    let unmounted = run(&[b"--unmount", b"nitrox-scratch"]);
    let finish = |ok: bool| {
        let mut id = [0u8; 8];
        let n = build_session_id(&mut id, session).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 3, &id[..n], &[], &mut alloc::vec::Vec::new());
        close(sup);
        ok
    };
    if unmounted.map(|(code, row, _)| (code, row)) != Some((0, alloc::vec![s("nitrox-scratch"), Value::Bool(true)])) {
        return finish(fail(b"disk --unmount nitrox-scratch in the admin view did not answer its table"));
    }
    if mounted_rw() != Some(false) {
        return finish(fail(b"the table still shows nitrox-scratch mounted after disk --unmount"));
    }
    let path = alloc::format!("/dev/blk/{scratch}");
    let mounted = run(&[b"--mount", path.as_bytes()]);
    let name = alloc::format!("blk-{scratch}");
    if mounted.map(|(code, row, _)| (code, row))
        != Some((0, alloc::vec![s(&name), s("nitrox-scratch"), s("/storage/nitrox-scratch"), s("rw"), Value::Null]))
    {
        return finish(fail(b"disk --mount in the admin view did not answer its table"));
    }
    if mounted_rw() != Some(true) {
        return finish(fail(b"disk --mount did not leave nitrox-scratch mounted writable"));
    }

    // Every admin session taken — the broker holds one, for `InUse` — through the admin
    // endpoint bound in a namespace of the probe's own, as `storage_admin_test` opens one.
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/storage/admin-endpoint", chan);
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    let at = b"/admin";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = st == 0
        && ns > 0
        && unsafe { syscall6(SYS_NS_BIND, ns as u64, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) } == 0;
    close(endpoint);
    let mut held = alloc::vec::Vec::new();
    let mut refused = 0;
    while bound && held.len() < 8 {
        let (st, h) = ns_lookup(ns as u64, at, chan);
        if st != 0 {
            refused = st;
            break;
        }
        held.push(h);
    }
    if ns > 0 {
        close(ns as u64);
    }
    let busy = if refused == KError::WouldBlock.as_i32() { run(&[b"--unmount", b"nitrox-scratch"]) } else { None };
    for h in held {
        close(h);
    }
    if refused != KError::WouldBlock.as_i32() {
        return finish(fail(b"the admin sessions never ran out, so there is no busy service to ask"));
    }
    let Some((1, row, said)) = busy else {
        return finish(fail(b"disk --unmount with every admin session taken did not exit 1"));
    };
    if !row.is_empty() || !says(&said, b"opened no admin session (WouldBlock)") || says(&said, b"storage grant") {
        return finish(fail(b"disk with every admin session taken did not name the service's WouldBlock"));
    }
    if mounted_rw() != Some(true) {
        return finish(fail(b"a refused disk --unmount unmounted nitrox-scratch"));
    }
    kprint(b"boot-probe: storage grant: disk --unmount and disk --mount ran in the admin view, each answered its table, the service agreed, and a busy service was named as busy ok\n");
    finish(true)
}
