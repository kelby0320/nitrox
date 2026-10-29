//! A group's bitmap block, as read.

use crate::FsError;

/// A bitmap block as read: a bit past its bytes is `Corrupt`, never an index past the buffer or
/// into the unread tail of a larger one.
pub(super) struct Bitmap<'a>(pub(super) &'a mut [u8]);

impl Bitmap<'_> {
    fn byte(&mut self, i: usize) -> Result<&mut u8, FsError> {
        self.0.get_mut(i / 8).ok_or(FsError::Corrupt)
    }
    pub(super) fn is_free(&mut self, i: usize) -> Result<bool, FsError> {
        Ok(*self.byte(i)? & (1 << (i % 8)) == 0)
    }
    pub(super) fn set(&mut self, i: usize) -> Result<(), FsError> {
        *self.byte(i)? |= 1 << (i % 8);
        Ok(())
    }
    pub(super) fn unset(&mut self, i: usize) -> Result<(), FsError> {
        *self.byte(i)? &= !(1 << (i % 8));
        Ok(())
    }
    /// The first free bit below `end`.
    pub(super) fn first_free(&mut self, end: usize) -> Result<Option<usize>, FsError> {
        for i in 0..end {
            if self.is_free(i)? {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }
}
