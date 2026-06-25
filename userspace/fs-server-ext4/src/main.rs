//! `fs-server-ext4` — the server `[[bin]]` (slice 7 Part 4).
//!
//! The first **userspace resource server**: a process that reads a read-only ext4
//! filesystem over a block device and serves it through the resource-server
//! protocol, reached transparently via the namespace (the kernel forwards
//! `sys_ns_lookup` to it — slice 7 Part 3).
//!
//! ## Bootstrap (driven by init in Part 6)
//!
//! 1. init spawns this process, installing **one** handle — a **control channel**
//!    endpoint — which the kernel delivers in `rdx` (`_start`'s third argument).
//! 2. init sends a **setup message** on that channel transferring the **read-only
//!    block-device** handle; the server receives it ([`recv_device`]).
//! 3. The server creates a **forwarding channel** pair, keeps the serving end, and
//!    sends `Meta::Ready` on the control channel **transferring the other (kernel)
//!    end** ([`send_ready`]); init binds that endpoint as a Userspace Server.
//! 4. The server loops: recv a forwarded `Namespace::Resolve`, read the file, and
//!    reply transferring a read-only `MemoryObject` of its content ([`serve_loop`]).
//!
//! The request→reply logic lives in the host-tested [`fs_server_ext4::serve`]
//! module; this file is only the syscall plumbing + the `BlockReader` over
//! `sys_io_submit`. **Alloc-free** — fixed `.bss` buffers, no global allocator.
//!
//! It never holds `BIND_NAMESPACE` (init binds its endpoint) and receives only the
//! handles it needs at spawn — see `CLAUDE.md` and
//! `docs/rationale/why-supervisor-registration.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use fs_server_ext4::serve::{Served, encode_error, serve_resolve};
use fs_server_ext4::{BlockReader, FsError, ext4};
use libkern::*;

/// One page; the scratch buffer + memory-object granularity.
const PAGE: u64 = 4096;
/// Block-device sector size (the `sys_io_submit` transfer unit).
const SECTOR: usize = 512;
/// IPC message size (the `RECV_MSG`/`REPLY_MSG` buffers); payload starts at 24.
const MSG_LEN: usize = 4096;
const PAYLOAD_OFF: usize = 24;

// --- fixed server buffers (.bss; the server is single-threaded) -------------
/// Inbox for a received message (setup, then each forwarded `Resolve`).
static mut RECV_MSG: [u8; 4096] = [0; 4096];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
/// Outbox for a reply (and the bootstrap Ready); the transferred handle in `[0]`.
static mut REPLY_MSG: [u8; 4096] = [0; 4096];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
/// Scratch for the file content (the 64 KiB read-model cap).
static mut CONTENT: [u8; ext4::MAX_FILE] = [0; ext4::MAX_FILE];
/// `sys_wait` scratch (one handle at a time).
static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Log `msg` and exit non-zero — the bootstrap failure path. (The server is not a
/// critical-path process like init/eshell, so exiting on a bootstrap fault is the
/// correct disposition; a supervisor observes the exit.)
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// Wait on a single handle then read a completed `PendingOperation`'s
/// `(status, result)` (`IoResult`: status @8, result @16) and close it.
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
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
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let result = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// A [`BlockReader`] over the block device: each `read_at` reads the covering
/// 512-byte sectors via `sys_io_submit` into a one-page scratch `MemoryObject`
/// (mapped R/W) and copies the requested sub-range out. Sector-at-a-time — simple
/// and correct; the parser's reads are small (≤ one 4 KiB filesystem block).
struct DiskReader {
    /// The read-only block-device handle (from the setup message).
    device: u64,
    /// A one-page scratch `MemoryObject` the device DMAs sectors into.
    scratch: u64,
    /// `scratch`, mapped R/W into this process — where read sectors land.
    scratch_addr: u64,
}

impl DiskReader {
    /// DMA `sector` (512 bytes) into the scratch object; `Io` on any failure.
    fn read_sector(&self, sector: u64) -> Result<(), FsError> {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.scratch,
            buf_offset: 0,
            offset: sector * SECTOR as u64,
            length: SECTOR as u64,
        };
        // SAFETY: `device` is a block DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != SECTOR as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }
}

impl BlockReader for DiskReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let sector = cur / SECTOR as u64;
            let in_sector = (cur % SECTOR as u64) as usize;
            let n = core::cmp::min(SECTOR - in_sector, buf.len() - done);
            self.read_sector(sector)?;
            // SAFETY: `scratch_addr` maps a full page R/W; the read sector occupies
            // `[0, 512)`, so `[in_sector, in_sector + n)` is in bounds.
            let src = unsafe {
                core::slice::from_raw_parts(
                    (self.scratch_addr as usize + in_sector) as *const u8,
                    n,
                )
            };
            buf[done..done + n].copy_from_slice(src);
            done += n;
        }
        Ok(())
    }
}

/// Receive the setup message on the control channel and return the transferred
/// block-device handle (its `handles[0]`). `None` on any failure.
fn recv_device(control: u64) -> Option<u64> {
    // SAFETY: one waiter on the control endpoint.
    let waited = unsafe {
        WAIT_HANDLES[0] = control;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    if waited != 1 {
        return None;
    }
    // SAFETY: valid recv out-params; on success the kernel installs the handle.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            control,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    // SAFETY: on success the kernel wrote the count + handle values.
    let count = unsafe { (&raw const RECV_COUNT).read() };
    if rr != 0 || count < 1 {
        return None;
    }
    Some(unsafe { (&raw const RECV_HANDLES[0]).read() })
}

/// Create a connected channel pair (depth 4), returning `(kernel_end, serve_end)`.
fn make_channel() -> Option<(u64, u64)> {
    static mut E0: u64 = 0;
    static mut E1: u64 = 0;
    // SAFETY: E0/E1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut E0) as u64, (&raw mut E1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const E0).read(), (&raw const E1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the
/// endpoint init binds as a Userspace Server). `false` on any failure.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"fs-server-ext4") {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes in the
    // IPC payload region (offset 24).
    let rs_len = unsafe {
        match librsproto::encode(
            &mut REPLY_MSG[24..],
            librsproto::OP_READY,
            0,
            0,
            &body[..body_len],
            1,
        ) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + the
    // transferred-handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: init's control
    // inbox starts empty, so the first Ready always has space.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// Materialise the file content (already in `CONTENT[..len]`) as a fresh read-only
/// `MemoryObject` to transfer: create it, map R/W, copy the bytes in, unmap, then
/// attenuate to `MAP_READ | TRANSFER` (read-only content the client may map +
/// receive). Returns the handle, or `None` on any failure.
fn make_content_memobj(len: usize) -> Option<u64> {
    let size = if len == 0 { PAGE } else { (len as u64).div_ceil(PAGE) * PAGE };
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, size, 0, 0, 0) };
    if mem < 0 {
        return None;
    }
    let mem = mem as u64;
    // SAFETY: register-only syscall.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return None;
    }
    // SAFETY: `addr` maps `size ≥ len` bytes R/W; `CONTENT[..len]` is initialised.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(addr as u64 as *mut u8, len);
        dst.copy_from_slice(&CONTENT[..len]);
    }
    // Unmap our own view (the object is transferred whole; keeping the mapping
    // would leak address space across requests) and attenuate to read-only content.
    // SAFETY: register-only syscalls; `addr`/`size` are our just-made mapping.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall2(SYS_HANDLE_RESTRICT, mem, RIGHT_MAP_READ | RIGHT_TRANSFER);
    }
    Some(mem)
}

/// Stamp the reply IpcMsg header — `payload_len` (@4), `handle_count` (@8) — and
/// stage the transferred handle (`handles[0]`).
fn stage_reply(payload_len: usize, handle: Option<u64>) -> usize {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid writable buffers.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let count = if let Some(h) = handle {
            REPLY_HANDLES[0] = h;
            1
        } else {
            0
        };
        REPLY_MSG[8] = count as u8;
        count
    }
}

/// Send the staged reply (`count` transferred handles) on `serve_end`. The kernel
/// completes the waiting lookup inline (the peer is its forwarding endpoint), so
/// `NoBlock` is correct and the reply is consumed regardless.
fn send_reply(serve_end: u64, count: usize) {
    // SAFETY: valid endpoint + message + `count` transferred handles.
    unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            count as u64,
            SENDMODE_NOBLOCK,
        );
    }
}

/// The serve loop: block for a forwarded `Namespace::Resolve`, resolve it, and
/// reply. Never returns (the server runs until torn down).
fn serve_loop<R: BlockReader>(reader: &R, serve_end: u64) -> ! {
    loop {
        // Block until a forwarded request lands in our inbox.
        // SAFETY: one waiter on the serving endpoint.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited != 1 {
            continue;
        }
        // SAFETY: valid recv out-params (a Resolve carries no transferred handles).
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                serve_end,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            continue;
        }

        // The rsproto request occupies the IpcMsg payload (offset 24, `payload_len`
        // bytes). Form non-aliasing slices over the distinct request/content/reply
        // statics via raw pointers.
        // SAFETY: `payload_len` is bounded to the payload region; the three slices
        // address disjoint statics, so no aliasing `&`/`&mut` is formed.
        let request_id;
        let served = unsafe {
            let payload_len = u32::from_le_bytes([
                RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7],
            ]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            request_id = librsproto::decode(req).map(|m| m.request_id).unwrap_or(0);
            let content = core::slice::from_raw_parts_mut(
                (&raw mut CONTENT) as *mut u8,
                ext4::MAX_FILE,
            );
            let reply = core::slice::from_raw_parts_mut(
                ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                MSG_LEN - PAYLOAD_OFF,
            );
            serve_resolve(reader, req, content, reply)
        };

        let count = match served {
            Served::File { reply_len, content_len } => match make_content_memobj(content_len) {
                Some(mem) => stage_reply(reply_len, Some(mem)),
                // Resolved the file but couldn't materialise the object (OOM):
                // turn the reply into an error so the lookup completes cleanly.
                None => {
                    // SAFETY: disjoint static; reply region as above.
                    let elen = unsafe {
                        let reply = core::slice::from_raw_parts_mut(
                            ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                            MSG_LEN - PAYLOAD_OFF,
                        );
                        encode_error(reply, request_id, KError::OutOfMemory.as_i32())
                    };
                    stage_reply(elen, None)
                }
            },
            Served::Error { reply_len } => stage_reply(reply_len, None),
        };
        send_reply(serve_end, count);
    }
}

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the
/// notification channel (unused), `rsi` = the inherited root namespace (unused —
/// the server resolves nothing), `rdx` = the **control channel** endpoint init
/// installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    // 1. Receive the block-device handle via the setup message.
    let device = match recv_device(control) {
        Some(d) => d,
        None => fail(b"fs-server: setup recv failed\n"),
    };

    // 2. A one-page scratch MemoryObject (mapped R/W) for the BlockReader.
    let scratch = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if scratch < 0 {
        fail(b"fs-server: scratch create failed\n");
    }
    let scratch = scratch as u64;
    let scratch_addr =
        unsafe { syscall4(SYS_MEMORY_MAP, scratch, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if scratch_addr < 0 {
        fail(b"fs-server: scratch map failed\n");
    }
    let reader = DiskReader { device, scratch, scratch_addr: scratch_addr as u64 };

    // 3. The forwarding channel: keep the serving end, hand the kernel end to init.
    let (kernel_end, serve_end) = match make_channel() {
        Some(p) => p,
        None => fail(b"fs-server: channel create failed\n"),
    };

    // 4. Announce readiness, transferring the kernel forwarding endpoint.
    if !send_ready(control, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    kprint(b"fs-server: ready (ext4, read-only)\n");

    // 5. Serve forwarded Resolve requests forever.
    serve_loop(&reader, serve_end);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
