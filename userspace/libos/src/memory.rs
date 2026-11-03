//! Memory objects, and the [`Mapping`] through which a process reaches an object's bytes.
//!
//! **Someone else may write a mapping's bytes at any time**: another process mapping the same
//! object, or a device the kernel lent it to. A Rust reference promises nothing writes them while
//! it lives, so a `Mapping` lends none: its bytes are copied in and out. Only a caller who knows
//! nothing else writes them takes a slice, and says so with `unsafe`.

use core::marker::PhantomData;
use core::ptr::NonNull;

use libkern::{KError, Rights};

use crate::error::{Error, Result, check};
use crate::handle::{CanMapWrite, Handle, MapReadWrite, Mappable, MemMode, Memory, both};
use crate::sys;

impl Handle<Memory, MapReadWrite> {
    /// An anonymous, zeroed memory object of `size` bytes, rounded up to a page.
    pub fn create(size: usize) -> Result<Handle<Memory, MapReadWrite>> {
        let h = check(sys::memory_create(size as u64))?;
        Ok(Handle::fresh(h, Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT))
    }
}

both!([T: Mappable, M: MemMode] T, M {
    /// Map the object's first `len` bytes with this mode's rights, wherever the kernel chooses.
    pub fn map(&self, len: usize) -> Result<Mapping<M>> {
        let addr = check(sys::memory_map(self.raw().0, 0, len as u64, M::map_rights()))?;
        let base = NonNull::new(addr as usize as *mut u8).ok_or(Error::from(KError::KernelError))?;
        Ok(Mapping { base, len, _m: PhantomData })
    }
});

/// `len` bytes of an object mapped into this process with mode `M`, unmapped on drop.
pub struct Mapping<M> {
    base: NonNull<u8>,
    len: usize,
    _m: PhantomData<M>,
}

impl<M> Mapping<M> {
    /// How many bytes it maps.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it maps nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where it starts, for code that reaches the bytes itself.
    pub fn as_ptr(&self) -> *const u8 {
        self.base.as_ptr()
    }

    /// `out.len()` bytes from `offset`, copied out. `InvalidArgument` past the end.
    pub fn read_into(&self, offset: usize, out: &mut [u8]) -> Result<()> {
        self.within(offset, out.len())?;
        // SAFETY: `offset..offset + out.len()` lies in the mapping, which lives as long as `self`,
        // and `out` is another object's memory; a copy forms no reference into the mapping.
        unsafe { core::ptr::copy_nonoverlapping(self.base.as_ptr().add(offset), out.as_mut_ptr(), out.len()) };
        Ok(())
    }

    /// The bytes as a slice.
    ///
    /// # Safety
    /// Nothing writes them while the slice lives: no other mapping of the object, here or in
    /// another process, and no device I/O into it.
    pub unsafe fn assume_unshared(&self) -> &[u8] {
        // SAFETY: the mapping covers `len` bytes for as long as `self`, and nothing writes them, by
        // this fn's contract.
        unsafe { core::slice::from_raw_parts(self.base.as_ptr(), self.len) }
    }

    /// Keep it mapped for the rest of the process's life: where it starts, and how long it is.
    pub fn into_raw(self) -> (*mut u8, usize) {
        let kept = (self.base.as_ptr(), self.len);
        core::mem::forget(self);
        kept
    }

    fn within(&self, offset: usize, len: usize) -> Result<()> {
        match offset.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            _ => Err(Error::from(KError::InvalidArgument)),
        }
    }
}

impl<M: CanMapWrite> Mapping<M> {
    /// Copy `bytes` in at `offset`. `InvalidArgument` past the end.
    pub fn write_from(&mut self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.within(offset, bytes.len())?;
        // SAFETY: as `read_into`, the other way.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.as_ptr().add(offset), bytes.len()) };
        Ok(())
    }

    /// Write `byte` over `len` bytes from `offset`. `InvalidArgument` past the end.
    pub fn fill(&mut self, offset: usize, len: usize, byte: u8) -> Result<()> {
        self.within(offset, len)?;
        // SAFETY: the range lies in the mapping, which lives as long as `self`.
        unsafe { core::ptr::write_bytes(self.base.as_ptr().add(offset), byte, len) };
        Ok(())
    }

    /// Copy `len` bytes from `src` at `src_at` to `at` here; `InvalidArgument` past either end.
    /// **Ranges that overlap in one object copy in no defined order**: its two mappings have
    /// addresses of their own, so the copy cannot see that it reads what it has written.
    pub fn copy_from<S>(&mut self, at: usize, src: &Mapping<S>, src_at: usize, len: usize) -> Result<()> {
        self.within(at, len)?;
        src.within(src_at, len)?;
        // SAFETY: both ranges lie in mappings that live for the call, and a copy forms no reference
        // into either; `copy`, as libos's model maps every mapping of an object at one address.
        unsafe { core::ptr::copy(src.base.as_ptr().add(src_at), self.base.as_ptr().add(at), len) };
        Ok(())
    }

    /// Where it starts, writable, for code that reaches the bytes itself.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.base.as_ptr()
    }

    /// The bytes as a writable slice.
    ///
    /// # Safety
    /// As [`assume_unshared`](Self::assume_unshared), and nothing reads them either.
    pub unsafe fn assume_unshared_mut(&mut self) -> &mut [u8] {
        // SAFETY: as `assume_unshared`, and `&mut self` lends it once.
        unsafe { core::slice::from_raw_parts_mut(self.base.as_ptr(), self.len) }
    }
}

impl<M> Drop for Mapping<M> {
    fn drop(&mut self) {
        // SAFETY: the mapping is this value's alone, and every borrow of its bytes has ended.
        unsafe { sys::memory_unmap(self.base.as_ptr() as u64, self.len as u64) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handle::{File, MapRead};

    #[test]
    fn a_mapping_copies_in_and_out_and_unmaps_on_drop() {
        sys::reset();
        let mem = Handle::<Memory, MapReadWrite>::create(100).unwrap();
        let mut m = mem.map(8192 / 2).unwrap();
        assert_eq!(sys::mappings(), 1);
        m.write_from(10, b"nitrox").unwrap();
        let mut out = [0u8; 8];
        m.read_into(9, &mut out).unwrap();
        assert_eq!(&out, b"\0nitrox\0");
        drop(m);
        assert_eq!(sys::mappings(), 0);
    }

    #[test]
    fn a_copy_past_the_end_is_refused_and_touches_nothing() {
        sys::reset();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let mut m = mem.map(16).unwrap();
        let mut out = [7u8; 4];
        let refused = |r: Result<()>| r.err().map(|e| e.code()) == Some(KError::InvalidArgument);
        assert!(refused(m.read_into(13, &mut out)), "13 + 4 > 16");
        assert!(refused(m.write_from(usize::MAX, b"x")), "no overflow past the end");
        assert!(refused(m.fill(16, 1, 0)));
        assert_eq!(out, [7; 4]);
        m.read_into(12, &mut out).unwrap();
        assert_eq!(out, [0; 4], "the last four bytes are in it");
    }

    #[test]
    fn a_map_asks_the_kernel_for_its_modes_rights() {
        sys::reset();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let ro = mem.duplicate(Rights::MAP_READ).unwrap().into_read_only().unwrap();
        assert!(ro.map(4096).is_ok());
        let rw: Handle<Memory, MapReadWrite> = mem.duplicate(Rights::MAP_READ).unwrap();
        assert_eq!(rw.map(4096).err().unwrap().code(), KError::NoAccess, "MAP_WRITE asked, not held");
        assert_eq!(mem.map(8192).err().unwrap().code(), KError::InvalidArgument, "past the object");
    }

    #[test]
    fn one_mapping_copies_from_another_within_both() {
        sys::reset();
        let a = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let b = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let mut from = a.map(4096).unwrap();
        from.write_from(0, b"abcdef").unwrap();
        let mut to = b.map(4096).unwrap();
        to.copy_from(10, &from, 1, 4).unwrap();
        let mut out = [0u8; 4];
        to.read_into(10, &mut out).unwrap();
        assert_eq!(&out, b"bcde");
        let refused = |r: Result<()>| r.err().map(|e| e.code()) == Some(KError::InvalidArgument);
        assert!(refused(to.copy_from(4093, &from, 0, 4)) && refused(to.copy_from(0, &from, 4093, 4)));
        let mut again = a.map(4096).unwrap();
        again.copy_from(100, &from, 0, 4).unwrap();
        from.read_into(100, &mut out).unwrap();
        assert_eq!(&out, b"abcd", "two mappings of one object, at ranges apart in it");
    }

    #[test]
    fn a_kept_mapping_stays_mapped() {
        sys::reset();
        let mem = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let (base, len) = mem.map(4096).unwrap().into_raw();
        assert!(!base.is_null() && len == 4096);
        drop(mem);
        assert_eq!(sys::mappings(), 1, "the object lives on in the mapping");
    }

    #[test]
    fn a_file_maps_as_memory_does() {
        sys::reset();
        let ns = crate::Handle::<crate::Namespace, crate::NsMutable>::create().unwrap();
        let rw = Rights::MAP_READ | Rights::MAP_WRITE;
        let f: Handle<File, MapReadWrite> = crate::block_on(crate::fs::create(&ns, "/f", 10, rw)).unwrap();
        let mut m = f.map(10).unwrap();
        m.write_from(0, b"0123456789").unwrap();
        let ro: Handle<File, MapRead> = crate::block_on(ns.lookup("/f", Rights::MAP_READ)).unwrap();
        let mut out = [0u8; 10];
        ro.map(10).unwrap().read_into(0, &mut out).unwrap();
        assert_eq!(&out, b"0123456789", "two mappings of one file see one set of pages");
    }
}
