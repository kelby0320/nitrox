//! The [`Framebuffer`] seam, and the two implementations behind it.
//!
//! This is the trait `docs/design/display-substrate.md` §8a names: "base, width,
//! height, pitch, format — with a real implementation over Limine's mapping and an
//! **in-memory one for tests**". It is what turns "composite these surfaces with this
//! damage" into a pure function, assertable pixel-exactly in milliseconds instead of
//! through a boot, exactly as `BlockReader` did for the ext4 parser and `Host` for the
//! shell's evaluator.

use alloc::vec;
use alloc::vec::Vec;

use crate::format::{PixelFormat, Rgb};
use crate::geom::{Rect, Size};

/// Everything about a pixel buffer's shape that cannot be inferred from its bytes.
///
/// `pitch` is bytes per **row**, not `width × bytes_per_pixel`: firmware routinely
/// pads rows for alignment, and assuming otherwise writes every row after the first
/// at a slightly wrong offset — a skew that a solid-colour test cannot detect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row, which is `>= width * format.bytes_per_pixel()`.
    pub pitch: usize,
    /// Channel layout.
    pub format: PixelFormat,
}

impl Geometry {
    /// Geometry with the tightest legal pitch for `width`.
    ///
    /// # Panics
    ///
    /// If `format` is not 32 bits per pixel. See [`Geometry::with_pitch`] for why that is
    /// checked here rather than at the write sites.
    pub fn packed(width: u32, height: u32, format: PixelFormat) -> Self {
        Self::with_pitch(width, height, width as usize * format.bytes_per_pixel(), format)
            .expect("a packed pitch always holds a row; only the depth can fail")
    }

    /// Geometry with an explicit row stride.
    ///
    /// Returns `None` if `pitch` cannot hold a row (which would alias rows onto each
    /// other), or if `format` is not 32 bits per pixel.
    ///
    /// **The depth check belongs here**, at the one point every framebuffer passes
    /// through, rather than at each write. `PixelFormat`'s fields are public, so a 16-bpp
    /// format is constructible; [`Framebuffer::put_pixel`] and
    /// [`Framebuffer::fill_rect`] write a 4-byte word while striding by
    /// `bytes_per_pixel()`, so such a format would index past the buffer's end and panic
    /// on the last pixel of a row. Refusing the geometry makes that unreachable instead of
    /// latent. (`from_limine` refuses non-32-bpp too, but nothing forces a `Geometry` to
    /// come from firmware.)
    pub const fn with_pitch(
        width: u32,
        height: u32,
        pitch: usize,
        format: PixelFormat,
    ) -> Option<Self> {
        if format.bits_per_pixel != 32 {
            return None;
        }
        if pitch < width as usize * format.bytes_per_pixel() {
            return None;
        }
        Some(Self { width, height, pitch, format })
    }

    /// The visible area as a rectangle at the origin.
    pub const fn bounds(&self) -> Rect {
        Rect::from_size(Size::new(self.width, self.height))
    }

    /// Bytes needed to store the buffer, including row padding.
    ///
    /// The last row needs only its visible span, but buffers are sized to whole rows
    /// because that is what every producer of one actually allocates.
    pub const fn byte_len(&self) -> usize {
        self.pitch * self.height as usize
    }

    /// Byte offset of pixel `(x, y)`, or `None` if it lies outside the visible area.
    pub const fn offset_of(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(y as usize * self.pitch + x as usize * self.format.bytes_per_pixel())
    }
}

/// A writable pixel buffer of known geometry.
///
/// Implementors supply geometry and byte access; every drawing operation is a
/// provided method, so the real framebuffer and the test one share one
/// implementation of the logic rather than two that can drift.
pub trait Framebuffer {
    /// The buffer's shape.
    fn geometry(&self) -> Geometry;

    /// The backing bytes, at least [`Geometry::byte_len`] long.
    fn bytes(&self) -> &[u8];

    /// The backing bytes, mutably.
    fn bytes_mut(&mut self) -> &mut [u8];

    /// Read a single pixel, or `None` outside the visible area.
    fn get_pixel(&self, x: u32, y: u32) -> Option<Rgb> {
        let g = self.geometry();
        let off = g.offset_of(x, y)?;
        let b = self.bytes();
        let word = u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]]);
        Some(g.format.decode(word))
    }

    /// Write a single pixel. Writes outside the visible area are dropped.
    fn put_pixel(&mut self, x: u32, y: u32, colour: Rgb) {
        let g = self.geometry();
        let Some(off) = g.offset_of(x, y) else { return };
        let word = g.format.encode(colour).to_le_bytes();
        self.bytes_mut()[off..off + 4].copy_from_slice(&word);
    }

    /// Fill `rect`, clipped to the visible area, with a solid colour.
    ///
    /// The row's starting offset comes from [`Geometry::offset_of`] rather than being
    /// recomputed here. That is deliberate: an earlier version open-coded
    /// `y * pitch + x * bpp`, and breaking `offset_of` to check the tests were not
    /// vacuous left this path silently correct — two copies of the same arithmetic,
    /// only one of them covered.
    fn fill_rect(&mut self, rect: Rect, colour: Rgb) {
        let g = self.geometry();
        let Some(clipped) = rect.intersect(&g.bounds()) else { return };
        let word = g.format.encode(colour).to_le_bytes();
        let bpp = g.format.bytes_per_pixel();
        let bytes = self.bytes_mut();
        for row in 0..clipped.size.h {
            let y = clipped.origin.y as u32 + row;
            let Some(start) = g.offset_of(clipped.origin.x as u32, y) else { continue };
            for col in 0..clipped.size.w as usize {
                let off = start + col * bpp;
                bytes[off..off + 4].copy_from_slice(&word);
            }
        }
    }

    /// Fill the entire visible area.
    fn clear(&mut self, colour: Rgb) {
        let bounds = self.geometry().bounds();
        self.fill_rect(bounds, colour);
    }
}

/// A framebuffer backed by an owned `Vec`, for host tests.
///
/// The point of the whole trait: this is the implementation the compositing tests
/// run against, so a wrong pixel is a failing assertion in milliseconds rather than a
/// screenshot someone squints at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MemFramebuffer {
    geometry: Geometry,
    bytes: Vec<u8>,
}

impl MemFramebuffer {
    /// A zeroed buffer of the given geometry.
    pub fn new(geometry: Geometry) -> Self {
        Self { geometry, bytes: vec![0u8; geometry.byte_len()] }
    }

    /// A buffer of the given geometry, filled with `colour`.
    pub fn filled(geometry: Geometry, colour: Rgb) -> Self {
        let mut fb = Self::new(geometry);
        fb.clear(colour);
        fb
    }

    /// Consume the framebuffer, yielding its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Framebuffer for MemFramebuffer {
    fn geometry(&self) -> Geometry {
        self.geometry
    }
    fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

/// A framebuffer over memory this crate does not own — a mapped device aperture.
///
/// This is the shape the compositor uses once the kernel hands the framebuffer to
/// userspace (plan M1 Part B). It is kept here, and host-tested over an ordinary
/// allocation, so that the raw-pointer path is exercised before any kernel work
/// exists to hide a mistake in it.
pub struct RawFramebuffer {
    geometry: Geometry,
    base: *mut u8,
    len: usize,
}

impl RawFramebuffer {
    /// Wrap a mapped framebuffer.
    ///
    /// # Safety
    ///
    /// `base` must point to at least `geometry.byte_len()` bytes that stay mapped,
    /// writable, and un-aliased for the lifetime of the returned value. The caller is
    /// the process holding the framebuffer binding; nothing else may write the same
    /// mapping concurrently, since compositing reads back what it wrote.
    pub const unsafe fn new(geometry: Geometry, base: *mut u8) -> Self {
        Self { geometry, base, len: geometry.byte_len() }
    }
}

impl Framebuffer for RawFramebuffer {
    fn geometry(&self) -> Geometry {
        self.geometry
    }
    fn bytes(&self) -> &[u8] {
        // SAFETY: `new` requires `base` to address `len` readable bytes that remain
        // mapped for this value's lifetime, and `&self` borrows it for no longer.
        unsafe { core::slice::from_raw_parts(self.base, self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, plus `&mut self` is the exclusive borrow the mutable
        // slice needs. `new`'s contract forbids any other writer to this mapping.
        unsafe { core::slice::from_raw_parts_mut(self.base, self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom() -> Geometry {
        Geometry::packed(8, 4, PixelFormat::XRGB8888)
    }

    #[test]
    fn a_new_buffer_is_zeroed_and_sized_to_whole_rows() {
        let fb = MemFramebuffer::new(Geometry::with_pitch(8, 4, 40, PixelFormat::XRGB8888).unwrap());
        assert_eq!(fb.bytes().len(), 160);
        assert!(fb.bytes().iter().all(|&b| b == 0));
    }

    #[test]
    fn with_pitch_refuses_a_stride_too_narrow_for_a_row() {
        // 8 px × 4 bytes needs 32; 31 would alias row n+1 over row n.
        assert!(Geometry::with_pitch(8, 4, 31, PixelFormat::XRGB8888).is_none());
        assert!(Geometry::with_pitch(8, 4, 32, PixelFormat::XRGB8888).is_some());
    }

    #[test]
    fn a_geometry_refuses_a_depth_the_write_paths_cannot_handle() {
        // `put_pixel`/`fill_rect` write 4 bytes while striding by `bytes_per_pixel()`, so
        // a 16-bpp geometry would index past the end of the last pixel in a row. Refused
        // at construction rather than left to panic later.
        let rgb565 = PixelFormat {
            bits_per_pixel: 16,
            red: crate::format::Channel::new(11, 5),
            green: crate::format::Channel::new(5, 6),
            blue: crate::format::Channel::new(0, 5),
        };
        assert!(Geometry::with_pitch(4, 2, 16, rgb565).is_none());
        assert!(Geometry::with_pitch(4, 2, 16, PixelFormat::XRGB8888).is_some());
    }

    #[test]
    fn pixels_round_trip_through_put_and_get() {
        let mut fb = MemFramebuffer::new(geom());
        let c = Rgb::new(0x11, 0x22, 0x33);
        fb.put_pixel(3, 2, c);
        assert_eq!(fb.get_pixel(3, 2), Some(c));
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::BLACK));
    }

    #[test]
    fn out_of_bounds_access_is_dropped_rather_than_panicking() {
        let mut fb = MemFramebuffer::new(geom());
        fb.put_pixel(8, 0, Rgb::new(1, 2, 3));
        fb.put_pixel(0, 4, Rgb::new(1, 2, 3));
        assert_eq!(fb.get_pixel(8, 0), None);
        assert_eq!(fb.get_pixel(0, 4), None);
        assert!(fb.bytes().iter().all(|&b| b == 0), "nothing should have been written");
    }

    #[test]
    fn padding_bytes_are_never_written_by_drawing() {
        // A row-padded buffer: the pad must stay zero, because the gate hashes only
        // visible pixels and a writer that runs into the pad has the wrong stride.
        let g = Geometry::with_pitch(4, 2, 24, PixelFormat::XRGB8888).unwrap();
        let mut fb = MemFramebuffer::new(g);
        fb.clear(Rgb::new(0xFF, 0xFF, 0xFF));
        for row in 0..2usize {
            let pad = &fb.bytes()[row * 24 + 16..row * 24 + 24];
            assert!(pad.iter().all(|&b| b == 0), "row {row} padding was written");
        }
    }

    #[test]
    fn fill_rect_clips_at_every_edge() {
        let mut fb = MemFramebuffer::new(geom());
        let c = Rgb::new(9, 9, 9);
        // Straddles the left and top edges.
        fb.fill_rect(Rect::new(-2, -1, 4, 3), c);
        assert_eq!(fb.get_pixel(0, 0), Some(c));
        assert_eq!(fb.get_pixel(1, 1), Some(c));
        assert_eq!(fb.get_pixel(2, 0), Some(Rgb::BLACK));
        // Entirely off-screen writes nothing.
        let before = fb.clone();
        fb.fill_rect(Rect::new(100, 100, 4, 4), Rgb::new(7, 7, 7));
        assert_eq!(fb, before);
    }

    #[test]
    fn a_raw_framebuffer_over_an_allocation_matches_the_in_memory_one() {
        // Exercises the raw-pointer path before any kernel plumbing exists to hide a
        // mistake in it.
        let g = Geometry::with_pitch(6, 3, 32, PixelFormat::XRGB8888).unwrap();
        let mut backing = vec![0u8; g.byte_len()];
        // SAFETY: `backing` outlives `raw`, is exactly `byte_len()` bytes, is
        // writable, and is not aliased while `raw` holds it.
        let mut raw = unsafe { RawFramebuffer::new(g, backing.as_mut_ptr()) };
        let mut mem = MemFramebuffer::new(g);

        for fb in [&mut raw as &mut dyn Framebuffer, &mut mem as &mut dyn Framebuffer] {
            fb.clear(Rgb::new(0x20, 0x30, 0x40));
            fb.fill_rect(Rect::new(1, 1, 3, 1), Rgb::new(0xAA, 0xBB, 0xCC));
        }
        assert_eq!(raw.bytes(), mem.bytes());
    }
}
