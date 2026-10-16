//! The registry, device and I/O-counter tests.

use super::*;

/// **Wait `ns`**, on a channel of its own that nothing signals: `sys_wait` takes an absolute deadline
/// and answers `TimedOut`.
fn nap(ns: u64) {
    let (mut ours, mut theirs) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let cr = unsafe { syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut ours) as u64, (&raw mut theirs) as u64, 1, 0) };
    if cr != 0 {
        return;
    }
    let handles = [ours];
    let mut results = [0u8; 24];
    // SAFETY: one handle this process holds, and room for one result.
    unsafe {
        syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, clock_ns().saturating_add(ns));
    }
    close(ours);
    close(theirs);
}

/// **The device registry, through its binding** (administration Part B.1). The kernel's host tests
/// see its table as a value; this sees `/dev/registry` as a process does, and holds it to the
/// paths it has to agree with:
/// - the snapshot decodes, through the reader everything else will use;
/// - **its block records are exactly what probing `/dev/blk` finds** — the same number, and each
///   record's served index resolving to a device of the record's size, whose `info` gives the
///   record's name;
/// - **the i8042's keyboard is served at 0 and its mouse at 1**, each resolving under
///   `/dev/input/raw`; a USB keyboard or mouse (Phase 6 Part B.2) resolves there too, at an index
///   after every i8042 record's — from 0 on a machine with no i8042 — under its `UsbDevice` record,
///   driven by `usb-hid`; and there is a keyboard and a mouse, from either;
/// - every record's `/dev/registry/<id>` is a device node, **and its id is its place** — which a
///   phantom record read past the count, all zeros, cannot be;
/// - **a USB device is under the controller that enumerated it** (Phase 6 Part A.3): its parent a
///   PCI function of class `0c/03/30` claimed by `xhci`, with a port, a speed and a name, which no
///   other kind carries. Each is printed, for `test-qemu` to hold to what QEMU attached: the
///   kernel's own line says a device was enumerated, and only this says the table got it.
///
/// The paths and the records read one field in the kernel, so a disagreement here would be a new
/// path that stopped reading it.
pub(super) fn registry_test(root_ns: u64) -> bool {
    use libkern::device::DeviceKind;
    use libkern::{RIGHT_INSPECT, RIGHT_READ};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: registry: ").s(what).s(b" FAIL").end();
        false
    };

    let Some(all) = registry_records(root_ns) else {
        return fail(b"no /dev/registry in the root namespace that reads");
    };
    let total = all.len();
    let mut blocks = 0u32;
    let (mut keyboard, mut mouse) = (false, false);
    // **Where a USB input record may be served from: after every i8042 record**, which on a machine
    // with no i8042 is 0. A fixed 2 failed every USB record on that machine (PR #358 review).
    let usb_from = all
        .iter()
        .filter(|r| matches!(r.kind(), DeviceKind::Keyboard | DeviceKind::Mouse) && r.driver() != b"usb-hid")
        .map(|r| r.served.saturating_add(1))
        .max()
        .unwrap_or(0);
    for (place, r) in all.iter().enumerate() {
        if r.id as usize != place {
            Line::new().s(b"boot-probe: registry: record ").u(place as u64).s(b" says it is ").u(r.id as u64).end();
            return fail(b"the records are not the table in order");
        }
        let path = alloc::format!("/dev/registry/{}", r.id);
        // **A departed device's paths answer `NotFound`** (Phase 6 Part C) — its id, and its served
        // index on its own path — and nothing else about it is checked: the record is a history, not
        // a device. `test-qemu`'s hot-plug leaves one or two in most boots.
        if r.is_departed() {
            let served = match r.kind() {
                DeviceKind::Keyboard | DeviceKind::Mouse => Some(alloc::format!("/dev/input/raw/{}", r.served)),
                k if k.is_block() => Some(alloc::format!("/dev/blk/{}", r.served)),
                _ => None,
            };
            for p in core::iter::once(path.clone()).chain(served) {
                let (st, h) = ns_lookup(root_ns, p.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
                close(h);
                if st != libkern::KError::NotFound.as_i32() {
                    Line::new()
                        .s(b"boot-probe: registry: departed ")
                        .s(p.as_bytes())
                        .s(b" answered ")
                        .i(st as i64)
                        .end();
                    return fail(b"a departed device's path still resolves");
                }
            }
            Line::new().s(b"boot-probe: registry: record ").u(r.id as u64).s(b" departed, its paths refused").end();
            continue;
        }
        let (st, node) = ns_lookup(root_ns, path.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
        let kind_ok = st == 0 && stat(node).is_some_and(|i| i.object_type == libkern::KObjectType::DeviceNode as u32);
        close(node);
        if !kind_ok {
            Line::new().s(b"boot-probe: registry: ").s(path.as_bytes()).s(b" is not a device node").end();
            return fail(b"a record's id does not resolve to its node");
        }
        match r.kind() {
            DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk => {
                blocks += 1;
                let dev = alloc::format!("/dev/blk/{}", r.served);
                let (st, h) = ns_lookup(root_ns, dev.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
                let size = if st == 0 { stat(h).map(|i| i.size) } else { None };
                close(h);
                let want = r.logical_block_size as u64 * r.block_count;
                if size != Some(want) {
                    Line::new().s(b"boot-probe: registry: ").s(dev.as_bytes()).s(b" is not the record's size").end();
                    return fail(b"a block record's served index names another device");
                }
                let info_path = alloc::format!("/dev/blk/{}/info", r.served);
                let (st, ih) = ns_lookup(root_ns, info_path.as_bytes(), RIGHT_MAP_READ | RIGHT_INSPECT);
                let info = if st == 0 { read_all(ih) } else { None };
                close(ih);
                // `BlockDeviceInfo`: `name_len` at 16, `name` from 24.
                let named = info.is_some_and(|b| {
                    let n = u32::from_le_bytes([b[16], b[17], b[18], b[19]]) as usize;
                    b.get(24..24 + n) == Some(r.name())
                });
                if !named {
                    return fail(b"a block record's name is not its device's");
                }
                // **A USB disk is under its USB device, and its partitions under it** (Phase 6
                // Part D): said, for `test-qemu` to hold to the stick it attached.
                if r.driver() == b"usb-storage" {
                    if !all.get(r.parent as usize).is_some_and(|p| p.kind() == DeviceKind::UsbDevice) {
                        return fail(b"a USB disk is not under its USB device");
                    }
                    Line::new()
                        .s(b"boot-probe: registry: usb disk blk-")
                        .u(r.served as u64)
                        .s(b", ")
                        .u(r.block_count)
                        .s(b" blocks of ")
                        .u(r.logical_block_size as u64)
                        .s(b", under usb-")
                        .u(r.parent as u64)
                        .end();
                } else if let Some(disk) = all.get(r.parent as usize).filter(|d| d.driver() == b"usb-storage") {
                    Line::new()
                        .s(b"boot-probe: registry: usb disk blk-")
                        .u(disk.served as u64)
                        .s(b"'s partition blk-")
                        .u(r.served as u64)
                        .s(b", by ")
                        .s(r.driver())
                        .end();
                }
            }
            DeviceKind::Keyboard | DeviceKind::Mouse => {
                let raw = alloc::format!("/dev/input/raw/{}", r.served);
                let (st, h) = ns_lookup(root_ns, raw.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
                close(h);
                if st != 0 {
                    return fail(b"an input record is not at its raw index");
                }
                if r.driver() == b"usb-hid" {
                    let under_usb = all.get(r.parent as usize).is_some_and(|p| p.kind() == DeviceKind::UsbDevice);
                    if !under_usb || r.served < usb_from {
                        return fail(b"a USB input record is not under its USB device, after the i8042's");
                    }
                    if r.kind() == DeviceKind::Keyboard {
                        keyboard = true;
                    } else {
                        mouse = true;
                    }
                    let what = if r.kind() == DeviceKind::Keyboard { b"keyboard".as_slice() } else { b"mouse" };
                    Line::new()
                        .s(b"boot-probe: registry: usb ")
                        .s(what)
                        .s(b" at /dev/input/raw/")
                        .u(r.served as u64)
                        .s(b", under usb-")
                        .u(r.parent as u64)
                        .end();
                    continue;
                }
                let want = if r.kind() == DeviceKind::Keyboard { 0 } else { 1 };
                if r.served != want {
                    return fail(b"an i8042 input record is not at its raw index");
                }
                if r.kind() == DeviceKind::Keyboard {
                    keyboard = true;
                } else {
                    mouse = true;
                }
            }
            DeviceKind::UsbDevice => {
                let Some(c) = all.get(r.parent as usize).filter(|c| {
                    c.kind() == DeviceKind::PciFunction
                        && (c.pci_class, c.subclass, c.prog_if) == (0x0c, 0x03, 0x30)
                        && c.outcome == libkern::device::OUTCOME_CLAIMED
                        && c.driver() == b"xhci"
                }) else {
                    return fail(b"a USB device is not under a claimed xHCI controller");
                };
                let bare = r.class == 0 && r.served == libkern::device::NOT_SERVED;
                if r.port == 0 || r.speed == 0 || r.name().is_empty() || r.driver() != b"xhci" || !bare {
                    return fail(b"a USB record lacks its port, speed, name or driver, or is served");
                }
                let at = alloc::format!(
                    "boot-probe: registry: usb port {}, {:04x}:{:04x} class {:02x}/{:02x}/{:02x}, speed {}, \
                     under {:02x}:{:02x}.{}, \"",
                    r.port,
                    r.vendor,
                    r.device,
                    r.pci_class,
                    r.subclass,
                    r.prog_if,
                    r.speed,
                    c.bus,
                    c.dev,
                    c.func
                );
                Line::new().s(at.as_bytes()).untrusted(r.name()).s(b"\"").end();
            }
            _ => {}
        }
    }
    // What the probes find, the old way.
    let mut probed = 0u32;
    loop {
        let dev = alloc::format!("/dev/blk/{probed}");
        let (st, h) = ns_lookup(root_ns, dev.as_bytes(), RIGHT_READ | RIGHT_INSPECT);
        close(h);
        if st != 0 {
            break;
        }
        probed += 1;
    }
    if probed != blocks {
        Line::new()
            .s(b"boot-probe: registry: ")
            .u(blocks as u64)
            .s(b" block records, ")
            .u(probed as u64)
            .s(b" probed")
            .end();
        return fail(b"the block records are not what /dev/blk serves");
    }
    if !keyboard || !mouse {
        return fail(b"no keyboard and mouse records");
    }
    // **The disk the machine started from, flagged, and nothing else** (Phase 6 Part D): every boot
    // that runs this one came off a GPT disk, so exactly one record says so, and it is a disk.
    let mut boot = all.iter().filter(|r| r.is_boot_medium());
    match (boot.next(), boot.next()) {
        (Some(d), None) if d.kind() == DeviceKind::Disk && !d.is_departed() => {
            Line::new()
                .s(b"boot-probe: registry: the boot disk is blk-")
                .u(d.served as u64)
                .s(b", and nothing else is flagged")
                .end();
        }
        (None, _) => return fail(b"no record is flagged as the disk the machine started from"),
        _ => return fail(b"the boot flag is on more than one record, or on one that is not a present disk"),
    }
    // **`/dev/registry/changes` refuses a read too short for its answer**, at submission (PR #361
    // review): `io-operation.md`'s rule, which the manager's eight-byte reads never test. Asked past
    // generation 0, which the table is long past, so a node that took it would answer at once rather
    // than keep one of the four places a waiting read has.
    let (st, changes) = ns_lookup(root_ns, b"/dev/registry/changes", RIGHT_READ);
    if st != 0 || changes == 0 {
        return fail(b"/dev/registry/changes does not open");
    }
    // SAFETY: register-only syscall.
    let page = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) }.max(0) as u64;
    let short = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: page, buf_offset: 0, offset: 0, length: 7 };
    // SAFETY: a valid `IoOp`; `changes` and `page` are handles this process holds.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, changes, (&short as *const IoOp) as u64) };
    close(changes);
    if page != 0 {
        close(page);
    }
    if po != libkern::KError::InvalidArgument.as_i32() as i64 {
        if po > 0 {
            close(po as u64);
        }
        Line::new().s(b"boot-probe: registry: a seven-byte read of /dev/registry/changes gave ").i(po).end();
        return fail(b"/dev/registry/changes did not refuse a read shorter than its answer");
    }
    Line::new()
        .s(b"boot-probe: registry: ")
        .u(total as u64)
        .s(b" nodes, ")
        .u(blocks as u64)
        .s(b" block devices as /dev/blk serves them, keyboard and mouse at their raw indices ok")
        .end();
    true
}

/// **The device manager, through its own paths** (administration Part B.2), before any owner but
/// `input-server` exists. `block` has no owner until Part C's storage service, so the probe takes
/// it:
/// - **the subscription replays every block device as `Arrived`, each with its node, then
///   `Settled`** with the count — the same devices `/dev/registry` lists as block;
/// - **a second subscription is refused while the first is held**, and taken once it is closed —
///   one owner per class, the kernel's one reader per device kept at the manager;
/// - **`info` lists `all.tsm` and a file per present device, and `all.tsm` is a table** with a row
///   each, **agreeing with the registry's present records** once the manager has caught up (Phase 6
///   Part C): it follows the table, so `test-qemu`'s hot-plug — landing while this runs — is in
///   both, or departed from both. Each present USB device is listed as `usb-<id>.tsm`. Until Part
///   C the manager read the table once, and this held it to a prefix of the registry. **It cannot
///   see a manager that never follows** when the hot-plug has come and gone before that manager's
///   one read — a run with following removed passed it — so `check-input --usb`, which orders its
///   own plugs, is the gate that holds following, and this holds agreement wherever the plug lands;
/// - **`input` is refused, because `input-server` holds it** (Part B.3) — which is how a probe
///   sees that the input server took its devices from the manager rather than from the raw paths.
///   Were it not held, this resolve would take the class for a moment and give it back;
/// - **the info-only endpoint answers the tables and nothing else** (Part B.4): `block` and
///   another `info-endpoint` are `NotFound` on it, where the root endpoint subscribes and mints,
///   and `info` opens the directory.
pub(super) fn devices_test(root_ns: u64) -> bool {
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: devices: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;

    // **`block` has its owner from boot on** (administration Part C.5): `service-mgr` spawns the
    // storage service straight after the manager and waits for its `Ready`, which comes only after
    // it has subscribed and settled. So `block` is refused here, as `input` is below. Until C.5 the
    // probe took the class itself, to see the replay settle before its resolve completed, a second
    // owner refused, and the class taken again once closed. Those stay the manager's host tests and
    // B.2's recorded controls, since taking the class now would take the disks from their owner.
    // That the replay reaches its owner is `storage_test`'s: a row per block record.
    let (st, block) = ns_lookup(root_ns, b"/svc/devices/block", chan);
    close(block);
    if st != libkern::KError::AlreadyExists.as_i32() {
        Line::new().s(b"boot-probe: devices: a subscription to block answered ").i(st as i64).end();
        return fail(b"block is not held, so the storage service did not take the disks");
    }

    // `input` has its owner from boot on: `init` waits for `input-server`'s `Ready`, which comes
    // only after it has subscribed and settled.
    let (st, input) = ns_lookup(root_ns, b"/svc/devices/input", chan);
    close(input);
    if st != libkern::KError::AlreadyExists.as_i32() {
        Line::new().s(b"boot-probe: devices: a subscription to input answered ").i(st as i64).end();
        return fail(b"input is not held, so input-server did not take its devices from the manager");
    }

    // **The endpoint a session is given answers the tables and nothing else** (Part B.4). `init`
    // couriers one of these for every session's `/dev/devices`, and `desktop-shell` holds it with
    // `BIND_NAMESPACE`, so it could bind it with no base — where the root endpoint would take
    // `block` as a subscription to every disk. The probe cannot bind (it holds no syscaps), but a
    // forwarding endpoint is a channel and the manager answers whatever resolve arrives on it, so
    // the probe sends the resolves a namespace would forward, as the kernel would.
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/devices/info-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no info-only endpoint at /svc/devices/info-endpoint");
    }
    let forward = |request_id: u64, suffix: &[u8]| -> Option<Received> {
        let mut body = [0u8; 64];
        let n = librsproto::namespace::resolve_request(&mut body, chan, 0, suffix)?;
        if !rs_send(endpoint, librsproto::OP_NS_RESOLVE, request_id, &body[..n], &[]) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(endpoint, deadline)?;
            if m.request_id == request_id {
                return Some(m);
            }
            m.handles.iter().for_each(|&h| close(h));
        }
    };
    let refused = |m: &Option<Received>| {
        m.as_ref().is_some_and(|m| {
            m.error
                && m.handles.is_empty()
                && librsproto::error::parse_error(&m.body)
                    .is_some_and(|e| e.kerror == libkern::KError::NotFound.as_i32())
        })
    };
    let block = forward(1, b"block");
    let minted = forward(2, b"info-endpoint");
    let listing = forward(3, b"info");
    close(endpoint);
    for m in [&block, &minted, &listing].into_iter().flatten() {
        m.handles.iter().for_each(|&h| close(h));
    }
    if !refused(&block) {
        return fail(b"the info-only endpoint answered `block` as something other than NotFound");
    }
    if !refused(&minted) {
        return fail(b"the info-only endpoint minted another");
    }
    if !listing.as_ref().is_some_and(|m| !m.error && m.handles.len() == 1) {
        return fail(b"the info-only endpoint would not open its directory");
    }

    // **The information side: the manager agrees with the registry's present records** (Phase 6
    // Part C). It follows the table, so once it has caught up its directory names every present
    // device and no departed one, `all.tsm` has a row each, and every present USB device is
    // `usb-<id>.tsm`. `test-qemu`'s hot-plug may still be landing as this runs, so both are read
    // again until they agree, for five seconds at most. Until Part C the manager read the table
    // once, and this held it to a prefix of the registry.
    let usb = |r: &libkern::device::DeviceRecord| r.kind() == libkern::device::DeviceKind::UsbDevice;
    let mut attempts = 0u32;
    let (rows, usb_named, departed) = loop {
        let Some(registry) = registry_records(root_ns) else {
            return fail(b"the registry does not read");
        };
        let present: alloc::vec::Vec<&libkern::device::DeviceRecord> =
            registry.iter().filter(|r| !r.is_departed()).collect();
        let mut dirbuf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
        let Ok(mut dir) = librsproto::session::Dir::open(root_ns, b"/svc/devices/info", &mut dirbuf) else {
            return fail(b"/svc/devices/info is not a directory");
        };
        let (mut names, mut has_all, mut usb_named, mut departed_named) = (0usize, false, 0usize, false);
        let mut has_io = false;
        let listed = dir.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                names += 1;
                has_all |= e.name == b"all.tsm";
                has_io |= e.name == b"io.tsm";
                let id = e
                    .name
                    .strip_prefix(b"usb-")
                    .and_then(|n| n.strip_suffix(b".tsm"))
                    .and_then(|n| core::str::from_utf8(n).ok()?.parse::<usize>().ok());
                if let Some(r) = id.and_then(|id| registry.get(id)).filter(|r| usb(r)) {
                    usb_named += !r.is_departed() as usize;
                    departed_named |= r.is_departed();
                }
            }
            true
        });
        dir.close();
        if listed.is_err() {
            return fail(b"the directory would not list");
        }
        let (st, table) = ns_lookup(root_ns, b"/svc/devices/info/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let tbytes = if st == 0 { read_all(table) } else { None };
        close(table);
        // The object is page-sized; the decoder stops at the table's terminator.
        let rows = tbytes.and_then(|b| libstream::wire::Table::decode(&b).ok()).map(|t| t.rows.len());
        let present_usb = present.iter().filter(|r| usb(r)).count();
        // `all.tsm` and `io.tsm` (Phase 6 Part H.2), then a file per device.
        let agree = has_all
            && has_io
            && names == present.len() + 2
            && rows == Some(present.len())
            && usb_named == present_usb
            && !departed_named;
        if agree {
            break (present.len(), usb_named, registry.len() - present.len());
        }
        attempts += 1;
        if attempts == 50 {
            Line::new()
                .s(b"boot-probe: devices: ")
                .u(names as u64)
                .s(b" entries, ")
                .u(rows.unwrap_or(0) as u64)
                .s(b" rows and ")
                .u(usb_named as u64)
                .s(b" usb-<id> for ")
                .u(present.len() as u64)
                .s(b" present devices, ")
                .u(present_usb as u64)
                .s(b" of them USB")
                .s(if departed_named { b"; a departed device listed" } else { b"" })
                .end();
            return fail(b"the manager's tables never agreed with the registry's present records");
        }
        nap(100_000_000);
    };
    // **And a listed USB device's file is its table** (PR #356 review): one row, named `usb-<id>`,
    // of kind `usb`, driven by `xhci`. The listing alone says only that a name was made.
    let now = registry_records(root_ns).unwrap_or_default();
    if let Some(r) = now.iter().find(|r| usb(r) && !r.is_departed()) {
        let name = alloc::format!("usb-{}", r.id);
        let path = alloc::format!("/svc/devices/info/{name}.tsm");
        let (st, h) = ns_lookup(root_ns, path.as_bytes(), RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        let row = bytes
            .and_then(|b| libstream::wire::Table::decode(&b).ok())
            .and_then(|mut t| (t.rows.len() == 1).then(|| t.rows.remove(0)));
        let says = |i: usize, want: &str| {
            row.as_ref().and_then(|r| r.get(i)) == Some(&libstream::wire::Value::Str(alloc::string::String::from(want)))
        };
        // Columns: name, kind, path, size, description, parent, driver.
        if !(says(0, &name) && says(1, "usb") && says(6, "xhci")) {
            Line::new().s(b"boot-probe: devices: ").s(path.as_bytes()).s(b" is not that device's row").end();
            return fail(b"a USB device's file is not its table");
        }
    }
    Line::new()
        .s(b"boot-probe: devices: block held by the storage service, input held by input-server, the info-only endpoint refusing block, all.tsm has ")
        .u(rows as u64)
        .s(b" rows, ")
        .u(usb_named as u64)
        .s(b" of them usb-<id>, and ")
        .u(departed as u64)
        .s(b" departed records left out, agreeing with the registry ok")
        .end();
    true
}

/// **What the disks and the page cache did, as `io.tsm` says** (Phase 6 Part H.2), read before and
/// after a file of 64 pages is made on the scratch disk through the storage service and synced:
/// 1. **the scratch disk's writes moved, its written bytes by at least the file's, and its busy
///    time** — the RAM disk's driver counted them where it served them — **and by no more than
///    a few dozen commands** (Part H.4): 40 writes, the file's data in a write-back IRP a run of
///    pages, its zeroes in 64 KiB writes, and the metadata — measured, 25. A page a command, the
///    data and the zeroes took two writes a page and the file 145; a block at a time the allocator
///    read and wrote the bitmap, descriptor and superblock for each block, and it took 328 — **and
///    no read a page**, 32 in all: a page a grow just allocated fills as zero without reading it
///    (`UNWRITTEN`, the second fix), and the server reads its directories, inodes and bitmaps
///    once, through its metadata cache (the third). Measured, 12; uncached, 58; filling each page
///    as well, 115 to 122;
/// 2. **the page cache's write-backs moved, by at least the file's bytes** — bounded below only,
///    since every file's write-backs count there;
/// 3. every present disk and RAM disk has a row, and the page cache one;
/// 4. **the disk `init`'s root is on has read something** — at boot, through whichever driver it
///    has: AHCI on an installed disk, a RAM disk on the live stick;
/// 5. **a file of 64 pages the build wrote reads back as written, in at most eight reads** (Part
///    H.4): nothing has opened it, so its pages fill from the device, each fill reading up to 31
///    pages ahead of its own in the same IRP, to where its blocks jump on the device — measured,
///    4, for the three extents `mke2fs` laid it in — where a page a fill took 64; and the page
///    cache filled at least its bytes.
///
/// Each holds on every machine this runs on, which is every `--selftest` gate's, so none names a
/// device by position: the scratch disk by its label, the root's by what is mounted at `/`.
pub(super) fn io_counters_test(root_ns: u64) -> bool {
    use libkern::SYS_FILE_CREATE;
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: io counters: ").s(what).s(b" FAIL").end();
        false
    };
    let table = |path: &[u8]| {
        let (st, h) = ns_lookup(root_ns, path, RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        bytes.and_then(|b| Table::decode(&b).ok())
    };
    let col = |t: &Table, name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let cell = |t: &Table, row: usize, name: &str| col(t, name).map(|c| t.rows[row][c].clone());
    let row_named =
        |t: &Table, name: &str| (0..t.rows.len()).find(|&r| cell(t, r, "name") == Some(Value::Str(name.into())));
    let num = |t: &Table, name: &str, column: &str| match row_named(t, name).and_then(|r| cell(t, r, column)) {
        Some(Value::Int(v)) => Some(v.max(0) as u64),
        _ => None,
    };
    let text = |v: Option<Value>| match v {
        Some(Value::Str(s)) => Some(s),
        _ => None,
    };

    // The scratch disk by its label, and the disk the root is on by what is mounted at `/`.
    let (Some(storage), Some(devices)) = (table(b"/svc/storage/info/all.tsm"), table(b"/svc/devices/info/all.tsm"))
    else {
        return fail(b"the storage or device table would not read");
    };
    let named_where = |column: &str, value: &str| {
        (0..storage.rows.len())
            .find(|&r| text(cell(&storage, r, column)).as_deref() == Some(value))
            .and_then(|r| text(cell(&storage, r, "name")))
    };
    let (Some(scratch), Some(root)) = (named_where("label", "nitrox-scratch"), named_where("mounted", "/")) else {
        return fail(b"the storage table has no scratch disk or root");
    };
    let root_disk = match row_named(&devices, &root) {
        Some(r) if text(cell(&devices, r, "kind")).as_deref() == Some("partition") => text(cell(&devices, r, "parent")),
        Some(_) => Some(root.clone()),
        None => None,
    };
    let Some(root_disk) = root_disk else {
        return fail(b"the root's device is not in the device table");
    };
    let disks = (0..devices.rows.len())
        .filter(|&r| matches!(text(cell(&devices, r, "kind")).as_deref(), Some("disk") | Some("ramdisk")))
        .count();

    let Some(before) = table(b"/svc/devices/info/io.tsm") else {
        return fail(b"io.tsm would not read");
    };
    const PAGES: u64 = 64;
    let len = PAGES * PAGE;
    let Some(fh) = file_resize(SYS_FILE_CREATE, root_ns, b"/svc/storage/fs/nitrox-scratch/probe-io", len) else {
        return fail(b"a file would not create on the scratch disk");
    };
    let Some(addr) = map_file(fh, len, RIGHT_MAP_READ | RIGHT_MAP_WRITE) else {
        close(fh);
        return fail(b"the file would not map writable");
    };
    for i in 0..len {
        // SAFETY: `len` bytes are mapped writable at `addr`.
        unsafe { ((addr + i) as *mut u8).write_volatile((i % 251) as u8) };
    }
    // SAFETY: unmapping this call's own mapping, before the sync, as `libfs` does.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr, 0) };
    // SAFETY: register-only syscall on a file handle this process holds.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, fh) };
    close(fh);
    if synced != 0 {
        return fail(b"the file would not sync");
    }
    let Some(after) = table(b"/svc/devices/info/io.tsm") else {
        return fail(b"io.tsm would not read after the sync");
    };
    let moved = |name: &str, column: &str| Some(num(&after, name, column)?.saturating_sub(num(&before, name, column)?));

    let (Some(reads), Some(writes)) = (moved(&scratch, "reads"), moved(&scratch, "writes")) else {
        return fail(b"the scratch disk has no row in io.tsm");
    };
    let (Some(written), Some(busy)) = (moved(&scratch, "written_bytes"), moved(&scratch, "busy_ns")) else {
        return fail(b"the scratch disk has no row in io.tsm");
    };
    if writes == 0 || written < len || busy == 0 || writes > 40 || reads > 32 {
        Line::new()
            .s(b"boot-probe: io counters: the scratch disk moved ")
            .u(reads)
            .s(b" reads and ")
            .u(writes)
            .s(b" writes of ")
            .u(written)
            .s(b" bytes, busy ")
            .u(busy)
            .s(b" ns, for a file of ")
            .u(PAGES)
            .s(b" pages FAIL")
            .end();
        return false;
    }
    let (Some(wb), Some(wb_bytes)) = (moved("page-cache", "writes"), moved("page-cache", "written_bytes")) else {
        return fail(b"io.tsm has no page-cache row");
    };
    if wb == 0 || wb_bytes < len {
        return fail(b"the page cache's write-backs did not move by the file's pages");
    }
    if after.rows.len() != disks + 1 {
        return fail(b"io.tsm is not a row per present disk and RAM disk, and the page cache's");
    }
    if num(&after, &root_disk, "reads").unwrap_or(0) == 0 {
        return fail(b"the disk init's root is on has no reads counted");
    }

    // The file the build wrote, read through a mapping: nothing has opened it, so each page comes
    // from the device, a fill and the pages it reads ahead an IRP.
    let (st, ah) = ns_lookup(root_ns, b"/svc/storage/fs/nitrox-scratch/ahead", RIGHT_MAP_READ);
    if st != 0 {
        close(ah);
        return fail(b"the build's file would not open");
    }
    let Some(at) = map_file(ah, len, RIGHT_MAP_READ) else {
        close(ah);
        return fail(b"the build's file would not map");
    };
    let mut wrong = 0u64;
    for i in 0..len {
        // SAFETY: `len` bytes are mapped readable at `at`.
        if unsafe { ((at + i) as *const u8).read_volatile() } != (i % 251) as u8 {
            wrong += 1;
        }
    }
    // SAFETY: unmapping this call's own mapping.
    unsafe { syscall2(SYS_MEMORY_UNMAP, at, 0) };
    close(ah);
    let Some(read) = table(b"/svc/devices/info/io.tsm") else {
        return fail(b"io.tsm would not read after the read-back");
    };
    let since = |name: &str, column: &str| Some(num(&read, name, column)?.saturating_sub(num(&after, name, column)?));
    let (Some(ahead_reads), Some(ahead_bytes), Some(filled)) =
        (since(&scratch, "reads"), since(&scratch, "read_bytes"), since("page-cache", "read_bytes"))
    else {
        return fail(b"io.tsm lost a row");
    };
    if wrong != 0 || ahead_bytes < len || ahead_reads > 8 || filled < len {
        Line::new()
            .s(b"boot-probe: io counters: the build's file of ")
            .u(PAGES)
            .s(b" pages read back with ")
            .u(wrong)
            .s(b" bytes wrong in ")
            .u(ahead_reads)
            .s(b" reads of ")
            .u(ahead_bytes)
            .s(b" bytes, the page cache filling ")
            .u(filled)
            .s(b" FAIL")
            .end();
        return false;
    }
    Line::new()
        .s(b"boot-probe: io counters: ")
        .s(scratch.as_bytes())
        .s(b" moved ")
        .u(reads)
        .s(b" reads and ")
        .u(writes)
        .s(b" writes of ")
        .u(written)
        .s(b" bytes and ")
        .u(wb)
        .s(b" page-cache write-backs of ")
        .u(wb_bytes)
        .s(b" bytes for a file of ")
        .u(PAGES)
        .s(b" pages, and read the build's back in ")
        .u(ahead_reads)
        .s(b" reads of ")
        .u(ahead_bytes)
        .s(b" bytes; ")
        .u(disks as u64)
        .s(b" disks in io.tsm, the root's ")
        .s(root_disk.as_bytes())
        .s(b" with reads ok")
        .end();
    true
}
