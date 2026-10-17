//! Pixel formats, described by channel masks rather than assumed.
//!
//! Limine reports the channel layout per channel and firmware does not always choose the same
//! one; a hardcoded `0x00RRGGBB` would be channel-swapped on hardware that reports BGR, which a
//! self-hash cannot see (`docs/design/display-substrate.md` §8c).

/// One colour channel's position within a pixel word.
///
/// `shift` is the bit offset of the channel's least significant bit; `size` its width in bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Channel {
    /// Bit offset of the channel's least significant bit.
    pub shift: u8,
    /// Channel width in bits.
    pub size: u8,
}

impl Channel {
    /// A channel `size` bits wide starting at bit `shift`.
    pub const fn new(shift: u8, size: u8) -> Self {
        Self { shift, size }
    }

    /// Place an 8-bit intensity into this channel's bits; narrower channels take the high bits, so
    /// `0xFF` stays full.
    const fn encode(&self, value: u8) -> u32 {
        if self.size == 0 {
            return 0;
        }
        let narrowed = if self.size >= 8 { value as u32 } else { (value >> (8 - self.size)) as u32 };
        (narrowed & ((1u32 << self.size) - 1)) << self.shift
    }

    /// Recover an 8-bit intensity; widening replicates the high bits so `0xFF` round-trips.
    const fn decode(&self, pixel: u32) -> u8 {
        if self.size == 0 {
            return 0;
        }
        let raw = (pixel >> self.shift) & ((1u32 << self.size) - 1);
        if self.size >= 8 {
            (raw & 0xFF) as u8
        } else {
            // Replicate high bits downwards: 0b10110 (5-bit) → 0b10110101.
            let shifted = (raw << (8 - self.size)) as u8;
            shifted | (shifted >> self.size)
        }
    }
}

/// An 8-bit-per-channel colour, independent of any framebuffer's layout.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rgb {
    /// Red intensity.
    pub r: u8,
    /// Green intensity.
    pub g: u8,
    /// Blue intensity.
    pub b: u8,
}

impl Rgb {
    /// A colour from its three channels.
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Black.
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);

    /// `self` moved `delta` units toward white or black, per channel, clamped at both ends. The
    /// whole of the theme's bevel; per channel, since the difference from a perceptual space is
    /// invisible at these amplitudes.
    pub const fn shade(self, delta: i16) -> Rgb {
        const fn one(c: u8, d: i16) -> u8 {
            let v = c as i16 + d;
            if v < 0 {
                0
            } else if v > 255 {
                255
            } else {
                v as u8
            }
        }
        Rgb::new(one(self.r, delta), one(self.g, delta), one(self.b, delta))
    }

    /// `self` composited over `under` at `coverage`, where 0 is invisible and 255 is opaque.
    ///
    /// Coverage is an argument because a stored alpha channel is only one thing that can supply
    /// it: a rasterised glyph passes the coverage it computed.
    ///
    /// Rounded to nearest: `+ 127` before the divide (255 is odd, so no exact tie). Truncation
    /// keeps the endpoints but disagrees on 48.7% of the `(src, under, coverage)` space, always
    /// downwards, which thins every antialiased glyph edge.
    pub const fn blend(self, under: Rgb, coverage: u8) -> Rgb {
        const fn mix(src: u8, dst: u8, a: u8) -> u8 {
            let (src, dst, a) = (src as u32, dst as u32, a as u32);
            ((src * a + dst * (255 - a) + 127) / 255) as u8
        }
        Rgb::new(mix(self.r, under.r, coverage), mix(self.g, under.g, coverage), mix(self.b, under.b, coverage))
    }
}

/// How pixels are laid out in a framebuffer or surface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PixelFormat {
    /// Bits per pixel. Only 32 is supported; see [`PixelFormat::from_limine`].
    pub bits_per_pixel: u8,
    /// The red channel's position.
    pub red: Channel,
    /// The green channel's position.
    pub green: Channel,
    /// The blue channel's position.
    pub blue: Channel,
    /// The alpha channel's position, or `None` for a format whose pixels are all opaque. `None`
    /// is "the question does not arise", not "alpha zero": readers use
    /// [`PixelFormat::alpha_of`], which answers 255.
    pub alpha: Option<Channel>,
}

impl PixelFormat {
    /// `0x00RRGGBB` — the layout QEMU's virtual framebuffer reports.
    pub const XRGB8888: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(16, 8),
        green: Channel::new(8, 8),
        blue: Channel::new(0, 8),
        alpha: None,
    };

    /// `0xAARRGGBB` — [`XRGB8888`](PixelFormat::XRGB8888)'s unused byte, given a meaning.
    ///
    /// Opt-in: it composites per pixel, where `XRGB8888` is copied a row at a time. The
    /// framebuffer itself is never this format.
    pub const ARGB8888: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(16, 8),
        green: Channel::new(8, 8),
        blue: Channel::new(0, 8),
        alpha: Some(Channel::new(24, 8)),
    };

    /// `0x00BBGGRR` — the channel-swapped layout, which some firmware reports.
    pub const XBGR8888: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(0, 8),
        green: Channel::new(8, 8),
        blue: Channel::new(16, 8),
        alpha: None,
    };

    /// Build a format from Limine's framebuffer response fields; `None` for any depth other than
    /// 32, a boot-time diagnosis rather than a visual artefact.
    pub const fn from_limine(
        bits_per_pixel: u16,
        red_mask_shift: u8,
        red_mask_size: u8,
        green_mask_shift: u8,
        green_mask_size: u8,
        blue_mask_shift: u8,
        blue_mask_size: u8,
    ) -> Option<PixelFormat> {
        if bits_per_pixel != 32 {
            return None;
        }
        Some(PixelFormat {
            bits_per_pixel: 32,
            red: Channel::new(red_mask_shift, red_mask_size),
            green: Channel::new(green_mask_shift, green_mask_size),
            blue: Channel::new(blue_mask_shift, blue_mask_size),
            // A scanout target has nothing behind it, so the fourth byte is never an alpha channel.
            alpha: None,
        })
    }

    /// Bytes occupied by one pixel.
    pub const fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel as usize) / 8
    }

    /// Pack a colour into this format's pixel word, fully opaque.
    ///
    /// Bits belonging to no channel are zero, so the hash is stable (§7). An alpha channel is
    /// filled with 255: `Rgb` carries no opacity, and zero would paint invisibly on an
    /// [`ARGB8888`](PixelFormat::ARGB8888) surface. See [`encode_alpha`](Self::encode_alpha).
    pub const fn encode(&self, c: Rgb) -> u32 {
        self.encode_alpha(c, 255)
    }

    /// Pack a colour and an opacity; `alpha` is discarded by a format with no alpha channel.
    pub const fn encode_alpha(&self, c: Rgb, alpha: u8) -> u32 {
        let rgb = self.red.encode(c.r) | self.green.encode(c.g) | self.blue.encode(c.b);
        match self.alpha {
            Some(a) => rgb | a.encode(alpha),
            None => rgb,
        }
    }

    /// This format's opacity for `pixel`: 255 with no alpha channel, as "no channel" and "opaque"
    /// answer a compositor identically.
    pub const fn alpha_of(&self, pixel: u32) -> u8 {
        match self.alpha {
            Some(a) => a.decode(pixel),
            None => 255,
        }
    }

    /// Whether pixels in this format carry their own opacity, which decides copying a row versus
    /// blending a pixel and whether [`compose_exposed`](crate::compose::compose_exposed) may trust
    /// a surface to hide what is under it.
    pub const fn has_alpha(&self) -> bool {
        self.alpha.is_some()
    }

    /// Unpack a colour from this format's pixel word.
    pub const fn decode(&self, pixel: u32) -> Rgb {
        Rgb { r: self.red.decode(pixel), g: self.green.decode(pixel), b: self.blue.decode(pixel) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xrgb_and_xbgr_pack_the_same_colour_to_different_words() {
        let c = Rgb::new(0x12, 0x34, 0x56);
        assert_eq!(PixelFormat::XRGB8888.encode(c), 0x0012_3456);
        assert_eq!(PixelFormat::XBGR8888.encode(c), 0x0056_3412);
    }

    #[test]
    fn encoding_round_trips_through_decoding() {
        for fmt in [PixelFormat::XRGB8888, PixelFormat::XBGR8888] {
            for c in [Rgb::new(0, 0, 0), Rgb::new(255, 255, 255), Rgb::new(1, 128, 254), Rgb::new(0x12, 0x34, 0x56)] {
                assert_eq!(fmt.decode(fmt.encode(c)), c, "{fmt:?} {c:?}");
            }
        }
    }

    #[test]
    fn unused_bits_are_zero_so_the_hash_is_stable() {
        let word = PixelFormat::XRGB8888.encode(Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(word & 0xFF00_0000, 0, "the unused byte must be zero");
        assert_eq!(word, 0x00FF_FFFF);
    }

    #[test]
    fn narrow_channels_keep_full_intensity_across_a_round_trip() {
        let rgb565 = PixelFormat {
            bits_per_pixel: 32,
            red: Channel::new(11, 5),
            green: Channel::new(5, 6),
            blue: Channel::new(0, 5),
            alpha: None,
        };
        let white = Rgb::new(255, 255, 255);
        assert_eq!(rgb565.decode(rgb565.encode(white)), white);
        assert_eq!(rgb565.decode(rgb565.encode(Rgb::BLACK)), Rgb::BLACK);
    }

    #[test]
    fn from_limine_accepts_32bpp_and_refuses_everything_else() {
        let fmt = PixelFormat::from_limine(32, 16, 8, 8, 8, 0, 8).expect("32bpp is supported");
        assert_eq!(fmt, PixelFormat::XRGB8888);
        let swapped = PixelFormat::from_limine(32, 0, 8, 8, 8, 16, 8).expect("32bpp is supported");
        assert_eq!(swapped, PixelFormat::XBGR8888);
        assert_ne!(swapped, fmt, "channel order must survive the hand-off");

        assert!(PixelFormat::from_limine(24, 16, 8, 8, 8, 0, 8).is_none());
        assert!(PixelFormat::from_limine(16, 11, 5, 5, 6, 0, 5).is_none());
    }

    #[test]
    fn bytes_per_pixel_follows_the_depth() {
        assert_eq!(PixelFormat::XRGB8888.bytes_per_pixel(), 4);
    }

    #[test]
    fn the_blend_endpoints_are_exact() {
        // 255 must return the source exactly and zero the destination, else a glyph's bounding box
        // tints the background. Both hold under truncation too.
        let src = Rgb::new(0xE0, 0x37, 0x91);
        let dst = Rgb::new(0x0E, 0x14, 0x1B);
        assert_eq!(src.blend(dst, 255), src, "full coverage is not the source");
        assert_eq!(src.blend(dst, 0), dst, "zero coverage disturbed the destination");
    }

    #[test]
    fn rounding_is_to_nearest_not_downwards() {
        // Deleting `+ 127` leaves the other tests green; a faint source over black separates the
        // rules: 200/255 of one level rounds to 1 and truncates to 0.
        assert_eq!(Rgb::new(1, 1, 1).blend(Rgb::BLACK, 200), Rgb::new(1, 1, 1));

        let mut lower = 0usize;
        for v in 0..=255u8 {
            for a in 0..=255u8 {
                let rounded = Rgb::new(v, 0, 0).blend(Rgb::BLACK, a).r as u32;
                let truncated = (v as u32 * a as u32) / 255;
                assert!(truncated <= rounded, "truncation exceeded rounding at v={v} a={a}");
                if truncated < rounded {
                    lower += 1;
                }
            }
        }
        assert!(lower > 20_000, "only {lower} of 65536 differ — is the rounding still there?");
    }

    #[test]
    fn every_channel_value_survives_full_coverage() {
        for v in 0..=255u8 {
            let c = Rgb::new(v, v, v);
            assert_eq!(c.blend(Rgb::BLACK, 255), c, "{v} did not survive");
            assert_eq!(Rgb::BLACK.blend(c, 0), c, "{v} was disturbed at zero coverage");
        }
    }

    #[test]
    fn half_coverage_lands_between_the_two() {
        // Returning either endpoint, or outside the interval, is invisible to the endpoint tests.
        let out = Rgb::new(200, 100, 0).blend(Rgb::new(0, 200, 40), 128);
        for (o, (a, b)) in [(out.r, (0u8, 200u8)), (out.g, (100, 200)), (out.b, (0, 40))] {
            assert!(o > a.min(b) && o < a.max(b), "{o} is not strictly between {a} and {b}");
        }
    }

    #[test]
    fn blending_is_monotonic_in_coverage() {
        // A sign slip or operand swap in `mix` passes both endpoint tests and fails this.
        let (src, dst) = (Rgb::new(255, 255, 255), Rgb::BLACK);
        let mut last = 0u8;
        for a in 0..=255u8 {
            let v = src.blend(dst, a).r;
            assert!(v >= last, "coverage {a} went backwards: {v} after {last}");
            last = v;
        }
        assert_eq!(last, 255);
    }

    #[test]
    fn the_operands_are_not_interchangeable() {
        // `blend` reads like a symmetric mix and is not; swapped operands give a wrong picture.
        let a = Rgb::new(255, 0, 0);
        let b = Rgb::new(0, 0, 255);
        assert_ne!(a.blend(b, 64), b.blend(a, 64));
        assert_eq!(a.blend(b, 64), b.blend(a, 255 - 64), "…and they are each other's mirror");
    }

    #[test]
    fn encoding_a_colour_into_an_alpha_format_makes_it_opaque() {
        // `Rgb` has no opacity: leaving alpha at zero would paint invisibly on ARGB8888.
        let c = Rgb::new(0x12, 0x34, 0x56);
        assert_eq!(PixelFormat::ARGB8888.encode(c), 0xFF12_3456);
        assert_eq!(PixelFormat::ARGB8888.alpha_of(PixelFormat::ARGB8888.encode(c)), 255);
        assert_eq!(PixelFormat::ARGB8888.encode(c) & 0x00FF_FFFF, PixelFormat::XRGB8888.encode(c));
    }

    #[test]
    fn a_format_with_no_alpha_channel_reads_as_fully_opaque() {
        for word in [0x0000_0000, 0x0012_3456, 0xFFFF_FFFF, 0x0000_00FF] {
            assert_eq!(PixelFormat::XRGB8888.alpha_of(word), 255);
            assert_eq!(PixelFormat::XBGR8888.alpha_of(word), 255);
        }
        assert!(!PixelFormat::XRGB8888.has_alpha());
        assert!(PixelFormat::ARGB8888.has_alpha());
    }

    #[test]
    fn encode_alpha_round_trips_through_decode_and_alpha_of() {
        for a in [0u8, 1, 64, 128, 254, 255] {
            let c = Rgb::new(0xDE, 0xAD, 0xBE);
            let w = PixelFormat::ARGB8888.encode_alpha(c, a);
            assert_eq!(PixelFormat::ARGB8888.decode(w), c);
            assert_eq!(PixelFormat::ARGB8888.alpha_of(w), a);
        }
        assert_eq!(
            PixelFormat::XRGB8888.encode_alpha(Rgb::new(1, 2, 3), 7),
            PixelFormat::XRGB8888.encode(Rgb::new(1, 2, 3))
        );
    }

    #[test]
    fn a_framebuffer_from_limine_never_has_an_alpha_channel() {
        let f = PixelFormat::from_limine(32, 16, 8, 8, 8, 0, 8).expect("32bpp is supported");
        assert!(!f.has_alpha());
        assert_eq!(f, PixelFormat::XRGB8888);
    }
}
