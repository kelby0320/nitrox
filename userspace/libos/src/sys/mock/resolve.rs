//! What a resolve does beyond resolving, as a filesystem server and the kernel answer it.

use super::*;

/// As the kernel's: a plain lookup, or what `op` says done there.
pub fn ns_lookup(ns: u64, path: &[u8], rights: u64, op: Resolve<'_>) -> i64 {
    match op {
        Resolve::Plain => plain_lookup(ns, path, rights),
        Resolve::Size(verb, size) => file_resize(verb, ns, path, rights, size),
        Resolve::Rename { to, flags } => file_rename(ns, path, to, flags as u64),
        Resolve::Flush => mount_answer(ns, path, false),
        Resolve::Held => mount_answer(ns, path, true),
    }
}

fn mount_answer(ns: u64, path: &[u8], held: bool) -> i64 {
    with(|m| {
        if let Err(r) = m.namespace(ns, Rights::LOOKUP) {
            return r;
        }
        if !held && m.busy_flushes > 0 {
            m.busy_flushes -= 1;
            return e(KError::WouldBlock);
        }
        let (status, result) = match m.mounts.get(path) {
            Some(&(_, still)) if held => (0, still as u64),
            Some(&(written, _)) => (m.fail_sync.take().unwrap_or(0), written as u64),
            None => (KError::NotFound as i32, 0),
        };
        let never = m.flush_never && !held;
        m.po(Po { ready: !never, never, status, result, lookup: None })
    })
}

/// The file verbs, as a filesystem server mounted over the whole namespace would answer them: a
/// create is not exclusive, a grow never shrinks, a truncate never grows.
fn file_resize(verb: Resize, ns: u64, path: &[u8], rights: u64, size: u64) -> i64 {
    with(|m| {
        if size > u32::MAX as u64 {
            return e(KError::TooLarge);
        }
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(path) {
            return e(KError::InvalidArgument);
        }
        let found = match (m.file_at(ns, path), verb) {
            (None, Resize::Create) => {
                let obj = m.object(Object::File { bytes: Pages::new(FILE_ROOM), size: 0 });
                let all = Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT | Rights::TRANSFER | Rights::DUPLICATE;
                m.bindings(ns).push(Binding::new(path, obj, all.bits()));
                Some((obj, all.bits()))
            }
            (None, _) => None,
            (Some(got), _) => Some(got),
        };
        let Some((obj, bound)) = found else { return m.lookup_po(Err(KError::NotFound)) };
        let ignored = verb == Resize::Truncate && core::mem::take(&mut m.ignore_truncate);
        if let Object::File { size: now, .. } = &mut m.objects[obj] {
            *now = match verb {
                Resize::Create | Resize::Grow => (*now).max(size),
                Resize::Truncate if ignored => *now,
                Resize::Truncate => (*now).min(size),
            };
        }
        m.lookup_po(Ok((obj, bound & rights)))
    })
}

/// An existing destination is refused `AlreadyExists` unless `flags` asks to replace it, as a
/// filesystem server answers (`librsproto`'s `RENAME_REPLACE`, bit 0).
fn file_rename(ns: u64, from: &[u8], to: &[u8], flags: u64) -> i64 {
    with(|m| {
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(from) || !valid_path(to) {
            return e(KError::InvalidArgument);
        }
        let b = m.bindings(ns);
        let status = match b.iter().position(|x| x.path == from) {
            Some(_) if flags & 1 == 0 && from != to && b.iter().any(|x| x.path == to) => KError::AlreadyExists as i32,
            Some(i) => {
                b.retain(|x| x.path != to);
                let i = b.iter().position(|x| x.path == from).unwrap_or(i);
                b[i].path = to.to_vec();
                0
            }
            None => KError::NotFound as i32,
        };
        let lookup = (status == 0 && core::mem::take(&mut m.rename_handle))
            .then(|| (m.object(Object::Memory(Pages::new(PAGE))), Rights::MAP_READ.bits()));
        m.po(Po { ready: true, never: false, status, result: 0, lookup })
    })
}
