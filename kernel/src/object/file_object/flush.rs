//! What a flush asks of a file: the mount it counts against, and the id that names it after.

use super::*;

impl FileObject {
    /// **The mount whose flush budget this file's flush counts in**: its producer's registration, for
    /// a file with a device behind it, forgotten or not; `None` for one with none.
    pub fn flush_owner(&self) -> Option<Ref<UserspaceServerReg>> {
        match &self.producer {
            Producer::FsServerBlocks { reg, .. } => Some(reg.clone()),
            _ => None,
        }
    }

    /// The `(registration, file id)` naming this file to its filesystem server, for a cached
    /// Model A file; `None` for any other. For the post-writeback `File::Touch`, which must reach
    /// a server *without* a resolve — by id, since a cached object outlives the resolve that named
    /// it. The clone pins the registration across the send.
    pub fn touch_target(file_obj: &Ref<FileObject>) -> Option<(Ref<UserspaceServerReg>, u64)> {
        // A forgotten file's id may already name another file.
        if file_obj.inner.lock().dead {
            return None;
        }
        match &file_obj.producer {
            Producer::FsServerBlocks { reg, file_id, .. } if *file_id != 0 => Some((reg.clone(), *file_id)),
            _ => None,
        }
    }
}
