//! Random bytes from the kernel's generator, read through the node a namespace binds at
//! `/dev/entropy`.

use libkern::{IO_OPCODE_READ, IoOp, KError, Rights};

use crate::error::Result;
use crate::exec::{Op, block_on};
use crate::handle::{CanLookup, Handle, MapReadWrite, Memory, Namespace, ReadOnly, Ref, Resource};
use crate::memory::Mapping;
use crate::time::Deadline;
use crate::wait::WaitSet;

/// The most one read fills, as the kernel answers.
const READ_MAX: usize = 256;

/// How long a read pauses before reading again when the kernel had no place for it to wait: a tick.
const NO_PLACE_PAUSE_NS: u64 = 10_000_000;

/// Where a namespace binds the generator.
pub const PATH: &str = "/dev/entropy";

/// A source of random bytes: the node a namespace binds at [`PATH`], and a page it reads into.
pub struct Source {
    node: Handle<Resource, ReadOnly>,
    page: Handle<Memory, MapReadWrite>,
    view: Mapping<MapReadWrite>,
}

impl Source {
    /// The source `ns` binds at [`PATH`]: `NotFound` if it binds none.
    pub fn open<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>) -> Result<Source> {
        Source::from_handle(block_on(ns.into().lookup(PATH, Rights::READ))?)
    }

    /// A source this process was handed: the node a lookup of [`PATH`] answers.
    pub fn from_handle(node: Handle<Resource, ReadOnly>) -> Result<Source> {
        let page = Handle::<Memory, MapReadWrite>::create(READ_MAX)?;
        let view = page.map(READ_MAX)?;
        Ok(Source { node, page, view })
    }

    /// Fill `buf` with random bytes. Before the generator is seeded, which only a machine with no
    /// hardware source sees, a read waits for the seeding, answers nothing, and is read again; one
    /// the kernel had no place to keep waiting, past its few for the machine, pauses and reads
    /// again. A second empty answer in a row is `IoError`: the generator answers one only as it
    /// seeds, so what [`open`](Self::open) found is not it.
    pub fn fill(&self, buf: &mut [u8]) -> Result<()> {
        let (mut at, mut waited) = (0, false);
        while at < buf.len() {
            let want = (buf.len() - at).min(READ_MAX);
            match block_on(self.node.read(&self.page, 0, 0, want as u64)) {
                Ok(0) if waited => return Err(KError::IoError.into()),
                Ok(0) => waited = true,
                Ok(n) => {
                    let n = n.min(want);
                    self.view.read_into(0, &mut buf[at..at + n])?;
                    (at, waited) = (at + n, false);
                }
                Err(e) if e.code() == KError::WouldBlock => crate::time::sleep(NO_PLACE_PAUSE_NS)?,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// [`fill`](Self::fill) for a caller that cannot wait, such as a server whose clients wait on
    /// it: refused `WouldBlock` before the generator is seeded, when what it filled is not to be
    /// used. **Each call refused holds one of the kernel's places for a reader waiting on the
    /// seeding** until it seeds, as closing the read's operation does not give it back: there are
    /// four for the machine, and a [`fill`](Self::fill) that finds them taken pauses and reads
    /// again.
    pub fn try_fill(&self, buf: &mut [u8]) -> Result<()> {
        let mut at = 0;
        while at < buf.len() {
            let want = (buf.len() - at).min(READ_MAX);
            let page = Handle::<Memory, MapReadWrite>::create(READ_MAX)?;
            let op = IoOp {
                opcode: IO_OPCODE_READ,
                flags: 0,
                buffer: page.raw().0,
                buf_offset: 0,
                offset: 0,
                length: want as u64,
            };
            let mut read = Op::submit(self.node.raw(), &op)?;
            let done = {
                let mut set = WaitSet::<1>::new();
                set.add_op(&read)?;
                set.wait(Deadline::POLL).ok().and_then(|signalled| read.completion_in(&signalled))
            };
            let n = match done {
                Some(done) => (done?.result as usize).min(want),
                None => 0,
            };
            if n == 0 {
                // Unseeded: the read waits for the seeding, and its page is let go unwaited.
                read.abandon(page);
                return Err(KError::WouldBlock.into());
            }
            drop(read);
            page.map(READ_MAX)?.read_into(0, &mut buf[at..at + n])?;
            at += n;
        }
        Ok(())
    }
}

/// [`Source::fill`] from the source `ns` binds.
pub fn fill<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, buf: &mut [u8]) -> Result<()> {
    Source::open(ns)?.fill(buf)
}

/// [`Source::try_fill`] from the source `ns` binds.
pub fn try_fill<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, buf: &mut [u8]) -> Result<()> {
    Source::open(ns)?.try_fill(buf)
}

#[cfg(test)]
mod tests;
