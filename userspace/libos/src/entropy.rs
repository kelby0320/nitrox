//! Random bytes from the kernel's generator.

use libkern::Rights;

use crate::error::{Result, check};
use crate::exec::{Op, block_on};
use crate::handle::{Handle, ReadOnly, Resource};
use crate::sys;

/// The most one read fills.
const READ_MAX: usize = 256;

/// Fill `buf` with random bytes. Before the generator is seeded, which only a machine with no
/// hardware source sees, each read waits for the seeding and reads again.
pub fn fill(buf: &mut [u8]) -> Result<()> {
    // TODO(surface-shape): a handle made for the call, until entropy is a node every namespace binds.
    let source: Handle<Resource, ReadOnly> = Handle::fresh(check(sys::entropy_create())?, Rights::empty());
    for chunk in buf.chunks_mut(READ_MAX) {
        loop {
            match check(sys::entropy_read(source.raw().0, chunk))? {
                0 => break,
                po => block_on(Op::status(po))?,
            };
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fill_reads_in_pieces_and_closes_its_source() {
        sys::reset();
        let mut buf = [0u8; 600];
        fill(&mut buf).unwrap();
        assert!(buf[256..].iter().any(|&b| b != 0) && buf[599] != 0, "every piece filled");
        assert_eq!(sys::open_handles(), 2, "the two devices, and no source left open");
    }

    #[test]
    fn an_unseeded_read_waits_and_reads_again() {
        sys::reset();
        sys::unseeded(1);
        let mut buf = [0u8; 8];
        fill(&mut buf).unwrap();
        assert!(buf.iter().any(|&b| b != 0));
        assert_eq!(sys::open_handles(), 2, "the PO closed too");
    }
}
