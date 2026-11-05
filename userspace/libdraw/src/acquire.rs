//! Acquiring the system framebuffer from the namespace.
//!
//! The one part of `libdraw` that touches syscalls, hence behind the optional `io` feature.
//!
//! Authority is the binding: a process can drive the display if and only if `/dev/framebuffer`
//! is in its namespace (`docs/design/display-substrate.md` §3). [`map_framebuffer`] takes a namespace
//! handle and either finds the binding or does not.

use libkern::abi::FramebufferInfo;
use libkern::handle::Rights;
use libos::{CanLookup, Handle, MapRead, MapReadWrite, Memory, Namespace, Ref, block_on};

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
    /// Firmware reported a pitch too small to hold a row, which would alias rows, or an aperture
    /// too short for its rows, which would write past the mapping.
    ImpossibleGeometry,
    /// The aperture resolved but could not be mapped.
    ApertureUnmappable,
}

/// The path the aperture is bound at.
pub const FRAMEBUFFER_PATH: &str = "/dev/framebuffer";
/// The path its geometry is served at.
pub const FRAMEBUFFER_INFO_PATH: &str = "/dev/framebuffer/info";

/// Read the framebuffer's geometry without mapping the aperture.
pub fn framebuffer_info<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
) -> Result<FramebufferInfo, AcquireError> {
    let obj: Handle<Memory, MapRead> =
        block_on(ns.into().lookup(FRAMEBUFFER_INFO_PATH, Rights::MAP_READ)).map_err(|_| AcquireError::NoBinding)?;

    // The kernel serves this leaf as exactly one `FramebufferInfo` followed by zero padding.
    let mut bytes = [0u8; core::mem::size_of::<FramebufferInfo>()];
    let mapping = obj.map(bytes.len()).map_err(|_| AcquireError::InfoUnmappable)?;
    mapping.read_into(0, &mut bytes).map_err(|_| AcquireError::InfoUnmappable)?;
    FramebufferInfo::from_bytes(&bytes).ok_or(AcquireError::InfoTruncated)
}

/// Turn a firmware geometry report into a [`Geometry`].
///
/// Separate so it can be tested on the host: a channel-order or stride mistake would live here.
pub fn geometry_from(info: &FramebufferInfo) -> Result<Geometry, AcquireError> {
    if info.bits_per_pixel != 32 {
        return Err(AcquireError::UnsupportedDepth(info.bits_per_pixel));
    }
    let format = PixelFormat {
        bits_per_pixel: 32,
        red: Channel::new(info.red_shift, info.red_size),
        green: Channel::new(info.green_shift, info.green_size),
        blue: Channel::new(info.blue_shift, info.blue_size),
        // The display is never an alpha format whatever firmware reports in the fourth byte; the
        // hardware leaves it zero, so a blend would paint nothing.
        alpha: None,
    };
    Geometry::with_pitch(info.width, info.height, info.pitch as usize, format)
        .filter(|g| info.byte_len >= g.byte_len() as u64)
        .ok_or(AcquireError::ImpossibleGeometry)
}

/// Map the framebuffer and wrap it as a writable [`RawFramebuffer`].
///
/// Reads `/dev/framebuffer/info` for the geometry, then maps `/dev/framebuffer`. The mapping
/// holds its own reference to the object, so it outlives the resolved handles.
///
/// # Safety
///
/// The returned [`RawFramebuffer`] must not outlive the address space, and nothing else may
/// write the mapping.
pub unsafe fn map_framebuffer<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
) -> Result<(RawFramebuffer, FramebufferInfo), AcquireError> {
    let ns = ns.into();
    let info = framebuffer_info(ns)?;
    let geometry = geometry_from(&info)?;

    let obj: Handle<Memory, MapReadWrite> = block_on(ns.lookup(FRAMEBUFFER_PATH, Rights::MAP_READ | Rights::MAP_WRITE))
        .map_err(|_| AcquireError::NoBinding)?;

    let (addr, _) = obj.map(info.byte_len as usize).map_err(|_| AcquireError::ApertureUnmappable)?.into_raw();
    // SAFETY: `addr` maps `byte_len` writable bytes of the aperture, at least `geometry`'s as
    // `geometry_from` checked, which stays mapped for the life of the address space; the caller's
    // contract forbids a second writer.
    Ok((unsafe { RawFramebuffer::new(geometry, addr) }, info))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::handle::RawHandle;
    use libos::NsReadOnly;

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
        // Firmware reports BGR and the client renders RGB anyway: a self-hash cannot see this.
        let mut i = info(32, 5120);
        i.red_shift = 0;
        i.blue_shift = 16;
        let g = geometry_from(&i).unwrap();
        assert_eq!(g.format, PixelFormat::XBGR8888);
        assert_ne!(g.format, PixelFormat::XRGB8888);
    }

    #[test]
    fn a_padded_stride_is_preserved_rather_than_recomputed() {
        // 1280 * 4 = 5120; 5376 means 64 bytes of row padding.
        let g = geometry_from(&info(32, 5376)).unwrap();
        assert_eq!(g.pitch, 5376, "the reported pitch must win over width * bpp");
    }

    #[test]
    fn an_unsupported_depth_is_refused_rather_than_rendered_wrong() {
        assert_eq!(geometry_from(&info(24, 3840)), Err(AcquireError::UnsupportedDepth(24)));
        assert_eq!(geometry_from(&info(16, 2560)), Err(AcquireError::UnsupportedDepth(16)));
    }

    #[test]
    fn an_aperture_short_of_its_last_row_is_refused() {
        let mut i = info(32, 5120);
        i.byte_len -= 1;
        assert_eq!(geometry_from(&i), Err(AcquireError::ImpossibleGeometry));
    }

    #[test]
    fn a_pitch_too_narrow_for_a_row_is_refused() {
        // Would alias row n+1 onto row n.
        assert_eq!(geometry_from(&info(32, 4096)), Err(AcquireError::ImpossibleGeometry));
    }

    #[test]
    fn a_namespace_without_the_display_has_no_binding() {
        libos::mock::reset();
        let (ns, _peer) = libos::mock::answering("/nothing/here");
        // SAFETY: the mock's handle, open for the test.
        let root = unsafe { Ref::<Namespace, NsReadOnly>::from_raw(RawHandle(ns), Rights::LOOKUP) };
        assert_eq!(framebuffer_info(root).err(), Some(AcquireError::NoBinding));
        // SAFETY: nothing is mapped, so nothing is written.
        assert_eq!(unsafe { map_framebuffer(root) }.err(), Some(AcquireError::NoBinding));
    }
}
