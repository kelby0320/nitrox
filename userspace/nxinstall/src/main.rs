//! `nxinstall` — install this system onto a disk, with an account of its own.
//!
//! ```text
//! with admin nxinstall              # the disks it can use, and why any other is in use
//! with admin nxinstall /dev/blk/2   # what it would do; then, asked, `yes`, an account, and do it
//! with admin nxinstall --help
//! ```
//!
//! It holds no authority of its own: it reaches a disk only when the view it runs in has one
//! bound, by the `disks` grant of `with admin` or `with install`. Absence is the sandbox; no check
//! in this program is what stops it. A disk the view lacks is one in use, and is named as such on
//! `stderr`, with the command that frees it when the storage service mounted it.
//!
//! Naming a disk shows the plan and asks, in one write to the terminal: the disk, what goes on
//! it, that everything on it is lost, and `type yes to go ahead:`. Anything but `yes` writes
//! nothing. A whole word rather than `[y/N]`, because a single key is answered before the
//! question is read. It then asks for the first account with `libprompt`: a name, echo on, and a
//! password twice, echo off. Nothing is written until all three questions are answered.
//!
//! It writes a GPT with two partitions, the installable ESP module whole onto the boot partition,
//! and on the root partition a new filesystem holding the pristine root's files
//! (`install-root.img`) but not the build's accounts. `/home` is made empty, and `/system/users`
//! and `/system/views.toml` are the new machine's own: its one account, the seeded policy making
//! it the administrator (`view_broker::policy::seed`), and its home.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use fs_server_ext4::mkfs;
use libfsserver::disk::PartitionIo;
use libgpt::table::{self, BACK_BYTES, BLOCK, FRONT_BYTES};
use libkern::abi::{
    BlockDeviceInfo, BlockKind, IO_OPCODE_FLUSH, IO_OPCODE_READ, IO_OPCODE_WRITE, IPC_PAYLOAD_SIZE, IoOp,
};
use libkern::handle::{RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_READ, RIGHT_WRITE};
use libkern::syscall::{
    SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_ENTROPY_CREATE, SYS_ENTROPY_READ, SYS_HANDLE_CLOSE, SYS_IO_SUBMIT,
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_NS_LOOKUP, SYS_WAIT, syscall1, syscall2, syscall4, syscall6,
};
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort, MsgPort};
use libstream::diag::Level;
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// What a failure before the program is running exits with: a malformed setup message, or a
/// panic. Every other status comes from [`nxinstall::Outcome`].
const EXIT_FAILURE: i64 = 1;

/// The transfer buffer, in bytes: 256 KiB, which is 64 pages and so 64 scatter-gather fragments,
/// inside what one AHCI command can describe (`MAX_PRDT_ENTRIES` is 248).
const CHUNK: u64 = 256 * 1024;

/// The GPT name of the partition the root is copied from, which is how the root source is
/// recognised among the RAM disks. Not the live root's `nitrox-live`, which the session writes to
/// and `init` has mounted.
const SOURCE_LABEL: &[u8] = libgpt::INSTALL_SOURCE_LABEL.as_bytes();

/// What `--help` prints, a line each, on `stdout`.
const HELP: &[&str] = &[
    "usage: with admin nxinstall [DEVICE]",
    "",
    "Install Nitrox onto a disk, erasing it, with a first account of its own.",
    "",
    "  with admin nxinstall          list the disks it can install to, and say",
    "                                why any other is in use and how to free it",
    "  with admin nxinstall DEVICE   show what installing to DEVICE would do,",
    "                                ask before going ahead, ask for the new",
    "                                machine's first account, and install",
    "",
    "DEVICE is a disk as the listing names it, such as /dev/blk/0. Nothing is",
    "written until every question is answered.",
    "",
    "Run it from the live stick's \"install to this machine\" entry, through",
    "`with admin`: the disks come with the view, and nxinstall alone has none.",
];

/// The block size of every filesystem this system makes or reads. 4 KiB is the reader's scratch
/// and the page size, so a file's blocks map to pages.
const FS_BLOCK: u64 = 4096;

/// Bytes of filesystem per inode, `mke2fs -i`'s default. `mkfs` caps the total, so this governs
/// small filesystems and the cap large ones.
const BYTES_PER_INODE: u32 = 16384;

static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// `sys_wait` results: one 24-byte `IoResult`, in a buffer rounded up to 32.
static mut WAIT_RESULTS: [u8; 32] = [0; 32];

static mut FRONT: [u8; FRONT_BYTES] = [0; FRONT_BYTES];
static mut BACK: [u8; BACK_BYTES] = [0; BACK_BYTES];

fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: valid single-waiter buffers; the PO handle is ours.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: closing the PO we own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    // SAFETY: written by the syscall above.
    unsafe {
        let r = (&raw const WAIT_RESULTS).read();
        let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        let value = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
        (status, value)
    }
}

fn lookup(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid namespace handle and path slice.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return None;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 || handle == 0 { None } else { Some(handle) }
}

fn scratch(size: u64) -> Option<(u64, u64)> {
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, size, 0, 0, 0) };
    if mem < 0 {
        return None;
    }
    let mem = mem as u64;
    // SAFETY: register-only syscall; `mem` is ours.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return None;
    }
    Some((mem, addr as u64))
}

/// 16 bytes from the kernel CSPRNG, for a partition GUID.
///
/// Refuses rather than inventing one: constant GUIDs would collide with every other machine this
/// program installed.
fn random_guid() -> Option<[u8; 16]> {
    // SAFETY: register-only syscall.
    let h = unsafe { syscall1(SYS_ENTROPY_CREATE, 0) };
    if h < 0 {
        return None;
    }
    let h = h as u64;
    let mut buf = [0u8; 16];
    // Read again after an unseeded pool's PO: it completing says only that the pool is now
    // seeded and writes nothing, so the buffer would be sixteen zeros.
    let ok = loop {
        // SAFETY: a valid writable 16-byte buffer.
        let r = unsafe { syscall4(SYS_ENTROPY_READ, h, (&raw mut buf) as u64, 16, 0) };
        if r == 0 {
            break true;
        }
        if r < 0 || po_wait(r as u64).0 != 0 {
            break false;
        }
    };
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    ok.then_some(buf)
}

/// One block device this view can reach.
struct Device {
    index: usize,
    /// The device handle, with read and write: [`devices`] asks for both, so a device bound
    /// read-only is left out of the list.
    handle: u64,
    info: BlockDeviceInfo,
}

impl Device {
    fn path(&self) -> String {
        format!("/dev/blk/{}", self.index)
    }

    fn name(&self) -> String {
        String::from_utf8_lossy(self.info.name()).into_owned()
    }

    fn read_at(&self, io: &Io, at: u64, buf: &mut [u8]) -> Result<(), &'static str> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = core::cmp::min(CHUNK as usize, buf.len() - done);
            io.submit(IO_OPCODE_READ, self.handle, at + done as u64, n as u64)?;
            // SAFETY: `io.addr` maps `CHUNK` bytes read-write and `n <= CHUNK`.
            let src = unsafe { core::slice::from_raw_parts(io.addr as *const u8, n) };
            buf[done..done + n].copy_from_slice(src);
            done += n;
        }
        Ok(())
    }

    fn write_at(&self, io: &Io, at: u64, buf: &[u8]) -> Result<(), &'static str> {
        let mut done = 0usize;
        while done < buf.len() {
            let n = core::cmp::min(CHUNK as usize, buf.len() - done);
            // SAFETY: `io.addr` maps `CHUNK` bytes read-write and `n <= CHUNK`.
            let dst = unsafe { core::slice::from_raw_parts_mut(io.addr as *mut u8, n) };
            dst.copy_from_slice(&buf[done..done + n]);
            io.submit(IO_OPCODE_WRITE, self.handle, at + done as u64, n as u64)?;
            done += n;
        }
        Ok(())
    }
}

/// The transfer buffer every copy goes through: one `MemoryObject`, mapped once.
struct Io {
    mem: u64,
    addr: u64,
}

impl Io {
    /// Submit one transfer and wait for it. `length` must be at most [`CHUNK`].
    fn submit(&self, opcode: u32, device: u64, offset: u64, length: u64) -> Result<(), &'static str> {
        let op = IoOp { opcode, flags: 0, buffer: self.mem, buf_offset: 0, offset, length };
        // SAFETY: `device` is a block device handle; `&op` is a valid `IoOp`.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err("the device refused the transfer");
        }
        let (status, moved) = po_wait(po as u64);
        if status != 0 {
            return Err("the transfer failed");
        }
        if moved != length {
            return Err("the device moved fewer bytes than asked");
        }
        Ok(())
    }
}

/// Every block device in `ns`, in index order. Listed, not probed: probing `0`, `1`, … would stop
/// at the first gap. An empty result is the ordinary answer in an ordinary session.
fn devices(ns: u64) -> Vec<Device> {
    let mut indices: Vec<usize> =
        libfs::ns_children(ns, b"/dev/blk").iter().filter_map(|(name, _)| nxinstall::block_index(name)).collect();
    indices.sort_unstable();
    indices.dedup();
    let mut out = Vec::new();
    for index in indices {
        let path = format!("/dev/blk/{index}");
        let Some(handle) = lookup(ns, path.as_bytes(), RIGHT_READ | RIGHT_WRITE) else {
            continue;
        };
        let ipath = format!("/dev/blk/{index}/info");
        let info = lookup(ns, ipath.as_bytes(), RIGHT_MAP_READ)
            .and_then(|h| {
                let size = core::mem::size_of::<BlockDeviceInfo>() as u64;
                // SAFETY: register-only syscall.
                let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, 4096, RIGHT_MAP_READ) };
                // SAFETY: closing our own handle; the mapping outlives it.
                unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
                if addr < 0 {
                    return None;
                }
                // SAFETY: `addr` maps a page read-only and `size` is under it.
                let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size as usize) };
                BlockDeviceInfo::read(bytes)
            })
            // A device whose info would not map reads as `Unknown`, which every destructive
            // path here refuses.
            .unwrap_or_default();
        out.push(Device { index, handle, info });
    }
    out
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = bytes;
    let mut unit = 0;
    while n >= 1024 && unit + 1 < UNITS.len() {
        n /= 1024;
        unit += 1;
    }
    format!("{} {}", n, UNITS[unit])
}

/// The two RAM disks this program copies from.
struct Sources<'a> {
    /// The installable ESP, copied onto the boot partition whole.
    esp: &'a Device,
    /// The pristine root image, whose *inner* partition is copied onto the root partition.
    root: &'a Device,
    /// Where the filesystem sits inside `root`: `(first block, block count)`.
    root_extent: (u64, u64),
}

/// Find the two sources among the RAM disks by what they contain, not what they are called: a FAT
/// boot sector, and a partition table naming [`SOURCE_LABEL`].
fn sources<'a>(devs: &'a [Device], io: &Io) -> Result<Sources<'a>, String> {
    let mut esp = None;
    let mut root = None;
    let mut root_extent = (0, 0);
    let mut front = [0u8; FRONT_BYTES];
    for d in devs.iter().filter(|d| d.info.kind() == BlockKind::RamDisk) {
        if d.read_at(io, 0, &mut front).is_err() {
            continue;
        }
        // A FAT boot sector: the 0x55AA signature at the end of block 0, and the family name at
        // 82 (FAT32) or 54 (FAT12/16).
        let fat = front[510] == 0x55 && front[511] == 0xAA && (&front[82..87] == b"FAT32" || &front[54..57] == b"FAT");
        if fat && esp.is_none() {
            esp = Some(d);
            continue;
        }
        if let Ok(t) = table::read(&front)
            && let Some(p) = t.by_name(SOURCE_LABEL)
        {
            root = Some(d);
            root_extent = (p.first_lba, p.blocks());
        }
    }
    match (esp, root) {
        (Some(esp), Some(root)) => Ok(Sources { esp, root, root_extent }),
        (None, _) => Err(String::from(
            "no installable ESP among this view's devices. That module is carried by the \
             live image's install entry alone — an ordinary live boot does not load it.",
        )),
        (_, None) => Err(format!(
            "no root to install among this view's devices: none of them holds a partition \
             named {}. That module, a pristine copy of the system, is carried by the live image's \
             install entry alone, as the ESP is.",
            String::from_utf8_lossy(SOURCE_LABEL)
        )),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the two devices, offsets and the progress hook are separate inputs of one copy"
)]
fn copy(
    io: &Io,
    src: &Device,
    src_at: u64,
    dst: &Device,
    dst_at: u64,
    bytes: u64,
    what: &str,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let mut done = 0u64;
    let mut reported = 0u64;
    while done < bytes {
        let n = core::cmp::min(CHUNK, bytes - done);
        io.submit(IO_OPCODE_READ, src.handle, src_at + done, n).map_err(|e| format!("reading {what}: {e}"))?;
        io.submit(IO_OPCODE_WRITE, dst.handle, dst_at + done, n).map_err(|e| format!("writing {what}: {e}"))?;
        done += n;
        if done - reported >= 8 * 1024 * 1024 || done == bytes {
            reported = done;
            say(&format!("  {what}: {} of {}", human(done), human(bytes)));
        }
    }
    Ok(())
}

/// Where the two partitions would go on `target`, or why they cannot. Worked out before the
/// confirmation, so a disk that cannot take the install is refused while the plan is on screen.
fn layout_for(target: &Device, srcs: &Sources) -> Result<nxinstall::Layout, String> {
    let block = target.info.logical_block_size.max(1) as u64;
    nxinstall::plan(
        target.info.block_count,
        target.info.logical_block_size,
        srcs.esp.info.byte_capacity() / BLOCK as u64,
        srcs.root_extent.1,
    )
    .map_err(|e| match e {
        nxinstall::PlanError::BlockSize(n) => format!(
            "this disk addresses {n}-byte blocks, and this installer writes tables in 512-byte \
             blocks. Nothing was written."
        ),
        nxinstall::PlanError::TooSmall { have, need } => format!(
            "this disk holds {} and the install needs {}. Nothing was written.",
            human(have * block),
            human(need * block)
        ),
    })
}

fn install(
    io: &Io,
    target: &Device,
    srcs: &Sources,
    layout: &nxinstall::Layout,
    account: &Account,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let block = target.info.logical_block_size as u64;
    let esp_bytes = srcs.esp.info.byte_capacity();
    // Every timestamp is the install's own; an unreadable clock leaves the epoch.
    let now = wall_clock_seconds();

    let (esp_guid, root_guid) = match (random_guid(), random_guid()) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(String::from(
                "the kernel would not give out random bytes, and a partition table with \
                 invented GUIDs is one that collides with every other machine. Nothing was \
                 written.",
            ));
        }
    };
    let disk_guid = random_guid().ok_or_else(|| String::from("no entropy for the disk GUID"))?;

    // The table first, and the filesystems after: a disk with a table and empty partitions can
    // be installed to again; the other order leaves bytes nobody can find.
    say("writing the partition table");
    log(&format!(
        "installing to {} ({}), boot {} + root {}",
        target.path(),
        target.name(),
        human(esp_bytes),
        human(layout.root_blocks() * block)
    ));
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded program; these statics have no other reader.
    let (front, back) = unsafe { (&mut *(&raw mut FRONT), &mut *(&raw mut BACK)) };
    table::build(target.info.block_count, disk_guid, &layout.partitions(esp_guid, root_guid), front, back)
        .map_err(|e| format!("the partition table would not build: {e:?}. Nothing was written."))?;
    target.write_at(io, 0, front).map_err(|e| format!("writing the partition table: {e}"))?;
    let back_at = (target.info.block_count - table::ARRAY_BLOCKS - 1) * BLOCK as u64;
    target.write_at(io, back_at, back).map_err(|e| format!("writing the backup partition table: {e}"))?;

    say(&format!("copying the boot partition ({})", human(esp_bytes)));
    copy(io, srcs.esp, 0, target, layout.esp_first * BLOCK as u64, esp_bytes, "boot", say)?;

    let root_bytes_dst = layout.root_blocks() * block;
    say(&format!("making a filesystem of {}", human(root_bytes_dst)));
    let dst = match PartitionIo::new(target.handle, layout.root_first * BLOCK as u64, root_bytes_dst) {
        Some(p) => p,
        None => return Err(String::from("could not allocate a transfer buffer for the target")),
    };
    let uuid = random_guid().ok_or_else(|| {
        String::from(
            "the kernel would not give out random bytes, and a filesystem with an invented \
             UUID collides with every other machine's. Nothing further was written.",
        )
    })?;
    let geom = mkfs::format(
        &dst,
        &mkfs::Params {
            blocks: root_bytes_dst / FS_BLOCK,
            block_size: FS_BLOCK as u32,
            bytes_per_inode: BYTES_PER_INODE,
            uuid,
            label: *b"nitrox-root\0\0\0\0\0",
            now,
        },
        // The slow part: about 300 MiB of scattered writes on a terabyte, one command at a time,
        // which a person watching a still screen cannot tell from a hang.
        &mut |done, total| {
            if done == 1 || done == total || done % 256 == 0 {
                say(&format!("  group {done} of {total}"));
            }
        },
    )
    .map_err(|e| {
        format!(
            "the filesystem would not lay out: {e:?}. The disk is partitioned \
                          and its boot partition written; nothing readable is on the root."
        )
    })?;
    say(&format!("  {} inodes across {} group(s)", geom.inodes_count, geom.groups));

    let src = match PartitionIo::new(
        srcs.root.handle,
        srcs.root_extent.0 * BLOCK as u64,
        srcs.root_extent.1 * BLOCK as u64,
    ) {
        Some(p) => p,
        None => return Err(String::from("could not allocate a transfer buffer for the source")),
    };
    say("copying the root filesystem");
    // Logged so a gate can assert the pristine copy was the source, not the live root.
    log(&format!("copying the root from {}'s {}", srcs.root.path(), String::from_utf8_lossy(SOURCE_LABEL)));
    let copied = nxinstall::copy::copy_tree(&src, &dst, now, nxinstall::account::PASS_OVER, say)
        .map_err(|e| format!("copying the root filesystem failed: {e:?}"))?;
    say(&format!("  {} director(ies), {} file(s), {}", copied.dirs, copied.files, human(copied.bytes)));

    say(&format!("making {}'s account, its administrator", String::from_utf8_lossy(&account.name)));
    let salt = random_guid().ok_or("the kernel would not give out random bytes for a salt")?;
    nxinstall::account::write(&dst, &account.name, &account.password, &salt, now)
        .map_err(|e| format!("writing the first account failed: {e}"))?;
    log(&format!("the first account is {}, its administrator", String::from_utf8_lossy(&account.name)));

    log(&format!(
        "wrote the partition table, the boot partition, and a {} root holding {} file(s)",
        human(root_bytes_dst),
        copied.files
    ));
    // Durable before "done": a drive may hold the last sectors in its volatile cache, and the
    // next thing a person does is take the power away.
    flush(target.handle).map_err(String::from)?;
    log("flushed the disk's write cache");
    Ok(())
}

/// Have `device` write its volatile cache to its medium and wait: an `IO_OPCODE_FLUSH`.
fn flush(device: u64) -> Result<(), &'static str> {
    let op = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `device` is a block device handle with `WRITE`; `&op` is a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, device, (&op as *const IoOp) as u64) };
    if po < 0 {
        return Err("the disk refused to flush its cache");
    }
    if po_wait(po as u64).0 != 0 {
        return Err("the disk could not flush its cache");
    }
    Ok(())
}

fn wall_clock_seconds() -> i64 {
    let mut nanos: u64 = 0;
    // SAFETY: a valid writable `u64` out-param.
    let r = unsafe { syscall2(SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    if r < 0 { 0 } else { (nanos / 1_000_000_000) as i64 }
}

struct Account {
    name: Vec<u8>,
    /// Its password, scrubbed by the caller once the install is over.
    password: Vec<u8>,
}

/// Ask for the first account on `term`: a name, echo on, then a password twice, echo off.
/// `Declined` if a question was cancelled, `NotInstalled` if an answer breaks `libusers`' rules or
/// the two passwords differ.
///
/// Each question logs its receipt, since nothing reads a desktop terminal's grid: the name's
/// before it is asked, and each password's once echo is off, so a gate's typing is not echoed.
fn ask_account(term: u64, stderr: Option<u64>) -> Result<Account, nxinstall::Outcome> {
    use nxinstall::Outcome;
    let say = |level: Level, line: &str| say_to(stderr, level, line);
    log("asking for the first account");
    let Some(name) = libprompt::ask_line(term, b"the new machine's first account: name: ") else {
        say(Level::Notice, "cancelled; nothing was written.");
        return Err(Outcome::Declined);
    };
    if !libusers::valid_name(&name) {
        say(
            Level::Error,
            "an account's name is 1 to 32 of a-z, 0-9, `_` and `-`, starting with a letter or `_`. \
             Nothing was written.",
        );
        return Err(Outcome::NotInstalled);
    }
    // Two receipts, neither a prefix of the other, since a gate matches on a line's start.
    let asked = &mut |n: u8| log(if n == 1 { "asking for its password" } else { "asking for the password again" });
    match libprompt::ask_new_password_then(term, b"password: ", asked) {
        Ok(password) => Ok(Account { name, password }),
        Err(why) => {
            // A cancelled question is the person's no; mismatched or refused passwords are an
            // answer that could not be used.
            let cancelled = why == libprompt::NewPassword::Cancelled;
            say(if cancelled { Level::Notice } else { Level::Error }, &format!("{}; nothing was written.", why.why()));
            Err(if cancelled { Outcome::Declined } else { Outcome::NotInstalled })
        }
    }
}

/// The disks this view does not hold, and why, from `/dev/devices`' and `/dev/storage`' tables.
/// Empty when either will not read: there is then nothing true to say.
fn withheld_disks(ns: u64, reachable: &[String]) -> Vec<nxinstall::withheld::Withheld> {
    use nxinstall::withheld::{Device as Row, Mount, withheld};
    let read = |path: &[u8]| libfs::read_file(ns, path).ok().and_then(|b| libstream::wire::Table::decode(&b).ok());
    let (Some(devices), Some(storage)) = (read(b"/dev/devices/all.tsm"), read(b"/dev/storage/all.tsm")) else {
        return Vec::new();
    };
    let cell = |t: &libstream::wire::Table, row: usize, name: &str| -> String {
        let Some(i) = t.schema.fields.iter().position(|f| f.name == name) else { return String::new() };
        match t.rows.get(row).and_then(|r| r.get(i)) {
            Some(Value::Str(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let rows: Vec<Row> = (0..devices.rows.len())
        .map(|r| Row {
            name: cell(&devices, r, "name"),
            kind: cell(&devices, r, "kind"),
            path: cell(&devices, r, "path"),
            description: cell(&devices, r, "description"),
            parent: cell(&devices, r, "parent"),
            boot: devices
                .schema
                .fields
                .iter()
                .position(|f| f.name == "boot")
                .and_then(|i| devices.rows.get(r).and_then(|row| row.get(i)))
                .is_some_and(|v| *v == Value::Bool(true)),
        })
        .collect();
    let mounts: Vec<Mount> = (0..storage.rows.len())
        .map(|r| Mount {
            name: cell(&storage, r, "name"),
            at: cell(&storage, r, "mounted"),
            by: cell(&storage, r, "by"),
        })
        .collect();
    let reachable: Vec<&str> = reachable.iter().map(|p| p.as_str()).collect();
    withheld(&rows, &mounts, &reachable)
}

/// The device list, as a TSM1 table on `stdout`; everything else goes to `stderr`.
fn emit_devices(stdout: u64, devs: &[Device]) {
    let schema = Schema::new()
        .field("path", TypeTag::String, TypeModifiers::NONE)
        .field("kind", TypeTag::String, TypeModifiers::NONE)
        .field("size", TypeTag::String, TypeModifiers::NONE)
        .field("name", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(ChannelSink::new(IpcPort::new(stdout), IPC_PAYLOAD_SIZE));
    let wrote = tw.write_schema(StreamFlags::NONE, &schema).and_then(|()| {
        for d in devs {
            tw.write_row(&[
                Value::Str(d.path()),
                Value::Str(String::from(kind_name(d.info.kind()))),
                Value::Str(human(d.info.byte_capacity())),
                Value::Str(d.name()),
            ])?;
        }
        tw.finish_with_status(0)
    });
    let _ = wrote.and_then(|()| tw.into_sink().finish());
}

fn kind_name(k: BlockKind) -> &'static str {
    match k {
        BlockKind::Unknown => "unknown",
        BlockKind::Disk => "disk",
        BlockKind::Partition => "partition",
        BlockKind::RamDisk => "ram disk",
    }
}

/// Record one milestone of a destructive operation in the system log, which outlives the
/// terminal's scrollback and is what a gate reads. Never what was typed: a path or identity logged
/// here is read back out of the tables or the device's own `info`, and an operand that names no
/// device is logged as that, without itself.
fn log(line: &str) {
    let mut bytes = Vec::from(&b"nxinstall: "[..]);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    kprint(&bytes);
}

/// Write one line to `stderr` at `level`, or to the kernel log when there is none. Each line is
/// its own message, since `stderr` is shared between pipeline stages; only an error-level line is
/// drawn as a refusal.
fn say_to(stderr: Option<u64>, level: Level, line: &str) {
    let mut bytes = Vec::from(line.as_bytes());
    bytes.push(b'\n');
    match stderr {
        Some(h) => {
            let mut port = IpcPort::new(h);
            if port.send(&libstream::diag::frame(level, &bytes), false).is_err() {
                kprint(&bytes);
            }
        }
        None => kprint(&bytes),
    }
}

/// `--help`'s lines on `stdout`; `false` with no `stdout`, or when the write failed.
fn help_out(stdout: Option<u64>) -> bool {
    stdout.is_some_and(|h| {
        let mut sink = ChannelSink::new(IpcPort::new(h), IPC_PAYLOAD_SIZE);
        libstream::table::write_text_fallback(&mut sink, HELP, 0).and_then(|()| sink.finish()).is_ok()
    })
}

fn run(
    namespace: u64,
    argv: &[String],
    stdout: Option<u64>,
    stderr: Option<u64>,
    terminal: Option<u64>,
) -> nxinstall::Outcome {
    use nxinstall::Outcome;
    // An error, a warning or a notice: what stopped it, what it left out, and progress.
    let say = |line: &str| say_to(stderr, Level::Error, line);
    let warn = |line: &str| say_to(stderr, Level::Warning, line);
    let mut note = |line: &str| say_to(stderr, Level::Notice, line);

    let operands: Vec<&String> = argv.iter().skip(1).collect();
    // Before anything is looked up, so `nxinstall --help` answers without a view.
    if operands.iter().any(|o| o.as_str() == "--help") {
        if !help_out(stdout) {
            for line in HELP {
                note(line);
            }
        }
        return Outcome::Help;
    }
    if let Some(flag) = operands.iter().find(|o| o.starts_with('-')) {
        say(&format!("nxinstall: no option {flag}; `nxinstall --help` says what it takes"));
        return Outcome::Usage;
    }
    if operands.len() > 1 {
        say("usage: with admin nxinstall [DEVICE] -- `nxinstall --help` says more");
        return Outcome::Usage;
    }

    let devs = devices(namespace);
    // Messages on `stderr`: the table on `stdout` stays what the view can use, which gates read
    // to prove `disks` withholds anything.
    let reachable: Vec<String> = devs.iter().map(|d| d.path()).collect();
    let withheld = withheld_disks(namespace, &reachable);

    if operands.is_empty() {
        if devs.is_empty() {
            say("no block devices in this view. Installing needs the disks granted: run it as \
                 `with admin nxinstall`, from the live image's \"install to this machine\" entry.");
            for w in &withheld {
                warn(&w.message());
            }
            return Outcome::NotInstalled;
        }
        match stdout {
            Some(h) => emit_devices(h, &devs),
            None => {
                for d in &devs {
                    note(&format!(
                        "{} {} {} {}",
                        d.path(),
                        kind_name(d.info.kind()),
                        human(d.info.byte_capacity()),
                        d.name()
                    ));
                }
            }
        }
        for w in &withheld {
            warn(&w.message());
        }
        return Outcome::Listed;
    }

    // A refusal goes in the system log, the only thing a gate can see of a conversation on a
    // terminal. The path logged is one the tables name, never taken from what was typed.
    let refuse = |path: &str, what: &str| log(&format!("refused {path}: {what}"));

    let wanted = operands[0].as_str();
    let Some(target) = devs.iter().find(|d| d.path() == wanted) else {
        // In use, said as such rather than "not here": the person can act on why.
        if let Some(w) = withheld.iter().find(|w| w.path == wanted) {
            say(&w.message());
            refuse(&w.path, w.refusal());
            return Outcome::NotInstalled;
        }
        say(&format!("{wanted} is not a block device this view can reach."));
        log("refused an operand that names no block device in this view");
        return Outcome::NotInstalled;
    };

    // Refusals are per-kind because each catches a different mistake: a partition is picked off
    // a listing by mistake, and a RAM disk is the running system itself.
    match target.info.kind() {
        BlockKind::Disk => {}
        BlockKind::Partition => {
            say(&format!(
                "{wanted} is a partition, not a disk. An install writes a partition table, which \
                 would destroy the disk this partition is part of."
            ));
            refuse(&target.path(), "it is a partition, not a disk");
            return Outcome::NotInstalled;
        }
        BlockKind::RamDisk => {
            say(&format!(
                "{wanted} is memory published as a disk — one of the modules this live system is \
                 running from. Writing it would destroy the running system and survive nothing."
            ));
            refuse(&target.path(), "it is a ram disk, not a disk");
            return Outcome::NotInstalled;
        }
        BlockKind::Unknown => {
            say(&format!("{wanted} does not say what it is, so this installer will not write to it."));
            refuse(&target.path(), "it does not say what it is");
            return Outcome::NotInstalled;
        }
    }
    if target.info.name().is_empty() {
        say(&format!(
            "{wanted} reports no model or serial, so the plan could not say which disk it is. This \
             installer will not write to a disk it cannot name."
        ));
        refuse(&target.path(), "it reports no model or serial");
        return Outcome::NotInstalled;
    }

    let Some((mem, addr)) = scratch(CHUNK) else {
        say("could not allocate a transfer buffer.");
        return Outcome::NotInstalled;
    };
    let io = Io { mem, addr };

    let srcs = match sources(&devs, &io) {
        Ok(s) => s,
        Err(e) => {
            say(&e);
            return Outcome::NotInstalled;
        }
    };
    let layout = match layout_for(target, &srcs) {
        Ok(l) => l,
        Err(e) => {
            say(&e);
            refuse(&target.path(), "the disk cannot take the install");
            return Outcome::NotInstalled;
        }
    };

    // Asked on the terminal: only `yes` goes ahead, then the account. Nothing has been written,
    // and nothing is until all of it is answered.
    let Some(term) = terminal else {
        say("this installer asks before it writes, on a terminal, and was given none. Nothing was written.");
        refuse(&target.path(), "no terminal to ask on");
        return Outcome::NotInstalled;
    };
    // The plan and the question in one write: sent to `stderr`, the plan could reach the screen
    // after the question, with nothing ordering the two channels.
    let identity = target.name();
    let block = target.info.logical_block_size as u64;
    let question = format!(
        "{} is {} ({})\r\n  a boot partition of {} and a root partition of {}\r\n  everything \
         already on that disk is lost\r\nerase {} and install Nitrox? type yes to go ahead: ",
        target.path(),
        identity,
        human(target.info.byte_capacity()),
        human(layout.esp_blocks() * block),
        human(layout.root_blocks() * block),
        target.path()
    );
    // The receipt a gate waits for before it types the answer: nothing reads a desktop terminal.
    log("asking to go ahead");
    let answer = libprompt::ask_line(term, question.as_bytes());
    if !answer.as_deref().is_some_and(nxinstall::confirmed) {
        note("nothing was written.");
        log(&format!("not going ahead; nothing was written to {}", target.path()));
        return Outcome::Declined;
    }
    let mut account = match ask_account(term, stderr) {
        Ok(account) => account,
        Err(outcome) => {
            if outcome == Outcome::Declined {
                log(&format!("the account was not given; nothing was written to {}", target.path()));
            } else {
                refuse(&target.path(), "the first account was not one the rules allow");
            }
            return outcome;
        }
    };

    note(&format!("installing to {} ({})", target.path(), identity));
    let installed = install(&io, target, &srcs, &layout, &account, &mut note);
    libkern::scrub(&mut account.password);
    match installed {
        Ok(()) => {
            note("done. Remove the installation medium and restart.");
            Outcome::Installed
        }
        Err(e) => {
            say(&e);
            Outcome::NotInstalled
        }
    }
}

/// Entry point: the shell's setup message carries `argv` and the streams.
///
/// # Safety
///
/// Called by the kernel ELF loader with the four bootstrap registers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn _start(notif: u64, namespace: u64, endpoint: u64, arg0: u64) -> ! {
    let boot = libstream::setup::bootstrap(notif, namespace, endpoint, arg0);
    let (argv, stdout, stderr, terminal) = match boot.setup() {
        Some(Ok(s)) => (s.argv, s.streams.stdout, s.streams.stderr, s.terminal),
        Some(Err(_)) => {
            kprint(b"nxinstall: malformed setup message\n");
            exit(EXIT_FAILURE);
        }
        None => (Vec::new(), None, None, None),
    };
    // SAFETY: single-threaded, before anything can panic.
    unsafe { PANIC_SINK = stderr.unwrap_or(0) };
    exit(run(namespace, &argv, stdout, stderr, terminal).status());
}

/// The `stderr` sink for the panic handler: `kprint` reaches COM1 only, which the target lacks.
static mut PANIC_SINK: u64 = 0;

static mut PANIC_MSG: [u8; 4096] = [0; 4096];

/// Nothing here is recoverable; say where it happened, on the terminal, and stop.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // SAFETY: single-threaded, and nothing runs after this.
    unsafe {
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
        let buf = &mut *(&raw mut PANIC_MSG);
        let mut n = 0usize;
        let mut put = |bytes: &[u8], n: &mut usize| {
            let take = bytes.len().min(buf.len().saturating_sub(*n + 1));
            buf[*n..*n + take].copy_from_slice(&bytes[..take]);
            *n += take;
        };
        put(b"nxinstall: stopped by an internal error", &mut n);
        if let Some(loc) = info.location() {
            put(b" at ", &mut n);
            put(loc.file().as_bytes(), &mut n);
            put(b":", &mut n);
            let mut line = loc.line();
            let mut digits = [0u8; 10];
            let mut d = digits.len();
            loop {
                d -= 1;
                digits[d] = b'0' + (line % 10) as u8;
                line /= 10;
                if line == 0 || d == 0 {
                    break;
                }
            }
            put(&digits[d..], &mut n);
        }
        put(b". Nothing further was written.\n", &mut n);
        kprint(&buf[..n]);
        let sink = (&raw const PANIC_SINK).read();
        if sink != 0 {
            raw_send(sink, &buf[..n]);
        }
    }
    exit(EXIT_FAILURE);
}

/// Send one message on a channel without allocating, for the panic handler.
fn raw_send(channel: u64, payload: &[u8]) {
    const OFF_PAYLOAD_LEN: usize = 4;
    const OFF_PAYLOAD: usize = 24;
    // SAFETY: single-threaded; this static is used by nothing else.
    unsafe {
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
        let msg = &mut *(&raw mut RAW_MSG);
        let n = payload.len().min(msg.len() - OFF_PAYLOAD);
        msg[..OFF_PAYLOAD].fill(0);
        msg[OFF_PAYLOAD_LEN..OFF_PAYLOAD_LEN + 4].copy_from_slice(&(n as u32).to_le_bytes());
        msg[OFF_PAYLOAD..OFF_PAYLOAD + n].copy_from_slice(&payload[..n]);
        let no_handles = [0u64; 1];
        // Non-blocking: a full channel is not worth hanging a dying process over.
        syscall6(
            SYS_CHANNEL_SEND,
            channel,
            msg.as_ptr() as u64,
            no_handles.as_ptr() as u64,
            0,
            libkern::abi::SENDMODE_NOBLOCK,
            0,
        );
    }
}

static mut RAW_MSG: [u8; 4096] = [0; 4096];
