//! The framebuffer geometry ABI — what a compositor must be told and cannot infer.
//!
//! `docs/design/display-substrate.md` §3: "What the compositor needs to be told is the
//! geometry it cannot infer: width, height, pitch (bytes per row, which is not width ×
//! bpp), and the channel layout. Limine reports all four to the kernel; they cross to
//! userspace as an attribute of the resource rather than a second protocol."
//!
//! Concretely, `/dev/framebuffer/info` resolves to a small read-only `MemoryObject`
//! holding one of these. That reuses the namespace lookup every other kernel server
//! uses — no new syscall, and no change to [`HandleInfo`], which is a frozen 24-byte
//! struct shared by every object type and the wrong place for per-type fields. (A
//! 16-versus-24-byte mismatch in that struct smashed a userspace stack once already.)
//!
//! **Channel layout is reported, not assumed.** Firmware does not always choose
//! `0x00RRGGBB`, and a compositor that hardcodes it renders channel-swapped output on
//! hardware that reports BGR — a bug a self-hash structurally cannot detect, because
//! the guest stays perfectly consistent with itself (§8c).
//!
//! [`HandleInfo`]: crate::libkern::handle::HandleInfo

use core::mem::{align_of, offset_of, size_of};

/// Geometry and pixel layout of the system framebuffer.
///
/// `#[repr(C)]` and mirrored byte-for-byte in `userspace/libkern/src/abi.rs`. Both
/// sides carry the layout asserts below; `cargo xtask abi-sync-check` deliberately does
/// not compare `#[repr(C)]` layouts because these asserts are the stronger check and
/// fail at build time.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct FramebufferInfo {
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Bytes per row. **Not** `width * bytes_per_pixel` — firmware pads rows, and code
    /// that assumes otherwise writes every row after the first at a skewed offset.
    pub pitch: u64,
    /// Total mappable bytes of the aperture (`pitch * height`, page-rounded).
    pub byte_len: u64,
    /// Bits per pixel. Only 32 is served; the server refuses anything else rather than
    /// letting a client render garbage.
    pub bits_per_pixel: u16,
    /// Bit offset of the red channel's least significant bit.
    pub red_shift: u8,
    /// Red channel width in bits.
    pub red_size: u8,
    /// Bit offset of the green channel's least significant bit.
    pub green_shift: u8,
    /// Green channel width in bits.
    pub green_size: u8,
    /// Bit offset of the blue channel's least significant bit.
    pub blue_shift: u8,
    /// Blue channel width in bits.
    pub blue_size: u8,
}

const _: () = assert!(size_of::<FramebufferInfo>() == 32);
const _: () = assert!(align_of::<FramebufferInfo>() == 8);
const _: () = assert!(offset_of!(FramebufferInfo, width) == 0);
const _: () = assert!(offset_of!(FramebufferInfo, height) == 4);
const _: () = assert!(offset_of!(FramebufferInfo, pitch) == 8);
const _: () = assert!(offset_of!(FramebufferInfo, byte_len) == 16);
const _: () = assert!(offset_of!(FramebufferInfo, bits_per_pixel) == 24);
const _: () = assert!(offset_of!(FramebufferInfo, red_shift) == 26);
const _: () = assert!(offset_of!(FramebufferInfo, red_size) == 27);
const _: () = assert!(offset_of!(FramebufferInfo, green_shift) == 28);
const _: () = assert!(offset_of!(FramebufferInfo, green_size) == 29);
const _: () = assert!(offset_of!(FramebufferInfo, blue_shift) == 30);
const _: () = assert!(offset_of!(FramebufferInfo, blue_size) == 31);

impl FramebufferInfo {
    /// Reinterpret the struct as bytes, for copying into a `MemoryObject`.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `Self` is `#[repr(C)]` with no padding holes (the asserts above pin
        // every offset and the total size), contains no pointers or references, and is
        // valid for reads of `size_of::<Self>()` bytes for the borrow's lifetime.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_bytes_round_trips_the_declared_layout() {
        let info = FramebufferInfo {
            width: 1280,
            height: 800,
            pitch: 5120,
            byte_len: 5120 * 800,
            bits_per_pixel: 32,
            red_shift: 16,
            red_size: 8,
            green_shift: 8,
            green_size: 8,
            blue_shift: 0,
            blue_size: 8,
        };
        let b = info.as_bytes();
        assert_eq!(b.len(), 32);
        // Read the fields back at their documented offsets, the way userspace will.
        assert_eq!(u32::from_le_bytes(b[0..4].try_into().unwrap()), 1280);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 800);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 5120);
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 5120 * 800);
        assert_eq!(u16::from_le_bytes(b[24..26].try_into().unwrap()), 32);
        assert_eq!(b[26], 16, "red_shift");
        assert_eq!(b[30], 0, "blue_shift");
        assert_eq!(b[31], 8, "blue_size");
    }

    #[test]
    fn the_struct_has_no_padding_holes_to_leak() {
        // Every byte is accounted for by a field, so `as_bytes` never copies
        // uninitialised padding out to userspace.
        let zeroed = FramebufferInfo::default();
        assert!(zeroed.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(
            size_of::<u32>() * 2 + size_of::<u64>() * 2 + size_of::<u16>() + 6,
            size_of::<FramebufferInfo>()
        );
    }
}
