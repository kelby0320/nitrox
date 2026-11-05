//! `libfs`, the file library programs read and write through: what it refuses, and what it leaves.

use super::*;
use libfs::{FileError, SaveError};
use libos::{MapReadWrite, fs};

/// The root filesystem's files something still holds, after a sync lets go of the dirty ones.
fn held(root: Root<'_>) -> i64 {
    let _ = block_on(root.flush("/system"));
    root.held("/system").map_or_else(|e| i64::from(e.code().as_i32()), |n| n as i64)
}

/// **libfs reads no directory as a file, and lets go of a file it refuses**: a directory has no
/// size and is not found, and a file past `MAX_COPY` is refused without its handle kept.
pub(super) fn libfs_refuses_a_directory_and_a_large_file_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: libfs refusals FAIL: ").s(why).end();
        false
    };
    if libfs::file_len(root, "/system/profiles").is_some() {
        return fail(b"a directory has a size");
    }
    if libfs::read(root, "/system/profiles") != Err(FileError::NotFound) {
        return fail(b"a directory was read as a file");
    }
    const LARGE: &str = "/system/libfs-too-large";
    let Ok(h) = block_on(fs::create::<_, MapReadWrite>(root, LARGE, libfs::MAX_COPY + 1, RW)) else {
        return fail(b"the large file was not created");
    };
    drop(h);
    let before = held(root);
    let refused = libfs::read(root, LARGE);
    let after = held(root);
    let removed = libfs::remove_entry(root, LARGE).is_ok();
    if refused != Err(FileError::TooLarge) {
        return fail(b"a file past MAX_COPY was not refused TooLarge");
    }
    if after != before {
        Line::new().s(b"boot-probe: libfs refusals: held ").i(before).s(b" then ").i(after).end();
        return fail(b"a refused read kept its file");
    }
    if !removed {
        return fail(b"the large file was not removed");
    }
    log::write(b"boot-probe: libfs reads no directory as a file, and lets go of one it refuses ok\n");
    true
}

/// **A save that cannot be written leaves the file as it was**, and what was in the way of its
/// temporary; one that can replaces the file whole and leaves no temporary beside it.
pub(super) fn save_leaves_the_file_when_it_fails_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: save FAIL: ").s(why).end();
        false
    };
    const PATH: &str = "/system/libfs-save";
    const OLD: &[u8] = b"what a failed save keeps\n";
    let temp = alloc::format!("{PATH}{}", libfs::SAVE_SUFFIX);
    let wrote = libfs::write(root, PATH, OLD).is_ok();
    // A directory where the temporary goes, so the temporary cannot be written.
    let blocked = wrote && libfs::make_dir(root, &temp).is_ok();
    let refused = blocked && matches!(libfs::save(root, PATH, b"new\n"), Err(SaveError::Write(_)));
    let kept = libfs::read(root, PATH).is_ok_and(|b| b == OLD);
    let left_alone = libfs::is_directory(root, &temp);
    let unblocked = !left_alone || libfs::remove_recursive(root, &temp, &mut |_, _| {}).is_ok();
    let saved = unblocked && libfs::save(root, PATH, b"new\n").is_ok();
    let replaced = libfs::read(root, PATH).is_ok_and(|b| b == b"new\n");
    let no_temp = libfs::file_len(root, &temp).is_none() && !libfs::is_directory(root, &temp);
    let _ = libfs::remove_entry(root, PATH);
    if !blocked {
        return fail(b"the file or the directory in the way was not made");
    }
    if !refused {
        return fail(b"a save whose temporary could not be written did not fail");
    }
    if !kept {
        return fail(b"a failed save changed the file");
    }
    if !left_alone {
        return fail(b"a failed save removed a directory it did not make");
    }
    if !saved || !replaced {
        return fail(b"a save did not replace the file");
    }
    if !no_temp {
        return fail(b"a save left its temporary");
    }
    log::write(b"boot-probe: a failed save leaves the file as it was, and a save replaces it whole ok\n");
    true
}
