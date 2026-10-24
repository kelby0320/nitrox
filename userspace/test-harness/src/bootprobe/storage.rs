//! The storage service's mount, listing and past-2-TiB tests, and a block device's bounds.

use super::*;

/// **A filesystem the storage service mounted, reached through it.**
///
/// The scratch disk is a RAM disk, and the service auto-mounted it at boot. Every step below goes
/// through `/svc/storage`, so the service answers `fs/nitrox-scratch/…` with `SUBNAMESPACE`, and
/// the kernel continues the resolve in the mount's own namespace, to the server the service spawned.
/// 1. `fs` lists the label, as a directory, and the image builder's `README` reads through it.
/// 2. **A file created and written through the service reaches the device**, read back from the
///    RAM disk itself, since a re-resolve would only read the page cache. A create is a resolve
///    with a size, so this also carries a continued operation through a real server.
/// 3. **A session endpoint, bound as a login supervisor will bind it**: at `/storage` with
///    the base `/fs`, and at `/dev/storage` with the base `/info`, in a namespace the probe builds.
///    The same file and the same table come back. Bound with no base, it will not mint another
///    endpoint.
/// 4. A label nothing is mounted under is `NotFound`.
pub(super) fn storage_mount_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_FILE_CREATE, SYS_NS_BIND, SYS_NS_CREATE};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage mount: ").s(what).s(b" FAIL").end();
        false
    };
    let read = |ns: u64, path: &[u8]| {
        let (st, h) = ns_lookup(ns, path, RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        bytes
    };

    let mut dirbuf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut dir) = librsproto::session::Dir::open(root_ns, b"/svc/storage/fs", &mut dirbuf) else {
        return fail(b"/svc/storage/fs is not a directory");
    };
    let mut listed_dir = false;
    let mut others = 0usize;
    let listed = dir.read_dir(|e| {
        if e.name == b"nitrox-scratch" {
            listed_dir = e.kind == librsproto::file::DIRENT_KIND_DIR;
        } else if e.name != b"." && e.name != b".." {
            others += 1;
        }
        true
    });
    dir.close();
    if listed.is_err() || !listed_dir || others != 0 {
        return fail(b"/svc/storage/fs is not the one directory nitrox-scratch");
    }

    if read(root_ns, b"/svc/storage/fs/nitrox-scratch/README").as_deref() != Some(SCRATCH_README) {
        return fail(b"the scratch disk's README did not read through /svc/storage");
    }

    const LEN: u64 = 3 * PAGE + 100;
    let path = b"/svc/storage/fs/nitrox-scratch/probe-write";
    let Some(fh) = file_resize(SYS_FILE_CREATE, root_ns, path, LEN) else {
        return fail(b"a file would not create through /svc/storage");
    };
    let Some(addr) = map_file(fh, LEN, RIGHT_MAP_READ | RIGHT_MAP_WRITE) else {
        close(fh);
        return fail(b"the created file would not map writable");
    };
    let pattern = |i: u64| (i * 7 + 3) as u8;
    for i in 0..LEN {
        // SAFETY: `LEN` bytes are mapped writable at `addr`.
        unsafe { ((addr + i) as *mut u8).write_volatile(pattern(i)) };
    }
    // Unmapped before the sync, as `libfs` does, so the write-back can leave the file clean.
    // SAFETY: unmapping this call's own mapping.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr, 0) };
    // SAFETY: register-only syscall on a file handle this process holds.
    let synced = unsafe { syscall1(SYS_FILE_SYNC, fh) };
    close(fh);
    if synced != 0 {
        return fail(b"the file made through /svc/storage would not sync");
    }
    let Some(t) = read(root_ns, b"/svc/storage/info/all.tsm").and_then(|b| libstream::wire::Table::decode(&b).ok())
    else {
        return fail(b"all.tsm would not read");
    };
    let name_c = t.schema.fields.iter().position(|f| f.name == "name");
    let label_c = t.schema.fields.iter().position(|f| f.name == "label");
    let device = name_c.zip(label_c).and_then(|(n, l)| {
        t.rows.iter().find_map(|row| match (&row[n], &row[l]) {
            (libstream::wire::Value::Str(name), libstream::wire::Value::Str(label)) if label == "nitrox-scratch" => {
                name.strip_prefix("blk-").map(|i| alloc::format!("/dev/blk/{i}"))
            }
            _ => None,
        })
    });
    let Some(raw) = device.and_then(|p| RootDevice::on(root_ns, p.as_bytes())) else {
        return fail(b"the scratch disk's device would not open raw");
    };
    let want: alloc::vec::Vec<u8> = (0..LEN).map(pattern).collect();
    if raw.read_file(b"/probe-write", 0, LEN as usize).as_deref() != Some(&want[..]) {
        return fail(b"what was written through /svc/storage is not on the device");
    }
    drop(raw);

    // 4. A session endpoint, bound as a supervisor will bind it.
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/storage/session-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no session endpoint at /svc/storage/session-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the session endpoint into");
    }
    let ns = ns as u64;
    let bind = |at: &[u8], base: &[u8]| {
        // SAFETY: a namespace this process created, valid path and base, an endpoint it holds.
        let r = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                at.as_ptr() as u64,
                at.len() as u64,
                endpoint,
                base.as_ptr() as u64,
                base.len() as u64,
            )
        };
        r == 0
    };
    let bound = bind(b"/storage", b"/fs") && bind(b"/dev/storage", b"/info") && bind(b"/raw", b"");
    close(endpoint);
    if !bound {
        close(ns);
        return fail(b"the session endpoint would not bind as a supervisor binds it");
    }
    let readme = read(ns, b"/storage/nitrox-scratch/README");
    let rows =
        read(ns, b"/dev/storage/all.tsm").and_then(|b| libstream::wire::Table::decode(&b).ok()).map(|t| t.rows.len());
    let (minted, other) = ns_lookup(ns, b"/raw/session-endpoint", chan);
    close(other);
    close(ns);
    if readme.as_deref() != Some(SCRATCH_README) {
        return fail(b"/storage/nitrox-scratch/README did not read through a session endpoint");
    }
    if rows != Some(t.rows.len()) {
        return fail(b"/dev/storage/all.tsm is not the table /svc/storage serves");
    }
    if minted != KError::NotFound.as_i32() {
        return fail(b"a session endpoint minted another");
    }

    let (st, h) = ns_lookup(root_ns, b"/svc/storage/fs/no-such-label/README", RIGHT_MAP_READ);
    close(h);
    if st != KError::NotFound.as_i32() {
        return fail(b"a label nothing is mounted under was not NotFound");
    }
    kprint(b"boot-probe: storage mount: nitrox-scratch listed, its README read and a new file written to the device through /svc/storage, the same through a session endpoint bound at /storage and /dev/storage, which minted nothing, and an unknown label NotFound ok\n");
    true
}

/// **A directory whose listing fills its pages to the brim lists whole**: 200 files
/// with names long enough that a page of entries reaches the most one message carries, made on the
/// scratch disk and listed through a session, as `list` and `copy` list one. A reply sized by the
/// page rather than by what a message carries is one the kernel refuses, the server never knows,
/// and the client waits for good — here, `test-qemu` timing out at this step.
pub(super) fn long_listing_test(root_ns: u64) -> bool {
    use libkern::SYS_FILE_CREATE;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: long listing: ").s(what).s(b" FAIL").end();
        false
    };
    const FILES: usize = 200;
    const PREFIX: &str = "a-name-long-enough-to-fill-a-listing-page-";
    let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut scratch) = librsproto::session::Dir::open(root_ns, b"/svc/storage/fs/nitrox-scratch", &mut buf) else {
        return fail(b"the scratch disk would not open as a directory");
    };
    let made = scratch.mkdir(b"probe-many");
    scratch.close();
    if made.is_err() {
        return fail(b"probe-many would not make");
    }
    for i in 0..FILES {
        let path = alloc::format!("/svc/storage/fs/nitrox-scratch/probe-many/{PREFIX}{i:03}");
        let Some(h) = file_resize(SYS_FILE_CREATE, root_ns, path.as_bytes(), 0) else {
            return fail(b"a file would not create in probe-many");
        };
        close(h);
    }
    let Ok(mut dir) = librsproto::session::Dir::open(root_ns, b"/svc/storage/fs/nitrox-scratch/probe-many", &mut buf)
    else {
        return fail(b"probe-many would not open as a directory");
    };
    let mut listed = 0usize;
    let read = dir.read_dir(|e| {
        listed += e.name.starts_with(PREFIX.as_bytes()) as usize;
        true
    });
    dir.close();
    if read.is_err() || listed != FILES {
        Line::new()
            .s(b"boot-probe: long listing: ")
            .u(listed as u64)
            .s(b" of ")
            .u(FILES as u64)
            .s(b" listed FAIL")
            .end();
        return false;
    }
    Line::new()
        .s(b"boot-probe: long listing: ")
        .u(FILES as u64)
        .s(b" files of ")
        .u((PREFIX.len() + 3) as u64)
        .s(b"-byte names listed whole, their pages filled to what a message carries ok")
        .end();
    true
}

/// **What `test-qemu` writes past 2 TiB**: a marker, then bytes no two of which a
/// sector at another place would share. The host reads it back out of the image.
const PAST_TWO_TIB: &[u8; 32] = b"nitrox: past two tebibytes, G.5\n";

/// **The block it goes at**: seven past the last a ten-byte command can name.
const PAST_TWO_TIB_BLOCK: u64 = (1 << 32) + 7;

/// **A USB disk of more than 2^32 blocks** (sixteen-byte commands): a sector
/// written through its node past 2 TiB — where a ten-byte command's block number would wrap — and
/// read back, the page cleared between. **On a machine with no such disk it says so and passes**:
/// every `--selftest` gate runs this, and only `test-qemu` attaches one, which holds this line.
pub(super) fn past_two_tib_test(root_ns: u64) -> bool {
    use libkern::device::DeviceKind;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: past 2 TiB: ").s(what).s(b" FAIL").end();
        false
    };
    let Some(all) = registry_records(root_ns) else {
        return fail(b"the registry does not read");
    };
    let Some(r) = all.iter().find(|r| {
        r.kind() == DeviceKind::Disk
            && !r.is_departed()
            && r.driver() == b"usb-storage"
            && r.block_count > u32::MAX as u64
    }) else {
        kprint(b"boot-probe: past 2 TiB: no USB disk of more than 2^32 blocks here; test-qemu attaches one\n");
        return true;
    };
    let path = alloc::format!("/dev/blk/{}", r.served);
    let (st, dev) = ns_lookup(root_ns, path.as_bytes(), libkern::RIGHT_READ | libkern::RIGHT_WRITE);
    if st != 0 || dev == 0 {
        return fail(b"its node would not open writable");
    }
    // SAFETY: register-only syscall.
    let page = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    let addr = if page > 0 {
        // SAFETY: `page` is a memory object we just created; the result is checked below
        unsafe { syscall4(SYS_MEMORY_MAP, page as u64, 0, PAGE, RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE) }
    } else {
        -1
    };
    if addr <= 0 {
        close(dev);
        if page > 0 {
            close(page as u64);
        }
        return fail(b"no page to move it through");
    }
    let (page, addr) = (page as u64, addr as u64);
    let want: alloc::vec::Vec<u8> = (0..512u32)
        .map(|i| if (i as usize) < PAST_TWO_TIB.len() { PAST_TWO_TIB[i as usize] } else { (i * 31 + 7) as u8 })
        .collect();
    // SAFETY: `addr` maps PAGE writable bytes, and 512 are written.
    let buf = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, 512) };
    let io = |opcode: u32| {
        let op = IoOp { opcode, flags: 0, buffer: page, buf_offset: 0, offset: PAST_TWO_TIB_BLOCK * 512, length: 512 };
        // SAFETY: a valid `IoOp`; `dev` and `page` are handles this process holds.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, dev, (&op as *const IoOp) as u64) };
        po > 0 && po_wait(po as u64) == (0, 512)
    };
    buf.copy_from_slice(&want);
    let wrote = io(libkern::abi::IO_OPCODE_WRITE);
    buf.fill(0);
    let read = wrote && io(IO_OPCODE_READ);
    let same = read && buf == &want[..];
    // SAFETY: unmapping this call's own mapping.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr, 0) };
    close(page);
    close(dev);
    if !wrote {
        return fail(b"the write past 2 TiB failed");
    }
    if !same {
        return fail(b"what was read back past 2 TiB is not what was written");
    }
    Line::new()
        .s(b"boot-probe: past 2 TiB: blk-")
        .u(r.served as u64)
        .s(b", ")
        .u(r.block_count)
        .s(b" blocks: a sector written and read back at block ")
        .u(PAST_TWO_TIB_BLOCK)
        .s(b" ok")
        .end();
    true
}

/// **The storage service owns the disks and says what is on them**,
/// read through the root endpoint, since the probe runs in the root namespace.
/// - `all.tsm` has a row per block record, in registry order: the device manager's replay reached
///   its owner whole.
/// - `nitrox-root` is `init`'s, at `/`, and the only row mounted there, so nothing mounts the root
///   twice. The service mounts nothing on a boot with nothing to mount: the ESP is FAT, which it
///   recognises and does not serve, and the disk holds only its table.
/// - What each device holds is what the image builder put there, read off the disks on a boot.
pub(super) fn storage_test(root_ns: u64) -> bool {
    use alloc::string::String;
    use libkern::device::{DeviceKind, DeviceRecord};
    use libstream::wire::Value;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: storage: ").s(what).s(b" FAIL").end();
        false
    };
    let Some(registry) = registry_records(root_ns) else {
        return fail(b"the registry does not read");
    };
    let block: alloc::vec::Vec<&DeviceRecord> = registry.iter().filter(|r| r.kind().is_block()).collect();
    let (st, table) = ns_lookup(root_ns, b"/svc/storage/info/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
    let bytes = if st == 0 { read_all(table) } else { None };
    close(table);
    let Some(bytes) = bytes else {
        return fail(b"/svc/storage/info/all.tsm would not map");
    };
    let Ok(t) = libstream::wire::Table::decode(&bytes) else {
        return fail(b"all.tsm is not a TSM1 table");
    };
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let (Some(name_c), Some(fs_c), Some(at_c), Some(by_c), Some(mode_c), Some(clean_c)) =
        (col("name"), col("filesystem"), col("mounted"), col("by"), col("mode"), col("clean"))
    else {
        return fail(b"all.tsm lacks a column");
    };
    let text = |row: usize, c: usize| match &t.rows[row][c] {
        Value::Str(s) => Some(s.as_str()),
        _ => None,
    };
    let name = |r: &DeviceRecord| alloc::format!("blk-{}", r.served);
    let want: alloc::vec::Vec<String> = block.iter().map(|r| name(r)).collect();
    let got: alloc::vec::Vec<&str> = (0..t.rows.len()).map(|i| text(i, name_c).unwrap_or("")).collect();
    if got != want {
        Line::new()
            .s(b"boot-probe: storage: ")
            .u(got.len() as u64)
            .s(b" rows for ")
            .u(want.len() as u64)
            .s(b" block records")
            .end();
        return fail(b"all.tsm is not a row per block record, in registry order");
    }
    let row = |r: &DeviceRecord| want.iter().position(|n| *n == name(r));
    let find = |kind: DeviceKind, label: &[u8]| {
        block.iter().find(|r| r.kind() == kind && (label.is_empty() || r.name() == label)).and_then(|r| row(r))
    };

    let Some(root) = find(DeviceKind::Partition, b"nitrox-root") else {
        return fail(b"no nitrox-root partition to check the root against");
    };
    let at_root: alloc::vec::Vec<usize> = (0..t.rows.len()).filter(|&i| text(i, at_c) == Some("/")).collect();
    if at_root != [root] {
        return fail(b"the root is not one row, nitrox-root's");
    }
    if text(root, by_c) != Some("init") || text(root, mode_c) != Some("rw") || text(root, fs_c) != Some("ext4") {
        return fail(b"the root is not init's writable ext4");
    }
    if t.rows[root][clean_c] != Value::Null {
        return fail(b"a root mounted writable reported how it was left");
    }
    // **What the service mounted is the test image's scratch disk, and only it**: the
    // one filesystem on this boot that is neither `init`'s nor FAT.
    let by_storage: alloc::vec::Vec<usize> = (0..t.rows.len()).filter(|&i| text(i, by_c) == Some("storage")).collect();
    let Some(scratch) = find(DeviceKind::RamDisk, b"") else {
        return fail(b"no RAM disk: the test image's scratch module did not arrive");
    };
    if by_storage != [scratch] {
        return fail(b"the service did not mount exactly the scratch disk");
    }
    let (Some(label_c), Some(kind_c)) = (col("label"), col("kind")) else {
        return fail(b"all.tsm lacks a column");
    };
    if text(scratch, at_c) != Some("/storage/nitrox-scratch")
        || text(scratch, mode_c) != Some("rw")
        || text(scratch, fs_c) != Some("ext4")
        || text(scratch, label_c) != Some("nitrox-scratch")
        || text(scratch, kind_c) != Some("ramdisk")
    {
        return fail(b"the scratch disk is not mounted writable at /storage/nitrox-scratch");
    }
    let (Some(esp), Some(disk)) = (find(DeviceKind::Partition, b"NITROX_ESP"), find(DeviceKind::Disk, b"")) else {
        return fail(b"no ESP or disk to check what they hold");
    };
    if text(esp, fs_c) != Some("fat") {
        return fail(b"the ESP is not FAT");
    }
    if t.rows[disk][fs_c] != Value::Null {
        return fail(b"the disk, which holds only its table, reported a filesystem");
    }

    let mut dirbuf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut dir) = librsproto::session::Dir::open(root_ns, b"/svc/storage/info", &mut dirbuf) else {
        return fail(b"/svc/storage/info is not a directory");
    };
    let mut names = 0usize;
    let listed = dir.read_dir(|e| {
        names += (e.name != b"." && e.name != b"..") as usize;
        true
    });
    dir.close();
    if listed.is_err() || names != block.len() + 1 {
        return fail(b"the directory is not all.tsm and a file per device");
    }
    let (st, other) = ns_lookup(root_ns, b"/svc/storage/block", RIGHT_MAP_READ);
    close(other);
    if st != libkern::KError::NotFound.as_i32() {
        return fail(b"a suffix the service does not serve was not NotFound");
    }
    Line::new()
        .s(b"boot-probe: storage: a row per block record (")
        .u(block.len() as u64)
        .s(b"), nitrox-root init's at /, the scratch disk the service's at /storage/nitrox-scratch, the ESP fat and the disk holding its table ok")
        .end();
    true
}

/// **A block request off its device's blocks is refused**, on the root partition: part of a block
/// with no operation made, a range past its end through the operation. The last block still reads.
pub(super) fn block_bounds_test(root_ns: u64) -> bool {
    use libkern::{IO_OPCODE_READ, KError};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: block bounds FAIL: ").s(why).end();
        false
    };
    let Some(records) = registry_records(root_ns) else {
        return fail(b"the registry does not read");
    };
    let labels: [&[u8]; 2] = [b"nitrox-root", b"nitrox-live"];
    let Some(root) = records.iter().find(|r| labels.contains(&r.name())) else {
        return fail(b"no root partition in the registry");
    };
    let Some(index) = root.block_index() else {
        return fail(b"the root partition has no /dev/blk index");
    };
    let path = alloc::format!("/dev/blk/{index}");
    let (st, dev) = ns_lookup(root_ns, path.as_bytes(), RIGHT_READ);
    if st != 0 || dev == 0 {
        return fail(b"the root partition does not open");
    }
    // SAFETY: register-only syscall.
    let page = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if page <= 0 {
        close(dev);
        return fail(b"no page to read into");
    }
    let page = page as u64;
    let submit = |offset: u64, length: u64| {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: page, buf_offset: 0, offset, length };
        // SAFETY: `op` is a valid `IoOp`; `dev` and `page` are handles this process holds.
        unsafe { syscall2(SYS_IO_SUBMIT, dev, (&op as *const IoOp) as u64) }
    };
    let invalid = KError::InvalidArgument.as_i32();
    let partial = [(0, 100), (100, 512), (0, 600)].iter().all(|&(o, l)| submit(o, l) == invalid as i64);
    let end = root.block_count * root.logical_block_size as u64;
    let past = submit(end, 512);
    let past = past >= 0 && po_wait(past as u64).0 == invalid;
    let last = submit(end - 512, 512);
    let last = last >= 0 && po_wait(last as u64).0 == 0;
    close(page);
    close(dev);
    if !partial {
        return fail(b"part of a block was taken");
    }
    if !past {
        return fail(b"a read past the end did not fail InvalidArgument");
    }
    if !last {
        return fail(b"the last block did not read");
    }
    kprint(b"boot-probe: block bounds: part of a block refused, past the end failed, the last block read ok\n");
    true
}
