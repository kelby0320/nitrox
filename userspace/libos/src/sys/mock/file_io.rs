//! A file's `Read` and `Write`, as the kernel answers them, the bytes moved at once.

use super::*;

/// The file's right and the buffer's, a buffer that holds the range, a write inside the file, and a
/// read short at its end. A write's buffer is memory or, for a copy, another file.
pub(super) fn file_io(m: &mut Mock, file: u64, op: &IoOp) -> i64 {
    let read = op.opcode == libkern::IO_OPCODE_READ;
    let (need, buf_need) =
        if read { (Rights::MAP_READ, Rights::MAP_WRITE) } else { (Rights::MAP_WRITE, Rights::MAP_READ) };
    let f = match m.typed(file, need, KObjectType::FileObject) {
        Ok((f, _)) => f,
        Err(r) => return r,
    };
    let b = match m.get(op.buffer, buf_need) {
        Ok((b, _)) => b,
        Err(r) => return r,
    };
    let size = |o: &Object| match o {
        Object::File { size, .. } => Some(*size),
        Object::Memory(p) => Some(p.len() as u64),
        _ => None,
    };
    let held = match &m.objects[b] {
        Object::File { .. } if read => None,
        o => size(o),
    };
    let file_size = size(&m.objects[f]).unwrap_or(0);
    let past = |at: u64, len: u64, end: u64| at.checked_add(len).is_none_or(|e| e > end);
    let overlap =
        op.offset < op.buf_offset.saturating_add(op.length) && op.buf_offset < op.offset.saturating_add(op.length);
    let onto_itself = !read && f == b && overlap && op.offset != op.buf_offset;
    let refused = onto_itself
        || op.flags & !libkern::IO_NOCACHE != 0
        || op.length > libkern::MAX_USER_COPY_SIZE
        || held.is_none_or(|end| past(op.buf_offset, op.length, end))
        || (!read && past(op.offset, op.length, file_size));
    if refused {
        return e(KError::InvalidArgument);
    }
    let n = if read { op.length.min(file_size.saturating_sub(op.offset)) } else { op.length } as usize;
    let (from, from_at, to, to_at) =
        if read { (f, op.offset, b, op.buf_offset) } else { (b, op.buf_offset, f, op.offset) };
    let mut bytes = vec![0; n];
    for (k, byte) in bytes.iter_mut().enumerate() {
        let at = from_at as usize + k;
        *byte = pages(&m.objects[from]).0.get(at / PAGE).map_or(0, |page| page.0[at % PAGE]);
    }
    if let Object::Memory(p) | Object::File { bytes: p, .. } = &mut m.objects[to] {
        for (k, &byte) in bytes.iter().enumerate() {
            let at = to_at as usize + k;
            p.0[at / PAGE].0[at % PAGE] = byte;
        }
    }
    let never = read && m.reads_never;
    let short = if read { core::mem::take(&mut m.read_short) } else { 0 };
    m.po(Po { ready: !never, never, status: 0, result: (n as u64).saturating_sub(short), lookup: None })
}

fn pages(o: &Object) -> &Pages {
    match o {
        Object::Memory(p) | Object::File { bytes: p, .. } => p,
        _ => unreachable!("a buffer checked to be memory or a file"),
    }
}
