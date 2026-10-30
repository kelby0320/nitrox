//! [`Pod`]: plain data, which any bytes of its size are a value of.

use core::mem::size_of;

/// **Plain data**: no padding, and every bit pattern of its size a value, so a copy may make one
/// from any bytes, a user's included, and its bytes hold nothing but its fields.
///
/// # Safety
/// Both of those hold. A struct implements it through [`pod!`], which checks them.
pub unsafe trait Pod: Copy + 'static {
    /// The value whose bytes are all zero.
    fn zeroed() -> Self {
        // SAFETY: every bit pattern is a value, all zeros among them.
        unsafe { core::mem::zeroed() }
    }
}

macro_rules! pod_integers {
    ($($t:ty),*) => {
        // SAFETY: an integer has no padding, and every bit pattern of its size is one.
        $(unsafe impl Pod for $t {})*
    };
}
pod_integers!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

// SAFETY: an array's elements are plain data, and an array has no padding between them.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}

/// The size of the field `f` names, which must be plain data: [`pod!`]'s check.
#[doc(hidden)]
pub const fn field_size<S, T: Pod>(_: fn(&S) -> &T) -> usize {
    size_of::<T>()
}

/// Implement [`Pod`] for the struct `$ty`, naming each field once. The build fails on a field left
/// out or named twice, on one not plain data, and on padding, which leaves the sizes short.
macro_rules! pod {
    ($ty:path { $($field:tt),+ $(,)? }) => {
        // SAFETY: the checks below: each field named once, each `Pod`, and together the struct.
        unsafe impl $crate::libkern::pod::Pod for $ty {}
        const _: fn($ty) = |v| {
            let $ty { $($field: _),+ } = v;
        };
        const _: () = assert!(
            core::mem::size_of::<$ty>() == 0 $(+ $crate::libkern::pod::field_size(|s: &$ty| &s.$field))+,
            "a field not named, or padding"
        );
    };
}
pub(crate) use pod;

/// The bytes of `v`.
pub fn bytes_of<T: Pod>(v: &T) -> &[u8] {
    // SAFETY: `T` is plain data, so its `size_of` bytes are all initialised, and are its fields'.
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

/// The bytes of `v`, writable: any bytes written there leave a `T`.
pub fn bytes_of_mut<T: Pod>(v: &mut T) -> &mut [u8] {
    // SAFETY: as `bytes_of`, and every bit pattern is a `T`, so no write leaves an invalid one.
    unsafe { core::slice::from_raw_parts_mut((v as *mut T).cast::<u8>(), size_of::<T>()) }
}

/// The bytes of `v` as the array of its size, checked as it compiles.
pub fn byte_array_of<T: Pod, const N: usize>(v: &T) -> &[u8; N] {
    const { assert!(size_of::<T>() == N) };
    // SAFETY: as `bytes_of`, and the array is exactly `T`'s size, its alignment 1.
    unsafe { &*(v as *const T).cast::<[u8; N]>() }
}

/// [`byte_array_of`], writable: any bytes written there leave a `T`.
pub fn byte_array_of_mut<T: Pod, const N: usize>(v: &mut T) -> &mut [u8; N] {
    const { assert!(size_of::<T>() == N) };
    // SAFETY: as `bytes_of_mut`, and the array is exactly `T`'s size, its alignment 1.
    unsafe { &mut *(v as *mut T).cast::<[u8; N]>() }
}

/// The bytes of a slice of plain data.
pub fn slice_bytes<T: Pod>(s: &[T]) -> &[u8] {
    // SAFETY: as `bytes_of`, for each of `s.len()` elements, with no padding between them.
    unsafe { core::slice::from_raw_parts(s.as_ptr().cast::<u8>(), core::mem::size_of_val(s)) }
}

/// [`slice_bytes`], writable: any bytes written there leave `T`s.
pub fn slice_bytes_mut<T: Pod>(s: &mut [T]) -> &mut [u8] {
    // SAFETY: as `slice_bytes`, and every bit pattern is a `T`, so no write leaves an invalid one.
    unsafe { core::slice::from_raw_parts_mut(s.as_mut_ptr().cast::<u8>(), core::mem::size_of_val(s)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct Pair {
        a: u32,
        b: [u8; 4],
    }
    pod!(Pair { a, b });

    #[test]
    fn plain_data_round_trips_through_its_bytes() {
        let mut p = Pair { a: 0x0403_0201, b: [5, 6, 7, 8] };
        assert_eq!(bytes_of(&p), &[1, 2, 3, 4, 5, 6, 7, 8]);
        bytes_of_mut(&mut p)[0] = 9;
        assert_eq!(p.a, 0x0403_0209);
        assert_eq!(Pair::zeroed(), Pair { a: 0, b: [0; 4] });
        assert_eq!(slice_bytes(&[1u16, 2]), &[1, 0, 2, 0]);
        let mut pairs = [0u16; 2];
        slice_bytes_mut(&mut pairs).copy_from_slice(&[3, 0, 4, 0]);
        assert_eq!(pairs, [3, 4]);
    }
}
