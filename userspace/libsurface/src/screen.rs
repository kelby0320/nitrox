//! The screen's size, read from `/dev/draw/screen` (`rsproto-surface-ops.md`, "Reading the
//! screen's size").
//!
//! A client that places or sizes something against the screen asks here rather than writing a
//! size down: a hard-coded 1280×800 put a window list below the last row of a 768-row screen.

use libkern::handle::Rights;
use libos::{CanLookup, Handle, MapRead, Memory, Namespace, Ref, block_on};
use librsproto::surface::{SCREEN_INFO_LEN, ScreenInfo};

use crate::UiError;

/// The path the compositor answers with the screen's size.
pub const SCREEN_PATH: &str = "/dev/draw/screen";

/// Resolve [`SCREEN_PATH`] in `ns`, map the object, and return the size it holds.
///
/// [`UiError::Transport`] when the resolve or the mapping fails, and [`UiError::Malformed`] for
/// an object that does not parse or holds a zero dimension.
pub fn info<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>) -> Result<ScreenInfo, UiError> {
    let obj: Handle<Memory, MapRead> =
        block_on(ns.into().lookup(SCREEN_PATH, Rights::MAP_READ)).map_err(|_| UiError::Transport)?;
    // The compositor mints an object per resolve; a mapping left behind would hold its frames for
    // the life of this process.
    let mut bytes = [0u8; SCREEN_INFO_LEN];
    obj.map(SCREEN_INFO_LEN).and_then(|m| m.read_into(0, &mut bytes)).map_err(|_| UiError::Transport)?;
    match ScreenInfo::read(&bytes) {
        Some(s) if s.width > 0 && s.height > 0 => Ok(s),
        _ => Err(UiError::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::handle::RawHandle;
    use libos::{MapReadWrite, NsMutable, NsReadOnly, fs, mock};

    /// A namespace whose `/dev/draw/screen` is a file holding `info`.
    fn screen_with(info: ScreenInfo) -> Handle<Namespace, NsMutable> {
        mock::reset();
        let ns = Handle::<Namespace, NsMutable>::create().unwrap();
        let rw = Rights::MAP_READ | Rights::MAP_WRITE;
        let f: Handle<libos::File, MapReadWrite> =
            block_on(fs::create(&ns, SCREEN_PATH, SCREEN_INFO_LEN as u64, rw)).unwrap();
        let mut bytes = [0u8; SCREEN_INFO_LEN];
        info.write(&mut bytes).unwrap();
        f.map(SCREEN_INFO_LEN).unwrap().write_from(0, &bytes).unwrap();
        ns
    }

    #[test]
    fn the_screen_is_read_from_the_object_and_unmapped_after() {
        let ns = screen_with(ScreenInfo { width: 1280, height: 800 });
        assert_eq!(info(&ns), Ok(ScreenInfo { width: 1280, height: 800 }));
        assert_eq!(mock::mappings(), 0, "the read leaves nothing mapped");
    }

    #[test]
    fn a_zero_dimension_is_malformed_and_a_missing_object_is_a_transport_failure() {
        let ns = screen_with(ScreenInfo { width: 1280, height: 0 });
        assert_eq!(info(&ns), Err(UiError::Malformed));
        let zeroed = mock::served(SCREEN_PATH);
        // SAFETY: the mock's handle, open for the test.
        let zeroed = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(zeroed), Rights::LOOKUP) };
        assert_eq!(info(zeroed), Err(UiError::Malformed), "an object of zeros holds no screen");
        let served = mock::served("/dev/draw/other");
        // SAFETY: the mock's handle, open for the test.
        let served = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(served), Rights::LOOKUP) };
        assert_eq!(info(served), Err(UiError::Transport), "nothing answers /dev/draw/screen");
    }
}
