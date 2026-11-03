//! What a process is handed at entry: its four bootstrap registers, owned and typed.

use libkern::{RawHandle, Rights};

use crate::handle::{Handle, Namespace, Notify, NsReadOnly, Only};

/// A process's bootstrap: its notification queue, its root namespace, the first handle it was
/// granted and its `arg0`. Whatever is not taken from it is closed when it drops.
pub struct Start {
    /// Where the kernel tells this process of its children and its own faults.
    pub notify: Option<Handle<Notify, Only>>,
    /// The namespace this process resolves names in, `LOOKUP` only.
    pub root: Option<Handle<Namespace, NsReadOnly>>,
    handle0: u64,
    /// The word its spawner chose.
    pub arg0: u64,
}

impl Start {
    /// Take `_start`'s four registers: `rdi`, `rsi`, `rdx` and `rcx` as the kernel set them.
    ///
    /// # Safety
    /// Called once, with the registers as they were at entry: each handle this process's, and
    /// wrapped nowhere else.
    pub unsafe fn from_registers(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> Start {
        Start {
            notify: (rdi != 0).then(|| Handle::fresh(rdi, Rights::WAIT)),
            root: (rsi != 0).then(|| Handle::fresh(rsi, Rights::LOOKUP)),
            handle0: rdx,
            arg0: rcx,
        }
    }

    /// The first handle this process was granted, as a `T` of mode `M`: `None` if there was none,
    /// or it was taken.
    pub fn take_handle0<T, M>(&mut self) -> Option<Handle<T, M>> {
        let raw = core::mem::take(&mut self.handle0);
        (raw != 0).then(|| Handle::fresh(raw, Rights::empty()))
    }

    /// The first handle, still `Start`'s, for code that takes it raw.
    pub fn handle0(&self) -> RawHandle {
        RawHandle(self.handle0)
    }
}

impl Drop for Start {
    fn drop(&mut self) {
        if self.handle0 != 0 {
            crate::sys::handle_close(self.handle0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, sys};

    #[test]
    fn a_start_owns_its_handles_and_closes_what_was_not_taken() {
        sys::reset();
        let (a, b) = Handle::<Channel, Only>::pair(1).unwrap();
        let (notify, root) = (sys::notify(&[]), sys::with_rights(1, Rights::READ));
        // SAFETY: fresh handles of the mock's, wrapped nowhere else.
        let mut start = unsafe { Start::from_registers(notify, root, a.into_raw().0, 9) };
        assert_eq!(start.arg0, 9);
        let ch: Handle<Channel, Only> = start.take_handle0().unwrap();
        assert!(start.take_handle0::<Channel, Only>().is_none(), "taken once");
        drop(start);
        assert!(!sys::is_open(notify) && !sys::is_open(root) && sys::is_open(ch.raw().0));
        // SAFETY: as above.
        let start = unsafe { Start::from_registers(0, 0, b.raw().0, 0) };
        let b = b.into_raw();
        drop(start);
        assert!(!sys::is_open(b.0), "a handle0 not taken is closed");
    }

    #[test]
    fn a_register_of_zero_is_no_handle() {
        // SAFETY: no handles.
        let start = unsafe { Start::from_registers(0, 0, 0, 0) };
        assert!(start.notify.is_none() && start.root.is_none() && start.handle0().is_null());
    }
}
