//! Which image a stage spawns: the program itself, or the interpreter its file asks for.

use super::{
    NitroxHost, RIGHT_MAP_READ, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, syscall1,
    syscall2, syscall4,
};
use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::HandleInfo;
use libkern::handle::{KOBJ_FILE_OBJECT, KOBJ_MEMORY_OBJECT};
use nxsh::exec::{HEAD_LEN, Runs, is_path_head, launch, what_runs};
use nxsh::host::StageSpec;

fn close(h: u64) {
    // SAFETY: closing a handle this shell owns.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

impl NitroxHost {
    /// The image a stage spawns and the argv it is sent: the stage's own program if it is ELF,
    /// else the interpreter or `nxsh` that [`what_runs`] picks, run on the file.
    ///
    /// Every stage is read, a `/bin` name included, since the file is what says; the cost is one
    /// map of at most [`HEAD_LEN`] bytes per stage, against a spawn that maps the whole image.
    pub(crate) fn resolve_stage(&mut self, spec: &StageSpec) -> Result<(u64, Vec<String>), String> {
        let h = self.resolve_program(&spec.program)?;
        let path =
            if is_path_head(&spec.program) { spec.program.clone() } else { alloc::format!("/bin/{}", spec.program) };
        let runs = match self.read_head(h) {
            Some(head) => what_runs(&head).map_err(|e| alloc::format!("`{path}`: {e}")),
            None => Err(alloc::format!("`{}` is not a program", spec.program)),
        };
        let l = match runs.as_ref().map(|r| launch(r, &path, &spec.argv)) {
            Ok(None) => return Ok((h, spec.argv.clone())),
            Ok(Some(l)) => l,
            Err(e) => {
                close(h);
                return Err(e.clone());
            }
        };
        close(h);
        let image = self.resolve_program(&l.program)?;
        if !matches!(self.read_head(image).as_deref().map(what_runs), Some(Ok(Runs::Program))) {
            close(image);
            return Err(alloc::format!("`{}` runs `{}`, which is not itself a program", path, l.program));
        }
        Ok((image, l.argv))
    }

    /// The first [`HEAD_LEN`] bytes of the file `h`, or `None` if `h` is not a readable file.
    ///
    /// Only a file or a memory object can be an image: a directory resolves to something else that
    /// stats with no size, and would otherwise run as an empty script.
    fn read_head(&mut self, h: u64) -> Option<Vec<u8>> {
        let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
        // SAFETY: a correctly sized `HandleInfo` out-param for a handle we own.
        let r = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
        if r != 0 || !matches!(info.object_type, KOBJ_FILE_OBJECT | KOBJ_MEMORY_OBJECT) {
            return None;
        }
        let len = info.size.min(HEAD_LEN as u64);
        if len == 0 {
            return Some(Vec::new());
        }
        // SAFETY: mapping a readable file handle for no more than its own length.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, len, RIGHT_MAP_READ) };
        if addr < 0 {
            return None;
        }
        // SAFETY: `addr` is a live read-only mapping of `len` bytes.
        let head = unsafe { core::slice::from_raw_parts(addr as *const u8, len as usize) }.to_vec();
        // SAFETY: unmapping what we just created.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, len) };
        Some(head)
    }
}
