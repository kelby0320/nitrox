//! What the service says of each device, a line each.

use super::*;

/// Say what one device holds, and whose it is. How a filesystem was left is not said while it is
/// mounted writable, as the table's `clean` is `Null` then. **A device not mounted says why**
/// ([`mounts::unmounted_why`]). The QEMU gates match these lines.
pub(super) fn report(d: &Device, mounts: &[Mounted], all: &[Device]) {
    let r = &d.record;
    let mount = mounts.iter().find(|m| m.device == r.id);
    let mut line = Line::new();
    line.s(b"storage-service: ").s(table::name(r).as_bytes()).s(b" (");
    line.s(match r.kind() {
        DeviceKind::Disk => b"disk".as_slice(),
        DeviceKind::Partition => b"partition",
        DeviceKind::RamDisk => b"ramdisk",
        _ => b"device",
    });
    if !r.name().is_empty() {
        line.s(b" ").untrusted(r.name());
    }
    line.s(b"): ");
    match &d.found {
        Found::Ext4 { label, .. } | Found::Fat { label, .. } => {
            line.s(d.found.filesystem().unwrap_or("").as_bytes());
            if !label.is_empty() {
                line.s(b" '").untrusted(label.as_bytes()).s(b"'");
            }
            if !mount.is_some_and(|m| m.mode == Mode::Rw) {
                line.s(match d.found.clean() {
                    Some(true) => b", left clean".as_slice(),
                    Some(false) => b", not left clean",
                    None => b", state unreadable",
                });
            }
        }
        Found::Nothing => {
            line.s(b"no filesystem");
        }
        Found::Ejected => {
            line.s(b"ejected");
        }
    }
    if let Some(m) = mount {
        line.s(b"; ").s(if m.by == By::Init { b"init's".as_slice() } else { b"mounted" });
        line.s(b" at ").untrusted(m.at.as_bytes()).s(if m.mode == Mode::Ro { b" (ro)".as_slice() } else { b" (rw)" });
        if let Some(why) = d.found.read_only() {
            line.s(b": ").s(why.as_bytes());
        }
    } else if let Some(why) = mounts::unmounted_why(d, all) {
        // **Said, not left to an absence**: a gate can then match the reason a device was passed
        // over. The table's `note` gives the same words.
        line.s(b"; ").s(why.as_bytes());
    }
    line.end();
}
