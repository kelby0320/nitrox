//! Minimal framebuffer console for Phase 0.
//!
//! Owns a raw pointer to the Limine-provided linear framebuffer and walks
//! it scanline-by-scanline via the protocol's `pitch` (bytes per row). All
//! drawing routines are intentionally simple — we do not maintain a back
//! buffer, scroll, or honour ANSI; this exists only to prove the boot path
//! works visibly.

use crate::font::FONT_8X16;
use crate::limine::Framebuffer;

/// 24-bit RGB colour.
#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[allow(dead_code)]
impl Rgb {
    pub const BLACK: Self = Rgb(0x00, 0x00, 0x00);
    pub const WHITE: Self = Rgb(0xff, 0xff, 0xff);
    /// Background — a dark slate. Chosen so the boot text is unambiguous
    /// even if QEMU's window isn't fully focused.
    pub const BG: Self = Rgb(0x0a, 0x18, 0x2c);
    pub const FG: Self = Rgb(0xea, 0xee, 0xf2);
    /// Nitrox tank decal palette. Convention: a yellow band bordered by
    /// dark green bands, with "NITROX" lettered on the yellow. These
    /// shades approximate the PADI/SCUBA standard markings.
    pub const NITROX_YELLOW: Self = Rgb(0xff, 0xcc, 0x00);
    pub const NITROX_GREEN: Self = Rgb(0x00, 0x66, 0x33);
}

/// Glyph cell dimensions. The hand-coded font in `font.rs` is 8x16.
const GLYPH_W: usize = 8;
const GLYPH_H: usize = 16;

pub struct FbWriter {
    base: *mut u8,
    width: usize,
    height: usize,
    pitch: usize,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

// SAFETY: The framebuffer is a process-global resource accessed from a
// single-threaded boot context. No other code touches it.
unsafe impl Send for FbWriter {}
unsafe impl Sync for FbWriter {}

impl FbWriter {
    /// Construct a writer from a Limine `Framebuffer`. Only `bpp == 32`
    /// packed-RGB framebuffers are handled; anything else returns `None`
    /// so the kernel can fall back to a halt-with-no-output rather than
    /// corrupting unrelated memory.
    ///
    /// # Safety
    /// The caller must guarantee that `fb.address` points at a writable,
    /// linearly-mapped framebuffer of at least `pitch * height` bytes and
    /// that no other code accesses it concurrently.
    pub unsafe fn from_limine(fb: &Framebuffer) -> Option<Self> {
        if fb.bpp != 32 {
            return None;
        }
        Some(Self {
            base: fb.address,
            width: fb.width as usize,
            height: fb.height as usize,
            pitch: fb.pitch as usize,
            red_shift: fb.red_mask_shift,
            green_shift: fb.green_mask_shift,
            blue_shift: fb.blue_mask_shift,
        })
    }

    fn pack(&self, c: Rgb) -> u32 {
        (c.0 as u32) << self.red_shift
            | (c.1 as u32) << self.green_shift
            | (c.2 as u32) << self.blue_shift
    }

    fn put_pixel(&mut self, x: usize, y: usize, color: u32) {
        if x >= self.width || y >= self.height {
            return;
        }
        let offset = y * self.pitch + x * 4;
        // SAFETY: `offset` is bounded by the size check above. The framebuffer
        // is mapped writable by Limine; writes are 32-bit aligned because
        // `x * 4` and `pitch` (in bytes) are both multiples of 4 on every
        // framebuffer Limine produces.
        unsafe {
            core::ptr::write_volatile(self.base.add(offset) as *mut u32, color);
        }
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    /// Fill the entire framebuffer with a single colour.
    pub fn clear(&mut self, color: Rgb) {
        let packed = self.pack(color);
        for y in 0..self.height {
            for x in 0..self.width {
                self.put_pixel(x, y, packed);
            }
        }
    }

    /// Fill an axis-aligned rectangle. Clamped to the framebuffer bounds.
    pub fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: Rgb) {
        let packed = self.pack(color);
        let x_end = (x + w).min(self.width);
        let y_end = (y + h).min(self.height);
        for yy in y..y_end {
            for xx in x..x_end {
                self.put_pixel(xx, yy, packed);
            }
        }
    }

    /// Draw a string at an absolute pixel position, scaled by `scale`
    /// (each glyph pixel becomes a `scale`×`scale` block). Unlit pixels
    /// are *not* drawn — the caller's existing background shows through.
    /// Glyphs that walk off the right edge are simply clipped.
    pub fn draw_text_at(
        &mut self,
        x: usize,
        y: usize,
        s: &[u8],
        fg: Rgb,
        scale: usize,
    ) {
        let fg_p = self.pack(fg);
        let cell_w = GLYPH_W * scale;
        for (i, &ch) in s.iter().enumerate() {
            let glyph = if (ch as usize) < FONT_8X16.len() {
                FONT_8X16[ch as usize]
            } else {
                [0u8; GLYPH_H]
            };
            let cell_x = x + i * cell_w;
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..GLYPH_W {
                    let mask = 1u8 << (7 - col);
                    if bits & mask == 0 {
                        continue;
                    }
                    let px0 = cell_x + col * scale;
                    let py0 = y + row * scale;
                    for dy in 0..scale {
                        for dx in 0..scale {
                            self.put_pixel(px0 + dx, py0 + dy, fg_p);
                        }
                    }
                }
            }
        }
    }

    /// Width of a scaled string in pixels — useful for centring.
    pub fn text_width(s: &[u8], scale: usize) -> usize {
        s.len() * GLYPH_W * scale
    }

    /// Height of one scaled glyph row in pixels.
    pub fn text_height(scale: usize) -> usize {
        GLYPH_H * scale
    }

}
