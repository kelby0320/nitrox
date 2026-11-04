//! Client-allocated frame buffers, and the one sequence that resizes them.
//!
//! Re-attaching a buffer id replaces the memory behind it, and the compositor refuses that for
//! the buffer it is displaying (`docs/spec/rsproto-surface-ops.md`, `AttachBuffer`). A resize is
//! therefore a property of each buffer, applied to whichever is free when asked for:
//! [`BufferPool::acquire`] is the only entry point. A double-buffered client converges in two
//! frames: the free buffer is replaced and committed, the displayed one when its release arrives.
//!
//! [`WindowRef::attach`] does not await a reply, so the geometry is recorded whether or not the
//! compositor accepted it. That is unreachable here: the only attach is on a buffer `next_free`
//! returned, the client's `busy` set is a superset of the compositor's `committed`, the only
//! refusal for a replace is the committed buffer, and this memory is what an attach asks for, which
//! a compositor that cannot map draws as nothing rather than refuse. A caller attaching outside
//! this path would not inherit that argument.

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::Geometry;
use libdraw::geom::Size;
use libos::{Handle, MapReadWrite, Mapping, Memory};

use crate::{Transport, UiError, WindowRef};

/// One buffer's memory, on this side.
struct Buffer {
    /// The id the compositor knows it by.
    id: u32,
    /// Where it is mapped in this process, unmapped with it.
    map: Mapping<MapReadWrite>,
}

/// The shared memory behind one window's buffers.
///
/// Owns the mappings and unmaps them on drop; does not own the window.
pub struct BufferPool {
    maps: Vec<Buffer>,
    /// The size every buffer is being brought to, one at a time.
    size: Size,
    /// The format every buffer is attached in, so a resize does not revert to opaque.
    format: PixelFormat,
}

impl BufferPool {
    /// Allocate and attach `count` buffers of `size`, as ids `0..count`.
    ///
    /// `None` if the memory could not be had or an attach failed; whatever was allocated
    /// before is unmapped on the way out.
    pub fn new<T: Transport>(window: &mut WindowRef<'_, T>, size: Size, count: usize) -> Option<Self> {
        Self::in_format(window, size, count, PixelFormat::XRGB8888)
    }

    /// [`new`](Self::new), in a chosen pixel format.
    ///
    /// `PixelFormat::ARGB8888` gives a translucent window; see
    /// [`WindowRef::attach_with_format`](crate::WindowRef::attach_with_format). The format is
    /// remembered, so a resize's re-attach keeps it: `install` is the only place this struct
    /// attaches anything.
    pub fn in_format<T: Transport>(
        window: &mut WindowRef<'_, T>,
        size: Size,
        count: usize,
        format: PixelFormat,
    ) -> Option<Self> {
        let mut pool = BufferPool { maps: Vec::new(), size, format };
        for id in 0..count as u32 {
            pool.install(window, id, size)?;
        }
        Some(pool)
    }

    /// The size [`acquire`](Self::acquire) was last asked for.
    pub fn size(&self) -> Size {
        self.size
    }

    /// A free buffer of `size` to draw into, replacing one that is the wrong shape.
    ///
    /// Blocks like [`WindowRef::acquire`]. The buffer returned has geometry `size`, its memory
    /// replaced if it had not. The displayed buffer is busy and never chosen, so the
    /// compositor's refusal to re-attach it cannot be reached from here.
    pub fn acquire<T: Transport>(&mut self, window: &mut WindowRef<'_, T>, size: Size) -> Result<u32, UiError> {
        self.size = size;
        let id = window.acquire()?;
        let want = pitch_of(size).and_then(|p| geometry_of(size, p)).ok_or(UiError::Malformed)?;
        if window.buffer_geometry(id) == Some(want) {
            return Ok(id);
        }
        self.install(window, id, size).ok_or(UiError::Malformed)?;
        Ok(id)
    }

    /// Copy a whole frame into buffer `id`; `false` if there is no such buffer or `frame` is not
    /// exactly its length, since a partial copy would leave a band of the previous frame.
    ///
    /// The compositor reads the buffer while this writes it, so the bytes are copied in and never
    /// borrowed.
    pub fn write(&mut self, id: u32, frame: &[u8]) -> bool {
        let Some(m) = self.maps.iter_mut().find(|m| m.id == id) else { return false };
        frame.len() == m.map.len() && m.map.write_from(0, frame).is_ok()
    }

    /// Allocate memory for `id` at `size`, attach it, and drop whatever was there.
    fn install<T: Transport>(&mut self, window: &mut WindowRef<'_, T>, id: u32, size: Size) -> Option<()> {
        let pitch = pitch_of(size)?;
        let len = pitch.checked_mul(size.h as usize)?;
        let memory = Handle::<Memory, MapReadWrite>::create(len).ok()?;
        let map = memory.map(len).ok()?;
        // A failed attach hands the handle back to be closed here: only a successful send
        // transfers it. The mapping unmaps with `map`.
        window.attach_memory(id, size.w, size.h, pitch as u32, memory, self.format).ok()?;
        // Dropping the old mapping first would leave a buffer id the compositor knows and this
        // side cannot write, and nothing to put back if the attach failed.
        if let Some(old) = self.maps.iter().position(|m| m.id == id) {
            self.maps.swap_remove(old);
        }
        self.maps.push(Buffer { id, map });
        Some(())
    }
}

impl<T: Transport> WindowRef<'_, T> {
    /// [`attach_with_format`](Self::attach_with_format), transferring `memory`. Handed back with the
    /// error if the attach failed, since only a successful send moves it.
    pub fn attach_memory(
        &mut self,
        buffer_id: u32,
        width: u32,
        height: u32,
        pitch: u32,
        memory: Handle<Memory, MapReadWrite>,
        format: PixelFormat,
    ) -> Result<(), (UiError, Handle<Memory, MapReadWrite>)> {
        match self.attach_with_format(buffer_id, width, height, pitch, memory.raw().0, format) {
            Ok(()) => {
                memory.into_raw();
                Ok(())
            }
            Err(e) => Err((e, memory)),
        }
    }
}

/// Bytes per row for `size` in `XRGB8888`, or `None` if that overflows.
fn pitch_of(size: Size) -> Option<usize> {
    (size.w as usize).checked_mul(4)
}

/// The geometry an attach of `size` at `pitch` produces.
fn geometry_of(size: Size, pitch: usize) -> Option<Geometry> {
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::harness::*;
    use crate::{CreateWindowRequest, Role, Session};
    use libkern::handle::{RawHandle, Rights};
    use libos::{Channel, Only, mock};
    use librsproto::surface::{ConfigureEvent, OP_ATTACH_BUFFER, OP_CONFIGURE, OP_CREATE_WINDOW};

    const SIZE: Size = Size { w: 8, h: 4 };
    const BYTES: usize = 8 * 4 * 4;

    /// A session with one configured window, and the compositor's end of its channel.
    fn session() -> (Session<crate::ipc::ChannelTransport>, u32, Handle<Channel, Only>) {
        mock::reset();
        let (t, peer) = connected();
        reply(&peer, OP_CREATE_WINDOW, 1, &7u32.to_le_bytes());
        let mut cfg = [0u8; 20];
        ConfigureEvent { window: 7, width: 8, height: 4, x: 0, y: 0 }.write(&mut cfg).unwrap();
        push(&peer, OP_CONFIGURE, 0, 0, &cfg);
        let mut s = Session::new(t);
        let id = s.create(&CreateWindowRequest::new(8, 4, Role::Normal), 2).expect("a window");
        assert!(taken(&peer).is_some(), "the create request");
        (s, id, peer)
    }

    #[test]
    fn each_buffer_is_mapped_and_its_memory_goes_to_the_compositor() {
        let (mut s, id, peer) = session();
        let open = mock::open_handles();
        let mut w = s.window(id).unwrap();
        let pool = BufferPool::new(&mut w, SIZE, 2).expect("two buffers");
        assert_eq!(mock::mappings(), 2);
        for want in [0u32, 1] {
            let (op, _, body, carried) = taken(&peer).expect("an attach");
            assert_eq!(op, OP_ATTACH_BUFFER);
            assert_eq!(u32::from_le_bytes(body[4..8].try_into().unwrap()), want);
            assert_eq!(carried.len(), 1, "the memory travels with it");
        }
        assert_eq!(mock::open_handles(), open, "the memory moved to the compositor: none left open here");
        assert_eq!(mock::stray_closes(), 0, "a handle that moved is not closed again");
        drop(pool);
        assert_eq!(mock::mappings(), 0, "unmapped with the pool");
    }

    #[test]
    fn a_frame_is_copied_in_whole_or_not_at_all() {
        let (mut s, id, _peer) = session();
        let mut w = s.window(id).unwrap();
        let mut pool = BufferPool::new(&mut w, SIZE, 2).unwrap();
        let frame: std::vec::Vec<u8> = (0..BYTES as u8).collect();
        assert!(pool.write(1, &frame));
        let mut back = [0u8; BYTES];
        pool.maps.iter().find(|m| m.id == 1).unwrap().map.read_into(0, &mut back).unwrap();
        assert_eq!(back.as_slice(), frame.as_slice());
        assert!(!pool.write(1, &frame[1..]), "a short frame leaves a band of the last one");
        assert!(!pool.write(9, &frame), "no such buffer");
    }

    #[test]
    fn a_resize_replaces_one_mapping_and_keeps_the_count() {
        let (mut s, id, _peer) = session();
        let mut w = s.window(id).unwrap();
        let mut pool = BufferPool::new(&mut w, SIZE, 2).unwrap();
        let bigger = Size { w: 16, h: 8 };
        let got = pool.acquire(&mut w, bigger).expect("a free buffer");
        assert_eq!(w.buffer_geometry(got).map(|g| (g.width, g.height)), Some((16, 8)));
        assert_eq!((mock::mappings(), pool.maps.len()), (2, 2), "the old mapping went when the new one came");
        assert_eq!(pool.maps.iter().find(|m| m.id == got).unwrap().map.len(), 16 * 8 * 4);
    }

    #[test]
    fn a_refused_attach_unmaps_and_closes_what_it_made() {
        let (mut s, id, peer) = session();
        drop(peer);
        let open = mock::open_handles();
        let mut w = s.window(id).unwrap();
        assert!(BufferPool::new(&mut w, SIZE, 2).is_none());
        assert_eq!(mock::mappings(), 0);
        assert_eq!(mock::open_handles(), open, "the memory handle was closed, not leaked");
    }

    #[test]
    fn the_raw_attach_leaves_a_refused_handle_with_its_caller() {
        let (mut s, id, peer) = session();
        let mem = Handle::<Memory, MapReadWrite>::create(BYTES).unwrap().into_raw().0;
        drop(peer);
        let mut w = s.window(id).unwrap();
        assert_eq!(w.attach(0, 8, 4, 32, mem), Err(UiError::Transport));
        assert!(mock::is_open(mem));
        // SAFETY: `mem` is the test's own handle, wrapped once; the raw attach left it open.
        let typed = unsafe { Handle::<Memory, MapReadWrite>::from_raw(RawHandle(mem), Rights::empty()) };
        let (e, back) = w.attach_memory(0, 8, 4, 32, typed, PixelFormat::XRGB8888).unwrap_err();
        assert_eq!(e, UiError::Transport);
        drop(back);
        assert!(!mock::is_open(mem), "the typed form hands it back to be closed");
    }
}
