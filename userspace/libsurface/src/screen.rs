//! The screen's size, read from `/dev/draw/screen` (`rsproto-surface-ops.md`, "Reading the
//! screen's size").
//!
//! A client that places or sizes something against the screen — a greeter centring itself, a
//! shell spanning it with bars — asks here rather than writing a size down. Until Phase 5 Part E
//! both did write it down, as 1280×800, and on the laptop's 768-row screen the shell's window list
//! was placed below the last row.

use librsproto::surface::{SCREEN_INFO_LEN, ScreenInfo};

use crate::UiError;

/// The path the compositor answers with the screen's size.
pub const SCREEN_PATH: &str = "/dev/draw/screen";

/// Resolve [`SCREEN_PATH`], map the object, and return the size it holds.
///
/// [`UiError::Transport`] when the resolve or the mapping fails — no compositor, or a namespace
/// without the leaf — and [`UiError::Malformed`] for an object that does not parse. A size of zero
/// in either dimension is refused as malformed too: nothing can be placed on it.
///
/// # Safety
///
/// `root_ns` must be a live namespace handle owned by the caller.
pub unsafe fn read(root_ns: u64) -> Result<ScreenInfo, UiError> {
    use libkern::handle::{RawHandle, Rights};
    use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

    // SAFETY: the caller guarantees `root_ns` is live and owned; `borrow` never closes.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable object holding one `ScreenInfo`.
    let obj = block_on(unsafe { ns.lookup::<Memory, MapRead>(SCREEN_PATH, Rights::MAP_READ) })
        .map_err(|_| UiError::Transport)?;
    let addr = obj.map(SCREEN_INFO_LEN).map_err(|_| UiError::Transport)?;
    // SAFETY: the compositor serves exactly `SCREEN_INFO_LEN` bytes here, mapped just above.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, SCREEN_INFO_LEN) };
    let info = ScreenInfo::read(bytes);
    // **Unmapped once read**: the compositor mints an object per resolve, and a mapping left behind
    // holds its frames for the life of this process — the leak `info` had on both sides of the
    // exchange (PR #175 review, finding 1).
    let _ = obj.unmap(addr as *mut u8, SCREEN_INFO_LEN);
    match info {
        Some(s) if s.width > 0 && s.height > 0 => Ok(s),
        _ => Err(UiError::Malformed),
    }
}
