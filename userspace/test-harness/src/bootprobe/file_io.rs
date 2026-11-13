//! A file read and written through `sys_io_submit`, and copied to another, on the root filesystem.

use super::*;

fn pattern(i: usize) -> u8 {
    (i * 7 + 3) as u8
}

/// **A file's `Write` lands, on the device once flushed, and one past its end is refused; a copy
/// moves its bytes; a `Read` is short at the end.**
pub(super) fn file_io_test(root: Root<'_>) -> bool {
    const PAGE: usize = super::PAGE as usize;
    let size = 3 * PAGE + 100;
    let a = block_on(fs::create::<_, MapReadWrite>(root, "/system/e8a", size as u64, RW));
    let b = block_on(fs::create::<_, MapReadWrite>(root, "/system/e8b", size as u64, RW));
    let (buf, back) =
        (Handle::<Memory, MapReadWrite>::create(4 * PAGE), Handle::<Memory, MapReadWrite>::create(4 * PAGE));
    let (Ok(a), Ok(b), Ok(buf), Ok(back)) = (a, b, buf, back) else {
        log::write(b"boot-probe: file io: the files or buffers could not be made FAIL\n");
        return false;
    };
    let bytes: Vec<u8> = (0..2 * PAGE).map(pattern).collect();
    if buf.map(4 * PAGE).and_then(|mut m| m.write_from(0, &bytes)).is_err() {
        log::write(b"boot-probe: file io: the buffer could not be filled FAIL\n");
        return false;
    }
    let wrote = block_on(a.write(&buf, 0, 50, bytes.len() as u64)).ok() == Some(bytes.len());
    let past = block_on(a.write(&buf, 0, size as u64 - 10, 20)).err().map(|e| e.code());
    let durable = block_on(fs::flush(&a)).is_ok()
        && RootDevice::open(root)
            .and_then(|d| d.read_file(b"/system/e8a", 50, bytes.len()))
            .is_some_and(|d| d == bytes);
    let copied = block_on(b.copy_from(&a, 0, 0, size as u64)).ok() == Some(size);
    let read = block_on(b.read(&back, 0, 0, (4 * PAGE) as u64)).ok() == Some(size);
    let mut got = alloc::vec![0xFF; size];
    let landed = back.map(4 * PAGE).and_then(|m| m.read_into(0, &mut got)).is_ok()
        && got[..50].iter().all(|&x| x == 0)
        && got[50..50 + bytes.len()] == bytes[..]
        && got[50 + bytes.len()..].iter().all(|&x| x == 0);
    let short = block_on(b.read(&back, 0, size as u64 - 10, 100)).ok() == Some(10);
    let mut line = Line::new();
    line.s(b"boot-probe: file io: a write ").s(if wrote { b"lands".as_slice() } else { b"FAILED" });
    line.s(if durable { b", on the device once flushed" } else { b", NOT on the device" });
    line.s(if past == Some(libkern::KError::InvalidArgument) {
        b"; past the end refused"
    } else {
        b"; past the end NOT refused"
    });
    line.s(if copied && landed { b"; a copy moves its bytes" } else { b"; a copy MISMATCH" });
    line.s(if read && short { b"; a read is short at the end" } else { b"; a read NOT short" });
    let ok = wrote && durable && past == Some(libkern::KError::InvalidArgument) && copied && landed && read && short;
    line.s(if ok { b" ok" } else { b" FAIL" }).end();
    ok
}

/// The bytes the page cache has filled from devices since boot, from `io.tsm`'s `page-cache` row.
fn filled(root: Root<'_>) -> Option<u64> {
    use libstream::wire::{Table, Value};
    let h = lookup::<File, MapRead, _>(root, "/svc/devices/info/io.tsm", Rights::MAP_READ | Rights::INSPECT).ok()?;
    let t = Table::decode(&read_all(&h)?).ok()?;
    let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name);
    let (name, bytes) = (col("name")?, col("read_bytes")?);
    let row = t.rows.iter().find(|r| r[name] == Value::Str("page-cache".into()))?;
    match row[bytes] {
        Value::Int(v) => Some(v.max(0) as u64),
        _ => None,
    }
}

/// **`IO_NOCACHE` lets a copy's window go, the source's with it, unless something maps it**: the
/// source read again after a copy fills from the device, and with a mapping held it does not.
pub(super) fn nocache_test(root: Root<'_>) -> bool {
    const LEN: usize = 16 * super::PAGE as usize;
    let src = block_on(fs::create::<_, MapReadWrite>(root, "/system/e8nc-src", LEN as u64, RW));
    let dst = block_on(fs::create::<_, MapReadWrite>(root, "/system/e8nc-dst", LEN as u64, RW));
    let buf = Handle::<Memory, MapReadWrite>::create(LEN);
    let (Ok(src), Ok(dst), Ok(buf)) = (src, dst, buf) else {
        log::write(b"boot-probe: no-cache: the files could not be made FAIL\n");
        return false;
    };
    let source = lookup::<File, MapRead, _>(root, "/system/e8nc-src", Rights::MAP_READ);
    let written = block_on(src.write(&buf, 0, 0, LEN as u64)).is_ok() && block_on(fs::flush(&src)).is_ok();
    let (Ok(source), true) = (source, written) else {
        log::write(b"boot-probe: no-cache: the source could not be written FAIL\n");
        return false;
    };
    let copied_then_reread = || {
        let copied = block_on(dst.copy_from(&source, 0, 0, LEN as u64)).ok() == Some(LEN);
        let before = filled(root)?;
        let read = block_on(source.read(&buf, 0, 0, LEN as u64)).ok() == Some(LEN);
        Some((copied && read).then_some(filled(root)?.saturating_sub(before)))
    };
    let mapping = source.map(LEN);
    let held = copied_then_reread().flatten();
    drop(mapping);
    let let_go = copied_then_reread().flatten();
    let ok = held.is_some_and(|n| n < LEN as u64) && let_go.is_some_and(|n| n >= LEN as u64);
    let mut line = Line::new();
    line.s(b"boot-probe: no-cache: a copy's source read again filled ").u(let_go.unwrap_or(0));
    line.s(b" bytes, and ").u(held.unwrap_or(0)).s(b" with a mapping held");
    line.s(if ok { b" ok" } else { b" FAIL" }).end();
    ok
}
