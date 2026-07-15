//! `profile-server` — the userspace **profile server** (Phase 3).
//!
//! A forwarding resource server that projects the content-addressed store into
//! user-facing `/bin`: bound at `/bin` by a supervisor (init), it answers forwarded
//! `Namespace::Resolve` lookups (`/bin/foo` → suffix `foo`) by **probing** each package
//! in its manifest — `<pkg>/bin/foo` in the store, in manifest order — and **re-exporting
//! the resolved store `FileObject` handle**. It is pure name resolution: it holds no
//! file content and stays out of the data path (faults on the returned handle go
//! straight to the fs-server). See `docs/architecture/profiles-and-namespace-projection.md`.
//!
//! Structurally identical to `fs-server-ext4` at the IPC/wire/bootstrap layers; the
//! difference is the "produce the object" step (onward `sys_ns_lookup` vs. a block read).
//!
//! `#![no_std]` + `#![no_main]`; `libkern` + `libheap` (alloc for the manifest + path
//! building) + `librsproto` (the wire codec).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::namespace::{OBJECT_KIND_MEMOBJ, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use profile_server::manifest::{self, Package};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PAGE: u64 = 4096;
/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Exit the process (does not return).
fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// Resolve `path` in namespace `ns` requesting `rights`; return the resolved handle, or
/// `0` on failure. Waits + closes the `PendingOperation`; the resolved handle is the
/// caller's to close/transfer.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // IoResult: status @8..12, resolved handle @16..24.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 {
        0
    } else {
        handle
    }
}

/// Read + parse the system profile manifest from the initramfs. Returns the ordered
/// package list (empty on any failure — the server then resolves nothing).
fn read_manifest(root_ns: u64) -> Vec<Package> {
    let mem = ns_lookup(root_ns, b"/initramfs/etc/profiles/system.toml", RIGHT_MAP_READ);
    if mem == 0 {
        kprint(b"profile-server: no system profile manifest\n");
        return Vec::new();
    }
    // SAFETY: `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return Vec::new();
    }
    // SAFETY: `addr` is a MAP_READ page holding the manifest bytes + zero padding.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let packages = core::str::from_utf8(&bytes[..len])
        .map(manifest::parse)
        .unwrap_or_default();
    // SAFETY: closing our own handle (the mapping persists via its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    packages
}

/// Create a connected forwarding-channel pair (depth 4). Returns `(kernel_end,
/// serve_end)`: init binds `kernel_end` as the Userspace-Server endpoint; the server
/// serves on `serve_end`. `None` on failure.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the endpoint
/// init binds as a Userspace Server at `/bin`). `false` on any failure.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"profile-server") {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: init's control
    // inbox starts empty.
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

/// Probe the profile's packages for `suffix` (e.g. `heartbeat`), in manifest order, and
/// return the first resolving store `FileObject` handle (requested rights + `TRANSFER`,
/// so it can be re-exported). `0` if no package provides it.
fn resolve_in_store(root_ns: u64, packages: &[Package], suffix: &[u8], rights: u64) -> u64 {
    let name = match core::str::from_utf8(suffix) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for pkg in packages {
        // <store path>/bin/<name>
        let path = format!("{}/bin/{}", pkg.path, name);
        let h = ns_lookup(root_ns, path.as_bytes(), rights | RIGHT_TRANSFER);
        if h != 0 {
            return h;
        }
    }
    0
}

/// Send a success reply on `serve_end` transferring the resolved handle. The kernel
/// completes the original caller's lookup inline (installs the handle), so `NoBlock`.
fn reply_success(serve_end: u64, request_id: u64, handle: u64) {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    // `content_len` is unused for a transferred handle (the kernel installs it directly).
    let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header (payload_len @4, handle_count @8) + the handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Send an error reply on `serve_end` (no transferred handle), echoing `op`/`request_id`
/// so the kernel routes it to the right pending lookup.
fn reply_error(serve_end: u64, request_id: u64, op: u16, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer.
    let rs_len = unsafe {
        match encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header; no transferred handles.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// The serve loop: block for a forwarded `Namespace::Resolve`, resolve it into the
/// store, and reply. Never returns.
fn serve_loop(root_ns: u64, serve_end: u64, packages: &[Package]) -> ! {
    kprint(b"profile-server: serving /bin over the store\n");
    loop {
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

        // Decode the rsproto request from the IpcMsg payload (offset 24, `payload_len`).
        // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG.
        let (op, request_id, handle, ok) = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]])
                as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    Some(r) => {
                        let h = resolve_in_store(root_ns, packages, r.suffix, r.requested_rights);
                        (m.op, m.request_id, h, true)
                    }
                    None => (m.op, m.request_id, 0, false),
                },
                Ok(m) => (m.op, m.request_id, 0, false),
                Err(_) => (0, 0, 0, false),
            }
        };

        if ok && handle != 0 {
            reply_success(serve_end, request_id, handle);
        } else if op == OP_NS_RESOLVE {
            reply_error(serve_end, request_id, op, KError::NotFound.as_i32());
        } else {
            reply_error(serve_end, request_id, op, KError::Unsupported.as_i32());
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited
/// root namespace (used — resolves `/store` + `/initramfs`), `rdx` = the control-channel
/// endpoint init installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"profile-server: up\n");
    // Read the manifest now — before init releases the initramfs.
    let packages = read_manifest(root_ns);
    kprint(b"profile-server: manifest loaded\n");

    let (kernel_end, serve_end) = match make_channel() {
        Some(pair) => pair,
        None => {
            kprint(b"profile-server: channel create FAIL\n");
            exit(1);
        }
    };
    if !send_ready(control, kernel_end) {
        kprint(b"profile-server: Ready send FAIL\n");
        exit(1);
    }
    serve_loop(root_ns, serve_end, &packages);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"profile-server: PANIC\n");
    exit(1);
}
