//! Pixel formats, described by channel masks rather than assumed.
//!
//! **Data-driven, on purpose.** Limine reports the framebuffer's channel layout as a
//! size and shift per channel, and firmware does not always choose the same one. The
//! display-arm plan names this as a risk in as many words: "Keep format handling
//! data-driven from Limine's report rather than hardcoding a layout that happens to
//! work". A hardcoded `0x00RRGGBB` would run correctly under QEMU and produce
//! channel-swapped output on hardware that reports BGR — a bug that is invisible to a
//! self-hash, because the guest is perfectly consistent with itself
//! (`docs/design/display-substrate.md` §8c).

/// One colour channel's position within a pixel word.
///
/// `shift` is the bit offset of the channel's least significant bit; `size` is its
/// width in bits. Both come straight from Limine's framebuffer response.
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

    /// Place an 8-bit intensity into this channel's bits.
    ///
    /// Narrower channels take the *high* bits of the input, which is the standard
    /// truncation: 8-bit `0xFF` must stay full intensity in a 5-bit channel.
    const fn encode(&self, value: u8) -> u32 {
        if self.size == 0 {
            return 0;
        }
        let narrowed = if self.size >= 8 { value as u32 } else { (value >> (8 - self.size)) as u32 };
        (narrowed & ((1u32 << self.size) - 1)) << self.shift
    }

    /// Recover an 8-bit intensity from this channel's bits.
    ///
    /// Widening replicates the high bits so that a full-intensity narrow channel
    /// round-trips to `0xFF` rather than to `0xF8`.
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

    /// `self` moved `delta` units toward white or black, per channel, clamped at both ends.
    ///
    /// **The whole of the theme's bevel** (M11 Part E, batch 2): a gradient here is one colour
    /// lightened at the top and darkened at the bottom by the same amount, so a face needs one
    /// value rather than two, and a new palette needs one number rather than one pair per
    /// gradient. Measured against the reference desktop, whose title bar spans ±10 around its
    /// midpoint and whose menu selection spans ±14 — close enough that a single amount is the
    /// honest model rather than a simplification of one.
    ///
    /// Per channel and not in a perceptual space, deliberately: the difference at these
    /// amplitudes is invisible, and a colour space is a dependency and a decision.
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
    /// **There is no alpha channel anywhere in this crate**, and this is not the beginning of
    /// one. Surfaces are opaque XRGB8888 and compositing is a copy; what needs blending is a
    /// *source* that arrives with per-pixel coverage — a rasterised glyph — against a
    /// destination already in the buffer. Coverage is an argument, not a stored channel, so
    /// nothing gains a fourth byte and `compose` stays a memcpy.
    ///
    /// **Rounded rather than truncated**, and the reason is not the endpoints. `+ 127` before
    /// the divide makes this round-to-nearest (255 is odd, so no value is ever an exact tie).
    /// The endpoints survive truncation too — at full coverage the numerator is `src * 255`
    /// and at zero it is `under * 255`, both exactly divisible — so an earlier version of this
    /// comment justified the constant with a property that does not need it.
    ///
    /// What it actually buys is every value *between*: rounding and truncation disagree on
    /// **48.7%** of the `(src, under, coverage)` space, and truncation always disagrees
    /// downwards. Dropping it darkens every partially-covered pixel by up to one level, which
    /// on antialiased text is a uniform thinning of every glyph edge — the kind of wrong that
    /// looks like a font-weight problem rather than an arithmetic one.
    pub const fn blend(self, under: Rgb, coverage: u8) -> Rgb {
        const fn mix(src: u8, dst: u8, a: u8) -> u8 {
            let (src, dst, a) = (src as u32, dst as u32, a as u32);
            ((src * a + dst * (255 - a) + 127) / 255) as u8
        }
        Rgb::new(
            mix(self.r, under.r, coverage),
            mix(self.g, under.g, coverage),
            mix(self.b, under.b, coverage),
        )
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
}

impl PixelFormat {
    /// `0x00RRGGBB` — the layout QEMU's virtual framebuffer reports.
    pub const XRGB8888: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(16, 8),
        green: Channel::new(8, 8),
        blue: Channel::new(0, 8),
    };

    /// `0x00BBGGRR` — the channel-swapped layout, which some firmware reports.
    pub const XBGR8888: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(0, 8),
        green: Channel::new(8, 8),
        blue: Channel::new(16, 8),
    };

    /// Build a format from Limine's framebuffer response fields.
    ///
    /// Returns `None` for any depth other than 32. The kernel's own `FbWriter` takes
    /// the same position, and refusing loudly is better than rendering garbage into a
    /// 16- or 24-bit buffer: an unsupported depth is a boot-time diagnosis, not a
    /// visual artefact to puzzle over later.
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
        })
    }

    /// Bytes occupied by one pixel.
    pub const fn bytes_per_pixel(&self) -> usize {
        (self.bits_per_pixel as usize) / 8
    }

    /// Pack a colour into this format's pixel word.
    ///
    /// Bits belonging to no channel are left **zero**. That matters for the gate: a
    /// pixel whose unused byte carried leftover data would hash differently run to
    /// run, and §7 requires compositing to be deterministic to the byte.
    pub const fn encode(&self, c: Rgb) -> u32 {
        self.red.encode(c.r) | self.green.encode(c.g) | self.blue.encode(c.b)
    }

    /// Unpack a colour from this format's pixel word.
    pub const fn decode(&self, pixel: u32) -> Rgb {
        Rgb {
            r: self.red.decode(pixel),
            g: self.green.decode(pixel),
            b: self.blue.decode(pixel),
        }
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
            for c in [
                Rgb::new(0, 0, 0),
                Rgb::new(255, 255, 255),
                Rgb::new(1, 128, 254),
                Rgb::new(0x12, 0x34, 0x56),
            ] {
                assert_eq!(fmt.decode(fmt.encode(c)), c, "{fmt:?} {c:?}");
            }
        }
    }

    #[test]
    fn unused_bits_are_zero_so_the_hash_is_stable() {
        // §7: no uninitialised or leftover bits in rendered output.
        let word = PixelFormat::XRGB8888.encode(Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(word & 0xFF00_0000, 0, "the unused byte must be zero");
        assert_eq!(word, 0x00FF_FFFF);
    }

    #[test]
    fn narrow_channels_keep_full_intensity_across_a_round_trip() {
        // A 5-6-5 layout: full white must survive rather than decaying to 0xF8.
        let rgb565 = PixelFormat {
            bits_per_pixel: 32,
            red: Channel::new(11, 5),
            green: Channel::new(5, 6),
            blue: Channel::new(0, 5),
        };
        let white = Rgb::new(255, 255, 255);
        assert_eq!(rgb565.decode(rgb565.encode(white)), white);
        assert_eq!(rgb565.decode(rgb565.encode(Rgb::BLACK)), Rgb::BLACK);
    }

    #[test]
    fn from_limine_accepts_32bpp_and_refuses_everything_else() {
        // QEMU's report: 32bpp, r@16 g@8 b@0, all 8 bits wide.
        let fmt = PixelFormat::from_limine(32, 16, 8, 8, 8, 0, 8).expect("32bpp is supported");
        assert_eq!(fmt, PixelFormat::XRGB8888);
        // A swapped report must produce a swapped format, not the default.
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
        // A glyph's interior is full coverage, so if 255 does not return the source exactly,
        // no text is ever quite its own colour; zero coverage must likewise return the
        // destination untouched, or a glyph's bounding box tints the background it does not
        // cover.
        //
        // **This does not test the rounding**, which an earlier version of this comment
        // claimed it did — see `rounding_is_to_nearest_not_downwards` below. Both endpoints
        // hold under truncation as well.
        let src = Rgb::new(0xE0, 0x37, 0x91);
        let dst = Rgb::new(0x0E, 0x14, 0x1B);
        assert_eq!(src.blend(dst, 255), src, "full coverage is not the source");
        assert_eq!(src.blend(dst, 0), dst, "zero coverage disturbed the destination");
    }

    #[test]
    fn rounding_is_to_nearest_not_downwards() {
        // **The test the `+ 127` did not have.** Deleting that constant left every other test
        // in this module green, because the endpoints survive truncation and the properties
        // below (monotonic, strictly between, mirrored) hold for both rules. So the one piece
        // of arithmetic a reader is most likely to read as a magic number had nothing
        // asserting it, and its stated justification was checkable and false.
        //
        // A faint source over black is where the two rules separate: 200/255 of one level
        // rounds to 1 and truncates to 0 — the difference between a visible glyph edge and a
        // missing one.
        assert_eq!(Rgb::new(1, 1, 1).blend(Rgb::BLACK, 200), Rgb::new(1, 1, 1));

        // And in the large: truncation is never *higher* than rounding, and is strictly lower
        // often enough that "they mostly agree" is not a defence.
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
        // The endpoint above with one colour proves one colour. Rounding errors are
        // value-dependent, so check the whole range rather than a sample.
        for v in 0..=255u8 {
            let c = Rgb::new(v, v, v);
            assert_eq!(c.blend(Rgb::BLACK, 255), c, "{v} did not survive");
            assert_eq!(Rgb::BLACK.blend(c, 0), c, "{v} was disturbed at zero coverage");
        }
    }

    #[test]
    fn half_coverage_lands_between_the_two() {
        // Not asserting an exact midpoint — the rounding rule is what it is — but a blend that
        // returned either endpoint, or something outside the interval, would be wrong in a way
        // no endpoint test can see.
        let out = Rgb::new(200, 100, 0).blend(Rgb::new(0, 200, 40), 128);
        for (o, (a, b)) in [(out.r, (0u8, 200u8)), (out.g, (100, 200)), (out.b, (0, 40))] {
            assert!(o > a.min(b) && o < a.max(b), "{o} is not strictly between {a} and {b}");
        }
    }

    #[test]
    fn blending_is_monotonic_in_coverage() {
        // More coverage never moves a channel away from the source. A sign slip or an operand
        // swap in `mix` passes both endpoint tests above and fails this.
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
        // `src.blend(dst, a)` is not `dst.blend(src, a)`. Stated as a test because the call
        // reads like a symmetric mix and is not, and swapping them at a call site produces a
        // picture that is subtly, uniformly wrong rather than obviously broken.
        let a = Rgb::new(255, 0, 0);
        let b = Rgb::new(0, 0, 255);
        assert_ne!(a.blend(b, 64), b.blend(a, 64));
        assert_eq!(a.blend(b, 64), b.blend(a, 255 - 64), "…and they are each other's mirror");
    }
}
