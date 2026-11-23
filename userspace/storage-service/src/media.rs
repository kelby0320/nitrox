use super::*;

impl Service {
    /// `Eject` `m`, from media session `ch`: the unmount chain, held check included.
    pub(super) fn eject(&mut self, ch: &Chan, m: Message) {
        let refuse = |err: KError, why: &[u8]| {
            let _ = server::reply_error(ch, m.op, m.request_id, err, why);
        };
        if m.op != OP_STORAGE_EJECT {
            return refuse(KError::Unsupported, b"a media session carries Eject alone");
        }
        let Ok(name) = core::str::from_utf8(&m.body) else {
            return refuse(KError::InvalidArgument, b"a name that is not UTF-8");
        };
        let (disk, names) = match storage_service::mounts::eject(&self.devices, &self.all_mounts(), name) {
            Ok(e) => (e.disk, e.names),
            Err(r) => return refuse(r.kerror(), r.why()),
        };
        // **The whole stick or none of it**: every mount is synced and asked whether a file is
        // held before any is unmounted, so a held file leaves no half-ejected stick.
        for n in &names {
            let Some(x) = self.mounted.iter().find(|x| &x.label == n) else { continue };
            if !sync_mount(&x.ns) {
                return refuse(KError::IoError, b"its files could not all be written back");
            }
            if held_files(n, &x.ns) != 0 {
                return refuse(KError::WouldBlock, b"a file on it is still open or mapped");
            }
        }
        for n in &names {
            // A file opened since the check refuses here, and what was unmounted before it stays
            // unmounted: the refusal says the stick cannot be pulled, which is what matters.
            if let Err((err, why)) = self.unmount(n, true) {
                return refuse(err, why);
            }
        }
        let said = names.join(", ");
        Line::new().s(b"storage-service: ejected ").untrusted(said.as_bytes()).s(b", safe to remove").end();
        mounts::mark_ejected(&mut self.devices, disk);
        // The reply names every mount of the drive, a name per line (`rsproto-storage-ops.md`).
        // **Before the stop**, which a stick that never answers holds for a command's deadline:
        // the eject is done either way, as everything on it is unmounted.
        let reply = names.join("\n");
        let _ = server::send(ch, m.op, m.request_id, RS_FLAG_REPLY, reply.as_bytes(), &mut Carried::new());
        let said_stop = mounts::stop_said(self.node_of(disk).map_or(Err(KError::NotFound), eject_medium));
        Line::new().s(b"storage-service: ").untrusted(said.as_bytes()).s(b" ").s(said_stop.as_bytes()).end();
    }
}
