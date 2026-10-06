//! **Names on a FAT** (Phase 6 Part E.2): long names in UTF-16, short names in 8.3, and the rules
//! for both.
//!
//! - **Long names** are up to 255 UTF-16 units, held in long-name entries before a file's short
//!   entry. The system speaks UTF-8, so a name crosses that line in both directions.
//! - **Short names** are eleven bytes, eight and three, upper case. Every file has one; a name
//!   that is not a valid upper-case 8.3 name gets a long name too, and a short one generated from
//!   it with a numeric tail (`LONGNA~1.TXT`).
//! - **Names are case-insensitive and case-preserving**, as FAT is read everywhere else — for
//!   ASCII letters. Other characters compare exactly: Unicode's case rules are not FAT's to apply
//!   here.

/// The longest long name, in UTF-16 units.
pub const MAX_UNITS: usize = 255;
/// How many UTF-16 units one long-name entry holds.
pub const UNITS_PER_ENTRY: usize = 13;

/// **A name as UTF-16 units**: its length, or `None` if it is not UTF-8 or longer than
/// [`MAX_UNITS`].
pub fn to_utf16(name: &[u8], out: &mut [u16; MAX_UNITS]) -> Option<usize> {
    let s = core::str::from_utf8(name).ok()?;
    let mut n = 0;
    for u in s.encode_utf16() {
        if n == MAX_UNITS {
            return None;
        }
        out[n] = u;
        n += 1;
    }
    Some(n)
}

/// **UTF-16 units as UTF-8 in `out`**: its length, or `None` if they are not valid UTF-16 or do not
/// fit — a long name of 255 units can take 765 bytes, and a listing entry carries 255.
pub fn from_utf16(units: &[u16], out: &mut [u8]) -> Option<usize> {
    let mut n = 0;
    for c in char::decode_utf16(units.iter().copied()) {
        let c = c.ok()?;
        let len = c.len_utf8();
        if n + len > out.len() {
            return None;
        }
        c.encode_utf8(&mut out[n..n + len]);
        n += len;
    }
    Some(n)
}

/// **Whether a FAT can hold `name`**: one or more characters, at most [`MAX_UNITS`] of UTF-16, not
/// `.` or `..`, none of `"*/:<>?\|` or a control character, and not ending in a space or a dot,
/// which other systems strip and so would read as another name.
pub fn valid(name: &[u8]) -> bool {
    let mut units = [0u16; MAX_UNITS];
    if name.is_empty() || name == b"." || name == b".." || to_utf16(name, &mut units).is_none() {
        return false;
    }
    if name.iter().any(|&c| c < 0x20 || c == 0x7F || b"\"*/:<>?\\|".contains(&c)) {
        return false;
    }
    !matches!(name.last(), Some(b' ' | b'.'))
}

/// **Two names the same to a FAT**: equal but for the case of ASCII letters.
pub fn eq_fold(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Whether `c` may stand in a short name as it is: an upper-case letter, a digit, or one of the
/// marks the specification allows.
fn short_char(c: u8) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit() || b"!#$%&'()-@^_`{}~".contains(&c)
}

/// **`name` as a short name, if it is one**: a valid upper-case 8.3 name, which needs no long name.
/// Its eleven bytes, space-padded.
pub fn short_exact(name: &[u8]) -> Option<[u8; 11]> {
    let (base, ext) = match name.iter().position(|&c| c == b'.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, &name[..0]),
    };
    let ok = (1..=8).contains(&base.len())
        && ext.len() <= 3
        && !(ext.is_empty() && name.contains(&b'.'))
        && base.iter().chain(ext).all(|&c| short_char(c));
    if !ok {
        return None;
    }
    let mut out = [b' '; 11];
    out[..base.len()].copy_from_slice(base);
    out[8..8 + ext.len()].copy_from_slice(ext);
    Some(out)
}

/// **The basis of a generated short name** (the specification's algorithm): `name` upper-cased,
/// spaces and leading dots dropped, the extension what follows the last dot, every character a
/// short name cannot hold as `_`. The base, up to eight bytes, and the extension, up to three.
pub fn basis(name: &[u8]) -> ([u8; 8], usize, [u8; 3], usize) {
    let fold = |c: u8| -> Option<u8> {
        match c {
            b' ' => None,
            c if short_char(c.to_ascii_uppercase()) => Some(c.to_ascii_uppercase()),
            _ => Some(b'_'),
        }
    };
    let start = name.iter().position(|&c| c != b'.').unwrap_or(name.len());
    let body = &name[start..];
    let (stem, ext) = match body.iter().rposition(|&c| c == b'.') {
        Some(dot) => (&body[..dot], &body[dot + 1..]),
        None => (body, &body[..0]),
    };
    let mut b = [b' '; 8];
    let mut bn = 0;
    // A multi-byte UTF-8 character is one character a short name cannot hold: one `_`.
    for &c in stem.iter().filter(|&&c| c != b'.' && (c < 0x80 || c >= 0xC0)) {
        if bn == 8 {
            break;
        }
        if let Some(c) = fold(c) {
            b[bn] = c;
            bn += 1;
        }
    }
    let mut e = [b' '; 3];
    let mut en = 0;
    for &c in ext.iter().filter(|&&c| c < 0x80 || c >= 0xC0) {
        if en == 3 {
            break;
        }
        if let Some(c) = fold(c) {
            e[en] = c;
            en += 1;
        }
    }
    if bn == 0 {
        b[0] = b'_';
        bn = 1;
    }
    (b, bn, e, en)
}

/// **A short name with numeric tail `n`**: as much of `base` as leaves room for `~n` in eight
/// bytes, then the tail, then the extension.
pub fn with_tail(base: &[u8], ext: &[u8], n: u32) -> [u8; 11] {
    let mut digits = [0u8; 10];
    let mut d = 0;
    let mut v = n;
    loop {
        digits[d] = b'0' + (v % 10) as u8;
        d += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    let keep = base.len().min(8 - 1 - d);
    let mut out = [b' '; 11];
    out[..keep].copy_from_slice(&base[..keep]);
    out[keep] = b'~';
    for i in 0..d {
        out[keep + 1 + i] = digits[d - 1 - i];
    }
    out[8..8 + ext.len().min(3)].copy_from_slice(&ext[..ext.len().min(3)]);
    out
}

/// **The checksum a long name's entries carry of their short entry's name**, which ties them to
/// it: a long name whose checksum does not match is stale, left by a system that renamed the file
/// without knowing about long names.
pub fn checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |sum, &c| (sum >> 1).wrapping_add(sum << 7).wrapping_add(c))
}

/// Windows' case bits in a short entry: its base, and its extension, are lower case.
pub const LOWER_BASE: u8 = 0x08;
pub const LOWER_EXT: u8 = 0x10;

/// **A short name as it is shown**: `NAME.EXT`, padding trimmed, with Windows' case bits applied —
/// so a file another system named `readme.txt` with no long name reads that way. A first byte of
/// `0x05` stands for `0xE5`. A byte outside ASCII, from an old system's code page, reads as `_`.
/// Its length in `out`.
pub fn short_display(raw: &[u8; 11], case: u8, out: &mut [u8; 12]) -> usize {
    let fix = |i: usize, c: u8, lower: bool| -> u8 {
        let c = if i == 0 && c == 0x05 { 0xE5 } else { c };
        if c >= 0x80 {
            b'_'
        } else if lower {
            c.to_ascii_lowercase()
        } else {
            c
        }
    };
    let blen = raw[..8].iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
    let elen = raw[8..].iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
    let mut n = 0;
    for (i, &c) in raw[..blen].iter().enumerate() {
        out[n] = fix(i, c, case & LOWER_BASE != 0);
        n += 1;
    }
    if elen > 0 {
        out[n] = b'.';
        n += 1;
        for &c in &raw[8..8 + elen] {
            out[n] = fix(1, c, case & LOWER_EXT != 0);
            n += 1;
        }
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_utf16_round_trip_and_a_name_too_long_is_refused() {
        let mut u = [0u16; MAX_UNITS];
        let name = "Ünïcödé 名前 🎉.txt".as_bytes();
        let n = to_utf16(name, &mut u).unwrap();
        assert!(u[..n].iter().any(|&c| (0xD800..0xDC00).contains(&c)), "the emoji is a surrogate pair");
        let mut back = [0u8; 255];
        let m = from_utf16(&u[..n], &mut back).unwrap();
        assert_eq!(&back[..m], name);
        assert!(to_utf16(&[b'a'; 255], &mut u).is_some());
        assert!(to_utf16(&[b'a'; 256], &mut u).is_none(), "256 units");
        assert_eq!(from_utf16(&[0xD800], &mut back), None, "an unpaired surrogate");
        let wide = ["é"; 200].concat();
        assert!(from_utf16(&wide.encode_utf16().collect::<Vec<_>>(), &mut back).is_none(), "400 bytes do not fit 255");
    }

    #[test]
    fn what_a_fat_can_name() {
        for ok in ["a", "README.TXT", "a long name.tar.gz", ".hidden", "naïve"] {
            assert!(valid(ok.as_bytes()), "{ok}");
        }
        for bad in ["", ".", "..", "a:b", "a*", "q?", "x|y", "t\"", "<", ">", "back\\slash", "tab\tname", "trailing ", "trailing."] {
            assert!(!valid(bad.as_bytes()), "{bad:?}");
        }
    }

    #[test]
    fn an_exact_short_name_is_upper_case_8_3_and_nothing_else() {
        assert_eq!(&short_exact(b"README.TXT").unwrap(), b"README  TXT");
        assert_eq!(&short_exact(b"MAKEFILE").unwrap(), b"MAKEFILE   ");
        assert_eq!(&short_exact(b"A~1.B").unwrap(), b"A~1     B  ");
        for not in ["readme.txt", "TOOLONGNAME.TXT", "A.TEXT", "A.B.C", ".HIDDEN", "A B", "TRAIL.", "NAÏVE"] {
            assert!(short_exact(not.as_bytes()).is_none(), "{not}");
        }
    }

    #[test]
    fn a_generated_short_name_is_the_basis_and_a_tail() {
        let (b, bn, e, en) = basis(b"a long name.tar.gz");
        assert_eq!((&b[..bn], &e[..en]), (&b"ALONGNAM"[..], &b"GZ"[..]));
        assert_eq!(&with_tail(&b[..bn], &e[..en], 1), b"ALONGN~1GZ ");
        assert_eq!(&with_tail(&b[..bn], &e[..en], 12), b"ALONG~12GZ ");
        let (b, bn, e, en) = basis("..naïve+résumé.pdf".as_bytes());
        assert_eq!((&b[..bn], &e[..en]), (&b"NA_VE_R_"[..], &b"PDF"[..]));
        let (b, bn, _, en) = basis("名前".as_bytes());
        assert_eq!((&b[..bn], en), (&b"__"[..], 0));
    }

    /// The specification's checksum, and the case bits shown.
    #[test]
    fn the_checksum_and_a_short_name_shown() {
        assert_eq!(checksum(b"README  TXT"), 0x73);
        let mut out = [0u8; 12];
        let n = short_display(b"README  TXT", LOWER_BASE | LOWER_EXT, &mut out);
        assert_eq!(&out[..n], b"readme.txt");
        let n = short_display(b"MAKEFILE   ", 0, &mut out);
        assert_eq!(&out[..n], b"MAKEFILE");
        let n = short_display(b"\x05BC     DAT", 0, &mut out);
        assert_eq!(&out[..n], b"_BC.DAT", "0x05 stands for 0xE5, outside ASCII");
    }

    #[test]
    fn names_compare_without_ascii_case() {
        assert!(eq_fold(b"ReadMe.TXT", b"readme.txt"));
        assert!(!eq_fold("É".as_bytes(), "é".as_bytes()), "beyond ASCII, exactly");
        assert!(!eq_fold(b"a", b"ab"));
    }
}
