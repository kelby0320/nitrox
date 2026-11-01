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
use libkern::{SYS_HANDLE_CLOSE, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, syscall2, syscall4};

use crate::{Transport, UiError, WindowRef};

/// One buffer's memory, on this side.
struct Mapping {
    /// The id the compositor knows it by.
    id: u32,
    /// Where it is mapped in this process.
    addr: *mut u8,
    /// How many bytes, for the unmap.
    len: usize,
}

/// The shared memory behind one window's buffers.
///
/// Owns the mappings and unmaps them on drop; does not own the window.
pub struct BufferPool {
    maps: Vec<Mapping>,
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
    pub fn write(&mut self, id: u32, frame: &[u8]) -> bool {
        let Some(m) = self.maps.iter().find(|m| m.id == id) else { return false };
        if frame.len() != m.len {
            return false;
        }
        // SAFETY: `m.addr` maps `m.len` writable bytes for this process, `frame` holds exactly
        // that many, and the two are distinct allocations — the mapping came from
        // `sys_memory_map` and the frame from the caller's own framebuffer.
        unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), m.addr, m.len) };
        true
    }

    /// Allocate memory for `id` at `size`, attach it, and drop whatever was there.
    fn install<T: Transport>(&mut self, window: &mut WindowRef<'_, T>, id: u32, size: Size) -> Option<()> {
        let pitch = pitch_of(size)?;
        let len = pitch.checked_mul(size.h as usize)?;
        let (handle, addr) = shared_buffer(len)?;
        if window.attach_with_format(id, size.w, size.h, pitch as u32, handle, self.format).is_err() {
            // The handle is transferred by a successful send only, so a failed one leaves it
            // this process's to close.
            // SAFETY: the attach failed, so the compositor took neither the handle's object nor
            // this mapping: a range this process just mapped, a handle it still owns.
            unsafe {
                syscall2(SYS_MEMORY_UNMAP, addr as u64, len as u64);
                syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0);
            }
            return None;
        }
        // Unmapping first would leave a buffer id the compositor knows and this side cannot
        // write, and nothing to put back if the attach failed.
        if let Some(old) = self.maps.iter().position(|m| m.id == id) {
            let old = self.maps.swap_remove(old);
            // SAFETY: unmapping a range this pool mapped and is no longer using; the
            // compositor's own mapping of that object is its to drop, and the attach above
            // told it to.
            unsafe { syscall2(SYS_MEMORY_UNMAP, old.addr as u64, old.len as u64) };
        }
        self.maps.push(Mapping { id, addr, len });
        Some(())
    }
}

impl Drop for BufferPool {
    /// Unmap every buffer.
    ///
    /// The compositor drops its own mapping when the window goes; this side's must be unmapped
    /// or the address space grows by a window per resize.
    fn drop(&mut self) {
        for m in &self.maps {
            // SAFETY: `addr`/`len` came from a successful `sys_memory_map` in `install`, and a
            // mapping is recorded once, so this unmaps once.
            unsafe { syscall2(SYS_MEMORY_UNMAP, m.addr as u64, m.len as u64) };
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

/// Create a `MemoryObject` of `len` bytes and map it read-write.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: mapping an object this process just created, read-write.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE)
    };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; closing our only handle.
        unsafe { syscall4(SYS_HANDLE_CLOSE, h as u64, 0, 0, 0) };
        return None;
    }
    Some((h as u64, addr as *mut u8))
}
