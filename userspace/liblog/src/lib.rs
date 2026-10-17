//! `liblog` — the client side of the logging service.

#![no_std]

use libkern::*;
use librsproto::log::encode_append;

pub use librsproto::log::{LEVEL_CRITICAL, LEVEL_DEBUG, LEVEL_ERROR, LEVEL_INFO, LEVEL_TRACE, LEVEL_WARN};

const PAYLOAD_OFF: usize = 24;
/// `sys_channel_send` copies a fixed 4096-byte `IpcMsg`, so the buffer must be that size;
/// a record that does not fit is dropped rather than truncated.
const BUF: usize = 4096;

/// A log endpoint; a zero (absent) handle makes every emit a no-op.
#[derive(Copy, Clone)]
pub struct Logger {
    handle: u64,
}

impl Logger {
    /// Wrap a log-endpoint handle.
    pub const fn new(handle: u64) -> Self {
        Self { handle }
    }

    /// Whether the endpoint is present.
    pub const fn is_valid(&self) -> bool {
        self.handle != 0
    }

    /// Emit `message` at `level`.
    pub fn log(&self, level: u8, message: &str) {
        self.emit(level, message, None);
    }

    /// Emit `message` at `level` with a self-declared `source`.
    pub fn log_source(&self, level: u8, message: &str, source: &str) {
        self.emit(level, message, Some(source));
    }

    /// Emit `message` at trace level.
    pub fn trace(&self, message: &str) {
        self.log(LEVEL_TRACE, message);
    }
    /// Emit `message` at debug level.
    pub fn debug(&self, message: &str) {
        self.log(LEVEL_DEBUG, message);
    }
    /// Emit `message` at info level.
    pub fn info(&self, message: &str) {
        self.log(LEVEL_INFO, message);
    }
    /// Emit `message` at warn level.
    pub fn warn(&self, message: &str) {
        self.log(LEVEL_WARN, message);
    }
    /// Emit `message` at error level.
    pub fn error(&self, message: &str) {
        self.log(LEVEL_ERROR, message);
    }
    /// Emit `message` at critical level.
    pub fn critical(&self, message: &str) {
        self.log(LEVEL_CRITICAL, message);
    }

    fn emit(&self, level: u8, message: &str, source: Option<&str>) {
        if self.handle == 0 {
            return;
        }
        let mut buf = [0u8; BUF];
        let body_len = match encode_append(
            &mut buf[PAYLOAD_OFF..],
            level,
            message.as_bytes(),
            None,
            None,
            source.map(str::as_bytes),
        ) {
            Some(n) => n,
            None => return, // record does not fit BUF — drop
        };
        // Stamp the IpcMsg header: payload_len @4, handle_count @8 = 0.
        buf[4..8].copy_from_slice(&(body_len as u32).to_le_bytes());
        buf[8] = 0;
        let no_handles = [0u64; 1];
        // SAFETY: `buf` is a valid IpcMsg (header + payload_len payload bytes); no
        // transferred handles; NoBlock so a slow/full sink never blocks the emitter.
        unsafe {
            syscall5(
                SYS_CHANNEL_SEND,
                self.handle,
                buf.as_ptr() as u64,
                no_handles.as_ptr() as u64,
                0,
                SENDMODE_NOBLOCK,
            );
        }
    }
}

/// Resolve a log path in `ns` to a [`Logger`]; an unresolvable path yields an invalid one.
pub fn open_source(ns: u64, path: &[u8]) -> Logger {
    // SAFETY: valid path pointer + namespace handle.
    let po =
        unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_SEND | RIGHT_TRANSFER) };
    if po < 0 {
        return Logger::new(0);
    }
    let wh = [po as u64];
    let mut wr = [0u8; 24];
    // SAFETY: `wh`/`wr` are valid buffers for a single waiter.
    let waited = unsafe { syscall4(SYS_WAIT, wh.as_ptr() as u64, 1, wr.as_mut_ptr() as u64, u64::MAX) };
    // IoResult: status @8..12, resolved handle @16..24.
    let status = i32::from_le_bytes([wr[8], wr[9], wr[10], wr[11]]);
    let handle = u64::from_le_bytes([wr[16], wr[17], wr[18], wr[19], wr[20], wr[21], wr[22], wr[23]]);
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 { Logger::new(0) } else { Logger::new(handle) }
}
