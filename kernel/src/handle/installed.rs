//! The handles a syscall installs, closed again unless it succeeds.

use crate::libkern::handle::{RawHandle, Rights};
use crate::object::ObjectRef;

use super::table::{HandleError, HandleTable};

/// The handles installed so far in one process's table, **closed on drop unless committed**: a
/// syscall that installs several and fails at any later step leaves none of them behind.
#[must_use = "dropping it closes every handle it installed"]
pub struct Installed<'t, const N: usize> {
    table: &'t HandleTable,
    pid: u32,
    handles: [RawHandle; N],
    len: usize,
}

impl<'t, const N: usize> Installed<'t, N> {
    /// None installed yet in `pid`'s table.
    pub fn new(table: &'t HandleTable, pid: u32) -> Self {
        Self { table, pid, handles: [RawHandle::NULL; N], len: 0 }
    }

    /// Install `object` with `rights`, to be closed with the rest unless committed. A refusal,
    /// or a set already holding `N`, hands the reference back.
    pub fn install(&mut self, object: ObjectRef, rights: Rights) -> Result<RawHandle, (HandleError, ObjectRef)> {
        if self.len == N {
            return Err((HandleError::OutOfHandles, object));
        }
        let h = self.table.install(self.pid, object, rights)?;
        self.handles[self.len] = h;
        self.len += 1;
        Ok(h)
    }

    /// Keep every handle: the syscall has succeeded.
    pub fn commit(mut self) {
        self.len = 0;
    }
}

impl<const N: usize> Drop for Installed<'_, N> {
    /// Close each handle, releasing its reference here: outside the table's lock, as every drop of
    /// one is.
    fn drop(&mut self) {
        for &h in &self.handles[..self.len] {
            drop(self.table.close(h, self.pid));
        }
    }
}

#[cfg(test)]
mod tests;
