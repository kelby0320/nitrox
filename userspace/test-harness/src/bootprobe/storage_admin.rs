//! The storage administration tests, with and without the grant.

use super::*;
use libkern::{IO_OPCODE_READ, IoOp};
use libos::{Op, fs};

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
pub(super) fn storage_admin_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use librsproto::storage::{
        OP_STORAGE_EJECT, OP_STORAGE_IN_USE, OP_STORAGE_MOUNT, OP_STORAGE_REREAD, OP_STORAGE_UNMOUNT, build_mount,
        is_changed, parse_in_use, parse_mounted,
    };
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage admin: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::DUPLICATE;

    let table = || read_path(root, "/svc/storage/info/all.tsm").and_then(|b| Table::decode(&b).ok());
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
    let (Some(scratch), Some(root_row), Some(esp)) =
        (row_where(&t, "label", "nitrox-scratch"), row_where(&t, "mounted", "/"), row_where(&t, "filesystem", "fat"))
    else {
        return fail(b"the table has no scratch disk, root or ESP to work with");
    };
    let names: Vec<alloc::string::String> =
        [scratch, root_row, esp].iter().map(|&r| text(&t, r, col(&t, "name")).unwrap_or_default()).collect();
    let Some(registry) = registry_records(root) else {
        return fail(b"the registry does not read");
    };
    let id_of =
        |name: &str| registry.iter().find(|r| r.block_index().is_some_and(|n| alloc::format!("blk-{n}") == name));
    let (Some(scratch_rec), Some(root_rec), Some(esp_rec)) = (id_of(&names[0]), id_of(&names[1]), id_of(&names[2]))
    else {
        return fail(b"a table name matches no registry record");
    };

    let Ok(endpoint) = lookup::<Channel, Only, _>(root, "/svc/storage/admin-endpoint", chan) else {
        return fail(b"no admin endpoint at /svc/storage/admin-endpoint");
    };
    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        return fail(b"no namespace to bind the admin endpoint into");
    };
    let at = "/dev/storage/admin";
    let bound = ns.bind_server(at, endpoint.as_ref(), "");
    drop(endpoint);
    let admin = if bound.is_ok() { lookup::<Channel, Only, _>(&ns, at, chan).ok() } else { None };
    drop(ns);
    let Some(admin) = admin else {
        return fail(b"/dev/storage/admin opened no admin session");
    };
    let mut next = 0u64;
    let mut ask = |op: u16, body: &[u8]| -> Option<Received> {
        next += 1;
        if !rs_send(&admin, op, next, body, Carried::new()) {
            return None;
        }
        let deadline = clock_ns() + 30_000_000_000;
        loop {
            let m = receive(&admin, deadline)?;
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

    // 0. **A watch and a media session, as a session opens them**: a resolve of
    //    `info/watch` and of `info/media` on a session endpoint, each answered with a channel.
    let endpoint = lookup::<Channel, Only, _>(root, "/svc/storage/session-endpoint", chan);
    let open = |endpoint: &Handle<Channel, Only>, suffix: &[u8]| -> Option<Handle<Channel, Only>> {
        let mut req = [0u8; 64];
        let n = librsproto::namespace::resolve_request(&mut req, chan.bits(), 0, suffix)?;
        if !rs_send(endpoint, librsproto::OP_NS_RESOLVE, 1, &req[..n], Carried::new()) {
            return None;
        }
        let mut m = receive(endpoint, clock_ns() + 5_000_000_000)?;
        if m.error || m.count() != 1 {
            return None;
        }
        m.take(0)
    };
    let (watch, media) = match &endpoint {
        Ok(endpoint) => (open(endpoint, b"info/watch"), open(endpoint, b"info/media")),
        Err(_) => (None, None),
    };
    drop(endpoint);
    let (Some(watch), Some(media)) = (watch, media) else {
        return fail(b"a session endpoint opened no watch, or no media session");
    };
    // **Whether the watch was pinged**, waiting until `deadline` for the first — `0` only looks —
    // and taking every ping queued behind it.
    let pinged = |deadline: u64| {
        let mut any = false;
        while let Some(m) = receive(&watch, if any { 0 } else { deadline }) {
            any |= is_changed(m.op, &m.body);
        }
        any
    };
    pinged(0);

    let mut ids = Vec::new();
    let in_use = ask(OP_STORAGE_IN_USE, &[]);
    if !in_use.as_ref().is_some_and(|m| !m.error && parse_in_use(&m.body, |id| ids.push(id))) {
        return fail(b"InUse was not answered");
    }
    let disk_of_root = root_rec.parent;
    if !(ids.contains(&scratch_rec.id) && ids.contains(&root_rec.id) && ids.contains(&disk_of_root))
        || ids.contains(&esp_rec.id)
    {
        return fail(b"InUse is not the scratch disk, the root and its disk, without the ESP");
    }

    let Ok(held) = lookup::<File, MapRead, _>(root, "/svc/storage/fs/nitrox-scratch/README", Rights::MAP_READ) else {
        return fail(b"the README would not open to hold");
    };
    let busy = ask(OP_STORAGE_UNMOUNT, b"nitrox-scratch");
    drop(held);
    if !refused(&busy, KError::WouldBlock) {
        return fail(b"an unmount with a file held was not refused WouldBlock");
    }
    // **No ping for a refusal**: the service pings at the end of the turn that changed the mounts,
    // so one owed for that turn is queued before the reply to any later request — and `InUse`'s
    // reply being here means a look at the watch now would find it.
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return fail(b"an unmount refused pinged the watch, or InUse was not answered");
    }
    if read_path(root, "/svc/storage/fs/nitrox-scratch/README").as_deref() != Some(SCRATCH_README) {
        return fail(b"a refused unmount did not leave the mount as it was");
    }

    const LEN: u64 = 2 * PAGE + 17;
    let Ok(fh) = block_on(fs::create::<_, MapReadWrite>(root, "/svc/storage/fs/nitrox-scratch/unsynced", LEN, RW))
    else {
        return fail(b"a file would not create to write unsynced");
    };
    let Ok(mut mapped) = fh.map(LEN as usize) else {
        return fail(b"that file would not map writable");
    };
    let pattern = |i: u64| (i * 13 + 5) as u8;
    let want: Vec<u8> = (0..LEN).map(pattern).collect();
    let _ = mapped.write_from(0, &want);
    // The mapping goes, and the handle with it, with no sync.
    drop(mapped);
    drop(fh);

    // 4. Unmount, then the label is gone — and the watch is told.
    if !answered(&ask(OP_STORAGE_UNMOUNT, b"nitrox-scratch")) {
        return fail(b"the unmount was not answered");
    }
    if !pinged(clock_ns() + 5_000_000_000) {
        return fail(b"the unmount did not ping the watch");
    }
    let st = status(&lookup::<File, MapRead, _>(root, "/svc/storage/fs/nitrox-scratch/README", Rights::MAP_READ));
    if st != KError::NotFound.as_i32() {
        return fail(b"the label still resolved once unmounted");
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mounted"))))
        != Some(None)
    {
        return fail(b"the table still shows the scratch disk mounted");
    }

    let Some(raw) = RootDevice::on(root, &alloc::format!("/dev/blk/{}", scratch_rec.served)) else {
        return fail(b"the scratch disk's device would not open raw");
    };
    let on_disk = raw.read_file(b"/unsynced", 0, LEN as usize);
    let clean = fs_server_ext4::ext4::was_left_clean(&raw);
    drop(raw);
    if on_disk.as_deref() != Some(&want[..]) {
        return fail(b"what was written and never synced is not on the device after the unmount");
    }
    if clean != Ok(true) {
        return fail(b"the unmounted filesystem was not left clean");
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
        return fail(b"a Reread of the unmounted scratch disk, a RAM disk, was not refused Unsupported");
    }

    // 6. Mounted again by name — once refused a hidden label, the one moment a mountable device
    //    exists to refuse it on.
    let mut body = [0u8; 64];
    let n = build_mount(&mut body, names[0].as_bytes(), b".hidden").unwrap_or(0);
    if !refused(&ask(OP_STORAGE_MOUNT, &body[..n]), KError::InvalidArgument) {
        return fail(b"a hidden label was not refused InvalidArgument");
    }
    let n = build_mount(&mut body, names[0].as_bytes(), b"").unwrap_or(0);
    let again = ask(OP_STORAGE_MOUNT, &body[..n]);
    let mounted = |m: &Received| parse_mounted(&m.body) == Some((&b"nitrox-scratch"[..], None));
    if !again.as_ref().is_some_and(|m| !m.error && mounted(m)) {
        return fail(b"a Mount by name did not bring nitrox-scratch back");
    }
    if !pinged(clock_ns() + 5_000_000_000) {
        return fail(b"the remount did not ping the watch");
    }
    if read_path(root, "/svc/storage/fs/nitrox-scratch/unsynced").as_deref() != Some(&want[..]) {
        return fail(b"the remounted filesystem does not hold the unsynced file");
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mode"))))
        != Some(Some(alloc::string::String::from("rw")))
    {
        return fail(b"the remount is not writable");
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
            return fail(b"a refusal");
        }
    }

    let Ok(session) = lookup::<Channel, Only, _>(root, "/svc/storage/session-endpoint", chan) else {
        return fail(b"no session endpoint");
    };
    let mut req = [0u8; 64];
    let n = librsproto::namespace::resolve_request(&mut req, chan.bits(), 0, b"admin-endpoint").unwrap_or(0);
    let asked = rs_send(&session, librsproto::OP_NS_RESOLVE, 1, &req[..n], Carried::new());
    let reply = if asked { receive(&session, clock_ns() + 5_000_000_000) } else { None };
    drop(session);
    if !refused(&reply, KError::NotFound) {
        return fail(b"a session endpoint answered admin-endpoint");
    }

    // 9. **A session's eject** of the scratch disk, a RAM disk: refused `NoAccess`, since an
    //    internal disk is the `storage` grant's; a name nothing is mounted under, `NotFound`.
    let eject = |id: u64, name: &[u8]| {
        if !rs_send(&media, OP_STORAGE_EJECT, id, name, Carried::new()) {
            return None;
        }
        let m = receive(&media, clock_ns() + 30_000_000_000)?;
        Some(m)
    };
    if !refused(&eject(1, b"nitrox-scratch"), KError::NoAccess) {
        return fail(b"a session's eject of the scratch disk was not refused NoAccess");
    }
    if !refused(&eject(2, b"no-such-label"), KError::NotFound) {
        return fail(b"a session's eject of an unknown name was not refused NotFound");
    }
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return fail(b"an eject refused pinged the watch");
    }
    if table().and_then(|t| row_where(&t, "label", "nitrox-scratch").map(|r| text(&t, r, col(&t, "mode"))))
        != Some(Some(alloc::string::String::from("rw")))
    {
        return fail(b"a refused eject unmounted the scratch disk");
    }

    // 10. **`Reread`'s refusals**. The disk the machine started from is found by its flag, and
    //     its partition by its parent — never "the first FAT", which on some machines is an
    //     internal disk's, that a `Reread` would take.
    let blk = |r: &libkern::device::DeviceRecord| alloc::format!("blk-{}", r.served);
    let root_disk = registry.iter().find(|r| r.id == root_rec.parent);
    let mounted_ids: Vec<u32> = ids.clone();
    let boot_part = registry.iter().find(|r| {
        r.kind() == libkern::device::DeviceKind::Partition
            && !r.is_departed()
            && registry.iter().any(|d| d.id == r.parent && d.is_boot_medium())
            && !mounted_ids.contains(&r.id)
    });
    let (Some(root_disk), Some(boot_part)) = (root_disk, boot_part) else {
        return fail(b"no disk holding init's root, or no unmounted partition of the boot disk, to ask Reread of");
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
            return fail(b"a Reread refusal");
        }
    }
    if !answered(&ask(OP_STORAGE_IN_USE, &[])) || pinged(0) {
        return fail(b"a refused Reread pinged the watch");
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
        match lookup::<Resource, ReadWrite, _>(root, &path, Rights::READ | Rights::WRITE) {
            Ok(node) => block_op(&node, libkern::abi::IO_OPCODE_RESCAN, 0, 0),
            Err(e) => i64::from(e.code().as_i32()),
        }
    };
    let sata = registry
        .iter()
        .find(|r| r.kind() == libkern::device::DeviceKind::Disk && r.driver() == b"ahci" && !r.is_departed());
    for (what, rec) in [(b"the SATA disk".as_slice(), sata), (b"the scratch RAM disk", Some(scratch_rec))] {
        let Some(rec) = rec else {
            return fail(b"no SATA disk to send a Rescan to");
        };
        if rescan_of(rec) != KError::Unsupported.as_i32() as i64 {
            Line::new().s(b"boot-probe: storage admin: a Rescan of ").s(what).s(b" was not refused Unsupported").end();
            return fail(b"a Rescan of a disk that is not a USB one");
        }
    }
    match stick {
        Some(stick) => {
            // **Its old partition's node, held across the rescan**: the window's own
            // refusal of a `Rescan`, and its retirement, through the node a holder keeps.
            let Some(old) = registry.iter().find(|p| {
                p.parent == stick.id && p.kind() == libkern::device::DeviceKind::Partition && !p.is_departed()
            }) else {
                return fail(b"the stick's partition has no record");
            };
            let path = alloc::format!("/dev/blk/{}", old.served);
            let Ok(held) = lookup::<Resource, ReadWrite, _>(root, &path, Rights::READ | Rights::WRITE) else {
                return fail(b"the stick's partition's node would not open");
            };
            let before = block_op(&held, IO_OPCODE_READ, 0, 512);
            let refused_rescan = block_op(&held, libkern::abi::IO_OPCODE_RESCAN, 0, 0);
            if before != 0 || refused_rescan != KError::Unsupported.as_i32() as i64 {
                drop(held);
                Line::new().s(b"boot-probe: storage admin: through the partition: a read ").i(before).s(b", a Rescan ").i(refused_rescan).end();
                return fail(b"a partition's node did not read, or did not refuse a Rescan Unsupported");
            }
            let reply = ask(OP_STORAGE_REREAD, blk(stick).as_bytes());
            if !reply.as_ref().is_some_and(|m| !m.error && m.body.is_empty()) {
                return fail(b"a Reread of a USB disk holding nothing mounted was not answered, with nothing mounted");
            }
            if !pinged(clock_ns() + 10_000_000_000) {
                return fail(b"a Reread that replaced a disk's partition did not ping the watch");
            }
            let Some(now) = registry_records(root) else {
                return fail(b"the registry does not read after the Reread");
            };
            let old: Vec<u32> =
                registry.iter().filter(|p| p.parent == stick.id && !p.is_departed()).map(|p| p.id).collect();
            let replaced = old.iter().all(|&id| now.iter().any(|p| p.id == id && p.is_departed()))
                && now.iter().any(|p| p.parent == stick.id && !p.is_departed() && !old.contains(&p.id));
            if !replaced {
                drop(held);
                return fail(b"the Reread did not replace the disk's partition records");
            }
            let after = block_op(&held, IO_OPCODE_READ, 0, 512);
            drop(held);
            if after != KError::PeerClosed.as_i32() as i64 {
                Line::new().s(b"boot-probe: storage admin: a read through the old partition after the rescan gave ").i(after).end();
                return fail(b"the old partition's window was not retired by the rescan");
            }
            Line::new()
                .s(b"boot-probe: storage admin: a Reread of ")
                .s(blk(stick).as_bytes())
                .s(b" rescanned it: its partition replaced, the old one's node refused after, and the watch pinged with no mount changed ok")
                .end();
        }
        None => log::write(b"boot-probe: storage admin: no USB disk holding a partition and nothing mounted here; test-qemu attaches one\n"),
    }
    log::write(b"boot-probe: storage admin: InUse the mounts and their disks, an unmount refused while a file was held, a file written and never synced on the device after the unmount, left clean, mounted again by name, each refusal its own, no admin reached from a session, a watch pinged by the unmount and the remount and not by a refusal, a session's eject of a RAM disk refused NoAccess, each Reread refusal its own ok\n");
    true
}

/// **One block operation on `dev`, waited for**: its status — `0`, the error its
/// completion carried, or the submit's own refusal. A page is made for the data of a read or a
/// write of `length` bytes at `offset`; a `Rescan` or a `Flush` names none.
fn block_op<T, M>(dev: &Handle<T, M>, opcode: u32, offset: u64, length: u64) -> i64 {
    let page = if length > 0 {
        match Handle::<Memory, MapReadWrite>::create(PAGE as usize) {
            Ok(page) => Some(page),
            Err(e) => return i64::from(e.code().as_i32()),
        }
    } else {
        None
    };
    let buffer = page.as_ref().map_or(0, |p| p.raw().0);
    let op = IoOp { opcode, flags: 0, buffer, buf_offset: 0, offset, length };
    match Op::submit(dev.raw(), &op).map(block_on) {
        Ok(Ok(_)) => 0,
        Ok(Err(e)) | Err(e) => i64::from(e.code().as_i32()),
    }
}

/// **`disk`, through the `storage` grant**: the success path of `with admin disk`.
/// `disk --unmount nitrox-scratch` and then `disk --mount /dev/blk/<n>` each run in the admin
/// view with a stdout pipe; the table each writes and its exit code are checked, and then the
/// service's own table.
///
/// **Then a busy service, which is not a missing grant.** With every admin session taken,
/// `disk --unmount` in the same view is refused, and what it writes on `stderr` names the
/// service's `WouldBlock` rather than telling a person who holds the grant to go and get it.
pub(super) fn storage_grant_test(root: Root<'_>) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage grant: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut exited = Vec::new();
    let Some(scratch) = registry_records(root).and_then(|r| {
        r.iter().find(|r| r.kind() == libkern::device::DeviceKind::RamDisk).and_then(|r| r.block_index())
    }) else {
        return fail(b"no scratch disk in the registry");
    };
    let Ok(sup) = lookup::<Channel, Only, _>(root, "/svc/views/session", chan) else {
        return fail(b"no supervisor channel at /svc/views/session");
    };
    let session = match views_call(&sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, Carried::new(), &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        return fail(b"OpenSession");
    };
    let ok = grant_session(root, session, scratch, &mut exited);
    let mut id = [0u8; 8];
    let n = build_session_id(&mut id, session).unwrap_or(0);
    let _ = views_call(&sup, OP_VIEWS_CLOSE_SESSION, 3, &id[..n], Carried::new(), &mut Vec::new());
    ok
}

/// What `storage_grant_test` runs in the admin view of `session`, with the scratch disk at
/// `/dev/blk/{scratch}`.
fn grant_session(root: Root<'_>, session: u64, scratch: u32, exited: &mut Vec<Vec<u8>>) -> bool {
    use libkern::KError;
    use libstream::wire::Value;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage grant: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let client_path = alloc::format!("/svc/views/s/{session}");

    // One program in the admin view: its exit code, the first row of the table it wrote, and
    // what it wrote on `stderr`.
    let mut run = |args: &[&[u8]]| {
        in_admin_view(root, &client_path, b"disk", args, exited)
            .map(|(code, rows, said)| (code, rows.into_iter().next().unwrap_or_default(), said))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);
    let s = |v: &str| Value::Str(alloc::string::String::from(v));
    let mounted_rw = || {
        let t = read_path(root, "/svc/storage/info/all.tsm").and_then(|b| libstream::wire::Table::decode(&b).ok())?;
        let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
        let (l, m) = (col("label")?, col("mode")?);
        t.rows.iter().find(|r| r[l] == s("nitrox-scratch")).map(|r| r[m] == s("rw"))
    };

    let unmounted = run(&[b"--unmount", b"nitrox-scratch"]);
    if unmounted.map(|(code, row, _)| (code, row)) != Some((0, alloc::vec![s("nitrox-scratch"), Value::Bool(true)])) {
        return fail(b"disk --unmount nitrox-scratch in the admin view did not answer its table");
    }
    if mounted_rw() != Some(false) {
        return fail(b"the table still shows nitrox-scratch mounted after disk --unmount");
    }
    let path = alloc::format!("/dev/blk/{scratch}");
    let mounted = run(&[b"--mount", path.as_bytes()]);
    let name = alloc::format!("blk-{scratch}");
    if mounted.map(|(code, row, _)| (code, row))
        != Some((0, alloc::vec![s(&name), s("nitrox-scratch"), s("/storage/nitrox-scratch"), s("rw"), Value::Null]))
    {
        return fail(b"disk --mount in the admin view did not answer its table");
    }
    if mounted_rw() != Some(true) {
        return fail(b"disk --mount did not leave nitrox-scratch mounted writable");
    }

    // Every admin session taken — the broker holds one, for `InUse` — through the admin
    // endpoint bound in a namespace of the probe's own, as `storage_admin_test` opens one.
    let endpoint = lookup::<Channel, Only, _>(root, "/svc/storage/admin-endpoint", chan);
    let ns = Handle::<Namespace, NsMutable>::create();
    let at = "/admin";
    let bound = match (&endpoint, &ns) {
        (Ok(endpoint), Ok(ns)) => ns.bind_server(at, endpoint.as_ref(), "").is_ok(),
        _ => false,
    };
    drop(endpoint);
    let mut held = Vec::new();
    let mut refused = 0;
    if let (true, Ok(ns)) = (bound, &ns) {
        while held.len() < 8 {
            match lookup::<Channel, Only, _>(ns, at, chan) {
                Ok(h) => held.push(h),
                Err(e) => {
                    refused = e.code().as_i32();
                    break;
                }
            }
        }
    }
    drop(ns);
    let busy = if refused == KError::WouldBlock.as_i32() { run(&[b"--unmount", b"nitrox-scratch"]) } else { None };
    drop(held);
    if refused != KError::WouldBlock.as_i32() {
        return fail(b"the admin sessions never ran out, so there is no busy service to ask");
    }
    let Some((1, row, said)) = busy else {
        return fail(b"disk --unmount with every admin session taken did not exit 1");
    };
    if !row.is_empty() || !says(&said, b"opened no admin session (WouldBlock)") || says(&said, b"storage grant") {
        return fail(b"disk with every admin session taken did not name the service's WouldBlock");
    }
    if mounted_rw() != Some(true) {
        return fail(b"a refused disk --unmount unmounted nitrox-scratch");
    }
    log::write(b"boot-probe: storage grant: disk --unmount and disk --mount ran in the admin view, each answered its table, the service agreed, and a busy service was named as busy ok\n");
    true
}
