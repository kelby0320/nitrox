use super::*;

/// **FAT as the protocol sees it**: what `libfsserver`'s loop calls, each the library's own method.
impl<R: BlockReader + BlockWriter> Volume for Fat<'_, R> {
    const NAME: &'static [u8] = b"fs-server-fat";
    const KIND: &'static [u8] = b"fat";
    type Unservable = Unservable;

    fn read_only(&self) -> bool {
        self.r.read_only()
    }

    fn check(&self) -> Result<(), Unservable> {
        Fat::check(self)
    }

    fn state_unwritable() -> Unservable {
        Unservable::StateUnwritable
    }

    fn was_left_clean(&self) -> Result<bool, FsError> {
        Fat::was_left_clean(self)
    }

    fn mark_mounted(&self) -> Result<(), FsError> {
        Fat::mark_mounted(self)
    }

    fn mark_clean(&self) -> Result<(), FsError> {
        Fat::mark_clean(self)
    }

    fn refusing_changes(&self) -> bool {
        self.broken.get()
    }

    fn map_file(&self, path: &[u8], runs: &mut [BlockRun]) -> Result<Mapped, FsError> {
        Fat::map_file(self, path, runs)
    }

    fn read_file(&self, path: &[u8], out: &mut [u8]) -> Result<usize, FsError> {
        Fat::read_file(self, path, out)
    }

    fn read_file_range(&self, path: &[u8], offset: u64, len: usize, out: &mut [u8]) -> Result<usize, FsError> {
        Fat::read_file_range(self, path, offset, len, out)
    }

    fn create_file(&self, parent: &[u8], name: &[u8], now: i64) -> Result<(), FsError> {
        Fat::create_file(self, parent, name, now)
    }

    fn grow_file(&self, path: &[u8], size: usize, now: i64) -> Result<u64, FsError> {
        Fat::grow_file(self, path, size, now)
    }

    fn truncate_file(&self, path: &[u8], size: usize, now: i64) -> Result<Option<u64>, FsError> {
        Fat::truncate_file(self, path, size, now)
    }

    fn resolve_dir(&self, path: &[u8]) -> Result<u64, FsError> {
        Fat::resolve_dir(self, path)
    }

    fn read_dir(&self, dir: u64, cursor: u64, emit: impl FnMut(&DirEntry) -> bool) -> Result<u64, FsError> {
        Fat::read_dir(self, dir, cursor, emit)
    }

    fn mkdir_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        Fat::mkdir_at(self, dir, name, now)
    }

    fn unlink_at(&self, dir: u64, name: &[u8], now: i64) -> Result<Option<u64>, FsError> {
        Fat::unlink_at(self, dir, name, now)
    }

    fn rmdir_at(&self, dir: u64, name: &[u8], now: i64, freeing: impl FnOnce(u64)) -> Result<(), FsError> {
        Fat::rmdir_at(self, dir, name, now, freeing)
    }

    fn touch_at(&self, dir: u64, name: &[u8], now: i64) -> Result<(), FsError> {
        Fat::touch_at(self, dir, name, now)
    }

    fn rename_at(&self, dir: u64, old: &[u8], new: &[u8], now: i64) -> Result<(), FsError> {
        Fat::rename_at(self, dir, old, new, now)
    }

    fn rename_path(&self, old: &[u8], new: &[u8], replace: bool, now: i64) -> Result<Option<u64>, FsError> {
        Fat::rename_path(self, old, new, replace, now)
    }

    fn touch_file(&self, id: u64, now: i64) -> Result<(), FsError> {
        Fat::touch_file(self, id, now)
    }

    fn release(&self, id: u64, now: i64) -> Result<(), FsError> {
        Fat::release(self, id, now)
    }
}
