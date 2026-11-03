//! The device layer's calls into libos, behind a seam a host test scripts and plays the device through.

use core::cell::RefCell;
use libkern::IoOp;
use libos::{Handle, MapReadWrite, Mapping, Memory, Op, ReadWrite, Ref, Resource, Result};

pub type Device<'a> = Ref<'a, Resource, ReadWrite>;
pub type Object = Handle<Memory, MapReadWrite>;
pub type View = Mapping<MapReadWrite>;

/// How a submitted transfer ended: the device is done with the buffer, or the wait failed and it
/// may not be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waited {
    Finished { status: i32, moved: u64 },
    Unfinished,
}

/// How a submitted I/O ended, waiting for it: a completion, whatever its status, or a wait
/// refused for good, after which the device may still use the buffer.
pub fn finished(op: Op) -> Waited {
    match op.wait_out() {
        Ok(done) => Waited::Finished { status: done.status, moved: done.result },
        Err(_) => Waited::Unfinished,
    }
}

#[cfg(not(test))]
pub fn create(len: usize) -> Result<Object> {
    Object::create(len)
}

#[cfg(not(test))]
pub fn map(mem: &Object, len: usize) -> Result<View> {
    mem.map(len)
}

/// Submit `op` and wait for it. `scratch` is the buffer's view, unless it is the zero span.
#[cfg(not(test))]
pub fn transfer(device: Device<'_>, op: &IoOp, _scratch: Option<&RefCell<View>>) -> Result<Waited> {
    Ok(finished(Op::submit(device.raw(), op)?))
}

#[cfg(test)]
pub use fake::*;

#[cfg(test)]
mod fake {
    use super::*;
    use libos::Error;
    use std::collections::VecDeque;
    use std::vec::Vec;

    pub type Script = VecDeque<Option<i64>>;

    #[derive(Default)]
    pub struct Fake {
        /// The device: bytes a READ copies from and a WRITE copies to, grown on write.
        pub device: Vec<u8>,
        pub creates: Script,
        pub maps: Script,
        pub waits: Script,
        pub completions: VecDeque<(i32, u64)>,
    }

    thread_local! {
        static FAKE: RefCell<Fake> = RefCell::new(Fake::default());
    }

    /// Runs `f` on this thread's fake: the script to set up, and the record to read back.
    pub fn with<R>(f: impl FnOnce(&mut Fake) -> R) -> R {
        FAKE.with(|fake| f(&mut fake.borrow_mut()))
    }

    pub fn create(len: usize) -> Result<Object> {
        if let Some(Some(v)) = with(|f| f.creates.pop_front()) {
            return Err(Error::from_status(v as i32));
        }
        Object::create(len)
    }

    pub fn map(mem: &Object, len: usize) -> Result<View> {
        if let Some(Some(v)) = with(|f| f.maps.pop_front()) {
            return Err(Error::from_status(v as i32));
        }
        mem.map(len)
    }

    pub fn transfer(device: Device<'_>, op: &IoOp, scratch: Option<&RefCell<View>>) -> Result<Waited> {
        let (status, moved) = with(|f| f.completions.pop_front()).unwrap_or((0, op.length));
        libos::mock::script_next(status, moved, true);
        let waited = finished(Op::submit(device.raw(), op)?);
        let (off, len) = (op.offset as usize, op.length as usize);
        with(|f| {
            if f.device.len() < off + len {
                f.device.resize(off + len, 0);
            }
            match (scratch, op.opcode) {
                (Some(view), libkern::IO_OPCODE_READ) => {
                    view.borrow_mut().write_from(0, &f.device[off..off + len]).expect("the buffer holds the transfer");
                }
                (Some(view), _) => {
                    view.borrow().read_into(0, &mut f.device[off..off + len]).expect("the buffer holds the transfer");
                }
                (None, _) => f.device[off..off + len].fill(0),
            }
        });
        if with(|f| f.waits.pop_front()).flatten().is_some() {
            return Ok(Waited::Unfinished);
        }
        Ok(waited)
    }
}
