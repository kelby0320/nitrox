//! Client-allocated frame buffers, and the one sequence that resizes them.
//!
//! **The mechanism M9 Part D asked to live here rather than in an application.** Every graphical
//! client allocates shared memory, attaches it, draws into it and hands the pixels to the
//! compositor; every client that honours a `Configure` then has to do it *again*, at a new size,
//! without touching the buffer the compositor is reading. That second half is the part with an
//! ordering rule in it, and an application that got it wrong would tear rather than fail.
//!
//! ## What the ordering rule is
//!
//! Re-attaching a buffer id replaces the memory behind it, and the compositor refuses that for
//! the buffer it is currently displaying (`docs/spec/rsproto-surface-ops.md`, `AttachBuffer`).
//! So a resize is not an operation on the window; it is a property of each buffer, applied to
//! whichever one is free at the moment it is asked for. [`BufferPool::acquire`] is therefore the
//! *only* entry point: ask for a buffer of the size you want to draw, and get one, replacing
//! what was there if it was the wrong shape. A double-buffered client converges in two frames —
//! the free buffer is replaced and committed, and the one that was on screen is replaced when
//! its release arrives.
//!
//! Nothing here is host-tested, and that is not an omission: every line of it is a memory
//! syscall or a call into [`WindowRef`], whose own record-keeping *is* host-tested against the
//! mock transport. The decision this makes — *is this buffer the size I want* — is a comparison
//! of [`WindowRef::buffer_geometry`] against a `Size`.
//!
//! ## The one place the two records could part
//!
//! This module's correctness rests on the client's idea of a buffer and the compositor's staying
//! in step, and [`WindowRef::attach`] does **not** await a reply — it returns as soon as the send
//! succeeds, so the geometry is recorded here whether or not the compositor accepted it. That is
//! unreachable as things stand: the only attach this module makes is on a buffer `next_free`
//! handed back, the client's `busy` set is a superset of the compositor's `committed`, and the
//! only refusal the compositor has for a replace is the committed buffer. It is worth knowing
//! that the argument is *that* rather than an acknowledgement, because a future caller attaching
//! outside this path would not inherit it (PR #252 review, optional 5).

use alloc::vec::Vec;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::Geometry;
use libdraw::geom::Size;
use libkern::{SYS_HANDLE_CLOSE, SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, syscall2,
    syscall4};

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
/// Owns the mappings and unmaps them on drop. It does **not** own the window: a caller that
/// destroys a window drops this too, in either order.
pub struct BufferPool {
    maps: Vec<Mapping>,
    /// The size every buffer is being brought to. Buffers reach it one at a time.
    size: Size,
}

impl BufferPool {
    /// Allocate and attach `count` buffers of `size`, as ids `0..count`.
    ///
    /// `None` if the memory could not be had or the compositor refused an attach — a caller
    /// with no buffers has no way to draw, so this is a fatal condition rather than one to
    /// carry on past. Whatever was allocated before the failure is unmapped on the way out.
    pub fn new<T: Transport>(
        window: &mut WindowRef<'_, T>,
        size: Size,
        count: usize,
    ) -> Option<Self> {
        let mut pool = BufferPool { maps: Vec::new(), size };
        for id in 0..count as u32 {
            if pool.install(window, id, size).is_none() {
                return None;
            }
        }
        Some(pool)
    }

    /// The size buffers are being brought to — what [`acquire`](Self::acquire) was last asked
    /// for.
    pub fn size(&self) -> Size {
        self.size
    }

    /// A free buffer of `size` to draw into, replacing one that is the wrong shape.
    ///
    /// **Blocks like [`WindowRef::acquire`] does**, and for the same reason: a client that has
    /// committed more frames than it has buffers must wait for a release rather than spin or
    /// give up. What this adds is the size: the buffer it hands back is one whose geometry is
    /// `size`, having replaced its memory if it was not.
    ///
    /// The buffer the compositor is displaying is never chosen — it is busy — so the refusal
    /// the compositor has for re-attaching a displayed buffer cannot be reached from here.
    pub fn acquire<T: Transport>(
        &mut self,
        window: &mut WindowRef<'_, T>,
        size: Size,
    ) -> Result<u32, UiError> {
        self.size = size;
        let id = window.acquire()?;
        let want = pitch_of(size).and_then(|p| geometry_of(size, p)).ok_or(UiError::Malformed)?;
        if window.buffer_geometry(id) == Some(want) {
            return Ok(id);
        }
        self.install(window, id, size).ok_or(UiError::Malformed)?;
        Ok(id)
    }

    /// Copy a whole frame into buffer `id`.
    ///
    /// `false` if there is no such buffer or `frame` is not exactly its length — a partial copy
    /// would leave a band of the previous frame, which is the stale-pixel bug the damage
    /// protocol exists to make rare. The caller composes into its own framebuffer and hands the
    /// whole thing over; the copy is one `memcpy` of a window, and is what lets the composition
    /// happen while the other buffer is on screen.
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
    fn install<T: Transport>(
        &mut self,
        window: &mut WindowRef<'_, T>,
        id: u32,
        size: Size,
    ) -> Option<()> {
        let pitch = pitch_of(size)?;
        let len = pitch.checked_mul(size.h as usize)?;
        let (handle, addr) = shared_buffer(len)?;
        if window.attach(id, size.w, size.h, pitch as u32, handle).is_err() {
            // **Both halves go back**, which the shape this was copied from did not do: the
            // handle is transferred by a *successful* send, so a failed one leaves it this
            // process's to close. Every caller today either exits or destroys its window, so it
            // was a leak on a dying path — but a client that carried on after a failed resize
            // would keep the object, and therefore the memory, for its whole run (PR #252
            // review, optional 6).
            //
            // SAFETY: the attach failed, so the compositor took neither the handle's object nor
            // this mapping; unmapping a range this process just mapped and closing a handle it
            // still owns.
            unsafe {
                syscall2(SYS_MEMORY_UNMAP, addr as u64, len as u64);
                syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0);
            }
            return None;
        }
        // **The old mapping goes only once the new one is attached.** Unmapping first would
        // leave a window of time with a buffer id the compositor knows and this side cannot
        // write, and nothing to put back if the allocation failed.
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
    /// **The pixels are the client's, and so is freeing them.** The compositor holds its own
    /// mapping of each object and drops that when the window goes; this side's mapping is what
    /// keeps the memory alive here, and a client that dropped the pool without unmapping would
    /// grow its address space by a window per resize.
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

/// The geometry an attach of `size` at `pitch` produces, for comparing against what a window
/// already has.
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
        syscall4(
            SYS_MEMORY_MAP,
            h as u64,
            0,
            len as u64,
            libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE,
        )
    };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; closing our only handle.
        unsafe { syscall4(SYS_HANDLE_CLOSE, h as u64, 0, 0, 0) };
        return None;
    }
    Some((h as u64, addr as *mut u8))
}
