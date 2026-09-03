//! Decoding PNG, so that a wallpaper is a file a person supplies.
//!
//! **The question upstream of the format was whether a wallpaper is a shipped asset or a file
//! somebody drops in their home directory** (M12 decision 2). Shipped assets would have let the
//! decode happen on the *host* at build time and the guest read something trivial — QOI at ~300
//! lines, or raw P6 at ~40 and 3 MB of disk. Both were costed and both were cheaper. The answer
//! is that a wallpaper a person cannot supply is not really a wallpaper, so the guest decodes
//! what people actually have.
//!
//! ## What this decodes, and what it refuses
//!
//! - **Bit depth 8**, every colour type: greyscale, RGB, palette, greyscale+alpha, RGBA.
//! - **No interlacing.** Adam7 is a second pass structure over the same filtering, and a
//!   progressive wallpaper is a picture that appears slightly sooner exactly once. Refused by
//!   name rather than misread.
//! - **Bit depths 1, 2, 4 and 16 are refused**, also by name. Sub-byte depths are a bit-unpacker
//!   and 16 is a byte-order decision; neither is what a photograph is stored as.
//!
//! A refusal is a [`PngError`] naming the cause, because the caller is `desktop-shell` and what
//! it does with one is put a sentence on the console beside a desktop that fell back to its
//! ground colour. "The image did not load" would be the same answer for a missing file, an
//! interlaced picture and a truncated download.
//!
//! ## Inflate is `miniz_oxide`
//!
//! Decided 2026-09-02, after building it for `x86_64-unknown-nitrox` — which is what
//! `userspace/CLAUDE.md` requires before any dependency is taken, and which the plan named as
//! the thing that settles this. Its whole transitive tree is `adler2`, both licences are
//! permissive, and `decompress_to_vec_zlib` hands back exactly the shape [`unfilter`] wants.
//! That last part is the clause that decided it — the same one that carried `ab_glyph` over
//! `fontdue`: a dependency you have to work around is worse than code you own, and this one
//! needs no working around.
//!
//! The alternative was ~500 lines of RFC 1951 we would own and could reuse for a package format
//! later. It remains the thing to reach for if this dependency ever stops fitting.

use alloc::vec::Vec;

use crate::format::PixelFormat;
use crate::framebuffer::Geometry;

/// The eight bytes every PNG begins with.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// **Chunk lengths are added to offsets without a checked add**, which is sound exactly while a
// `u32` length cannot overflow a `usize` offset. Stated here rather than assumed: this crate
// builds for the host as well as for `x86_64-unknown-nitrox`, and a 32-bit host would make the
// arithmetic in `decode` wrap on a file claiming a 4 GiB chunk.
const _: () = assert!(usize::BITS >= 64, "PNG chunk arithmetic assumes a 64-bit usize");

/// What a decode may allocate at its peak, in bytes.
///
/// **Chosen against the machine, which is the thing the cap has to be about.** The guest boots
/// with 256 MiB; 32 is a comfortable twelfth of it, admits every wallpaper up to 2560x1600, and
/// refuses 4K — which will be somebody's picture one day, and will then be a *refusal* rather
/// than a dead session (see [`PngError::OutOfMemory`]).
pub const MAX_DECODE_BYTES: usize = 32 * 1024 * 1024;

/// Bytes held per pixel at the peak of a decode, which is what [`MAX_PIXELS`] divides.
///
/// Two buffers are alive at once, twice: `unfilter` holds the inflated stream and its own output
/// together, then `to_xrgb` holds that output and the XRGB8888 result. RGBA is the worst case at
/// four bytes a sample, so both moments are `4 + 4`. `decode` drops the inflated stream between
/// them so the two peaks do not add.
const PEAK_BYTES_PER_PIXEL: usize = 8;

/// The largest image this will decode, in pixels.
///
/// **A bound on `width * height`, not on either alone**, because what has to be allocated is
/// their product — and **derived from [`MAX_DECODE_BYTES`] rather than picked**, which is what
/// makes the refusal below mean what it says.
///
/// It was 64 megapixels, and that number refused nothing that mattered: an all-zero 8192x8192
/// RGB PNG compresses to about 190 KiB, passes `libfs`'s 8 MiB read cap with three orders of
/// magnitude to spare, passes a 64-megapixel test *exactly*, and then asks for well over half a
/// gigabyte — on a 256 MiB machine, where `libheap` returns null, `handle_alloc_error` fires and
/// `desktop-shell`'s panic handler exits the graphical session. A cap ten times the machine's
/// memory is a cap that only refuses arithmetic overflow (PR #272 review, worth fixing 2).
pub const MAX_PIXELS: u64 = (MAX_DECODE_BYTES / PEAK_BYTES_PER_PIXEL) as u64;

/// Why a PNG did not decode.
///
/// **Named causes rather than one failure**, for the reason this module's header gives: the
/// caller turns these into a console line, and a desktop that fell back to its ground colour
/// should say which of these happened.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PngError {
    /// Not a PNG at all — the signature is wrong or the file is shorter than one.
    NotPng,
    /// A chunk ran past the end of the file, or the header is incomplete.
    Truncated,
    /// The bit depth is not 8. The payload is what was asked for.
    BitDepth(u8),
    /// The colour type is not one of the five defined ones. The payload is what was asked for.
    ColourType(u8),
    /// Adam7 interlacing, which this does not implement.
    Interlaced,
    /// A palette image whose `PLTE` is missing, short, or names an index it does not hold.
    Palette,
    /// `width * height` is zero or over [`MAX_PIXELS`].
    Size,
    /// An allocation the decode needed could not be made.
    ///
    /// **A refusal rather than a panic**, which is the whole reason the buffers below are
    /// reserved fallibly: `libheap` returns null on exhaustion, `handle_alloc_error` fires, and
    /// the caller's panic handler ends the process — so an infallible `vec![0; n]` here makes a
    /// too-large picture kill the graphical session instead of leaving it a ground colour and a
    /// sentence. [`MAX_PIXELS`] catches the ordinary case; this catches the one where the
    /// machine is already short.
    OutOfMemory,
    /// The zlib stream did not inflate.
    Inflate,
    /// The inflated data is not the size the header implies — too short, or a filter byte names
    /// a method that does not exist.
    Filter,
}

impl PngError {
    /// A sentence for a log line. Borrowed and `'static`, so a caller with no allocator can
    /// print one.
    pub fn why(&self) -> &'static str {
        match self {
            PngError::NotPng => "is not a PNG",
            PngError::Truncated => "is truncated",
            PngError::BitDepth(_) => "is not 8 bits per channel",
            PngError::ColourType(_) => "has a colour type this does not decode",
            PngError::Interlaced => "is interlaced, which this does not decode",
            PngError::Palette => "has a broken palette",
            PngError::Size => "is empty, or larger than this will decode",
            PngError::OutOfMemory => "did not fit in memory",
            PngError::Inflate => "did not inflate",
            PngError::Filter => "has a filter this does not decode",
        }
    }
}

/// A decoded image: XRGB8888 pixels and the geometry describing them.
///
/// **The same pair every other surface in this crate is**, so a decoded picture goes into
/// [`box_downscale`](crate::scale::box_downscale) or a blit without a conversion step. The
/// alpha channel is *dropped* rather than kept, and the reason is the picture rather than the
/// substrate: `libdraw` blends (since M5 Part A) and surfaces can carry alpha (since M13 Part B),
/// but a wallpaper is drawn on nothing, so there is nothing for
/// it to blend with. Keeping a channel no code reads would be a promise this cannot honour.
#[derive(Debug, PartialEq, Eq)]
pub struct Image {
    /// XRGB8888, `geometry.pitch` bytes per row.
    pub pixels: Vec<u8>,
    /// Its size and stride. `pitch` is `width * 4` exactly — a decoded image has no padding.
    pub geometry: Geometry,
}

impl Image {
    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.geometry.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.geometry.height
    }
}

/// What a PNG's `IHDR` says about it.
struct Header {
    width: u32,
    height: u32,
    colour: u8,
    /// Bytes per pixel in the *unfiltered* data — what the filters operate on.
    bpp: usize,
}

/// Decode `bytes` as a PNG.
pub fn decode(bytes: &[u8]) -> Result<Image, PngError> {
    if bytes.len() < SIGNATURE.len() || bytes[..SIGNATURE.len()] != SIGNATURE {
        return Err(PngError::NotPng);
    }
    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let mut idat: Vec<u8> = Vec::new();

    // **Chunks are walked rather than assumed to be in a fixed order.** The spec allows ancillary
    // chunks anywhere between `IHDR` and `IEND`, and real encoders put `gAMA`, `pHYs`, `iCCP`
    // and text chunks in whatever order suits them — so a decoder that expected `IHDR, IDAT,
    // IEND` would refuse most photographs.
    let mut at = SIGNATURE.len();
    loop {
        // Length (4), type (4), data, CRC (4).
        if at + 8 > bytes.len() {
            return Err(PngError::Truncated);
        }
        let len = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            as usize;
        let kind = &bytes[at + 4..at + 8];
        let start = at + 8;
        // **Plain arithmetic, and the assertion above is what makes it safe.** `len` comes from
        // four bytes, so it is at most 4 GiB; `at` is bounded by the file. On a 64-bit `usize`
        // the sum cannot wrap, so a `checked_add` here would be a guard that *cannot fire* —
        // which reads as protecting an invariant it does not, the note PR #269's review left on
        // the same shape. The const assertion states the assumption instead, so a 32-bit target
        // breaks the build rather than the decoder.
        let end = start + len;
        if end + 4 > bytes.len() {
            return Err(PngError::Truncated);
        }
        let data = &bytes[start..end];
        match kind {
            b"IHDR" => header = Some(parse_ihdr(data)?),
            b"PLTE" => {
                if data.len() % 3 != 0 {
                    return Err(PngError::Palette);
                }
                palette = data.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
            }
            // **Concatenated, because a PNG's compressed data is one zlib stream split across
            // however many `IDAT`s the encoder felt like.** Inflating each separately is the
            // classic way to decode the first chunk of a large image and nothing else.
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        // The CRC is skipped rather than checked. It guards against a corrupt *transfer*, and
        // every failure it would catch is one the length checks and the inflate already refuse —
        // a wrong CRC with a valid zlib stream is a picture that decodes correctly.
        at = end + 4;
    }

    let h = header.ok_or(PngError::NotPng)?;
    // **Bounded by what the header implies**, which is the honest limit and closes a hole a cap
    // on the *image* cannot: a few hundred bytes of `IDAT` can inflate to gigabytes, and an
    // unbounded `decompress_to_vec` would allocate all of it before anything here saw a byte.
    // One filter byte plus a row of samples, per row, is exactly what `unfilter` reads.
    let expect = h.height as usize * (1 + h.width as usize * h.bpp);
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, expect)
        .map_err(|_| PngError::Inflate)?;
    let unfiltered = unfilter(&raw, &h)?;
    // **Dropped before the XRGB buffer is asked for**, so the two moments where two buffers are
    // alive do not add. That is what makes `PEAK_BYTES_PER_PIXEL` eight rather than twelve, and
    // it is the difference between admitting a 2560x1600 picture and refusing it.
    drop(raw);
    to_xrgb(&unfiltered, &h, &palette)
}

/// `n` zeroed bytes, or [`PngError::OutOfMemory`] — never a panic.
///
/// `vec![0; n]` aborts through `handle_alloc_error` when the allocator returns null, which in
/// this system means the calling *process* exits. A wallpaper that does not fit has to be a
/// sentence on the console, so every buffer sized from a file's header is reserved this way.
fn zeroed(n: usize) -> Result<Vec<u8>, PngError> {
    let mut v: Vec<u8> = Vec::new();
    v.try_reserve_exact(n).map_err(|_| PngError::OutOfMemory)?;
    v.resize(n, 0);
    Ok(v)
}

/// Parse `IHDR`, refusing what this decoder does not implement.
fn parse_ihdr(data: &[u8]) -> Result<Header, PngError> {
    if data.len() < 13 {
        return Err(PngError::Truncated);
    }
    let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let depth = data[8];
    let colour = data[9];
    let interlace = data[12];

    // **Before anything is allocated.** These are four bytes from a file, and the product is
    // what gets multiplied by four and handed to the allocator.
    if width == 0 || height == 0 || width as u64 * height as u64 > MAX_PIXELS {
        return Err(PngError::Size);
    }
    if depth != 8 {
        return Err(PngError::BitDepth(depth));
    }
    if interlace != 0 {
        return Err(PngError::Interlaced);
    }
    // Channels per pixel, which at depth 8 is bytes per pixel — and bytes per pixel is what the
    // filters are defined in terms of, so it is the only form of this number worth keeping.
    let bpp = match colour {
        0 => 1, // greyscale
        2 => 3, // RGB
        3 => 1, // palette index
        4 => 2, // greyscale + alpha
        6 => 4, // RGBA
        other => return Err(PngError::ColourType(other)),
    };
    Ok(Header { width, height, colour, bpp })
}

/// Reverse the per-row filters, dropping the filter byte from each row.
///
/// **The one part of PNG that is genuinely an algorithm**, and the one place a decoder is
/// usually wrong: the four predictors read the pixel to the left, the row above, and the pixel
/// above-left, all of which are *already unfiltered* — so this reads its own output and must
/// treat off-image neighbours as zero rather than skipping them.
fn unfilter(raw: &[u8], h: &Header) -> Result<Vec<u8>, PngError> {
    let stride = h.width as usize * h.bpp;
    let expect = (stride + 1) * h.height as usize;
    if raw.len() < expect {
        return Err(PngError::Filter);
    }
    let mut out = zeroed(stride * h.height as usize)?;
    for y in 0..h.height as usize {
        let filter = raw[y * (stride + 1)];
        let src = &raw[y * (stride + 1) + 1..y * (stride + 1) + 1 + stride];
        for x in 0..stride {
            // The byte `bpp` to the left in this row, or 0 at the start of it.
            let a = if x >= h.bpp { out[y * stride + x - h.bpp] } else { 0 };
            // The byte directly above, or 0 on the first row.
            let b = if y > 0 { out[(y - 1) * stride + x] } else { 0 };
            // Above and to the left; 0 outside the image in either direction.
            let c = if y > 0 && x >= h.bpp { out[(y - 1) * stride + x - h.bpp] } else { 0 };
            let v = match filter {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                // Average: the sum is computed in a *wider* type and halved before it comes
                // back. `(a + b) / 2` in `u8` would overflow for any pair summing past 255,
                // which is most of a photograph.
                3 => src[x].wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => src[x].wrapping_add(paeth(a, b, c)),
                _ => return Err(PngError::Filter),
            };
            out[y * stride + x] = v;
        }
    }
    Ok(out)
}

/// The Paeth predictor: whichever of `a`, `b`, `c` is closest to `a + b - c`.
///
/// **The arithmetic is `i16`, and that is not an optimisation.** `a + b - c` leaves `[0, 255]`
/// in both directions for ordinary pixel values, so computing it in `u8` gives a different
/// answer than the spec's — visible as coloured noise along edges, which is the classic way to
/// get a PNG decoder subtly wrong.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    // The tie-breaking order is the spec's — `a`, then `b`, then `c` — and it matters: an
    // encoder chose its filter assuming this order, so reversing it decodes differently.
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// Turn unfiltered samples into XRGB8888.
fn to_xrgb(data: &[u8], h: &Header, palette: &[[u8; 3]]) -> Result<Image, PngError> {
    let (w, ht) = (h.width as usize, h.height as usize);
    let mut pixels = zeroed(w * ht * 4)?;
    let format = PixelFormat::XRGB8888;
    for i in 0..w * ht {
        let s = i * h.bpp;
        let (r, g, b) = match h.colour {
            0 => (data[s], data[s], data[s]),
            2 => (data[s], data[s + 1], data[s + 2]),
            3 => {
                let e = *palette.get(data[s] as usize).ok_or(PngError::Palette)?;
                (e[0], e[1], e[2])
            }
            // The alpha byte is read past and dropped — see [`Image`] for why the channel is not
            // kept at all.
            4 => (data[s], data[s], data[s]),
            _ => (data[s], data[s + 1], data[s + 2]),
        };
        let word = format.encode(crate::format::Rgb::new(r, g, b));
        pixels[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    let geometry = Geometry::with_pitch(h.width, h.height, w * 4, format).ok_or(PngError::Size)?;
    Ok(Image { pixels, geometry })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a PNG chunk: length, type, data, and a CRC this decoder does not check.
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(data.len() as u32).to_be_bytes());
        v.extend_from_slice(kind);
        v.extend_from_slice(data);
        v.extend_from_slice(&[0, 0, 0, 0]);
        v
    }

    fn ihdr(w: u32, h: u32, depth: u8, colour: u8, interlace: u8) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(&w.to_be_bytes());
        d.extend_from_slice(&h.to_be_bytes());
        d.push(depth);
        d.push(colour);
        d.push(0); // compression
        d.push(0); // filter method
        d.push(interlace);
        chunk(b"IHDR", &d)
    }

    /// A whole PNG: header, `PLTE` if given, the zlib-compressed rows, and `IEND`.
    ///
    /// **The rows are compressed by a real deflate**, not stored raw with a hand-made zlib
    /// wrapper: the point of these tests is the decoder, and a fixture whose compression this
    /// crate also produced would let a decoder that mis-reads the stream pass by symmetry.
    fn png(w: u32, h: u32, colour: u8, rows: &[u8], plte: Option<&[u8]>) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(w, h, 8, colour, 0));
        if let Some(p) = plte {
            v.extend_from_slice(&chunk(b"PLTE", p));
        }
        let z = miniz_oxide::deflate::compress_to_vec_zlib(rows, 6);
        v.extend_from_slice(&chunk(b"IDAT", &z));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        v
    }

    /// The pixel at `(x, y)` as `(r, g, b)`.
    fn px(img: &Image, x: u32, y: u32) -> (u8, u8, u8) {
        let off = y as usize * img.geometry.pitch + x as usize * 4;
        let word = u32::from_le_bytes([
            img.pixels[off],
            img.pixels[off + 1],
            img.pixels[off + 2],
            img.pixels[off + 3],
        ]);
        let c = img.geometry.format.decode(word);
        (c.r, c.g, c.b)
    }

    #[test]
    fn a_two_by_two_rgb_image_decodes() {
        // Filter 0 on both rows: the bytes are the pixels.
        let rows = [
            0, 255, 0, 0, 0, 255, 0, // row 0: red, green
            0, 0, 0, 255, 255, 255, 255, // row 1: blue, white
        ];
        let img = decode(&png(2, 2, 2, &rows, None)).expect("decodes");
        assert_eq!((img.width(), img.height()), (2, 2));
        assert_eq!(px(&img, 0, 0), (255, 0, 0));
        assert_eq!(px(&img, 1, 0), (0, 255, 0));
        assert_eq!(px(&img, 0, 1), (0, 0, 255));
        assert_eq!(px(&img, 1, 1), (255, 255, 255));
    }

    #[test]
    fn the_pitch_has_no_padding_and_the_buffer_is_exactly_that_size() {
        // A decoded image goes straight into a blit or a downscale, both of which read `pitch`.
        // A pitch that did not match the buffer would shear the picture.
        let rows = [0, 1, 2, 3, 4, 5, 6];
        let img = decode(&png(2, 1, 2, &rows, None)).expect("decodes");
        assert_eq!(img.geometry.pitch, 8);
        assert_eq!(img.pixels.len(), 8);
    }

    #[test]
    fn a_sub_filter_reads_the_pixel_to_its_left() {
        // Filter 1 is `left`, and at the start of a row the left neighbour is **zero**, not the
        // last pixel of the row above. A decoder that wrapped would decode every row but the
        // first one wrong, and only for images whose rows do not happen to start dark.
        let rows = [
            1, 10, 20, 30, 5, 5, 5, // row 0: (10,20,30) then +(5,5,5)
        ];
        let img = decode(&png(2, 1, 2, &rows, None)).expect("decodes");
        assert_eq!(px(&img, 0, 0), (10, 20, 30));
        assert_eq!(px(&img, 1, 0), (15, 25, 35));
    }

    #[test]
    fn an_up_filter_reads_the_row_above() {
        let rows = [
            0, 10, 20, 30, // row 0, unfiltered
            2, 1, 2, 3, // row 1: up + (1,2,3)
        ];
        let img = decode(&png(1, 2, 2, &rows, None)).expect("decodes");
        assert_eq!(px(&img, 0, 0), (10, 20, 30));
        assert_eq!(px(&img, 0, 1), (11, 22, 33));
    }

    #[test]
    fn an_average_filter_does_not_overflow_a_byte() {
        // `(a + b) / 2` computed in `u8` wraps for any pair summing past 255 — which is most of
        // a photograph.
        //
        // **It takes two pixels to reach the bug**, and the first version of this test used one:
        // at the start of a row the left neighbour is *zero*, so `(0 + 200) / 2` never
        // overflows and the test passed against a `u8` implementation. The second pixel of row
        // 1 is where both neighbours are large — `a = 200` and `b = 200`, whose true average is
        // 200 and whose wrapping one is 72.
        let rows = [
            0, 200, 200, 200, 200, 200, 200, // row 0: two pixels, all channels 200
            3, 100, 100, 100, 0, 0, 0, // row 1: average, then average with nothing added
        ];
        let img = decode(&png(2, 2, 2, &rows, None)).expect("decodes");
        // Pixel 0: a = 0, b = 200 → 100, plus 100.
        assert_eq!(px(&img, 0, 1), (200, 200, 200));
        // Pixel 1: a = 200 (just computed), b = 200 → 200, plus nothing.
        assert_eq!(px(&img, 1, 1), (200, 200, 200));
    }

    #[test]
    fn the_paeth_predictor_matches_the_spec_where_it_is_easy_to_get_wrong() {
        // **`a + b - c` leaves `[0, 255]` in both directions.** Computed in `u8` this gives a
        // different answer, which shows up as coloured noise along edges. `a=200, b=100, c=250`
        // gives `p = 50`: `pa = 150`, `pb = 50`, `pc = 200`, so `b` wins.
        assert_eq!(paeth(200, 100, 250), 100);
        // And underflow the other way: `a=10, b=20, c=200` gives `p = -170`, so `a` is closest.
        assert_eq!(paeth(10, 20, 200), 10);
        // **The tie-break order decides real cases, not only the all-equal one.** `paeth(5,5,5)`
        // was the first version of this assertion and it pins nothing: every branch returns 5.
        // These two are where `a`-then-`b`-then-`c` and any other order disagree — an encoder
        // chose its filter assuming the spec's order, so a decoder with a different one produces
        // different pixels.
        assert_eq!(paeth(0, 3, 1), 3, "pa == pc, and the spec reaches `b` before `c`");
        assert_eq!(paeth(0, 3, 2), 0, "pa ties with pb and pc, and `a` wins");
    }

    #[test]
    fn a_paeth_filtered_row_decodes() {
        let rows = [
            0, 10, 20, 30, 40, 50, 60, // row 0
            4, 1, 1, 1, 1, 1, 1, // row 1, paeth
        ];
        let img = decode(&png(2, 2, 2, &rows, None)).expect("decodes");
        // First pixel of row 1: a = 0, b = 10, c = 0 → p = 10, b wins → 10, plus 1.
        assert_eq!(px(&img, 0, 1), (11, 21, 31));
    }

    #[test]
    fn greyscale_and_palette_and_alpha_all_decode() {
        // One grey byte becomes three equal channels.
        let grey = decode(&png(2, 1, 0, &[0, 40, 200], None)).expect("greyscale decodes");
        assert_eq!(px(&grey, 0, 0), (40, 40, 40));
        assert_eq!(px(&grey, 1, 0), (200, 200, 200));

        // A palette index looks its colour up.
        let plte = [255, 0, 0, 0, 0, 255];
        let pal = decode(&png(2, 1, 3, &[0, 1, 0], Some(&plte))).expect("palette decodes");
        assert_eq!(px(&pal, 0, 0), (0, 0, 255));
        assert_eq!(px(&pal, 1, 0), (255, 0, 0));

        // RGBA drops its fourth byte rather than reading it as the next pixel's red.
        let rgba = decode(&png(2, 1, 6, &[0, 1, 2, 3, 128, 4, 5, 6, 255], None)).expect("rgba");
        assert_eq!(px(&rgba, 0, 0), (1, 2, 3));
        assert_eq!(px(&rgba, 1, 0), (4, 5, 6));

        // Greyscale + alpha does the same with two bytes per pixel.
        let ga = decode(&png(2, 1, 4, &[0, 60, 128, 90, 255], None)).expect("grey+alpha");
        assert_eq!(px(&ga, 0, 0), (60, 60, 60));
        assert_eq!(px(&ga, 1, 0), (90, 90, 90));
    }

    #[test]
    fn several_idat_chunks_are_one_zlib_stream() {
        // A real encoder splits its compressed data across as many `IDAT`s as it likes.
        // Inflating each separately is the classic way to decode the first chunk and nothing
        // else — so the bytes are split *inside* the stream, where neither half is valid alone.
        let rows = [0u8, 1, 2, 3, 0, 4, 5, 6];
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6);
        let (a, b) = z.split_at(z.len() / 2);
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 2, 8, 2, 0));
        v.extend_from_slice(&chunk(b"IDAT", a));
        v.extend_from_slice(&chunk(b"IDAT", b));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        let img = decode(&v).expect("two IDATs are one stream");
        assert_eq!(px(&img, 0, 0), (1, 2, 3));
        assert_eq!(px(&img, 0, 1), (4, 5, 6));
    }

    #[test]
    fn an_ancillary_chunk_between_the_ones_that_matter_is_skipped() {
        // Real encoders put `gAMA`, `pHYs` and text chunks wherever they like. A decoder that
        // expected `IHDR, IDAT, IEND` would refuse most photographs.
        let rows = [0u8, 1, 2, 3];
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6);
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        v.extend_from_slice(&chunk(b"gAMA", &[0, 1, 0x86, 0xA0]));
        v.extend_from_slice(&chunk(b"tEXt", b"Comment\0made by a person"));
        v.extend_from_slice(&chunk(b"IDAT", &z));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(px(&decode(&v).expect("decodes"), 0, 0), (1, 2, 3));
    }

    #[test]
    fn every_refusal_names_its_cause() {
        // The caller turns these into a console line beside a desktop that fell back to its
        // ground colour, so "the image did not load" for all of them would be the same answer
        // for a missing file, an interlaced picture and a truncated download.
        assert_eq!(decode(b"not a png at all"), Err(PngError::NotPng));
        assert_eq!(decode(&[]), Err(PngError::NotPng));

        let mut interlaced = Vec::new();
        interlaced.extend_from_slice(&SIGNATURE);
        interlaced.extend_from_slice(&ihdr(2, 2, 8, 2, 1));
        interlaced.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&interlaced), Err(PngError::Interlaced));

        let mut deep = Vec::new();
        deep.extend_from_slice(&SIGNATURE);
        deep.extend_from_slice(&ihdr(2, 2, 16, 2, 0));
        deep.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&deep), Err(PngError::BitDepth(16)));

        let mut odd = Vec::new();
        odd.extend_from_slice(&SIGNATURE);
        odd.extend_from_slice(&ihdr(2, 2, 8, 5, 0));
        odd.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&odd), Err(PngError::ColourType(5)));

        let mut empty = Vec::new();
        empty.extend_from_slice(&SIGNATURE);
        empty.extend_from_slice(&ihdr(0, 4, 8, 2, 0));
        empty.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&empty), Err(PngError::Size));
    }

    #[test]
    fn a_header_claiming_more_pixels_than_the_cap_is_refused_before_anything_is_allocated() {
        // **Four bytes from a file, multiplied by four and handed to the allocator.** 65535 by
        // 65535 is 4.2 gigapixels; without the cap this asks for tens of gigabytes and the
        // process dies somewhere with no message.
        let mut huge = Vec::new();
        huge.extend_from_slice(&SIGNATURE);
        huge.extend_from_slice(&ihdr(65535, 65535, 8, 2, 0));
        huge.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&huge), Err(PngError::Size));
    }

    #[test]
    fn the_cap_refuses_a_picture_the_machine_could_not_hold() {
        // **The case the old 64-megapixel cap did not refuse** (PR #272 review, worth fixing 2):
        // an 8192x8192 image compresses to a couple of hundred kilobytes when it is flat, sails
        // through `libfs`'s 8 MiB read cap, passed a 64-megapixel test *exactly*, and then asked
        // for well over half a gigabyte on a 256 MiB machine. The number is derived now, so this
        // asserts the derivation rather than a literal.
        let mut big = Vec::new();
        big.extend_from_slice(&SIGNATURE);
        big.extend_from_slice(&ihdr(8192, 8192, 8, 2, 0));
        big.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&big), Err(PngError::Size));
        // …and the sizes a wallpaper actually is are still admitted. These are *header* checks:
        // what they pin is which images `parse_ihdr` lets through, not a whole decode.
        assert!(1920 * 1080 < MAX_PIXELS, "a 1080p picture fits");
        assert!(2560 * 1600 < MAX_PIXELS, "so does a 2560x1600 one");
        assert!(3840 * 2160 > MAX_PIXELS, "and 4K is refused rather than fatal");
        // The budget is what the bound is made of, so a change to one moves the other.
        assert_eq!(MAX_PIXELS, (MAX_DECODE_BYTES / PEAK_BYTES_PER_PIXEL) as u64);
    }

    #[test]
    fn a_stream_that_inflates_past_what_the_header_implies_is_refused() {
        // **A few hundred bytes of `IDAT` can inflate to gigabytes**, and a cap on the *image*
        // cannot catch it: the header is small and honest, and the allocation happens inside the
        // inflate before anything here sees a byte. The limit is what the rows actually need —
        // one filter byte plus a row of samples, per row.
        let honest = [0u8, 1, 2, 3]; // 1x1 RGB needs exactly four bytes
        let mut bomb = alloc::vec![0u8; 64 * 1024];
        bomb[..4].copy_from_slice(&honest);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&bomb, 9);
        assert!(z.len() < 1024, "the fixture is small and expands hugely: {} bytes", z.len());
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        v.extend_from_slice(&chunk(b"IDAT", &z));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&v), Err(PngError::Inflate));
    }

    #[test]
    fn a_chunk_length_past_the_end_of_the_file_is_refused() {
        // The length is four bytes a peer controls. A reader that trusted it slices past the
        // end — the same class as the clipboard codec's, and the reason this is tested with
        // bytes a correct writer would never produce.
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        // An `IDAT` claiming a megabyte, with nothing after it.
        v.extend_from_slice(&(1_000_000u32).to_be_bytes());
        v.extend_from_slice(b"IDAT");
        v.extend_from_slice(&[0, 0, 0]);
        assert_eq!(decode(&v), Err(PngError::Truncated));
    }

    #[test]
    fn a_short_inflated_stream_is_refused_rather_than_read_past() {
        // The header says how many bytes the rows need; the zlib stream is a separate claim.
        // A decoder that trusted the header would index past its own buffer.
        let rows = [0u8, 1, 2, 3]; // one row, but the header will claim four
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 4, 8, 2, 0));
        v.extend_from_slice(&chunk(b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6)));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&v), Err(PngError::Filter));
    }

    #[test]
    fn an_unknown_filter_byte_is_refused() {
        let rows = [9u8, 1, 2, 3];
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        v.extend_from_slice(&chunk(b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(&rows, 6)));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&v), Err(PngError::Filter));
    }

    #[test]
    fn a_palette_index_past_the_palette_is_refused() {
        let plte = [255, 0, 0];
        assert_eq!(decode(&png(1, 1, 3, &[0, 7], Some(&plte))), Err(PngError::Palette));
    }

    #[test]
    fn a_broken_zlib_stream_is_refused() {
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        v.extend_from_slice(&chunk(b"IDAT", b"this is not deflate"));
        v.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&v), Err(PngError::Inflate));
    }
}
