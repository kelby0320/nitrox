//! `nxinstall` — write this running system onto a disk.
//!
//! ```text
//! nxinstall                         # what block devices this session can see
//! nxinstall /dev/blk/2              # what it would do, and the words to confirm it with
//! nxinstall /dev/blk/2 "QEMU HARDDISK (QM00002)"   # do it
//! ```
//!
//! ## It holds no authority of its own
//!
//! There is no privileged installer here, because there is nothing to be privileged *as*: this
//! system has no root account and no way to become one. `nxinstall` reaches a disk only when the
//! session it runs in has one bound — which today means the live image's third boot entry, and
//! later means whatever [`administration.md`](../../docs/planning/administration.md)'s elevation
//! broker hands over. Run from an ordinary session it resolves nothing and says so. **Absence is
//! the sandbox**: no check in this program is what stops it, and removing one would not help it.
//!
//! ## The confirmation is an argument, not a prompt
//!
//! A destructive operation should be hard to do by accident, and "type the disk's identity back"
//! is the form that resists a reflex — unlike `[y/N]`, which a person answers before reading.
//! Here it is the second operand rather than an interactive read, for a reason that is this
//! system's rather than a preference: **no program in Nitrox reads a terminal**. A stage's
//! `stdin` is a typed stream from the stage before it (shell design §10a), and `/dev/tty` in an
//! application namespace *mints a fresh terminal* rather than naming the one the program is
//! running in — so there is no prompt to write to and no keystroke to read back. A first run
//! prints the plan and the exact line that would carry it out; running that line is the
//! confirmation.
//!
//! It also means the dangerous form cannot be reached by holding Return: the identity has to
//! come from somewhere, and the only place it exists is the report the first run printed.
//!
//! ## What it writes
//!
//! A GPT with two partitions (`nxinstall::plan`), then two raw copies: the installable ESP
//! module onto the boot partition, and the live root's filesystem onto the root partition. Both
//! sources are RAM disks the bootloader loaded — this program never formats anything, which is
//! what keeps FAT32 out of the tree and, for now, ext4 writing out of the install path.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libgpt::table::{self, BACK_BYTES, BLOCK, FRONT_BYTES};
use libkern::abi::{
    BlockDeviceInfo, BlockKind, IO_OPCODE_READ, IO_OPCODE_WRITE, IPC_PAYLOAD_SIZE, IoOp,
};
use libkern::handle::{RIGHT_MAP_READ, RIGHT_MAP_WRITE, RIGHT_READ, RIGHT_WRITE};
use libkern::syscall::{
    SYS_ENTROPY_CREATE, SYS_ENTROPY_READ, SYS_HANDLE_CLOSE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE,
    SYS_MEMORY_MAP, SYS_NS_LOOKUP, SYS_WAIT, syscall1, syscall2, syscall4,
};
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort, MsgPort};
use libstream::table::TableWriter;
use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag, Value};

/// `alloc` backing: the report lines and the TSM1 encoder both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// What a failure before the program is running exits with — a malformed setup message, or a
/// panic. Every other status comes from [`nxinstall::Outcome`], which is where the policy is
/// stated and tested.
const EXIT_FAILURE: i64 = 1;

/// How far `/dev/blk/<n>` is scanned. The registry is dense, so the first miss ends the scan;
/// this only bounds a kernel that grew a hole.
const MAX_BLOCK_DEVICES: usize = 16;

/// The transfer buffer, in bytes: 256 KiB, which is 64 pages and so 64 scatter-gather fragments
/// — comfortably inside what one AHCI command can describe (`MAX_PRDT_ENTRIES` is 248). Copying
/// 33 MiB one 4 KiB page at a time would be 8,500 round trips through the device and the
/// scheduler; at this size it is 132.
const CHUNK: u64 = 256 * 1024;

/// The GPT name the live image's root partition carries, which is how the root *source* is
/// recognised among the RAM disks. It is deliberately not `nitrox-root`: the thing being copied
/// is the live root, and what it becomes is named by the table this program writes.
const LIVE_LABEL: &[u8] = b"nitrox-live";

/// `sys_wait` scratch for a single pending operation.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// `sys_wait` results: one 32-byte record.
static mut WAIT_RESULTS: [u8; 32] = [0; 32];

/// The front of a GPT: protective MBR, header, entry array.
static mut FRONT: [u8; FRONT_BYTES] = [0; FRONT_BYTES];
/// The back of a GPT: the entry array again, then the backup header.
static mut BACK: [u8; BACK_BYTES] = [0; BACK_BYTES];

// ---------------------------------------------------------------------------------------------
// Syscall plumbing
// ---------------------------------------------------------------------------------------------

/// Wait for one pending operation and return its `(status, value)`.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: valid single-waiter buffers; the PO handle is ours.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
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

/// Resolve `path` in `ns` asking for `rights`, or `None`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid namespace handle and path slice.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return None;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 || handle == 0 { None } else { Some(handle) }
}

/// A `MemoryObject` of `size` bytes, mapped read-write; `(handle, address)`.
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
/// **Refuses rather than inventing one.** A table whose GUIDs are constant is a table that
/// collides with every other machine installed by this program, and the failure it produces
/// arrives much later, somewhere else.
fn random_guid() -> Option<[u8; 16]> {
    // SAFETY: register-only syscall.
    let h = unsafe { syscall1(SYS_ENTROPY_CREATE, 0) };
    if h < 0 {
        return None;
    }
    let h = h as u64;
    let mut buf = [0u8; 16];
    // SAFETY: a valid writable 16-byte buffer.
    let r = unsafe { syscall4(SYS_ENTROPY_READ, h, (&raw mut buf) as u64, 16, 0) };
    // A positive return is a PO: the pool is not seeded yet, so wait for the fill.
    let ok = if r == 0 {
        true
    } else if r > 0 {
        po_wait(r as u64).0 == 0
    } else {
        false
    };
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    ok.then_some(buf)
}

// ---------------------------------------------------------------------------------------------
// Block devices
// ---------------------------------------------------------------------------------------------

/// One block device this session can reach.
struct Device {
    /// Its index under `/dev/blk`.
    index: usize,
    /// The device handle, with whatever of read/write the binding allowed.
    handle: u64,
    /// What it says it is.
    info: BlockDeviceInfo,
}

impl Device {
    /// Its path, for a report and for the command line that confirms it.
    fn path(&self) -> String {
        format!("/dev/blk/{}", self.index)
    }

    /// Its name, as text.
    fn name(&self) -> String {
        String::from_utf8_lossy(self.info.name()).into_owned()
    }

    /// Read `buf.len()` bytes from byte offset `at` through `io`.
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

    /// Write `buf` at byte offset `at` through `io`.
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
    /// The object handle, named by each `IoOp`.
    mem: u64,
    /// Its mapping in this process.
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

/// Every block device in `ns`, in index order.
///
/// The registry is dense, so the first index that does not resolve ends the scan — and an empty
/// result is the ordinary answer in an ordinary session, not an error.
fn devices(ns: u64) -> Vec<Device> {
    let mut out = Vec::new();
    for index in 0..MAX_BLOCK_DEVICES {
        let path = format!("/dev/blk/{index}");
        let Some(handle) = lookup(ns, path.as_bytes(), RIGHT_READ | RIGHT_WRITE) else {
            break;
        };
        // The info leaf is mapped, not read: it is a `MemoryObject` snapshot, the same shape
        // `/dev/framebuffer/info` uses.
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
                let bytes = unsafe {
                    core::slice::from_raw_parts(addr as u64 as *const u8, size as usize)
                };
                BlockDeviceInfo::read(bytes)
            })
            // A device whose info would not map is still a device, and it reads as `Unknown` —
            // which every destructive path here refuses.
            .unwrap_or_default();
        out.push(Device { index, handle, info });
    }
    out
}

/// Bytes rendered for a person: `33 MiB`, `931 GiB`.
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

// ---------------------------------------------------------------------------------------------
// The install
// ---------------------------------------------------------------------------------------------

/// The two RAM disks this program copies from.
struct Sources<'a> {
    /// The installable ESP, copied onto the boot partition whole.
    esp: &'a Device,
    /// The live root image, whose *inner* partition is copied onto the root partition.
    root: &'a Device,
    /// Where the filesystem sits inside `root`: `(first block, block count)`.
    root_extent: (u64, u64),
}

/// Find the two sources among the RAM disks.
///
/// **By what they contain, not what they are called.** A module's name is `module 2
/// (/boot/install-esp.img)` — a path in a build script, which is the wrong thing for an
/// installer to depend on. A FAT volume says so in its boot sector, and the root image carries a
/// partition table naming [`LIVE_LABEL`]; both are properties of the bytes being copied.
fn sources<'a>(devs: &'a [Device], io: &Io) -> Result<Sources<'a>, String> {
    let mut esp = None;
    let mut root = None;
    let mut root_extent = (0, 0);
    let mut front = [0u8; FRONT_BYTES];
    for d in devs.iter().filter(|d| d.info.kind() == BlockKind::RamDisk) {
        if d.read_at(io, 0, &mut front).is_err() {
            continue;
        }
        // A FAT boot sector: the signature at the end of block 0, and one of the two places the
        // family name is written (FAT32 at 82, FAT12/16 at 54).
        let fat = front[510] == 0x55
            && front[511] == 0xAA
            && (&front[82..87] == b"FAT32" || &front[54..57] == b"FAT");
        if fat && esp.is_none() {
            esp = Some(d);
            continue;
        }
        if let Ok(t) = table::read(&front) {
            if let Some(p) = t.by_name(LIVE_LABEL) {
                root = Some(d);
                root_extent = (p.first_lba, p.blocks());
            }
        }
    }
    match (esp, root) {
        (Some(esp), Some(root)) => Ok(Sources { esp, root, root_extent }),
        (None, _) => Err(String::from(
            "no installable ESP among this session's devices. That module is carried by the \
             live image's install entry alone — an ordinary live boot does not load it.",
        )),
        (_, None) => Err(format!(
            "no root image among this session's devices: none of them holds a partition named {}.",
            String::from_utf8_lossy(LIVE_LABEL)
        )),
    }
}

/// Copy `blocks` blocks from `src` at `src_at` to `dst` at `dst_at`, reporting progress.
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
        // Straight through the one buffer: read fills it, write drains it. Nothing is staged in
        // this process's heap, so the copy costs `CHUNK` of memory whatever the disk's size.
        io.submit(IO_OPCODE_READ, src.handle, src_at + done, n)
            .map_err(|e| format!("reading {what}: {e}"))?;
        io.submit(IO_OPCODE_WRITE, dst.handle, dst_at + done, n)
            .map_err(|e| format!("writing {what}: {e}"))?;
        done += n;
        if done - reported >= 8 * 1024 * 1024 || done == bytes {
            reported = done;
            say(&format!("  {what}: {} of {}", human(done), human(bytes)));
        }
    }
    Ok(())
}

/// Where the two partitions would go on `target`, or why they cannot.
///
/// **Worked out before the confirmation, not after it.** A disk that cannot take the install is
/// refused while the person is still reading the plan, rather than after they have typed its
/// name back — and the plan can then say how big the root partition would actually be, which is
/// the number they are agreeing to lose (first install attempt, 2026-09-17).
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

/// Everything after the confirmation matched.
fn install(
    io: &Io,
    target: &Device,
    srcs: &Sources,
    layout: &nxinstall::Layout,
    say: &mut dyn FnMut(&str),
) -> Result<(), String> {
    let block = target.info.logical_block_size as u64;
    let esp_bytes = srcs.esp.info.byte_capacity();
    let root_bytes = srcs.root_extent.1 * BLOCK as u64;

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

    // **The table first, and the filesystems after.** A disk whose table is written but whose
    // partitions are empty is one this program can be run against again; the other order leaves
    // bytes nobody can find.
    say("writing the partition table");
    log(&format!(
        "installing to {} ({}), boot {} + root {}",
        target.path(),
        target.name(),
        human(esp_bytes),
        human(layout.root_blocks() * block)
    ));
    // SAFETY: single-threaded program; these statics have no other reader.
    let (front, back) = unsafe { (&mut *(&raw mut FRONT), &mut *(&raw mut BACK)) };
    table::build(
        target.info.block_count,
        disk_guid,
        &layout.partitions(esp_guid, root_guid),
        front,
        back,
    )
    .map_err(|e| format!("the partition table would not build: {e:?}. Nothing was written."))?;
    target
        .write_at(io, 0, front)
        .map_err(|e| format!("writing the partition table: {e}"))?;
    let back_at = (target.info.block_count - table::ARRAY_BLOCKS - 1) * BLOCK as u64;
    target
        .write_at(io, back_at, back)
        .map_err(|e| format!("writing the backup partition table: {e}"))?;

    say(&format!("copying the boot partition ({})", human(esp_bytes)));
    copy(
        io,
        srcs.esp,
        0,
        target,
        layout.esp_first * BLOCK as u64,
        esp_bytes,
        "boot",
        say,
    )?;

    say(&format!("copying the root filesystem ({})", human(root_bytes)));
    copy(
        io,
        srcs.root,
        srcs.root_extent.0 * BLOCK as u64,
        target,
        layout.root_first * BLOCK as u64,
        root_bytes,
        "root",
        say,
    )?;
    log("wrote the partition table, the boot partition and the root filesystem");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------------------------

/// The device list, as a TSM1 table on `stdout` — a table because it is *data*: a shell can sort
/// it, filter it, or count it. Everything else this program says is a message to a person and
/// goes to `stderr`.
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

/// A word for a kind, matching the kernel's own.
fn kind_name(k: BlockKind) -> &'static str {
    match k {
        BlockKind::Unknown => "unknown",
        BlockKind::Disk => "disk",
        BlockKind::Partition => "partition",
        BlockKind::RamDisk => "ram disk",
    }
}

/// Record one milestone of a destructive operation in the **system log**.
///
/// Separate from [`say_to`], which talks to the person running the program. This is the record
/// an install leaves behind: on the machine Phase 5 targets there is no serial port and the
/// terminal's scrollback goes away with the session, so "what did the installer actually do"
/// has to survive somewhere. Only the milestones — a refusal is a conversation, not an event.
///
/// **Never the operand.** The identity logged here is read back out of the device's own `info`,
/// not taken from the command line: what a person typed does not belong on a console, and the
/// two strings are equal exactly when the install proceeds anyway.
fn log(line: &str) {
    let mut bytes = Vec::from(&b"nxinstall: "[..]);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    kprint(&bytes);
}

/// Write one line to `stderr`, or to the kernel log when there is none.
///
/// Each line is its own message: `stderr` is shared between the stages of a pipeline, so a
/// partial line left in it would interleave with another stage's.
fn say_to(stderr: Option<u64>, line: &str) {
    let mut bytes = Vec::from(line.as_bytes());
    bytes.push(b'\n');
    match stderr {
        Some(h) => {
            let mut port = IpcPort::new(h);
            if port.send(&bytes, false).is_err() {
                kprint(&bytes);
            }
        }
        None => kprint(&bytes),
    }
}

// ---------------------------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------------------------

/// Run, and return the exit status.
fn run(
    namespace: u64,
    argv: &[String],
    stdout: Option<u64>,
    stderr: Option<u64>,
) -> nxinstall::Outcome {
    use nxinstall::Outcome;
    let mut say = |line: &str| say_to(stderr, line);

    let devs = devices(namespace);
    let operands: Vec<&String> = argv.iter().skip(1).collect();

    if operands.is_empty() {
        if devs.is_empty() {
            say(
                "no block devices in this session. Installing needs a session that was given \
                 one — the live image's \"install to this machine\" entry.",
            );
            return Outcome::NotInstalled;
        }
        match stdout {
            Some(h) => emit_devices(h, &devs),
            None => {
                for d in &devs {
                    say(&format!(
                        "{} {} {} {}",
                        d.path(),
                        kind_name(d.info.kind()),
                        human(d.info.byte_capacity()),
                        d.name()
                    ));
                }
            }
        }
        return Outcome::Listed;
    }
    if operands.len() > 2 {
        say("usage: nxinstall [DEVICE [IDENTITY]]");
        return Outcome::Usage;
    }

    // The target, by the path the listing printed.
    let wanted = operands[0].as_str();
    let Some(target) = devs.iter().find(|d| d.path() == wanted) else {
        say(&format!("{wanted} is not a block device this session can reach."));
        return Outcome::NotInstalled;
    };

    // **What it is, before what it is called.** The refusals are per-kind because the mistake
    // each one catches is a different mistake: a partition is what a person picks off a listing
    // by mistake, and a RAM disk is the running system itself.
    match target.info.kind() {
        BlockKind::Disk => {}
        BlockKind::Partition => {
            say(&format!(
                "{wanted} is a partition, not a disk. An install writes a partition table, which \
                 would destroy the disk this partition is part of."
            ));
            return Outcome::NotInstalled;
        }
        BlockKind::RamDisk => {
            say(&format!(
                "{wanted} is memory published as a disk — one of the modules this live system is \
                 running from. Writing it would destroy the running system and survive nothing."
            ));
            return Outcome::NotInstalled;
        }
        BlockKind::Unknown => {
            say(&format!(
                "{wanted} does not say what it is, so this installer will not write to it."
            ));
            return Outcome::NotInstalled;
        }
    }
    if target.info.name().is_empty() {
        say(&format!(
            "{wanted} reports no model or serial, so there is nothing to confirm it by. This \
             installer will not write to a disk it cannot name."
        ));
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
            return Outcome::NotInstalled;
        }
    };

    let identity = target.name();
    let block = target.info.logical_block_size as u64;
    if operands.len() == 1 {
        say(&format!(
            "{} is {} ({})",
            target.path(),
            identity,
            human(target.info.byte_capacity())
        ));
        say(&format!(
            "  a boot partition of {} and a root partition of {}",
            human(layout.esp_blocks() * block),
            human(layout.root_blocks() * block)
        ));
        say("  everything already on that disk is lost");
        say("");
        say("nothing has been written. To go ahead, name the disk back:");
        say(&format!("  nxinstall {} \"{}\"", target.path(), identity));
        // **A plan is an answer, not a failure** — see `Outcome`. This is the ordinary first
        // step through the program, and exiting non-zero put `nxsh: pipeline failed` directly
        // under the line telling a person what to type next.
        return Outcome::Planned;
    }

    if operands[1].as_str() != identity.as_str() {
        say(&format!(
            "that is not what {} is called, so nothing was written. It is \"{}\".",
            target.path(),
            identity
        ));
        return Outcome::NotInstalled;
    }

    say(&format!("installing to {} ({})", target.path(), identity));
    match install(&io, target, &srcs, &layout, &mut say) {
        Ok(()) => {
            say("done. Remove the installation medium and restart.");
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
    let (argv, stdout, stderr) = match boot.setup() {
        Some(Ok(s)) => (s.argv, s.streams.stdout, s.streams.stderr),
        Some(Err(_)) => {
            kprint(b"nxinstall: malformed setup message\n");
            exit(EXIT_FAILURE);
        }
        // Spawned without a shell: no `argv`, so the only thing it can do is list.
        None => (Vec::new(), None, None),
    };
    exit(run(namespace, &argv, stdout, stderr).status());
}

/// Nothing here is recoverable; say where it happened and stop.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxinstall: panic\n");
    let _ = info;
    exit(EXIT_FAILURE);
}
