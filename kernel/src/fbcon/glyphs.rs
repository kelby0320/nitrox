//! The console's glyphs: an 8×16 bitmap face, **Terminus Font** (normal weight) in the PSF1
//! form Debian's `console-setup` ships (`assets/fonts/Lat15-Terminus16.psf`; SIL OFL 1.1,
//! licence in `assets/fonts/LICENSE-Terminus.txt`), embedded whole and read in place; its shape
//! is checked when the kernel compiles. A bitmap, because the console draws before there is an
//! allocator or a filesystem. **Also compiled into `xtask`**, by path, whose `-serial none`
//! gate reads a screendump back with these glyphs, so this file names nothing outside itself.
//!
//! PSF1 is four header bytes (`0x36 0x04`, a mode, the glyph height), 256 glyphs of
//! [`GLYPH_H`] bytes each — one byte per pixel row, bit 7 leftmost — and then, when the mode
//! says so, a Unicode table: per glyph, the UCS-2 codepoints it draws, each list ended by
//! `0xFFFF`. `0xFFFE` starts a list of combining sequences, which the console never draws.

/// Glyph width in pixels. PSF1 fixes it at eight.
pub const GLYPH_W: usize = 8;
/// Glyph height in pixels, as this face's header states (checked at compile time).
pub const GLYPH_H: usize = 16;

/// The glyph drawn for a character the face does not have.
pub const REPLACEMENT: u8 = b'?';
/// The glyph a cleared cell holds.
pub const BLANK: u8 = b' ';

const PSF: &[u8] = include_bytes!("../../../assets/fonts/Lat15-Terminus16.psf");

const MAGIC: [u8; 2] = [0x36, 0x04];
/// Mode bit: 512 glyphs rather than 256.
const MODE_512: u8 = 0x01;
/// Mode bit: a Unicode table follows the glyphs.
const MODE_HAS_TABLE: u8 = 0x02;
const HEADER: usize = 4;
const COUNT: usize = 256;
const TABLE: usize = HEADER + COUNT * GLYPH_H;
const END_OF_GLYPH: u16 = 0xFFFF;
const SEQUENCE: u16 = 0xFFFE;

const _: () = assert!(
    well_formed(),
    "assets/fonts/Lat15-Terminus16.psf is not the 256-glyph, 16-row PSF1 face with a Unicode \
     table and printable ASCII at its own index that the console reads"
);

/// Whether the embedded file is the face everything below assumes, including **printable ASCII
/// at its own index**, which [`index_of`] takes as a shortcut instead of searching the table.
const fn well_formed() -> bool {
    if PSF.len() < TABLE + 2 || !(PSF.len() - TABLE).is_multiple_of(2) {
        return false;
    }
    if PSF[0] != MAGIC[0] || PSF[1] != MAGIC[1] {
        return false;
    }
    if PSF[2] & MODE_512 != 0 || PSF[2] & MODE_HAS_TABLE == 0 || PSF[3] as usize != GLYPH_H {
        return false;
    }
    let mut c = 0x20u16;
    while c < 0x7F {
        match search(c) {
            Some(i) if i as u16 == c => {}
            _ => return false,
        }
        c += 1;
    }
    true
}

/// The first glyph the Unicode table lists as drawing `cp`.
const fn search(cp: u16) -> Option<u8> {
    let mut at = TABLE;
    let mut glyph = 0usize;
    let mut in_sequence = false;
    while at + 1 < PSF.len() && glyph < COUNT {
        let entry = u16::from_le_bytes([PSF[at], PSF[at + 1]]);
        at += 2;
        if entry == END_OF_GLYPH {
            glyph += 1;
            in_sequence = false;
        } else if entry == SEQUENCE {
            in_sequence = true;
        } else if !in_sequence && entry == cp {
            return Some(glyph as u8);
        }
    }
    None
}

/// The glyph that draws `ch`, if the face has one.
pub fn index_of(ch: char) -> Option<u8> {
    let cp = ch as u32;
    if (0x20..0x7F).contains(&cp) {
        return Some(cp as u8);
    }
    u16::try_from(cp).ok().and_then(search)
}

/// The glyph that draws `ch`, or [`REPLACEMENT`] when the face has none.
pub fn index_or_replacement(ch: char) -> u8 {
    index_of(ch).unwrap_or(REPLACEMENT)
}

/// The pixel rows of glyph `index`, top first; bit 7 of each is the leftmost pixel.
pub fn rows(index: u8) -> &'static [u8; GLYPH_H] {
    let at = HEADER + index as usize * GLYPH_H;
    // `at + GLYPH_H <= TABLE <= PSF.len()` for every `u8` (`well_formed`); the fallback is
    // unreachable and exists so this cannot panic.
    match PSF[at..].first_chunk::<GLYPH_H>() {
        Some(r) => r,
        None => &[0; GLYPH_H],
    }
}

/// The character glyph `index` is listed as drawing first, if it is listed at all.
pub fn char_of(index: u8) -> Option<char> {
    let mut at = TABLE;
    let mut glyph = 0usize;
    let mut in_sequence = false;
    while at + 1 < PSF.len() && glyph <= index as usize {
        let entry = u16::from_le_bytes([PSF[at], PSF[at + 1]]);
        at += 2;
        if entry == END_OF_GLYPH {
            glyph += 1;
            in_sequence = false;
        } else if entry == SEQUENCE {
            in_sequence = true;
        } else if !in_sequence && glyph == index as usize {
            return char::from_u32(entry as u32);
        }
    }
    None
}

/// Which glyph a bitmap is, trying printable ASCII first.
///
/// For reading a screen back, where several glyphs can be pixel-identical (a blank and a
/// no-break space) and plain index order would name a codepoint nobody printed.
pub fn identify(bitmap: &[u8; GLYPH_H]) -> Option<u8> {
    let ascii = 0x20u8..0x7F;
    let rest = (0u8..0x20).chain(0x7F..=0xFF);
    ascii.chain(rest).find(|&i| rows(i) == bitmap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_ascii_draws_itself_and_differs_from_its_neighbours() {
        for c in 0x20u8..0x7F {
            assert_eq!(index_of(c as char), Some(c));
            assert_eq!(char_of(c), Some(c as char));
        }
        assert_ne!(rows(b'a'), rows(b'b'));
        assert_ne!(rows(b'0'), rows(b'O'));
        assert_eq!(rows(BLANK), &[0; GLYPH_H], "a cleared cell is blank");
    }

    #[test]
    fn the_table_is_searched_for_what_ascii_does_not_cover() {
        let dash = index_of('—').expect("the face draws an em dash");
        assert!(dash >= 0x80, "an em dash is not an ASCII glyph");
        assert_eq!(char_of(dash), Some('—'));
        assert!(index_of('→').is_some());
        assert_eq!(index_of('⇒'), None, "not in this face");
        assert_eq!(index_or_replacement('⇒'), REPLACEMENT);
        assert_eq!(index_or_replacement('😀'), REPLACEMENT, "beyond UCS-2");
    }

    #[test]
    fn a_bitmap_is_identified_as_the_ascii_glyph_it_matches() {
        for c in 0x20u8..0x7F {
            assert_eq!(identify(rows(c)), Some(c), "{:?}", c as char);
        }
        assert_eq!(identify(&[0xAA; GLYPH_H]), None);
    }
}
