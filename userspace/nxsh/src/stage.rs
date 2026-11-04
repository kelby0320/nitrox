//! Which image a stage spawns: the program itself, or the interpreter its file asks for.

use super::NitroxHost;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use libkern::handle::{KOBJ_FILE_OBJECT, KOBJ_MEMORY_OBJECT};
use libos::{File, Handle, MapRead, Ref};
use libstream::wire::Record;
use nxsh::exec::{HEAD_LEN, Runs, is_path_head, launch, what_runs};
use nxsh::host::StageSpec;
use nxsh::paths::program_path;

/// A pipeline's images, each closed once its stage is spawned and the rest on any return: the
/// spawner owns an image (`process-spawn-args.md`), and one left open holds its file, so a stick a
/// program ran from could not be ejected.
pub(crate) struct Images(pub(crate) Vec<Option<Handle<File, MapRead>>>);

impl Images {
    /// Close stage `i`'s image, which its spawn has read.
    pub(crate) fn release(&mut self, i: usize) {
        if let Some(slot) = self.0.get_mut(i) {
            *slot = None;
        }
    }

    /// Stage `i`'s image, if it is still open.
    pub(crate) fn image(&self, i: usize) -> Option<Ref<'_, File, MapRead>> {
        self.0.get(i)?.as_ref().map(|h| h.as_ref())
    }
}

impl NitroxHost {
    /// The image a stage spawns and the argv it is sent: the stage's own program if it is ELF,
    /// else the interpreter or `nxsh` that [`what_runs`] picks, run on the file.
    ///
    /// Every stage is read, a `/bin` name included, since the file is what says; the cost is one
    /// map of at most [`HEAD_LEN`] bytes per stage, against a spawn that maps the whole image.
    pub(crate) fn resolve_stage(
        &mut self,
        spec: &StageSpec,
        env: &Record,
    ) -> Result<(Handle<File, MapRead>, Vec<String>), String> {
        let h = self.resolve_program(&spec.program)?;
        // A bare name found at all is `/bin/{name}` while `PROGRAM_DIRS` has one entry; a second
        // entry must hand on the path its lookup found.
        let path =
            if is_path_head(&spec.program) { spec.program.clone() } else { alloc::format!("/bin/{}", spec.program) };
        let runs = match read_head(&h) {
            Some(head) => what_runs(&head).map_err(|e| alloc::format!("`{path}`: {e}")),
            None => Err(alloc::format!("`{}` is not a program", spec.program)),
        };
        let l = match runs.as_ref().map(|r| launch(r, &path, &spec.argv)) {
            Ok(None) => return Ok((h, spec.argv.clone())),
            Ok(Some(l)) => l,
            Err(e) => return Err(e.clone()),
        };
        drop(h);
        let image =
            self.resolve_program(&program_path(&l.program, env).map_err(|e| alloc::format!("`{path}`: {e}"))?)?;
        if !matches!(read_head(&image).as_deref().map(what_runs), Some(Ok(Runs::Program))) {
            return Err(alloc::format!("`{}` runs `{}`, which is not itself a program", path, l.program));
        }
        Ok((image, l.argv))
    }
}

/// The first [`HEAD_LEN`] bytes of the file `h`, or `None` if `h` is not a readable file.
///
/// Read through a mapping, so a fill that fails is a fault that suspends the shell rather than
/// an error here, as `read_file`'s is — `TODO(mapped-read-faults)`.
///
/// Only a file or a memory object can be an image: a directory resolves to something else that
/// stats with no size, and would otherwise run as an empty script.
fn read_head(h: &Handle<File, MapRead>) -> Option<Vec<u8>> {
    let info = h.stat().ok()?;
    if !matches!(info.object_type, KOBJ_FILE_OBJECT | KOBJ_MEMORY_OBJECT) {
        return None;
    }
    let len = info.size.min(HEAD_LEN as u64) as usize;
    if len == 0 {
        return Some(Vec::new());
    }
    let map = h.map(len).ok()?;
    let mut head = vec![0; len];
    map.read_into(0, &mut head).ok()?;
    Some(head)
}
