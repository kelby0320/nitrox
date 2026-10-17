//! Decoding PNG, so that a wallpaper is a file a person supplies.
//!
//! Bit depth 8 only, every colour type. Adam7 interlacing and other depths are refused by name,
//! as a [`PngError`] the caller puts on the console beside a desktop that fell back to its ground
//! colour. Inflate is `miniz_oxide`: small tree, permissive licence, builds for the target.

use alloc::vec::Vec;

use crate::format::PixelFormat;
use crate::framebuffer::Geometry;

/// The eight bytes every PNG begins with.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

// Chunk lengths are added to offsets without a checked add, sound only while a `u32` length cannot
// overflow a `usize` offset; this crate also builds for 32-bit hosts.
const _: () = assert!(usize::BITS >= 64, "PNG chunk arithmetic assumes a 64-bit usize");

/// What a decode may allocate at its peak, in bytes.
///
/// The guest boots with 256 MiB; 32 is a twelfth, admits up to 2560x1600 and refuses 4K as a
/// refusal rather than a dead session (see [`PngError::OutOfMemory`]).
pub const MAX_DECODE_BYTES: usize = 32 * 1024 * 1024;

/// Bytes held per pixel at a decode's peak, which [`MAX_PIXELS`] divides: `unfilter` holds the
/// inflated stream and its output, then `to_xrgb` that output and the result (RGBA is `4 + 4`);
/// `decode` drops the stream between, so the peaks do not add.
const PEAK_BYTES_PER_PIXEL: usize = 8;

/// The largest image this will decode, in pixels, derived from [`MAX_DECODE_BYTES`]: a flat
/// 8192x8192 PNG compresses to 190 KiB yet needs over half a gigabyte.
pub const MAX_PIXELS: u64 = (MAX_DECODE_BYTES / PEAK_BYTES_PER_PIXEL) as u64;

/// Why a PNG did not decode, by named cause so the console line says which happened.
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
    /// An allocation the decode needed could not be made: a refusal rather than a panic, since
    /// `libheap` returns null on exhaustion. [`MAX_PIXELS`] catches the ordinary case.
    OutOfMemory,
    /// The zlib stream did not inflate.
    Inflate,
    /// The inflated data is not the size the header implies — too short, or a filter byte names
    /// a method that does not exist.
    Filter,
}

impl PngError {
    /// A sentence for a log line, `'static` so a caller with no allocator can print it.
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

/// A decoded image: XRGB8888 pixels and their geometry, the pair every surface here is, so it
/// goes into [`box_downscale`](crate::scale::box_downscale) or a blit unconverted. Alpha is
/// dropped: a wallpaper is drawn on nothing.
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

    // Chunks are walked, not assumed in order: ancillary chunks may sit anywhere before `IEND`.
    let mut at = SIGNATURE.len();
    loop {
        // Length (4), type (4), data, CRC (4).
        if at + 8 > bytes.len() {
            return Err(PngError::Truncated);
        }
        let len = u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        let kind = &bytes[at + 4..at + 8];
        let start = at + 8;
        // Plain arithmetic: `len` is at most 4 GiB and `at` is bounded by the file, so on a 64-bit
        // `usize` the sum cannot wrap (the const assertion above states that).
        let end = start + len;
        if end + 4 > bytes.len() {
            return Err(PngError::Truncated);
        }
        let data = &bytes[start..end];
        match kind {
            b"IHDR" => header = Some(parse_ihdr(data)?),
            b"PLTE" => {
                if !data.len().is_multiple_of(3) {
                    return Err(PngError::Palette);
                }
                palette = data.as_chunks::<3>().0.iter().map(|c| [c[0], c[1], c[2]]).collect();
            }
            // A PNG's compressed data is one zlib stream split across however many `IDAT`s.
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        // The CRC is skipped: the length checks and the inflate refuse every failure it catches.
        at = end + 4;
    }

    let h = header.ok_or(PngError::NotPng)?;
    // Bounded by what the header implies, since a few hundred bytes of `IDAT` can inflate to
    // gigabytes: one filter byte plus a row of samples, per row.
    let expect = h.height as usize * (1 + h.width as usize * h.bpp);
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, expect).map_err(|_| PngError::Inflate)?;
    let unfiltered = unfilter(&raw, &h)?;
    // Dropped before the XRGB buffer is asked for, so the two peaks do not add.
    drop(raw);
    to_xrgb(&unfiltered, &h, &palette)
}

/// `n` zeroed bytes, or [`PngError::OutOfMemory`]; `vec![0; n]` would abort the process.
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

    // Before anything is allocated: these are four bytes from a file.
    if width == 0 || height == 0 || width as u64 * height as u64 > MAX_PIXELS {
        return Err(PngError::Size);
    }
    if depth != 8 {
        return Err(PngError::BitDepth(depth));
    }
    if interlace != 0 {
        return Err(PngError::Interlaced);
    }
    // Channels per pixel; at depth 8 that is bytes per pixel, which the filters are defined in.
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

/// The predictors read already-unfiltered neighbours, so this reads its own output and treats
/// off-image neighbours as zero.
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
            let a = if x >= h.bpp { out[y * stride + x - h.bpp] } else { 0 };
            let b = if y > 0 { out[(y - 1) * stride + x] } else { 0 };
            let c = if y > 0 && x >= h.bpp { out[(y - 1) * stride + x - h.bpp] } else { 0 };
            let v = match filter {
                0 => src[x],
                1 => src[x].wrapping_add(a),
                2 => src[x].wrapping_add(b),
                // Computed wider: `(a + b) / 2` in `u8` overflows past 255.
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
/// The arithmetic is `i16`: `a + b - c` leaves `[0, 255]`, and `u8` gives the wrong answer.
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = ((p - a as i16).abs(), (p - b as i16).abs(), (p - c as i16).abs());
    // Tie-break order is the spec's (`a`, `b`, `c`); encoders chose their filters assuming it.
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
            // The alpha byte is read past and dropped (see [`Image`]).
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

    /// A whole PNG, its rows compressed by a real deflate so a decoder cannot pass by symmetry.
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
        let word = u32::from_le_bytes([img.pixels[off], img.pixels[off + 1], img.pixels[off + 2], img.pixels[off + 3]]);
        let c = img.geometry.format.decode(word);
        (c.r, c.g, c.b)
    }

    #[test]
    fn a_two_by_two_rgb_image_decodes() {
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
        let rows = [0, 1, 2, 3, 4, 5, 6];
        let img = decode(&png(2, 1, 2, &rows, None)).expect("decodes");
        assert_eq!(img.geometry.pitch, 8);
        assert_eq!(img.pixels.len(), 8);
    }

    #[test]
    fn a_sub_filter_reads_the_pixel_to_its_left() {
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
        // Needs two pixels: at a row's start the left neighbour is zero and cannot overflow; at row
        // 1's second pixel `a = b = 200`, true average 200, wrapping 72.
        let rows = [
            0, 200, 200, 200, 200, 200, 200, // row 0: two pixels, all channels 200
            3, 100, 100, 100, 0, 0, 0, // row 1: average, then average with nothing added
        ];
        let img = decode(&png(2, 2, 2, &rows, None)).expect("decodes");
        assert_eq!(px(&img, 0, 1), (200, 200, 200));
        assert_eq!(px(&img, 1, 1), (200, 200, 200));
    }

    #[test]
    fn the_paeth_predictor_matches_the_spec_where_it_is_easy_to_get_wrong() {
        // `a=200, b=100, c=250` gives `p = 50`: `pa = 150`, `pb = 50`, `pc = 200`, so `b` wins.
        assert_eq!(paeth(200, 100, 250), 100);
        // And underflow: `a=10, b=20, c=200` gives `p = -170`, so `a` is closest.
        assert_eq!(paeth(10, 20, 200), 10);
        // The tie-break decides real cases; `paeth(5,5,5)` pins nothing, as every branch returns 5.
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
        assert_eq!(px(&img, 0, 1), (11, 21, 31));
    }

    #[test]
    fn greyscale_and_palette_and_alpha_all_decode() {
        let grey = decode(&png(2, 1, 0, &[0, 40, 200], None)).expect("greyscale decodes");
        assert_eq!(px(&grey, 0, 0), (40, 40, 40));
        assert_eq!(px(&grey, 1, 0), (200, 200, 200));

        let plte = [255, 0, 0, 0, 0, 255];
        let pal = decode(&png(2, 1, 3, &[0, 1, 0], Some(&plte))).expect("palette decodes");
        assert_eq!(px(&pal, 0, 0), (0, 0, 255));
        assert_eq!(px(&pal, 1, 0), (255, 0, 0));

        let rgba = decode(&png(2, 1, 6, &[0, 1, 2, 3, 128, 4, 5, 6, 255], None)).expect("rgba");
        assert_eq!(px(&rgba, 0, 0), (1, 2, 3));
        assert_eq!(px(&rgba, 1, 0), (4, 5, 6));

        let ga = decode(&png(2, 1, 4, &[0, 60, 128, 90, 255], None)).expect("grey+alpha");
        assert_eq!(px(&ga, 0, 0), (60, 60, 60));
        assert_eq!(px(&ga, 1, 0), (90, 90, 90));
    }

    #[test]
    fn several_idat_chunks_are_one_zlib_stream() {
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
        // 65535 by 65535 is 4.2 gigapixels; without the cap this asks for tens of gigabytes.
        let mut huge = Vec::new();
        huge.extend_from_slice(&SIGNATURE);
        huge.extend_from_slice(&ihdr(65535, 65535, 8, 2, 0));
        huge.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&huge), Err(PngError::Size));
    }

    #[test]
    fn the_cap_refuses_a_picture_the_machine_could_not_hold() {
        // A flat 8192x8192 image is small on disk but needs over half a gigabyte.
        let mut big = Vec::new();
        big.extend_from_slice(&SIGNATURE);
        big.extend_from_slice(&ihdr(8192, 8192, 8, 2, 0));
        big.extend_from_slice(&chunk(b"IEND", &[]));
        assert_eq!(decode(&big), Err(PngError::Size));
        // The sizes a wallpaper actually is are still admitted (header checks only).
        const { assert!(1920 * 1080 < MAX_PIXELS, "a 1080p picture fits") };
        const { assert!(2560 * 1600 < MAX_PIXELS, "so does a 2560x1600 one") };
        const { assert!(3840 * 2160 > MAX_PIXELS, "and 4K is refused rather than fatal") };
        assert_eq!(MAX_PIXELS, (MAX_DECODE_BYTES / PEAK_BYTES_PER_PIXEL) as u64);
    }

    #[test]
    fn a_stream_that_inflates_past_what_the_header_implies_is_refused() {
        // A few hundred bytes of `IDAT` can inflate to gigabytes before anything sees a byte;
        // the limit is what the rows need.
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
        let mut v = Vec::new();
        v.extend_from_slice(&SIGNATURE);
        v.extend_from_slice(&ihdr(1, 1, 8, 2, 0));
        v.extend_from_slice(&(1_000_000u32).to_be_bytes());
        v.extend_from_slice(b"IDAT");
        v.extend_from_slice(&[0, 0, 0]);
        assert_eq!(decode(&v), Err(PngError::Truncated));
    }

    #[test]
    fn a_short_inflated_stream_is_refused_rather_than_read_past() {
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
