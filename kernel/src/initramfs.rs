//! The boot **initramfs**: a Limine-loaded CPIO-newc blob, parsed in-kernel.
//!
//! Limine loads one module (the initramfs, see `boot/limine.conf` +
//! `kernel/src/limine.rs` [`ModuleRequest`](crate::limine::ModuleRequest)) into
//! HHDM-mapped memory; the kernel records it here at boot with [`set_blob`]. The
//! in-kernel `/initramfs` resource server ([`crate::object::kernel_server`])
//! resolves a path to a file's bytes with [`lookup`] and hands userspace a
//! read-only `MemoryObject` copy. Init reads `etc/init.toml` and (later) spawn
//! images from it.
//!
//! Only the **CPIO `newc`** format (magic `070701`) is parsed — the format
//! `cpio -H newc` / the xtask packer emits. Parsing is pure over a byte slice
//! and host-testable; reclaiming the blob's pages is deferred to the
//! resource-server lifecycle work (`docs/rationale/deferred-decisions.md`).

use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// The recorded blob pointer (HHDM-virtual) and length. `null` ⇒ no initramfs.
static BLOB_PTR: AtomicPtr<u8> = AtomicPtr::new(ptr::null_mut());
static BLOB_LEN: AtomicUsize = AtomicUsize::new(0);

/// Record the initramfs blob (a Limine module). Called once at boot.
///
/// # Safety
/// `ptr`/`len` must describe a live, `'static` byte region — the Limine-mapped
/// module (in `MEMMAP_KERNEL_AND_MODULES` memory, never reclaimed this slice).
pub unsafe fn set_blob(ptr: *mut u8, len: usize) {
    BLOB_LEN.store(len, Ordering::Relaxed);
    // Release so a later `blob()` Acquire observes the length too.
    BLOB_PTR.store(ptr, Ordering::Release);
}

/// The initramfs bytes, or `None` if no module was loaded.
pub fn blob() -> Option<&'static [u8]> {
    let p = BLOB_PTR.load(Ordering::Acquire);
    if p.is_null() {
        return None;
    }
    let len = BLOB_LEN.load(Ordering::Relaxed);
    // SAFETY: `set_blob` recorded a live `'static` region of `len` bytes at `p`.
    Some(unsafe { slice::from_raw_parts(p, len) })
}

const NEWC_MAGIC: &[u8; 6] = b"070701";
const HDR_LEN: usize = 110;
const TRAILER: &[u8] = b"TRAILER!!!";

/// Round `x` up to the next multiple of 4 (CPIO-newc aligns names and data to
/// 4-byte boundaries from the start of the archive).
const fn align4(x: usize) -> usize {
    (x + 3) & !3
}

/// Parse 8 ASCII hex digits at `field[..8]` into a `u32`. `None` if non-hex.
fn hex8(field: &[u8]) -> Option<u32> {
    if field.len() < 8 {
        return None;
    }
    let mut v: u32 = 0;
    let mut i = 0;
    while i < 8 {
        let d = match field[i] {
            c @ b'0'..=b'9' => c - b'0',
            c @ b'a'..=b'f' => c - b'a' + 10,
            c @ b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        v = (v << 4) | d as u32;
        i += 1;
    }
    Some(v)
}

/// Strip a leading `./` (cpio often stores `./etc/foo`) so callers can pass a
/// plain relative path (`etc/foo`).
fn normalize(name: &[u8]) -> &[u8] {
    if let [b'.', b'/', rest @ ..] = name {
        rest
    } else {
        name
    }
}

/// Look up `name` (a path relative to the archive root, no leading `/`) in the
/// CPIO-newc `blob`; return its file data slice. `None` if the file is absent or
/// the archive is malformed. Directory entries and the trailer never match.
pub fn lookup<'a>(blob: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let want = normalize(name);
    let mut off = 0usize;
    loop {
        if off + HDR_LEN > blob.len() {
            return None;
        }
        let hdr = &blob[off..off + HDR_LEN];
        if &hdr[0..6] != NEWC_MAGIC {
            return None;
        }
        // newc fields are 8 ASCII-hex each after the 6-byte magic:
        // filesize at bytes 54..62, namesize at 94..102.
        let filesize = hex8(&hdr[54..62])? as usize;
        let namesize = hex8(&hdr[94..102])? as usize;

        let name_start = off + HDR_LEN;
        let name_end = name_start.checked_add(namesize)?;
        if name_end > blob.len() || namesize == 0 {
            return None;
        }
        // The name field includes a trailing NUL (counted in namesize).
        let entry_name = normalize(&blob[name_start..name_end - 1]);

        // Data follows the name, padded to a 4-byte boundary (absolute, since the
        // archive starts 4-aligned); the next header is 4-aligned past the data.
        let data_start = align4(name_end);
        let data_end = data_start.checked_add(filesize)?;
        if data_end > blob.len() {
            return None;
        }

        if entry_name == TRAILER {
            return None; // end of archive
        }
        if entry_name == want {
            return Some(&blob[data_start..data_end]);
        }
        off = align4(data_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one CPIO-newc entry (header + NUL-terminated name + data, each
    /// region padded to 4 bytes) into `out`.
    fn push_entry(out: &mut Vec<u8>, name: &str, data: &[u8]) {
        let namesize = name.len() + 1; // + NUL
        let mut hdr = Vec::new();
        hdr.extend_from_slice(b"070701");
        // 13 eight-hex fields; only filesize (idx 6) and namesize (idx 11) matter.
        let fields = [
            1u32,                // ino
            0o100644,            // mode
            0,                   // uid
            0,                   // gid
            1,                   // nlink
            0,                   // mtime
            data.len() as u32,   // filesize
            0,                   // devmajor
            0,                   // devminor
            0,                   // rdevmajor
            0,                   // rdevminor
            namesize as u32,     // namesize
            0,                   // check
        ];
        for f in fields {
            hdr.extend_from_slice(format!("{f:08x}").as_bytes());
        }
        assert_eq!(hdr.len(), HDR_LEN);
        out.extend_from_slice(&hdr);
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (n, d) in entries {
            push_entry(&mut out, n, d);
        }
        push_entry(&mut out, "TRAILER!!!", b"");
        out
    }

    #[test]
    fn finds_files_and_misses() {
        let cpio = archive(&[
            ("etc/init.toml", b"[[mount]]\n"),
            ("sbin/init", b"\x7fELFstub"),
        ]);
        assert_eq!(lookup(&cpio, b"etc/init.toml"), Some(&b"[[mount]]\n"[..]));
        assert_eq!(lookup(&cpio, b"sbin/init"), Some(&b"\x7fELFstub"[..]));
        assert_eq!(lookup(&cpio, b"nope"), None);
    }

    #[test]
    fn strips_dot_slash_on_both_sides() {
        let cpio = archive(&[("./etc/init.toml", b"x")]);
        assert_eq!(lookup(&cpio, b"etc/init.toml"), Some(&b"x"[..]));
        assert_eq!(lookup(&cpio, b"./etc/init.toml"), Some(&b"x"[..]));
    }

    #[test]
    fn empty_file_and_odd_lengths_align() {
        // A 1-byte and a 3-byte file exercise the 4-byte data padding.
        let cpio = archive(&[("a", b"x"), ("bb", b"yyy"), ("empty", b"")]);
        assert_eq!(lookup(&cpio, b"a"), Some(&b"x"[..]));
        assert_eq!(lookup(&cpio, b"bb"), Some(&b"yyy"[..]));
        assert_eq!(lookup(&cpio, b"empty"), Some(&b""[..]));
    }

    #[test]
    fn trailer_and_garbage_return_none() {
        let cpio = archive(&[("a", b"x")]);
        assert_eq!(lookup(&cpio, b"TRAILER!!!"), None);
        assert_eq!(lookup(b"not a cpio archive", b"a"), None);
        assert_eq!(lookup(&[], b"a"), None);
    }
}
