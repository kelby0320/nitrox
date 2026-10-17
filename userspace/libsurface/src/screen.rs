//! The screen's size, read from `/dev/draw/screen` (`rsproto-surface-ops.md`, "Reading the
//! screen's size").
//!
//! A client that places or sizes something against the screen asks here rather than writing a
//! size down: a hard-coded 1280×800 put a window list below the last row of a 768-row screen.

use librsproto::surface::{SCREEN_INFO_LEN, ScreenInfo};

use crate::UiError;

/// The path the compositor answers with the screen's size.
pub const SCREEN_PATH: &str = "/dev/draw/screen";

/// Resolve [`SCREEN_PATH`], map the object, and return the size it holds.
///
/// [`UiError::Transport`] when the resolve or the mapping fails, and [`UiError::Malformed`] for
/// an object that does not parse or holds a zero dimension.
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
    // The compositor mints an object per resolve; a mapping left behind holds its frames for the
    // life of this process.
    let _ = obj.unmap(addr, SCREEN_INFO_LEN);
    match info {
        Some(s) if s.width > 0 && s.height > 0 => Ok(s),
        _ => Err(UiError::Malformed),
    }
}
