//! Acquiring the system framebuffer from the namespace.
//!
//! The one part of `libdraw` that touches syscalls, and therefore the one part behind
//! the optional `io` feature — the same split `libstream` uses to keep its wire core
//! dependency-free and host-testable. Everything else in this crate is pure logic over
//! the [`Framebuffer`](crate::framebuffer::Framebuffer) trait and needs no OS at all.
//!
//! **Authority is the binding.** There is no display capability to request: a process
//! can drive the display if and only if `/dev/framebuffer` is in the namespace it was
//! given (`docs/design/display-substrate.md` §3). [`acquire`] therefore takes a
//! namespace handle and either finds the binding or does not.
//!
//! This lives in the library rather than in whichever program happens to need it first,
//! because it is the compositor's forever — mapping the aperture and turning firmware's
//! geometry report into a [`Geometry`] is not throwaway code, even though the program
//! driving it during Milestone 1 is.

use libkern::abi::FramebufferInfo;
use libkern::handle::{RawHandle, Rights};
use libos::{Handle, MapRead, MapReadWrite, Memory, Namespace, NsReadOnly, block_on};

use crate::format::{Channel, PixelFormat};
use crate::framebuffer::{Geometry, RawFramebuffer};

/// Why acquiring the framebuffer failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AcquireError {
    /// `/dev/framebuffer/info` did not resolve — no display bound into this namespace.
    NoBinding,
    /// The info object resolved but could not be mapped.
    InfoUnmappable,
    /// The info object was shorter than a [`FramebufferInfo`].
    InfoTruncated,
    /// Firmware reported a depth this crate does not render (only 32 bpp is supported).
    UnsupportedDepth(u16),
    /// Firmware reported a pitch too small to hold a row, which would alias rows.
    ImpossibleGeometry,
    /// The aperture resolved but could not be mapped.
    ApertureUnmappable,
}

/// The path the aperture is bound at.
pub const FRAMEBUFFER_PATH: &str = "/dev/framebuffer";
/// The path its geometry is served at.
pub const FRAMEBUFFER_INFO_PATH: &str = "/dev/framebuffer/info";

/// Read the framebuffer's geometry without mapping the aperture.
///
/// Useful on its own: a program can report what the display *is* without taking
/// write access to it.
///
/// # Safety
///
/// `root_ns` must be a live namespace handle owned by the caller for the duration of
/// the call. It is borrowed, never closed.
pub unsafe fn read_info(root_ns: u64) -> Result<FramebufferInfo, AcquireError> {
    // SAFETY: the caller guarantees `root_ns` is live and owned; `borrow` yields a
    // non-owning view that never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable object holding one FramebufferInfo.
    let obj = block_on(unsafe {
        ns.lookup::<Memory, MapRead>(FRAMEBUFFER_INFO_PATH, Rights::MAP_READ)
    })
    .map_err(|_| AcquireError::NoBinding)?;

    let addr = obj.map(core::mem::size_of::<FramebufferInfo>()).map_err(|_| AcquireError::InfoUnmappable)?;
    // SAFETY: `map` returned a mapping of at least the requested length, and the
    // kernel serves this leaf as exactly one `FramebufferInfo` followed by zero padding.
    let bytes = unsafe {
        core::slice::from_raw_parts(addr as *const u8, core::mem::size_of::<FramebufferInfo>())
    };
    FramebufferInfo::from_bytes(bytes).ok_or(AcquireError::InfoTruncated)
}

/// Turn a firmware geometry report into a [`Geometry`].
///
/// Separated out so it can be tested on the host without a namespace: this is where a
/// channel-order or stride mistake would live, and it is pure arithmetic.
pub fn geometry_from(info: &FramebufferInfo) -> Result<Geometry, AcquireError> {
    if info.bits_per_pixel != 32 {
        return Err(AcquireError::UnsupportedDepth(info.bits_per_pixel));
    }
    let format = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(info.red_shift, info.red_size),
        green: Channel::new(info.green_shift, info.green_size),
        blue: Channel::new(info.blue_shift, info.blue_size),
    };
    Geometry::with_pitch(info.width, info.height, info.pitch as usize, format)
        .ok_or(AcquireError::ImpossibleGeometry)
}

/// Map the framebuffer and wrap it as a writable [`RawFramebuffer`].
///
/// Reads `/dev/framebuffer/info` for the geometry, then maps `/dev/framebuffer` itself.
/// The returned framebuffer borrows a mapping that outlives the resolved handles — a
/// mapping holds its own reference to the object, so closing the handle does not unmap.
///
/// # Safety
///
/// `root_ns` must be a live namespace handle owned by the caller for the duration of
/// the call. The returned [`RawFramebuffer`] must not outlive the process's address
/// space, and nothing else may write the same mapping concurrently.
pub unsafe fn acquire(root_ns: u64) -> Result<(RawFramebuffer, FramebufferInfo), AcquireError> {
    // SAFETY: forwarded from this function's own contract.
    let info = unsafe { read_info(root_ns) }?;
    let geometry = geometry_from(&info)?;

    // SAFETY: as above — borrowed, never closed.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read/write-mappable MemoryObject over the aperture.
    let obj = block_on(unsafe {
        ns.lookup::<Memory, MapReadWrite>(
            FRAMEBUFFER_PATH,
            Rights::MAP_READ | Rights::MAP_WRITE,
        )
    })
    .map_err(|_| AcquireError::NoBinding)?;

    let addr = obj.map(info.byte_len as usize).map_err(|_| AcquireError::ApertureUnmappable)?;
    // SAFETY: `addr` maps `byte_len` writable bytes of the aperture, which stays mapped
    // for the life of the address space; the caller's contract forbids a second writer.
    Ok((unsafe { RawFramebuffer::new(geometry, addr) }, info))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(bpp: u16, pitch: u64) -> FramebufferInfo {
        FramebufferInfo {
            width: 1280,
            height: 800,
            pitch,
            byte_len: pitch * 800,
            bits_per_pixel: bpp,
            red_shift: 16,
            red_size: 8,
            green_shift: 8,
            green_size: 8,
            blue_shift: 0,
            blue_size: 8,
        }
    }

    #[test]
    fn a_qemu_style_report_becomes_the_expected_geometry() {
        let g = geometry_from(&info(32, 5120)).unwrap();
        assert_eq!(g.width, 1280);
        assert_eq!(g.height, 800);
        assert_eq!(g.pitch, 5120);
        assert_eq!(g.format, PixelFormat::XRGB8888);
    }

    #[test]
    fn a_swapped_channel_report_is_carried_through_not_normalised() {
        // The bug a self-hash cannot see: firmware reports BGR and the client renders
        // RGB anyway. The report must survive into the format.
        let mut i = info(32, 5120);
        i.red_shift = 0;
        i.blue_shift = 16;
        let g = geometry_from(&i).unwrap();
        assert_eq!(g.format, PixelFormat::XBGR8888);
        assert_ne!(g.format, PixelFormat::XRGB8888);
    }

    #[test]
    fn a_padded_stride_is_preserved_rather_than_recomputed() {
        // 1280 * 4 = 5120; firmware reporting 5376 means 64 bytes of row padding.
        let g = geometry_from(&info(32, 5376)).unwrap();
        assert_eq!(g.pitch, 5376, "the reported pitch must win over width * bpp");
    }

    #[test]
    fn an_unsupported_depth_is_refused_rather_than_rendered_wrong() {
        assert_eq!(geometry_from(&info(24, 3840)), Err(AcquireError::UnsupportedDepth(24)));
        assert_eq!(geometry_from(&info(16, 2560)), Err(AcquireError::UnsupportedDepth(16)));
    }

    #[test]
    fn a_pitch_too_narrow_for_a_row_is_refused() {
        // Would alias row n+1 onto row n. Better to fail than to render a smear.
        assert_eq!(geometry_from(&info(32, 4096)), Err(AcquireError::ImpossibleGeometry));
    }
}
