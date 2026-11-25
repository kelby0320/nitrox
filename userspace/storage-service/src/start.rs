//! The boot's start, once the Ready is answered: the session endpoint first, then every device.

use super::*;

/// Twice the kernel's 8 forwarded lookups a registration holds: only a direct sender meets it.
const KEPT_MAX: usize = 16;

impl Service {
    /// **Mint the session endpoint before a disk is read**, which `service-mgr` asks for next. What
    /// else is asked is kept for when the mounts are made, so no client reads half a table.
    pub(super) fn mint_before_reading(&mut self, serve: &Chan) -> Vec<Message> {
        let mut kept = Vec::new();
        let by = within(DERIVE_TIMEOUT_NS);
        while wait_until(serve, by) {
            match recv_request(serve) {
                Ok(Some(m)) if asked(&m) == Asked::SessionEndpoint => {
                    self.mint_session_endpoint(serve, m.request_id);
                    break;
                }
                Ok(Some(m)) if kept.len() == KEPT_MAX => fail(serve, m.op, m.request_id, KError::WouldBlock),
                Ok(Some(m)) => kept.push(m),
                Ok(None) => {}
                Err(()) => self.stop(CLOSED, 1),
            }
        }
        kept
    }

    /// Read each device the boot holds, as its server would, and mount what the boot's rules say.
    // TODO(storage-start-unserved): a shutdown or a fifth resolve meanwhile waits for this.
    pub(super) fn read_and_mount(&mut self, held: Vec<(DeviceRecord, Node)>) {
        Line::new().s(b"storage-service: ").u(held.len() as u64).s(b" block device(s)").end();
        for (record, node) in held {
            let io = DeviceIo::new(&node, &record, &self.scratch);
            self.devices.push(Device { found: probe_record(&io, &record), record });
            self.nodes.push((record.id, node));
        }
        // Every node is kept, mounted or not, for an administrator's mount and an unmount's flush.
        let mut plan = automount(&self.devices, &self.init, self.live, self.init_known);
        plan.truncate(MAX_MOUNTS);
        for (id, node) in &self.nodes {
            let Some(p) = plan.iter().find(|p| p.device == *id) else {
                continue;
            };
            match mount(&self.root, p, node) {
                Ok(m) => self.mounted.push(m),
                Err(why) => Line::new()
                    .s(b"storage-service: ")
                    .untrusted(p.label.as_bytes())
                    .s(b" did not mount: ")
                    .s(why)
                    .end(),
            }
        }
    }
}
